# `magicblock-processor`

The processor schedules transactions across a fixed pool of SVM executors and
commits their results through keeper.

Produced blocks are signed after computing their hash chain. Replication and
local recovery both recompute the chain from ordered transaction signatures and
validate each block's parent and hash. Neither replaces the original signature;
local recovery also skips ledger appends.
The sequencer acknowledges boundaries only after validation and application.

Replay executors commit account state and cache the re-executed terminal status,
but do not append ledger records or publish live transaction subscriptions.

The sequencer preserves canonical stream order for account conflicts while
retaining parallel execution for disjoint transactions and read/read access.
Block-local dependency tracking is independent of executor completion order.

Lookahead is bounded at 16 pending transactions per executor, applying
backpressure to the input stream once the bound is reached. Every full drain
resets the ordering state and block-local tickets: block boundaries, quiescence
barriers, and orderly shutdown all start the next scheduling epoch cleanly.

Block hashes chain the prior block hash with each appended transaction's
canonical signature. Finalization drains executor work before publishing the
ledger boundary, so every transaction's execution metadata precedes the block
that contains it.

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
