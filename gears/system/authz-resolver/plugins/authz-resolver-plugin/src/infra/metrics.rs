//! OpenTelemetry metrics for the `AuthZ` resolver plugin (design §3.13).
//!
//! The host installs the global meter provider at startup; this module pulls
//! instruments from it. When no exporter is configured the provider is a
//! no-op, so emitting is always cheap and safe.
//!
//! Instruments use full, literal Prometheus names: counters end in `_total`
//! and duration histograms in `_milliseconds`, with the suffix baked into the
//! instrument name (no `.with_unit()`); the cache-hit-ratio gauge carries no
//! suffix. This matches the platform's `add_metric_suffixes: false` collector
//! posture (like RMS / policy-engine / openbao), so the exporter renders the
//! names verbatim.
//!
//! Covers the §3.13 instrument set:
//! - orchestration-level (emitted from the `evaluate()` wrapper): end-to-end
//!   latency, deny / error / fail-closed counts, scope-type distribution;
//! - per-component (threaded into the relevant code): cache hit ratio +
//!   hierarchy query duration (HierarchyCache/Client), RBAC query duration,
//!   constraint-compilation duration, and the security/versatility counters
//!   (`unsupported_property`, `token_scope_narrowing`, `barrier_mode_override`,
//!   `capability_negotiation`).
//!
//! **Not implemented:** `authz_audit_event_delivery_lag_seconds` — audit is
//! log-only in v1 (no event transport), so there is no delivery lag to
//! measure. It will be added alongside audit event emission.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use authz_resolver_sdk::EvaluationResponse;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use tracing::debug;

use crate::domain::deny::error_codes;
use crate::domain::error::PluginError;
use crate::domain::metrics_port::{
    CacheKind, Decision, DenyReason, FailClosedReason, HierarchyOp, NarrowingOp, RbacOp, Resolver,
    ScopeTypeLabel,
};

/// Number of distinct [`CacheKind`] variants. The atomic hit/total packing
/// below allocates one counter per kind; [`cache_idx`] maps a kind to its slot.
const CACHE_KINDS: usize = 4;

/// Map a domain [`CacheKind`] to its slot in the per-kind atomic counters.
/// Kept infra-side (with the atomic packing it serves) so the domain port
/// stays free of storage concerns.
const fn cache_idx(kind: CacheKind) -> usize {
    match kind {
        CacheKind::TenantSubtree => 0,
        CacheKind::TenantMeta => 1,
        CacheKind::GroupSubtree => 2,
        CacheKind::GroupMembers => 3,
    }
}

/// Convert a `Duration` to milliseconds (`f64`), preserving sub-millisecond
/// precision. The duration histograms are named `_milliseconds` and record in
/// milliseconds, so every measured `Duration` is funneled through this.
#[inline]
fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Meter / instrumentation scope name (matches the toolkit module name).
pub(crate) const METER_NAME: &str = "authz-resolver-plugin";

// ─── Metric names (literal Prometheus form; `add_metric_suffixes: false`) ─────
// Full names with suffixes baked in: counters end `_total`, duration
// histograms `_milliseconds`, the cache-hit-ratio gauge carries no suffix. No
// `.with_unit()` — the collector renders these names verbatim, matching the
// platform's other modules (RMS / policy-engine / openbao).
/// Histogram: end-to-end `evaluate()` latency in milliseconds, by `decision`.
pub(crate) const AUTHZ_EVALUATION_DURATION: &str = "authz_evaluation_duration_milliseconds";
/// Counter: business denials by `reason`.
pub(crate) const AUTHZ_EVALUATION_DENY: &str = "authz_evaluation_deny_total";
/// Counter: system errors (excluding business denials) by `error_type`.
pub(crate) const AUTHZ_EVALUATION_ERROR: &str = "authz_evaluation_error_total";
/// Counter: fail-closed denials caused by system issues, by `reason`.
pub(crate) const AUTHZ_FAIL_CLOSED: &str = "authz_fail_closed_total";
/// Counter: allowed evaluations by RBAC `scope_type`.
pub(crate) const AUTHZ_EVALUATION_BY_SCOPE_TYPE: &str = "authz_evaluation_by_scope_type_total";
/// Gauge: hierarchy cache hit ratio in `[0.0, 1.0]` by `cache_type` (no suffix).
pub(crate) const AUTHZ_EVALUATION_CACHE_HIT_RATIO: &str = "authz_evaluation_cache_hit_ratio";
/// Histogram: hierarchy resolver query latency in milliseconds, by `resolver` + `operation`.
pub(crate) const AUTHZ_HIERARCHY_QUERY_DURATION: &str =
    "authz_hierarchy_query_duration_milliseconds";
