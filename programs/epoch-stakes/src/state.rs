//! On-chain epoch stakes account binary format.
//!
//! Defines the binary layout for epoch stakes accounts stored on-chain.
//! These accounts store the per-vote-account data the runtime publishes
//! at every epoch boundary, and are managed by the runtime exclusively.
//! See SIMD-0511 for the design rationale.
//!
//! ## Account Layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │ Header (32 bytes)                                                │
//! │   version: u32          - format version (currently 1)           │
//! │   num_entries: u32      - vote accounts in table                 │
//! │   epoch: u64            - epoch these stakes are for             │
//! │   total_stake: u64      - sum of all delegated stake             │
//! │   _reserved: [u8; 8]    - must be zero                           │
//! ├──────────────────────────────────────────────────────────────────┤
//! │ Entries (num_entries x 224 bytes), sorted by vote_pubkey:        │
//! │   vote_pubkey:                      Pubkey  (32 B, offset   0)   │
//! │   node_pubkey:                      Pubkey  (32 B, offset  32)   │
//! │   inflation_rewards_collector:      Pubkey  (32 B, offset  64)   │
//! │   block_revenue_collector:          Pubkey  (32 B, offset  96)   │
//! │   delegated_stake:                  u64     ( 8 B, offset 128)   │
//! │   cumulative_credits:               u64     ( 8 B, offset 136)   │
//! │   inflation_rewards_commission_bps: u16     ( 2 B, offset 144)   │
//! │   block_revenue_commission_bps:     u16     ( 2 B, offset 146)   │
//! │   alpenglow_rank:                   u16     ( 2 B, offset 148)   │
//! │   _reserved:                        [u8;10] (10 B, offset 150)   │
//! │   bls_pubkey_compressed:            [u8;48](48 B, offset 160)   │
//! │   _reserved:                        [u8;16] (16 B, offset 208)   │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Per-entry size is 224 bytes so subsequent entries remain on a
//! 32-byte boundary, preserving zero-copy `Pubkey` reads. The collector
//! and commission fields mirror the vote account v4 state introduced by
//! SIMD-0185 and consumed by SIMD-0232. For vote accounts whose state
//! predates v4, callers MUST populate the fields with the SIMD-0185
//! migration defaults: `inflation_rewards_collector = vote_pubkey`,
//! `block_revenue_collector = node_pubkey`,
//! `inflation_rewards_commission_bps = 100 * commission`, and
//! `block_revenue_commission_bps = 10_000`.

use {solana_clock::Epoch, solana_pubkey::Pubkey};

/// Current format version.
pub const VERSION: u32 = 1;

/// Size of the fixed header in bytes. Padded to 32 bytes so entries
/// start on a 32-byte boundary.
pub const HEADER_SIZE: usize = 32;

/// Size of one compressed BLS public key in vote account v4.
pub const BLS_PUBKEY_COMPRESSED_SIZE: usize = 48;

/// Value used when a vote account has no Alpenglow validator rank.
pub const UNRANKED: u16 = u16::MAX;

/// Size of one entry. 224 bytes, multiple of 32 to preserve Pubkey
/// alignment for every entry.
pub const ENTRY_SIZE: usize = 224;

