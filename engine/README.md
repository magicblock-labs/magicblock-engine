# `magicblock-engine`

Embed `Engine` to execute Solana transactions, simulate them without committing,
and work with local account state. Start with the [workspace guide](../README.md)
for startup and submission examples; this page covers the
integration contracts behind those examples.

## Opening an engine

Pass a `KeeperBuilder` and a retained `ShutdownManager` to `Engine::new`.
Use `None` for internal block pacing or an `ExternalPacer` for caller-supplied
boundaries. Startup opens and recovers state before starting live execution.
MagicRoot, the System Program, and the Compute Budget Program are registered as
native builtins before keeper opens startup state.

`Engine::signer` is always the local keypair. `Engine::authority` returns the
configured remote authority for a replica, or the local identity when no
override is configured. Replication uses that distinction to sign locally while
authenticating its immediate upstream.

## Submitting transactions

Choose whether you need a committed result, queueing acknowledgment, or a dry run:

| Method | Result |
| :-- | :-- |
| `execute` | Waits for admission rejection or committed execution through a request-specific reply channel. |
| `schedule` | Acknowledges queueing, not admission or execution; duplicates and invalid blockhashes are silently dropped. No completion channel is allocated. |
| `simulate` | Executes against account copies without committing changes. |

Signature subscriptions are observers, not request-completion channels. They and
status reads report execution results only, including retained results.
There is no execution deadline, and cancelling a wait does not cancel submitted
work. Accepted work must publish a terminal result or the host must fail-stop on
an execution infrastructure failure; Engine cannot recover a missing result in
a live process. Keep Tokio running until Engine services stop.

Normal submission sanitizes and verifies each transaction. Replication instead
uses `Engine::verifier` to sanitize, authority-check, and batch-verify payloads,
then passes the opaque results to `TransactionAccessor::verified` without
repeating crypto. Only use verified values from that Engine's verifier.
Retained local-ledger replay has a separate private verification bypass.

## Account replacement

Use `Engine::account(pubkey).await` when importing or replacing an account image.
It acquires an exclusive materialization lease so another accessor cannot replace
the same account while your operation is pending. The lease does not serialize
ordinary transactions. Materialization and deletion require the local signer to
match the engine's authority.

`AccountAccessor::materialize` composes complete-account MagicRoot patches,
finalization, and optional `PostFinalize` actions in one transaction. Patches
cover non-flag fields; finalization installs the complete caller-supplied flags
without changing lamports. Actions immediately follow finalization. Accepted
mode/slot combinations follow the [account lifecycle table](../solana/account/README.md).
A newer slot alone does not permit replacement of authoritative state.

`materialize` and `delete` consume the accessor and return `Result<()>`. After
submission, Engine retains the lease through request completion and
success bookkeeping, even if the caller stops waiting. Dropping an idle accessor
or cancelling before submission releases it without submitting work.

There is no internal execution deadline. A timeout does not cancel execution;
`EngineError::Task` preserves a completion task's Tokio `JoinError` and does not
prove rollback. Reacquire the accessor and reread before retrying or recovering.

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

Stop external ingress, then call `terminate` on the `ShutdownManager` retained
from startup. Keep the engine handle and Tokio runtime alive while it drains.
The manager stops the replication client, pacemaker, sequencer, and backing
services in order.

Shutdown behavior follows the pacing source. Internal pacing publishes a final
block. External pacing also writes `CURRENT/volatile.db` so the next open can
restore volatile state. Both paths hold the sequencer barrier while issuing a
terminal ledger sync, which closes the appender and reader workers and flushes
account storage without waiting for every engine handle to be dropped. During
normal follower shutdown, the replication client first flushes its applied cursor;
see the [follower recovery contract](../replicator/README.md#follower-recovery).
