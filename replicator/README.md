# `magicblock-replicator`

Keep another Engine deployment in step with a running source over TCP. A follower
resumes from its durable `BlockstorePosition` when the source still has that
history; otherwise, it receives the newest available accountsdb snapshot and
asks the host to restart.

## Connecting a follower

The [workspace overview](../README.md#-keep-another-deployment-in-step) introduces the workflow. Configure
the identities and pacing before starting the connection:

1. On the source, start `ReplicationDispatcher` with a reachable address and an
   allowlist of follower local identities. An empty allowlist denies all followers.
2. Open the follower with its own storage and local keypair. Set
   `nucleus::config::Authority::remote` to the source authority, and give
   `Engine::new` an external pacer so it applies upstream block boundaries.
3. Start `ReplicationClient` with the source address and the pacer sender. Keep
   both machines' clocks within 30 seconds for signed handshakes.
4. Handle `RestartRequired` through the shutdown manager: drain services and
   reopen the follower from the same directories to install a staged snapshot.

Each follower identity can have only one active transfer. The server releases
its reservation when the connection worker exits. Stream workers detect peer
disconnects on writes triggered by durable cursor updates, published at least
once per produced block while the engine is running.

## Identities and relays

A follower signs handshake requests with its local key, but verifies the source
against `Engine::authority()`. Responses are signed with the server's local key;
every dispatcher must therefore hold the canonical authority key.

A follower with a different local signer is a terminal leaf. On such an engine,
`ReplicationDispatcher::spawn` logs a warning and returns `Ok(())` without binding
a listener. Any number of leaves may follow the source or a shared-key relay.
Every relay holds the shared private key, so the source and all relays share one
compromise and key rotation boundary.

A shared-key follower may also serve downstream followers. It waits for each
upstream seal, validates its state, then persists the original seal and archives
its own snapshot before consuming further entries. Downstream clients verify
responses against the original source authority.

## Follower recovery

Before each handshake, the follower quiesces execution, flushes queued ledger
appends, and reports the resulting cursor. A received snapshot is written to the
successor superblock directory; staging waits for durable seal completion. The
seal's cumulative transaction count replaces the follower ledger baseline,
including when a nonempty follower falls behind retention. The client then
reports `RestartRequired`; keeper restores the staged snapshot on the next
startup and engine replay advances it to the ledger tip.

Normal follower shutdown flushes the cursor before the engine writes
`CURRENT/volatile.db`. Internally paced origins instead append one reset marker
at startup before producing their first new block, so followers clear
chain-mirrored volatile state at the same stream position while retaining
internal system accounts.

On normal follower shutdown, Control keeps consuming ordered transaction batches
until the sequencer acknowledges the next validated and applied block, then
barriers execution and flushes the cursor before stopping Ingest. The operational
block heartbeat supplies that boundary, reconnecting first when necessary.
Replication failure and snapshot restart paths do not claim this guarantee.

## Protocol

Protocol version 1 uses wincode control messages prefixed by a little-endian
`u32` length. Control frames are limited to 65,535 bytes before allocation.
Snapshot archives and blockstore bytes follow the selected response without
additional framing.

Handshake requests and responses are signed with the sender's local key and
must be within 30 seconds of the receiver's clock. Ingest also verifies each
block, superblock seal, and reset against the configured authority before handing
it to Control. Signatures cover the message kind and full payload, excluding
the signature itself. Invalid signatures terminate replication. Snapshot
bootstrap verifies the original seal signature before staging any data. Followers
preserve these signatures in their own ledger instead of signing again.

## Transfer ordering

The async dispatcher assigns each accepted socket to a blocking thread with
bounded blocking file and socket I/O. Published ledger cursors are transfer
boundaries, including sealed tails and intermediate superblocks.

Ingest batches at most 128 transactions (typically 128 KiB), fencing before each
block, seal, reset, or reconnect. Verification runs on idle Control or overlaps
Control's scheduling in Ingest. Control alone owns handshakes, reconnect cursors,
barriers, snapshot staging, scheduling, and pacing, preserving stream order.