/// Histogram: in-process RBAC service call latency in milliseconds, by `operation`.
pub(crate) const AUTHZ_RBAC_QUERY_DURATION: &str = "authz_rbac_query_duration_milliseconds";
/// Histogram: constraint-compilation latency in milliseconds, by `scope_type`.
pub(crate) const AUTHZ_CONSTRAINT_COMPILATION_DURATION: &str =
    "authz_constraint_compilation_duration_milliseconds";
/// Counter: unsupported-property denials, by `resource_type`.
pub(crate) const AUTHZ_UNSUPPORTED_PROPERTY: &str = "authz_unsupported_property_total";
/// Counter: RBAC allows rejected because their aggregate scope does not match
/// the contributing role-assignment scopes. Dimensionless to keep cardinality bounded.
pub(crate) const AUTHZ_SCOPE_PROVENANCE_REJECTION: &str = "authz_scope_provenance_rejection_total";
/// Counter: token-scope narrowing events, by `operation`.
pub(crate) const AUTHZ_TOKEN_SCOPE_NARROWING: &str = "authz_token_scope_narrowing_total";
/// Counter: cross-barrier (`barrier_mode=Ignore`) requests, by `resource_family`.
pub(crate) const AUTHZ_BARRIER_MODE_OVERRIDE: &str = "authz_barrier_mode_override_total";
/// Counter: capability-negotiation distribution, by declared `capabilities`.
pub(crate) const AUTHZ_CAPABILITY_NEGOTIATION: &str = "authz_capability_negotiation_total";

/// OpenTelemetry-backed metrics handle, shared across plugin components.
pub struct AuthZMetrics {
    evaluation_duration_milliseconds: Histogram<f64>,
    evaluation_deny_total: Counter<u64>,
    evaluation_error_total: Counter<u64>,
    fail_closed_total: Counter<u64>,
    evaluation_by_scope_type_total: Counter<u64>,
    cache_hit_ratio: Gauge<f64>,
    hierarchy_query_duration_milliseconds: Histogram<f64>,
    rbac_query_duration_milliseconds: Histogram<f64>,
    constraint_compilation_duration_milliseconds: Histogram<f64>,
    unsupported_property_total: Counter<u64>,
    scope_provenance_rejection_total: Counter<u64>,
    token_scope_narrowing_total: Counter<u64>,
    barrier_mode_override_total: Counter<u64>,
    capability_negotiation_total: Counter<u64>,
    // Cumulative hits+total per cache kind, packed into one atomic
    // (hits in the high 32 bits, total in the low 32) so a single
    // `fetch_add` updates both together — readers never observe an
    // inconsistent (hits, total) pair. Recomputed into the gauge per access.
    cache_counts: [AtomicU64; CACHE_KINDS],
}

impl std::fmt::Debug for AuthZMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // OTel instruments are opaque handles — nothing useful to print.
        f.debug_struct("AuthZMetrics").finish_non_exhaustive()
    }
}

