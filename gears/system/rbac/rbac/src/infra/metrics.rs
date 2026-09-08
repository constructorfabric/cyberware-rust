//! OpenTelemetry adapter implementing [`PermissionMetricsPort`].
//!
//! Instruments are pulled from the process-global meter provider installed by
//! the host; a no-op until an exporter is wired, so emitting is always cheap
//! and safe. Instruments use full, literal Prometheus names: counters end in
//! `_total` and duration histograms in `_milliseconds`, with the suffix baked into
//! the instrument name (no `.with_unit()`). This matches the platform's
//! `add_metric_suffixes: false` collector posture, so the exporter renders the
//! names verbatim — consistent with RMS / policy-engine / openbao.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

use crate::domain::ports::metrics::{
    Dependency, DependencyOp, DependencyOutcome, EvalDenyReason, EvalErrorType, EvalResult,
    EvalScopeType, NameKind, NameOutcome, PermissionMetricsPort, PrincipalNameMetricsPort,
};

/// Meter / instrumentation scope name (matches the toolkit module name).
pub(crate) const METER_NAME: &str = "rbac";

// ─── Metric names (literal Prometheus form; `add_metric_suffixes: false`) ─────
// Full names with suffixes baked in: counters end `_total`, duration
// histograms `_milliseconds`. No `.with_unit()` — the collector renders verbatim.
const RBAC_PERMISSION_EVAL_DURATION: &str = "rbac_permission_eval_duration_milliseconds";
const RBAC_PERMISSION_DENY: &str = "rbac_permission_deny_total";
const RBAC_PERMISSION_EVAL_BY_SCOPE_TYPE: &str = "rbac_permission_eval_by_scope_type_total";
const RBAC_PERMISSION_EVAL_ERROR: &str = "rbac_permission_eval_error_total";
const RBAC_SUBJECT_ROLES_DURATION: &str = "rbac_subject_roles_duration_milliseconds";
const RBAC_DEPENDENCY_QUERY_DURATION: &str = "rbac_dependency_query_duration_milliseconds";
const RBAC_DEPENDENCY_HEALTH: &str = "rbac_dependency_health_total";
const RBAC_PRINCIPAL_NAME_RESOLVE: &str = "rbac_principal_name_resolve_total";
// Inventory gauges (bare counts → no unit suffix).
const RBAC_ROLE_DEFINITIONS: &str = "rbac_role_definitions";
const RBAC_ROLE_ASSIGNMENTS: &str = "rbac_role_assignments";

/// OpenTelemetry-backed metrics handle for the RBAC evaluator.
pub struct RbacMetricsMeter {
    permission_eval_duration: Histogram<f64>,
    permission_deny: Counter<u64>,
    permission_eval_by_scope_type: Counter<u64>,
    permission_eval_error: Counter<u64>,
    subject_roles_duration: Histogram<f64>,
    dependency_query_duration: Histogram<f64>,
    dependency_health: Counter<u64>,
    principal_name_resolve: Counter<u64>,
    role_definitions: Gauge<i64>,
    role_assignments: Gauge<i64>,
}

impl std::fmt::Debug for RbacMetricsMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RbacMetricsMeter").finish_non_exhaustive()
    }
}

