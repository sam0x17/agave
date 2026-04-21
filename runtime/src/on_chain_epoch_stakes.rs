//! On-chain epoch stakes account management.
//!
//! Writes one new account at every epoch boundary containing the epoch
//! stakes (the vote-account-to-delegated-stake mapping) for the upcoming
//! epoch. The account is addressed by a PDA keyed on the epoch number, so
//! each epoch has a stable, never-modified account. See SIMD-0511 for the
//! full design.

use {
    crate::bank::Bank, solana_account::AccountSharedData, solana_clock::Epoch,
    solana_leader_schedule::epoch_stakes_on_chain as stakes_format, solana_pubkey::Pubkey,
};

/// PDA seed prefix for epoch stakes accounts.
pub const EPOCH_STAKES_SEED_PREFIX: &[u8] = b"epoch_stakes";

/// Derive the PDA for the epoch stakes account at a given epoch.
pub fn epoch_stakes_address(epoch: Epoch) -> Pubkey {
    let (pubkey, _) = Pubkey::find_program_address(
        &[EPOCH_STAKES_SEED_PREFIX, &epoch.to_le_bytes()],
        &solana_epoch_stakes_program::id(),
    );
    pubkey
}

/// Helper to create and store an account with data owned by the epoch stakes program.
fn store_program_account(bank: &Bank, dest_addr: &Pubkey, data: &[u8]) {
    let lamports = bank
        .rent_collector()
        .rent
        .minimum_balance(data.len())
        .max(1);
    let mut account =
        AccountSharedData::new(lamports, data.len(), &solana_epoch_stakes_program::id());
    account.set_data_from_slice(data);
    bank.store_account_and_update_capitalization(dest_addr, &account);
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

/// Update the on-chain epoch stakes accounts at an epoch boundary.
///
/// Called from `process_new_epoch()`. Writes a new account for the current
/// epoch (if missing, e.g. on first activation) and for the upcoming epoch.
/// Accounts already on disk are never modified; every epoch gets its own
/// permanent PDA. See SIMD-0511.
pub(crate) fn update_on_chain_epoch_stakes(bank: &Bank) {
    let current_epoch = bank.epoch();
    let next_epoch = current_epoch + 1;

    // On first activation, the current epoch's account doesn't exist yet.
    // On subsequent calls this is a no-op because the write helper early-returns.
    write_epoch_stakes_account(bank, current_epoch);

    // Write the next epoch's account if stakes are known. On the very first
    // epoch boundary after activation, this may be unavailable; in that case
    // the next call to this function will pick it up.
    write_epoch_stakes_account(bank, next_epoch);
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{
            bootstrap_validator_stake_lamports, create_genesis_config_with_leader,
        },
        solana_account::ReadableAccount,
        solana_leader_schedule::epoch_stakes_on_chain::deserialize_header,
    };

    #[test]
    fn test_pda_derivation_is_deterministic() {
        assert_eq!(epoch_stakes_address(0), epoch_stakes_address(0));
        assert_eq!(epoch_stakes_address(42), epoch_stakes_address(42));
        assert_ne!(epoch_stakes_address(0), epoch_stakes_address(1));
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

        update_on_chain_epoch_stakes(&bank);

        let stakes_account = bank
            .get_account(&epoch_stakes_address(epoch))
            .expect("epoch stakes account should exist after bootstrap");

        let header = deserialize_header(stakes_account.data()).unwrap();
        assert_eq!(header.epoch, epoch);
        assert!(header.num_entries > 0);
        assert!(header.total_stake > 0);

        // Owner is the epoch stakes program.
        assert_eq!(*stakes_account.owner(), solana_epoch_stakes_program::id());
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

        update_on_chain_epoch_stakes(&bank);

        let data_before = bank
            .get_account(&epoch_stakes_address(epoch))
            .unwrap()
            .data()
            .to_vec();

        update_on_chain_epoch_stakes(&bank);

        let data_after = bank
            .get_account(&epoch_stakes_address(epoch))
            .unwrap()
            .data()
            .to_vec();

        assert_eq!(data_before, data_after);
    }

    /// Verify the on-chain epoch stakes match what the bank has internally.
    #[test]
    fn test_on_chain_matches_bank_epoch_stakes() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);
        let epoch = bank.epoch();
        let bank_vote_accounts = bank.epoch_vote_accounts(epoch).unwrap().clone();

        update_on_chain_epoch_stakes(&bank);
        let account = bank.get_account(&epoch_stakes_address(epoch)).unwrap();
        let header = deserialize_header(account.data()).unwrap();

        assert_eq!(header.num_entries as usize, bank_vote_accounts.len());

        let on_chain_total: u64 = bank_vote_accounts.values().map(|(stake, _)| *stake).sum();
        assert_eq!(header.total_stake, on_chain_total);
    }
}
