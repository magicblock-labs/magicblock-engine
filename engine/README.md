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

`Engine::account(pubkey).await` acquires an exclusive materialization lease.
`AccountAccessor::materialize` composes complete-account MagicRoot patches,
finalization, and optional `PostFinalize` actions in one transaction. Patches
cover non-flag fields; finalization installs the complete caller-supplied flags
without changing lamports. Actions immediately follow finalization. Accepted
mode/slot combinations follow the [account lifecycle table](../solana/account/README.md).
A newer slot alone does not permit replacement of authoritative state.

`materialize` and `delete` consume the accessor and return `Result<()>`. After
submission, Engine retains the lease through terminal signature completion and
success bookkeeping, even if the caller stops waiting. Dropping an idle accessor
or cancelling before submission releases it without submitting work.

There is no internal execution deadline. A timeout does not cancel execution;
`EngineError::Task` preserves a completion task's Tokio `JoinError` and does not
prove rollback. Reacquire the accessor and reread before retrying or recovering.
Its lease serializes accessor operations, not ordinary transactions.

Accepted work must publish a terminal result or the host must fail-stop on an
execution infrastructure failure; Engine cannot recover a missing result in a
live process. Keep Tokio running until Engine services stop. Ordinary transaction
`execute` likewise has no deadline and cancelling its wait does not cancel work.

### Confirmed redelegation

Materialization permits `Transient(S) -> Delegated(T)` only with `T > S`. Do not
synthesize an intermediate `ReadOnly` update. Already-delegated accounts cannot
be rematerialized as delegated, preventing repeated activation through this API.

The caller (Chainlink) must establish a new delegation, not merely a newer
observation of the old one:

- Obtain a coherent confirmed account/delegation-record pair targeting this
  engine's authority, with `delegation_slot > S`.
- Derive actions and `PostFinalize::source_program` from that verified record.
- After acquiring the accessor, reread local state and reconcile the request.
  Execution checks lifecycle rules against the state it actually loads.

Engine neither verifies chain confirmation nor classifies delegation generations.
A definitive execution failure rolls back replacement and action account changes.
The caller must retain undelegation tracking until success or reconciled recovery;
a timeout alone is not grounds for rescue.

## Startup and recovery

[Keeper](../keeper/README.md#startup-and-recovery) validates and, when necessary,
restores accountsdb. If state still trails the ledger, `Engine::new` replays
retained entries after its sealed snapshot through a temporary sequencer, without
re-appending records or publishing live subscriptions. Replayed terminal results
are cached. Seal mismatches or unequal final transaction counts return
`ReplayError::StateMismatch`.

Current state opens without replay when its slot and transaction count are each
at least the ledger values. Accountsdb's count is a checkpoint high-water mark
and may exceed locally retained history after snapshot bootstrap.

On a current-state restart, a leader restores only the latest blockhash and
starts with an empty processed-signature cache. Transactions signed with older
hashes are therefore rejected before execution. A replica restores the bounded
blockhash and signature window from raw blockstore data; recovered signatures
deduplicate replicated input without retaining historical statuses.

Internal pacing appends one reset marker at the current slot and clears
chain-mirrored volatile accounts before the pacemaker task starts. Internal
system accounts remain available. Replicas use external pacing and retain
restored volatile state. The public pacing interface is `ExternalPacer` carrying
`ExternalBlock` values with `BlockInput::Production` or `BlockInput::Replay`.
Produced blocks are signed; replayed blocks retain their signatures and undergo
hash-chain validation. Completion acknowledges validation and application.
Followers apply upstream seals under an execution barrier.

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