impl AuthZMetrics {
    /// Build the v1 instrument set from the supplied meter.
    pub(crate) fn new(meter: &Meter) -> Self {
        Self {
            evaluation_duration_milliseconds: meter
                .f64_histogram(AUTHZ_EVALUATION_DURATION)
                .with_description("End-to-end evaluate() latency in milliseconds, by decision")
                // Buckets tuned for the p95 ≤ 20ms / p99 ≤ 50ms latency NFR.
                .with_boundaries(vec![1.0, 5.0, 10.0, 20.0, 50.0, 100.0, 250.0])
                .build(),
            evaluation_deny_total: meter
                .u64_counter(AUTHZ_EVALUATION_DENY)
                .with_description("Business denials by reason")
                .build(),
            evaluation_error_total: meter
                .u64_counter(AUTHZ_EVALUATION_ERROR)
                .with_description("System errors (excluding business denials) by error type")
                .build(),
            fail_closed_total: meter
                .u64_counter(AUTHZ_FAIL_CLOSED)
                .with_description("Fail-closed denials caused by system issues, by reason")
                .build(),
            evaluation_by_scope_type_total: meter
                .u64_counter(AUTHZ_EVALUATION_BY_SCOPE_TYPE)
                .with_description("Allowed evaluations by RBAC scope type")
                .build(),
            cache_hit_ratio: meter
                .f64_gauge(AUTHZ_EVALUATION_CACHE_HIT_RATIO)
                .with_description("Hierarchy cache hit ratio (0.0-1.0) by cache type")
                .build(),
            hierarchy_query_duration_milliseconds: meter
                .f64_histogram(AUTHZ_HIERARCHY_QUERY_DURATION)
                .with_description("Hierarchy resolver query latency in milliseconds")
                .with_boundaries(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0])
                .build(),
            rbac_query_duration_milliseconds: meter
                .f64_histogram(AUTHZ_RBAC_QUERY_DURATION)
                .with_description("In-process RBAC service call latency in milliseconds")
                .with_boundaries(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0])
                .build(),
            constraint_compilation_duration_milliseconds: meter
                .f64_histogram(AUTHZ_CONSTRAINT_COMPILATION_DURATION)
                .with_description("Constraint compilation latency in milliseconds, by scope type")
                .with_boundaries(vec![0.1, 0.5, 1.0, 5.0, 10.0])
                .build(),
            unsupported_property_total: meter
                .u64_counter(AUTHZ_UNSUPPORTED_PROPERTY)
                .with_description(
                    "Unsupported-property denials (PEP config issue) by resource type",
                )
                .build(),
            scope_provenance_rejection_total: meter
                .u64_counter(AUTHZ_SCOPE_PROVENANCE_REJECTION)
                .with_description(
                    "RBAC allows rejected because aggregate scope disagreed with assignment provenance",
                )
                .build(),
            token_scope_narrowing_total: meter
                .u64_counter(AUTHZ_TOKEN_SCOPE_NARROWING)
                .with_description("Token-scope narrowing events by operation")
                .build(),
            barrier_mode_override_total: meter
                .u64_counter(AUTHZ_BARRIER_MODE_OVERRIDE)
                .with_description("Cross-barrier (barrier_mode=Ignore) requests by resource family")
                .build(),
            capability_negotiation_total: meter
                .u64_counter(AUTHZ_CAPABILITY_NEGOTIATION)
                .with_description("Capability-negotiation distribution by declared capabilities")
                .build(),
            cache_counts: Default::default(),
        }
    }

    /// Build a handle bound to the process-global meter provider. Used by the
    /// default `AuthZResolverPlugin::new`; the provider is a no-op until the
    /// host installs an exporter, so this is always safe.
    pub(crate) fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter(METER_NAME))
    }

    /// Record the outcome of one `evaluate()` call: latency always, plus the
    /// deny / error / fail-closed counters as appropriate. Called once per
    /// evaluation from the trait wrapper, so every return path is covered.
    pub(crate) fn record_outcome(
        &self,
        elapsed: Duration,
        result: &Result<EvaluationResponse, PluginError>,
    ) {
        let decision = match result {
            Ok(resp) if resp.decision => Decision::Allow,
            Ok(_) => Decision::Deny,
            Err(_) => Decision::Error,
        };
        self.evaluation_duration_milliseconds.record(
            millis(elapsed),
            &[KeyValue::new("decision", decision.as_str())],
        );

        match result {
            // Business deny.
            Ok(resp) if !resp.decision => {
                let reason = deny_reason_label(resp);
                self.evaluation_deny_total
                    .add(1, &[KeyValue::new("reason", reason.as_str())]);
                // `constraints_unavailable` is a contract-violation deny that
                // §3.13 counts as fail-closed (not a normal business deny).
                if matches!(reason, DenyReason::ConstraintsUnavailable) {
                    self.fail_closed_total.add(
                        1,
                        &[KeyValue::new(
                            "reason",
                            FailClosedReason::ConstraintsUnavailable.as_str(),
                        )],
                    );
                }
            }
            // System error.
            Err(err) => {
                let (error_type, fail_closed_reason) = err.labels();
                self.evaluation_error_total
                    .add(1, &[KeyValue::new("error_type", error_type.as_str())]);
                if let Some(reason) = fail_closed_reason {
                    self.fail_closed_total
                        .add(1, &[KeyValue::new("reason", reason.as_str())]);
                }
            }
            // Allow — no deny/error counter.
            Ok(_) => {}
        }
    }

    /// Record the RBAC scope type of an allowed evaluation. The caller maps
    /// the `PermissionScopeType` to a stable label (kept in `evaluate.rs` so
    /// this module stays decoupled from `rbac-sdk`).
    pub(crate) fn record_scope_type(&self, scope_type: ScopeTypeLabel) {
        self.evaluation_by_scope_type_total
            .add(1, &[KeyValue::new("scope_type", scope_type.as_str())]);
    }

    /// Record a hierarchy cache access and refresh the hit-ratio gauge for
    /// the given cache kind. hits+total are packed into one atomic and bumped
    /// with a single `fetch_add`, so the (hits, total) pair read back is always
    /// consistent (no `hits > total` interleaving across separate atomics).
    pub(crate) fn record_cache_access(&self, kind: CacheKind, hit: bool) {
        // High 32 bits = hits, low 32 bits = total. A hit bumps both; a miss
        // bumps only total.
        let delta: u64 = if hit { (1 << 32) | 1 } else { 1 };
        let packed = self.cache_counts[cache_idx(kind)].fetch_add(delta, Ordering::Relaxed) + delta;
        let hits = packed >> 32;
        let total = packed & 0xFFFF_FFFF;
        // Keep `total` from wrapping the low 32 bits (which would carry into
        // `hits` and permanently corrupt the ratio): once it crosses 2^31,
        // halve both. The ratio is preserved and headroom restored. `fetch_update`
        // (a CAS retry loop) halves the *current* value, so increments made by
        // other threads between our `fetch_add` and here are not lost. The
        // closure re-checks the threshold against the *current* value so racing
        // threads that observed the same trip don't each halve in turn (which
        // would shrink the counter by 2^N instead of 2).
        if total >= (1 << 31) {
            _ = self.cache_counts[cache_idx(kind)].fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |cur| {
                    if (cur & 0xFFFF_FFFF) < (1 << 31) {
                        // Another thread already halved; skip the CAS.
                        None
                    } else {
                        Some(((cur >> 33) << 32) | ((cur & 0xFFFF_FFFF) >> 1))
                    }
                },
            );
        }
        // The ratio uses the values we observed at `fetch_add`; halving above
        // only restores counter headroom and never changes hits/total.
        #[allow(clippy::cast_precision_loss)]
        let ratio = (hits as f64 / total as f64).clamp(0.0, 1.0);
        self.cache_hit_ratio
            .record(ratio, &[KeyValue::new("cache_type", kind.as_str())]);
    }

    /// Record a hierarchy resolver query duration (`resolver` ∈ tenant/rg).
    pub(crate) fn record_hierarchy_query(
        &self,
        resolver: Resolver,
        operation: HierarchyOp,
        duration: Duration,
    ) {
        self.hierarchy_query_duration_milliseconds.record(
            millis(duration),
            &[
                KeyValue::new("resolver", resolver.as_str()),
                KeyValue::new("operation", operation.as_str()),
            ],
        );
    }

    /// Record an in-process RBAC service call duration.
    pub(crate) fn record_rbac_query(&self, operation: RbacOp, duration: Duration) {
        self.rbac_query_duration_milliseconds.record(
            millis(duration),
            &[KeyValue::new("operation", operation.as_str())],
        );
    }

    /// Record constraint-compilation duration for the given scope type.
    pub(crate) fn record_constraint_compilation(
        &self,
        scope_type: ScopeTypeLabel,
        duration: Duration,
    ) {
        self.constraint_compilation_duration_milliseconds.record(
            millis(duration),
            &[KeyValue::new("scope_type", scope_type.as_str())],
        );
    }

    /// Increment the unsupported-property deny counter (PEP config issue).
    /// Dimensionless: the resource type is request-controlled, so using it as a
    /// label would risk unbounded metric cardinality.
    pub(crate) fn inc_unsupported_property(&self) {
        self.unsupported_property_total.add(1, &[]);
    }

    /// Count a fail-closed RBAC allow whose aggregate scope cannot be proven
    /// from its contributing assignments. No labels are attached: scope values
    /// and assignment identifiers are unnecessary and would increase either
    /// cardinality or permission-state exposure.
    pub(crate) fn inc_scope_provenance_rejection(&self) {
        self.scope_provenance_rejection_total.add(1, &[]);
    }

    /// Increment the token-scope narrowing counter for a third-party request.
    /// `operation` MUST be a bounded label (a known operation class or
    /// `"other"`) — never a raw request string — to keep cardinality bounded.
    pub(crate) fn inc_token_scope_narrowing(&self, operation: NarrowingOp) {
        self.token_scope_narrowing_total
            .add(1, &[KeyValue::new("operation", operation.as_str())]);
    }

    /// Increment the cross-barrier (`barrier_mode=Ignore`) counter.
    /// Dimensionless: a `resource_family` derived from the request would be
    /// unbounded; the count alone is the audit-trail signal.
    pub(crate) fn inc_barrier_mode_override(&self) {
        self.barrier_mode_override_total.add(1, &[]);
    }

    /// Record the PEP's declared capability set (sorted, comma-joined).
    pub(crate) fn inc_capability_negotiation(&self, capabilities: &str) {
        self.capability_negotiation_total
            .add(1, &[KeyValue::new("capabilities", capabilities.to_owned())]);
    }
}

