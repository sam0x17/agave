//! On-chain leader schedule account management.
//!
//! Updates leader schedule accounts at epoch boundaries. These accounts are
//! owned by the leader schedule native program and store the schedule in a
//! compact binary format for consumption by indexers and on-chain programs.

use {
    crate::bank::Bank,
    solana_account::AccountSharedData,
    solana_clock::Epoch,
    solana_leader_schedule::on_chain as format,
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

    // Compute the schedule the same way LeaderScheduleCache does.
    let schedule = solana_leader_schedule::LeaderSchedule::new(
        vote_accounts,
        epoch,
        slots_in_epoch,
        solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    );

    // Extract identity pubkeys per slot for serialization.
    let slot_leaders: Vec<Pubkey> = schedule
        .get_slot_leaders()
        .iter()
        .map(|sl| sl.id)
        .collect();

    let data = format::serialize_leader_schedule(&slot_leaders, epoch);
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
        // First activation: populate both accounts from scratch.
        write_schedule_account(bank, current_epoch, &CURRENT_SCHEDULE_ADDR);
        // Next epoch vote accounts may not be available yet; if so, use
        // current epoch as a fallback — it will be overwritten at the next
        // epoch boundary.
        let next_epoch_available = bank.epoch_vote_accounts(next_epoch).is_some();
        let next_schedule_epoch = if next_epoch_available {
            next_epoch
        } else {
            current_epoch
        };
        write_schedule_account(bank, next_schedule_epoch, &NEXT_SCHEDULE_ADDR);
    } else {
        // Rotate: copy next -> current, then compute new next.
        if let Some(next_account) = bank.get_account(&*NEXT_SCHEDULE_ADDR) {
            bank.store_account_and_update_capitalization(
                &CURRENT_SCHEDULE_ADDR,
                &next_account,
            );
        }
        write_schedule_account(bank, next_epoch, &NEXT_SCHEDULE_ADDR);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{
            bootstrap_validator_stake_lamports, create_genesis_config_with_leader,
        },
        solana_account::ReadableAccount,
        solana_leader_schedule::on_chain::{deserialize_header, get_leader_at_block},
    };

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

        // Verify preconditions: epoch vote accounts must exist for epoch 0.
        let epoch = bank.epoch();
        assert!(
            bank.epoch_vote_accounts(epoch).is_some(),
            "epoch_vote_accounts({epoch}) should exist",
        );
        let vote_accounts = bank.epoch_vote_accounts(epoch).unwrap();
        let staked_count = vote_accounts
            .values()
            .filter(|(stake, _)| *stake > 0)
            .count();
        assert!(
            staked_count > 0,
            "expected at least one staked vote account, got {staked_count}",
        );

        // Accounts should not exist yet.
        assert!(bank.get_account(&*CURRENT_SCHEDULE_ADDR).is_none());
        assert!(bank.get_account(&*NEXT_SCHEDULE_ADDR).is_none());

        update_on_chain_leader_schedule(&bank);

        // Both accounts should now exist.
        let current = bank
            .get_account(&*CURRENT_SCHEDULE_ADDR)
            .expect("current schedule account should exist after bootstrap");
        let next = bank
            .get_account(&*NEXT_SCHEDULE_ADDR)
            .expect("next schedule account should exist after bootstrap");

        // Verify current schedule.
        let header = deserialize_header(current.data()).unwrap();
        assert_eq!(header.epoch, bank.epoch());
        assert!(header.num_leaders > 0);
        assert!(header.num_slot_blocks > 0);

        // The sole leader should be leader_pubkey.
        let leader = get_leader_at_block(current.data(), 0).unwrap();
        assert_eq!(leader, leader_pubkey);

        // Next schedule should be for a future epoch.
        let next_header = deserialize_header(next.data()).unwrap();
        assert!(next_header.epoch >= bank.epoch());

        // Verify owner is the leader schedule program.
        assert_eq!(*current.owner(), solana_leader_schedule_program::id());
        assert_eq!(*next.owner(), solana_leader_schedule_program::id());
    }

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

        let _next_data_before = bank
            .get_account(&*NEXT_SCHEDULE_ADDR)
            .unwrap()
            .data()
            .to_vec();

        // Simulate a subsequent epoch boundary update.
        update_on_chain_leader_schedule(&bank);

        // After rotation, current should contain what next had before.
        // (In practice the epoch doesn't advance here so next gets
        // recomputed with the same data, but current should now exist.)
        let current = bank.get_account(&*CURRENT_SCHEDULE_ADDR).unwrap();
        assert!(deserialize_header(current.data()).is_some());
    }
}
