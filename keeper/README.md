# `magicblock-keeper`

Keeper brings account state, retained history, and live subscriptions together
for an embedded engine. Use `KeeperBuilder` to configure storage, authority, and
initial state; most applications access the resulting services through `Engine`.

[Accountsdb](../accountsdb/README.md) provides account storage, while the
[ledger](../ledger/README.md) retains execution history.

## Authority

Local identity signs locally produced records. A follower also has an effective
upstream authority whose records it authenticates; the two identities need not
be the same. Retain the configured identities across restart.

The effective authority identifies the engine-local sponsor account. Its balance
survives restart and is replenished on reset. Existing deployments must retain
that account rather than relying on startup to recreate missing funding state.

## Caches and subscriptions

Use reads for retained state and subscriptions for live observations. Delivery
contracts differ, so consumers must handle backpressure appropriately:

| Subscription | Delivery |
| :-- | :-- |
| Signature results | One terminal result, including an already retained execution result. |
| Accounts, programs, logs, blocks, completed snapshots | Multicast; slow receivers are disconnected. |
| Processed transactions, service messages, cache evictions | One receiver per process lifetime; slow consumption backpressures the producer. |

Signature observers report execution results, not admission rejections. Request
completion does not depend on observers consuming their notifications. Account
updates delivered to subscribers do not retain borrowed storage views.

Recent results and duplicate protection are bounded by slot-based retention,
not permanent transaction history. Account-cache eviction must not discard
unresolved engine-authoritative state.
Account mutation leases expose the mode and slot captured after acquisition for
materialization decisions; ordinary transaction writes are outside that lease.
Recency bookkeeping requires mutable access to the lease.

## Startup and recovery

Startup opens and validates both stores and supplies configured initial accounts,
programs, features, and sysvars. When current account state cannot be used, Keeper
restores a retained snapshot; Engine then replays the remaining ledger history.
Recovery depends on valid retained state and may fail rather than open an
inconsistent engine.

Restart does not restore every live cache. Leaders resume with the latest
blockhash and without the previous processed-signature cache; followers recover
bounded duplicate protection from retained history. Recovered signatures do not
necessarily have retained execution results. Snapshot bootstrap may also leave
account-state transaction counts ahead of locally retained history.

## Epochs and Clock

Use `Keeper::epoch_schedule()` to interpret slots as informational epochs. Each
epoch spans `blockstore.superblock` slots without warmup; zero disables periodic
sealing and uses 432,000-slot epochs. The schedule follows local configuration,
so it can differ between peers or change on restart. Sealing does not advance it.

Execution and simulation see Clock at the slot after the latest completed block,
with epochs derived from that slot and the Unix timestamp from the block.
`epoch_start_timestamp` is unsupported and remains zero.

## State boundaries

Full superblocks capture recoverable state and rotate history. Checksum-only
checkpoints verify persisted state between snapshots without rotation or forced
disk synchronization. Followers retain authenticated upstream signatures rather
than signing those records again, and a mismatch stops processing.

Callers must quiesce writes before snapshotting or sampling a checksum and uphold
the APIs' borrowed-reader safety requirements. Sealing completion acknowledges
durable rotation, not completion of snapshot archival.

## Synchronization

Use an ordinary sync to make preceding work durable while keeping services
available. Terminal synchronization is reserved for coordinated shutdown and
irreversibly closes the storage services.

The `testkit` feature supplies temporary stores and execution fixtures for
consumers that need a real Keeper in their tests.
