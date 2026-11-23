//! On-chain epoch stakes account management.
//!
//! Writes one new account at every epoch boundary containing the epoch
//! stakes (the vote-account-to-delegated-stake mapping) taken at the start
//! of the current epoch, for use by consensus in the following epoch. The account
//! is addressed by a PDA keyed on the snapshot epoch number, so
//! each epoch has stable data that the runtime writes once. Eight epoch
//! accounts are retained; older accounts are closed at epoch boundaries.
//! See SIMD-0676 for the full design.

use {
    crate::{bank::Bank, epoch_stakes::VersionedEpochStakes},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Epoch,
    solana_epoch_stakes_program::state::{
        self as stakes_format, BLS_PUBKEY_COMPRESSED_SIZE, EpochStakesEntry, RETAINED_EPOCHS,
        UNRANKED,
    },
    solana_pubkey::Pubkey,
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::state::VoteStateVersions,
    std::{collections::HashMap, sync::Arc},
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

/// Store snapshot data under the dedicated epoch stakes owner address.
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
    // The bank indexes this snapshot by its consensus epoch, one epoch later
    // than the snapshot epoch used by the on-chain address and header.
    let Some(epoch_stakes) = bank.epoch_stakes(epoch + 1) else {
        return;
    };
    let entries = snapshot_entries(epoch_stakes, bank.feature_set.snapshot().alpenglow);

    let data = stakes_format::serialize_epoch_stakes(&entries, epoch);
    store_program_account(bank, &addr, &data);
}

