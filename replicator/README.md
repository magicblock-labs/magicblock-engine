# Replicator

Replicator streams a source engine's ordered execution history over TCP. Followers
re-execute transaction inputs locally and validate the resulting history and
persisted state. This replicates execution, not just result records, and assumes a
configured source authority rather than consensus between peers.

## Stream processing

The source dispatcher serves retained [ledger history](../ledger/README.md).
The follower authenticates inputs, schedules transactions in stream order, and
feeds upstream blocks into Engine's external pacemaker. Block validation checks
ordered transaction history; superblocks and checksum checkpoints also compare
persisted state. Source signatures are retained unchanged.

State checks exclude volatile accounts and don't prove collision-free state identity.
Checkpoints create neither snapshots nor forced disk synchronization. Invalid
signatures and detected divergence are terminal replication errors.

## Connecting a follower

A follower owns its storage and local identity, with the source configured as its
effective authority. External pacing makes execution follow upstream boundaries
instead of generating independent blocks. The source uses an explicit follower
allowlist: an empty list admits nobody, and each identity can hold only one active
stream. Authenticated handshakes require synchronized peer clocks.

A dispatcher must hold the canonical authority key, including when serving from a
relay. A follower with a different local key is a terminal leaf and doesn't start
a dispatcher. Shared-key relays share the source's compromise and key-rotation
boundary. Peers must agree on record formats and execution semantics.

## Follower recovery

Reconnect resumes after already applied work while preserving stream order. A
synchronized ledger position is sufficient while the source retains the required
history. Otherwise the follower stages an available snapshot and requests a host
restart. The host must drain services and reopen the same storage; reopening
installs the snapshot before consuming its remaining tail. Snapshot progress can
exceed locally retained transaction history.

Upstream resets discard mirrored volatile state while preserving internal system
accounts and authoritative state.

Normal shutdown drains through the next validated block and synchronizes that
position. A checkpoint after that block remains pending until reconnect. This
requires the source's block heartbeat, reconnecting first if necessary; failure
and snapshot-restart paths don't promise the same graceful boundary.

The [client](src/client.rs) owns ordered ingestion and recovery; the
[server](src/server.rs) selects streams or snapshots. [Protocol framing](src/protocol.rs)
also defines the authentication handshake.
