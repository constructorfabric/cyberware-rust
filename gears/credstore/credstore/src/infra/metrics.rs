//! OpenTelemetry adapter implementing [`CredStoreMetricsPort`].
//!
//! Instruments are pulled from the process-global meter provider installed by
//! the host; a no-op until an exporter is wired. Instrument names are full
//! literal Prometheus names: counters end in `_total`, duration histograms in
//! `_seconds`, with suffixes baked in (no `.with_unit()`). Matches the
//! platform's `add_metric_suffixes: false` collector posture.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::domain::ports::metrics::{
    CredStoreMetricsPort, Dep, DepOp, FenceVerify, Outcome, ReadOutcome,
};

/// Meter / instrumentation scope name.
pub(crate) const METER_NAME: &str = "credstore";

// ─── Metric names (literal Prometheus form; `add_metric_suffixes: false`) ─────
const CREDSTORE_READ_OUTCOME: &str = "credstore_read_outcome_total";
const CREDSTORE_WALKUP_DEPTH: &str = "credstore_walkup_depth";
const CREDSTORE_DEPENDENCY_QUERY_DURATION: &str = "credstore_dependency_query_duration_seconds";
const CREDSTORE_DEPENDENCY_HEALTH: &str = "credstore_dependency_health_total";
const CREDSTORE_CROSS_TENANT_DENIED: &str = "credstore_cross_tenant_denied_total";
const CREDSTORE_FENCE_VERIFY: &str = "credstore_fence_verify_total";
const CREDSTORE_GC_DELETED: &str = "credstore_gc_deleted_total";
const CREDSTORE_GC_PENDING_RECLAIMED: &str = "credstore_gc_pending_reclaimed_total";
const CREDSTORE_EXPIRED_DELETED: &str = "credstore_expired_deleted_total";

/// OpenTelemetry-backed metrics handle for the credstore module.
pub struct CredStoreMetricsMeter {
    read_outcome: Counter<u64>,
    walkup_depth: Histogram<u64>,
    dependency_query_duration: Histogram<f64>,
    dependency_health: Counter<u64>,
    cross_tenant_denied: Counter<u64>,
    fence_verify: Counter<u64>,
    gc_deleted: Counter<u64>,
    gc_pending_reclaimed: Counter<u64>,
    expired_deleted: Counter<u64>,
}

impl std::fmt::Debug for CredStoreMetricsMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredStoreMetricsMeter")
            .finish_non_exhaustive()
    }
}

impl CredStoreMetricsMeter {
    /// Build the instrument set from the supplied meter.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            read_outcome: meter
                .u64_counter(CREDSTORE_READ_OUTCOME)
                .with_description("Secret read results by outcome")
                .build(),
            walkup_depth: meter
                .u64_histogram(CREDSTORE_WALKUP_DEPTH)
                .with_description("Tenant walk-up depth when resolving inherited secrets")
                .build(),
            dependency_query_duration: meter
                .f64_histogram(CREDSTORE_DEPENDENCY_QUERY_DURATION)
                .with_description("Upstream dependency query latency, by dependency + operation")
                .with_boundaries(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25])
                .build(),
            dependency_health: meter
                .u64_counter(CREDSTORE_DEPENDENCY_HEALTH)
                .with_description(
                    "Upstream dependency call outcomes, by dependency + operation + outcome",
                )
                .build(),
            cross_tenant_denied: meter
                .u64_counter(CREDSTORE_CROSS_TENANT_DENIED)
                .with_description("Cross-tenant secret access attempts that were denied")
                .build(),
            fence_verify: meter
                .u64_counter(CREDSTORE_FENCE_VERIFY)
                .with_description(
                    "Value-fingerprint fence verdicts on reads, by outcome \
                     (mismatch = fail-closed 404, the alertable signal)",
                )
                .build(),
            gc_deleted: meter
                .u64_counter(CREDSTORE_GC_DELETED)
                .with_description(
                    "Maintenance job: backend versions deleted by the gc drain \
                     (superseded/removed/aborted)",
                )
                .build(),
            gc_pending_reclaimed: meter
                .u64_counter(CREDSTORE_GC_PENDING_RECLAIMED)
                .with_description(
                    "Maintenance job: orphaned pending versions reclaimed (a sustained climb \
                     means writes are crashing or timing out before commit)",
                )
                .build(),
            expired_deleted: meter
                .u64_counter(CREDSTORE_EXPIRED_DELETED)
                .with_description("Maintenance job: expired active rows removed")
                .build(),
        }
    }

    /// Build a handle bound to the process-global meter provider.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter(METER_NAME))
    }
}

