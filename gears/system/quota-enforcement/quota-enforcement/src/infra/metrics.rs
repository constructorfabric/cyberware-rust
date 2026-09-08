//! `OpenTelemetry` adapter behind the [`QeMetrics`] port.
//!
//! Instruments are declared on a scoped `Meter` from `ToolKit`'s global
//! `SdkMeterProvider`. The gear runs no exporter and exposes no scrape
//! endpoint; `ToolKit` pushes OTLP when its `otel` feature is on. Names are the
//! PRD section 5.16 catalogue names, optionally under a configured prefix.
//! Label values are `&'static str` from closed enums only.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter};

use crate::config::MetricsConfig;
use crate::domain::ports::metrics::{
    DenialReason, QeMetrics, REASON_LABEL, SURFACE_LABEL, ValidationReason, ValidationSurface,
};

/// Catalogue name of the admission-denial counter.
pub const DENIAL_TOTAL: &str = "denial_total";

/// Catalogue name of the contract-validation failure counter.
pub const CONTRACT_VALIDATION_FAILURES_TOTAL: &str = "contract_validation_failures_total";

/// Catalogue name of the projection/metric incompatibility counter.
pub const ADMITTED_METRIC_VIOLATIONS_TOTAL: &str = "admitted_metric_violations_total";

/// The gear's instruments.
// @cpt-dod:cpt-cf-quota-enforcement-dod-telemetry-conventions:p1
// @cpt-dod:cpt-cf-quota-enforcement-dod-contract-validation-telemetry:p1
pub struct QeMetricsMeter {
    denials: Counter<u64>,
    contract_validation_failures: Counter<u64>,
    admitted_metric_violations: Counter<u64>,
}

// @cpt-algo:cpt-cf-quota-enforcement-algo-telemetry-emission:p1
impl QeMetricsMeter {
    /// Declare the instruments on `meter`.
    #[must_use]
    pub fn new(meter: &Meter, config: &MetricsConfig) -> Self {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-closed
        // Only PRD 5.16 catalogue instruments are declared here.
        let denials = meter
            .u64_counter(config.instrument_name(DENIAL_TOTAL))
            .with_description("Admission denials by closed reason kind")
            .build();
        let contract_validation_failures = meter
            .u64_counter(config.instrument_name(CONTRACT_VALIDATION_FAILURES_TOTAL))
            .with_description("Rejected contract instances by closed validation surface and reason")
            .build();
        let admitted_metric_violations = meter
            .u64_counter(config.instrument_name(ADMITTED_METRIC_VIOLATIONS_TOTAL))
            .with_description("Projection/metric incompatibilities by closed validation surface")
            .build();
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-closed
        Self {
            denials,
            contract_validation_failures,
            admitted_metric_violations,
        }
    }

    /// Build the adapter on the process-global meter provider.
    ///
    /// When metrics are disabled the global provider is a no-op, so the
    /// instruments cost nothing and are built unconditionally.
    #[must_use]
    pub fn on_global_meter(config: &MetricsConfig) -> Arc<Self> {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-export
        let scope = opentelemetry::InstrumentationScope::builder("quota-enforcement").build();
        let meter = opentelemetry::global::meter_with_scope(scope);
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-export
        Arc::new(Self::new(&meter, config))
    }

    fn add_denial(&self, reason: DenialReason) {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-emit
        // @cpt-begin:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-highcard
        // The only label is a closed-enum value. No identifier ever enters.
        self.denials
            .add(1, &[KeyValue::new(REASON_LABEL, reason.as_label())]);
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-highcard
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-emit
    }
}

impl QeMetrics for QeMetricsMeter {
    fn record_denial(&self, reason: DenialReason) {
        self.add_denial(reason);
    }

    fn record_contract_validation_failure(
        &self,
        surface: ValidationSurface,
        reason: ValidationReason,
    ) {
        // Both labels are closed-enum values; metric names, projection types,
        // and caller attribution never enter (PRD section 5.16).
        self.contract_validation_failures.add(
            1,
            &[
                KeyValue::new(SURFACE_LABEL, surface.as_label()),
                KeyValue::new(REASON_LABEL, reason.as_label()),
            ],
        );
    }

    fn record_admitted_metric_violation(&self, surface: ValidationSurface) {
        self.admitted_metric_violations
            .add(1, &[KeyValue::new(SURFACE_LABEL, surface.as_label())]);
    }
}

/// Build the adapter on the process-global meter provider.
#[must_use]
pub fn build_default_adapter(config: &MetricsConfig) -> Arc<QeMetricsMeter> {
    QeMetricsMeter::on_global_meter(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
