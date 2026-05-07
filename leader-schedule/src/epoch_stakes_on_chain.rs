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
//! ┌────────────────────────────────────────────────────────────┐
//! │ Header (32 bytes)                                          │
//! │   version: u32          — format version (currently 1)     │
//! │   num_entries: u32      — vote accounts in table           │
//! │   epoch: u64            — epoch these stakes are for       │
//! │   total_stake: u64      — sum of all delegated stake       │
//! │   _reserved: [u8; 8]    — must be zero                     │
//! ├────────────────────────────────────────────────────────────┤
//! │ Entries (num_entries × 128 bytes), sorted by vote_pubkey:  │
//! │   vote_pubkey:           Pubkey  (32 bytes, offset   0)    │
//! │   node_pubkey:           Pubkey  (32 bytes, offset  32)    │
//! │   commission_collector:  Pubkey  (32 bytes, offset  64)    │
//! │   delegated_stake:       u64     ( 8 bytes, offset  96)    │
//! │   cumulative_credits:    u64     ( 8 bytes, offset 104)    │
//! │   commission:            u8      ( 1 byte,  offset 112)    │
//! │   _reserved:             [u8;15] (15 bytes, offset 113)    │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! Per-entry size is 128 bytes so subsequent entries remain on a
//! 32-byte boundary, preserving zero-copy `Pubkey` reads. The
//! `commission_collector` field reserves space for the per-vote-account
//! collector address introduced by SIMD-0232. Until that SIMD is active,
//! callers MUST populate it with `node_pubkey`.

use {solana_clock::Epoch, solana_pubkey::Pubkey};

/// Current format version.
pub const VERSION: u32 = 1;

/// Size of the fixed header in bytes. Padded to 32 bytes so entries
/// start on a 32-byte boundary.
pub const HEADER_SIZE: usize = 32;

/// Size of one entry. 128 bytes, multiple of 32 to preserve Pubkey
/// alignment for every entry.
pub const ENTRY_SIZE: usize = 128;

// Field offsets within an entry.
const ENTRY_VOTE_PUBKEY_OFFSET: usize = 0;
const ENTRY_NODE_PUBKEY_OFFSET: usize = 32;
const ENTRY_COMMISSION_COLLECTOR_OFFSET: usize = 64;
const ENTRY_DELEGATED_STAKE_OFFSET: usize = 96;
const ENTRY_CUMULATIVE_CREDITS_OFFSET: usize = 104;
const ENTRY_COMMISSION_OFFSET: usize = 112;

