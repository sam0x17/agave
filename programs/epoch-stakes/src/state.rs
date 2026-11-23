//! On-chain epoch stakes account binary format.
//!
//! Defines the binary layout for epoch stakes accounts stored on-chain.
//! These accounts store the per-vote-account data the runtime publishes
//! at every epoch boundary, and are managed by the runtime exclusively.
//! See SIMD-0676 for the design rationale.
//!
//! ## Account Layout
//!
//! The header contains version, entry count, snapshot epoch and total stake,
//! followed by eight zero bytes. Each 264-byte entry contains vote and node keys,
//! stake, reward counters, commissions, signer index, BLS key, latest vote/root,
//! pending rewards, collectors and WGS84 ECEF location. Offsets match SIMD-0676.
//!
//! Version 1 has immutable field meanings and layout, including zero padding.
//! Reusing padding or changing a field requires a new format version.
//!
//! Entries have a fixed stride of 264 bytes and begin after the 32-byte header.
//! Commission fields preserve raw vote-account v4 values, including values above
//! 10,000. Pre-v4 states use the legacy inflation commission times 100 and a
//! block-revenue commission of 10,000, following SIMD-0185.

use {
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
};

/// Current format version.
pub const VERSION: u32 = 1;

/// Number of snapshot accounts retained: the current epoch and seven previous epochs.
pub const RETAINED_EPOCHS: Epoch = 8;

/// Size of the fixed header in bytes. Padded to 32 bytes so entries
/// start on a 32-byte boundary.
pub const HEADER_SIZE: usize = 32;

/// Size of one compressed BLS public key in vote account v4.
pub const BLS_PUBKEY_COMPRESSED_SIZE: usize = 48;

/// Value used when a vote account has no Alpenglow validator rank.
pub const UNRANKED: u16 = u16::MAX;

/// Size of one serialized entry in bytes.
pub const ENTRY_SIZE: usize = 264;

/// Maximum admitted vote accounts under SIMD-0357.
pub const MAX_ENTRIES: usize = 2_000;

// Field offsets within an entry.
const ENTRY_VOTE_PUBKEY_OFFSET: usize = 0;
const ENTRY_NODE_PUBKEY_OFFSET: usize = 32;
const ENTRY_DELEGATED_STAKE_OFFSET: usize = 64;
const ENTRY_ACCUMULATED_LAMPORTS_OFFSET: usize = 72;
const ENTRY_INFLATION_REWARDS_COMMISSION_BPS_OFFSET: usize = 80;
const ENTRY_BLOCK_REVENUE_COMMISSION_BPS_OFFSET: usize = 82;
const ENTRY_BLS_SIGNER_INDEX_OFFSET: usize = 84;
const ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET: usize = 96;
const ENTRY_LATEST_VOTE_OFFSET: usize = 144;
const ENTRY_ROOT_SLOT_OFFSET: usize = 152;
const ENTRY_PENDING_DELEGATOR_REWARDS_OFFSET: usize = 160;
const ENTRY_LAST_REWARD_EPOCH_OFFSET: usize = 168;
const ENTRY_PREVIOUS_ACCUMULATED_LAMPORTS_OFFSET: usize = 176;
const ENTRY_INFLATION_REWARDS_COLLECTOR_OFFSET: usize = 184;
const ENTRY_BLOCK_REVENUE_COLLECTOR_OFFSET: usize = 216;
const ENTRY_LOCATION_OFFSET: usize = 248;

fn serialized_len(num_entries: usize) -> Option<usize> {
    num_entries
        .checked_mul(ENTRY_SIZE)?
        .checked_add(HEADER_SIZE)
}

fn read_array<const N: usize>(data: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    data.get(offset..end)?.try_into().ok()
}

fn write_bytes(data: &mut [u8], offset: usize, value: &[u8]) {
    let end = offset
        .checked_add(value.len())
        .expect("epoch stakes field offset must fit in usize");
    data.get_mut(offset..end)
        .expect("epoch stakes field must fit in entry")
        .copy_from_slice(value);
}

