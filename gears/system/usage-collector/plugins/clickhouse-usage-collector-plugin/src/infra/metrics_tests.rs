// Test modules using bare `panic!` opt in explicitly.
#![allow(clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::{ErrorClass, InsertMode, LockMode, Metrics, QueryKind, label};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

/// A local `SdkMeterProvider` backed by an in-memory exporter. Local (not the
/// process-global) provider so the recording assertions are parallel-safe:
/// [`Metrics::with_meter`] takes the meter explicitly, so tests never mutate
/// `opentelemetry::global` state.
fn local_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    (provider, exporter)
}

/// Total of all `u64` Sum (counter) data points named `name`.
fn counter_sum(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
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

/// Total of the `u64` Sum (counter) data points named `name` carrying
/// `label_key == label_value`.
fn counter_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
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

/// Last value of the `u64` Gauge named `name`, if recorded.
fn gauge_last_u64(exporter: &InMemoryMetricExporter, name: &str) -> Option<u64> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Gauge(g)) = metric.data()
                {
                    return g
                        .data_points()
                        .next()
                        .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value);
                }
            }
        }
    }
    None
}

/// Total observation count across the `f64` Histogram data points named `name`.
fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
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

/// With an in-memory reader installed, the recording helpers emit the expected
/// counter / gauge / histogram series — covering a plain counter, a
/// label-split counter, both gauge kinds, and a duration histogram.
#[tokio::test]
async fn recording_helpers_emit_expected_series() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter(super::SCOPE_NAME));

    // Counter (plain): three absorbed-dedup increments accumulate to 3.
    metrics.inc_dedup_absorbed();
    metrics.inc_dedup_absorbed();
    metrics.inc_dedup_absorbed();

    // Counter (labelled): backend errors split by `error_category`.
    metrics.inc_backend_error(ErrorClass::Transient);
    metrics.inc_backend_error(ErrorClass::Transient);
    metrics.inc_backend_error(ErrorClass::Internal);

    // Gauges: last-value semantics.
    metrics.set_catalog_size(42);
    metrics.set_ready(true);

    // Histogram (labelled): two insert observations.
    metrics.record_insert(InsertMode::Batch, 0.01);
    metrics.record_insert(InsertMode::Batch, 0.02);

    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_clickhouse_dedup_absorbed_total"),
        3,
    );
    assert_eq!(
        counter_sum(&exporter, "uc_clickhouse_backend_errors_total"),
        3,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_clickhouse_backend_errors_total",
            label::ERROR_CATEGORY,
            label::ERROR_CATEGORY_TRANSIENT,
        ),
        2,
    );
    assert_eq!(
        gauge_last_u64(&exporter, "uc_clickhouse_usage_type_catalog_size"),
        Some(42),
    );
    assert_eq!(gauge_last_u64(&exporter, "uc_clickhouse_ready"), Some(1));
    assert_eq!(
        histogram_count(&exporter, "uc_clickhouse_insert_duration_seconds"),
        2,
    );
}

/// Lock instrument set emits expected series: acquire-duration histogram split
/// by mode, contention counter split by mode, and unavailable counter.
#[tokio::test]
async fn lock_instruments_emit_expected_series() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter(super::SCOPE_NAME));

    // One shared and one exclusive acquire.
    metrics.record_lock_acquire(LockMode::Create, 0.005);
    metrics.record_lock_acquire(LockMode::Delete, 0.010);

    // Contention: two shared contention events, one exclusive.
    metrics.inc_lock_contention(LockMode::Create);
    metrics.inc_lock_contention(LockMode::Create);
    metrics.inc_lock_contention(LockMode::Delete);

    // One unavailable event (shared — session loss on ensure_still_held).
    metrics.inc_lock_manager_unavailable(LockMode::Create);

    provider.force_flush().unwrap();

    // Acquire-duration histogram: 2 observations total.
    assert_eq!(
        histogram_count(&exporter, "uc_clickhouse_lock_acquire_duration_seconds"),
        2,
    );

    // Contention counter split by mode.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_clickhouse_lock_contention_total",
            label::MODE,
            label::MODE_CREATE,
        ),
        2,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_clickhouse_lock_contention_total",
            label::MODE,
            label::MODE_DELETE,
        ),
        1,
    );

    // Unavailable counter.
    assert_eq!(
        counter_sum(&exporter, "uc_clickhouse_lock_manager_unavailable_total"),
        1,
    );
}

/// Smoke-checks [`Metrics::new`] (global provider path) plus value assertions
/// for remaining helpers via the local in-memory seam.
#[tokio::test]
async fn remaining_helpers_emit_expected_series() {
    // Global-provider path: must not panic; recordings are no-ops (no reader).
    let global = Metrics::new();
    global.set_ready(true);
    global.set_catalog_size(0);
    global.inc_dedup_absorbed();
    global.record_insert(InsertMode::Single, 0.001);
    global.record_query(QueryKind::Aggregated, 0.002);
    global.inc_backend_error(ErrorClass::Transient);

    // Value assertions via local in-memory reader.
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter(super::SCOPE_NAME));

    metrics.inc_idempotency_conflict();
    metrics.inc_idempotency_conflict();
    metrics.inc_compensation();
    metrics.inc_compensation();
    metrics.inc_compensation();
    metrics.inc_usage_type_referenced();
    metrics.inc_migration_failure();
    metrics.inc_query_request(QueryKind::Raw);
    metrics.inc_query_request(QueryKind::Raw);
    metrics.set_catalog_size(7);
    metrics.record_query(QueryKind::Raw, 0.001);

    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_clickhouse_idempotency_conflicts_total"),
        2,
    );
    assert_eq!(
        counter_sum(&exporter, "uc_clickhouse_compensations_total"),
        3,
    );
    assert_eq!(
        counter_sum(&exporter, "uc_clickhouse_usage_type_referenced_total"),
        1,
    );
    assert_eq!(
        counter_sum(&exporter, "uc_clickhouse_migration_failures_total"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_clickhouse_query_requests_total",
            label::QUERY_KIND,
            label::QUERY_KIND_RAW,
        ),
        2,
    );
    assert_eq!(
        gauge_last_u64(&exporter, "uc_clickhouse_usage_type_catalog_size"),
        Some(7),
    );
    assert_eq!(
        histogram_count(&exporter, "uc_clickhouse_query_duration_seconds"),
        1,
    );
}

/// `Default` must build the same inventory as [`Metrics::new`] — it is the
/// entry point any caller relying on `#[derive(Default)]` composition gets.
#[test]
fn default_builds_the_same_inventory_as_new() {
    let from_default = Metrics::default();
    // Recording through the default-built inventory must not panic: every
    // instrument is registered, not left uninitialised.
    from_default.inc_query_request(QueryKind::Raw);
    from_default.set_catalog_size(3);
}
