# Processor

Processor schedules Solana transactions across SVM workers and commits their
results through [Keeper](../keeper/README.md). Parallelism is constrained by
account dependencies: conflicts retain input order, while disjoint access and
read/read access can execute concurrently. The same ordering drives live execution,
replication, and recovery replay.

## Scheduling and execution

The sequencer builds a dependency graph as transactions are admitted. A read
follows the latest unfinished writer; a write follows both that writer and the
unfinished readers since it. Dependencies are established during registration,
not lock acquisition, so a blocked transaction already constrains later accesses.
Workers commit before releasing dependent work.

This preserves canonical conflict order without serializing independent transactions.
Request completion reports admission rejection or the committed execution result;
fire-and-forget submission acknowledges queueing only. Completion doesn't depend
on notification consumption.

## Execution boundaries

Blocks drain preceding execution before publication. Replication validates the
ordered history while retaining upstream signatures. Recovery replay applies
account changes and restores cached results without appending duplicate history
or publishing live transaction notifications.

Snapshots and checksums require a stable state beyond a queue drain. An execution
barrier drains in-flight work and prevents later execution until released.
Block-boundary checkpoints hold this barrier while sampling state; shutdown uses
it before terminal synchronization.

The execution path also relies on barrier exclusion to prevent relocation of
borrowed account storage. Simulation runs independently and retains its own reader
protection during account loading.

## Runtime policy and simulation

Processor translates transaction resource configuration into SVM budgets. V1 uses
inline limits: omitted compute and loaded-account data limits are zero, and omitted
heap size is 32 KiB. Compute and loaded-data requests are capped at runtime maxima;
heap requests must pass sanitization. Legacy, v0, and private Magicblock transactions
use compute-budget instructions. All versions execute without fees.

Simulation shares the runtime environment but loads owned account copies. It
returns an execution record without account writeback or history append.

[Dependency tracking](src/sequencer/order.rs) defines ordering; the
[executor](src/executor.rs) joins SVM execution to commit. [SVM setup](src/svm.rs)
owns runtime policy, and the [simulator](src/simulator.rs) owns non-committing execution.
