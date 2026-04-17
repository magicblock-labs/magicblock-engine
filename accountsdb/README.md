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

## Persisted layout

`CURRENT/storage.db` contains a metadata header followed by account images in the
borrowed `solana-account` layout. Each image includes its full pubkey so scans can
recover keys without the index. Offsets are measured in 8-byte `StorageUnit`s.

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

Defragmentation requires exclusive access. It sorts free spans, compacts the
tail half, updates indexes before moving live bytes, and shrinks the mapped file.
Repeated passes reduce the remaining hole set geometrically.

## Snapshots and volatile state

`AccountsDB::snapshot` requires exclusive write access. It records the
superblock id, defragments and flushes persisted state, clones the active tree,
and serializes the current volatile map into the clone's `volatile.db`.

`dump(None)` writes `CURRENT/volatile.db` for a clean externally paced shutdown.
The next open restores that file into memory and removes it. `reset` instead
removes chain-mirrored volatile accounts while preserving internal system
accounts and rebuilding their owner indexes. Persisted engine-authoritative
state is never reset.
