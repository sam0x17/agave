//! On-chain leader schedule account management.
//!
//! Updates leader schedule accounts at epoch boundaries. These accounts are
//! owned by the leader schedule native program and store the schedule in a
//! compact binary format for consumption by indexers and on-chain programs.

use {
    crate::bank::Bank,
    solana_account::AccountSharedData,
    solana_clock::Epoch,
    solana_leader_schedule::{on_chain as format, NUM_CONSECUTIVE_LEADER_SLOTS},
    solana_pubkey::Pubkey,
    std::sync::LazyLock,
};

/// PDA for the current epoch's leader schedule account.
pub static CURRENT_SCHEDULE_ADDR: LazyLock<Pubkey> = LazyLock::new(|| {
    let (pubkey, _) = Pubkey::find_program_address(
        &[b"current_schedule"],
        &solana_leader_schedule_program::id(),
    );
    pubkey
});

/// PDA for the next epoch's leader schedule account.
pub static NEXT_SCHEDULE_ADDR: LazyLock<Pubkey> = LazyLock::new(|| {
    let (pubkey, _) = Pubkey::find_program_address(
        &[b"next_schedule"],
        &solana_leader_schedule_program::id(),
    );
    pubkey
});

/// Compute and store the leader schedule for a given epoch into `dest_addr`.
fn write_schedule_account(bank: &Bank, epoch: Epoch, dest_addr: &Pubkey) {
    let vote_accounts = match bank.epoch_vote_accounts(epoch) {
        Some(va) => va,
        None => return,
    };

    let slots_in_epoch: usize = bank
        .epoch_schedule()
        .get_slots_in_epoch(epoch)
        .try_into()
        .expect("slots in epoch must fit in usize");

    let slots_per_block = NUM_CONSECUTIVE_LEADER_SLOTS;

    // Compute the schedule the same way LeaderScheduleCache does.
    let schedule = solana_leader_schedule::LeaderSchedule::new(
        vote_accounts,
        epoch,
        slots_in_epoch,
        slots_per_block,
    );

    // Extract identity pubkeys per slot for serialization.
    let slot_leaders: Vec<Pubkey> = schedule
        .get_slot_leaders()
        .iter()
        .map(|sl| sl.id)
        .collect();

    let data = format::serialize_leader_schedule(
        &slot_leaders,
        epoch,
        slots_per_block.get(),
    );
    let lamports = bank
        .rent_collector()
        .rent
        .minimum_balance(data.len())
        .max(1);
    let mut account = AccountSharedData::new(
        lamports,
        data.len(),
        &solana_leader_schedule_program::id(),
    );
    account.set_data_from_slice(&data);
    bank.store_account_and_update_capitalization(dest_addr, &account);
}

