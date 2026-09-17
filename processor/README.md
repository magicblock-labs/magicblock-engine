# `magicblock-processor`

The processor schedules transactions across a fixed pool of SVM executors and
commits their results through keeper.

The sequencer preserves canonical order for account conflicts while executing
disjoint transactions and read/read access in parallel. Dependency tracking does
not depend on executor completion order. Lookahead is bounded at 16 pending
transactions per executor; full drains reset ordering state and block tickets.

Block hashes chain the prior hash with ordered transaction signatures. Boundaries
drain executor work before publication, ensuring execution metadata precedes its
block. Produced blocks are signed; replication and recovery recompute and
validate the hash chain without replacing signatures. Acknowledgment follows
validation and application.

Local replay commits account state and caches terminal results, but neither
appends ledger records nor publishes live transaction notifications.

## Quiescence

The sequencer barrier drains all executor work, acknowledges the caller, and
holds new execution until its guard is released. Engine uses the barrier for
coherent superblock snapshots, replay seal checks, replication handshakes, and
shutdown. A superblock checkpoint finalizes its block and enters that pause as
one sequencer message, so later transactions cannot enter the sealed snapshot.

Transaction execution uses an unguarded AccountsDB loader: the sequencer already
excludes compaction through execution, commit, and owned subscription fanout.
This avoids reader registration, slot updates, and admission fences on the
transaction-loading path. Simulation retains guarded reads because it runs
independently of the sequencer barrier.

## Simulation

Simulation has a separate worker and SVM context. It resolves a transaction,
loads owned account copies, executes against the current block environment, and
returns an `ExecutionRecord` without appending to the ledger or storing account
changes.
