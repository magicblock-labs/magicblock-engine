//! Prometheus metrics for processor.

use std::sync::OnceLock;

use nucleus::metrics::{self as metric, OperationTimer};
use nucleus::metrics::{IntCounter, IntGauge, MetricOperation, MetricSpec, OperationCounters};

/// Process-wide processor metrics registered in the default Prometheus registry.
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Operation latency histogram recorded in microseconds.
const OPERATION_TIME: MetricSpec = MetricSpec {
    name: "processor_operation_duration_micros",
    help: "Processor operation duration distribution in microseconds.",
};
/// Executors currently running transactions.
const BUSY_EXECUTORS: MetricSpec = MetricSpec {
    name: "processor_busy_executors",
    help: "Current processor executors running transactions.",
};
/// Transactions waiting for input-order dependencies.
const BLOCKED_TRANSACTIONS: MetricSpec = MetricSpec {
    name: "processor_blocked_transactions",
    help: "Current processor transactions with unfinished ordering dependencies.",
};
/// Transactions registered with an ordering dependency.
const LOCK_CONFLICTS: MetricSpec = MetricSpec {
    name: "processor_lock_conflicts",
    help: "Processor transactions registered with an account ordering dependency.",
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
    metric::with_metrics(&METRICS, |m| {
        m.busy_executors.set(metric::gauge_value(count))
    });
}

/// Records one transaction waiting for ordering dependencies.
pub(crate) fn blocked_transaction() {
    metric::with_metrics(&METRICS, |m| m.blocked_transactions.inc());
}

/// Records transactions whose ordering dependencies completed together.
pub(crate) fn unblocked_transactions(count: usize) {
    metric::with_metrics(&METRICS, |m| {
        m.blocked_transactions.sub(metric::gauge_value(count))
    });
}

/// Records one transaction registered with an ordering dependency.
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
    /// Executors currently running transactions.
    busy_executors: IntGauge,
    /// Transactions waiting for input-order dependencies.
    blocked_transactions: IntGauge,
    /// Transactions registered with an ordering dependency.
    lock_conflicts: IntCounter,
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
        Self {
            operations: OperationCounters::new(OPERATION_TIME),
            busy_executors: metric::gauge(BUSY_EXECUTORS, 0),
            blocked_transactions: metric::gauge(BLOCKED_TRANSACTIONS, 0),
            lock_conflicts: metric::counter(LOCK_CONFLICTS, 0),
            failed_transactions,
        }
    }
}