// Field offsets within an entry.
const ENTRY_VOTE_PUBKEY_OFFSET: usize = 0;
const ENTRY_NODE_PUBKEY_OFFSET: usize = 32;
const ENTRY_INFLATION_REWARDS_COLLECTOR_OFFSET: usize = 64;
const ENTRY_BLOCK_REVENUE_COLLECTOR_OFFSET: usize = 96;
const ENTRY_DELEGATED_STAKE_OFFSET: usize = 128;
const ENTRY_CUMULATIVE_CREDITS_OFFSET: usize = 136;
const ENTRY_INFLATION_REWARDS_COMMISSION_BPS_OFFSET: usize = 144;
const ENTRY_BLOCK_REVENUE_COMMISSION_BPS_OFFSET: usize = 146;
const ENTRY_ALPENGLOW_RANK_OFFSET: usize = 148;
const ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET: usize = 160;

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
    /// Address that collects the inflation rewards commission for this
    /// vote account (SIMD-0185/SIMD-0232). Set to `vote_pubkey` for vote
    /// accounts whose state predates vote account v4.
    pub inflation_rewards_collector: Pubkey,
    /// Address that collects block fee revenue for this vote account
    /// (SIMD-0185/SIMD-0232). Set to `node_pubkey` for vote accounts
    /// whose state predates vote account v4.
    pub block_revenue_collector: Pubkey,
    /// Total stake delegated to this vote account, in lamports.
    pub delegated_stake: u64,
    /// Cumulative epoch credits earned by this vote account through the
    /// epoch this account represents.
    pub cumulative_credits: u64,
    /// Inflation rewards commission in basis points `[0, 10000]`.
    pub inflation_rewards_commission_bps: u16,
    /// Block revenue commission in basis points `[0, 10000]`.
    pub block_revenue_commission_bps: u16,
    /// Validator rank used by Alpenglow, or [`UNRANKED`] when this vote
    /// account is not in the active Alpenglow validator set.
    pub alpenglow_rank: u16,
    /// Compressed BLS public key from vote account v4. All zeroes when the
    /// vote account does not have a BLS key.
    pub bls_pubkey_compressed: [u8; BLS_PUBKEY_COMPRESSED_SIZE],
}

/// Serialize epoch stakes into the on-chain binary format.
///
/// Entries are sorted by `vote_pubkey` in the output regardless of input
/// order.
pub fn serialize_epoch_stakes(entries: &[EpochStakesEntry], epoch: Epoch) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|e| e.vote_pubkey);

    let num_entries = sorted.len();
    let total_stake: u64 = sorted.iter().map(|e| e.delegated_stake).sum();

    let data_len =
        serialized_len(num_entries).expect("epoch stakes account size must fit in usize");
    let num_entries = u32::try_from(num_entries).expect("epoch stakes entry count must fit in u32");
    let mut data = vec![0u8; data_len];

    // Header.
    data[0..4].copy_from_slice(&VERSION.to_le_bytes());
    data[4..8].copy_from_slice(&num_entries.to_le_bytes());
    data[8..16].copy_from_slice(&epoch.to_le_bytes());
    data[16..24].copy_from_slice(&total_stake.to_le_bytes());
    // Reserved bytes [24..32] left as zero.

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
            ENTRY_INFLATION_REWARDS_COLLECTOR_OFFSET,
            entry.inflation_rewards_collector.as_ref(),
        );
        write_bytes(
            entry_data,
            ENTRY_BLOCK_REVENUE_COLLECTOR_OFFSET,
            entry.block_revenue_collector.as_ref(),
        );
        write_bytes(
            entry_data,
            ENTRY_DELEGATED_STAKE_OFFSET,
            &entry.delegated_stake.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_CUMULATIVE_CREDITS_OFFSET,
            &entry.cumulative_credits.to_le_bytes(),
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
            ENTRY_ALPENGLOW_RANK_OFFSET,
            &entry.alpenglow_rank.to_le_bytes(),
        );
        write_bytes(
            entry_data,
            ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET,
            &entry.bls_pubkey_compressed,
        );
        // Reserved bytes [150..160] and [208..224] left as zero.
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
/// or the declared sizes exceed the available data.
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
    if data.len() < expected_len {
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
    let vote_pubkey = read_pubkey(entry_data, ENTRY_VOTE_PUBKEY_OFFSET)?;
    let node_pubkey = read_pubkey(entry_data, ENTRY_NODE_PUBKEY_OFFSET)?;
    let inflation_rewards_collector =
        read_pubkey(entry_data, ENTRY_INFLATION_REWARDS_COLLECTOR_OFFSET)?;
    let block_revenue_collector = read_pubkey(entry_data, ENTRY_BLOCK_REVENUE_COLLECTOR_OFFSET)?;
    let delegated_stake = u64::from_le_bytes(read_array(entry_data, ENTRY_DELEGATED_STAKE_OFFSET)?);
    let cumulative_credits =
        u64::from_le_bytes(read_array(entry_data, ENTRY_CUMULATIVE_CREDITS_OFFSET)?);
    let inflation_rewards_commission_bps = u16::from_le_bytes(read_array(
        entry_data,
        ENTRY_INFLATION_REWARDS_COMMISSION_BPS_OFFSET,
    )?);
    let block_revenue_commission_bps = u16::from_le_bytes(read_array(
        entry_data,
        ENTRY_BLOCK_REVENUE_COMMISSION_BPS_OFFSET,
    )?);
    let alpenglow_rank = u16::from_le_bytes(read_array(entry_data, ENTRY_ALPENGLOW_RANK_OFFSET)?);
    let bls_pubkey_compressed = read_array(entry_data, ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET)?;

    Some(EpochStakesEntry {
        vote_pubkey,
        node_pubkey,
        inflation_rewards_collector,
        block_revenue_collector,
        delegated_stake,
        cumulative_credits,
        inflation_rewards_commission_bps,
        block_revenue_commission_bps,
        alpenglow_rank,
        bls_pubkey_compressed,
    })
}

