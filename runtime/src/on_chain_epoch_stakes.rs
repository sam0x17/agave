//! On-chain epoch stakes account management.
//!
//! Writes one new account at every epoch boundary containing the epoch
//! stakes (the vote-account-to-delegated-stake mapping) for the upcoming
//! epoch. The account is addressed by a PDA keyed on the epoch number, so
//! each epoch has stable data that the runtime writes once. See SIMD-0511
//! for the full design.

use {
    crate::bank::Bank,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Epoch,
    solana_epoch_stakes_program::state::{
        self as stakes_format, BLS_PUBKEY_COMPRESSED_SIZE, EpochStakesEntry, UNRANKED,
    },
    solana_pubkey::Pubkey,
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
    let rent_exempt_minimum = bank
        .rent_collector()
        .rent
        .minimum_balance(data.len())
        .max(1);
    // A transaction can transfer lamports to a future PDA before the runtime
    // writes it. Preserve that balance when replacing the system-owned
    // placeholder so pre-funding cannot block publication or burn funds.
    let lamports = bank.get_balance(dest_addr).max(rent_exempt_minimum);
    let mut account =
        AccountSharedData::new(lamports, data.len(), &solana_epoch_stakes_program::id());
    account.set_data_from_slice(data);
    bank.store_account_and_update_capitalization(dest_addr, &account);
}

/// Serialize and store the epoch stakes account for `epoch`, if not already written.
fn write_epoch_stakes_account(bank: &Bank, epoch: Epoch) {
    let addr = epoch_stakes_address(epoch);
    if let Some(account) = bank.get_account(&addr) {
        // Only an account previously written by the runtime is complete. A
        // system-owned account may exist because anyone can pre-fund a PDA.
        if account.owner() == &solana_epoch_stakes_program::id() {
            return;
        }
    }
    let Some(epoch_stakes) = bank.epoch_stakes(epoch) else {
        return;
    };
    let vote_accounts = epoch_stakes.stakes().vote_accounts().as_ref();
    let rank_map = bank
        .feature_set
        .snapshot()
        .alpenglow
        .then(|| epoch_stakes.bls_pubkey_to_rank_map());

    let entries: Vec<EpochStakesEntry> = vote_accounts
        .iter()
        .map(|(vote_pubkey, (stake, vote_account))| {
            let view = vote_account.vote_state_view();
            let node_pubkey = *vote_account.node_pubkey();
            // SIMD-0185/SIMD-0232 collectors and commissions. For vote
            // accounts whose state predates vote account v4, fall back to
            // the SIMD-0185 migration defaults per the SIMD-0511 schema
            // rules: inflation rewards were previously collected into the
            // vote account, block revenue into the validator identity.
            let inflation_rewards_collector = view
                .inflation_rewards_collector()
                .copied()
                .unwrap_or(*vote_pubkey);
            let block_revenue_collector = view
                .block_revenue_collector()
                .copied()
                .unwrap_or(node_pubkey);
            EpochStakesEntry {
                vote_pubkey: *vote_pubkey,
                node_pubkey,
                inflation_rewards_collector,
                block_revenue_collector,
                delegated_stake: *stake,
                cumulative_credits: view.credits(),
                inflation_rewards_commission_bps: view.inflation_rewards_commission(),
                block_revenue_commission_bps: view.block_revenue_commission(),
                alpenglow_rank: rank_map
                    .and_then(|rank_map| rank_map.get_rank_for_vote_pubkey(vote_pubkey))
                    .copied()
                    .unwrap_or(UNRANKED),
                bls_pubkey_compressed: view
                    .bls_pubkey_compressed()
                    .unwrap_or([0; BLS_PUBKEY_COMPRESSED_SIZE]),
            }
        })
        .collect();

    let data = stakes_format::serialize_epoch_stakes(&entries, epoch);
    store_program_account(bank, &addr, &data);
}