/// One row in the epoch stakes table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochStakesEntry {
    /// The vote account address. Primary key; entries are sorted by this.
    pub vote_pubkey: Pubkey,
    /// The validator identity address operating this vote account.
    pub node_pubkey: Pubkey,
    /// Effective delegated stake at the snapshot, before partitioned rewards payouts.
    pub delegated_stake: u64,
    /// Cumulative reward counter at the start of `epoch`, or zero for empty history.
    /// Alpenglow increments it in lamports, but migration can carry a Tower
    /// credits offset. Differences measure recorded rewards only across
    /// continuous, unsaturated Alpenglow snapshots of the same account incarnation.
    pub accumulated_lamports: u64,
    /// Raw inflation rewards commission in basis points, without clamping.
    /// With SIMD-0249 active, this snapshot supplies the commission for rewards
    /// earned in `epoch + 1`, subject to its missing-vote-account fallback rules.
    pub inflation_rewards_commission_bps: u16,
    /// Raw block revenue commission in basis points, without clamping.
    pub block_revenue_commission_bps: u16,
    /// Validator rank used by Alpenglow in `epoch + 1`, or [`UNRANKED`] when this vote
    /// account is not in the active Alpenglow validator set.
    pub bls_signer_index: u16,
    /// Compressed BLS public key from vote account v4. All zeroes when the
    /// vote account does not have a BLS key.
    pub bls_pubkey_compressed: [u8; BLS_PUBKEY_COMPRESSED_SIZE],
    /// Last voted slot at the snapshot, encoded as `u64::MAX` when absent.
    pub latest_vote: Option<Slot>,
    /// Root slot at the snapshot, encoded as `u64::MAX` when absent.
    pub root_slot: Option<Slot>,
    /// Pending delegator rewards before the boundary sweep.
    pub pending_delegator_rewards: u64,
    /// Epoch of the latest recorded reward tuple, absent for an empty history.
    pub last_reward_epoch: Option<Epoch>,
    /// Previous counter from that same tuple, including migration offsets.
    pub previous_accumulated_lamports: u64,
    /// Historical collector addresses, not a payment instruction.
    pub inflation_rewards_collector: Pubkey,
    pub block_revenue_collector: Pubkey,
    /// WGS84 ECEF coordinates in meters. Zero means unregistered.
    pub location: [i32; 3],
}

/// Serialize epoch stakes into the on-chain binary format.
///
/// Entries are sorted by `vote_pubkey` in the output regardless of input
/// order.
pub fn serialize_epoch_stakes(entries: &[EpochStakesEntry], epoch: Epoch) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|e| e.vote_pubkey);

    let num_entries = sorted.len();
    assert!(num_entries <= MAX_ENTRIES, "too many epoch stakes entries");
    assert!(
        sorted
            .windows(2)
            .all(|pair| pair[0].vote_pubkey < pair[1].vote_pubkey),
        "duplicate vote key"
    );
    let total_stake = sorted
        .iter()
        .try_fold(0u64, |total, entry| {
            total.checked_add(entry.delegated_stake)
        })
        .expect("epoch stakes total overflow");

    let data_len =
        serialized_len(num_entries).expect("epoch stakes account size must fit in usize");
    let num_entries = u32::try_from(num_entries).expect("epoch stakes entry count must fit in u32");
    let mut data = vec![0u8; data_len];

    // Header.
    data[0..4].copy_from_slice(&VERSION.to_le_bytes());
    data[4..8].copy_from_slice(&num_entries.to_le_bytes());
    data[8..16].copy_from_slice(&epoch.to_le_bytes());
    data[16..24].copy_from_slice(&total_stake.to_le_bytes());
    // Padding bytes [24..32] left as zero.

    // Entries.
    let (entry_data, remainder) = data[HEADER_SIZE..].as_chunks_mut::<ENTRY_SIZE>();
    debug_assert!(remainder.is_empty());
    for (entry_data, entry) in entry_data.iter_mut().zip(&sorted) {
        write_bytes(
            entry_data,
            ENTRY_VOTE_PUBKEY_OFFSET,
            entry.vote_pubkey.as_ref(),
        );
        write_bytes(
            entry_data,
            ENTRY_NODE_PUBKEY_OFFSET,
            entry.node_pubkey.as_ref(),
        );
        write_bytes(
            entry_data,
            ENTRY_DELEGATED_STAKE_OFFSET,
            &entry.delegated_stake.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_ACCUMULATED_LAMPORTS_OFFSET,
            &entry.accumulated_lamports.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_INFLATION_REWARDS_COMMISSION_BPS_OFFSET,
            &entry.inflation_rewards_commission_bps.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_BLOCK_REVENUE_COMMISSION_BPS_OFFSET,
            &entry.block_revenue_commission_bps.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_BLS_SIGNER_INDEX_OFFSET,
            &entry.bls_signer_index.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET,
            &entry.bls_pubkey_compressed,
        );
        write_bytes(
            entry_data,
            ENTRY_LATEST_VOTE_OFFSET,
            &entry.latest_vote.unwrap_or(Slot::MAX).to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_ROOT_SLOT_OFFSET,
            &entry.root_slot.unwrap_or(Slot::MAX).to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_PENDING_DELEGATOR_REWARDS_OFFSET,
            &entry.pending_delegator_rewards.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_LAST_REWARD_EPOCH_OFFSET,
            &entry.last_reward_epoch.unwrap_or(Epoch::MAX).to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_PREVIOUS_ACCUMULATED_LAMPORTS_OFFSET,
            &entry.previous_accumulated_lamports.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_INFLATION_REWARDS_COLLECTOR_OFFSET,
            entry.inflation_rewards_collector.as_ref(),
        );
        write_bytes(
            entry_data,
            ENTRY_BLOCK_REVENUE_COLLECTOR_OFFSET,
            entry.block_revenue_collector.as_ref(),
        );
        for (coordinate, bytes) in entry
            .location
            .iter()
            .zip(entry_data[248..260].as_chunks_mut::<4>().0.iter_mut())
        {
            bytes.copy_from_slice(&coordinate.to_le_bytes());
        }
        // Padding bytes [86..96] and [260..264] left as zero.
    }

    data
}

