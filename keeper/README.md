# Keeper

Keeper coordinates account storage, execution history, caches, and subscriptions.
The processor commits through Keeper rather than managing those services separately;
MBV usually accesses them through Engine. Keeper owns [Accountsdb](../accountsdb/README.md)
and the [ledger](../ledger/README.md), together with the runtime configuration needed
to interpret their state.

## Startup and recovery

Startup validates account storage against ledger progress, restores a retained
snapshot when necessary, and seeds programs, features, and sysvars.
[Engine](../engine/README.md#startup-and-recovery) replays the remaining history.
Recovery can fail rather than open inconsistent state and depends on valid retained
snapshots and history.

Read-side caches aren't durable state. Leaders resume with the latest blockhash
and an empty processed-signature cache; followers reconstruct bounded duplicate
protection from retained history. Recovered signatures needn't have retained
execution results. Snapshot bootstrap can also leave account-state transaction
counts ahead of locally retained history.

## Authority and sponsorship

The local identity signs locally produced records; effective authority identifies
whose records the engine authenticates. A follower can have a distinct local key
while accepting its source's authority. Identities must remain consistent across restart.

Effective authority also identifies the engine-local sponsor account. MagicRoot
balances imported account lamports against it to preserve transaction-wide balance.
Startup preserves an existing sponsor balance and seeds an absent sponsor with the
initial balance. Reset restores that balance on an existing sponsor; reseeding a
missing account doesn't recover its previous balance.

## Caches and subscriptions

Retained reads and live subscriptions have different availability and delivery
contracts. Recent execution results and duplicate protection expire by slot;
account-cache eviction must preserve unresolved engine-authoritative state.

| Stream | Delivery |
| :-- | :-- |
| Signature results | One terminal execution result, including an already retained result. |
| Accounts, programs, logs, blocks, completed snapshots | Multicast; slow receivers are disconnected. |
| Processed transactions, service messages, cache evictions | One receiver per process lifetime; unbounded queues never block producers. |

Admission rejection is returned through request completion, not signature observers.
Completion is independent of notification consumption. Account updates own their
data rather than retaining references into mutable storage.

## State boundaries

Full superblocks pair recoverable account snapshots with sealed history segments.
Keeper exports the snapshot, queues the seal, and archives the export asynchronously.
Seal completion acknowledges durable rotation, not completion of snapshot archival.

Checksum checkpoints compare persisted state without snapshots, rotation, or forced
disk synchronization. Followers preserve authenticated upstream signatures on both
boundary types; mismatches stop processing. Callers must quiesce writes for either
operation and uphold borrowed-reader safety requirements.

Ordinary synchronization makes preceding work durable while services remain
available. Terminal synchronization is reserved for coordinated shutdown and
irreversibly closes storage services.

## Epochs and Clock

Keeper supplies Solana sysvars without validator epoch policy. Its informational
epoch schedule follows local `blockstore.superblock` configuration, without warmup;
zero disables periodic sealing and uses 432,000-slot epochs. Schedules can differ
between peers or change on restart. Sealing doesn't itself advance the schedule.

Execution and simulation see Clock at the slot after the latest completed block,
with epochs derived from that slot and Unix timestamp from the block. The unsupported
`epoch_start_timestamp` remains zero.

[Construction and recovery](src/builder.rs) establish initial state;
[accessors](src/accessor.rs) implement commits and reads, and
[subscriptions](src/subscriptions.rs) define fanout. The `testkit` feature provides
temporary stores and real execution fixtures.
