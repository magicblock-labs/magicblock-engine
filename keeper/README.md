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
non-zero superblock interval used by pacing and cache TTL calculation. The
shared accountsdb, blockstore, and ledger parameters are defined by nucleus;
keeper consumes them when opening its durable stores and caches.

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

`Keeper::finalize_superblock` snapshots accountsdb at the current ledger head,
computes the persisted-account checksum, queues the corresponding
`SuperblockSeal`, and archives the snapshot in the successor superblock
directory. Its completion signal resolves after the appender durably seals the
files and index and publishes the rotation. Events queued after the seal remain
ordered into the successor while that work completes.

Finalization requires exclusive account-store access. Engine obtains that
exclusivity through the sequencer and simulator barriers before calling it.

## Synchronization

`Keeper::sync(false)` flushes queued appends and accountsdb while keeping ledger
workers available, as required by replay and replication. `Keeper::sync(true)`
is the irreversible shutdown fence: it closes every reader after earlier queued
requests, flushes and closes the appender, then flushes accountsdb.

## Caches and subscriptions

Signature and recent-block caches use slot-based TTLs with lazy eviction on
insertion. The account cache is an LRU that also coordinates concurrent loads of
missing accounts. Only non-authoritative modes enter the eviction LRU;
delegated, ephemeral, and unresolved transient state remains outside it.

Dedicated channels publish account and program updates, signature results, logs,
processed transactions, blocks, cache evictions, completed snapshots, and
service messages. Signatures have terminal oneshot fanout; persistent multicast
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
