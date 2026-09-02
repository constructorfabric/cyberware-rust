//! Rendered names and labels against an in-memory `OpenTelemetry` exporter.

#![allow(clippy::expect_used)]

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use super::{DENIAL_TOTAL, QeMetricsMeter, build_default_adapter};
use crate::config::MetricsConfig;
use crate::domain::ports::metrics::{DenialReason, QeMetrics, REASON_LABEL};

fn local_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    (provider, exporter)
}

/// Sum of a `u64` counter over the data points whose `label` equals `value`.
/// `None` when no instrument of that name was exported.
fn counter_sum(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label: Option<(&str, &str)>,
) -> Option<u64> {
    let metrics = exporter.get_finished_metrics().expect("finished metrics");
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() != name {
                    continue;
                }
                let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                    return None;
                };
                return Some(
                    sum.data_points()
                        .filter(|dp| {
                            label.is_none_or(|(k, v)| {
                                dp.attributes()
                                    .any(|kv| kv.key.as_str() == k && kv.value.as_str() == v)
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum(),
                );
            }
        }
    }
    None
}

#[test]
fn denial_total_is_rendered_under_the_catalogue_name_with_the_reason_label() {
    let (provider, exporter) = local_provider();
    let meter = QeMetricsMeter::new(
        &provider.meter("quota-enforcement"),
        &MetricsConfig::default(),
    );

    meter.record_denial(DenialReason::PermissionDenied);
    meter.record_denial(DenialReason::PermissionDenied);
    meter.record_denial(DenialReason::PdpUnavailable);
    provider.force_flush().expect("flush");

    assert_eq!(
        counter_sum(
            &exporter,
            DENIAL_TOTAL,
            Some((REASON_LABEL, "permission_denied"))
        ),
        Some(2)
    );
    assert_eq!(
        counter_sum(
            &exporter,
            DENIAL_TOTAL,
            Some((REASON_LABEL, "pdp_unavailable"))
        ),
        Some(1)
    );
    assert_eq!(
        counter_sum(&exporter, DENIAL_TOTAL, Some((REASON_LABEL, "not_ready"))),
        Some(0),
        "an unrecorded reason has no series"
    );
    assert_eq!(counter_sum(&exporter, DENIAL_TOTAL, None), Some(3));
}

#[test]
fn a_configured_prefix_namespaces_the_instrument() {
    let (provider, exporter) = local_provider();
    let config = MetricsConfig {
        prefix: "qe".to_owned(),
    };
    let meter = QeMetricsMeter::new(&provider.meter("quota-enforcement"), &config);
    meter.record_denial(DenialReason::InvalidArgument);
    provider.force_flush().expect("flush");
    assert_eq!(counter_sum(&exporter, "qe_denial_total", None), Some(1));
    assert_eq!(
        counter_sum(&exporter, DENIAL_TOTAL, None),
        None,
        "no unprefixed series"
    );
}

#[test]
fn every_reason_label_is_a_distinct_snake_case_token() {
    let labels: Vec<&str> = DenialReason::ALL.iter().map(|r| r.as_label()).collect();
    let mut dedup = labels.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), labels.len(), "duplicate labels: {labels:?}");
    for label in labels {
        assert!(
            label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "label {label:?} must be snake_case"
        );
    }
}

#[test]
fn the_default_adapter_builds_on_the_global_provider_without_panicking() {
    let adapter = build_default_adapter(&MetricsConfig::default());
    adapter.record_denial(DenialReason::NotReady);
}
