//! On-chain leader schedule account binary format.
//!
//! Defines the raw binary layout for leader schedule accounts stored on-chain.
//! These accounts are managed by the runtime at epoch boundaries and are
//! read-only from the perspective of transactions.
//!
//! ## Account Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │ Header (64 bytes)                                   │
//! │   version: u32              — format version (1)    │
//! │   num_leader_spans: u32                             │
//! │   epoch: u64                                        │
//! │   num_leaders: u16                                  │
//! │   slots_per_span: u16                               │
//! │   _reserved: [u8; 12]       — must be zero          │
//! │   epoch_stakes_hash: [u8; 32] — SHA-256 of input    │
//! ├─────────────────────────────────────────────────────┤
//! │ Identity Table (num_leaders × 64 bytes)             │
//! │   entries: [(Pubkey, Pubkey); num_leaders]           │
//! │   — (identity, vote_account) pairs, sorted by       │
//! │     identity key byte order                         │
//! ├─────────────────────────────────────────────────────┤
//! │ Schedule (num_leader_spans × 2 bytes)               │
//! │   leader_indices: [u16; num_leader_spans]           │
//! └─────────────────────────────────────────────────────┘
//! ```

use {
    crate::SlotLeader,
    solana_clock::Epoch,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

/// Current format version.
pub const VERSION: u32 = 1;

/// Size of the fixed header in bytes (64 = 20 data + 12 reserved + 32 hash).
/// Identity table starts at offset 64, aligned to 32 bytes.
pub const HEADER_SIZE: usize = 64;

/// Offset of the epoch_stakes_hash field within the header.
const EPOCH_STAKES_HASH_OFFSET: usize = 32;

/// Size of one identity table entry: (identity Pubkey, vote account Pubkey).
pub const IDENTITY_ENTRY_SIZE: usize = 64;

/// Size of one schedule index entry (u16).
pub const SCHEDULE_INDEX_SIZE: usize = 2;

/// Serialize a leader schedule into the on-chain binary format.
///
/// `slots_per_span` is the number of consecutive slots assigned to each
/// leader (currently `NUM_CONSECUTIVE_LEADER_SLOTS`, i.e. 4). It is stored
/// in the header so consumers can decode the schedule without hardcoding
/// the constant.
///
/// `epoch_stakes_hash` is the SHA-256 hash of the epoch stakes that were
/// input to the leader schedule computation, allowing consumers to verify
/// the schedule was derived from the expected stake distribution.
pub fn serialize_leader_schedule(
    slot_leaders: &[SlotLeader],
    epoch: Epoch,
    slots_per_span: usize,
    epoch_stakes_hash: &Hash,
) -> Vec<u8> {
    // Build sorted, deduplicated identity table and index map.
    // Each entry is an (identity, vote_account) pair.
    let mut unique_entries: Vec<SlotLeader> = slot_leaders.to_vec();
    unique_entries.sort_by(|a, b| a.id.cmp(&b.id).then(a.vote_address.cmp(&b.vote_address)));
    unique_entries.dedup();

    let entry_to_index: HashMap<SlotLeader, u16> = unique_entries
        .iter()
        .enumerate()
        .map(|(i, sl)| (*sl, i as u16))
        .collect();

    // One entry per leader span.
    let num_leader_spans = slot_leaders.len().div_ceil(slots_per_span);
    let num_leaders = unique_entries.len();

    let data_len =
        HEADER_SIZE + num_leaders * IDENTITY_ENTRY_SIZE + num_leader_spans * SCHEDULE_INDEX_SIZE;
    let mut data = vec![0u8; data_len];

    // Write header (64 bytes). Reserved bytes [20..32] are left as zero
    // from the vec![0u8; ..] init. Hash occupies [32..64].
    data[0..4].copy_from_slice(&VERSION.to_le_bytes());
    data[4..8].copy_from_slice(&(num_leader_spans as u32).to_le_bytes());
    data[8..16].copy_from_slice(&epoch.to_le_bytes());
    data[16..18].copy_from_slice(&(num_leaders as u16).to_le_bytes());
    data[18..20].copy_from_slice(&(slots_per_span as u16).to_le_bytes());
    data[EPOCH_STAKES_HASH_OFFSET..EPOCH_STAKES_HASH_OFFSET + 32]
        .copy_from_slice(epoch_stakes_hash.as_ref());

    // Write identity table: (identity, vote_account) pairs.
    let identities_start = HEADER_SIZE;
    for (i, entry) in unique_entries.iter().enumerate() {
        let offset = identities_start + i * IDENTITY_ENTRY_SIZE;
        data[offset..offset + 32].copy_from_slice(entry.id.as_ref());
        data[offset + 32..offset + 64].copy_from_slice(entry.vote_address.as_ref());
    }

    // Write schedule indices (one per leader span, using the first slot's leader).
    let schedule_start = identities_start + num_leaders * IDENTITY_ENTRY_SIZE;
    for span in 0..num_leader_spans {
        let slot = span * slots_per_span;
        let leader = &slot_leaders[slot];
        let idx = entry_to_index[leader];
        let offset = schedule_start + span * SCHEDULE_INDEX_SIZE;
        data[offset..offset + SCHEDULE_INDEX_SIZE].copy_from_slice(&idx.to_le_bytes());
    }

    data
}

/// Deserialized header from an on-chain leader schedule account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderScheduleHeader {
    pub version: u32,
    pub num_leader_spans: u32,
    pub epoch: Epoch,
    pub num_leaders: u16,
    pub slots_per_span: u16,
    pub epoch_stakes_hash: Hash,
}

