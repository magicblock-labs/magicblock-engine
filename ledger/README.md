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
and rotates to the next superblock. The successor metadata retains the sealed
snapshot's checksum and cumulative transaction count so it remains
self-describing after retention removes the preceding blockstore.

Reader requests run on a worker pool. Each worker owns its decode buffers and
reads only through published cursors. The active Fjall index uses two background
workers and an 8 MiB cache. Sealed indexes open with one worker on demand; the
two immediately preceding the active head are exempt from idle eviction, while
older indexes close after ten idle minutes. The optional `testkit` feature uses
one reader worker without changing the on-disk format.

Each block atomically commits its transaction, block, and account index changes.
Ordinary boundaries flush data files and the Fjall journal to the operating
system with `Buffer` durability before publishing cursors. Explicit syncs,
seals, resets, shutdown, and retention boundaries additionally use file
`sync_data` and Fjall `SyncData` before synchronously flushing metadata. Thus a
process crash recovers complete published blocks, while an OS or power failure
may discard the active tail after the last strong boundary.

The account index stores `pubkey_prefix || execution_span_be` as its key and an
empty value. Fixed-width big-endian slot and account-span key components make
reverse Fjall ranges start at the newest entry. Opaque span values remain
little-endian. LZ4 is disabled because the realistic index fixture reduced
closed-directory size by only 7.37%.

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
oldest sealed superblock from ledger metadata and synchronously purges its
directory; the active head is never removed.

The size check assumes the ledger directory is on a dedicated filesystem.
Unrelated files on that filesystem contribute to the used-byte total and can
trigger earlier retention.
