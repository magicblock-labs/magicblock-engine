//! Prometheus metrics for keeper.

use std::sync::OnceLock;

use nucleus::metrics::{self as metric, OperationTimer};
use nucleus::metrics::{IntCounter, IntGauge, MetricOperation, MetricSpec, OperationCounters};

/// Process-wide keeper metrics registered in the default Prometheus registry.
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Operation latency histogram recorded in microseconds.
const OPERATION_TIME: MetricSpec = MetricSpec {
    name: "keeper_operation_duration_micros",
    help: "Keeper operation duration distribution in microseconds.",
};
/// Account load cache entries.
const ACCOUNT_CACHE_ENTRIES: MetricSpec = MetricSpec {
    name: "keeper_account_cache_entries",
    help: "Current account load cache entries.",
};
/// Signature cache entries.
const SIGNATURE_CACHE_ENTRIES: MetricSpec = MetricSpec {
    name: "keeper_signature_cache_entries",
    help: "Current signature cache entries.",
};
/// Block hash cache entries.
const BLOCK_HASH_CACHE_ENTRIES: MetricSpec = MetricSpec {
    name: "keeper_block_hash_cache_entries",
    help: "Current block hash cache entries.",
};
/// Most recently completed snapshot archive size.
const SNAPSHOT_SIZE: MetricSpec = MetricSpec {
    name: "keeper_snapshot_size",
    help: "Most recently completed snapshot archive size in bytes.",
};
/// Account cache eviction counter.
const ACCOUNT_CACHE_EVICTIONS: MetricSpec = MetricSpec {
    name: "keeper_account_cache_evictions",
    help: "Account cache evictions.",
};
/// Account resolution conflict counter.
const ACCOUNT_RESOLUTION_RACES: MetricSpec = MetricSpec {
    name: "keeper_account_resolution_races",
    help: "Account resolution race conditions.",
};

/// Keeper operation used as a low-cardinality operation label.
#[derive(Clone, Copy)]
pub(crate) enum Operation {
    /// Superblock finalization path.
    FinalizeSuperblock,
    /// Idle subscription cleanup path.
    Cleanup,
    /// Snapshot tar/zstd archival path.
    ArchiveSnapshot,
}

impl MetricOperation for Operation {
    /// Returns the Prometheus label value for this operation.
    fn label(self) -> &'static str {
        match self {
            Operation::FinalizeSuperblock => "finalize_superblock",
            Operation::Cleanup => "cleanup",
            Operation::ArchiveSnapshot => "archive_snapshot",
        }
    }
}

/// Registers keeper metrics once and seeds gauges from current caches.
pub(crate) fn init() {
    METRICS.get_or_init(Default::default);
}

/// Starts an operation timer that records latency when the returned guard drops.
pub(crate) fn time(op: Operation) -> OperationTimer<'static> {
    op.time(METRICS.get().map(|m| &m.operations))
}

/// Records an account cache insertion that increases current occupancy.
pub(crate) fn account_cache_insert() {
    metric::with_metrics(&METRICS, |m| m.account_cache_entries.inc());
}

/// Refreshes signature cache entry gauge.
pub(crate) fn signature_entries(count: usize) {
    metric::with_metrics(&METRICS, |m| m.signature_cache_entries.set(count as i64));
}

/// Refreshes block hash cache entry gauge.
pub(crate) fn block_hash_entries(count: usize) {
    metric::with_metrics(&METRICS, |m| m.block_hash_cache_entries.set(count as i64));
}

/// Records the most recently completed snapshot archive size in bytes.
pub(crate) fn snapshot_size(bytes: u64) {
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    metric::with_metrics(&METRICS, |m| m.snapshot_size.set(bytes));
}

/// Records one account cache eviction.
pub(crate) fn account_cache_eviction() {
    metric::with_metrics(&METRICS, |m| m.account_cache_evictions.inc());
}

/// Records one account resolution race condition.
pub(crate) fn account_resolution_race() {
    metric::with_metrics(&METRICS, |m| m.account_resolution_race.inc());
}

/// Owns all Prometheus collectors registered by keeper.
struct Metrics {
    /// Runtime operation duration and completion counters.
    operations: OperationCounters,
    /// Account cache entry gauge.
    account_cache_entries: IntGauge,
    /// Signature cache entry gauge.
    signature_cache_entries: IntGauge,
    /// Block hash cache entry gauge.
    block_hash_cache_entries: IntGauge,
    /// Most recently completed snapshot archive size in bytes.
    snapshot_size: IntGauge,
    /// Account cache eviction counter.
    account_cache_evictions: IntCounter,
    /// Account resolution race conditions counter.
    account_resolution_race: IntCounter,
}
impl Default for Metrics {
    /// Builds collectors and registers them in the default Prometheus registry.
    fn default() -> Self {
        let metrics = Self {
            operations: OperationCounters::new(OPERATION_TIME),
            account_cache_entries: metric::gauge(ACCOUNT_CACHE_ENTRIES, 0),
            signature_cache_entries: metric::gauge(SIGNATURE_CACHE_ENTRIES, 0),
            block_hash_cache_entries: metric::gauge(BLOCK_HASH_CACHE_ENTRIES, 0),
            snapshot_size: metric::gauge(SNAPSHOT_SIZE, 0),
            account_cache_evictions: metric::counter(ACCOUNT_CACHE_EVICTIONS, 0),
            account_resolution_race: metric::counter(ACCOUNT_RESOLUTION_RACES, 0),
        };
        metrics.register();
        metrics
    }
}

impl Metrics {
    /// Registers all collectors in the default Prometheus registry.
    fn register(&self) {
        self.operations.register("keeper");
        metric::register("keeper", self.account_cache_entries.clone());
        metric::register("keeper", self.signature_cache_entries.clone());
        metric::register("keeper", self.block_hash_cache_entries.clone());
        metric::register("keeper", self.snapshot_size.clone());
        metric::register("keeper", self.account_cache_evictions.clone());
        metric::register("keeper", self.account_resolution_race.clone());
    }
}