/// One row in the epoch stakes table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochStakesEntry {
    /// The vote account address. Primary key; entries are sorted by this.
    pub vote_pubkey: Pubkey,
    /// The validator identity address operating this vote account.
    pub node_pubkey: Pubkey,
    /// Address that collects validator commission for this vote account.
    /// Reserved for SIMD-0232. Set to `node_pubkey` until SIMD-0232 is
    /// active.
    pub commission_collector: Pubkey,
    /// Total stake delegated to this vote account, in lamports.
    pub delegated_stake: u64,
    /// Cumulative epoch credits earned by this vote account through the
    /// epoch this account represents.
    pub cumulative_credits: u64,
    /// Validator commission as a percentage in `[0, 100]`.
    pub commission: u8,
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

    let data_len = HEADER_SIZE + num_entries * ENTRY_SIZE;
    let mut data = vec![0u8; data_len];

    // Header.
    data[0..4].copy_from_slice(&VERSION.to_le_bytes());
    data[4..8].copy_from_slice(&(num_entries as u32).to_le_bytes());
    data[8..16].copy_from_slice(&epoch.to_le_bytes());
    data[16..24].copy_from_slice(&total_stake.to_le_bytes());
    // Reserved bytes [24..32] left as zero.

    // Entries.
    for (i, entry) in sorted.iter().enumerate() {
        let base = HEADER_SIZE + i * ENTRY_SIZE;
        data[base + ENTRY_VOTE_PUBKEY_OFFSET..base + ENTRY_VOTE_PUBKEY_OFFSET + 32]
            .copy_from_slice(entry.vote_pubkey.as_ref());
        data[base + ENTRY_NODE_PUBKEY_OFFSET..base + ENTRY_NODE_PUBKEY_OFFSET + 32]
            .copy_from_slice(entry.node_pubkey.as_ref());
        data[base + ENTRY_COMMISSION_COLLECTOR_OFFSET
            ..base + ENTRY_COMMISSION_COLLECTOR_OFFSET + 32]
            .copy_from_slice(entry.commission_collector.as_ref());
        data[base + ENTRY_DELEGATED_STAKE_OFFSET..base + ENTRY_DELEGATED_STAKE_OFFSET + 8]
            .copy_from_slice(&entry.delegated_stake.to_le_bytes());
        data[base + ENTRY_CUMULATIVE_CREDITS_OFFSET..base + ENTRY_CUMULATIVE_CREDITS_OFFSET + 8]
            .copy_from_slice(&entry.cumulative_credits.to_le_bytes());
        data[base + ENTRY_COMMISSION_OFFSET] = entry.commission;
        // Reserved bytes [113..128] left as zero.
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

    let expected_len = HEADER_SIZE + num_entries as usize * ENTRY_SIZE;
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

/// Look up an entry by index.
pub fn get_entry(data: &[u8], index: usize) -> Option<EpochStakesEntry> {
    let header = deserialize_header(data)?;
    if index >= header.num_entries as usize {
        return None;
    }

    let base = HEADER_SIZE + index * ENTRY_SIZE;
    let vote_pubkey = Pubkey::from(
        <[u8; 32]>::try_from(
            &data[base + ENTRY_VOTE_PUBKEY_OFFSET..base + ENTRY_VOTE_PUBKEY_OFFSET + 32],
        )
        .ok()?,
    );
    let node_pubkey = Pubkey::from(
        <[u8; 32]>::try_from(
            &data[base + ENTRY_NODE_PUBKEY_OFFSET..base + ENTRY_NODE_PUBKEY_OFFSET + 32],
        )
        .ok()?,
    );
    let commission_collector = Pubkey::from(
        <[u8; 32]>::try_from(
            &data[base + ENTRY_COMMISSION_COLLECTOR_OFFSET
                ..base + ENTRY_COMMISSION_COLLECTOR_OFFSET + 32],
        )
        .ok()?,
    );
    let delegated_stake = u64::from_le_bytes(
        data[base + ENTRY_DELEGATED_STAKE_OFFSET..base + ENTRY_DELEGATED_STAKE_OFFSET + 8]
            .try_into()
            .ok()?,
    );
    let cumulative_credits = u64::from_le_bytes(
        data[base + ENTRY_CUMULATIVE_CREDITS_OFFSET..base + ENTRY_CUMULATIVE_CREDITS_OFFSET + 8]
            .try_into()
            .ok()?,
    );
    let commission = data[base + ENTRY_COMMISSION_OFFSET];

    Some(EpochStakesEntry {
        vote_pubkey,
        node_pubkey,
        commission_collector,
        delegated_stake,
        cumulative_credits,
        commission,
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
mod tests {
    use super::*;

    fn make_entry(seed: u8, stake: u64) -> EpochStakesEntry {
        EpochStakesEntry {
            vote_pubkey: Pubkey::new_unique(),
            node_pubkey: Pubkey::new_unique(),
            commission_collector: Pubkey::new_unique(),
            delegated_stake: stake,
            cumulative_credits: u64::from(seed) * 1_000,
            commission: seed,
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
        // 32 + 256_000 = 256_032 bytes ≈ 250 KB
        assert_eq!(expected_size, 256_032);
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
        assert_eq!(ENTRY_COMMISSION_COLLECTOR_OFFSET % 32, 0);
        // u64 fields are 8-byte aligned.
        assert_eq!(ENTRY_DELEGATED_STAKE_OFFSET % 8, 0);
        assert_eq!(ENTRY_CUMULATIVE_CREDITS_OFFSET % 8, 0);
        // Entry size keeps subsequent entries 32-byte aligned.
        assert_eq!(ENTRY_SIZE % 32, 0);
        // Header keeps the entries section 32-byte aligned.
        assert_eq!(HEADER_SIZE % 32, 0);
    }
}
