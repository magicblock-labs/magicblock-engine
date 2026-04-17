# `magicblock-accountsdb`

Accounts storage for the engine.

The crate keeps two backends:

- `PersistedStore` is the authoritative store for mutable accounts.
- `VolatileStore` is the authoritative store for immutable accounts.

`AccountsDB` routes each account by current mutability:

- mutable accounts are persisted
- immutable accounts are volatile
- borrowed immutable accounts also flow through persisted so stale persisted
  entries are removed
- owned mutable accounts also flow through volatile so stale volatile entries
  are removed

## Storage layout

Persisted data lives in one mapped file:

```text
storage.db
  meta header
  account images
```

The meta header tracks the current cursor and file size. Runtime counters are
tracked on the store handle. Account images are written in the same borrowed
layout used by `solana-account`, with the full pubkey stored in the image
prefix.

The persisted index lives under:

```text
<root>/index
```

It contains:

- `accounts`: compact account key tag -> storage offset + owner tag
- `programs`: compact owner tag -> storage offset
- `freelist`: image size -> reusable storage offset

## Invariants

- persisted account images must be 8-byte aligned
- persisted offsets are measured in `StorageUnit`
- the mmap header reservation must stay in front of the account images
- defrag sweeps the tail half of the hole list per pass, so repeated runs cut
  the remaining work geometrically
- `PersistedProgramIter` keeps its read transaction alive for the duration of iteration
- opposite-backend writes exist only to remove stale copies after a mode flip
- account images store the full pubkey in their prefix so iteration can recover it without LMDB

## Batch Semantics

- persisted batches commit once at the end
- if the LMDB commit fails, borrowed images are rolled back in memory
- rollback restores logical account state

## Snapshots

Snapshots copy the active database tree for a slot.

- the caller must hold exclusive write access while the snapshot runs
- persisted state is flushed before the tree is cloned
- the cloned volatile.db is replaced with the current in-memory volatile store
- snapshot consistency depends on the no-concurrent-writes rule

## Module split

- `lib.rs`: façade, loaders, error type
- `store/mod.rs`: persisted load and write path
- `store/index.rs`: LMDB index schema and lookups
- `store/mmap.rs`: file growth and mapped storage layout
- `store/defrag.rs`: storage compaction helper
- `volatile.rs`: in-memory state