/// Map a deny response's GTS error code to a §3.13 `reason` label.
fn deny_reason_label(resp: &EvaluationResponse) -> DenyReason {
    let Some(code) = resp
        .context
        .deny_reason
        .as_ref()
        .map(|d| d.error_code.as_str())
    else {
        return DenyReason::Unknown;
    };
    match code {
        error_codes::INSUFFICIENT_PERMISSIONS_V1 => DenyReason::NoPermission,
        error_codes::SCOPE_MISMATCH_V1 => DenyReason::ScopeMismatch,
        error_codes::UNKNOWN_RESOURCE_TYPE_V1 => DenyReason::UnknownResourceType,
        error_codes::UNSUPPORTED_PROPERTY_V1 => DenyReason::UnsupportedProperty,
        error_codes::CONSTRAINTS_UNAVAILABLE_V1 => DenyReason::ConstraintsUnavailable,
        error_codes::EXPANSION_INFEASIBLE_V1 => DenyReason::ExpansionInfeasible,
        error_codes::INVALID_REQUEST_V1 => DenyReason::InvalidRequest,
        // A deny code with no mapped label is a programming gap (a new code was
        // added without updating this map). Bucket it as "unknown" for bounded
        // cardinality. Logged at `debug` (not `warn`) so a deploy that lands a
        // new code without updating this map doesn't flood the hot path — the
        // code is already observable on the audit record and on
        // `evaluation_deny{reason="unknown"}`.
        other => {
            debug!(
                deny_error_code = other,
                "deny code has no metrics reason label; bucketed as 'unknown'"
            );
            DenyReason::Unknown
        }
    }
}

