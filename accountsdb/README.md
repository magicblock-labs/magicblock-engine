# `magicblock-accountsdb`

Account storage for the execution engine, with scoped zero-copy reads, snapshots,
and state checksums. Engine-authoritative accounts are persisted; externally
owned state is kept in memory and can be fetched again.

Storage follows the [account lifecycle](../solana/account/README.md). Delegated,
Magic, and transient accounts remain authoritative, even when they are no longer
user-mutable. Closing an account removes it from storage. Resetting externally
owned state preserves authoritative accounts and internal system accounts.

## Reading and writing

Scoped reads let callers inspect account data without retaining references into
storage. Read callbacks may retry, so they must be side-effect-free. Keep reader
scopes short: they prevent relocation, not concurrent account writes, and must
not span asynchronous work or invoke compaction.

Raw borrowed access is reserved for callers that can uphold its lifetime and
exclusion requirements. Unguarded readers must also exclude compaction. Follow
the safety contracts on the access APIs rather than treating a borrowed account
as an independent owned value.

Transaction commits track progress for recovery, including failed executions
that produce no account changes. Direct administrative writes do not advance
that transaction count.

## Snapshots and checksums

Snapshots capture state for recovery; compaction reclaims unused persisted
storage. Both require quiesced account writes, and relocation must wait for
borrowed readers. Platforms must support the reader synchronization required by
the store; initialization failures propagate to the caller.

Checksums cover persisted account state and its slot and superblock identity,
not volatile accounts or the transaction counter. A cached checksum describes a
previously published state. Fresh sampling requires quiesced writes and metadata
updates, but does not flush storage or refresh the cached value.

Volatile state can be saved for snapshot recovery and clean follower restart.
It is not made durable by ordinary account writes.
