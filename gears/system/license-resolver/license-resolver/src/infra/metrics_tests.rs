use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use super::{DEFAULT_PREFIX, DURATION_BUCKETS_MS, LicenseMetricsMeter};
use crate::domain::{CheckOutcome, LicenseMetrics, ViolationKind};

const TEST_VENDOR: &str = "acme";
const CONTRACT: &str = "gts.cf.core.lic.res.v1~x.y.v1~";

fn local_meter() -> (
    SdkMeterProvider,
    InMemoryMetricExporter,
    LicenseMetricsMeter,
) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let meter = LicenseMetricsMeter::new(
        &provider.meter("license-resolver"),
        DEFAULT_PREFIX,
        TEST_VENDOR,
    );
    (provider, exporter, meter)
}

fn counter_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    key: &str,
    value: &str,
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
                            dp.attributes()
                                .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

fn histogram_count_and_bounds(
    exporter: &InMemoryMetricExporter,
    name: &str,
) -> Option<(u64, Vec<f64>)> {
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
                        .map(|dp| (dp.count(), dp.bounds().collect()));
                }
            }
        }
    }
    None
}

#[test]
fn check_counter_carries_contract_type_vendor_and_outcome() {
    let (provider, exporter, meter) = local_meter();
    meter.record_check(CONTRACT, CheckOutcome::Granted);
    meter.record_check(CONTRACT, CheckOutcome::NoPlugin);
    provider.force_flush().unwrap();

    let by =
        |key: &str, value: &str| counter_sum_with_label(&exporter, "license_check", key, value);
    assert_eq!(by("outcome", "granted"), 1);
    assert_eq!(by("outcome", "no_plugin"), 1);
    assert_eq!(by("vendor", TEST_VENDOR), 2);
    assert_eq!(by("contract_type", CONTRACT), 2);
}

#[test]
fn latency_histogram_pins_the_ms_buckets() {
    let (provider, exporter, meter) = local_meter();
    meter.record_resolver_latency(3.0);
    provider.force_flush().unwrap();

    let (count, bounds) = histogram_count_and_bounds(&exporter, "license_check_duration_ms")
        .expect("latency histogram must be exported");
    assert_eq!(count, 1);
    assert_eq!(bounds, DURATION_BUCKETS_MS.to_vec());
    assert!(
        bounds.contains(&50.0),
        "the 50 ms read-latency target must be a bucket boundary: {bounds:?}"
    );
}

#[test]
fn validation_failure_counter_carries_the_violation_kind() {
    let (provider, exporter, meter) = local_meter();
    meter.record_validation_failure(ViolationKind::SchemaMismatch);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "license_validation_failure",
            "violation_kind",
            "SCHEMA_MISMATCH",
        ),
        1
    );
}