/// Update the on-chain leader schedule accounts at an epoch boundary.
///
/// Called from `process_new_epoch()`. On the first activation, both accounts
/// are bootstrapped. On subsequent epoch boundaries, `next` is rotated into
/// `current` and a new `next` is computed.
pub(crate) fn update_on_chain_leader_schedule(bank: &Bank) {
    let current_epoch = bank.epoch();
    let next_epoch = current_epoch + 1;

    let is_bootstrap = bank.get_account(&*CURRENT_SCHEDULE_ADDR).is_none();

    if is_bootstrap {
        // First activation: populate current. Only populate next if
        // vote accounts for that epoch are already available.
        write_schedule_account(bank, current_epoch, &CURRENT_SCHEDULE_ADDR);
        if bank.epoch_vote_accounts(next_epoch).is_some() {
            write_schedule_account(bank, next_epoch, &NEXT_SCHEDULE_ADDR);
        }
    } else {
        // Rotate: copy next -> current (if next exists), then compute new next.
        if let Some(next_account) = bank.get_account(&*NEXT_SCHEDULE_ADDR) {
            bank.store_account_and_update_capitalization(
                &CURRENT_SCHEDULE_ADDR,
                &next_account,
            );
        } else {
            // next_schedule wasn't populated yet (edge case after bootstrap
            // when next epoch data wasn't available). Compute current directly.
            write_schedule_account(bank, current_epoch, &CURRENT_SCHEDULE_ADDR);
        }
        write_schedule_account(bank, next_epoch, &NEXT_SCHEDULE_ADDR);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            genesis_utils::{
                bootstrap_validator_stake_lamports, create_genesis_config_with_leader,
            },
            leader_schedule_utils,
        },
        solana_account::ReadableAccount,
        solana_leader_schedule::on_chain::{deserialize_header, get_leader_at_block},
    };

    /// Verify the hardcoded PDA addresses in reserved-account-keys match
    /// the computed values. This test will fail if the program ID changes
    /// without updating the reserved keys.
    #[test]
    fn test_pda_addresses_are_consistent() {
        use solana_pubkey::Pubkey;
        let current_expected =
            Pubkey::from_str_const("7yJfSmGSR1m4Xy6JVvxufauQWo7oEqrHVsjWiAS8hSpD");
        let next_expected =
            Pubkey::from_str_const("9RXx9Z8EcmAnv3LMHk8GLzM5wsyUT8G4YoVfiqsURGMN");
        assert_eq!(*CURRENT_SCHEDULE_ADDR, current_expected);
        assert_eq!(*NEXT_SCHEDULE_ADDR, next_expected);
    }

    #[test]
    fn test_bootstrap_creates_both_accounts() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);

        // Verify preconditions.
        let epoch = bank.epoch();
        assert!(bank.epoch_vote_accounts(epoch).is_some());

        update_on_chain_leader_schedule(&bank);

        // Current account should exist.
        let current = bank
            .get_account(&*CURRENT_SCHEDULE_ADDR)
            .expect("current schedule account should exist after bootstrap");

        let header = deserialize_header(current.data()).unwrap();
        assert_eq!(header.epoch, epoch);
        assert!(header.num_leaders > 0);
        assert!(header.num_slot_blocks > 0);
        assert_eq!(header.slots_per_block, NUM_CONSECUTIVE_LEADER_SLOTS.get() as u16);

        // The sole leader should be leader_pubkey.
        let leader = get_leader_at_block(current.data(), 0).unwrap();
        assert_eq!(leader, leader_pubkey);

        // Verify owner is the leader schedule program.
        assert_eq!(*current.owner(), solana_leader_schedule_program::id());
    }

    #[test]
    fn test_rotation_copies_next_to_current() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);

        // Bootstrap.
        update_on_chain_leader_schedule(&bank);

        // Simulate a subsequent epoch boundary update.
        update_on_chain_leader_schedule(&bank);

        // After rotation, current should still be valid.
        let current = bank.get_account(&*CURRENT_SCHEDULE_ADDR).unwrap();
        assert!(deserialize_header(current.data()).is_some());
    }

    /// Verify the on-chain schedule matches what leader_schedule_utils computes.
    #[test]
    fn test_on_chain_matches_leader_schedule_cache() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);
        let epoch = bank.epoch();

        // Compute via leader_schedule_utils (same path as LeaderScheduleCache).
        let canonical_schedule =
            leader_schedule_utils::leader_schedule(epoch, &bank).unwrap();

        // Compute via on-chain serialization.
        update_on_chain_leader_schedule(&bank);
        let current = bank.get_account(&*CURRENT_SCHEDULE_ADDR).unwrap();
        let header = deserialize_header(current.data()).unwrap();

        // Verify every slot block matches.
        for block_idx in 0..header.num_slot_blocks as usize {
            let slot_idx = block_idx * header.slots_per_block as usize;
            let on_chain_leader =
                get_leader_at_block(current.data(), block_idx).unwrap();
            let canonical_leader = canonical_schedule[slot_idx as u64].id;
            assert_eq!(
                on_chain_leader, canonical_leader,
                "mismatch at block {block_idx} (slot {slot_idx})"
            );
        }
    }
}
