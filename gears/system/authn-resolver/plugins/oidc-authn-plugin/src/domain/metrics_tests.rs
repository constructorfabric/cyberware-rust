#![allow(clippy::expect_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use opentelemetry::metrics::{Meter, MeterProvider};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, Instrument, PeriodicReader, SdkMeterProvider, Stream,
};
use toolkit_macros::domain_model;

use super::{AuthNMetrics, TOKEN_REJECTION_REASON_EXPIRED};

/// In-memory OpenTelemetry provider + exporter for unit, integration, and bench tests.
#[domain_model]
pub struct MetricsHarness {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl MetricsHarness {
    /// Create a new harness backed by an in-memory exporter.
    #[must_use]
    pub fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(|_: &Instrument| Stream::builder().build().ok())
            .build();
        Self { provider, exporter }
    }

    /// Create a meter using the canonical plugin meter name.
    #[must_use]
    pub fn meter(&self) -> Meter {
        self.provider.meter(crate::OidcAuthNPluginGear::MODULE_NAME)
    }

    /// Create a metrics handle bound to this harness.
    #[must_use]
    pub fn metrics(&self) -> Arc<AuthNMetrics> {
        Arc::new(AuthNMetrics::new(&self.meter()))
    }

    /// Flush aggregated data into the in-memory exporter.
    pub fn force_flush(&self) {
        self.provider
            .force_flush()
            .expect("test meter provider should flush");
    }

    /// Check whether any metric with the given name has been exported.
    #[must_use]
    pub fn metric_exists(&self, name: &str) -> bool {
        self.exporter
            .get_finished_metrics()
            .expect("in-memory exporter should be readable")
            .iter()
            .any(|rm| {
                rm.scope_metrics()
                    .any(|sm| sm.metrics().any(|metric| metric.name() == name))
            })
    }

