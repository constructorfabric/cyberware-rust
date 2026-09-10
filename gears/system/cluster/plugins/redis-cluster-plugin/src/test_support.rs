//! A recording [`ClusterMetrics`] double, shared by the Layer-1 tests of every
//! module that emits (TESTING.md §2).
//!
//! The ADR-004 contract is a *port*, so "which signal fires for which outcome"
//! is decidable with no server: a recording sink turns the question into an
//! assertion over a list. Only the OpenTelemetry adapter and the real log lines
//! need a container, which is what the `RD-SPEC-*` runs cover.

use std::sync::{Arc, Mutex};

use cluster_sdk::observability::ClusterMetrics;
use opentelemetry::metrics::Meter;

use crate::observability::{RedisSignals, plugin_meter};

/// One recorded call on the [`ClusterMetrics`] port.
///
/// Durations are recorded as the bare fact that a duration was recorded rather
/// than its value: an elapsed time is not reproducible, and what the tests need
/// to know is that the histogram was fed for the same op the counter was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// `cluster_cache_ops_total{op,result}`.
    CacheOp(String, String),
    /// `cluster_cache_op_duration_seconds{op}`.
    CacheDuration(String),
    /// `cluster_lock_ops_total{op,result}`.
    LockOp(String, String),
    /// `cluster_lock_op_duration_seconds{op}`.
    LockDuration(String),
    /// `cluster_leader_transitions_total{transition}`.
    LeaderTransition(String),
    /// `cluster_watch_resets_total{primitive}`.
    WatchReset(String),
    /// `cluster_provider_errors_total{kind}`.
    ProviderError(String),
}

/// A [`ClusterMetrics`] that remembers everything it was told, in order.
#[derive(Debug, Default)]
pub struct RecordingMetrics {
    recorded: Mutex<Vec<Signal>>,
}

impl RecordingMetrics {
    /// Everything recorded so far, in emission order.
    pub fn signals(&self) -> Vec<Signal> {
        self.recorded
            .lock()
            .expect("the recording metrics mutex is never poisoned in tests")
            .clone()
    }

    /// The `(op, result)` pairs recorded on `cluster_lock_ops_total`.
    pub fn lock_ops(&self) -> Vec<(String, String)> {
        self.signals()
            .into_iter()
            .filter_map(|signal| match signal {
                Signal::LockOp(op, result) => Some((op, result)),
                _ => None,
            })
            .collect()
    }

    /// The bounded `kind`s recorded on `cluster_provider_errors_total`.
    pub fn provider_error_kinds(&self) -> Vec<String> {
        self.signals()
            .into_iter()
            .filter_map(|signal| match signal {
                Signal::ProviderError(kind) => Some(kind),
                _ => None,
            })
            .collect()
    }

    /// The `primitive`s recorded on `cluster_watch_resets_total`.
    pub fn watch_resets(&self) -> Vec<String> {
        self.signals()
            .into_iter()
            .filter_map(|signal| match signal {
                Signal::WatchReset(primitive) => Some(primitive),
                _ => None,
            })
            .collect()
    }

    fn push(&self, signal: Signal) {
        self.recorded
            .lock()
            .expect("the recording metrics mutex is never poisoned in tests")
            .push(signal);
    }
}

impl ClusterMetrics for RecordingMetrics {
    fn cache_op(&self, op: &str, result: &str) {
        self.push(Signal::CacheOp(op.to_owned(), result.to_owned()));
    }

    fn cache_op_duration(&self, op: &str, _seconds: f64) {
        self.push(Signal::CacheDuration(op.to_owned()));
    }

    fn lock_op(&self, op: &str, result: &str) {
        self.push(Signal::LockOp(op.to_owned(), result.to_owned()));
    }

    fn lock_op_duration(&self, op: &str, _seconds: f64) {
        self.push(Signal::LockDuration(op.to_owned()));
    }

    fn leader_transition(&self, transition: &str) {
        self.push(Signal::LeaderTransition(transition.to_owned()));
    }

    fn watch_reset(&self, primitive: &str) {
        self.push(Signal::WatchReset(primitive.to_owned()));
    }

    fn provider_error(&self, kind: &str) {
        self.push(Signal::ProviderError(kind.to_owned()));
    }
}

/// A meter with no provider installed behind it, so the plugin-local
/// instruments a test builds are inert.
///
/// The default for tests that only care about the [`ClusterMetrics`] port. Use
/// [`metered_signals`] instead when the assertion is about one of the four
/// plugin-local instruments, which are OpenTelemetry-side and need a reader.
pub fn inert_meter() -> Meter {
    plugin_meter()
}

/// A signals value over a recording sink, plus the sink itself.
pub fn recording_signals() -> (Arc<RedisSignals>, Arc<RecordingMetrics>) {
    let recorder = Arc::new(RecordingMetrics::default());
    let signals = Arc::new(RedisSignals::new(
        Arc::clone(&recorder) as Arc<dyn ClusterMetrics>,
        &inert_meter(),
        crate::provider::PROVIDER_NAME,
    ));
    (signals, recorder)
}

