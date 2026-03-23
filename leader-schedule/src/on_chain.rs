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
//! │ Header (16 bytes)                                   │
//! │   epoch: u64                                        │
//! │   num_leaders: u16                                  │
//! │   num_slot_blocks: u32                              │
//! │   slots_per_block: u16                              │
//! ├─────────────────────────────────────────────────────┤
//! │ Identity Table (num_leaders × 32 bytes)             │
//! │   identities: [Pubkey; num_leaders]                 │
//! ├─────────────────────────────────────────────────────┤
//! │ Schedule (num_slot_blocks × 2 bytes)                │
//! │   leader_indices: [u16; num_slot_blocks]            │
//! └─────────────────────────────────────────────────────┘
//! ```

use {
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

/// Size of the fixed header in bytes.
pub const HEADER_SIZE: usize = 16;

/// Size of one identity entry (a single Pubkey).
pub const IDENTITY_SIZE: usize = 32;

/// Size of one schedule index entry (u16).
pub const SCHEDULE_INDEX_SIZE: usize = 2;

/// Serialize a leader schedule into the on-chain binary format.
///
/// `slots_per_block` is the number of consecutive slots assigned to each
/// leader (currently `NUM_CONSECUTIVE_LEADER_SLOTS`, i.e. 4). It is stored
/// in the header so consumers can decode the schedule without hardcoding
/// the constant.
pub fn serialize_leader_schedule(
    slot_leaders: &[Pubkey],
    epoch: Epoch,
    slots_per_block: usize,
) -> Vec<u8> {
    // Build sorted, deduplicated identity table and index map.
    let mut unique_identities: Vec<Pubkey> = slot_leaders.iter().copied().collect();
    unique_identities.sort();
    unique_identities.dedup();

    let identity_to_index: HashMap<Pubkey, u16> = unique_identities
        .iter()
        .enumerate()
        .map(|(i, pk)| (*pk, i as u16))
        .collect();

    // One entry per leader block.
    let num_slot_blocks = (slot_leaders.len() + slots_per_block - 1) / slots_per_block;
    let num_leaders = unique_identities.len();

    let data_len = HEADER_SIZE
        + num_leaders * IDENTITY_SIZE
        + num_slot_blocks * SCHEDULE_INDEX_SIZE;
    let mut data = vec![0u8; data_len];

    // Write header.
    data[0..8].copy_from_slice(&epoch.to_le_bytes());
    data[8..10].copy_from_slice(&(num_leaders as u16).to_le_bytes());
    data[10..14].copy_from_slice(&(num_slot_blocks as u32).to_le_bytes());
    data[14..16].copy_from_slice(&(slots_per_block as u16).to_le_bytes());

    // Write identity table.
    let identities_start = HEADER_SIZE;
    for (i, pubkey) in unique_identities.iter().enumerate() {
        let offset = identities_start + i * IDENTITY_SIZE;
        data[offset..offset + IDENTITY_SIZE].copy_from_slice(pubkey.as_ref());
    }

    // Write schedule indices (one per leader block, using the first slot's leader).
    let schedule_start = identities_start + num_leaders * IDENTITY_SIZE;
    for block in 0..num_slot_blocks {
        let slot = block * slots_per_block;
        let leader = &slot_leaders[slot];
        let idx = identity_to_index[leader];
        let offset = schedule_start + block * SCHEDULE_INDEX_SIZE;
        data[offset..offset + SCHEDULE_INDEX_SIZE].copy_from_slice(&idx.to_le_bytes());
    }

    data
}

/// Deserialized header from an on-chain leader schedule account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderScheduleHeader {
    pub epoch: Epoch,
    pub num_leaders: u16,
    pub num_slot_blocks: u32,
    pub slots_per_block: u16,
}

/// Deserialize the header from raw account data.
pub fn deserialize_header(data: &[u8]) -> Option<LeaderScheduleHeader> {
    if data.len() < HEADER_SIZE {
        return None;
    }
    let epoch = u64::from_le_bytes(data[0..8].try_into().ok()?);
    let num_leaders = u16::from_le_bytes(data[8..10].try_into().ok()?);
    let num_slot_blocks = u32::from_le_bytes(data[10..14].try_into().ok()?);
    let slots_per_block = u16::from_le_bytes(data[14..16].try_into().ok()?);

    if slots_per_block == 0 {
        return None;
    }

    let expected_len = HEADER_SIZE
        + num_leaders as usize * IDENTITY_SIZE
        + num_slot_blocks as usize * SCHEDULE_INDEX_SIZE;
    if data.len() < expected_len {
        return None;
    }

    Some(LeaderScheduleHeader {
        epoch,
        num_leaders,
        num_slot_blocks,
        slots_per_block,
    })
}

/// Look up the leader identity pubkey for a given slot-block index.
///
/// `block_index` is the 0-based index of the leader block within the epoch
/// (i.e., `slot_index_within_epoch / slots_per_block`).
pub fn get_leader_at_block(data: &[u8], block_index: usize) -> Option<Pubkey> {
    let header = deserialize_header(data)?;
    if block_index >= header.num_slot_blocks as usize {
        return None;
    }

    let schedule_start = HEADER_SIZE + header.num_leaders as usize * IDENTITY_SIZE;
    let idx_offset = schedule_start + block_index * SCHEDULE_INDEX_SIZE;
    let leader_idx =
        u16::from_le_bytes(data[idx_offset..idx_offset + SCHEDULE_INDEX_SIZE].try_into().ok()?)
            as usize;

    if leader_idx >= header.num_leaders as usize {
        return None;
    }

    let identity_offset = HEADER_SIZE + leader_idx * IDENTITY_SIZE;
    Some(Pubkey::from(
        <[u8; 32]>::try_from(&data[identity_offset..identity_offset + IDENTITY_SIZE]).ok()?,
    ))
}

