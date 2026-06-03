//! Prometheus metrics for processor.

use std::sync::OnceLock;

use nucleus::metrics::{self as metric, OperationTimer};
use nucleus::metrics::{
    IntCounter, IntCounterVec, IntGauge, MetricOperation, MetricSpec, OperationCounters,
};

/// Process-wide processor metrics registered in the default Prometheus registry.
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Operation latency histogram recorded in microseconds.
const OPERATION_TIME: MetricSpec = MetricSpec {
    name: "processor_operation_duration_micros",
    help: "Processor operation duration distribution in microseconds.",
};
/// Executors currently running transaction batches.
const BUSY_EXECUTORS: MetricSpec = MetricSpec {
    name: "processor_busy_executors",
    help: "Current processor executors running transaction batches.",
};
/// Transactions queued behind account-lock conflicts.
const BLOCKED_TRANSACTIONS: MetricSpec = MetricSpec {
    name: "processor_blocked_transactions",
    help: "Current processor transactions queued behind account-lock conflicts.",
};
/// Account lock conflict counter.
const LOCK_CONFLICTS: MetricSpec = MetricSpec {
    name: "processor_lock_conflicts",
    help: "Processor account-lock conflicts.",
};
/// Failed transaction counter grouped by terminal failure kind.
const FAILED_TRANSACTIONS: MetricSpec = MetricSpec {
    name: "processor_failed_transactions",
    help: "Transactions dropped by the sequencer or failed during execution.",
};

/// Fixed failure kinds used to select pre-resolved counter handles.
#[derive(Clone, Copy)]
pub(crate) enum FailureKind {
    /// Transaction rejected before executor dispatch.
    SequencerDrop = 0,
    /// Transaction load or execution result was unsuccessful.
    Execution = 1,
}

/// Processor operation used as a low-cardinality operation label.
#[derive(Clone, Copy)]
pub(crate) enum Operation {
    /// Block finalization path.
    FinalizeBlock,
    /// Quiescence barrier drain path.
    BarrierDrain,
}

impl MetricOperation for Operation {
    /// Returns the Prometheus label value for this operation.
    fn label(self) -> &'static str {
        match self {
            Operation::FinalizeBlock => "finalize_block",
            Operation::BarrierDrain => "barrier_drain",
        }
    }
}

/// Registers processor metrics once.
pub(crate) fn init() {
    METRICS.get_or_init(Default::default);
}

/// Starts an operation timer that records latency when the returned guard drops.
pub(crate) fn time(op: Operation) -> OperationTimer<'static> {
    op.time(METRICS.get().map(|m| &m.operations))
}

/// Refreshes the busy executor gauge.
pub(crate) fn busy_executors(count: usize) {
    metric::with_metrics(&METRICS, |m| m.busy_executors.set(count as i64));
}

/// Records one transaction queued behind an executor.
pub(crate) fn blocked_transaction() {
    metric::with_metrics(&METRICS, |m| m.blocked_transactions.inc());
}

/// Records one queued transaction leaving an executor backlog.
pub(crate) fn unblocked_transaction() {
    metric::with_metrics(&METRICS, |m| m.blocked_transactions.dec());
}

/// Records one account-lock conflict.
pub(crate) fn lock_conflict() {
    metric::with_metrics(&METRICS, |m| m.lock_conflicts.inc());
}

/// Records one terminal transaction failure.
pub(crate) fn failed_transaction(kind: FailureKind) {
    metric::with_metrics(&METRICS, |m| m.failed_transactions[kind as usize].inc());
}

/// Owns all Prometheus collectors registered by processor.
struct Metrics {
    /// Runtime operation duration and completion counters.
    operations: OperationCounters,
    /// Executors currently running transaction batches.
    busy_executors: IntGauge,
    /// Transactions queued behind account-lock conflicts.
    blocked_transactions: IntGauge,
    /// Account-lock conflict counter.
    lock_conflicts: IntCounter,
    /// Registered failed transaction counter labeled by kind.
    failed_transactions_vec: IntCounterVec,
    /// Per-kind failed transaction counters resolved during initialization.
    failed_transactions: [IntCounter; 2],
}

impl Default for Metrics {
    /// Builds collectors and registers them in the default Prometheus registry.
    fn default() -> Self {
        let failed_transactions_vec = metric::counter_vec(FAILED_TRANSACTIONS, &["kind"]);
        let failed_transactions = [
            failed_transactions_vec.with_label_values(&["dropped"]),
            failed_transactions_vec.with_label_values(&["execution"]),
        ];
        let metrics = Self {
            operations: OperationCounters::new(OPERATION_TIME),
            busy_executors: metric::gauge(BUSY_EXECUTORS, 0),
            blocked_transactions: metric::gauge(BLOCKED_TRANSACTIONS, 0),
            lock_conflicts: metric::counter(LOCK_CONFLICTS, 0),
            failed_transactions_vec,
            failed_transactions,
        };
        metrics.register();
        metrics
    }
}

impl Metrics {
    /// Registers all collectors in the default Prometheus registry.
    fn register(&self) {
        self.operations.register("processor");
        metric::register("processor", self.busy_executors.clone());
        metric::register("processor", self.blocked_transactions.clone());
        metric::register("processor", self.lock_conflicts.clone());
        metric::register("processor", self.failed_transactions_vec.clone());
    }
}