/// An in-process reader over the plugin-local instruments.
///
/// Holds the provider as well as the exporter because a counter is only visible
/// after a collection: [`counter`](Self::counter) flushes first, which is what
/// makes the readback deterministic rather than dependent on a periodic
/// reader's timer.
pub struct MetricReadback {
    provider: opentelemetry_sdk::metrics::SdkMeterProvider,
    exporter: opentelemetry_sdk::metrics::InMemoryMetricExporter,
}

impl MetricReadback {
    /// The summed value of the `u64` counter whose *instrument* name is `name`.
    ///
    /// The instrument name, not the contract name: the `_total` suffix is
    /// re-appended by the Prometheus exporter rather than carried on the
    /// instrument, so a caller asserting on `cluster_redis_script_reloads_total`
    /// passes it through the same strip the emitter used. `0` covers both "the
    /// counter exists and is zero" and "nothing was ever recorded", which are
    /// the same thing to a scrape.
    pub fn counter(&self, name: &str) -> u64 {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        let name = name.strip_suffix("_total").unwrap_or(name);
        let _flushed = self.provider.force_flush();
        let Ok(collected) = self.exporter.get_finished_metrics() else {
            return 0;
        };
        let mut total = 0;
        for resource in &collected {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() == name
                        && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                    {
                        total += sum
                            .data_points()
                            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                            .sum::<u64>();
                    }
                }
            }
        }
        total
    }
}

/// A `tracing` writer that appends into a shared buffer.
#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("the log capture mutex is never poisoned in tests")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for SharedWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// Installs a thread-local WARN capture for the current test, returning its
/// uninstall guard and buffer.
///
/// The Layer-1 counterpart of `tests/common`'s `scoped_capture`, and it exists
/// for the assertions a recording [`ClusterMetrics`] cannot make: whether a path
/// that moved a counter also *said* so. A counter and a log stream that disagree
/// leave an operator with a reset they can see in the metric and no line
/// explaining it.
///
/// Thread-local rather than global, so one test's capture cannot see another's
/// events. That is sufficient here because the events under test are emitted
/// inline by a directly-awaited function rather than from a spawned task — the
/// reason `tests/common` needs a global sink as well.
///
/// A process-global sink is installed first all the same, and idempotently,
/// because `tracing` caches each callsite's interest process-wide the first time
/// it is evaluated: another test reaching the callsite first, on a thread with no
/// subscriber, caches it as `Interest::never` and every later evaluation
/// short-circuits before consulting the thread-local dispatcher. The capture then
/// comes back empty and the test fails claiming the event was never emitted.
pub fn scoped_capture() -> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<u8>>>) {
    use std::sync::OnceLock;
    // Discarded. This exists only so the callsites are interesting process-wide.
    static GLOBAL: OnceLock<()> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let sink = tracing_subscriber::fmt()
            .with_writer(SharedWriter(Arc::new(Mutex::new(Vec::new()))))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let _installed = tracing::subscriber::set_global_default(sink);
    });

    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(SharedWriter(Arc::clone(&buf)))
        .with_max_level(tracing::Level::WARN)
        // No ANSI: this buffer is searched as text, and the styling wraps field
        // names in escape codes, so `primitive="cache"` would stop being a
        // contiguous substring while still looking like one in a failure message.
        .with_ansi(false)
        .finish();
    (tracing::subscriber::set_default(subscriber), buf)
}

/// How many times `needle` appears in a capture buffer.
///
/// Every DESIGN.md §9 event carries its catalogued name twice — as the `name:`
/// field and opening the human message — so passing a `logs::*` constant matches
/// the message half, which is what the `fmt` layer prints.
pub fn count_occurrences(buf: &Arc<Mutex<Vec<u8>>>, needle: &str) -> usize {
    let bytes = buf
        .lock()
        .expect("the log capture mutex is never poisoned in tests");
    String::from_utf8_lossy(&bytes).matches(needle).count()
}

/// The whole capture buffer, for a failure message that should show what *was*
/// logged when the expected line is missing.
pub fn captured(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = buf
        .lock()
        .expect("the log capture mutex is never poisoned in tests");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A signals value whose plugin-local instruments are readable in-process.
pub fn metered_signals() -> (Arc<RedisSignals>, MetricReadback) {
    let (signals, _recorder, readback) = recording_metered_signals();
    (signals, readback)
}

/// Both sinks at once: the [`ClusterMetrics`] recorder *and* a reader over the
/// plugin-local instruments.
///
/// For the assertions that span both halves of the signal surface — a path that
/// is specified to move a catalog counter and a Redis-specific one together, so
/// that asserting either alone would leave the other removable without a test
/// failing. [`recording_signals`] and [`metered_signals`] stay as the narrower
/// defaults; this is the same construction with nothing discarded.
pub fn recording_metered_signals() -> (Arc<RedisSignals>, Arc<RecordingMetrics>, MetricReadback) {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let recorder = Arc::new(RecordingMetrics::default());
    let signals = Arc::new(RedisSignals::new(
        Arc::clone(&recorder) as Arc<dyn ClusterMetrics>,
        &provider.meter("redis-cluster-plugin-tests"),
        crate::provider::PROVIDER_NAME,
    ));
    (signals, recorder, MetricReadback { provider, exporter })
}