/// Update the on-chain epoch stakes accounts at an epoch boundary.
///
/// Called from `process_new_epoch()`. Writes a new account for the current
/// epoch (if missing, e.g. on first activation) and for the upcoming epoch.
/// Account data already written by the runtime is not rewritten; every epoch
/// gets its own permanent PDA. See SIMD-0511.
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
            ValidatorVoteKeypairs, bootstrap_validator_stake_lamports,
            create_genesis_config_with_alpenglow_vote_accounts, create_genesis_config_with_leader,
        },
        solana_epoch_stakes_program::state::deserialize_header,
        solana_sdk_ids::system_program,
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

    #[test]
    fn test_prefunded_pda_does_not_block_write() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);
        let epoch = bank.epoch();
        let address = epoch_stakes_address(epoch);
        let prefunded_lamports = 42;
        bank.store_account_and_update_capitalization(
            &address,
            &AccountSharedData::new(prefunded_lamports, 0, &system_program::id()),
        );

        update_on_chain_epoch_stakes(&bank);

        let account = bank.get_account(&address).unwrap();
        assert_eq!(account.owner(), &solana_epoch_stakes_program::id());
        assert!(account.lamports() >= prefunded_lamports);
        assert_eq!(deserialize_header(account.data()).unwrap().epoch, epoch);
    }

    /// Verify the on-chain epoch stakes match what the bank has internally,
    /// across every per-vote-account field defined by SIMD-0511.
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

        let bank_total: u64 = bank_vote_accounts.values().map(|(stake, _)| *stake).sum();
        assert_eq!(header.total_stake, bank_total);

        // Cross-check every per-vote-account field.
        let on_chain_entries =
            solana_epoch_stakes_program::state::get_all_entries(account.data()).unwrap();
        for entry in &on_chain_entries {
            let (bank_stake, bank_vote_account) = bank_vote_accounts
                .get(&entry.vote_pubkey)
                .expect("vote_pubkey from on-chain account should exist in bank");
            assert_eq!(entry.delegated_stake, *bank_stake);
            assert_eq!(entry.node_pubkey, *bank_vote_account.node_pubkey());
            let view = bank_vote_account.vote_state_view();
            assert_eq!(
                entry.inflation_rewards_commission_bps,
                view.inflation_rewards_commission()
            );
            assert_eq!(
                entry.block_revenue_commission_bps,
                view.block_revenue_commission()
            );
            assert_eq!(entry.cumulative_credits, view.credits());
            assert_eq!(entry.alpenglow_rank, UNRANKED);
            assert_eq!(
                entry.bls_pubkey_compressed,
                view.bls_pubkey_compressed()
                    .unwrap_or([0; BLS_PUBKEY_COMPRESSED_SIZE])
            );
            let expected_inflation_collector = view
                .inflation_rewards_collector()
                .copied()
                .unwrap_or(entry.vote_pubkey);
            assert_eq!(
                entry.inflation_rewards_collector,
                expected_inflation_collector
            );
            let expected_block_revenue_collector = view
                .block_revenue_collector()
                .copied()
                .unwrap_or(*bank_vote_account.node_pubkey());
            assert_eq!(
                entry.block_revenue_collector,
                expected_block_revenue_collector
            );
        }
    }

    #[test]
    fn test_alpenglow_rank_matches_bank_rank_map() {
        let validator_keypairs = [ValidatorVoteKeypairs::new_rand()];
        let genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![100],
        )
        .genesis_config;
        let bank = Bank::new_for_tests(&genesis_config);
        assert!(bank.feature_set.snapshot().alpenglow);

        let epoch = bank.epoch();
        update_on_chain_epoch_stakes(&bank);

        let account = bank.get_account(&epoch_stakes_address(epoch)).unwrap();
        let entries = solana_epoch_stakes_program::state::get_all_entries(account.data()).unwrap();
        let rank_map = bank.epoch_stakes(epoch).unwrap().bls_pubkey_to_rank_map();

        assert_eq!(entries.len(), 1);
        assert_eq!(
            rank_map
                .get_rank_for_vote_pubkey(&entries[0].vote_pubkey)
                .copied(),
            Some(entries[0].alpenglow_rank)
        );
        assert_eq!(entries[0].alpenglow_rank, 0);
        assert_ne!(
            entries[0].bls_pubkey_compressed,
            [0; BLS_PUBKEY_COMPRESSED_SIZE]
        );
    }
}
