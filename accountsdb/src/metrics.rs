//! Prometheus metrics for accountsdb.

use std::sync::{OnceLock, atomic::Ordering::*};

use nucleus::metrics as metric;
use nucleus::metrics::{IntCounter, MetricOperation, MetricSpec, OperationCounters};

use crate::{AccountsDB, store::Stats};

/// Process-wide accountsdb metrics registered in the default Prometheus registry.
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Persisted account image load counter.
const READS: MetricSpec = MetricSpec {
    name: "accountsdb_persisted_reads",
    help: "Persisted account image loads.",
};
/// Borrowed account commit counter.
const COMMITS: MetricSpec = MetricSpec {
    name: "accountsdb_persisted_commits",
    help: "Borrowed account commits into persisted storage.",
};
/// Fresh mapped-storage allocation counter.
const ALLOCS: MetricSpec = MetricSpec {
    name: "accountsdb_persisted_allocs",
    help: "Fresh allocations from the mapped persisted storage file.",
};
/// Persisted freelist reuse counter.
const REALLOCS: MetricSpec = MetricSpec {
    name: "accountsdb_persisted_reallocs",
    help: "Allocations reused from the persisted freelist.",
};
/// Defragmentation relocation counter.
const COMPACTIONS: MetricSpec = MetricSpec {
    name: "accountsdb_persisted_compactions",
    help: "Persisted account relocations during defragmentation.",
};
/// Persisted account removal counter.
const REMOVALS: MetricSpec = MetricSpec {
    name: "accountsdb_persisted_removals",
    help: "Persisted account removals.",
};

/// Persisted storage resize counter.
const RESIZES: MetricSpec = MetricSpec {
    name: "accountsdb_persisted_resizes",
    help: "Persisted storage file resizes.",
};
/// Operation latency histogram recorded in microseconds.
const OPERATION_TIME: MetricSpec = MetricSpec {
    name: "accountsdb_operation_duration_micros",
    help: "Accountsdb operation duration distribution in microseconds.",
};

/// Accountsdb operation used as a low-cardinality operation label.
#[derive(Clone, Copy)]
pub(crate) enum Operation {
    /// Persisted store flush path.
    Flush,
    /// Persisted checksum path.
    Checksum,
    /// Accountsdb snapshot path.
    Snapshot,
    /// Volatile-state dump path.
    Dump,
    /// Persisted store defragmentation path.
    Defragmentation,
}

impl MetricOperation for Operation {
    /// Returns the Prometheus label value for this operation.
    fn label(self) -> &'static str {
        match self {
            Operation::Flush => "flush",
            Operation::Checksum => "checksum",
            Operation::Snapshot => "snapshot",
            Operation::Dump => "dump",
            Operation::Defragmentation => "defragmentation",
        }
    }
}

/// Registers accountsdb metrics once, seeding durable counters from persisted stats.
pub(crate) fn init(db: &AccountsDB) {
    METRICS.get_or_init(|| Metrics::new(db.persisted.storage.stats()));
}

/// Records one persisted account image load.
pub(crate) fn read() {
    metric::with_metrics(&METRICS, |m| m.reads.inc());
}

/// Records one borrowed account commit into persisted storage.
pub(crate) fn commit() {
    metric::with_metrics(&METRICS, |m| m.commits.inc());
}

/// Records one fresh allocation from the mapped persisted storage file.
pub(crate) fn alloc() {
    metric::with_metrics(&METRICS, |m| m.allocs.inc());
}

/// Records one allocation reuse from the persisted freelist.
pub(crate) fn realloc() {
    metric::with_metrics(&METRICS, |m| m.reallocs.inc());
}

/// Records persisted account relocations during defragmentation.
pub(crate) fn compaction(count: u64) {
    metric::with_metrics(&METRICS, |m| m.compactions.inc_by(count));
}

/// Records one persisted account removal.
pub(crate) fn removal() {
    metric::with_metrics(&METRICS, |m| m.removals.inc());
}

/// Records one persisted storage file resize.
pub(crate) fn resize() {
    metric::with_metrics(&METRICS, |m| m.resizes.inc());
}

/// Starts an operation timer that records latency when the returned guard drops.
pub(crate) fn time(op: Operation) -> metric::OperationTimer<'static> {
    op.time(METRICS.get().map(|m| &m.operations))
}

/// Owns all Prometheus collectors registered by accountsdb.
struct Metrics {
    /// Durable persisted account image load counter.
    reads: IntCounter,
    /// Durable borrowed account commit counter.
    commits: IntCounter,
    /// Durable fresh allocation counter.
    allocs: IntCounter,
    /// Durable freelist reuse counter.
    reallocs: IntCounter,
    /// Durable defragmentation relocation counter.
    compactions: IntCounter,
    /// Durable persisted account removal counter.
    removals: IntCounter,
    /// Durable persisted storage resize counter.
    resizes: IntCounter,
    /// Runtime operation duration and completion counters.
    operations: OperationCounters,
}

impl Metrics {
    /// Builds collectors and seeds durable counters from persisted mmap stats.
    fn new(stats: &Stats) -> Self {
        Self {
            reads: metric::counter(READS, stats.reads.load(Relaxed)),
            commits: metric::counter(COMMITS, stats.commits.load(Relaxed)),
            allocs: metric::counter(ALLOCS, stats.allocs.load(Relaxed)),
            reallocs: metric::counter(REALLOCS, stats.reallocs.load(Relaxed)),
            compactions: metric::counter(COMPACTIONS, stats.compactions.load(Relaxed)),
            removals: metric::counter(REMOVALS, stats.removals.load(Relaxed)),
            resizes: metric::counter(RESIZES, stats.resizes.load(Relaxed)),
            operations: OperationCounters::new(OPERATION_TIME),
        }
    }
}
