# `magicblock-keeper`

Keeper brings account state, retained history, and live subscriptions together
for an embedded engine. Use `KeeperBuilder` to configure storage, authority, and
initial state; most applications access the resulting services through `Engine`.

[Accountsdb](../accountsdb/README.md) provides account storage, while the
[ledger](../ledger/README.md) retains execution history.

## Authority

Local identity signs locally produced records. A follower also has an effective
upstream authority whose records it authenticates. Retain the configured
identities across restart.

The effective authority identifies the engine-local sponsor account. Its balance
is preserved when the account already exists. Startup seeds an absent sponsor
with the initial sponsor balance; reset restores that balance on an existing
sponsor. Reseeding does not recover a missing account's previous balance.

## Caches and subscriptions

Use reads for retained state and subscriptions for live observations. Delivery
contracts differ, so consumers must handle backpressure appropriately:

| Subscription | Delivery |
| :-- | :-- |
| Signature results | One terminal result, including an already retained execution result. |
| Accounts, programs, logs, blocks, completed snapshots | Multicast; slow receivers are disconnected. |
| Processed transactions, service messages, cache evictions | One receiver per process lifetime; unbounded queues never block producers. |

Signature observers report execution results. Admission rejections are returned
through request completion, which is independent of notification consumption.
Account updates delivered to subscribers own their account data.

Recent results and duplicate protection are bounded by slot-based retention.
Account-cache eviction must preserve unresolved engine-authoritative state.

## Startup and recovery

Startup opens and validates both stores and supplies configured initial accounts,
programs, features, and sysvars. When current account state cannot be used, Keeper
restores a retained snapshot; Engine then replays the remaining ledger history.
Recovery depends on valid retained state and may fail rather than open an
inconsistent engine.

Leaders resume with the latest blockhash and an empty processed-signature cache;
followers recover
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
