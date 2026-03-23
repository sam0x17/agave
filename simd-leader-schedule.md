---
simd: 'XXXX'
title: On-Chain Leader Schedule
authors:
  - Sam (Anza)
category: Standard
type: Core
status: Draft
created: 2026-03-23
feature: (fill in with feature tracking issues once accepted)
---

## Summary

Store the leader schedule for the current and upcoming epochs in on-chain
accounts, enabling downstream consumers to subscribe to account updates and
on-chain programs to verify leader identity.

## Motivation

The Solana leader schedule is currently only accessible through RPC methods
(`getLeaderSchedule`, `getSlotLeaders`). This creates several problems:

**For indexers and off-chain infrastructure:** There is no way to subscribe to
leader schedule changes. Consumers must poll RPC endpoints, introducing latency
and unnecessary load. With the schedule stored in on-chain accounts, Geyser
plugins and websocket `accountSubscribe` calls can deliver schedule updates in
real time at epoch boundaries.

**For on-chain programs:** The leader schedule is not available on-chain at all.
This makes it impossible to:

- Compute validator skip rates on-chain (the slot history sysvar shows which
  slots were skipped, but cannot tie skips to validator identities)
- Gate program execution based on the current block producer
- Use leader slot share as a proxy for stake weight in on-chain governance

The leader schedule is already deterministically computed by every validator from
epoch vote account stakes. This proposal simply makes that data available as
account state.

## New Terminology

