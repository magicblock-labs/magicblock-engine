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
They are persisted as 64-bit values, and production reserves a 1 TiB virtual
mapping that is backed by the file only as it grows. The transaction counter is
metadata and is not part of the account checksum.

The LMDB index under `CURRENT/index` contains:

- `accounts`: account key tag to storage offset and owner tag.
- `programs`: owner tag to account offsets.
- `freelist`: image size to reusable offsets.

Read transactions use LMDB's thread-local reader slots, with capacity for 256
reader threads. Dropping a transaction ends its active snapshot, while its slot
remains cached for reuse by the same thread. Keeping snapshots short therefore
limits old-page retention without giving up reader-slot reuse.

`PersistedProgramIter` retains its read transaction for the persisted portion of
iteration. The optional `testkit` feature uses smaller maps and growth blocks
without changing the on-disk format.

## Reader scopes

Use `AccountLoader::read` and `AccountsDB::program` for scoped zero-copy
reads; callbacks can return encoded results or owned snapshots. Program iteration
only exposes callback results. `AccountLoader::mode` reads volatile mode under
the map guard without cloning account data, and persisted mode through the same
cached index and sequence check as ordinary reads. `unsafe load` is reserved for
zero-copy transaction execution: borrowed results must remain protected through
commit, with concurrent account writes, deletion, and storage reuse excluded.
Retaining a loader or iterator excludes compaction, not ordinary account writes.
`AccountLoader::unguarded` is the exception: callers must exclude compaction
themselves for the loader and all borrowed results. It skips reader admission
without introducing a separate account lookup path.

Per-source load counters (`accountsdb_loads`) and backend account-count gauges
(`accountsdb_accounts`) are no longer exported. Persisted storage counters,
including `accountsdb_persisted_reads`, and operation timings remain available.

Guarded loaders and program iterators enter the scope before opening their LMDB
transactions. Registered readers update only their own cache-line-separated
slot; first registration and reads meeting compaction use the maintenance mutex.
Nested synchronous scopes share the outer admission. Do not hold a reader scope
across asynchronous work or invoke compaction from inside one.

Linux requires expedited private `membarrier` support, registered through rustix
when opening the database. Readers use compiler fences; compaction issues the
process-wide barrier. Registration and barrier errors propagate, with admission
restored on maintenance failure. macOS uses full memory fences on both sides of
the same protocol.

## Writes and compaction

A persisted batch commits its LMDB transaction once. If applying or committing
the batch fails, already committed borrowed images are rolled back so indexed
state remains authoritative. Freed image spans enter the freelist.

Defragmentation requires exclusive access. Snapshot export drains registered
readers before packing and blocks new readers until relocation and truncation
finish. Account writes must still be quiesced by the caller. Snapshot export
packs tail accounts into exact holes or the smallest fitting holes that leave
a minimum useful remainder. It copies only between non-overlapping spans and
publishes all relocations in one index transaction. Vacated source spans are
deferred to the next pass, so some fragmented layouts may stall.

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