impl RbacMetricsMeter {
    /// Build the instrument set from the supplied meter.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            permission_eval_duration: meter
                .f64_histogram(RBAC_PERMISSION_EVAL_DURATION)
                .with_description(
                    "End-to-end evaluate_permission() latency in milliseconds, by result",
                )
                .with_boundaries(vec![1.0, 5.0, 10.0, 20.0, 50.0, 100.0, 250.0])
                .build(),
            permission_deny: meter
                .u64_counter(RBAC_PERMISSION_DENY)
                .with_description("Permission denials by categorical reason")
                .build(),
            permission_eval_by_scope_type: meter
                .u64_counter(RBAC_PERMISSION_EVAL_BY_SCOPE_TYPE)
                .with_description("Allowed evaluations by aggregated scope type")
                .build(),
            permission_eval_error: meter
                .u64_counter(RBAC_PERMISSION_EVAL_ERROR)
                .with_description("Failed evaluations by error class")
                .build(),
            subject_roles_duration: meter
                .f64_histogram(RBAC_SUBJECT_ROLES_DURATION)
                .with_description(
                    "get_subject_roles() latency in milliseconds, by include_group_roles",
                )
                .with_boundaries(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0])
                .build(),
            dependency_query_duration: meter
                .f64_histogram(RBAC_DEPENDENCY_QUERY_DURATION)
                .with_description(
                    "Upstream dependency query latency in milliseconds, by dependency + operation",
                )
                .with_boundaries(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0])
                .build(),
            dependency_health: meter
                .u64_counter(RBAC_DEPENDENCY_HEALTH)
                .with_description(
                    "Upstream dependency call outcomes, by dependency + operation + outcome",
                )
                .build(),
            principal_name_resolve: meter
                .u64_counter(RBAC_PRINCIPAL_NAME_RESOLVE)
                .with_description(
                    "Role-assignment display-name resolutions by name kind and outcome",
                )
                .build(),
            role_definitions: meter
                .i64_gauge(RBAC_ROLE_DEFINITIONS)
                .with_description("Live count of role definitions")
                .build(),
            role_assignments: meter
                .i64_gauge(RBAC_ROLE_ASSIGNMENTS)
                .with_description("Live count of role assignments")
                .build(),
        }
    }

    /// Build a handle bound to the process-global meter provider.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter(METER_NAME))
    }

    /// Record the live RBAC inventory gauges (`rbac_role_definitions` /
    /// `rbac_role_assignments`). Driven by the periodic refresher spawned
    /// in module init — RBAC has no `serve()` loop of its own.
    pub fn record_inventory(&self, role_definitions: i64, role_assignments: i64) {
        self.role_definitions.record(role_definitions, &[]);
        self.role_assignments.record(role_assignments, &[]);
    }
}

impl PrincipalNameMetricsPort for RbacMetricsMeter {
    /// Count display-name resolution outcomes.
    ///
    /// Labels are categorical only — never a subject id, principal id,
    /// display name or tenant id. Names are precisely the data this
    /// feature exposes on an authenticated, authorized read path; leaking
    /// them (or the tenant they belong to) through an unauthenticated
    /// metrics scrape would be a wider disclosure than the feature itself.
    fn principal_name_resolve(&self, kind: NameKind, outcome: NameOutcome, count: u64) {
        // A page with no principals of this kind must not emit a
        // zero-valued sample: it would be indistinguishable from a real
        // resolution in a rate() query.
        if count == 0 {
            return;
        }
        self.principal_name_resolve.add(
            count,
            &[
                KeyValue::new("kind", kind.as_str()),
                KeyValue::new("outcome", outcome.as_str()),
            ],
        );
    }
}

impl PermissionMetricsPort for RbacMetricsMeter {
    fn permission_eval_duration(&self, result: EvalResult, secs: f64) {
        // Port carries seconds; the `_milliseconds` instrument records ms.
        self.permission_eval_duration
            .record(secs * 1000.0, &[KeyValue::new("result", result.as_str())]);
    }

    fn permission_deny(&self, reason: EvalDenyReason) {
        self.permission_deny
            .add(1, &[KeyValue::new("reason", reason.as_str())]);
    }

    fn permission_allow_scope_type(&self, scope_type: EvalScopeType) {
        self.permission_eval_by_scope_type
            .add(1, &[KeyValue::new("scope_type", scope_type.as_str())]);
    }

    fn permission_eval_error(&self, error: EvalErrorType) {
        self.permission_eval_error
            .add(1, &[KeyValue::new("error_type", error.as_str())]);
    }

    fn subject_roles_duration(&self, include_group_roles: bool, secs: f64) {
        let v = if include_group_roles { "true" } else { "false" };
        self.subject_roles_duration
            .record(secs * 1000.0, &[KeyValue::new("include_group_roles", v)]);
    }

    fn dependency_query(
        &self,
        dep: Dependency,
        op: DependencyOp,
        outcome: DependencyOutcome,
        secs: f64,
    ) {
        self.dependency_query_duration.record(
            secs * 1000.0,
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
}

#[cfg(feature = "test-support")]
pub mod test_harness {
    //! In-memory OpenTelemetry harness for asserting emitted RBAC metrics.
    #![allow(clippy::expect_used, clippy::missing_panics_doc, dead_code)]

    use opentelemetry::metrics::{Meter, MeterProvider};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::{METER_NAME, RbacMetricsMeter};

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
        pub fn metrics(&self) -> RbacMetricsMeter {
            RbacMetricsMeter::new(&self.meter())
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