/// Deserialized header from an on-chain epoch stakes account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochStakesHeader {
    pub version: u32,
    pub num_entries: u32,
    pub epoch: Epoch,
    pub total_stake: u64,
}

/// Deserialize the header from raw account data.
///
/// Returns `None` if the data is too short, the version is unsupported,
/// or the data length differs from the declared size.
pub fn deserialize_header(data: &[u8]) -> Option<EpochStakesHeader> {
    if data.len() < HEADER_SIZE {
        return None;
    }
    let version = u32::from_le_bytes(data[0..4].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let num_entries = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let epoch = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let total_stake = u64::from_le_bytes(data[16..24].try_into().ok()?);

    let expected_len = serialized_len(num_entries as usize)?;
    if data.len() != expected_len || num_entries as usize > MAX_ENTRIES || data[24..32] != [0; 8] {
        return None;
    }

    Some(EpochStakesHeader {
        version,
        num_entries,
        epoch,
        total_stake,
    })
}

fn read_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    Some(Pubkey::from(read_array(data, offset)?))
}

/// Look up an entry by index.
pub fn get_entry(data: &[u8], index: usize) -> Option<EpochStakesEntry> {
    let header = deserialize_header(data)?;
    if index >= header.num_entries as usize {
        return None;
    }

    let start = index.checked_mul(ENTRY_SIZE)?.checked_add(HEADER_SIZE)?;
    let end = start.checked_add(ENTRY_SIZE)?;
    let entry_data = data.get(start..end)?;
    if entry_data[86..96] != [0; 10] || entry_data[260..264] != [0; 4] {
        return None;
    }
    let vote_pubkey = read_pubkey(entry_data, ENTRY_VOTE_PUBKEY_OFFSET)?;
    let node_pubkey = read_pubkey(entry_data, ENTRY_NODE_PUBKEY_OFFSET)?;
    let delegated_stake = u64::from_le_bytes(read_array(entry_data, ENTRY_DELEGATED_STAKE_OFFSET)?);
    let accumulated_lamports =
        u64::from_le_bytes(read_array(entry_data, ENTRY_ACCUMULATED_LAMPORTS_OFFSET)?);
    let inflation_rewards_commission_bps = u16::from_le_bytes(read_array(
        entry_data,
        ENTRY_INFLATION_REWARDS_COMMISSION_BPS_OFFSET,
    )?);
    let block_revenue_commission_bps = u16::from_le_bytes(read_array(
        entry_data,
        ENTRY_BLOCK_REVENUE_COMMISSION_BPS_OFFSET,
    )?);
    let bls_signer_index =
        u16::from_le_bytes(read_array(entry_data, ENTRY_BLS_SIGNER_INDEX_OFFSET)?);
    let bls_pubkey_compressed = read_array(entry_data, ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET)?;
    let latest_vote = u64::from_le_bytes(read_array(entry_data, ENTRY_LATEST_VOTE_OFFSET)?);
    let root_slot = u64::from_le_bytes(read_array(entry_data, ENTRY_ROOT_SLOT_OFFSET)?);

    Some(EpochStakesEntry {
        vote_pubkey,
        node_pubkey,
        delegated_stake,
        accumulated_lamports,
        inflation_rewards_commission_bps,
        block_revenue_commission_bps,
        bls_signer_index,
        bls_pubkey_compressed,
        latest_vote: (latest_vote != Slot::MAX).then_some(latest_vote),
        root_slot: (root_slot != Slot::MAX).then_some(root_slot),
        pending_delegator_rewards: u64::from_le_bytes(read_array(
            entry_data,
            ENTRY_PENDING_DELEGATOR_REWARDS_OFFSET,
        )?),
        last_reward_epoch: {
            let epoch = u64::from_le_bytes(read_array(entry_data, ENTRY_LAST_REWARD_EPOCH_OFFSET)?);
            (epoch != Epoch::MAX).then_some(epoch)
        },
        previous_accumulated_lamports: u64::from_le_bytes(read_array(
            entry_data,
            ENTRY_PREVIOUS_ACCUMULATED_LAMPORTS_OFFSET,
        )?),
        inflation_rewards_collector: read_pubkey(
            entry_data,
            ENTRY_INFLATION_REWARDS_COLLECTOR_OFFSET,
        )?,
        block_revenue_collector: read_pubkey(entry_data, ENTRY_BLOCK_REVENUE_COLLECTOR_OFFSET)?,
        location: [
            i32::from_le_bytes(read_array(entry_data, ENTRY_LOCATION_OFFSET)?),
            i32::from_le_bytes(read_array(entry_data, ENTRY_LOCATION_OFFSET + 4)?),
            i32::from_le_bytes(read_array(entry_data, ENTRY_LOCATION_OFFSET + 8)?),
        ],
    })
}

