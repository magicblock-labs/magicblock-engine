# `magicblock-replicator`

Keep another Engine deployment in step with a source over TCP. Replication
streams ordered execution history, authenticates the source, and checks that the
follower reconstructs matching state. It provides neither consensus nor automatic
failover.

## Connecting a follower

Run a dispatcher on the source with an explicit follower allowlist. Open each
follower on its own storage, configure the source as its effective authority, and
use external block pacing so upstream boundaries drive execution. An empty
allowlist admits no followers.

Keep peer clocks synchronized for authenticated handshakes. Each follower
identity may have only one active stream. The host must handle snapshot-driven
restart requests by draining services and reopening the same storage.

See the [workspace example](../README.md#-keep-another-deployment-in-step) for the
connection workflow.

## Trust and state verification

Followers may have their own local signing keys, but verify records against the
source authority and preserve the source's signatures in retained history.
Invalid signatures or detected state divergence stop replication; they are not
transport errors to retry or a request to repair state automatically.

Block validation checks ordered transaction history. Full superblocks and
checksum-only checkpoints additionally compare persisted state. These checks do
not establish equality of volatile state or prove collision-free state identity.
Checkpoints add no snapshots or forced disk synchronization.

A relay serving downstream followers must hold the canonical authority key.
A follower with a different local key is a terminal leaf and does not start a
dispatcher. Shared-key relays share the same compromise and key-rotation boundary
as the source.

## Follower recovery

A follower resumes from its synchronized ledger position while the source still
retains that history. Otherwise it stages an available snapshot and asks the host
to restart; reopening installs the snapshot before consuming its remaining tail.
Snapshot progress may exceed the follower's locally retained transaction history.

Reconnect preserves stream order and does not repeat already applied work.
Upstream resets discard mirrored volatile state while preserving internal system
accounts and authoritative state.

Normal shutdown drains through the next validated block and synchronizes that
position. A checkpoint after that block remains pending until reconnect. This
requires the source's block heartbeat, reconnecting first if necessary; failure
and snapshot-restart paths do not promise the same graceful boundary.

Replication peers must agree on record formats and execution semantics. Valid
signatures alone do not make incompatible runtimes interchangeable.
