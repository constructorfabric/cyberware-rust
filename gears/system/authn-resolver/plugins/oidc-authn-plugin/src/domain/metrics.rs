//! Observability metrics for the Oidc `AuthN` plugin.
//!
//! The host installs the global meter provider during startup. This gear owns
//! the typed OpenTelemetry instruments used across the plugin and exposes a
//! small, domain-oriented recording API to the rest of the codebase.

mod definitions;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use toolkit_macros::domain_model;

pub(crate) use definitions::{
    AUTHN_CIRCUIT_BREAKER_CLOSED_TOTAL, AUTHN_CIRCUIT_BREAKER_STATE, AUTHN_ERRORS_TOTAL,
    AUTHN_FIRST_PARTY_RATIO, AUTHN_IDP_UP, AUTHN_JWKS_CACHE_ENTRIES, AUTHN_JWKS_CACHE_HITS_TOTAL,
    AUTHN_JWKS_CACHE_MISSES_TOTAL, AUTHN_JWKS_FETCH_DURATION_SECONDS,
    AUTHN_JWKS_REFRESH_FAILURES_TOTAL, AUTHN_JWT_VALIDATION_DURATION_SECONDS,
    AUTHN_REQUEST_FAILURES_TOTAL, AUTHN_REQUEST_SUCCESS_DURATION_SECONDS,
    AUTHN_S2S_EXCHANGE_DURATION_SECONDS, AUTHN_S2S_EXCHANGE_ERRORS_TOTAL, AUTHN_S2S_EXCHANGE_TOTAL,
    AUTHN_TOKEN_REJECTED_TOTAL, TOKEN_REJECTION_REASON_EXPIRED,
    TOKEN_REJECTION_REASON_INVALID_AUDIENCE, TOKEN_REJECTION_REASON_INVALID_IAT,
    TOKEN_REJECTION_REASON_INVALID_SIG, TOKEN_REJECTION_REASON_INVALID_TENANT,
    TOKEN_REJECTION_REASON_INVALID_TYP, TOKEN_REJECTION_REASON_MISSING_AUDIENCE,
    TOKEN_REJECTION_REASON_MISSING_TENANT, TOKEN_REJECTION_REASON_UNTRUSTED_ISSUER,
};

/// Seconds-scale bucket boundaries for the authn duration histograms;
/// the SDK default boundaries are millisecond-scale.
const AUTHN_DURATION_BOUNDARIES_SECS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

/// OpenTelemetry-backed metrics handle shared across plugin components.
#[domain_model]
pub struct AuthNMetrics {
    jwks_cache_hits_total: Counter<u64>,
    jwks_cache_misses_total: Counter<u64>,
    jwks_cache_entries: Gauge<f64>,
    jwt_validation_duration_seconds: Histogram<f64>,
    jwks_fetch_duration_seconds: Histogram<f64>,
    request_success_duration_seconds: Histogram<f64>,
    request_failures_total: Counter<u64>,
    errors_total: Counter<u64>,
    circuit_breaker_state: Gauge<f64>,
    circuit_breaker_closed_total: Counter<u64>,
    idp_up: Gauge<f64>,
    jwks_refresh_failures_total: Counter<u64>,
    token_rejected_total: Counter<u64>,
    first_party_ratio: Gauge<f64>,
    s2s_exchange_total: Counter<u64>,
    s2s_exchange_errors_total: Counter<u64>,
    s2s_exchange_duration_seconds: Histogram<f64>,
    first_party_success_count: AtomicU64,
    successful_auth_count: AtomicU64,
}

