//! The instrument contract: rendered names, label keys, label values and bucket layouts.

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, InMemoryMetricExporterBuilder, PeriodicReader, SdkMeterProvider,
    Temporality,
};

use super::{
    ACTIVATION_WRITE_SET_BUCKETS, AdmissionMetricsMeter, OPERATION_DURATION_BUCKETS_SECONDS, SCOPE,
};
use crate::domain::admission::vector::{VectorDrift, VectorRole};
use crate::domain::ports::metrics::{AdmissionMetrics, RefusalStage, TerminalStatus};

fn default_prefix() -> String {
    crate::config::MetricsConfig::default().effective_prefix("types-registry")
}

fn recorder() -> (
    SdkMeterProvider,
    InMemoryMetricExporter,
    AdmissionMetricsMeter,
) {
    let exporter = InMemoryMetricExporterBuilder::new()
        .with_temporality(Temporality::Delta)
        .build();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = AdmissionMetricsMeter::new(&provider.meter(SCOPE), &default_prefix());
    (provider, exporter, metrics)
}

fn counter_sum(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

fn counter_sum_where(
    exporter: &InMemoryMetricExporter,
    name: &str,
    labels: &[(&str, &str)],
) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .filter(|dp| {
                            labels.iter().all(|(key, value)| {
                                dp.attributes().any(|kv| {
                                    kv.key.as_str() == *key && kv.value.as_str() == *value
                                })
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

fn histogram_bounds(exporter: &InMemoryMetricExporter, name: &str) -> Option<Vec<f64>> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h.data_points().next().map(|dp| dp.bounds().collect());
                }
            }
        }
    }
    None
}

fn histogram_sum(exporter: &InMemoryMetricExporter, name: &str) -> Option<f64> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .next()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::sum);
                }
            }
        }
    }
    None
}

fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

#[test]
fn candidates_are_counted_by_their_terminal_status() {
    let (provider, exporter, metrics) = recorder();

    metrics.candidate_terminalized(TerminalStatus::Succeeded);
    metrics.candidate_terminalized(TerminalStatus::Succeeded);
    metrics.candidate_terminalized(TerminalStatus::Unchanged);
    metrics.candidate_terminalized(TerminalStatus::Failed);
    provider.force_flush().unwrap();

    assert_eq!(counter_sum(&exporter, "types_registry_candidates_total"), 4);
    assert_eq!(
        counter_sum_where(
            &exporter,
            "types_registry_candidates_total",
            &[("status", "succeeded")],
        ),
        2,
    );
    assert_eq!(
        counter_sum_where(
            &exporter,
            "types_registry_candidates_total",
            &[("status", "unchanged")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where(
            &exporter,
            "types_registry_candidates_total",
            &[("status", "failed")],
        ),
        1,
    );
}

#[test]
fn a_non_terminal_status_does_not_convert_to_a_terminal_one() {
    use crate::domain::enums::OperationItemStatus;

    assert!(TerminalStatus::try_from(OperationItemStatus::Pending).is_err());
    assert!(TerminalStatus::try_from(OperationItemStatus::Running).is_err());
    for status in [
        OperationItemStatus::Succeeded,
        OperationItemStatus::Unchanged,
        OperationItemStatus::Failed,
    ] {
        assert!(
            TerminalStatus::try_from(status).is_ok(),
            "{status:?} is terminal and must convert"
        );
    }
}

#[test]
fn refusals_carry_their_stage_and_reason() {
    let (provider, exporter, metrics) = recorder();

    metrics.refused(RefusalStage::Acceptance, "empty_batch");
    metrics.refused(RefusalStage::Admission, "precondition_failed");
    metrics.refused(RefusalStage::Admission, "precondition_failed");
    provider.force_flush().unwrap();

    assert_eq!(counter_sum(&exporter, "types_registry_refusals_total"), 3);
    assert_eq!(
        counter_sum_where(
            &exporter,
            "types_registry_refusals_total",
            &[("stage", "acceptance"), ("reason", "empty_batch")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where(
            &exporter,
            "types_registry_refusals_total",
            &[("stage", "admission"), ("reason", "precondition_failed")],
        ),
        2,
    );
}

#[test]
fn two_reasons_at_one_stage_are_two_series() {
    let (provider, exporter, metrics) = recorder();

    metrics.refused(RefusalStage::Acceptance, "empty_batch");
    metrics.refused(RefusalStage::Acceptance, "duplicate_candidate");
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_where(
            &exporter,
            "types_registry_refusals_total",
            &[("reason", "empty_batch")],
        ),
        1,
    );
    assert_eq!(
        counter_sum_where(
            &exporter,
            "types_registry_refusals_total",
            &[("reason", "duplicate_candidate")],
        ),
        1,
    );
}

#[test]
fn revalidation_retries_are_counted_by_drift_shape() {
    let (provider, exporter, metrics) = recorder();

    metrics.revalidation_retried(&VectorDrift::Moved {
        gts_id: "x".to_owned(),
        role: VectorRole::Dependency,
        recorded: 1,
        found: 2,
    });
    metrics.revalidation_retried(&VectorDrift::Appeared {
        gts_id: "y".to_owned(),
        role: VectorRole::Dependent,
    });
    metrics.revalidation_retried(&VectorDrift::Vanished {
        gts_id: "z".to_owned(),
        role: VectorRole::Dependency,
    });
    metrics.revalidation_retried(&VectorDrift::Refreshed {
        gts_id: "w".to_owned(),
    });
    metrics.revalidation_retried(&VectorDrift::CurrentProjectionMoved {
        gts_id: "v".to_owned(),
    });
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "types_registry_revalidations_total"),
        5,
    );
    for shape in [
        "moved",
        "appeared",
        "vanished",
        "refreshed",
        "current_projection_moved",
    ] {
        assert_eq!(
            counter_sum_where(
                &exporter,
                "types_registry_revalidations_total",
                &[("drift", shape)],
            ),
            1,
            "one retry per drift shape, missing {shape}",
        );
    }
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn activation_write_set_buckets_reach_the_configured_default_bound() {
    let (provider, exporter, metrics) = recorder();

    metrics.observe_activation_write_set(3);
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_bounds(&exporter, "types_registry_activation_write_set"),
        Some(ACTIVATION_WRITE_SET_BUCKETS.to_vec()),
    );
    assert_eq!(
        ACTIVATION_WRITE_SET_BUCKETS.last().copied(),
        Some(crate::config::Limits::default().activation_write_set as f64),
        "the top bucket tracks the default limits.activation_write_set",
    );
    assert_eq!(
        histogram_sum(&exporter, "types_registry_activation_write_set"),
        Some(3.0),
    );
}

#[test]
fn an_empty_activation_write_set_is_still_observed() {
    let (provider, exporter, metrics) = recorder();

    metrics.observe_activation_write_set(0);
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "types_registry_activation_write_set"),
        1,
    );
    assert_eq!(
        histogram_sum(&exporter, "types_registry_activation_write_set"),
        Some(0.0),
    );
}

