//! On-chain leader schedule and epoch stakes account management.
//!
//! Writes two new accounts at every epoch boundary: one containing the leader
//! schedule and one containing the epoch stakes for the upcoming epoch. Both
//! are addressed by PDAs keyed on the epoch number, so each epoch has a
//! stable, never-modified account. See SIMD-0511 for the full design.

use {
    crate::bank::Bank,
    solana_account::AccountSharedData,
    solana_clock::Epoch,
    solana_hash::Hash,
    solana_leader_schedule::{
        NUM_CONSECUTIVE_LEADER_SLOTS, epoch_stakes_on_chain as stakes_format, on_chain as format,
    },
    solana_pubkey::Pubkey,
    solana_sha256_hasher::Hasher,
};

/// PDA seed prefix for leader schedule accounts.
pub const SCHEDULE_SEED_PREFIX: &[u8] = b"schedule";

/// PDA seed prefix for epoch stakes accounts.
pub const EPOCH_STAKES_SEED_PREFIX: &[u8] = b"epoch_stakes";

/// Derive the PDA for the leader schedule account at a given epoch.
pub fn schedule_address(epoch: Epoch) -> Pubkey {
    let (pubkey, _) = Pubkey::find_program_address(
        &[SCHEDULE_SEED_PREFIX, &epoch.to_le_bytes()],
        &solana_leader_schedule_program::id(),
    );
    pubkey
}

/// Derive the PDA for the epoch stakes account at a given epoch.
pub fn epoch_stakes_address(epoch: Epoch) -> Pubkey {
    let (pubkey, _) = Pubkey::find_program_address(
        &[EPOCH_STAKES_SEED_PREFIX, &epoch.to_le_bytes()],
        &solana_leader_schedule_program::id(),
    );
    pubkey
}

/// Helper to create and store an account with data owned by the leader schedule program.
fn store_program_account(bank: &Bank, dest_addr: &Pubkey, data: &[u8]) {
    let lamports = bank
        .rent_collector()
        .rent
        .minimum_balance(data.len())
        .max(1);
    let mut account =
        AccountSharedData::new(lamports, data.len(), &solana_leader_schedule_program::id());
    account.set_data_from_slice(data);
    bank.store_account_and_update_capitalization(dest_addr, &account);
}

/// Compute the SHA-256 hash of epoch stakes for inclusion in the leader schedule header.
/// Hashes the sorted (vote_pubkey, stake) pairs in deterministic order.
fn compute_epoch_stakes_hash(
    vote_accounts: &solana_vote::vote_account::VoteAccountsHashMap,
) -> Hash {
    let mut sorted_stakes: Vec<_> = vote_accounts
        .iter()
        .map(|(pubkey, (stake, _))| (*pubkey, *stake))
        .collect();
    sorted_stakes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Hasher::default();
    for (pubkey, stake) in &sorted_stakes {
        hasher.hash(pubkey.as_ref());
        hasher.hash(&stake.to_le_bytes());
    }
    hasher.result()
}

/// Serialize and store the epoch stakes account for `epoch`, if not already written.
fn write_epoch_stakes_account(bank: &Bank, epoch: Epoch) {
    let addr = epoch_stakes_address(epoch);
    if bank.get_account(&addr).is_some() {
        return;
    }
    let Some(vote_accounts) = bank.epoch_vote_accounts(epoch) else {
        return;
    };

    let stakes: Vec<_> = vote_accounts
        .iter()
        .map(|(pubkey, (stake, _))| (*pubkey, *stake))
        .collect();

    let data = stakes_format::serialize_epoch_stakes(&stakes, epoch);
    store_program_account(bank, &addr, &data);
}

/// Serialize and store the leader schedule account for `epoch`, if not already written.
fn write_schedule_account(bank: &Bank, epoch: Epoch) {
    let addr = schedule_address(epoch);
    if bank.get_account(&addr).is_some() {
        return;
    }
    let Some(vote_accounts) = bank.epoch_vote_accounts(epoch) else {
        return;
    };

    let slots_in_epoch: usize = bank
        .epoch_schedule()
        .get_slots_in_epoch(epoch)
        .try_into()
        .expect("slots in epoch must fit in usize");

    let slots_per_span = NUM_CONSECUTIVE_LEADER_SLOTS;

    let schedule = solana_leader_schedule::LeaderSchedule::new(
        vote_accounts,
        epoch,
        slots_in_epoch,
        slots_per_span,
    );

    let slot_leaders: Vec<_> = schedule.get_slot_leaders().copied().collect();
    let epoch_stakes_hash = compute_epoch_stakes_hash(vote_accounts);
    let data = format::serialize_leader_schedule(
        &slot_leaders,
        epoch,
        slots_per_span.get(),
        &epoch_stakes_hash,
    );
    store_program_account(bank, &addr, &data);
}

