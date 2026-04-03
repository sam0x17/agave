//! On-chain epoch stakes account binary format.
//!
//! Defines the binary layout for epoch stakes accounts stored on-chain.
//! These accounts store the mapping from vote account to delegated stake
//! and are managed by the runtime at epoch boundaries.
//!
//! ## Account Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │ Header (32 bytes)                                   │
//! │   version: u32          — format version (1)        │
//! │   num_entries: u32      — vote accounts in table    │
//! │   epoch: u64            — epoch these stakes are for│
//! │   total_stake: u64      — sum of all stakes         │
//! │   _reserved: [u8; 8]    — must be zero              │
//! ├─────────────────────────────────────────────────────┤
//! │ Entries (num_entries × 40 bytes)                    │
//! │   entries: [(Pubkey, u64); num_entries]             │
//! │   — (vote account, stake in lamports),              │
//! │     sorted by vote account pubkey byte order        │
//! └─────────────────────────────────────────────────────┘
//! ```

use {solana_clock::Epoch, solana_pubkey::Pubkey};

/// Current format version.
pub const VERSION: u32 = 1;

/// Size of the fixed header in bytes.
/// Padded to 32 bytes so entries start on a 32-byte boundary.
pub const HEADER_SIZE: usize = 32;

/// Size of one entry: (Pubkey, u64).
pub const ENTRY_SIZE: usize = 40;

/// Serialize epoch stakes into the on-chain binary format.
///
/// `stakes` is a slice of `(vote_account, delegated_stake)` pairs.
/// They will be sorted by vote account pubkey in the output.
pub fn serialize_epoch_stakes(stakes: &[(Pubkey, u64)], epoch: Epoch) -> Vec<u8> {
    let mut sorted_stakes = stakes.to_vec();
    sorted_stakes.sort_by_key(|(pubkey, _)| *pubkey);

    let num_entries = sorted_stakes.len();
    let total_stake: u64 = sorted_stakes.iter().map(|(_, s)| s).sum();

    let data_len = HEADER_SIZE + num_entries * ENTRY_SIZE;
    let mut data = vec![0u8; data_len];

    // Write header.
    data[0..4].copy_from_slice(&VERSION.to_le_bytes());
    data[4..8].copy_from_slice(&(num_entries as u32).to_le_bytes());
    data[8..16].copy_from_slice(&epoch.to_le_bytes());
    data[16..24].copy_from_slice(&total_stake.to_le_bytes());
    // Reserved bytes [24..32] are left as zero.

    // Write entries.
    for (i, (pubkey, stake)) in sorted_stakes.iter().enumerate() {
        let offset = HEADER_SIZE + i * ENTRY_SIZE;
        data[offset..offset + 32].copy_from_slice(pubkey.as_ref());
        data[offset + 32..offset + 40].copy_from_slice(&stake.to_le_bytes());
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

/// Look up the stake for a given vote account by index.
pub fn get_entry(data: &[u8], index: usize) -> Option<(Pubkey, u64)> {
    let header = deserialize_header(data)?;
    if index >= header.num_entries as usize {
        return None;
    }

    let offset = HEADER_SIZE + index * ENTRY_SIZE;
    let pubkey = Pubkey::from(<[u8; 32]>::try_from(&data[offset..offset + 32]).ok()?);
    let stake = u64::from_le_bytes(data[offset + 32..offset + 40].try_into().ok()?);
    Some((pubkey, stake))
}

/// Deserialize all entries from account data.
pub fn get_all_entries(data: &[u8]) -> Option<Vec<(Pubkey, u64)>> {
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

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let stakes = vec![
            (Pubkey::new_unique(), 1_000_000),
            (Pubkey::new_unique(), 2_000_000),
            (Pubkey::new_unique(), 500_000),
        ];

        let epoch = 42;
        let data = serialize_epoch_stakes(&stakes, epoch);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.version, VERSION);
        assert_eq!(header.num_entries, 3);
        assert_eq!(header.epoch, epoch);
        assert_eq!(header.total_stake, 3_500_000);

        // Verify entries are sorted by pubkey.
        let entries = get_all_entries(&data).unwrap();
        assert_eq!(entries.len(), 3);
        for i in 0..entries.len() - 1 {
            assert!(entries[i].0 < entries[i + 1].0);
        }

        // Verify all original stakes are present.
        let mut original_stakes: Vec<u64> = stakes.iter().map(|(_, s)| *s).collect();
        let mut result_stakes: Vec<u64> = entries.iter().map(|(_, s)| *s).collect();
        original_stakes.sort();
        result_stakes.sort();
        assert_eq!(original_stakes, result_stakes);
    }

    #[test]
    fn test_single_entry() {
        let pubkey = Pubkey::new_unique();
        let stakes = vec![(pubkey, 42)];
        let data = serialize_epoch_stakes(&stakes, 1);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.num_entries, 1);
        assert_eq!(header.total_stake, 42);

        let (pk, stake) = get_entry(&data, 0).unwrap();
        assert_eq!(pk, pubkey);
        assert_eq!(stake, 42);

        assert!(get_entry(&data, 1).is_none());
    }

    #[test]
    fn test_empty_returns_none() {
        assert!(deserialize_header(&[]).is_none());
        assert!(deserialize_header(&[0; 16]).is_none());
    }

    #[test]
    fn test_unknown_version_returns_none() {
        let stakes = vec![(Pubkey::new_unique(), 100)];
        let mut data = serialize_epoch_stakes(&stakes, 0);
        data[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(deserialize_header(&data).is_none());
    }

    #[test]
    fn test_account_size_mainnet_scale() {
        let num_validators = 2000;
        let expected_size = HEADER_SIZE + num_validators * ENTRY_SIZE;
        // 32 + 80000 = 80032 bytes ≈ 80 KB
        assert_eq!(expected_size, 80_032);
        assert!(expected_size < 10 * 1024 * 1024);
    }

    #[test]
    fn test_truncated_data_returns_none() {
        let stakes = vec![(Pubkey::new_unique(), 100)];
        let data = serialize_epoch_stakes(&stakes, 0);
        assert!(deserialize_header(&data[..data.len() - 1]).is_none());
    }
}
