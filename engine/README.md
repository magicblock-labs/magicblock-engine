# `magicblock-engine`

Embed `Engine` to execute Solana transactions, simulate them without committing,
and manage local account state. It combines ordered execution, storage, live
subscriptions, and recovery without introducing validator consensus or fork choice.
Start with the [workspace guide](../README.md) for integration examples.

## Opening an engine

Configure initial state and storage with `KeeperBuilder`, retain a
`ShutdownManager`, and choose internal or externally supplied block pacing.
Startup validates and recovers state before accepting live execution.

Keep local signing identity distinct from effective authority: followers sign
local messages with their own key but authenticate replicated records against
the configured upstream authority.

## Submitting transactions

Choose the completion boundary your application needs:

| Method | Result |
| :-- | :-- |
| `execute` | Admission rejection or committed execution result. |
| `schedule` | Queueing acknowledgment, not admission or execution success. |
| `simulate` | Execution against account copies, without committing changes. |

Signature subscriptions observe execution results, not admission rejections.
Scheduling can therefore discard rejected work without notifying a signature
observer. Retained status and duplicate protection are bounded, not permanent.

Cancelling a wait does not cancel submitted work, and Engine imposes no internal
execution deadline. A lost completion or infrastructure failure is not evidence
of rollback; the host must stop on execution infrastructure failure rather than
continue with an unknown outcome. Keep the async runtime alive through shutdown.

Replication uses authority-verified transaction inputs. Values verified for one
Engine must not be reused as proof of authorization for another.

## Account replacement

Account accessors serialize competing materialization operations for the same
account, but do not serialize ordinary transactions. Materialization and deletion
require the local signer to match the engine authority.

Use `missing_accounts` to scan a batch for absent accounts and retain leases
only for those still absent after acquisition. Each accessor exposes `pubkey()`
to match it to a request and `exists()` to recheck presence under the lease.
It does not select transient accounts for refresh. For direct updates, `account`
returns an accessor whose `observed()` mode and slot can inform caller-owned
eligibility policy. The observation is captured after lease acquisition; ordinary
transactions can still change the account afterward. Engine handles mode-and-slot
deduplication against that same observation.

Replacement and its follow-up actions execute atomically. The host must validate
source freshness, creation or replacement eligibility, and action provenance;
Engine enforces the [account lifecycle](../solana/account/README.md), not base-chain
confirmation or application-specific token rules.
An older image or one matching the observed mode and slot is skipped without
running follow-up actions. Thus `materialize` success means applied or skipped as
already handled or superseded; a different-mode image at the same slot still
reaches lifecycle validation.

In particular, redelegating transient state requires a genuinely new delegation
at a strictly newer slot, not just a newer observation of the old delegation.
Already-delegated accounts cannot be replaced in the same mode. Magic accounts
remain authoritative until explicitly closed; an empty token balance is not
grounds for removing that protection.

Once submitted, replacement retains its materialization lease through completion,
even if the caller stops waiting. After a timeout or uncertain result, reacquire
the accessor and reconcile state before retrying. A definitive execution failure
rolls back replacement and follow-up account changes.

## Startup and recovery

[Keeper](../keeper/README.md#startup-and-recovery) restores usable account state;
Engine replays retained history when needed, without duplicating ledger records
or live notifications. State mismatches refuse startup rather than silently
accepting divergence. Recovery depends on retained snapshots and history, not
an arbitrary-crash recovery guarantee.

Full superblocks provide snapshot and recovery boundaries. Optional checksum
checkpoints detect persisted-state divergence between them, without snapshotting
or forcing disk synchronization. Their interval is independent of superblocks;
full superblocks take precedence when both are due. Followers consume upstream
boundaries rather than generating their own.

## Shutdown

Stop external ingress and terminate the retained shutdown manager while the
runtime is still running. Coordinated shutdown drains work and synchronizes
storage. Internally paced producers publish a final block; followers also preserve
volatile state for restart. See the [follower recovery contract](../replicator/README.md#follower-recovery)
for the boundary at which replication stops.
