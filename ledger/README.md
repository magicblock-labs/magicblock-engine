# Ledger

The ledger retains transaction inputs, execution results, blocks, and authenticated
state boundaries. It supports historical queries, recovery replay, and replication
from a stream position. [Accountsdb](../accountsdb/README.md) holds current state;
the ledger supplies the history needed to reconstruct or inspect that state.

## Superblocks and replay

History is partitioned into superblocks to align snapshot recovery with retention.
A full seal records a snapshot boundary and opens the next history segment. Sealed
segments can be retired without removing the active stream. Replay starts after
the last sealed superblock already represented in the consumer's state; consumers
behind retention need a snapshot or another recovery source.

Checksum-only checkpoints compare persisted state between full snapshots without
creating snapshots or rotating history. They are divergence checks, not additional
recovery points.

## Append and read paths

The appender preserves execution-history order and producer signatures. A separate
indexer makes records searchable by transaction, account, and block. Replay includes
state boundaries; block queries return the transactions belonging to each block.
Storage retains signatures without authenticating them; that belongs to the
replication consumer.

Publication, indexed visibility, and durability are separate boundaries. Asynchronous
indexing means live state can reflect an execution before historical queries find
it. Ordinary blocks and checksum checkpoints use buffered publication rather than
forced disk synchronization.

Explicit synchronization drains earlier writes and indexing work. Coordinated
shutdown is the supported complete-history boundary: a crash can leave published
data without indexes, and startup doesn't automatically rebuild crash-tail indexes.
Account-state recovery therefore doesn't imply complete queryable history.
Terminal synchronization closes services and can't serve as an ordinary flush.

Account-history indexes use compact public-key prefixes. Collisions can share
results; a prefix match isn't proof of full-key identity.

## Retention

Retention removes sealed history and preserves the active superblock. In-flight
readers can delay physical reclamation. The size limit measures used space on the
ledger filesystem, so a dedicated filesystem is expected: unrelated files can
trigger earlier retention.

Record encoding and semantics must agree across storage and replication participants,
including persisted format changes. The [schema](src/schema.rs) defines records;
[appending](src/appender.rs), [indexing](src/indexer.rs), and [reading](src/reader.rs)
are separate services. [Ledger lifecycle](src/lib.rs) owns opening and retiring segments.
