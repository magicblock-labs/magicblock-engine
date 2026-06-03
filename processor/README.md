# `magicblock-processor`

The processor schedules transactions across a fixed pool of SVM executors and
commits their results through keeper.

The sequencer tracks each static account key with executor-holder bits and an
exclusive-write bit. Transactions with non-conflicting read/write sets run in
parallel. Conflicting transactions queue behind the executor holding the
blocking account and are retried when that executor becomes available.

Block finalization drains executor work before publishing the ledger boundary,
so every transaction's execution metadata precedes the block that contains it.

## Quiescence

The sequencer barrier drains all executor work, acknowledges the caller, and
holds new execution until its guard is released. Engine uses the barrier for
coherent superblock snapshots, replay seal checks, replication handshakes, and
shutdown.

## Simulation

Simulation has a separate worker and SVM context. It resolves a transaction,
loads owned account copies, executes against the current block environment, and
returns an `ExecutionRecord` without appending to the ledger or storing account
changes.
