//! On-chain epoch stakes account management.
//!
//! Writes one new account at every epoch boundary containing the epoch
//! stakes (the vote-account-to-delegated-stake mapping) for the upcoming
//! epoch. The account is addressed by a PDA keyed on the epoch number, so
//! each epoch has stable data that the runtime writes once. Eight epoch
//! accounts are retained; older accounts are closed at epoch boundaries.
//! See SIMD-0511 for the full design.

use {
    crate::bank::Bank,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Epoch,
    solana_epoch_stakes_program::state::{
        self as stakes_format, BLS_PUBKEY_COMPRESSED_SIZE, EpochStakesEntry, RETAINED_EPOCHS,
        UNRANKED,
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

fn close_expired_accounts(bank: &Bank) {
    // Only the parent's retained window can contain published accounts. Limit
    // the lookups to those eight epochs even if the bank skips several epochs.
    // Use parent_slot so this also works without an in-memory parent bank.
    let parent_epoch = bank.epoch_schedule().get_epoch(bank.parent_slot());
    let first_parent_epoch = parent_epoch.saturating_sub(RETAINED_EPOCHS - 2);
    let first_retained_epoch = bank.epoch().saturating_sub(RETAINED_EPOCHS - 2);
    let end = first_retained_epoch.min(parent_epoch.saturating_add(2));
    for epoch in first_parent_epoch..end {
        let address = epoch_stakes_address(epoch);
        if bank
            .get_account(&address)
            .is_some_and(|account| account.owner() == &solana_epoch_stakes_program::id())
        {
            // Burn the full balance, including transferred lamports. The store
            // helper also subtracts the closed account's data size.
            bank.store_account_and_update_capitalization(&address, &AccountSharedData::default());
        }
    }
}

/// Update the on-chain epoch stakes accounts at an epoch boundary.
///
/// Called from `process_new_epoch()`. Writes a new account for the current
/// epoch (if missing, e.g. on first activation) and for the upcoming epoch.
/// Retains six previous epochs, the current epoch, and the upcoming epoch.
/// Retained account data is not rewritten or moved between addresses.
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

    close_expired_accounts(bank);
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, bootstrap_validator_stake_lamports,
                create_genesis_config_with_alpenglow_vote_accounts,
                create_genesis_config_with_leader,
            },
        },
        agave_feature_set::on_chain_epoch_stakes,
        solana_account::WritableAccount,
        solana_epoch_schedule::{EpochSchedule, MINIMUM_SLOTS_PER_EPOCH},
        solana_epoch_stakes_program::state::deserialize_header,
        solana_feature_gate_interface::{self as feature, Feature},
        solana_genesis_config::GenesisConfig,
        solana_leader_schedule::SlotLeader,
        solana_rent::Rent,
        solana_sdk_ids::system_program,
        std::{
            collections::BTreeMap,
            sync::{Arc, RwLock},
        },
    };

    fn retention_test_genesis() -> GenesisConfig {
        let mut genesis_config = create_genesis_config_with_leader(
            0,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;
        genesis_config.epoch_schedule =
            EpochSchedule::custom(MINIMUM_SLOTS_PER_EPOCH, MINIMUM_SLOTS_PER_EPOCH, false);
        genesis_config
    }

    fn bank_at_epoch(
        bank_forks: &Arc<RwLock<BankForks>>,
        parent: Arc<Bank>,
        epoch: Epoch,
    ) -> Arc<Bank> {
        let slot = parent.epoch_schedule().get_first_slot_in_epoch(epoch);
        Bank::new_from_parent_with_bank_forks(bank_forks, parent, SlotLeader::default(), slot)
    }

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
        let mut genesis_config = retention_test_genesis();
        genesis_config.rent = Rent::default();

        for above_minimum in [false, true] {
            let bank = Bank::new_for_tests(&genesis_config);
            let epoch = bank.epoch();
            let address = epoch_stakes_address(epoch);
            let data_len = stakes_format::HEADER_SIZE
                + bank.epoch_vote_accounts(epoch).unwrap().len() * stakes_format::ENTRY_SIZE;
            let minimum_balance = bank.rent_collector().rent.minimum_balance(data_len).max(1);
            let prefunded_lamports = if above_minimum {
                minimum_balance + 42
            } else {
                minimum_balance / 2
            };
            bank.store_account_and_update_capitalization(
                &address,
                &AccountSharedData::new(prefunded_lamports, 0, &system_program::id()),
            );
            let capitalization = bank.capitalization();

            write_epoch_stakes_account(&bank, epoch);

            let account = bank.get_account(&address).unwrap();
            assert_eq!(account.owner(), &solana_epoch_stakes_program::id());
            assert_eq!(account.lamports(), minimum_balance.max(prefunded_lamports));
            assert_eq!(deserialize_header(account.data()).unwrap().epoch, epoch);
            assert_eq!(
                bank.capitalization(),
                capitalization + minimum_balance.saturating_sub(prefunded_lamports),
            );
        }
    }

    #[test]
    fn test_epoch_boundaries_retain_eight_immutable_accounts() {
        let (mut bank, bank_forks) =
            Bank::new_for_tests(&retention_test_genesis()).wrap_with_bank_forks_for_tests();
        let mut snapshots = BTreeMap::new();

        for current_epoch in 1..=12 {
            bank = bank_at_epoch(&bank_forks, bank, current_epoch);
            let accounts = bank
                .get_program_accounts(&solana_epoch_stakes_program::id())
                .unwrap();
            let mut retained = Vec::new();
            for (address, account) in accounts {
                let epoch = deserialize_header(account.data()).unwrap().epoch;
                assert_eq!(address, epoch_stakes_address(epoch));
                assert_eq!(
                    snapshots
                        .entry(epoch)
                        .or_insert_with(|| account.data().to_vec())
                        .as_slice(),
                    account.data(),
                );
                retained.push(epoch);
            }
            retained.sort_unstable();
            assert_eq!(
                retained,
                (current_epoch.saturating_sub(6).max(1)..=current_epoch + 1).collect::<Vec<_>>(),
            );
            assert!(bank.get_account(&epoch_stakes_address(0)).is_none());

            // Running the update again neither republishes expired epochs nor
            // changes the supply or account-data accounting.
            let capitalization = bank.capitalization();
            let data_size = bank.load_accounts_data_size();
            update_on_chain_epoch_stakes(&bank);
            assert_eq!(bank.capitalization(), capitalization);
            assert_eq!(bank.load_accounts_data_size(), data_size);
        }
        assert!(bank.get_account(&epoch_stakes_address(5)).is_none());
        assert!(bank.get_account(&epoch_stakes_address(6)).is_some());
    }

    #[test]
    fn test_closure_burns_balance_and_preserves_other_owners() {
        for current_epoch in [12, 20] {
            let mut genesis_config = retention_test_genesis();
            genesis_config.accounts.remove(&on_chain_epoch_stakes::id());
            let (bank, bank_forks) =
                Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
            let parent = bank_at_epoch(&bank_forks, bank, 7);
            // Seed the parent's retained window. Epoch 4 was never published;
            // epoch 2 is a system-owned placeholder, and 3 has another owner.
            for epoch in [1, 5, 6, 7, 8] {
                store_program_account(
                    &parent,
                    &epoch_stakes_address(epoch),
                    &stakes_format::serialize_epoch_stakes(&[], epoch),
                );
            }
            let donated_address = epoch_stakes_address(1);
            let mut donated_account = parent.get_account(&donated_address).unwrap();
            donated_account.set_lamports(donated_account.lamports() + 500);
            parent.store_account_and_update_capitalization(&donated_address, &donated_account);
            let placeholders = [
                (
                    epoch_stakes_address(2),
                    AccountSharedData::new(42, 0, &system_program::id()),
                ),
                (
                    epoch_stakes_address(3),
                    AccountSharedData::new(43, 17, &Pubkey::new_unique()),
                ),
                (
                    epoch_stakes_address(30),
                    AccountSharedData::new(44, 0, &system_program::id()),
                ),
            ];
            for (address, account) in &placeholders {
                parent.store_account_and_update_capitalization(address, account);
            }

            let bank = bank_at_epoch(&bank_forks, parent.clone(), current_epoch);
            let expired = if current_epoch == 12 {
                vec![1, 5]
            } else {
                vec![1, 5, 6, 7, 8]
            };
            let mut burned_lamports = 0;
            let mut removed_data_size = 0;
            for epoch in &expired {
                let account = bank.get_account(&epoch_stakes_address(*epoch)).unwrap();
                burned_lamports += account.lamports();
                removed_data_size += account.data().len() as u64;
            }
            let capitalization = bank.capitalization();
            let data_size = bank.load_accounts_data_size();

            close_expired_accounts(&bank);

            assert_eq!(bank.capitalization(), capitalization - burned_lamports);
            assert_eq!(
                bank.load_accounts_data_size(),
                data_size - removed_data_size
            );
            for epoch in expired {
                let address = epoch_stakes_address(epoch);
                assert!(bank.get_account(&address).is_none());
                assert!(parent.get_account(&address).is_some());
            }
            for (address, account) in &placeholders {
                assert_eq!(bank.get_account(address).as_ref(), Some(account));
            }
            if current_epoch == 12 {
                for epoch in 6..=8 {
                    let address = epoch_stakes_address(epoch);
                    assert_eq!(bank.get_account(&address), parent.get_account(&address));
                }
            }

            close_expired_accounts(&bank);
            assert_eq!(bank.capitalization(), capitalization - burned_lamports);
            assert_eq!(
                bank.load_accounts_data_size(),
                data_size - removed_data_size
            );
        }
    }

    #[test]
    fn test_activation_and_skipped_epochs_do_not_backfill_history() {
        let mut genesis_config = retention_test_genesis();
        genesis_config.accounts.remove(&on_chain_epoch_stakes::id());
        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let bank = bank_at_epoch(&bank_forks, bank, 3);
        assert!(
            bank.get_program_accounts(&solana_epoch_stakes_program::id())
                .unwrap()
                .is_empty()
        );
        bank.store_account_and_update_capitalization(
            &on_chain_epoch_stakes::id(),
            &feature::create_account(&Feature::default(), 1),
        );

        let bank = bank_at_epoch(&bank_forks, bank, 4);
        assert!(bank.feature_set.is_active(&on_chain_epoch_stakes::id()));
        for epoch in 0..4 {
            assert!(bank.get_account(&epoch_stakes_address(epoch)).is_none());
        }
        for epoch in 4..=5 {
            assert!(bank.get_account(&epoch_stakes_address(epoch)).is_some());
        }

        // A jump past the entire retained window closes every old account.
        // The missing current snapshot does not extend the retention window.
        let bank = bank_at_epoch(&bank_forks, bank, 12);
        assert!(bank.epoch_stakes(12).is_none());
        for epoch in 0..=12 {
            assert!(bank.get_account(&epoch_stakes_address(epoch)).is_none());
        }
        assert!(bank.get_account(&epoch_stakes_address(13)).is_some());
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