/// Deserialize all entries from account data.
pub fn get_all_entries(data: &[u8]) -> Option<Vec<EpochStakesEntry>> {
    let header = deserialize_header(data)?;
    let mut entries = Vec::with_capacity(header.num_entries as usize);
    for i in 0..header.num_entries as usize {
        entries.push(get_entry(data, i)?);
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
            inflation_rewards_collector: Pubkey::new_from_array([seed.wrapping_add(2); 32]),
            block_revenue_collector: Pubkey::new_from_array([seed.wrapping_add(3); 32]),
            delegated_stake: stake,
            cumulative_credits: u64::from(seed) * 1_000,
            inflation_rewards_commission_bps: u16::from(seed) * 100,
            block_revenue_commission_bps: 10_000 - u16::from(seed) * 100,
            alpenglow_rank: u16::from(seed),
            bls_pubkey_compressed: [seed; BLS_PUBKEY_COMPRESSED_SIZE],
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
        // 32 + 448_000 = 448_032 bytes, about 438 KiB.
        assert_eq!(expected_size, 448_032);
        assert!(expected_size < 10 * 1024 * 1024);
    }

    #[test]
    fn test_truncated_data_returns_none() {
        let entries = vec![make_entry(1, 100)];
        let data = serialize_epoch_stakes(&entries, 0);
        assert!(deserialize_header(&data[..data.len() - 1]).is_none());
    }

    #[test]
    fn test_entry_alignment_offsets() {
        // Pubkey fields within an entry are 32-byte aligned relative to
        // entry start.
        assert_eq!(ENTRY_VOTE_PUBKEY_OFFSET % 32, 0);
        assert_eq!(ENTRY_NODE_PUBKEY_OFFSET % 32, 0);
        assert_eq!(ENTRY_INFLATION_REWARDS_COLLECTOR_OFFSET % 32, 0);
        assert_eq!(ENTRY_BLOCK_REVENUE_COLLECTOR_OFFSET % 32, 0);
        // u64 fields are 8-byte aligned.
        assert_eq!(ENTRY_DELEGATED_STAKE_OFFSET % 8, 0);
        assert_eq!(ENTRY_CUMULATIVE_CREDITS_OFFSET % 8, 0);
        // u16 fields are 2-byte aligned.
        assert_eq!(ENTRY_INFLATION_REWARDS_COMMISSION_BPS_OFFSET % 2, 0);
        assert_eq!(ENTRY_BLOCK_REVENUE_COMMISSION_BPS_OFFSET % 2, 0);
        assert_eq!(ENTRY_ALPENGLOW_RANK_OFFSET % 2, 0);
        assert_eq!(ENTRY_BLS_PUBKEY_COMPRESSED_OFFSET % 32, 0);
        // Entry size keeps subsequent entries 32-byte aligned.
        assert_eq!(ENTRY_SIZE % 32, 0);
        // Header keeps the entries section 32-byte aligned.
        assert_eq!(HEADER_SIZE % 32, 0);
    }
}
