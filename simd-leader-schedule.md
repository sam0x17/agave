---
simd: 'XXXX'
title: Leader Schedule Sysvar
authors:
  - Jon Cinque (Anza)
category: Standard
type: Core
status: Draft
created: 2024-03-XX
feature: (fill in with feature tracking issues once accepted)
---

## Summary

This proposal outlines new sysvars to provide the leader schedule for a network.

Note: this could also be achieved through a syscall, but the proposal aims to
show that it is reasonable and generally more useful to store the information in
accounts.

## Motivation

The Solana blockchain uses a single block producer model, where the leaders are
chosen through a pseudo-random stake-weighted shuffle each epoch.

The leader schedule, however, is only available over RPC, and not available to
on-chain programs. This limits the amount of trustless auditing that can be
performed on validators.

For example, it's impossible to know the skip rate of a validator on-chain. The
slot history sysvar tells which slots were skipped, but we cannot tie that
information to a validator identity. As a result, this important validator
metric must be derived from off-chain data.

Also, it isn't possible for an on-chain program to avoid being executed by
a particular validator.

The leader schedule is also a rough stand-in for stake weight. If a validator
has 5% of leader slots, then that validator has around 5% of stake.

## Design

### Accounts

The leader schedule sysvar comprises two separate accounts. One account,
`LeaderIdentities`, stores the unique leader identity pubkeys, sorted in byte-order.
Another account, `LeaderSchedule`, gives the leader schedule, stored as an
array of indices into the `LeaderIdentities` account, where each index is stored as
a little-endian `u32`.

At 432k slots per epoch, where leaders get 4 slots at a time, we can store one
entry for each 4-slot block, so the `LeaderSchedule` account always takes up
432kb (432k slots / 4 slots per leader * 4 bytes per leader).

With a network of 2k voting nodes in the leader schedule, the `LeaderIdentities`
account takes up 64kb (32 bytes per leader * 2k voting nodes). The maximum
possible size for the account is 3,456kb, in the case where stake is equally
distributed between all nodes, and there are at least 108k voting nodes in the
network.

For on-chain usage, the network maintains three sets of sysvar accounts:
one for the last epoch, current epoch, and next epoch.

### Structs

To show these accounts more clearly in code, here is their structure. All `u32`s
are in little-endian format:

```rust
type Pubkey = [u8; 32];

struct LeaderIdentities {
    num_leaders: u32,
    keys: [Pubkey; num_leaders],
}

struct LeaderSchedule {
    leader_indices: [u32; 108_000],
}
```

### Runtime Changes

At every epoch boundary, the runtime shifts the account contents, so that the
current epoch becomes the last epoch, the future epoch becomes the current epoch,
and the upcoming leader schedule is written into the future epoch accounts.

The `LeaderIdentities` account is stored at address `Leader1dentities111111111111111111111111111`
and the `LeaderSchedule` account at `LeaderSchedu1e11111111111111111111111111111`.

## Alternatives Considered

The main alternative is to use a new syscall to retrieve the leader schedule in
batches. Since the size of both accounts is safely within the limits of 10MB,
and syscalls are not performant for the VM, we opted for providing all possible
information through a sysvar account.

We could also only store the current epoch's leader schedule. This limits
usefuless for on-chain analytics, however, because it would be difficult to
judge performance at the end of an epoch. Programs would need to run in the
last few slots to be useful.

Additionally, the slot history sysvar goes back 1 million slots, which is a bit
more than 2 epochs, so it makes sense to align the leader schedule to be roughly
as long.

The future epoch schedule account is provided because it's already been computed
by the cluster anyway, and will simply make that information public.

## Security Considerations

The main security consideration is the size of the accounts, but their size is
on the order of a large program.

## Backwards Compatibility

No concerns because the accounts are totally new.
