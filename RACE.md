# Shutdown and ledger publication races

Investigated against Engine `698d0370` (tag `0.14.2`) while running the MBV
ChainSync stack. The proposed repairs are on `fix/chainsync-ci-durability`.

## Execution can reach a closed ledger during shutdown

`TransactionAccessor::enqueue` checks `Engine::terminating`, then awaits a send
to the bounded sequencer channel. The check and the send are not atomic with
`Engine::shutdown`.

The original shutdown path sets `terminating`, enqueues an ordinary barrier,
waits for previously sequenced execution, and sends the ledger's final sync.
That sync makes the appender exit. Dropping the barrier then lets the sequencer
resume until its shutdown tier is cancelled.

A producer that passed the termination check can enqueue behind the barrier:

```text
producer                    shutdown                    sequencer / ledger
check terminating: false
                            set terminating: true
                            enqueue Barrier
                                                        drain preceding work
                                                        acknowledge and pause
enqueue Transaction
                            final ledger sync
                                                        appender exits
                            release barrier
                                                        consume Transaction
                                                        append fails: appender closed
```

This does not require a new submission after shutdown was requested: a sender
already waiting for channel capacity can complete later. An ordinary barrier
quiesces execution only until released; it does not close ingress.

### Observed evidence

The RedSuite `undelegation_recovery` job in MBV run `38414758804` reported a
non-clean restart. Its `er-committor-faults.log.prev` shows the appender exiting
at `14:42:13.278`, the sequencer resuming at `14:42:13.297`, and then the sequencer
terminating with `State(ServiceUnavailable(LedgerAppender))`.

The logs establish the shutdown ordering and resulting failure. They do not
identify the individual producer that enqueued the late message.

### Repair

Use a distinct shutdown fence: the sequencer closes its receiver, processes
already queued messages, drains their execution, and acknowledges quiescence.
Only then does Engine close durable storage. Closing the receiver rejects
waiting sends and prevents additional work from being admitted after the fence.
Normal snapshot barriers retain their existing pause-and-resume behavior.

Relevant sources: `engine/src/accessor.rs`, `engine/src/lib.rs`,
`nucleus/src/runtime.rs`, `processor/src/sequencer/mod.rs`, and
`ledger/src/appender.rs`.

## Replay can restore state without its transaction index

The original `LedgerAppender::write_block` publishes flushed data-file cursors,
ledger counters, and the blockstore position before enqueueing the block's
index commit. Transaction/account index entries are accumulated by a separate
worker and committed with that block.

```text
appender                         indexer                 process restart
flush transaction/execution data
publish block cursors and counts
enqueue block index commit
                                 work still queued
SIGKILL
                                                         replay published transactions
                                                         recover their account writes
                                                         transaction lookup misses
```

The on-disk replay cursor can therefore cover work whose Fjall index existed
only in the worker queue or pending batch. Startup replay rebuilds account state
from the published blockstore; it does not rebuild the missing transaction
index.

### Observed evidence

The same run's `checkpoint_durability` scenario recovered counter state
`count=144, updates=12`, but its retained transaction evidence implied
`count=64, updates=8`. Startup logged `accountsdb_txns=12`, `ledger_txns=28`,
followed by successful replay. This is consistent with the publication race;
a deterministic reproduction of that exact failed workload is still needed.

### Repair and remaining ordering

Flush data, await the block's buffered atomic index commit, then publish the
replay cursors and counters. This introduces one index acknowledgement per
block, without adding a per-block fsync. Synchronous fences also persist indexes
before publishing their cursors.

There is an inverse crash window between index commit and cursor publication.
Startup removes active-head index entries beyond the published file cursors,
so they cannot expose transactions that replay did not restore or point into
bytes subsequently overwritten by the appender. Clean heads take a last-block
index check; only an unpublished indexed tail requires a keyspace scan.

This is process-crash ordering, not a claim of atomic power-loss durability
across the data files, mmap metadata, and Fjall journals. Buffered boundaries
retain their existing durability policy. The added index wait's effect on block
publication latency has not been benchmarked.

Relevant sources: `ledger/src/appender.rs`, `ledger/src/indexer.rs`,
`ledger/src/index.rs`, `ledger/src/lib.rs`, `ledger/src/reader.rs`, and
`keeper/src/lib.rs`.

## Validation

Affected Engine, Processor, and Ledger checks and strict Clippy passed. Existing
`replay_rebuilds_state_after_counter_lag` and
`test_process_crash_recovers_buffered_block` tests passed; the latter also passed
after indexed-tail cleanup was added.
These tests exercise recovery/shutdown but do not deterministically force both
interleavings above. The next MBV CI run will exercise the failing RedSuite
workloads against the rewired Engine revision.