**Leader schedule account:** A system-managed account (not a sysvar — see
[Alternatives Considered](#alternatives-considered)) that stores both the unique
leader identities and the slot-to-leader mapping for a single epoch.

## Detailed Design

### Account Structure

Two accounts are maintained: one for the **current epoch** and one for the
**next epoch**. Each account contains a self-describing binary layout with both
the identity table and the schedule index array.

All multi-byte integers are little-endian.

```
┌─────────────────────────────────────────────────────┐
│ Header (16 bytes)                                   │
│   epoch: u64            — epoch this schedule is for│
│   num_leaders: u16      — unique leaders in table   │
│   num_slot_blocks: u32  — slots_in_epoch / 4        │
│   _reserved: [u8; 2]                                │
├─────────────────────────────────────────────────────┤
│ Identity Table (num_leaders × 32 bytes)             │
│   identities: [Pubkey; num_leaders]                 │
│   — validator identity keys, sorted by byte order   │
├─────────────────────────────────────────────────────┤
│ Schedule (num_slot_blocks × 2 bytes)                │
│   leader_indices: [u16; num_slot_blocks]            │
│   — index into Identity Table per 4-slot block      │
└─────────────────────────────────────────────────────┘
```

#### Size Analysis

With mainnet parameters (432,000 slots/epoch, ~2,000 active validators):

| Component | Calculation | Size |
|-----------|------------|------|
| Header | fixed | 16 bytes |
| Identity Table | 2,000 × 32 bytes | 64 KB |
| Schedule | 108,000 × 2 bytes | 216 KB |
| **Total per account** | | **~280 KB** |
| **Total (2 accounts)** | | **~560 KB** |

The maximum identity table size occurs when stake is distributed equally across
the maximum number of validators. With `u16` indices, the identity table
supports up to 65,535 unique leaders. At 65,535 leaders × 32 bytes = 2 MB for
the identity table alone. The theoretical maximum account size is ~2.2 MB, well
within the 10 MB account data limit.

**Note on index width:** This proposal uses `u16` indices (2 bytes) rather than
`u32` (4 bytes), saving 216 KB per account at current mainnet parameters. The
`u16` limit of 65,535 unique validators provides substantial headroom — mainnet
currently has ~2,000 validators in the leader schedule. If the network were to
exceed 65,535 validators with non-zero stake, a future SIMD could introduce a
new version with wider indices. The `_reserved` header bytes could be used for
versioning.

#### Vote Address Inclusion

This proposal stores only validator identity pubkeys in the Identity Table, not
vote account addresses. Including vote addresses would double the identity table
size (~128 KB for 2,000 validators) and provide marginal benefit since vote
accounts are already queryable on-chain by their address. Programs needing to
map identity → vote account can perform that lookup separately.

**Open question:** Should vote addresses be included in the Identity Table
alongside identity pubkeys? This would increase the per-identity entry to 64
bytes but enable direct cross-referencing without additional account lookups.
Community feedback is welcome on whether the added utility justifies the size
increase.

### Account Addresses

The two accounts live at well-known addresses derived as Program Derived
Addresses (PDAs) from the owning program:

```
current_schedule = PDA(leader_schedule_program_id, ["current"])
next_schedule    = PDA(leader_schedule_program_id, ["next"])
```

Using PDAs rather than vanity-ground addresses ensures the addresses are
deterministic and verifiable. The seeds are fixed strings — the account
**contents** rotate at epoch boundaries, not the addresses. This means indexers
subscribe to exactly two stable addresses.

### Owner Program

These accounts are owned by a new native program, the **Leader Schedule
program** (`LeaderSchedu1e1111111111111111111111111111111`). This program:

- Rejects all instructions (the accounts are read-only from the perspective of
  transactions)
- Serves only as the owner for the two leader schedule accounts
- Is updated exclusively by the runtime at epoch boundaries

### Runtime Behavior

#### Epoch Boundary Update

At each epoch boundary (when `parent.epoch() < new.epoch()`), the runtime:

1. Copies the contents of `next_schedule` into `current_schedule`
2. Computes the leader schedule for the upcoming epoch (epoch + 1) using the
   same `leader_schedule_utils::leader_schedule()` logic that already populates
   the `LeaderScheduleCache`
3. Serializes the new schedule into the binary format described above
4. Writes the result to `next_schedule`
5. Adjusts lamport balances for rent exemption on both accounts

This integrates alongside existing epoch-boundary processing in
`process_new_epoch()`, after vote account stake snapshots are taken.

#### Feature Activation

On the first epoch boundary after feature activation:

1. Both accounts are created with the system-allocated rent-exempt balance
2. `current_schedule` is populated with the current epoch's leader schedule
3. `next_schedule` is populated with the next epoch's leader schedule

#### Consistency

The leader schedule written to these accounts is identical to what
`LeaderScheduleCache` computes and what `getLeaderSchedule` returns over RPC.
The deterministic computation (ChaCha20 RNG seeded with epoch, stake-weighted
sampling) is unchanged.

### On-Chain Program Access

Programs access the leader schedule by including the account as a read-only
input to their instruction:

```rust
// Example: check who the leader is at a given slot
fn get_leader_at_slot(
    schedule_account: &AccountInfo,
    target_slot: u64,
    epoch_schedule: &EpochSchedule,
) -> Result<Pubkey, ProgramError> {
    let data = schedule_account.data.borrow();

    // Parse header
    let epoch = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let num_leaders = u16::from_le_bytes(data[8..10].try_into().unwrap()) as usize;
    let num_slot_blocks = u32::from_le_bytes(data[10..14].try_into().unwrap()) as usize;

    // Compute slot block index within this epoch
    let (slot_epoch, slot_index) = epoch_schedule.get_epoch_and_slot_index(target_slot);
    if slot_epoch != epoch {
        return Err(ProgramError::InvalidArgument);
    }
    let block_index = slot_index as usize / 4;

    // Look up leader index in schedule
    let schedule_offset = 16 + (num_leaders * 32);
    let idx_offset = schedule_offset + (block_index * 2);
    let leader_idx = u16::from_le_bytes(
        data[idx_offset..idx_offset + 2].try_into().unwrap()
    ) as usize;

    // Look up identity in table
    let identity_offset = 16 + (leader_idx * 32);
    let pubkey = Pubkey::try_from(&data[identity_offset..identity_offset + 32])
        .map_err(|_| ProgramError::InvalidAccountData)?;

    Ok(pubkey)
}
```

The raw binary format enables zero-copy access — programs can index directly
into the account data without deserialization overhead.

### RPC

No changes to existing RPC methods. The `getLeaderSchedule` and
`getSlotLeaders` methods continue to work as before. Clients that prefer
account-subscription-based access can use the new accounts.

New RPC methods may be proposed in a follow-up SIMD if warranted, but are not
part of this proposal.

## Alternatives Considered

### Sysvar Accounts

The most natural approach would be to make these sysvar accounts, following
the pattern of `SlotHashes`, `StakeHistory`, etc. However, the sysvar
infrastructure carries significant overhead:

- **Hardcoded cache:** The `SysvarCache` struct has a fixed field per sysvar.
  Adding a new sysvar requires modifications to ~15 files across the runtime,
  program-runtime, syscalls, SVM, and test infrastructure.
- **Per-bank caching:** Every bank creation populates the sysvar cache. For
  accounts that change only at epoch boundaries, this is unnecessary overhead.
- **Serialization constraints:** Sysvars traditionally use bincode
  serialization. The leader schedule benefits from a raw binary layout for
  zero-copy on-chain access.

A system-managed account owned by a dedicated native program achieves the same
goals (runtime-controlled, read-only, well-known addresses) without coupling to
the sysvar cache infrastructure. Programs read the account data directly, just
as they would any other account.

### Syscall-Only Approach

A syscall like `sol_get_slot_leader(slot) -> Pubkey` would be more efficient
for on-chain programs that only need to check individual slots. However:

- It does not serve the off-chain subscription use case at all
- It still requires the schedule to be computed and stored somewhere accessible
  to the syscall handler
- A syscall could be proposed as a complementary follow-up SIMD

### Three Epochs (Last, Current, Next)

An earlier draft of this proposal included the previous epoch's schedule for
retrospective analytics. Two epochs (current + next) were chosen instead because:

- Off-chain consumers can retain historical schedule data by subscribing to
  account updates and storing snapshots
- The slot history sysvar already provides skip information; combining it with
  a saved schedule snapshot is straightforward
- Fewer accounts means less state and simpler rotation logic

### Single Combined Account with Both Epochs

Storing both epochs in one account would halve the number of accounts but
roughly double the account size. Separate accounts allow programs to load only
the epoch they need, reducing per-transaction account data.

### u32 Indices

Using `u32` indices instead of `u16` would support over 4 billion unique leaders
but doubles the schedule portion of the account (~432 KB vs ~216 KB at current
epoch length). Given that `u16` supports 65,535 unique leaders — over 30× the
current validator count — the space savings are worthwhile.

## Security Considerations

### Account Size

Each account is ~280 KB at current mainnet parameters. This is comparable in
size to large existing accounts (programs, etc.) and well within the 10 MB
limit. The combined footprint of ~560 KB for both accounts is modest relative
to overall validator memory usage.

### Transaction Loading Cost

Programs that load a leader schedule account will consume compute units
proportional to the account data size (~280 KB). This is significant but
comparable to loading other large accounts. Programs that only need to check a
single slot's leader can optimize by reading only the relevant bytes using
zero-copy access patterns.

### Read-Only Guarantees

The accounts are owned by a native program that rejects all instructions. They
cannot be modified by any transaction — only by the runtime at epoch boundaries.
This provides the same integrity guarantee as sysvar accounts.

### Determinism

The leader schedule computation is deterministic (same epoch + same stakes =
same schedule). All validators will produce identical account contents for the
same epoch, ensuring consensus on account state.

## Backwards Compatibility

This proposal introduces new accounts and a new native program. It does not
modify any existing accounts, programs, sysvars, or RPC methods. There are no
backwards compatibility concerns.

Validators that have not activated the feature will not create or update these
accounts. Once the feature is activated network-wide, all validators will
maintain consistent account state.
