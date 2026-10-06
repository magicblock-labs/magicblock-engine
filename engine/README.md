# Engine

Engine is MBV's execution subsystem. It combines Solana transaction execution,
account storage, retained history, subscriptions, and recovery behind one handle.
MBV owns external-chain integration and commit policy; Engine owns local execution
and the resulting state. Its blocks delimit execution history, not consensus or
confirmation.

## Execution flow

Transaction ingress parses and sanitizes inputs before submission; ordinary
execution verifies signatures. The [processor](../processor/README.md) schedules
account dependencies and runs SVM workers. [Keeper](../keeper/README.md) coordinates
account writeback, history, caches, and notifications.

Engine starts these services and the block pacemaker. Local production uses an
internal timer; followers supply upstream boundaries through an external pacer.
The processor drains preceding work at each boundary before advancing its Clock
and blockhash. Pacing, snapshots, and replay therefore share the execution order
rather than sampling state independently.

## Submitting transactions

`execute` waits for admission rejection or a committed execution result.
`schedule` acknowledges queueing only. `simulate` runs against account copies
without committing. Execution completion doesn't imply disk synchronization.

Ordinary execution and scheduling verify signatures before queueing.
`simulate(true)` verifies signatures too; `simulate(false)` skips verification,
allowing unsigned inputs or a replaced blockhash. Sanitization and
private-transaction authority checks still apply in both modes.

Signature subscriptions report execution results, not admission rejections.
Scheduled work rejected during admission can be dropped without a signature
notification. Results and duplicate protection are bounded by retention.

Submitted work is independent of its waiter: cancellation doesn't cancel
execution, and Engine imposes no execution deadline. A lost reply or infrastructure
failure isn't evidence of rollback. The host must stop on execution infrastructure
failure rather than continue with an unknown outcome, keeping the runtime alive
through coordinated shutdown. Replication verification is specific to the receiving
Engine; verified inputs aren't transferable proof of authorization for another instance.

## Account replacement

External account images enter execution through privileged
[MagicRoot](../programs/magic-root-program/README.md) transactions. Replacement and
follow-up actions are atomic, so account activation uses the same ordering and
rollback machinery as ordinary work rather than a separate storage-write path.

Account accessors serialize materialization for a key, not ordinary transactions.
Their presence, mode, and slot observations are captured after lease acquisition
and can be superseded by execution. `missing_accounts` retains leases only for
accounts still absent after acquisition; `account` also supports existing state.

Materialization skips an image older than the observation or matching its mode
and slot, without running follow-up actions. Success can therefore mean applied
or skipped as already handled or superseded. A different mode at the same slot
still reaches [lifecycle validation](../solana/account/README.md#account-lifecycle).

MBV validates source freshness, replacement eligibility, and action provenance.
Engine enforces lifecycle transitions. Transient redelegation requires a new
delegation at a strictly newer slot, not a newer observation of the old delegation.
Delegated state can't be replaced in the same mode. Magic state remains authoritative
until explicitly closed, regardless of token balance. Materialization and deletion
require the local signer to match the effective authority.

After submission, the operation retains its lease through completion even if the
waiter is cancelled. An uncertain outcome requires reacquiring the accessor and
reconciling state before retrying. A definitive execution failure rolls back the
replacement and follow-up account changes.

## Startup and recovery

[Keeper](../keeper/README.md#startup-and-recovery) opens and validates the stores,
restoring a retained snapshot when needed. Engine replays the remaining history
before live execution, without duplicating ledger entries or live notifications.
State mismatches refuse startup; recovery depends on valid retained snapshots and history.

Full superblocks pair account snapshots with history boundaries. Optional checksum
checkpoints check persisted state between snapshots without snapshotting or forcing
disk synchronization. Their interval is independent of superblocks; full superblocks
take precedence when both are due. Followers consume upstream boundaries.

## Shutdown

The host stops ingress and terminates the retained shutdown manager while Engine
and the async runtime remain alive. Services drain execution and synchronize storage.
Locally paced engines publish a final block; followers also save volatile state for
restart. Follower draining follows the [replication boundary](../replicator/README.md#follower-recovery).

The orchestration path is in [startup and replay](src/lib.rs), with ingress in
[transaction verification](src/transaction.rs), imports in [accessors](src/accessor.rs),
and boundaries in the [pacemaker](src/pacemaker.rs).
