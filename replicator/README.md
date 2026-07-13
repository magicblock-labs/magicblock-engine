# `magicblock-replicator`

Replicator transfers durable execution state over TCP between independent
engine deployments, typically running on different machines. The follower
reports its durable `BlockstorePosition`; the server either resumes the retained
blockstore stream or sends the newest available accountsdb snapshot.

## Protocol

Protocol version 1 uses wincode control messages prefixed by a little-endian
`u32` length. Control frames are limited to 65,535 bytes before allocation.
Snapshot archives and blockstore bytes follow the selected response without
additional framing.

Handshake requests and responses are signed with the sender's local key and
must be within 30 seconds of the receiver's clock. A server accepts only local
follower identities in its allowlist; an empty allowlist denies all followers.
A follower verifies responses against `Engine::authority()`, which must be
configured with the source authority through `nucleus::config::Authority::remote`.

Every dispatcher must sign with that same canonical authority key. A follower
whose local signer differs from `Engine::authority()` is therefore a terminal
leaf and dispatcher startup rejects it before binding a listener. Any number of
such leaves may follow the source or a relay. Every relay instead holds the
shared private key, so the source and all relays have one compromise and key
rotation boundary.

The async dispatcher accepts sockets and assigns each connection to a blocking
thread. File and socket operations on that thread use bounded blocking I/O.
Published ledger cursors are transfer boundaries, including sealed tails and
intermediate superblocks.

## Follower recovery

Before each handshake, the follower quiesces execution, flushes queued ledger
appends, and reports the resulting cursor. A received snapshot is written to the
successor superblock directory and its seal is appended synchronously. The
client then reports `RestartRequired`; keeper restores the staged snapshot on
the next startup and engine replay advances it to the ledger tip.

Externally paced shutdown flushes the cursor before writing
`CURRENT/volatile.db`. Internally paced origins instead append one reset marker
at startup before producing their first new block, so followers clear
chain-mirrored volatile state at the same stream position while retaining
internal system accounts.

A shared-key follower may also serve downstream followers. It derives and
validates superblock seals from replicated block boundaries and archives its own
snapshots, while downstream clients continue to verify every response against
the original source authority. A distinct-key follower can consume the same
state but cannot relay it.