    /// Sum all matching counter data points.
    #[must_use]
    pub fn counter_value(&self, name: &str, expected_attrs: &[(&str, &str)]) -> u64 {
        let metrics = self
            .exporter
            .get_finished_metrics()
            .expect("in-memory exporter should be readable");
        let mut total = 0u64;

        for resource_metrics in &metrics {
            for scope_metrics in resource_metrics.scope_metrics() {
                for metric in scope_metrics.metrics() {
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

        for resource_metrics in &metrics {
            for scope_metrics in resource_metrics.scope_metrics() {
                for metric in scope_metrics.metrics() {
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

        for resource_metrics in &metrics {
            for scope_metrics in resource_metrics.scope_metrics() {
                for metric in scope_metrics.metrics() {
                    if metric.name() == name
                        && let AggregatedMetrics::F64(MetricData::Histogram(hist)) = metric.data()
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

    /// Bucket boundaries of the named histogram, if exported.
    #[must_use]
    pub fn histogram_bounds(&self, name: &str) -> Option<Vec<f64>> {
        let metrics = self
            .exporter
            .get_finished_metrics()
            .expect("in-memory exporter should be readable");

        for resource_metrics in &metrics {
            for scope_metrics in resource_metrics.scope_metrics() {
                for metric in scope_metrics.metrics() {
                    if metric.name() == name
                        && let AggregatedMetrics::F64(MetricData::Histogram(hist)) = metric.data()
                        && let Some(dp) = hist.data_points().next()
                    {
                        return Some(dp.bounds().collect());
                    }
                }
            }
        }

        None
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
    expected.iter().all(|(expected_key, expected_value)| {
        actual
            .iter()
            .any(|kv| kv.key.as_str() == *expected_key && kv.value.as_str() == *expected_value)
    }) && actual.len() == expected.len()
}

use super::*;

#[test]
fn constructor_emits_initial_gauge_state() {
    let harness = MetricsHarness::new();
    let _metrics = harness.metrics();

    harness.force_flush();

    assert_eq!(
        harness.gauge_value(AUTHN_JWKS_CACHE_ENTRIES, &[]),
        Some(0.0)
    );
    assert_eq!(harness.gauge_value(AUTHN_FIRST_PARTY_RATIO, &[]), Some(0.0));
}

#[test]
fn production_otel_path_records_metrics() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();

    metrics.record_request_success_duration(Duration::from_millis(4));
    metrics.increment_request_failure("unauthorized");
    metrics.increment_error("unauthorized");
    metrics.increment_token_rejected(TOKEN_REJECTION_REASON_EXPIRED);
    metrics.increment_jwks_refresh_failures();
    metrics.set_circuit_breaker_state("idp.example.com", 2.0);
    metrics.increment_circuit_breaker_closed("idp.example.com");
    metrics.set_idp_up("idp.example.com", 0.0);
    metrics.increment_jwks_cache_hit();
    metrics.increment_jwks_cache_miss();
    metrics.record_jwks_cache_entries(3);
    metrics.record_jwt_validation_duration(Duration::from_millis(2));
    metrics.record_jwks_fetch_duration(Duration::from_millis(50));
    metrics.observe_first_party_auth(true);
    metrics.observe_first_party_auth(false);
    metrics.increment_s2s_exchange();
    metrics.increment_s2s_exchange_error("token_acquisition_failed");
    metrics.record_s2s_exchange_duration(Duration::from_millis(150));

    harness.force_flush();

    assert_eq!(
        harness.histogram_count(AUTHN_REQUEST_SUCCESS_DURATION_SECONDS, &[]),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHN_REQUEST_FAILURES_TOTAL, &[("reason", "unauthorized")]),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHN_ERRORS_TOTAL, &[("type", "unauthorized")]),
        1
    );
    assert_eq!(
        harness.counter_value(
            AUTHN_TOKEN_REJECTED_TOTAL,
            &[("reason", TOKEN_REJECTION_REASON_EXPIRED)]
        ),
        1
    );
    assert_eq!(
        harness.counter_value(AUTHN_JWKS_REFRESH_FAILURES_TOTAL, &[]),
        1
    );
    assert_eq!(harness.counter_value(AUTHN_S2S_EXCHANGE_TOTAL, &[]), 1);
    assert_eq!(
        harness.counter_value(
            AUTHN_S2S_EXCHANGE_ERRORS_TOTAL,
            &[("type", "token_acquisition_failed")]
        ),
        1
    );
    assert_eq!(
        harness.gauge_value(AUTHN_CIRCUIT_BREAKER_STATE, &[("host", "idp.example.com")]),
        Some(2.0)
    );
    assert_eq!(
        harness.counter_value(
            AUTHN_CIRCUIT_BREAKER_CLOSED_TOTAL,
            &[("host", "idp.example.com")]
        ),
        1
    );
    assert_eq!(
        harness.gauge_value(AUTHN_IDP_UP, &[("host", "idp.example.com")]),
        Some(0.0)
    );
    assert_eq!(harness.counter_value(AUTHN_JWKS_CACHE_HITS_TOTAL, &[]), 1);
    assert_eq!(harness.counter_value(AUTHN_JWKS_CACHE_MISSES_TOTAL, &[]), 1);
    assert_eq!(
        harness.gauge_value(AUTHN_JWKS_CACHE_ENTRIES, &[]),
        Some(3.0)
    );
    assert_eq!(harness.gauge_value(AUTHN_FIRST_PARTY_RATIO, &[]), Some(0.5));
    assert!(harness.histogram_count(AUTHN_JWT_VALIDATION_DURATION_SECONDS, &[]) >= 1);
    assert!(harness.histogram_count(AUTHN_JWKS_FETCH_DURATION_SECONDS, &[]) >= 1);
    assert!(harness.histogram_count(AUTHN_S2S_EXCHANGE_DURATION_SECONDS, &[]) >= 1);
}

#[test]
fn duration_histograms_use_second_scale_bucket_boundaries() {
    let harness = MetricsHarness::new();
    let metrics = harness.metrics();

    // Record a sample so each histogram exports a data point.
    metrics.record_request_success_duration(Duration::from_millis(3));
    metrics.record_jwks_fetch_duration(Duration::from_millis(40));

    harness.force_flush();

    for name in [
        AUTHN_REQUEST_SUCCESS_DURATION_SECONDS,
        AUTHN_JWKS_FETCH_DURATION_SECONDS,
    ] {
        let bounds = harness
            .histogram_bounds(name)
            .expect("duration histogram should export a data point");
        assert_eq!(
            bounds,
            vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0
            ],
            "{name} records seconds and must use second-scale bucket boundaries"
        );
    }
}
