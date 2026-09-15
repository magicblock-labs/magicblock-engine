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

`materialize` and `delete` consume the borrowing accessor and return `Result<()>`.
Ownership is released after definitive completion and success bookkeeping;
retrying requires reacquiring the account and rechecking its state.
Dropping an idle accessor or cancelling before submission releases ownership
without submitting work. Immediately after successful submission, a mutation
transfers its lease to a task that awaits the existing signature subscription and
completes recency bookkeeping even if its caller stops waiting. There is no
execution deadline; callers may time out their own wait, then reacquire and reread
state. A completion task panic or cancellation returns `EngineError::Task` with
the original Tokio `JoinError`; this is an infrastructure failure, not proof of
transaction rollback.

```rust,ignore
let accessor = engine.account(key).await;
accessor.materialize(account, None).await?;
// Any retry must acquire a new accessor and reread current state first.
```

Execution relies on the host's fail-stop contract: accepted work publishes a
terminal signature result, or the host shuts down the process after an execution
infrastructure failure. It does not recover a missing result in a live process.
Keep the Tokio runtime alive until Engine services stop; aborting its tasks is
not an account-operation cancellation mechanism. Ordinary transaction `execute`
also waits without an internal deadline, but cancelling its wait never cancels
submitted execution.

### Confirmed redelegation

Use that same operation for direct `Transient(S) -> Delegated(T)`, with `T > S`.
Equal-slot and stale input are rejected. Once delegated, the account cannot be
rematerialized as delegated at any slot, preventing duplicate activation actions
through this operation. Do not synthesize an intermediate `ReadOnly` update.

Chainlink, not Engine, establishes that the previous delegation ended. Before
replacement it must obtain a coherent confirmed account/delegation-record pair,
verify delegation to this engine's authority, and establish that the record's
`delegation_slot > S`. A newer observation slot alone is insufficient. Actions
and their `PostFinalize::source_program` must come from that verified record.
After acquiring the accessor, reread local state and reconcile the request before
materializing; a waiting caller cannot assume its original classification still
applies. The lease serializes accessor operations, not ordinary transactions;
execution validates the lifecycle against the account state it actually loads.

The confirmed image at `S` precedes the subsequent on-chain undelegation and
redelegation, so that new delegation necessarily occurs after `S`. Under this
caller contract, equal-slot reactivation is invalid input, not a missing feature.
Engine neither verifies chain confirmation nor classifies delegation generations.

A definitive execution failure rolls back replacement and action account changes,
preserving the transient image. A retry must reacquire the account and reclassify
its current state. A caller's timeout does not cancel execution or prove rollback;
the caller must reconcile the outcome before deciding on recovery. Chainlink must
retain undelegation tracking until materialization succeeds or recovery is
reconciled; its consuming-API migration must make rescue reacquire and reclassify
rather than reuse the old accessor. Its recovery implementation is tracked in
[MBV #1671](https://github.com/magicblock-labs/magicblock-validator/issues/1671).

Magicblock construction rejects instruction, address, account-meta, and
instruction-data lengths that cannot be represented by the V1 wire fields.

## Startup and recovery

Keeper restores an accountsdb snapshot when the active store is corrupt, its
sealed superblock trails the retained ledger, or its committed transaction count
trails the ledger's durable count. Accountsdb's count is a checkpoint high-water
mark, so a count ahead of the locally retained ledger is current, including for
snapshots staged by a replication follower. Superblock lag remains recoverable
independently of the counters.

If accountsdb then trails the ledger tip, `Engine::new` replays retained entries
from the successor of its sealed snapshot through a temporary sequencer. The
private replay dispatcher handles every retained entry without re-appending. Replay
quiesces at superblock seals and compares the reconstructed checksum with the
recorded seal. A mismatch returns `ReplayError::StateMismatch`. Current state
opens without replay when its slot and transaction count are each at least the
ledger values. After replay actually runs, the final transaction counts must be
equal or startup returns `ReplayError::StateMismatch`. Replay caches each
re-executed terminal transaction result without appending it to the ledger.

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
The sequencer signs produced blocks and validates replayed hash-chain metadata without replacing signatures. Completion acknowledges
validation and application. Followers apply upstream seals on arrival under an
execution barrier; local recovery does not append records.

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
