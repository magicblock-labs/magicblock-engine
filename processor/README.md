# `magicblock-processor`

Ordered parallel transaction execution for the engine. The processor schedules
work across SVM executors and commits results through Keeper, preserving input
order for conflicting accounts while allowing independent work to run in parallel.

Request completion distinguishes admission rejection from committed execution.
Fire-and-forget submission acknowledges queueing. Request completion is
independent of live observers consuming their notifications.

Block boundaries drain preceding execution before publication. Replication and
recovery validate the same ordered transaction history rather than replacing
upstream signatures. Local replay restores account state and cached results
without appending duplicate history or publishing live transaction notifications.

## Quiescence

An execution barrier drains in-flight work and holds later execution until the
caller releases it. This gives snapshots, state checks, and shutdown a coherent
boundary. A block-boundary checkpoint must pause execution before later
transactions can change the sampled state.

The execution path relies on this exclusion when using borrowed account storage.
Simulation runs independently and must retain its own protection against storage
relocation.

## Simulation

Simulation executes against account copies and returns an execution record
without committing account changes or appending history. It shares the engine's
runtime environment.
