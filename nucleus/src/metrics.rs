//! Prometheus metric helpers shared by engine crates.

use std::{fmt::Display, sync::OnceLock, time::Instant};

use prometheus::default_registry;
pub use prometheus::{
    Histogram, HistogramOpts, HistogramTimer, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts,
};
use tracing::{info, warn};

/// Prometheus namespace shared by all engine metrics.
const NAMESPACE: &str = "engine";

/// Logs time between recorded events and total elapsed time when dropped.
pub struct EventTimer {
    sequence: &'static str,
    start: Instant,
    interval: Instant,
}

impl EventTimer {
    /// Starts timing an event sequence identified by `sequence`.
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

/// Engine metric name and help text, converted to options with the `engine` namespace.
/// Use explicit [`Opts`] for collectors with caller-controlled names or namespaces.
#[derive(Clone, Copy)]
pub struct MetricSpec {
    /// Prometheus collector name.
    pub name: &'static str,
    /// Prometheus help text.
    pub help: &'static str,
}

impl From<MetricSpec> for Opts {
    fn from(spec: MetricSpec) -> Self {
        Self::new(spec.name, spec.help).namespace(NAMESPACE)
    }
}

/// Creates and registers a counter after applying its initial value.
pub fn counter(opts: impl Into<Opts>, initial: u64) -> IntCounter {
    let counter = validate(IntCounter::with_opts(opts.into()));
    counter.inc_by(initial);
    register(counter)
}

/// Creates and registers a labeled counter.
pub fn counter_vec(opts: impl Into<Opts>, labels: &[&str]) -> IntCounterVec {
    register(validate(IntCounterVec::new(opts.into(), labels)))
}

/// Creates and registers a gauge after applying its initial value.
pub fn gauge(opts: impl Into<Opts>, initial: i64) -> IntGauge {
    let gauge = validate(IntGauge::with_opts(opts.into()));
    gauge.set(initial);
    register(gauge)
}

/// Creates and registers a labeled gauge.
pub fn gauge_vec(opts: impl Into<Opts>, labels: &[&str]) -> IntGaugeVec {
    register(validate(IntGaugeVec::new(opts.into(), labels)))
}

/// Creates and registers a histogram with caller-controlled names, units, and buckets.
pub fn histogram(opts: HistogramOpts) -> Histogram {
    register(validate(Histogram::with_opts(opts)))
}

/// Creates and registers a labeled histogram with caller-controlled options.
pub fn histogram_vec(opts: HistogramOpts, labels: &[&str]) -> HistogramVec {
    register(validate(HistogramVec::new(opts, labels)))
}

/// Converts an unsigned metric value to a saturating Prometheus gauge value.
pub fn gauge_value<T>(value: T) -> i64
where
    i64: TryFrom<T>,
{
    i64::try_from(value).unwrap_or(i64::MAX)
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
        let opts = HistogramOpts::new(micros.name, micros.help)
            .namespace(NAMESPACE)
            .buckets(OPERATION_BUCKETS_MICROS.to_vec());
        Self(histogram_vec(opts, &["op"]))
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

/// Registers `collector`, logging registry errors without aborting startup.
fn register<C>(collector: C) -> C
where
    C: prometheus::core::Collector + Clone + 'static,
{
    if let Err(error) = default_registry().register(Box::new(collector.clone())) {
        let descriptors = collector.desc();
        let name = descriptors.first().map(|desc| desc.fq_name.as_str());
        warn!(metric = name, ?error, "failed to register metric");
    }
    collector
}

/// Unwraps construction of static metric definitions.
#[allow(clippy::expect_used)]
fn validate<T>(result: prometheus::Result<T>) -> T {
    result.expect("valid static Prometheus metric definition")
}
