# `magicblock-keeper`

Keeper opens accountsdb and the ledger as one durable state boundary. It also
owns startup account seeding, read-side caches, and live subscription fanout.
Account routing remains in accountsdb and ledger retention remains in ledger.

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

## Superblock finalization

`Keeper::finalize_superblock` requires quiesced execution; accountsdb separately
drains scoped readers while relocating and truncating storage. It snapshots
accountsdb to refresh the checksum, then signs the reconstructed seal or compares
it with an authenticated upstream payload and retains its signature. The snapshot
is archived in the successor directory; completion acknowledges durable sealing
and rotation, not archive completion. `SuperblockAccessor::sealed` returns the
unsigned accountsdb state.

Producers sign resets, followers append the original signed reset, and local
recovery only applies its payload. All share the same volatile-state mutation.

## Synchronization

`Keeper::sync(false)` flushes queued appends and accountsdb while keeping ledger
workers available, as required by replay and replication. `Keeper::sync(true)`
is the irreversible shutdown fence: it closes every reader after earlier queued
requests, flushes and closes the appender, then flushes accountsdb.

## Caches and subscriptions

Signature and recent-block caches use slot-based TTLs. Each push sweeps at most
`EVICTION_LIMIT` expired entries under the insertion lock. Reads do not evict:
expired signatures remain readable and reject duplicates until a push sweeps them.
The account cache is an LRU that also coordinates concurrent loads of
missing accounts. Only non-authoritative modes enter the eviction LRU;
delegated, ephemeral, and unresolved transient state remains outside it.

Dedicated channels publish account and program updates, signature results, logs,
processed transactions, blocks, cache evictions, completed snapshots, and
service messages. Processed-transaction accounts are made owned before queueing,
so subscribers never retain mmap views across compaction. This copy is skipped
when there is no live processed-transaction subscriber.
Signatures have terminal oneshot fanout; persistent multicast
streams give each receiver a bounded queue and disconnect a receiver that falls
behind. Processed transactions, service messages, and cache evictions each have
one process-lifetime receiver and apply producer backpressure when full.
Append rejection notifies only its newest signature waiter, preserving older
waiters for an already accepted transaction; invalid-blockhash status is cached.

## `testkit`

The `testkit` feature exposes a keeper backed by throwaway directories plus v42
account and transaction helpers, including persisted-metadata fault injection.
When enabled, Keeper's build script builds the v42 SBF artifact consumed by the
harness. Downstream tests enable the feature on their dev-dependency instead of
duplicating the setup.