fn snapshot_entries(epoch_stakes: &VersionedEpochStakes, alpenglow: bool) -> Vec<EpochStakesEntry> {
    let vote_accounts = epoch_stakes.stakes().vote_accounts().as_ref();
    let rank_map = alpenglow
        .then(|| epoch_stakes.try_bls_pubkey_to_rank_map())
        .flatten();

    vote_accounts
        .iter()
        .map(|(vote_pubkey, (stake, vote_account))| {
            let view = vote_account.vote_state_view();
            let node_pubkey = *vote_account.node_pubkey();
            let reward = view.epoch_credits_iter().last();
            EpochStakesEntry {
                vote_pubkey: *vote_pubkey,
                node_pubkey,
                delegated_stake: *stake,
                accumulated_lamports: view.credits(),
                inflation_rewards_commission_bps: view.inflation_rewards_commission(),
                block_revenue_commission_bps: view.block_revenue_commission(),
                bls_signer_index: rank_map
                    .and_then(|rank_map| rank_map.get_rank_for_vote_pubkey(vote_pubkey))
                    .copied()
                    .unwrap_or(UNRANKED),
                bls_pubkey_compressed: view
                    .bls_pubkey_compressed()
                    .unwrap_or([0; BLS_PUBKEY_COMPRESSED_SIZE]),
                latest_vote: view.last_voted_slot(),
                root_slot: view.root_slot(),
                pending_delegator_rewards: view.pending_delegator_rewards(),
                last_reward_epoch: reward.map(|item| item.epoch()),
                previous_accumulated_lamports: reward.map_or(0, |item| item.prev_credits()),
                inflation_rewards_collector: view
                    .inflation_rewards_collector()
                    .copied()
                    .unwrap_or(*vote_pubkey),
                block_revenue_collector: view
                    .block_revenue_collector()
                    .copied()
                    .unwrap_or(node_pubkey),
                // The vote-state versions supported by this revision have no location.
                location: [0; 3],
            }
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RestoreError {
    #[error("missing epoch stakes account for snapshot epoch {0}")]
    Missing(Epoch),
    #[error("invalid epoch stakes account for snapshot epoch {0}")]
    Invalid(Epoch),
    #[error("epoch stakes account disagrees with snapshot manifest for epoch {0}")]
    ManifestMismatch(Epoch),
    #[error("missing legacy epoch stakes inputs for snapshot epoch {0}")]
    MissingLegacy(Epoch),
}

/// Reconstruct published cache fields from this bank's restored accounts.
/// Full historical vote states remain required for fields absent from the account
/// format, including authorized voters and migration reward history.
pub(crate) fn restore_epoch_stakes(
    bank: &Bank,
) -> Result<HashMap<Epoch, VersionedEpochStakes>, RestoreError> {
    let mut restored = bank.epoch_stakes_map().clone();
    let Some(activation_slot) = bank
        .feature_set
        .activated_slot(&agave_feature_set::on_chain_epoch_stakes::id())
    else {
        return Ok(restored);
    };
    // Genesis does not run process_new_epoch. The first publication is at the
    // first actual epoch boundary, even when the feature starts active.
    let activation_epoch = bank.epoch_schedule().get_epoch(activation_slot).max(1);
    let first = activation_epoch.max(bank.epoch().saturating_sub(RETAINED_EPOCHS - 1));
    for epoch in first..=bank.epoch() {
        let consensus_epoch = epoch.checked_add(1).ok_or(RestoreError::Invalid(epoch))?;
        let legacy = restored.get(&consensus_epoch);
        let required = epoch == bank.epoch() || legacy.is_some();
        let account = bank.get_account_with_fixed_root(&epoch_stakes_address(epoch));
        let Some(account) =
            account.filter(|account| account.owner() == &solana_epoch_stakes_program::id())
        else {
            if required {
                return Err(RestoreError::Missing(epoch));
            }
            continue;
        };
        let header = stakes_format::deserialize_header(account.data())
            .ok_or(RestoreError::Invalid(epoch))?;
        let entries =
            stakes_format::get_all_entries(account.data()).ok_or(RestoreError::Invalid(epoch))?;
        if header.epoch != epoch || account.executable() {
            return Err(RestoreError::Invalid(epoch));
        }
        let legacy = legacy.ok_or(RestoreError::MissingLegacy(epoch))?;
        let mut expected = snapshot_entries(legacy, bank.feature_set.snapshot().alpenglow);
        expected.sort_by_key(|entry| entry.vote_pubkey);
        if entries != expected {
            return Err(RestoreError::ManifestMismatch(epoch));
        }
        let mut vote_accounts = HashMap::with_capacity(entries.len());
        for entry in entries {
            let historical = &legacy.stakes().vote_accounts().as_ref()[&entry.vote_pubkey].1;
            let account =
                restore_vote_account(&entry, historical).ok_or(RestoreError::Invalid(epoch))?;
            vote_accounts.insert(entry.vote_pubkey, (entry.delegated_stake, account));
        }
        let rebuilt = legacy.with_restored_vote_accounts(
            VoteAccounts::from(Arc::new(vote_accounts)),
            consensus_epoch,
        );
        // Do not trust serialized derived maps either. Rebuild identity and
        // authorized-voter mappings and check the stored signer indices above.
        let normalized_nodes = |stakes: &VersionedEpochStakes| {
            let mut nodes = stakes.node_id_to_vote_accounts().as_ref().clone();
            for node in nodes.values_mut() {
                node.vote_accounts.sort_unstable();
            }
            nodes
        };
        if rebuilt.total_stake() != legacy.total_stake()
            || normalized_nodes(&rebuilt) != normalized_nodes(legacy)
            || rebuilt.epoch_authorized_voters() != legacy.epoch_authorized_voters()
        {
            return Err(RestoreError::ManifestMismatch(epoch));
        }
        restored.insert(consensus_epoch, rebuilt);
    }
    Ok(restored)
}

fn restore_vote_account(entry: &EpochStakesEntry, legacy: &VoteAccount) -> Option<VoteAccount> {
    use solana_account::state_traits::StateMutWincode;
    let mut account = legacy.account().clone();
    let mut state: VoteStateVersions = account.state().ok()?;
    // Matching fields were checked before this call. Retain all omitted fields,
    // vote lockouts and prior reward tuples rather than inventing a full state.
    let credits = match &mut state {
        VoteStateVersions::V4(state) => {
            state.node_pubkey = entry.node_pubkey;
            state.inflation_rewards_collector = entry.inflation_rewards_collector;
            state.block_revenue_collector = entry.block_revenue_collector;
            state.inflation_rewards_commission_bps = entry.inflation_rewards_commission_bps;
            state.block_revenue_commission_bps = entry.block_revenue_commission_bps;
            state.pending_delegator_rewards = entry.pending_delegator_rewards;
            // Keep None distinct from a present all-zero key in legacy inputs.
            state.bls_pubkey_compressed = state
                .bls_pubkey_compressed
                .map(|_| entry.bls_pubkey_compressed);
            state.root_slot = entry.root_slot;
            &mut state.epoch_credits
        }
        VoteStateVersions::V3(state) => {
            state.node_pubkey = entry.node_pubkey;
            state.commission = u8::try_from(entry.inflation_rewards_commission_bps / 100).ok()?;
            state.root_slot = entry.root_slot;
            &mut state.epoch_credits
        }
        VoteStateVersions::V1_14_11(state) => {
            state.node_pubkey = entry.node_pubkey;
            state.commission = u8::try_from(entry.inflation_rewards_commission_bps / 100).ok()?;
            state.root_slot = entry.root_slot;
            &mut state.epoch_credits
        }
        VoteStateVersions::Uninitialized => return None,
    };
    if let Some(epoch) = entry.last_reward_epoch {
        *credits.last_mut()? = (
            epoch,
            entry.accumulated_lamports,
            entry.previous_accumulated_lamports,
        );
    }
    account.set_state(&state).ok()?;
    VoteAccount::try_from(account).ok()
}

fn close_expired_accounts(bank: &Bank) {
    // Only the parent's retained window can contain published accounts. Limit
    // the lookups to those eight epochs even if the bank skips several epochs.
    // Use parent_slot so this also works without an in-memory parent bank.
    let parent_epoch = bank.epoch_schedule().get_epoch(bank.parent_slot());
    let first_parent_epoch = parent_epoch.saturating_sub(RETAINED_EPOCHS - 1);
    let first_retained_epoch = bank.epoch().saturating_sub(RETAINED_EPOCHS - 1);
    let end = first_retained_epoch.min(parent_epoch.saturating_add(1));
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
/// Called from `process_new_epoch()`. Writes the snapshot taken at the start
/// of the current epoch, for use by consensus in the following epoch. No earlier
/// snapshot is backfilled. Retains the current snapshot and seven previous epochs.
pub(crate) fn update_on_chain_epoch_stakes(bank: &Bank) {
    write_epoch_stakes_account(bank, bank.epoch());
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
        solana_account::{WritableAccount, state_traits::StateMutWincode},
        solana_epoch_schedule::{EpochSchedule, MINIMUM_SLOTS_PER_EPOCH},
        solana_epoch_stakes_program::state::deserialize_header,
        solana_feature_gate_interface::{self as feature, Feature},
        solana_genesis_config::GenesisConfig,
        solana_instruction::{AccountMeta, Instruction},
        solana_leader_schedule::SlotLeader,
        solana_rent::Rent,
        solana_sdk_ids::system_program,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::Transaction,
        solana_transaction_error::TransactionError,
        solana_vote_interface::state::{Lockout, VoteStateV4, VoteStateVersions},
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
    fn test_snapshot_epoch_tracks_boundary_and_preserves_raw_commissions() {
        let (bank, bank_forks) =
            Bank::new_for_tests(&retention_test_genesis()).wrap_with_bank_forks_for_tests();
        let vote_pubkey = *bank.epoch_vote_accounts(1).unwrap().keys().next().unwrap();
        let old_commission = bank.epoch_vote_accounts(1).unwrap()[&vote_pubkey]
            .1
            .vote_state_view()
            .inflation_rewards_commission();
        let mut vote_account = bank.get_account(&vote_pubkey).unwrap();
        let mut vote_state = VoteStateV4::deserialize(vote_account.data(), &vote_pubkey).unwrap();
        vote_state.inflation_rewards_commission_bps = 10_001;
        vote_state.block_revenue_commission_bps = u16::MAX;
        vote_state.epoch_credits.push((0, 42, 17));
        vote_state.pending_delegator_rewards = 23;
        vote_state.inflation_rewards_collector = Pubkey::new_unique();
        vote_state.block_revenue_collector = Pubkey::new_unique();
        let expected_inflation_collector = vote_state.inflation_rewards_collector;
        let expected_block_collector = vote_state.block_revenue_collector;
        vote_state.votes.push_back(Lockout::new(15).into());
        vote_state.root_slot = Some(10);
        vote_account
            .set_state(&VoteStateVersions::new_v4(vote_state))
            .unwrap();
        bank.store_account(&vote_pubkey, &vote_account);

        let bank = bank_at_epoch(&bank_forks, bank, 1);
        let address = epoch_stakes_address(1);
        let snapshot = bank.get_account(&address).unwrap();
        assert_eq!(deserialize_header(snapshot.data()).unwrap().epoch, 1);
        let entry = stakes_format::get_entry(snapshot.data(), 0).unwrap();
        assert_eq!(entry.vote_pubkey, vote_pubkey);
        assert_eq!(entry.inflation_rewards_commission_bps, 10_001);
        assert_eq!(entry.block_revenue_commission_bps, u16::MAX);
        assert_eq!(entry.accumulated_lamports, 42);
        assert_eq!(entry.last_reward_epoch, Some(0));
        assert_eq!(entry.previous_accumulated_lamports, 17);
        assert_eq!(entry.pending_delegator_rewards, 23);
        assert_eq!(
            entry.inflation_rewards_collector,
            expected_inflation_collector
        );
        assert_eq!(entry.block_revenue_collector, expected_block_collector);
        assert_eq!(entry.location, [0; 3]);
        assert_eq!(entry.latest_vote, Some(15));
        assert_eq!(entry.root_slot, Some(10));
        assert!(bank.get_account(&epoch_stakes_address(0)).is_none());
        assert_eq!(
            bank.epoch_vote_accounts(1).unwrap()[&vote_pubkey]
                .1
                .vote_state_view()
                .inflation_rewards_commission(),
            old_commission,
        );
        assert!(bank.get_account(&epoch_stakes_address(2)).is_none());

        // Changes later in the epoch must not replace the boundary snapshot.
        let mut live_account = bank.get_account(&vote_pubkey).unwrap();
        let mut live_state = VoteStateV4::deserialize(live_account.data(), &vote_pubkey).unwrap();
        live_state.inflation_rewards_commission_bps = 2_000;
        live_state.epoch_credits.push((1, 100, 42));
        live_state.votes.clear();
        live_state.root_slot = None;
        live_account
            .set_state(&VoteStateVersions::new_v4(live_state))
            .unwrap();
        bank.store_account(&vote_pubkey, &live_account);
        update_on_chain_epoch_stakes(&bank);
        assert_eq!(bank.get_account(&address).unwrap().data(), snapshot.data());
    }

    #[test]
    fn test_epoch_stakes_accounts_accept_transfers_without_changing_data() {
        let genesis = create_genesis_config_with_leader(
            1_000_000_000,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        );
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        update_on_chain_epoch_stakes(&bank);
        let address = epoch_stakes_address(bank.epoch());
        for destination in [address, solana_epoch_stakes_program::id()] {
            let before = bank.get_account(&destination).unwrap_or_default();
            let transfer = solana_system_transaction::transfer(
                &genesis.mint_keypair,
                &destination,
                1_000_000,
                bank.last_blockhash(),
            );
            bank.process_transaction(&transfer).unwrap();
            let after = bank.get_account(&destination).unwrap();
            assert_eq!(after.lamports(), before.lamports() + 1_000_000);
            assert_eq!(after.data(), before.data());
            assert_eq!(after.owner(), before.owner());
        }
    }

    #[test]
    fn test_pda_derivation_is_deterministic() {
        assert_eq!(epoch_stakes_address(0), epoch_stakes_address(0));
        assert_eq!(epoch_stakes_address(42), epoch_stakes_address(42));
        assert_ne!(epoch_stakes_address(0), epoch_stakes_address(1));
    }

    #[test]
    fn test_non_executable_owner_preserves_snapshot_transaction_protections() {
        let genesis = create_genesis_config_with_leader(
            1_000_000_000,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        );
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        update_on_chain_epoch_stakes(&bank);
        let address = epoch_stakes_address(bank.epoch());
        let owner_address = solana_epoch_stakes_program::id();
        assert!(bank.get_account(&owner_address).is_none());
        bank.transfer(1_000_000, &genesis.mint_keypair, &owner_address)
            .unwrap();
        let owner = bank.get_account(&owner_address).unwrap();
        assert_eq!(owner.owner(), &system_program::id());
        assert!(!owner.executable());
        assert!(owner.data().is_empty());
        let snapshot = bank.get_account(&address).unwrap();

        let transaction = |instruction| {
            Transaction::new_signed_with_payer(
                &[instruction],
                Some(&genesis.mint_keypair.pubkey()),
                &[&genesis.mint_keypair],
                bank.last_blockhash(),
            )
        };
        let invoke_owner = Instruction {
            program_id: owner_address,
            accounts: vec![AccountMeta::new(address, false)],
            data: vec![],
        };
        assert_eq!(
            bank.process_transaction(&transaction(invoke_owner)),
            Err(TransactionError::InvalidProgramForExecution),
        );

        // A transaction cannot sign for the PDA. Removing the signer bit must
        // not let system instructions change its data, owner, or balance.
        for mut instruction in [
            system_instruction::allocate(&address, 1),
            system_instruction::assign(&address, &system_program::id()),
            system_instruction::transfer(&address, &genesis.mint_keypair.pubkey(), 1),
        ] {
            instruction.accounts[0].is_signer = false;
            assert!(bank.process_transaction(&transaction(instruction)).is_err());
            assert_eq!(bank.get_account(&address).as_ref(), Some(&snapshot));
        }
    }

    #[test]
    fn test_publication_creates_current_account() {
        let leader_pubkey = solana_pubkey::new_rand();
        let genesis_config = create_genesis_config_with_leader(
            0,
            &leader_pubkey,
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;

        let bank = Bank::new_for_tests(&genesis_config);
        let epoch = bank.epoch();
        assert!(bank.epoch_vote_accounts(epoch + 1).is_some());

        update_on_chain_epoch_stakes(&bank);

        let stakes_account = bank
            .get_account(&epoch_stakes_address(epoch))
            .expect("epoch stakes account should exist after publication");

        let header = deserialize_header(stakes_account.data()).unwrap();
        assert_eq!(header.epoch, epoch);
        assert!(header.num_entries > 0);
        assert!(header.total_stake > 0);

        // The snapshot is owned by the dedicated non-executable address.
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
                + bank.epoch_vote_accounts(epoch + 1).unwrap().len() * stakes_format::ENTRY_SIZE;
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
                (1.max(current_epoch.saturating_sub(7))..=current_epoch).collect::<Vec<_>>(),
            );
            assert!(
                bank.get_account(&epoch_stakes_address(current_epoch + 1))
                    .is_none()
            );

            let restored = restore_epoch_stakes(&bank).unwrap();
            assert_eq!(restored.len(), bank.epoch_stakes_map().len());
            for (epoch, stakes) in &restored {
                assert_eq!(
                    stakes.stakes().vote_accounts(),
                    bank.epoch_stakes(*epoch).unwrap().stakes().vote_accounts()
                );
            }

            // Running the update again neither republishes expired epochs nor
            // changes the supply or account-data accounting.
            let capitalization = bank.capitalization();
            let data_size = bank.load_accounts_data_size();
            update_on_chain_epoch_stakes(&bank);
            assert_eq!(bank.capitalization(), capitalization);
            assert_eq!(bank.load_accounts_data_size(), data_size);
        }
        assert!(bank.get_account(&epoch_stakes_address(4)).is_none());
        assert!(bank.get_account(&epoch_stakes_address(5)).is_some());
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
            for epoch in [0, 1, 5, 6, 7] {
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
                vec![0, 1]
            } else {
                vec![0, 1, 5, 6, 7]
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
                for epoch in 5..=7 {
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
    fn test_activation_publishes_only_current_snapshot_and_skips_missing_epochs() {
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
        assert!(bank.epoch_stakes(4).is_some());
        for epoch in 0..4 {
            assert!(bank.get_account(&epoch_stakes_address(epoch)).is_none());
        }
        assert!(bank.get_account(&epoch_stakes_address(4)).is_some());
        assert!(bank.get_account(&epoch_stakes_address(5)).is_none());

        // A jump past the entire retained window closes every old account.
        // The new boundary publishes its own snapshot, not a future epoch.
        let bank = bank_at_epoch(&bank_forks, bank, 12);
        assert!(bank.epoch_stakes(12).is_none());
        for epoch in 0..12 {
            assert!(bank.get_account(&epoch_stakes_address(epoch)).is_none());
        }
        assert!(bank.get_account(&epoch_stakes_address(12)).is_some());
        assert!(bank.get_account(&epoch_stakes_address(13)).is_none());
        assert!(restore_epoch_stakes(&bank).is_ok());
    }

    /// Verify the on-chain epoch stakes match what the bank has internally,
    /// across every per-vote-account field defined by SIMD-0676.
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
        let bank_vote_accounts = bank.epoch_vote_accounts(epoch + 1).unwrap().clone();

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
            assert_eq!(entry.accumulated_lamports, view.credits());
            assert_eq!(entry.latest_vote, view.last_voted_slot());
            assert_eq!(entry.root_slot, view.root_slot());
            assert_eq!(entry.bls_signer_index, UNRANKED);
            assert_eq!(
                entry.bls_pubkey_compressed,
                view.bls_pubkey_compressed()
                    .unwrap_or([0; BLS_PUBKEY_COMPRESSED_SIZE])
            );
        }
    }

    #[test]
    fn test_bls_signer_index_matches_bank_rank_map() {
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
        let rank_map = bank
            .epoch_stakes(epoch + 1)
            .unwrap()
            .bls_pubkey_to_rank_map();

        assert_eq!(entries.len(), 1);
        assert_eq!(
            rank_map
                .get_rank_for_vote_pubkey(&entries[0].vote_pubkey)
                .copied(),
            Some(entries[0].bls_signer_index)
        );
        assert_eq!(entries[0].bls_signer_index, 0);
        assert_ne!(
            entries[0].bls_pubkey_compressed,
            [0; BLS_PUBKEY_COMPRESSED_SIZE]
        );
    }

    #[test]
    fn test_snapshot_retains_entries_excluded_by_rank_deduplication() {
        for duplicate_bls_key in [false, true] {
            let validator_keypairs =
                std::array::from_fn::<_, 3, _>(|_| ValidatorVoteKeypairs::new_rand());
            let mut genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
                1_000_000_000,
                &validator_keypairs,
                vec![100; 3],
            )
            .genesis_config;
            let vote_pubkey = validator_keypairs[2].vote_keypair.pubkey();
            let mut account =
                AccountSharedData::from(genesis_config.accounts.get(&vote_pubkey).unwrap().clone());
            let mut state = VoteStateV4::deserialize(account.data(), &vote_pubkey).unwrap();
            if duplicate_bls_key {
                state.bls_pubkey_compressed = Some(
                    validator_keypairs[1]
                        .bls_keypair
                        .public
                        .to_bytes_compressed(),
                );
            } else {
                state.node_pubkey = validator_keypairs[1].node_keypair.pubkey();
            }
            account
                .set_state(&VoteStateVersions::new_v4(state))
                .unwrap();
            genesis_config.accounts.insert(vote_pubkey, account.into());

            let bank = Bank::new_for_tests(&genesis_config);
            update_on_chain_epoch_stakes(&bank);
            let snapshot = bank
                .get_account(&epoch_stakes_address(bank.epoch()))
                .unwrap();
            let entries = stakes_format::get_all_entries(snapshot.data()).unwrap();
            assert_eq!(entries.len(), 3);
            let vote_accounts = bank.epoch_vote_accounts(bank.epoch() + 1).unwrap();
            for (index, keypairs) in validator_keypairs.iter().enumerate() {
                let pubkey = keypairs.vote_keypair.pubkey();
                let entry = entries
                    .iter()
                    .find(|entry| entry.vote_pubkey == pubkey)
                    .unwrap();
                assert_eq!(
                    entry.bls_signer_index,
                    if index == 0 { 0 } else { UNRANKED }
                );
                assert_eq!(entry.delegated_stake, vote_accounts[&pubkey].0);
            }
        }
    }
    #[test]
    fn test_epoch_stakes_restore_rejects_corruption_and_missing_inputs() {
        let (bank, forks) =
            Bank::new_for_tests(&retention_test_genesis()).wrap_with_bank_forks_for_tests();
        let bank = bank_at_epoch(&forks, bank, 1);
        let address = epoch_stakes_address(1);
        let original = bank.get_account(&address).unwrap();
        assert!(restore_epoch_stakes(&bank).is_ok());
        // Cover version, length, epoch, total, both padding areas and every
        // published field, including fields absent from current vote states.
        for offset in [
            0, 4, 8, 16, 24, 32, 64, 96, 104, 112, 114, 116, 118, 128, 176, 184, 192, 200, 208,
            216, 248, 280, 292,
        ] {
            let mut invalid = original.clone();
            let mut data = invalid.data().to_vec();
            data[offset] ^= 1;
            invalid.set_data_from_slice(&data);
            bank.store_account(&address, &invalid);
            assert!(
                restore_epoch_stakes(&bank).is_err(),
                "accepted byte {offset}"
            );
        }
        bank.store_account(&address, &AccountSharedData::default());
        assert!(matches!(
            restore_epoch_stakes(&bank),
            Err(RestoreError::Missing(1))
        ));
        bank.store_account(
            &address,
            &AccountSharedData::new(10, 0, &system_program::id()),
        );
        assert!(matches!(
            restore_epoch_stakes(&bank),
            Err(RestoreError::Missing(1))
        ));
        bank.store_account(&address, &original);
        assert!(restore_epoch_stakes(&bank).is_ok());
    }

    #[test]
    fn test_epoch_stakes_restore_preserves_old_reward_history_and_authorities() {
        let (bank, forks) =
            Bank::new_for_tests(&retention_test_genesis()).wrap_with_bank_forks_for_tests();
        let key = *bank.epoch_vote_accounts(1).unwrap().keys().next().unwrap();
        let mut account = bank.get_account(&key).unwrap();
        let mut state = VoteStateV4::deserialize(account.data(), &key).unwrap();
        state.epoch_credits = vec![(0, 100, 50), (1, 110, 100)];
        account
            .set_state(&VoteStateVersions::new_v4(state))
            .unwrap();
        bank.store_account(&key, &account);
        let bank = bank_at_epoch(&forks, bank, 20);
        let entry = stakes_format::get_entry(
            bank.get_account(&epoch_stakes_address(20)).unwrap().data(),
            0,
        )
        .unwrap();
        assert_eq!(
            (
                entry.last_reward_epoch,
                entry.accumulated_lamports,
                entry.previous_accumulated_lamports
            ),
            (Some(1), 110, 100)
        );
        let restored = restore_epoch_stakes(&bank).unwrap();
        assert_eq!(
            restored[&21].stakes().vote_accounts().as_ref(),
            bank.epoch_vote_accounts(21).unwrap()
        );
        assert_eq!(
            restored[&21].epoch_authorized_voters(),
            bank.epoch_stakes(21).unwrap().epoch_authorized_voters()
        );
    }

    #[test]
    fn test_epoch_stakes_full_and_incremental_snapshot_restore() {
        use {
            crate::{
                runtime_config::RuntimeConfig,
                snapshot_bank_utils::{
                    bank_from_snapshot_archives, bank_to_full_snapshot_archive,
                    bank_to_incremental_snapshot_archive,
                },
            },
            agave_snapshots::snapshot_config::SnapshotConfig,
            solana_accounts_db::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING,
            solana_hash::Hash,
            tempfile::TempDir,
        };
        let genesis = retention_test_genesis();
        let (mut bank, forks) = Bank::new_for_tests(&genesis).wrap_with_bank_forks_for_tests();
        for epoch in 1..=9 {
            bank = bank_at_epoch(&forks, bank, epoch);
        }
        bank.fill_bank_with_ticks_for_tests();
        bank.set_block_id(Some(Hash::default()));
        let dirs: [TempDir; 3] = std::array::from_fn(|_| TempDir::new().unwrap());
        let config = SnapshotConfig {
            bank_snapshots_dir: dirs[0].path().to_path_buf(),
            full_snapshot_archives_dir: dirs[1].path().to_path_buf(),
            incremental_snapshot_archives_dir: dirs[2].path().to_path_buf(),
            ..SnapshotConfig::default()
        };
        let full = bank_to_full_snapshot_archive(&config, &bank).unwrap();
        let restore = |bank: &Bank, incremental| {
            let accounts = TempDir::new().unwrap();
            let restored = bank_from_snapshot_archives(
                &[accounts.path().to_path_buf()],
                &full,
                incremental,
                &config,
                &genesis,
                &RuntimeConfig::default(),
                None,
                Some(*bank.leader()),
                None,
                false,
                false,
                ACCOUNTS_DB_CONFIG_FOR_TESTING,
                None,
                Arc::default(),
            )
            .unwrap();
            assert_eq!(restored.epoch(), bank.epoch());
            assert_eq!(
                restored.epoch_stakes_map().len(),
                bank.epoch_stakes_map().len()
            );
            for (epoch, expected) in bank.epoch_stakes_map() {
                assert_eq!(
                    restored
                        .epoch_stakes(*epoch)
                        .unwrap()
                        .stakes()
                        .vote_accounts(),
                    expected.stakes().vote_accounts()
                );
            }
            for epoch in bank.epoch().saturating_sub(7)..=bank.epoch() {
                let address = epoch_stakes_address(epoch);
                assert_eq!(restored.get_account(&address), bank.get_account(&address));
            }
        };
        restore(&bank, None);
        let full_slot = bank.slot();
        bank = bank_at_epoch(&forks, bank, 10);
        bank.fill_bank_with_ticks_for_tests();
        bank.set_block_id(Some(Hash::default()));
        let incremental = bank_to_incremental_snapshot_archive(&config, &bank, full_slot).unwrap();
        restore(&bank, Some(&incremental));
    }
    #[test]
    fn test_epoch_stakes_without_eligible_bls_signers() {
        let bank = Bank::new_for_tests(&retention_test_genesis());
        let mut accounts = bank.epoch_vote_accounts(1).unwrap().clone();
        for (key, (_, vote_account)) in &mut accounts {
            let mut account = vote_account.account().clone();
            let mut state = VoteStateV4::deserialize(account.data(), key).unwrap();
            state.bls_pubkey_compressed = None;
            account
                .set_state(&VoteStateVersions::new_v4(state))
                .unwrap();
            *vote_account = VoteAccount::try_from(account).unwrap();
        }
        let stakes = VersionedEpochStakes::new_for_tests(accounts, 1);
        let entries = snapshot_entries(&stakes, true);
        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .all(|entry| entry.bls_signer_index == UNRANKED)
        );
        let empty = VersionedEpochStakes::new_for_tests(HashMap::new(), 1);
        assert!(snapshot_entries(&empty, true).is_empty());
    }
}