/// Update the on-chain leader schedule and epoch stakes accounts at an epoch boundary.
///
/// Called from `process_new_epoch()`. Writes new accounts for the current
/// epoch (if missing, e.g. on first activation) and for the upcoming epoch.
/// Accounts already on disk are never modified — every epoch gets its own
/// permanent PDA. See SIMD-0511.
pub(crate) fn update_on_chain_leader_schedule(bank: &Bank) {
    let current_epoch = bank.epoch();
    let next_epoch = current_epoch + 1;

    // On first activation, the current epoch's accounts don't exist yet.
    // On subsequent calls this is a no-op because the write helpers early-return.
    write_epoch_stakes_account(bank, current_epoch);
    write_schedule_account(bank, current_epoch);

    // Write the next epoch's accounts if stakes are known. On the very first
    // epoch boundary after activation, this may be unavailable; in that case
    // the next call to this function will pick it up.
    write_epoch_stakes_account(bank, next_epoch);
    write_schedule_account(bank, next_epoch);
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
        solana_leader_schedule::on_chain::{deserialize_header, get_leader_at_span},
    };

    #[test]
    fn test_pda_derivation_is_deterministic() {
        // Same epoch yields the same address across calls.
        assert_eq!(schedule_address(0), schedule_address(0));
        assert_eq!(schedule_address(42), schedule_address(42));
        assert_eq!(epoch_stakes_address(0), epoch_stakes_address(0));

        // Different epochs yield different addresses.
        assert_ne!(schedule_address(0), schedule_address(1));
        assert_ne!(epoch_stakes_address(0), epoch_stakes_address(1));

        // Schedule and epoch-stakes PDAs for the same epoch are distinct.
        assert_ne!(schedule_address(7), epoch_stakes_address(7));
    }

    #[test]
    fn test_bootstrap_creates_current_account() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);
        let epoch = bank.epoch();
        assert!(bank.epoch_vote_accounts(epoch).is_some());

        update_on_chain_leader_schedule(&bank);

        // Account for the current epoch exists at its epoch-keyed PDA.
        let current = bank
            .get_account(&schedule_address(epoch))
            .expect("current schedule account should exist after bootstrap");

        let header = deserialize_header(current.data()).unwrap();
        assert_eq!(header.epoch, epoch);
        assert!(header.num_leaders > 0);
        assert!(header.num_leader_spans > 0);
        assert_eq!(
            header.slots_per_span,
            NUM_CONSECUTIVE_LEADER_SLOTS.get() as u16
        );

        // The sole leader should be leader_pubkey.
        let leader = get_leader_at_span(current.data(), 0).unwrap();
        assert_eq!(leader.id, leader_pubkey);

        // Owner is the leader schedule program.
        assert_eq!(*current.owner(), solana_leader_schedule_program::id());

        // Epoch stakes account for the current epoch also exists.
        assert!(bank.get_account(&epoch_stakes_address(epoch)).is_some());
    }

    #[test]
    fn test_repeat_calls_are_idempotent_and_do_not_rewrite() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);
        let epoch = bank.epoch();

        update_on_chain_leader_schedule(&bank);

        // Capture the account data after the first write.
        let data_before = bank
            .get_account(&schedule_address(epoch))
            .unwrap()
            .data()
            .to_vec();

        // A second call at the same epoch boundary must not rewrite.
        update_on_chain_leader_schedule(&bank);

        let data_after = bank
            .get_account(&schedule_address(epoch))
            .unwrap()
            .data()
            .to_vec();

        assert_eq!(data_before, data_after);
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

        let canonical_schedule = leader_schedule_utils::leader_schedule(epoch, &bank).unwrap();

        update_on_chain_leader_schedule(&bank);
        let current = bank.get_account(&schedule_address(epoch)).unwrap();
        let header = deserialize_header(current.data()).unwrap();

        for span_idx in 0..header.num_leader_spans as usize {
            let slot_idx = span_idx * header.slots_per_span as usize;
            let on_chain_leader = get_leader_at_span(current.data(), span_idx).unwrap();
            let canonical_leader = canonical_schedule[slot_idx as u64];
            assert_eq!(
                on_chain_leader.id, canonical_leader.id,
                "identity mismatch at span {span_idx} (slot {slot_idx})"
            );
            assert_eq!(
                on_chain_leader.vote_address, canonical_leader.vote_address,
                "vote address mismatch at span {span_idx} (slot {slot_idx})"
            );
        }
    }
}
