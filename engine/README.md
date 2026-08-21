# `magicblock-engine`

This crate exposes `Engine`, the consumer-facing handle over keeper state,
transaction sequencing, simulation, block pacing, recovery, and MagicRoot
account operations. It registers MagicRoot and the System Program as native
builtins before keeper opens startup state.

`Engine::signer` is always the local keypair. `Engine::authority` returns the
configured remote authority for a replica, or the local identity when no
override is configured. Replication uses that distinction to sign locally while
authenticating its immediate upstream.

Normal transaction submission sanitizes and verifies each transaction.
Replication instead uses `Engine::verifier` to sanitize, authority-check, and
batch-verify payloads, then consumes the resulting opaque transactions through
the trusted `TransactionAccessor::verified` path without repeating crypto.
Retained local-ledger replay has a separate private verification bypass.

## Account replacement

`AccountAccessor::{create, update}` composes complete-account MagicRoot patch
transactions. Replacement slots are monotonic: a newer slot is accepted, an
equal slot requires a genuine account-mode transition, and an older slot is
rejected even when the mode changes. Failed replacements are transactionally
rolled back. Complete-account patch sequences cover non-flag fields, while
finalization atomically installs the caller-supplied complete flag value without
changing lamports. Callers are responsible for supplying current state; later
replacements remain subject to the account's slot and lifecycle rules. `create`
appends any `PostFinalize` actions immediately after finalization in the same
transaction. Magicblock construction rejects instruction, address, account-meta,
and instruction-data lengths that cannot be represented by the V1 wire fields.

## Startup and recovery

Keeper restores an accountsdb snapshot when the active store is corrupt, its
sealed superblock trails the retained ledger, or its committed transaction count
trails the ledger's durable count. Accountsdb's count is a checkpoint high-water
mark, so a count ahead of the locally retained ledger is current, including for
snapshots staged by a replication follower. Superblock lag remains recoverable
independently of the counters.

If accountsdb then trails the ledger tip, `Engine::new` replays retained entries
from the successor of its sealed snapshot through a temporary sequencer. Replay
quiesces at superblock seals and compares the reconstructed checksum with the
recorded seal. A mismatch returns `ReplayError::StateMismatch`. Current state
opens without replay when its slot and transaction count are each at least the
ledger values. After replay actually runs, the final transaction counts must be
equal or startup returns `ReplayError::StateMismatch`.

Internal pacing appends one reset marker at the current slot and clears
chain-mirrored volatile accounts before the pacemaker task starts. Internal
system accounts remain available. Replicas use external pacing and retain
restored volatile state. External block producers supply the slot and timestamp;
the sequencer overwrites hash-chain metadata with its locally computed hash and
parent.

## Shutdown

Shutdown behavior follows the pacing source. Internal pacing publishes a final
block and flushes durable state. External pacing flushes the durable cursor
before writing `CURRENT/volatile.db`, allowing the next open and replication
handshake to resume from matching state. The pacemaker holds the sequencer
barrier while issuing a terminal ledger sync, which closes the appender and
reader workers without waiting for every engine handle to be dropped.

The embedding service retains the `ShutdownManager` passed to `Engine::new` and
calls `terminate` after stopping external ingress. The manager stops the
replication client, pacemaker, sequencer, and backing services in order.
