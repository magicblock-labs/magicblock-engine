# Accountsdb

Accountsdb stores current account state; the [ledger](../ledger/README.md) retains
execution history. The store separates engine-authoritative state from rebuildable
external-chain mirrors. Authoritative accounts are persisted, while mirrored state
lives in memory and can be fetched again.

Routing follows [lifecycle mode](../solana/account/README.md#account-lifecycle),
not memory representation. Delegated, Magic, and transient accounts remain
authoritative, including transient state that no longer permits user writes.
Closed accounts are removed. Reset discards mirrored state while preserving
authoritative and internal system accounts.

## Storage access and writeback

Persisted accounts support borrowed reads and copy-on-write execution over an
[active/shadow layout](../solana/account/README.md#borrowed-storage). This avoids
copying account data on every execution load, but ties access to storage lifetime
and relocation rules.

Scoped readers prevent relocation, not concurrent account writes. Read callbacks
may retry and must be side-effect-free. Scopes must remain short, never span async
work, and never invoke compaction. Raw borrowed access requires the lifetime and
exclusion guarantees specified by its unsafe API; unguarded readers must exclude
compaction themselves.

Commit advances the recovery transaction count after storing account transitions.
Failed executions with no account changes still count when they reach the commit
path; direct administrative writes don't. Recovery compares this progress with
the ledger rather than inferring it solely from account contents.

## Snapshots and checksums

Snapshots capture recoverable state; compaction reclaims unused persisted space.
Both require quiesced writes, and relocation waits for borrowed readers. Platform
support for reader synchronization is required, with initialization failures
propagated to the caller.

Snapshot exports are temporary. [Keeper](../keeper/README.md#state-boundaries)
archives them into retained ledger history and removes orphaned exports on startup.
Volatile state can be saved for snapshot recovery and clean follower restart;
ordinary account writes don't make it durable.

Checksums cover persisted accounts plus slot and superblock identity, excluding
volatile accounts, the transaction counter, and the chain slot. The cached checksum
describes previously published state. Fresh sampling requires writes and metadata
updates to be quiesced, but neither flushes storage nor refreshes that cache.

[Store routing and loading](src/lib.rs) connect the [persisted store](src/store/mod.rs)
and volatile backend. [Snapshots](src/snapshot.rs) handle recovery exports;
[reader admission](src/readers.rs) coordinates relocation with active readers.