#[test]
fn operation_duration_is_recorded_in_seconds() {
    let (provider, exporter, metrics) = recorder();

    metrics.observe_operation_duration(std::time::Duration::from_millis(250));
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_bounds(&exporter, "types_registry_operation_duration_seconds"),
        Some(OPERATION_DURATION_BUCKETS_SECONDS.to_vec()),
    );
    let sum = histogram_sum(&exporter, "types_registry_operation_duration_seconds")
        .expect("the duration must be observed");
    assert!(
        (sum - 0.25).abs() < 1e-9,
        "250ms must be recorded as 0.25s, got {sum}",
    );
}

#[test]
fn the_default_prefix_is_the_gear_name_in_snake_case() {
    assert_eq!(default_prefix(), "types_registry");
}

#[test]
fn a_configured_prefix_renames_every_series() {
    let exporter = InMemoryMetricExporterBuilder::new()
        .with_temporality(Temporality::Delta)
        .build();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = AdmissionMetricsMeter::new(&provider.meter(SCOPE), "tenant_a_tr");

    metrics.candidate_terminalized(TerminalStatus::Succeeded);
    metrics.refused(RefusalStage::Acceptance, "zero_precondition");
    metrics.observe_activation_write_set(1);
    metrics.observe_operation_duration(std::time::Duration::from_millis(5));
    provider.force_flush().unwrap();

    let names = recorded_names(&exporter);
    for suffix in [
        "candidates_total",
        "refusals_total",
        "activation_write_set",
        "operation_duration_seconds",
    ] {
        assert!(
            names.contains(&format!("tenant_a_tr_{suffix}")),
            "expected tenant_a_tr_{suffix} among {names:?}"
        );
        assert!(
            !names.contains(&format!("types_registry_{suffix}")),
            "the default prefix must not survive a configured one: {names:?}"
        );
    }
}

fn recorded_names(exporter: &InMemoryMetricExporter) -> Vec<String> {
    let mut names = Vec::new();
    for rm in &exporter.get_finished_metrics().unwrap() {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                names.push(metric.name().to_owned());
            }
        }
    }
    names
}