/// Deserialize all entries from account data.
pub fn get_all_entries(data: &[u8]) -> Option<Vec<EpochStakesEntry>> {
    let header = deserialize_header(data)?;
    let mut entries = Vec::with_capacity(header.num_entries as usize);
    for i in 0..header.num_entries as usize {
        entries.push(get_entry(data, i)?);
    }
    if !entries
        .windows(2)
        .all(|pair| pair[0].vote_pubkey < pair[1].vote_pubkey)
    {
        return None;
    }
    let total = entries.iter().try_fold(0u64, |total, entry| {
        total.checked_add(entry.delegated_stake)
    })?;
    if total != header.total_stake
        || entries.iter().any(|entry| {
            entry.last_reward_epoch.is_none()
                && (entry.accumulated_lamports != 0 || entry.previous_accumulated_lamports != 0)
        })
    {
        return None;
    }
    Some(entries)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn make_entry(seed: u8, stake: u64) -> EpochStakesEntry {
        EpochStakesEntry {
            vote_pubkey: Pubkey::new_from_array([seed; 32]),
            node_pubkey: Pubkey::new_from_array([seed.wrapping_add(1); 32]),
            delegated_stake: stake,
            accumulated_lamports: u64::from(seed) * 1_000,
            inflation_rewards_commission_bps: u16::from(seed) * 100,
            block_revenue_commission_bps: 10_000 - u16::from(seed) * 100,
            bls_signer_index: u16::from(seed),
            bls_pubkey_compressed: [seed; BLS_PUBKEY_COMPRESSED_SIZE],
            latest_vote: Some(u64::from(seed) * 10),
            root_slot: Some(u64::from(seed)),
            pending_delegator_rewards: 123,
            last_reward_epoch: Some(5),
            previous_accumulated_lamports: 17,
            inflation_rewards_collector: Pubkey::new_from_array([42; 32]),
            block_revenue_collector: Pubkey::new_from_array([43; 32]),
            location: [-1, 2, -3],
        }
    }

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let entries = vec![
            make_entry(1, 1_000_000),
            make_entry(2, 2_000_000),
            make_entry(3, 500_000),
        ];

        let epoch = 42;
        let data = serialize_epoch_stakes(&entries, epoch);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.version, VERSION);
        assert_eq!(header.num_entries, 3);
        assert_eq!(header.epoch, epoch);
        assert_eq!(header.total_stake, 3_500_000);

        let out = get_all_entries(&data).unwrap();
        assert_eq!(out.len(), 3);

        // Sorted by vote_pubkey.
        for i in 0..out.len() - 1 {
            assert!(out[i].vote_pubkey < out[i + 1].vote_pubkey);
        }

        // Round-trip preserves every field for each input entry.
        for entry in &entries {
            let found = out
                .iter()
                .find(|e| e.vote_pubkey == entry.vote_pubkey)
                .unwrap();
            assert_eq!(found, entry);
        }
    }

    #[test]
    fn test_single_entry() {
        let entry = make_entry(7, 42);
        let entries = vec![entry];
        let data = serialize_epoch_stakes(&entries, 1);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.num_entries, 1);
        assert_eq!(header.total_stake, 42);

        let out = get_entry(&data, 0).unwrap();
        assert_eq!(out, entry);
        assert!(get_entry(&data, 1).is_none());
    }

    #[test]
    fn test_empty_returns_none() {
        assert!(deserialize_header(&[]).is_none());
        assert!(deserialize_header(&[0; 16]).is_none());
    }

    #[test]
    fn test_unknown_version_returns_none() {
        let entries = vec![make_entry(1, 100)];
        let mut data = serialize_epoch_stakes(&entries, 0);
        data[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(deserialize_header(&data).is_none());
    }

    #[test]
    fn test_account_size_mainnet_scale() {
        let num_validators = 2000;
        let expected_size = HEADER_SIZE + num_validators * ENTRY_SIZE;
        // 32 + 528_000 = 528_032 bytes, about 516 KiB.
        assert_eq!(expected_size, 528_032);
        assert!(expected_size < 10 * 1024 * 1024);
    }

    #[test]
    fn test_truncated_data_returns_none() {
        let entries = vec![make_entry(1, 100)];
        let data = serialize_epoch_stakes(&entries, 0);
        assert!(deserialize_header(&data[..data.len() - 1]).is_none());
    }

    #[test]
    fn test_wire_layout_and_raw_commissions() {
        let entry = EpochStakesEntry {
            inflation_rewards_commission_bps: 10_001,
            block_revenue_commission_bps: u16::MAX,
            ..make_entry(3, 0x0807_0605_0403_0201)
        };
        let data = serialize_epoch_stakes(&[entry], 100);
        assert_eq!(data.len(), 296);
        assert_eq!(&data[8..16], &100u64.to_le_bytes());
        let row = &data[32..];
        assert_eq!(&row[0..32], entry.vote_pubkey.as_ref());
        assert_eq!(&row[32..64], entry.node_pubkey.as_ref());
        assert_eq!(&row[64..72], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&row[72..80], &entry.accumulated_lamports.to_le_bytes());
        assert_eq!(&row[80..82], &10_001u16.to_le_bytes());
        assert_eq!(&row[82..84], &u16::MAX.to_le_bytes());
        assert_eq!(&row[84..86], &3u16.to_le_bytes());
        assert_eq!(&row[86..96], &[0; 10]);
        assert_eq!(&row[96..144], &entry.bls_pubkey_compressed);
        assert_eq!(&row[144..152], &30u64.to_le_bytes());
        assert_eq!(&row[152..160], &3u64.to_le_bytes());
        assert_eq!(&row[160..168], &123u64.to_le_bytes());
        assert_eq!(&row[168..176], &5u64.to_le_bytes());
        assert_eq!(&row[176..184], &17u64.to_le_bytes());
        assert_eq!(&row[184..216], entry.inflation_rewards_collector.as_ref());
        assert_eq!(&row[216..248], entry.block_revenue_collector.as_ref());
        assert_eq!(&row[248..252], &(-1i32).to_le_bytes());
        assert_eq!(&row[252..256], &2i32.to_le_bytes());
        assert_eq!(&row[256..260], &(-3i32).to_le_bytes());
        assert_eq!(&row[260..264], &[0; 4]);
        assert_eq!(get_entry(&data, 0), Some(entry));

        let mut trailing = data;
        trailing.push(0);
        assert!(deserialize_header(&trailing).is_none());
    }

    #[test]
    fn test_optional_health_slots_distinguish_absence_from_zero() {
        for latest_vote in [None, Some(0), Some(Slot::MAX - 1)] {
            for root_slot in [None, Some(0), Some(Slot::MAX - 1)] {
                let entry = EpochStakesEntry {
                    latest_vote,
                    root_slot,
                    ..make_entry(1, 100)
                };
                let data = serialize_epoch_stakes(&[entry], 1);
                assert_eq!(
                    &data[176..184],
                    &latest_vote.unwrap_or(Slot::MAX).to_le_bytes()
                );
                assert_eq!(
                    &data[184..192],
                    &root_slot.unwrap_or(Slot::MAX).to_le_bytes()
                );
                assert_eq!(get_entry(&data, 0), Some(entry));
            }
        }
    }

    #[test]
    fn test_entry_alignment_offsets() {
        // Pubkey fields within an entry are 32-byte aligned relative to
        // entry start.
        assert_eq!(ENTRY_VOTE_PUBKEY_OFFSET % 32, 0);
        assert_eq!(ENTRY_NODE_PUBKEY_OFFSET % 32, 0);
        // u64 fields are 8-byte aligned.
        assert_eq!(ENTRY_DELEGATED_STAKE_OFFSET % 8, 0);
        assert_eq!(ENTRY_ACCUMULATED_LAMPORTS_OFFSET % 8, 0);
        assert_eq!(ENTRY_LATEST_VOTE_OFFSET % 8, 0);
        assert_eq!(ENTRY_ROOT_SLOT_OFFSET % 8, 0);
        // u16 fields are 2-byte aligned.
        assert_eq!(ENTRY_INFLATION_REWARDS_COMMISSION_BPS_OFFSET % 2, 0);
        assert_eq!(ENTRY_BLOCK_REVENUE_COMMISSION_BPS_OFFSET % 2, 0);
        assert_eq!(ENTRY_BLS_SIGNER_INDEX_OFFSET % 2, 0);
        assert_eq!(ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET % 32, 0);
        // Entries maintain integer alignment, not 32-byte key alignment.
        assert_eq!(ENTRY_SIZE % 8, 0);
        // Header keeps the entries section 32-byte aligned.
        assert_eq!(HEADER_SIZE % 32, 0);
    }
    #[test]
    fn test_reject_noncanonical_epoch_stakes() {
        let valid = serialize_epoch_stakes(&[make_entry(1, 100), make_entry(2, 200)], 20);
        for offset in [
            24,
            HEADER_SIZE + 86,
            HEADER_SIZE + 260,
            HEADER_SIZE + ENTRY_SIZE + 86,
        ] {
            let mut invalid = valid.clone();
            invalid[offset] = 1;
            assert!(get_all_entries(&invalid).is_none());
        }
        let mut invalid = valid.clone();
        invalid[16] ^= 1;
        assert!(get_all_entries(&invalid).is_none());
        let mut invalid = valid.clone();
        let key = invalid[32..64].to_vec();
        invalid[32 + ENTRY_SIZE..64 + ENTRY_SIZE].copy_from_slice(&key);
        assert!(get_all_entries(&invalid).is_none());
        let mut invalid = valid.clone();
        invalid[32 + 64..32 + 72].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(get_all_entries(&invalid).is_none());
        let mut invalid = valid;
        invalid[4..8].copy_from_slice(&2001u32.to_le_bytes());
        invalid.resize(HEADER_SIZE + 2001 * ENTRY_SIZE, 0);
        assert!(deserialize_header(&invalid).is_none());
    }

    #[test]
    fn test_empty_reward_history_and_signed_location() {
        let entry = EpochStakesEntry {
            last_reward_epoch: None,
            accumulated_lamports: 0,
            previous_accumulated_lamports: 0,
            location: [i32::MIN, 0, i32::MAX],
            ..make_entry(1, 100)
        };
        let data = serialize_epoch_stakes(&[entry], 1);
        assert_eq!(get_all_entries(&data), Some(vec![entry]));
        assert_eq!(&data[32 + 168..32 + 176], &u64::MAX.to_le_bytes());
        let mut inconsistent = data;
        inconsistent[32 + 72] = 1;
        assert!(get_all_entries(&inconsistent).is_none());
    }
}