// This module does not decide `error_type` / `fail_closed`. That classification
// lives on `domain::error::PluginError::labels`, as an exhaustive match the
// compiler checks, so a new failure mode cannot ship unclassified.

#[cfg(feature = "test-support")]
pub mod test_harness {
    //! In-memory OpenTelemetry harness for asserting emitted metrics in tests.
    //! Consumed by this crate's `cfg(test)` tests and by downstream crates via
    //! the `test-support` feature; `dead_code` is allowed because the plain
    //! library compilation can't see those test-only consumers.
    #![allow(clippy::expect_used, clippy::missing_panics_doc, dead_code)]

    use std::sync::Arc;

    use opentelemetry::metrics::{Meter, MeterProvider};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::{AuthZMetrics, METER_NAME};

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
        pub fn metrics(&self) -> Arc<AuthZMetrics> {
            Arc::new(AuthZMetrics::new(&self.meter()))
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

        /// Read the latest matching gauge value.
        #[must_use]
        pub fn gauge_value(&self, name: &str, expected_attrs: &[(&str, &str)]) -> Option<f64> {
            let metrics = self
                .exporter
                .get_finished_metrics()
                .expect("in-memory exporter should be readable");
            let mut latest = None;
            for rm in &metrics {
                for sm in rm.scope_metrics() {
                    for metric in sm.metrics() {
                        if metric.name() == name
                            && let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = metric.data()
                        {
                            for dp in gauge.data_points() {
                                if attributes_match(dp.attributes(), expected_attrs) {
                                    latest = Some(dp.value());
                                }
                            }
                        }
                    }
                }
            }
            latest
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