/// Deserialize the header from raw account data.
///
/// Returns `None` if the data is too short, the version is unsupported,
/// or the declared sizes exceed the available data.
pub fn deserialize_header(data: &[u8]) -> Option<LeaderScheduleHeader> {
    if data.len() < HEADER_SIZE {
        return None;
    }
    let version = u32::from_le_bytes(data[0..4].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let num_leader_spans = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let epoch = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let num_leaders = u16::from_le_bytes(data[16..18].try_into().ok()?);
    let slots_per_span = u16::from_le_bytes(data[18..20].try_into().ok()?);
    let epoch_stakes_hash = Hash::new_from_array(
        <[u8; 32]>::try_from(
            &data[EPOCH_STAKES_HASH_OFFSET..EPOCH_STAKES_HASH_OFFSET + 32],
        )
        .ok()?,
    );

    if slots_per_span == 0 {
        return None;
    }

    let expected_len = HEADER_SIZE
        + num_leaders as usize * IDENTITY_ENTRY_SIZE
        + num_leader_spans as usize * SCHEDULE_INDEX_SIZE;
    if data.len() < expected_len {
        return None;
    }

    Some(LeaderScheduleHeader {
        version,
        num_leader_spans,
        epoch,
        num_leaders,
        slots_per_span,
        epoch_stakes_hash,
    })
}

/// Look up the leader identity/vote-account pair for a given leader-span index.
///
/// `span_index` is the 0-based index of the leader span within the epoch
/// (i.e., `slot_index_within_epoch / slots_per_span`).
pub fn get_leader_at_span(data: &[u8], span_index: usize) -> Option<SlotLeader> {
    let header = deserialize_header(data)?;
    if span_index >= header.num_leader_spans as usize {
        return None;
    }

    let schedule_start = HEADER_SIZE + header.num_leaders as usize * IDENTITY_ENTRY_SIZE;
    let idx_offset = schedule_start + span_index * SCHEDULE_INDEX_SIZE;
    let leader_idx = u16::from_le_bytes(
        data[idx_offset..idx_offset + SCHEDULE_INDEX_SIZE]
            .try_into()
            .ok()?,
    ) as usize;

    if leader_idx >= header.num_leaders as usize {
        return None;
    }

    let entry_offset = HEADER_SIZE + leader_idx * IDENTITY_ENTRY_SIZE;
    let id = Pubkey::from(<[u8; 32]>::try_from(&data[entry_offset..entry_offset + 32]).ok()?);
    let vote_address =
        Pubkey::from(<[u8; 32]>::try_from(&data[entry_offset + 32..entry_offset + 64]).ok()?);
    Some(SlotLeader { id, vote_address })
}

/// Look up the leader identity/vote-account pair for a given slot within the epoch.
///
/// `slot_index` is the 0-based slot offset within the epoch. The
/// `slots_per_span` value is read from the header.
pub fn get_leader_at_slot_index(data: &[u8], slot_index: usize) -> Option<SlotLeader> {
    let header = deserialize_header(data)?;
    let span_index = slot_index / header.slots_per_span as usize;
    get_leader_at_span(data, span_index)
}

/// Deserialize the full identity table from account data.
///
/// Returns `(identity, vote_account)` pairs in their stored (sorted) order.
pub fn get_identity_entries(data: &[u8]) -> Option<Vec<SlotLeader>> {
    let header = deserialize_header(data)?;
    let mut entries = Vec::with_capacity(header.num_leaders as usize);
    for i in 0..header.num_leaders as usize {
        let offset = HEADER_SIZE + i * IDENTITY_ENTRY_SIZE;
        let id = Pubkey::from(<[u8; 32]>::try_from(&data[offset..offset + 32]).ok()?);
        let vote_address =
            Pubkey::from(<[u8; 32]>::try_from(&data[offset + 32..offset + 64]).ok()?);
        entries.push(SlotLeader { id, vote_address });
    }
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOTS_PER_SPAN: usize = 4;
    const TEST_HASH: Hash = Hash::new_from_array([0xAB; 32]);

    fn make_slot_leader(id: Pubkey) -> SlotLeader {
        SlotLeader {
            id,
            vote_address: Pubkey::new_unique(),
        }
    }

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let leaders = [
            make_slot_leader(Pubkey::new_unique()),
            make_slot_leader(Pubkey::new_unique()),
            make_slot_leader(Pubkey::new_unique()),
        ];
        // 12 slots = 3 spans of 4
        let slot_leaders: Vec<SlotLeader> = vec![
            leaders[0], leaders[0], leaders[0], leaders[0], // span 0
            leaders[1], leaders[1], leaders[1], leaders[1], // span 1
            leaders[2], leaders[2], leaders[2], leaders[2], // span 2
        ];

        let epoch = 42;
        let data = serialize_leader_schedule(&slot_leaders, epoch, SLOTS_PER_SPAN, &TEST_HASH);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.version, VERSION);
        assert_eq!(header.num_leader_spans, 3);
        assert_eq!(header.epoch, epoch);
        assert_eq!(header.num_leaders, 3);
        assert_eq!(header.slots_per_span, 4);
        assert_eq!(header.epoch_stakes_hash, TEST_HASH);

        // Verify entries are sorted by identity
        let entries = get_identity_entries(&data).unwrap();
        assert_eq!(entries.len(), 3);
        for i in 0..entries.len() - 1 {
            assert!(entries[i].id < entries[i + 1].id);
        }

        // Verify leader lookups return correct identity + vote pairs
        let span0 = get_leader_at_span(&data, 0).unwrap();
        assert_eq!(span0.id, leaders[0].id);
        assert_eq!(span0.vote_address, leaders[0].vote_address);

        let span1 = get_leader_at_span(&data, 1).unwrap();
        assert_eq!(span1.id, leaders[1].id);
        assert_eq!(span1.vote_address, leaders[1].vote_address);

        let span2 = get_leader_at_span(&data, 2).unwrap();
        assert_eq!(span2.id, leaders[2].id);

        // Verify slot-level lookups
        assert_eq!(
            get_leader_at_slot_index(&data, 0).unwrap().id,
            leaders[0].id
        );
        assert_eq!(
            get_leader_at_slot_index(&data, 3).unwrap().id,
            leaders[0].id
        );
        assert_eq!(
            get_leader_at_slot_index(&data, 4).unwrap().id,
            leaders[1].id
        );
        assert_eq!(
            get_leader_at_slot_index(&data, 11).unwrap().id,
            leaders[2].id
        );

        // Out of bounds
        assert!(get_leader_at_span(&data, 3).is_none());
    }

    #[test]
    fn test_single_leader() {
        let leader = make_slot_leader(Pubkey::new_unique());
        let slot_leaders = vec![leader; 8]; // 2 spans
        let data = serialize_leader_schedule(&slot_leaders, 100, SLOTS_PER_SPAN, &TEST_HASH);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.num_leaders, 1);
        assert_eq!(header.num_leader_spans, 2);

        let result = get_leader_at_span(&data, 0).unwrap();
        assert_eq!(result.id, leader.id);
        assert_eq!(result.vote_address, leader.vote_address);
    }

    #[test]
    fn test_same_identity_multiple_vote_accounts() {
        let identity = Pubkey::new_unique();
        let leader_a = SlotLeader {
            id: identity,
            vote_address: Pubkey::new_unique(),
        };
        let leader_b = SlotLeader {
            id: identity,
            vote_address: Pubkey::new_unique(),
        };
        let slot_leaders: Vec<SlotLeader> = vec![
            leader_a, leader_a, leader_a, leader_a, // span 0
            leader_b, leader_b, leader_b, leader_b, // span 1
        ];
        let data = serialize_leader_schedule(&slot_leaders, 1, SLOTS_PER_SPAN, &TEST_HASH);

        let header = deserialize_header(&data).unwrap();
        // Two distinct entries even though same identity
        assert_eq!(header.num_leaders, 2);

        let span0 = get_leader_at_span(&data, 0).unwrap();
        let span1 = get_leader_at_span(&data, 1).unwrap();
        assert_eq!(span0.id, identity);
        assert_eq!(span1.id, identity);
        assert_ne!(span0.vote_address, span1.vote_address);
    }

    #[test]
    fn test_non_standard_slots_per_span() {
        let leaders = [
            make_slot_leader(Pubkey::new_unique()),
            make_slot_leader(Pubkey::new_unique()),
        ];
        let slot_leaders: Vec<SlotLeader> = vec![
            leaders[0], leaders[0], // span 0 (2 slots)
            leaders[1], leaders[1], // span 1 (2 slots)
        ];
        let data = serialize_leader_schedule(&slot_leaders, 1, 2, &TEST_HASH);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.slots_per_span, 2);
        assert_eq!(header.num_leader_spans, 2);

        assert_eq!(
            get_leader_at_slot_index(&data, 0).unwrap().id,
            leaders[0].id
        );
        assert_eq!(
            get_leader_at_slot_index(&data, 1).unwrap().id,
            leaders[0].id
        );
        assert_eq!(
            get_leader_at_slot_index(&data, 2).unwrap().id,
            leaders[1].id
        );
        assert_eq!(
            get_leader_at_slot_index(&data, 3).unwrap().id,
            leaders[1].id
        );
    }

    #[test]
    fn test_unknown_version_returns_none() {
        let leader = make_slot_leader(Pubkey::new_unique());
        let slot_leaders = vec![leader; 4];
        let mut data = serialize_leader_schedule(&slot_leaders, 0, SLOTS_PER_SPAN, &TEST_HASH);

        // Overwrite version to 99
        data[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(deserialize_header(&data).is_none());
    }

    #[test]
    fn test_account_size_mainnet_scale() {
        let num_validators = 2000;
        let slots_per_epoch = 432_000;
        let num_spans = slots_per_epoch / SLOTS_PER_SPAN;

        let expected_size =
            HEADER_SIZE + num_validators * IDENTITY_ENTRY_SIZE + num_spans * SCHEDULE_INDEX_SIZE;

        // 64 + 128000 + 216000 = 344064 bytes ≈ 336 KB
        assert_eq!(expected_size, 344_064);
        assert!(expected_size < 10 * 1024 * 1024); // well under 10MB limit
    }

    #[test]
    fn test_empty_data_returns_none() {
        assert!(deserialize_header(&[]).is_none());
        assert!(deserialize_header(&[0; 8]).is_none());
        assert!(get_leader_at_span(&[], 0).is_none());
    }

    #[test]
    fn test_truncated_data_returns_none() {
        let leader = make_slot_leader(Pubkey::new_unique());
        let slot_leaders = vec![leader; 4];
        let data = serialize_leader_schedule(&slot_leaders, 0, SLOTS_PER_SPAN, &TEST_HASH);

        // Truncate the data
        assert!(deserialize_header(&data[..data.len() - 1]).is_none());
    }
}