/// Look up the leader identity pubkey for a given slot within the epoch.
///
/// `slot_index` is the 0-based slot offset within the epoch. The
/// `slots_per_block` value is read from the header.
pub fn get_leader_at_slot_index(data: &[u8], slot_index: usize) -> Option<Pubkey> {
    let header = deserialize_header(data)?;
    let block_index = slot_index / header.slots_per_block as usize;
    get_leader_at_block(data, block_index)
}

/// Deserialize the full identity table from account data.
pub fn get_identities(data: &[u8]) -> Option<Vec<Pubkey>> {
    let header = deserialize_header(data)?;
    let mut identities = Vec::with_capacity(header.num_leaders as usize);
    for i in 0..header.num_leaders as usize {
        let offset = HEADER_SIZE + i * IDENTITY_SIZE;
        let pubkey = Pubkey::from(
            <[u8; 32]>::try_from(&data[offset..offset + IDENTITY_SIZE]).ok()?,
        );
        identities.push(pubkey);
    }
    Some(identities)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOTS_PER_BLOCK: usize = 4;

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let leaders = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        // 12 slots = 3 blocks of 4
        let slot_leaders: Vec<Pubkey> = vec![
            leaders[0], leaders[0], leaders[0], leaders[0], // block 0
            leaders[1], leaders[1], leaders[1], leaders[1], // block 1
            leaders[2], leaders[2], leaders[2], leaders[2], // block 2
        ];

        let epoch = 42;
        let data = serialize_leader_schedule(&slot_leaders, epoch, SLOTS_PER_BLOCK);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.epoch, epoch);
        assert_eq!(header.num_leaders, 3);
        assert_eq!(header.num_slot_blocks, 3);
        assert_eq!(header.slots_per_block, 4);

        // Verify identities are sorted
        let identities = get_identities(&data).unwrap();
        assert_eq!(identities.len(), 3);
        for i in 0..identities.len() - 1 {
            assert!(identities[i] < identities[i + 1]);
        }

        // Verify leader lookups
        assert_eq!(get_leader_at_block(&data, 0).unwrap(), leaders[0]);
        assert_eq!(get_leader_at_block(&data, 1).unwrap(), leaders[1]);
        assert_eq!(get_leader_at_block(&data, 2).unwrap(), leaders[2]);

        // Verify slot-level lookups
        assert_eq!(get_leader_at_slot_index(&data, 0).unwrap(), leaders[0]);
        assert_eq!(get_leader_at_slot_index(&data, 3).unwrap(), leaders[0]);
        assert_eq!(get_leader_at_slot_index(&data, 4).unwrap(), leaders[1]);
        assert_eq!(get_leader_at_slot_index(&data, 11).unwrap(), leaders[2]);

        // Out of bounds
        assert!(get_leader_at_block(&data, 3).is_none());
    }

    #[test]
    fn test_single_leader() {
        let leader = Pubkey::new_unique();
        let slot_leaders = vec![leader; 8]; // 2 blocks
        let data = serialize_leader_schedule(&slot_leaders, 100, SLOTS_PER_BLOCK);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.num_leaders, 1);
        assert_eq!(header.num_slot_blocks, 2);

        assert_eq!(get_leader_at_block(&data, 0).unwrap(), leader);
        assert_eq!(get_leader_at_block(&data, 1).unwrap(), leader);
    }

    #[test]
    fn test_non_standard_slots_per_block() {
        // Simulate a future where slots_per_block != 4
        let leaders = [Pubkey::new_unique(), Pubkey::new_unique()];
        let slot_leaders: Vec<Pubkey> = vec![
            leaders[0], leaders[0], // block 0 (2 slots)
            leaders[1], leaders[1], // block 1 (2 slots)
        ];
        let data = serialize_leader_schedule(&slot_leaders, 1, 2);

        let header = deserialize_header(&data).unwrap();
        assert_eq!(header.slots_per_block, 2);
        assert_eq!(header.num_slot_blocks, 2);

        assert_eq!(get_leader_at_slot_index(&data, 0).unwrap(), leaders[0]);
        assert_eq!(get_leader_at_slot_index(&data, 1).unwrap(), leaders[0]);
        assert_eq!(get_leader_at_slot_index(&data, 2).unwrap(), leaders[1]);
        assert_eq!(get_leader_at_slot_index(&data, 3).unwrap(), leaders[1]);
    }

    #[test]
    fn test_account_size_mainnet_scale() {
        let num_validators = 2000;
        let slots_per_epoch = 432_000;
        let num_blocks = slots_per_epoch / SLOTS_PER_BLOCK;

        let expected_size = HEADER_SIZE
            + num_validators * IDENTITY_SIZE
            + num_blocks * SCHEDULE_INDEX_SIZE;

        // 16 + 64000 + 216000 = 280016 bytes ≈ 273 KB
        assert_eq!(expected_size, 280_016);
        assert!(expected_size < 10 * 1024 * 1024); // well under 10MB limit
    }

    #[test]
    fn test_empty_data_returns_none() {
        assert!(deserialize_header(&[]).is_none());
        assert!(deserialize_header(&[0; 8]).is_none());
        assert!(get_leader_at_block(&[], 0).is_none());
    }

    #[test]
    fn test_truncated_data_returns_none() {
        let leader = Pubkey::new_unique();
        let slot_leaders = vec![leader; 4];
        let data = serialize_leader_schedule(&slot_leaders, 0, SLOTS_PER_BLOCK);

        // Truncate the data
        assert!(deserialize_header(&data[..data.len() - 1]).is_none());
    }
}
