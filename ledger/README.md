# `magicblock-ledger`

The ledger stores transaction bytes, execution metadata, block boundaries,
superblock seals, and volatile-state reset markers. History files are partitioned
into superblock directories, while one global Fjall database partitions index
entries into a matching keyspace per superblock.

```text
ledger.meta
index/
superblock-000000001/
  superblock.meta
  blockstore.db
  executions.db
```

`blockstore.db` is a wincode stream. Blockstore decoding permits allocations up
to the ledger's 25-bit encoded entry-size bound (33,554,431 bytes); larger
entries are rejected. Wincode-encoded execution headers and bitcode details
compressed at Zstd level 3 with the embedded dictionary are both stored in
`executions.db`.
Frames omit Zstd dictionary IDs; the version stored first in each
`superblock.meta` selects the storage format, including its execution-details
codec. The current format is version 1. Changing the dictionary or codec
requires a ledger-version bump and explicit compatibility handling.

## Append and read paths

One appender owns ordered writes. Transaction bytes are appended first and kept
pending until their execution metadata arrives; only then are transaction and
account indexes inserted. Every durable sync flushes data and indexes, publishes
durable file cursors, transfers the accumulated transaction count, and flushes
ledger metadata. When the sync carries a block boundary, it also publishes that
block's slot and increments the block count. A seal finalizes the active files
and rotates to the next superblock. The successor metadata retains the sealed
snapshot's checksum and cumulative transaction count so it remains
self-describing after retention removes the preceding blockstore.

Reader requests run on a worker pool. Each worker owns its decode buffers and
reads only through published cursors. The ledger-wide Fjall index uses two
background workers and one 64 MiB cache across all superblock keyspaces. The
optional `testkit` feature uses one reader worker without changing the on-disk
format. Point lookups read the latest visible value, while range and prefix
iterators carry their own Fjall snapshot guard; the append-only index does not
need a request-wide snapshot.

Each block atomically commits its transaction, block, and account index changes.
Ordinary boundaries flush data files and the Fjall journal to the operating
system with `Buffer` durability before publishing cursors. Explicit syncs,
seals, resets, shutdown, and retention boundaries additionally use file
`sync_data` and Fjall `SyncData` before synchronously flushing metadata. Thus a
process crash recovers complete published blocks, while an OS or power failure
may discard the active tail after the last strong boundary. Sealing also queues
the immutable keyspace's active memtable for background SST flushing so its
journal history can be reclaimed.

Within each superblock keyspace, a leading byte namespaces transaction, block,
and account entries. The account index stores
`account_tag || pubkey_prefix || execution_span_be` as its key and an empty
value. Fixed-width big-endian slot and account-span key components make reverse
Fjall ranges start at the newest entry. Opaque span values remain little-endian.
LZ4 is disabled because the realistic index fixture reduced closed-directory
size by only 7.37%.

During coordinated shutdown, one queue marker per reader closes the pool after
earlier requests. A final appender sync flushes every preceding event, reports
its durability result, and then closes the appender. Intermediate replication
syncs flush without closing either service, and retained sender clones do not
delay terminal shutdown.

Replay is superblock-based. The consumer supplies the last sealed superblock
already reflected in its state, and the reader streams each retained successor
through the active head in full.

## Retention

At a block boundary, the appender checks used bytes on the ledger filesystem.
When the configured limit is reached, `Ledger::truncate` durably removes the
oldest sealed superblock from ledger metadata, then returns a worker that
destroys its index keyspace and directory. The appender joins that worker before
starting another truncation and during shutdown, propagating cleanup failures;
the active head is never removed. Fjall may defer physical keyspace reclamation
until in-flight readers release their handles.

The size check assumes the ledger directory is on a dedicated filesystem.
Unrelated files on that filesystem contribute to the used-byte total and can
trigger earlier retention.
