# `magicblock-accountsdb`

Accountsdb routes account state between two backends according to
`AccountMode::authoritative()`:

- `PersistedStore` is an mmap-backed account file with LMDB indexes. It holds
  delegated, ephemeral, and transient accounts controlled by the engine.
- `VolatileStore` is an in-memory map for externally owned state that can be
  fetched again.

Every store operation touches the backend required by both the account's current
representation and authoritative classification. This commits borrowed images
in persistent storage, inserts owned images there, updates owned volatile
images, and removes stale copies after mode changes or closure. `Transient`
remains authoritative and runtime-immutable until its lifecycle state resolves.

`AccountsDB::commit` is the ledger-transaction boundary. It stores successful
account transitions and then advances a persistent transaction counter; empty
transitions from failed executions advance the counter as well. Direct `store`
operations used for initialization, sysvars, and administrative writes do not.

## Persisted layout

`CURRENT/storage.db` contains a metadata header followed by account images in the
borrowed `solana-account` layout. Each image includes its full pubkey so scans can
recover keys without the index. Offsets are measured in 8-byte `StorageUnit`s.
The transaction counter is metadata and is not part of the account checksum.

The LMDB index under `CURRENT/index` contains:

- `accounts`: account key tag to storage offset and owner tag.
- `programs`: owner tag to account offsets.
- `freelist`: image size to reusable offsets.

`PersistedProgramIter` retains its read transaction for the persisted portion of
iteration. The optional `testkit` feature uses smaller maps and growth blocks
without changing the on-disk format.

## Writes and compaction

A persisted batch commits its LMDB transaction once. If applying or committing
the batch fails, already committed borrowed images are rolled back so indexed
state remains authoritative. Freed image spans enter the freelist.

Defragmentation requires exclusive access. Snapshot export packs tail accounts
into exact holes or the smallest fitting holes that retain at least 33 storage
units. It copies only between non-overlapping spans and publishes all
relocations in one index transaction. Vacated source spans are deferred to the
next pass, so some fragmented layouts may stall.

After validation, keeper startup repeats committed packing passes to a fixed
point before exposing the database to readers. Snapshot export runs one pass.
Both paths synchronously flush successful changes.

## Snapshots and volatile state

`AccountsDB::snapshot` requires exclusive write access. It records the
superblock id, runs one packing pass and flushes persisted state, clones the
active tree, and serializes the current volatile map into the clone's
`volatile.db`.

`dump(None)` writes `CURRENT/volatile.db` for a clean externally paced shutdown.
The next open restores that file into memory and removes it. `reset` instead
removes chain-mirrored volatile accounts while preserving internal system
accounts and rebuilding their owner indexes. Persisted engine-authoritative
state is never reset.
