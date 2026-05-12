//! Prometheus metric helpers shared by engine crates.

use std::{fmt::Display, sync::OnceLock, time::Instant};

use prometheus::{HistogramOpts, HistogramVec, Opts, default_registry};
pub use prometheus::{IntCounter, IntCounterVec, IntGauge, IntGaugeVec};
use tracing::{info, warn};

/// Duration logger for the time elapsed between events and their total duration.
pub struct EventTimer {
    sequence: &'static str,
    start: Instant,
    interval: Instant,
}

impl EventTimer {
    /// Initialize a new timer for the given sequence of events
    pub fn new(sequence: &'static str) -> Self {
        let start = Instant::now();
        let interval = start;
        Self { sequence, start, interval }
    }
    /// Logs the supplied event and time since construction or the previous event,
    /// then starts a new interval.
    pub fn record(&mut self, event: impl Display) {
        let elapsed = self.interval.elapsed();
        self.interval = Instant::now();
        info!(?elapsed, "{}: {event}", self.sequence);
    }
}

impl Drop for EventTimer {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        info!(?elapsed, "{} is complete", self.sequence);
    }
}

/// Metric name and help text kept together so collector definitions stay grepable.
#[derive(Clone, Copy)]
pub struct MetricSpec {
    /// Prometheus collector name.
    pub name: &'static str,
    /// Prometheus help text.
    pub help: &'static str,
}

/// Creates a counter and applies an initial value before registration.
pub fn counter(spec: MetricSpec, initial: u64) -> IntCounter {
    let counter = validate(IntCounter::with_opts(Opts::new(spec.name, spec.help)));
    counter.inc_by(initial);
    counter
}

/// Creates a labeled counter.
pub fn counter_vec(spec: MetricSpec, labels: &[&'static str]) -> IntCounterVec {
    validate(IntCounterVec::new(Opts::new(spec.name, spec.help), labels))
}

/// Creates a gauge and applies an initial value before registration.
pub fn gauge(spec: MetricSpec, initial: i64) -> IntGauge {
    let gauge = validate(IntGauge::with_opts(Opts::new(spec.name, spec.help)));
    gauge.set(initial);
    gauge
}

/// Creates a labeled gauge.
pub fn gauge_vec(spec: MetricSpec, labels: &[&'static str]) -> IntGaugeVec {
    validate(IntGaugeVec::new(Opts::new(spec.name, spec.help), labels))
}

/// Registers `collector`, logging duplicate-name or registry errors without aborting startup.
pub fn register<C>(namespace: &'static str, collector: C)
where
    C: prometheus::core::Collector + 'static,
{
    if let Err(error) = default_registry().register(Box::new(collector)) {
        warn!(namespace, ?error, "failed to register metric");
    }
}

/// Applies `f` when metrics have been initialized; early calls are intentionally no-ops.
pub fn with_metrics<T>(metrics: &OnceLock<T>, f: impl FnOnce(&T)) {
    if let Some(metrics) = metrics.get() {
        f(metrics);
    }
}

/// Low-cardinality operation label used by duration histograms.
pub trait MetricOperation: Copy {
    /// Returns the Prometheus label value for this operation.
    fn label(self) -> &'static str;

    /// Starts a timer against `counters`, or a no-op timer before metrics are initialized.
    fn time(self, counters: Option<&OperationCounters>) -> OperationTimer<'_> {
        counters.map(|c| c.time(self)).unwrap_or_else(|| OperationTimer::noop(self))
    }
}

/// Runtime operation duration histogram sharing one `op` label.
pub struct OperationCounters(HistogramVec);

/// Operation duration histogram buckets, in microseconds.
const OPERATION_BUCKETS_MICROS: [f64; 8] =
    [50.0, 200.0, 800.0, 3_200.0, 12_800.0, 51_200.0, 204_800.0, 1_000_000.0];

impl OperationCounters {
    /// Builds the duration histogram collector.
    pub fn new(micros: MetricSpec) -> Self {
        let opts =
            HistogramOpts::new(micros.name, micros.help).buckets(OPERATION_BUCKETS_MICROS.to_vec());
        Self(validate(HistogramVec::new(opts, &["op"])))
    }

    /// Registers the operation histogram in the default Prometheus registry.
    pub fn register(&self, namespace: &'static str) {
        register(namespace, self.0.clone());
    }

    /// Starts an operation timer that records latency when the returned guard drops.
    pub fn time(&self, op: impl MetricOperation) -> OperationTimer<'_> {
        OperationTimer {
            counters: Some(self),
            op: op.label(),
            started: Instant::now(),
        }
    }
}

/// Drop guard that records elapsed operation time in the metrics registry.
pub struct OperationTimer<'a> {
    /// Operation counters to update on drop.
    counters: Option<&'a OperationCounters>,
    /// Operation label recorded with the duration observation.
    op: &'static str,
    /// Monotonic start instant captured when the guard is created.
    started: Instant,
}

impl OperationTimer<'static> {
    /// Returns a timer that intentionally records nothing.
    pub fn noop(op: impl MetricOperation) -> Self {
        Self {
            counters: None,
            op: op.label(),
            started: Instant::now(),
        }
    }
}

impl Drop for OperationTimer<'_> {
    /// Records elapsed microseconds when the timer leaves scope.
    fn drop(&mut self) {
        let Some(counters) = self.counters else {
            return;
        };
        let elapsed = self.started.elapsed().as_micros() as f64;
        counters.0.with_label_values(&[self.op]).observe(elapsed);
    }
}

/// Unwraps construction of static metric definitions.
#[allow(clippy::expect_used)]
fn validate<T>(result: prometheus::Result<T>) -> T {
    result.expect("prometheus metric registration should succeed")
}
