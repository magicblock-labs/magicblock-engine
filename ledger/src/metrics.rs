//! Prometheus metrics for ledger.

use std::sync::{OnceLock, atomic::Ordering::Relaxed};

use nucleus::metrics as metric;
use nucleus::metrics::{IntGauge, MetricOperation, MetricSpec, OperationCounters};

use crate::Ledger;

/// Process-wide ledger metrics registered in the default Prometheus registry.
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Operation latency histogram recorded in microseconds.
const OPERATION_TIME: MetricSpec = MetricSpec {
    name: "ledger_operation_duration_micros",
    help: "Ledger operation duration distribution in microseconds.",
};
/// Transactions waiting for execution metadata in the appender.
const PENDING_TRANSACTIONS: MetricSpec = MetricSpec {
    name: "ledger_pending_transactions",
    help: "Current ledger transactions waiting for execution metadata.",
};
/// Total committed transactions from ledger metadata.
const TRANSACTIONS: MetricSpec = MetricSpec {
    name: "ledger_transactions",
    help: "Current total transactions committed into ledger metadata.",
};
/// Total committed blocks from ledger metadata.
const BLOCKS: MetricSpec = MetricSpec {
    name: "ledger_blocks",
    help: "Current total blocks committed into ledger metadata.",
};
/// Total superblocks allocated from ledger metadata.
const SUPERBLOCKS: MetricSpec = MetricSpec {
    name: "ledger_superblocks",
    help: "Current total ledger superblocks allocated from genesis.",
};

/// Ledger operation used as a low-cardinality operation label.
#[derive(Clone, Copy)]
pub(crate) enum Operation {
    /// Transaction read request.
    ReadTransaction,
    /// Transaction status read request.
    ReadTransactionStatus,
    /// Account signature history read request.
    ReadAccountSignatures,
    /// Single-block read request.
    ReadBlock,
    /// Block range read request.
    ReadBlockRange,
    /// Retained blockstore replay request.
    Replay,
    /// Superblock rotation path.
    Rotate,
    /// Retention truncation path.
    Truncate,
    /// Data-file sync path.
    FileSync,
    /// Data sync to OS buffers
    BufferSync,
    /// Data-file finalization path.
    FileFinalize,
}

impl MetricOperation for Operation {
    /// Returns the Prometheus label value for this operation.
    fn label(self) -> &'static str {
        match self {
            Operation::ReadTransaction => "read_transaction",
            Operation::ReadTransactionStatus => "read_transaction_status",
            Operation::ReadAccountSignatures => "read_account_signatures",
            Operation::ReadBlock => "read_block",
            Operation::ReadBlockRange => "read_block_range",
            Operation::Replay => "replay",
            Operation::Rotate => "rotate",
            Operation::Truncate => "truncate",
            Operation::FileSync => "file_sync",
            Operation::FileFinalize => "file_finalize",
            Operation::BufferSync => "buffer_sync",
        }
    }
}

/// Registers ledger metrics once and seeds gauges from opened ledger metadata.
pub(crate) fn init(ledger: &Ledger) {
    METRICS.get_or_init(|| Metrics::new(ledger));
}

/// Starts an operation timer that records latency when the returned guard drops.
pub(crate) fn time(op: Operation) -> metric::OperationTimer<'static> {
    op.time(METRICS.get().map(|m| &m.operations))
}

/// Refreshes the current pending transaction count.
pub(crate) fn pending_transactions(count: usize) {
    metric::with_metrics(&METRICS, |m| {
        m.pending_transactions.set(metric::gauge_value(count))
    });
}

/// Refreshes ledger-wide count gauges from metadata.
pub(crate) fn ledger_counts(ledger: &Ledger) {
    metric::with_metrics(&METRICS, |m| m.ledger_counts(ledger));
}

/// Owns all Prometheus collectors registered by ledger.
struct Metrics {
    /// Runtime operation duration and completion counters.
    operations: OperationCounters,
    /// Runtime pending transaction count.
    pending_transactions: IntGauge,
    /// Runtime transaction total gauge.
    transactions: IntGauge,
    /// Runtime block total gauge.
    blocks: IntGauge,
    /// Runtime superblock total gauge.
    superblocks: IntGauge,
}

impl Metrics {
    /// Builds collectors and registers them in the default Prometheus registry.
    fn new(ledger: &Ledger) -> Self {
        Self {
            operations: OperationCounters::new(OPERATION_TIME),
            pending_transactions: metric::gauge(PENDING_TRANSACTIONS, 0),
            transactions: metric::gauge(
                TRANSACTIONS,
                metric::gauge_value(ledger.meta.transactions.load(Relaxed)),
            ),
            blocks: metric::gauge(
                BLOCKS,
                metric::gauge_value(ledger.meta.blocks.load(Relaxed)),
            ),
            superblocks: metric::gauge(SUPERBLOCKS, metric::gauge_value(ledger.meta.head())),
        }
    }

    /// Refreshes count gauges from ledger metadata.
    fn ledger_counts(&self, ledger: &Ledger) {
        self.transactions
            .set(metric::gauge_value(ledger.meta.transactions.load(Relaxed)));
        self.blocks.set(metric::gauge_value(ledger.meta.blocks.load(Relaxed)));
        self.superblocks.set(metric::gauge_value(ledger.meta.head()));
    }
}
