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

One appender owns ordered data-file writes and remains authoritative for their
spans. Executor threads attach compact account descriptors to execution events,
and the appender forwards those descriptors and its file spans to one bounded,
ordered index worker. At each block, the appender publishes data-file cursors
before enqueueing the block marker; the worker then atomically commits that
block's transaction, account, and block entries. A seal drains the index worker,
finalizes the active files, and rotates both services to the next superblock. The
successor metadata retains the sealed snapshot's checksum and cumulative
transaction count so it remains self-describing after retention removes the
preceding blockstore.

Reader requests run on a worker pool. Each worker owns its decode buffers and
reads only through published cursors. The ledger-wide Fjall index uses two
background workers and one 64 MiB cache across all superblock keyspaces. The
optional `testkit` feature uses one reader worker without changing the on-disk
format. Point lookups read the latest visible value, while range and prefix
iterators carry their own Fjall snapshot guard; the append-only index does not
need a request-wide snapshot.

Block-range reads scan published blockstore bytes directly, without consulting
the index or executions file. They stream boundaries in storage order through a
bounded channel, with the preceding transactions' 16-byte signature prefixes
kept in blockstore order. Cancellation is checked between decoded entries. This
is a read-side projection only and does not change the on-disk blockstore or
index format.

Each block atomically commits its transaction, block, and account index changes.
Index visibility is asynchronous: recent transaction and block lookups may be
absent, account history omits trailing unindexed blocks, and the live signature
cache may lead durable history. Explicit syncs, seals, resets, retention, and
shutdown are ordered index drain fences and use file `sync_data` and Fjall
`SyncData`. Ordinary block
boundaries use `Buffer` durability. Thus a process crash can leave a published
data tail without indexes; graceful shutdown is the supported complete-history
boundary and no crash-tail index rebuild is performed. Sealing also queues the
immutable keyspace's active memtable for background SST flushing so its journal
history can be reclaimed.

Within each superblock keyspace, a leading byte namespaces transaction, block,
and account entries. Transaction keys retain 16 signature bytes, while account
keys retain eight public-key bytes. The account index stores
`account_tag || pubkey_prefix || execution_span_be` as its key and an empty
value. Fixed-width big-endian slot and account-span key components make reverse
Fjall ranges start at the newest entry. Opaque span values remain little-endian.
Accounts with colliding eight-byte prefixes share history results. This layout
has no compatibility path or ledger-version bump; deployment requires a fresh
ledger directory.
LZ4 is disabled because the realistic index fixture reduced closed-directory
size by only 7.37%.

During coordinated shutdown, one queue marker per reader closes the pool after
earlier requests. A final appender sync flushes every preceding event, fences the
indexer, and reports completion. The appender then closes its sole index sender,
allowing the drained worker to exit. Intermediate replication syncs fence without
closing either service, and retained appender sender clones do not delay terminal
shutdown.

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