impl CredStoreMetricsPort for CredStoreMetricsMeter {
    fn read_outcome(&self, outcome: ReadOutcome) {
        self.read_outcome
            .add(1, &[KeyValue::new("outcome", outcome.as_str())]);
    }

    fn walkup_depth(&self, depth: u64) {
        self.walkup_depth.record(depth, &[]);
    }

    fn dependency(&self, dep: Dep, op: DepOp, outcome: Outcome, secs: f64) {
        self.dependency_query_duration.record(
            secs,
            &[
                KeyValue::new("dependency", dep.as_str()),
                KeyValue::new("operation", op.as_str()),
            ],
        );
        self.dependency_health.add(
            1,
            &[
                KeyValue::new("dependency", dep.as_str()),
                KeyValue::new("operation", op.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }

    fn cross_tenant_denied(&self) {
        self.cross_tenant_denied.add(1, &[]);
    }

    fn fence_verify(&self, outcome: FenceVerify) {
        self.fence_verify
            .add(1, &[KeyValue::new("outcome", outcome.as_str())]);
    }

    fn gc_deleted(&self, n: u64) {
        self.gc_deleted.add(n, &[]);
    }

    fn gc_pending_reclaimed(&self, n: u64) {
        self.gc_pending_reclaimed.add(n, &[]);
    }

    fn expired_deleted(&self, n: u64) {
        self.expired_deleted.add(n, &[]);
    }
}

#[cfg(feature = "test-support")]
pub mod test_harness {
    //! In-memory OpenTelemetry harness for asserting emitted credstore metrics.
    #![allow(clippy::expect_used, clippy::missing_panics_doc, dead_code)]

    use opentelemetry::metrics::{Meter, MeterProvider};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::{CredStoreMetricsMeter, METER_NAME};

    /// In-memory meter provider + exporter for unit and integration tests.
    pub struct MetricsHarness {
        provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
    }

    impl MetricsHarness {
        #[must_use]
        pub fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .build();
            Self { provider, exporter }
        }

        #[must_use]
        pub fn meter(&self) -> Meter {
            self.provider.meter(METER_NAME)
        }

        /// A metrics handle bound to this harness's provider.
        #[must_use]
        pub fn metrics(&self) -> CredStoreMetricsMeter {
            CredStoreMetricsMeter::new(&self.meter())
        }

        /// Flush aggregated data into the in-memory exporter.
        pub fn force_flush(&self) {
            self.provider
                .force_flush()
                .expect("test meter provider should flush");
        }

        /// Sum all matching `u64` counter data points.
        #[must_use]
        pub fn counter_value(&self, name: &str, expected_attrs: &[(&str, &str)]) -> u64 {
            let metrics = self
                .exporter
                .get_finished_metrics()
                .expect("in-memory exporter should be readable");
            let mut total = 0u64;
            for rm in &metrics {
                for sm in rm.scope_metrics() {
                    for metric in sm.metrics() {
                        if metric.name() == name
                            && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                        {
                            for dp in sum.data_points() {
                                if attributes_match(dp.attributes(), expected_attrs) {
                                    total += dp.value();
                                }
                            }
                        }
                    }
                }
            }
            total
        }

        /// Sum matching histogram sample counts.
        #[must_use]
        pub fn histogram_count(&self, name: &str, expected_attrs: &[(&str, &str)]) -> u64 {
            let metrics = self
                .exporter
                .get_finished_metrics()
                .expect("in-memory exporter should be readable");
            let mut total = 0u64;
            for rm in &metrics {
                for sm in rm.scope_metrics() {
                    for metric in sm.metrics() {
                        if metric.name() == name
                            && let AggregatedMetrics::F64(MetricData::Histogram(hist)) =
                                metric.data()
                        {
                            for dp in hist.data_points() {
                                if attributes_match(dp.attributes(), expected_attrs) {
                                    total += dp.count();
                                }
                            }
                        }
                    }
                }
            }
            total
        }
    }

    impl Default for MetricsHarness {
        fn default() -> Self {
            Self::new()
        }
    }

    fn attributes_match<'a>(
        actual_attrs: impl Iterator<Item = &'a opentelemetry::KeyValue>,
        expected: &[(&str, &str)],
    ) -> bool {
        let actual = actual_attrs.collect::<Vec<_>>();
        expected.iter().all(|(k, v)| {
            actual
                .iter()
                .any(|kv| kv.key.as_str() == *k && kv.value.as_str() == *v)
        }) && actual.len() == expected.len()
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
