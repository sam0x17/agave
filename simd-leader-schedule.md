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
accounts and provide a `sol_get_slot_leader` syscall, enabling downstream
consumers to subscribe to schedule changes and on-chain programs to verify
leader identity efficiently.

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
account state and via a syscall.

## New Terminology

**Leader schedule account:** A system-managed account (not a sysvar — see
[Alternatives Considered](#alternatives-considered)) that stores both the unique
leader identities and the slot-to-leader mapping for a single epoch.

## Detailed Design

This proposal has two complementary parts:

1. **On-chain accounts** for the full schedule (serves indexers and off-chain
   consumers via account subscriptions)
2. **A syscall** for efficient single-slot leader lookups (serves on-chain
   programs without requiring them to load a ~280 KB account)

### Part 1: Leader Schedule Accounts

#### Account Structure

Two accounts are maintained: one for the **current epoch** and one for the
**next epoch**. Each account contains a self-describing binary layout with both
the identity table and the schedule index array.

All multi-byte integers are little-endian.

```
┌─────────────────────────────────────────────────────┐
│ Header (16 bytes)                                   │
│   epoch: u64            — epoch this schedule is for│
│   num_leaders: u16      — unique leaders in table   │
│   num_slot_blocks: u32  — leader blocks in schedule │
│   slots_per_block: u16  — slots per leader block    │
├─────────────────────────────────────────────────────┤
│ Identity Table (num_leaders × 32 bytes)             │
│   identities: [Pubkey; num_leaders]                 │
│   — validator identity keys, sorted by byte order   │
├─────────────────────────────────────────────────────┤
│ Schedule (num_slot_blocks × 2 bytes)                │
│   leader_indices: [u16; num_slot_blocks]            │
│   — index into Identity Table per leader block      │
└─────────────────────────────────────────────────────┘
```

The `slots_per_block` field records how many consecutive slots are assigned to
each leader (currently 4, i.e. `NUM_CONSECUTIVE_LEADER_SLOTS`). Consumers
**must** read this field from the header rather than hardcoding the divisor.
This ensures the format remains valid if the number of consecutive leader slots
changes in a future consensus update (e.g. under Alpenglow).

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
new version with wider indices.

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

#### Account Addresses

The two accounts live at well-known addresses derived as Program Derived
Addresses (PDAs) from the owning program:

```
current_schedule = PDA(leader_schedule_program_id, ["current_schedule"])
next_schedule    = PDA(leader_schedule_program_id, ["next_schedule"])
```

Using PDAs rather than vanity-ground addresses ensures the addresses are
deterministic and verifiable. The seeds are fixed strings — the account
**contents** rotate at epoch boundaries, not the addresses. This means indexers
subscribe to exactly two stable addresses.

#### Owner Program

These accounts are owned by a new native program, the **Leader Schedule
program**. This program:

- Rejects all instructions (the accounts are read-only from the perspective of
  transactions)
- Serves only as the owner for the two leader schedule accounts
- Is updated exclusively by the runtime at epoch boundaries

#### Runtime Behavior

##### Epoch Boundary Update

At each epoch boundary (when `parent.epoch() < new.epoch()`), the runtime:

1. Copies the contents of `next_schedule` into `current_schedule`
2. Computes the leader schedule for `current_epoch + 1` using the same
   stake-weighted shuffle (`LeaderSchedule::new()`) that already populates the
   `LeaderScheduleCache`
3. Serializes the new schedule into the binary format described above
4. Writes the result to `next_schedule`

Account lamport balances are set to the rent-exempt minimum (or 1 lamport,
whichever is greater) on each write.

This integrates into the existing epoch-boundary processing in
`process_new_epoch()`, after vote account stake snapshots are taken and
`update_epoch_stakes()` has been called.

##### Feature Activation

On the first epoch boundary after feature activation:

1. Both accounts are created with the rent-exempt balance (minimum 1 lamport,
   since zero-lamport accounts are treated as non-existent by the runtime)
2. `current_schedule` is populated with the current epoch's leader schedule
3. `next_schedule` is populated with the next epoch's leader schedule, if vote
   account stakes for that epoch are available. If not yet available, the
   `next_schedule` account is left empty and will be populated at the next
   epoch boundary

Consumers **must** check the `epoch` field in the header before using the
account data.

##### Consistency

The leader schedule written to these accounts is identical to what
`LeaderScheduleCache` computes and what `getLeaderSchedule` returns over RPC.
The deterministic computation (ChaCha20 RNG seeded with epoch, stake-weighted
sampling) is unchanged.

#### On-Chain Program Access via Accounts

Programs that need the full schedule (e.g. for analytics across many slots) can
include the account as a read-only input to their instruction. The raw binary
format enables zero-copy access — programs can index directly into the account
data without deserialization overhead.

For programs that only need to check one or a few slots, the
`sol_get_slot_leader` syscall (Part 2) is more efficient.

### Part 2: `sol_get_slot_leader` Syscall

#### Interface

```
u64 sol_get_slot_leader(
    u64 slot,           // absolute slot number
    u8 *leader_out,     // pointer to 32-byte output buffer
)
```

**Parameters:**
- `slot`: The absolute slot number to query.
- `leader_out`: Pointer to a 32-byte buffer where the leader identity pubkey
  will be written.

**Return value:**
- `0` on success (leader pubkey written to `leader_out`)
- `1` if the slot is outside the range covered by available schedule data
  (current and next epoch)

#### Compute Units

```
syscall_base + floor(32 / cpi_bytes_per_unit) + mem_op_base
```

With current defaults: `100 + floor(32/250) + 10 = 110` CU. This is
comparable to other lightweight syscalls like `sol_get_epoch_stake`.

#### Behavior

The syscall reads from the validator's in-memory leader schedule (the same
`LeaderScheduleCache` used by the RPC layer). It does **not** read from the
on-chain accounts — the accounts serve the off-chain subscription use case,
while the syscall serves the on-chain program use case.

The syscall covers slots in the current and next epochs. Queries for slots
outside this range return 1 (not found) without aborting the program.

### RPC

No changes to existing RPC methods. The `getLeaderSchedule` and
`getSlotLeaders` methods continue to work as before. Clients that prefer
account-subscription-based access can use the new accounts.

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

### Accounts-Only (No Syscall)

An earlier draft of this proposal relied solely on accounts for on-chain
access. Loading a ~280 KB account into a transaction is expensive — it
consumes a significant portion of the per-transaction account data budget and
costs substantial compute units. Most on-chain use cases (checking a single
slot's leader, gating execution) only need a single pubkey. The syscall
provides this at ~110 CU instead of loading the full account.

### Syscall-Only (No Accounts)

A syscall alone would serve on-chain programs but would not serve the primary
off-chain use case: indexers subscribing to schedule changes via Geyser plugins
and websocket `accountSubscribe`. The accounts and syscall are complementary.

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

### Capitalization Impact

Creating these accounts at feature activation increases total capitalization by
the rent-exempt minimum for ~560 KB of account data. At current rent parameters
this is approximately 4 SOL. This is a one-time, small increase that occurs
at the epoch boundary when the feature activates. No ongoing lamport changes
occur beyond minor adjustments if account sizes change between epochs.

### Transaction Loading Cost

Programs that load a leader schedule account will consume compute units
proportional to the account data size (~280 KB). This is significant but
comparable to loading other large accounts. Most programs should prefer the
`sol_get_slot_leader` syscall (~110 CU) over loading the full account.

### Read-Only Guarantees

The accounts are protected by two independent mechanisms:

1. **Program-level:** The owning native program rejects all instructions, so no
   transaction can modify the accounts through program invocation.
2. **Transaction-level:** The program ID and both PDA addresses are added to the
   reserved account keys list (gated on the same feature). This prevents any
   transaction from acquiring a write lock on these accounts, even if a
   malicious program were to claim ownership.

Combined, these provide the same integrity guarantee as sysvar accounts.

### Determinism

The leader schedule computation is deterministic (same epoch + same stakes =
same schedule). All validators will produce identical account contents for the
same epoch, ensuring consensus on account state. The syscall reads from the
same deterministic computation.

## Backwards Compatibility

This proposal introduces new accounts, a new native program, and a new syscall.
It does not modify any existing accounts, programs, sysvars, or RPC methods.
There are no backwards compatibility concerns.

Validators that have not activated the feature will not create or update these
accounts, and the syscall will not be available. Once the feature is activated
network-wide, all validators will maintain consistent account state and expose
the syscall.
