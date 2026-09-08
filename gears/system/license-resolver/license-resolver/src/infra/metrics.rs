//! `OpenTelemetry` adapter behind the [`LicenseMetrics`] port.
//!
//! Counters omit a `_total` suffix — the Prometheus exporter appends it.
//!
//! Cardinality rule: a label value is either a `&'static str` from a closed
//! enum ([`CheckOutcome`], [`ViolationKind`]), the process-wide configured
//! vendor, or a contract type the validator has confirmed registered — never a
//! caller-supplied string. Instance ids, `metadata` values and tenant ids are
//! span and log fields only.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::domain::{CheckOutcome, LicenseMetrics, ViolationKind};

/// Instrument-name prefix for this gear.
pub const DEFAULT_PREFIX: &str = "license";

/// Latency buckets in milliseconds. Chosen to straddle the 50 ms read-latency
/// target so the requirement is readable straight off the histogram.
const DURATION_BUCKETS_MS: [f64; 12] = [
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0,
];

const CONTRACT_TYPE_LABEL: &str = "contract_type";
const VENDOR_LABEL: &str = "vendor";
const OUTCOME_LABEL: &str = "outcome";
const VIOLATION_KIND_LABEL: &str = "violation_kind";

/// Records the resolver's check counter, boundary latency and validation
/// failures.
pub struct LicenseMetricsMeter {
    check: Counter<u64>,
    check_duration_ms: Histogram<f64>,
    validation_failure: Counter<u64>,
    vendor: KeyValue,
}

impl LicenseMetricsMeter {
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str, vendor: &str) -> Self {
        Self {
            check: meter
                .u64_counter(format!("{prefix}_check"))
                .with_description("License checks completed (contract_type, vendor, outcome)")
                .build(),
            check_duration_ms: meter
                .f64_histogram(format!("{prefix}_check_duration_ms"))
                .with_description(
                    "Resolver-side license check latency in milliseconds \
                     (contract validation and plugin selection, excluding backend compute)",
                )
                .with_unit("ms")
                .with_boundaries(DURATION_BUCKETS_MS.to_vec())
                .build(),
            validation_failure: meter
                .u64_counter(format!("{prefix}_validation_failure"))
                .with_description("Licensing contract violations found (violation_kind)")
                .build(),
            vendor: KeyValue::new(VENDOR_LABEL, vendor.to_owned()),
        }
    }
}

impl LicenseMetrics for LicenseMetricsMeter {
    fn record_check(&self, contract_type: &str, outcome: CheckOutcome) {
        self.check.add(
            1,
            &[
                KeyValue::new(CONTRACT_TYPE_LABEL, contract_type.to_owned()),
                self.vendor.clone(),
                KeyValue::new(OUTCOME_LABEL, outcome.as_label()),
            ],
        );
    }

    fn record_resolver_latency(&self, millis: f64) {
        self.check_duration_ms.record(millis, &[]);
    }

    fn record_validation_failure(&self, kind: ViolationKind) {
        self.validation_failure
            .add(1, &[KeyValue::new(VIOLATION_KIND_LABEL, kind.as_label())]);
    }
}

/// Build the adapter against the process-global meter provider.
///
/// When metrics are disabled the global provider is a no-op, so the instruments
/// are free and can be built unconditionally.
#[must_use]
pub fn build_default_adapter(vendor: &str) -> Arc<LicenseMetricsMeter> {
    let scope = opentelemetry::InstrumentationScope::builder("license-resolver").build();
    let meter = opentelemetry::global::meter_with_scope(scope);
    Arc::new(LicenseMetricsMeter::new(&meter, DEFAULT_PREFIX, vendor))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
