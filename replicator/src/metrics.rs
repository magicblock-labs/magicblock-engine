//! Prometheus metrics for replication clients and servers.

use std::sync::OnceLock;

use nucleus::metrics::{self as metric, OperationTimer};
use nucleus::metrics::{IntCounter, IntGauge, MetricOperation, MetricSpec, OperationCounters};

/// Process-wide replicator metrics registered in the default Prometheus registry.
static METRICS: OnceLock<Metrics> = OnceLock::new();

const OPERATION_TIME: MetricSpec = MetricSpec {
    name: "replicator_operation_duration_micros",
    help: "Replication operation duration distribution in microseconds.",
};
const CLIENT_STREAM_CONNECTED: MetricSpec = MetricSpec {
    name: "replicator_client_stream_connected",
    help: "Whether the replication client currently holds a live blockstore stream.",
};
const SERVER_CONNECTIONS: MetricSpec = MetricSpec {
    name: "replicator_server_connections",
    help: "Current replication server connection workers.",
};
const CLIENT_CONNECTION_ATTEMPTS: MetricSpec = MetricSpec {
    name: "replicator_client_connection_attempts",
    help: "Replication client connection attempts.",
};
const CLIENT_STATE_MISMATCHES: MetricSpec = MetricSpec {
    name: "replicator_client_state_mismatches",
    help: "Superblock seal mismatches detected by the replication client.",
};
const SERVER_CURSOR_UPDATES_SKIPPED: MetricSpec = MetricSpec {
    name: "replicator_server_cursor_updates_skipped",
    help: "Replication server cursor updates skipped after receiver lag.",
};

/// Replication operation used as a fixed low-cardinality label.
#[derive(Clone, Copy)]
pub(crate) enum Operation {
    ClientConnect,
    ClientStageSnapshot,
    ServerHandshake,
    ServerAdvance,
    ServerSendSnapshot,
}

impl MetricOperation for Operation {
    fn label(self) -> &'static str {
        match self {
            Self::ClientConnect => "client_connect",
            Self::ClientStageSnapshot => "client_stage_snapshot",
            Self::ServerHandshake => "server_handshake",
            Self::ServerAdvance => "server_advance",
            Self::ServerSendSnapshot => "server_send_snapshot",
        }
    }
}

/// Registers all replicator metrics once.
pub(crate) fn init() {
    METRICS.get_or_init(Default::default);
}

/// Starts an operation timer that records latency when the returned guard drops.
pub(crate) fn time(op: Operation) -> OperationTimer<'static> {
    op.time(METRICS.get().map(|m| &m.operations))
}

/// Records a client connection attempt.
pub(crate) fn client_connection_attempt() {
    metric::with_metrics(&METRICS, |m| m.client_connection_attempts.inc());
}

/// Marks a client blockstore stream live until the returned guard drops.
pub(crate) fn client_connection() -> ClientConnection {
    metric::with_metrics(&METRICS, |m| m.client_stream_connected.set(1));
    ClientConnection
}

/// Records a superblock seal mismatch.
pub(crate) fn client_state_mismatch() {
    metric::with_metrics(&METRICS, |m| m.client_state_mismatches.inc());
}

/// Counts a server worker until the returned guard drops.
pub(crate) fn server_connection() -> ServerConnection {
    metric::with_metrics(&METRICS, |m| m.server_connections.inc());
    ServerConnection
}

/// Records durable cursor updates skipped by a lagged receiver.
pub(crate) fn server_cursor_updates_skipped(skipped: u64) {
    metric::with_metrics(&METRICS, |m| {
        m.server_cursor_updates_skipped.inc_by(skipped)
    });
}

/// Clears the live-client gauge on every stream exit path.
pub(crate) struct ClientConnection;

impl Drop for ClientConnection {
    fn drop(&mut self) {
        metric::with_metrics(&METRICS, |m| m.client_stream_connected.set(0));
    }
}

/// Decrements the active-server-worker gauge on every worker exit path.
pub(crate) struct ServerConnection;

impl Drop for ServerConnection {
    fn drop(&mut self) {
        metric::with_metrics(&METRICS, |m| m.server_connections.dec());
    }
}

/// Owns all Prometheus collectors registered by replicator.
struct Metrics {
    operations: OperationCounters,
    client_stream_connected: IntGauge,
    server_connections: IntGauge,
    client_connection_attempts: IntCounter,
    client_state_mismatches: IntCounter,
    server_cursor_updates_skipped: IntCounter,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            operations: OperationCounters::new(OPERATION_TIME),
            client_stream_connected: metric::gauge(CLIENT_STREAM_CONNECTED, 0),
            server_connections: metric::gauge(SERVER_CONNECTIONS, 0),
            client_connection_attempts: metric::counter(CLIENT_CONNECTION_ATTEMPTS, 0),
            client_state_mismatches: metric::counter(CLIENT_STATE_MISMATCHES, 0),
            server_cursor_updates_skipped: metric::counter(SERVER_CURSOR_UPDATES_SKIPPED, 0),
        }
    }
}
