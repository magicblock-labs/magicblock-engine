# `magicblock-keeper`

Keeper gives an embedding service a consistent place to read account state and
history, follow live updates, and recover the two stores together. Most callers
reach it through `Engine`; use `KeeperBuilder` to configure initial state and
storage. Account routing stays in [accountsdb](../accountsdb/README.md), and
history retention stays in [ledger](../ledger/README.md).

## Authority

`nucleus::config::Authority::local` is the keypair used for locally signed
messages. When `Authority::remote` is set, `Keeper::authority` returns that
immediate upstream identity instead of the local pubkey, while `Keeper::signer`
continues to return the local signer. Replication followers retain both values
across restart.

The effective authority also identifies the engine's sponsor account. Keeper
creates this engine-local account only for an empty ledger, persists its spent
balance across restarts, and restores its initial balance on reset. Startup
rejects a non-empty deployment whose configured authority account is absent.

## Caches and subscriptions

Subscribe when you need to react to changes; use reads for retained state and
results. Delivery depends on the stream, so choose how your consumer handles
backpressure before connecting it:

| Subscription | Delivery |
| :-- | :-- |
| Signature result | Terminal oneshot fanout, including an already retained execution result. |
| Account, program, logs, blocks, completed snapshots | Multicast, with a bounded queue per receiver; a receiver that falls behind is disconnected. |
| Processed transactions, service messages, cache evictions | One process-lifetime receiver per stream; a full queue backpressures the producer. |

Admission rejection goes only to the submitting request: it is neither cached
nor sent to signature observers. Rejected signatures keep their dedup reservation
until normal expiry. Processor completes requests independently of fanout.

`subscribe_signature` registers before checking retained status, and commits
cache results before fanout. This ordering lets concurrent and late subscribers
observe a retained result without missing it between registration and lookup.
Account and program accessors return receivers directly; signature registration
awaits the retained-status lookup. Registration is synchronous and may wait for
an SCC bucket lock, never channel capacity.

### Cache lifetime

Signature and recent-block caches expire by slot, not wall-clock time. Each push
sweeps at most `EVICTION_LIMIT` expired entries under the insertion lock. Reads
do not evict: an expired signature remains readable and rejects duplicates until
a push sweeps it.

The account cache coordinates concurrent loads of missing accounts and tracks
non-authoritative accounts in an eviction LRU. Delegated, Magic, and
unresolved transient state stays outside that LRU.

### Delivery internals

Processed-transaction accounts are copied into owned storage before queueing so
a subscriber cannot retain mmap views across compaction. The copy is skipped
when no processed-transaction subscriber is live.

Signature and multicast registries count occupied keys atomically. Empty sends
and membership checks skip hashing and map access; only first registration and
last removal change the count. Closed receivers stay counted until pruning.
This avoids a global subscription mutex or bucket-count scan. Full-map cleanup
yields on bucket contention.

## Startup and recovery

`KeeperBuilder::build` opens both stores and seeds active feature accounts,
native builtins, configured loader-v4 programs, caller-provided accounts,
authority funding, and sysvars.

Accountsdb is restored from the newest retained snapshot when validation finds
corruption, its sealed superblock trails the ledger head, or its committed
transaction count trails the ledger's durable count. The accountsdb count is a
checkpoint high-water mark and may exceed the locally retained ledger count,
including after replication snapshot bootstrap. The original active tree is
saved until the restored snapshot validates. Engine replay is responsible for
advancing restored state from the successor of its sealed superblock through the
ledger tip and must finish with matching transaction counts when replay runs.

`nucleus::config::BlockstoreParams` supplies the expected block time and
superblock interval (`u64`, zero skips periodic sealing of nonzero slots). The
shared accountsdb, blockstore, and ledger parameters are defined by nucleus;
keeper consumes them when opening its durable stores and caches.

Startup preserves `SlotHashes`; when missing outside genesis, it rebuilds up to
512 slots through block-only ledger reads. Missing blocks are skipped, an empty
result keeps the genesis image, and read errors propagate.

The newest `SlotHashes` entry bounds cache recovery, so an unreplayed ledger tail
cannot advance startup state:

- Leaders retain only the latest hash and start with no processed signatures,
  rejecting pre-restart transactions that reference older hashes.
- Followers stream the retained blockstore window to recover hashes and compact
  signature prefixes. Recovered signatures deduplicate input without historical
  terminal statuses; persisted `SlotHashes` also supports snapshot-only bootstrap.
- Engine replay caches re-executed terminal results without ledger appends or
  live transaction notifications.

## Superblock finalization

A superblock seals a checkpoint that recovery and replication can validate.
Quiesce execution before calling `Keeper::finalize_superblock`; accountsdb also
drains scoped readers before relocating and truncating storage. Finalization
snapshots accountsdb to refresh the checksum, then signs the reconstructed seal
or compares it with an authenticated upstream payload and retains its signature.

The snapshot is archived in the successor directory. Completion acknowledges
durable sealing and rotation, not archive completion.
`SuperblockAccessor::sealed` returns the unsigned accountsdb state.

Producers sign resets, followers append the original signed reset, and local
recovery only applies its payload. All share the same volatile-state mutation.

## Synchronization

Choose a nonterminal flush during replay or replication; reserve the terminal
fence for shutdown:

- `Keeper::sync(false)` flushes queued appends and accountsdb while leaving ledger
  workers available.
- `Keeper::sync(true)` irreversibly closes every reader after earlier queued
  requests, flushes and closes the appender, then flushes accountsdb.

## `testkit`

The `testkit` feature exposes a keeper backed by throwaway directories plus v42
account and transaction helpers, including persisted-metadata fault injection.
When enabled, Keeper's build script builds the v42 SBF artifact consumed by the
harness. Downstream tests enable the feature on their dev-dependency instead of
duplicating the setup.
