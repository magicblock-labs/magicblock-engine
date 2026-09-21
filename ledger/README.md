# `magicblock-ledger`

Retained execution history for the engine: transactions, execution results,
blocks, and authenticated state boundaries. The ledger supports transaction and
account-history queries, ordered replay, and replication from a stream position.

History is grouped into superblocks so old data can be retired without removing
the active stream. Full superblocks are snapshot and recovery boundaries;
checksum-only checkpoints allow earlier divergence checks without creating
snapshots or rotating history.

## Append and read paths

Records preserve execution order and the producer's signatures. Storage retains
signatures; the replication consumer is responsible for authenticating them.
Replay includes state boundaries, while ordinary block queries return the
transactions belonging to each block.

Publication, query visibility, and durability are different boundaries. Indexing
is asynchronous, so a recent execution may appear in live state before historical
queries can find it. Ordinary blocks and checksum checkpoints use buffered
publication rather than a forced disk sync.

Explicit synchronization drains earlier writes and indexing work. Coordinated
shutdown is the supported complete-history boundary: a process crash can leave
published data without corresponding indexes, and there is no automatic
crash-tail index rebuild. A terminal sync closes the services and cannot be used
as an ordinary flush.

Account-history indexes use compact key prefixes. Colliding prefixes can share
results; callers must not treat a prefix match as proof of full-key identity.

## Retention

Replay starts after the last sealed superblock already represented in the
consumer's state and proceeds through retained history. Consumers that fall
behind retention need a snapshot or another recovery source.

Retention removes sealed history, never the active superblock. Its size limit
measures used space on the ledger filesystem, so a dedicated filesystem is
expected: unrelated files can trigger earlier retention. In-flight readers may
delay physical space reclamation.

Storage and replication participants must agree on record encoding and meaning.
Changing persisted formats is a compatibility decision, not an implementation
cleanup.
