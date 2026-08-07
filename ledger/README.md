# `magicblock-ledger`

The ledger stores transaction bytes, execution metadata, block boundaries,
superblock seals, and volatile-state reset markers. History is partitioned into
self-contained superblock directories so retention removes a complete sealed
segment without compacting the active store.

```text
ledger.meta
superblock-000000001/
  superblock.meta
  blockstore.db
  executions.db
  index/
```

`blockstore.db` is a wincode stream. Blockstore decoding permits allocations up
to the ledger's 25-bit encoded entry-size bound (33,554,431 bytes); larger
entries are rejected. Execution headers and zstd-compressed bitcode details are
stored separately in `executions.db`.

## Append and read paths

One appender owns ordered writes. Transaction bytes are appended first and kept
pending until their execution metadata arrives; only then are transaction and
account indexes inserted. Every durable sync flushes data and indexes, publishes
durable file cursors, transfers the accumulated transaction count, and flushes
ledger metadata. When the sync carries a block boundary, it also publishes that
block's slot and increments the block count. A seal finalizes the active files
and rotates to the next superblock.

Reader requests run on a worker pool. Each worker owns its decode buffers and
reads only through published cursors. The optional `testkit` feature reduces
LMDB map sizes and uses one reader worker without changing the on-disk format.

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
When the configured limit is reached, `Ledger::truncate` removes the oldest
sealed superblock; the active head is never removed.

The size check assumes the ledger directory is on a dedicated filesystem.
Unrelated files on that filesystem contribute to the used-byte total and can
trigger earlier retention.