impl std::fmt::Debug for AuthNMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthNMetrics")
            .field(
                "first_party_success_count",
                &self.first_party_success_count.load(Ordering::Relaxed),
            )
            .field(
                "successful_auth_count",
                &self.successful_auth_count.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl AuthNMetrics {
    /// Create the full set of plugin instruments from the provided meter.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new(meter: &Meter) -> Self {
        let metrics = Self {
            jwks_cache_hits_total: meter
                .u64_counter(AUTHN_JWKS_CACHE_HITS_TOTAL)
                .with_description("JWKS cache hits")
                .with_unit("{hit}")
                .build(),
            jwks_cache_misses_total: meter
                .u64_counter(AUTHN_JWKS_CACHE_MISSES_TOTAL)
                .with_description("JWKS cache misses")
                .with_unit("{miss}")
                .build(),
            jwks_cache_entries: meter
                .f64_gauge(AUTHN_JWKS_CACHE_ENTRIES)
                .with_description("Number of entries in JWKS cache")
                .with_unit("{entry}")
                .build(),
            jwt_validation_duration_seconds: meter
                .f64_histogram(AUTHN_JWT_VALIDATION_DURATION_SECONDS)
                .with_description("JWT local validation duration")
                .with_unit("s")
                .with_boundaries(
                    // Explicit JWT validation histogram buckets tuned for sub-10ms latency.
                    vec![0.001, 0.002, 0.005, 0.01, 0.025, 0.05, 0.1],
                )
                .build(),
            jwks_fetch_duration_seconds: meter
                .f64_histogram(AUTHN_JWKS_FETCH_DURATION_SECONDS)
                .with_description("JWKS network fetch duration")
                .with_unit("s")
                .with_boundaries(AUTHN_DURATION_BOUNDARIES_SECS.to_vec())
                .build(),
            request_success_duration_seconds: meter
                .f64_histogram(AUTHN_REQUEST_SUCCESS_DURATION_SECONDS)
                .with_description("Successful authentication request duration")
                .with_unit("s")
                .with_boundaries(AUTHN_DURATION_BOUNDARIES_SECS.to_vec())
                .build(),
            request_failures_total: meter
                .u64_counter(AUTHN_REQUEST_FAILURES_TOTAL)
                .with_description("Failed authentication requests by reason")
                .with_unit("{request}")
                .build(),
            errors_total: meter
                .u64_counter(AUTHN_ERRORS_TOTAL)
                .with_description("Resolver errors by error type")
                .with_unit("{error}")
                .build(),
            circuit_breaker_state: meter
                .f64_gauge(AUTHN_CIRCUIT_BREAKER_STATE)
                .with_description("Circuit breaker state (0=closed,1=half-open,2=open)")
                .with_unit("1")
                .build(),
            circuit_breaker_closed_total: meter
                .u64_counter(AUTHN_CIRCUIT_BREAKER_CLOSED_TOTAL)
                .with_description("Circuit breaker close transitions by host")
                .with_unit("{transition}")
                .build(),
            idp_up: meter
                .f64_gauge(AUTHN_IDP_UP)
                .with_description("IdP availability probe (0/1)")
                .with_unit("1")
                .build(),
            jwks_refresh_failures_total: meter
                .u64_counter(AUTHN_JWKS_REFRESH_FAILURES_TOTAL)
                .with_description("Failed forced JWKS refresh attempts")
                .with_unit("{failure}")
                .build(),
            token_rejected_total: meter
                .u64_counter(AUTHN_TOKEN_REJECTED_TOTAL)
                .with_description("Rejected tokens grouped by reason")
                .with_unit("{rejection}")
                .build(),
            first_party_ratio: meter
                .f64_gauge(AUTHN_FIRST_PARTY_RATIO)
                .with_description("First-party authentication ratio")
                .with_unit("1")
                .build(),
            s2s_exchange_total: meter
                .u64_counter(AUTHN_S2S_EXCHANGE_TOTAL)
                .with_description("Total S2S client credentials exchange attempts")
                .with_unit("{exchange}")
                .build(),
            s2s_exchange_errors_total: meter
                .u64_counter(AUTHN_S2S_EXCHANGE_ERRORS_TOTAL)
                .with_description("S2S exchange errors by error type")
                .with_unit("{error}")
                .build(),
            s2s_exchange_duration_seconds: meter
                .f64_histogram(AUTHN_S2S_EXCHANGE_DURATION_SECONDS)
                .with_description("S2S client credentials exchange duration")
                .with_unit("s")
                .with_boundaries(
                    // Histogram buckets for S2S token exchange (network call: 50-500ms typical).
                    vec![0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0],
                )
                .build(),
            first_party_success_count: AtomicU64::new(0),
            successful_auth_count: AtomicU64::new(0),
        };

        // Emit initial state for gauges that are not scoped to a runtime key.
        // Host-scoped breaker gauges are emitted when each host breaker is created.
        metrics.record_jwks_cache_entries(0);
        metrics.first_party_ratio.record(0.0, &[]);

        metrics
    }

    /// Increment JWKS cache hit counter.
    pub fn increment_jwks_cache_hit(&self) {
        self.jwks_cache_hits_total.add(1, &[]);
    }

    /// Increment JWKS cache miss counter.
    pub fn increment_jwks_cache_miss(&self) {
        self.jwks_cache_misses_total.add(1, &[]);
    }

    /// Record a JWKS cache entry-count gauge.
    #[allow(clippy::cast_precision_loss)]
    pub fn record_jwks_cache_entries(&self, entries: usize) {
        self.jwks_cache_entries.record(entries as f64, &[]);
    }

    /// Record JWT local validation duration.
    pub fn record_jwt_validation_duration(&self, duration: Duration) {
        self.jwt_validation_duration_seconds
            .record(duration.as_secs_f64(), &[]);
    }

    /// Record JWKS remote fetch duration.
    pub fn record_jwks_fetch_duration(&self, duration: Duration) {
        self.jwks_fetch_duration_seconds
            .record(duration.as_secs_f64(), &[]);
    }

    /// Record successful authentication request duration.
    pub fn record_request_success_duration(&self, duration: Duration) {
        self.request_success_duration_seconds
            .record(duration.as_secs_f64(), &[]);
    }

    /// Increment failed authentication request counter for a specific reason label.
    pub fn increment_request_failure(&self, reason: &'static str) {
        self.request_failures_total
            .add(1, &[KeyValue::new("reason", reason)]);
    }

    /// Increment resolver error counter by error type.
    pub fn increment_error(&self, error_type: &'static str) {
        self.errors_total
            .add(1, &[KeyValue::new("type", error_type)]);
    }

    /// Set circuit-breaker state gauge value for a host.
    pub fn set_circuit_breaker_state(&self, host: &str, state: f64) {
        self.circuit_breaker_state
            .record(state, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Increment circuit-breaker close transition counter for a host.
    pub fn increment_circuit_breaker_closed(&self, host: &str) {
        self.circuit_breaker_closed_total
            .add(1, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Set identity-provider availability probe gauge value for a host.
    pub fn set_idp_up(&self, host: &str, value: f64) {
        self.idp_up
            .record(value, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Increment failed forced JWKS refresh attempts.
    pub fn increment_jwks_refresh_failures(&self) {
        self.jwks_refresh_failures_total.add(1, &[]);
    }

    /// Increment token rejection counter for a specific reason label.
    pub fn increment_token_rejected(&self, reason: &'static str) {
        self.token_rejected_total
            .add(1, &[KeyValue::new("reason", reason)]);
    }

    /// Increment S2S exchange attempt counter.
    pub fn increment_s2s_exchange(&self) {
        self.s2s_exchange_total.add(1, &[]);
    }

    /// Increment S2S exchange error counter with an error type label.
    pub fn increment_s2s_exchange_error(&self, error_type: &'static str) {
        self.s2s_exchange_errors_total
            .add(1, &[KeyValue::new("type", error_type)]);
    }

    /// Record S2S exchange duration.
    pub fn record_s2s_exchange_duration(&self, duration: Duration) {
        self.s2s_exchange_duration_seconds
            .record(duration.as_secs_f64(), &[]);
    }

    /// Update first-party ratio gauge using a running success ratio.
    pub fn observe_first_party_auth(&self, is_first_party: bool) {
        let total = self.successful_auth_count.fetch_add(1, Ordering::AcqRel) + 1;
        let first_party = if is_first_party {
            self.first_party_success_count
                .fetch_add(1, Ordering::AcqRel)
                + 1
        } else {
            self.first_party_success_count.load(Ordering::Acquire)
        };

        #[allow(clippy::cast_precision_loss)]
        let ratio = (first_party as f64 / total as f64).clamp(0.0, 1.0);
        self.first_party_ratio.record(ratio, &[]);
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
pub mod test_harness;
