//! `OpenTelemetry` adapter behind the [`QeMetrics`] port.
//!
//! Instruments are declared on a scoped `Meter` from `ToolKit`'s global
//! `SdkMeterProvider`. The gear runs no exporter and exposes no scrape
//! endpoint; `ToolKit` pushes OTLP when its `otel` feature is on. Names are the
//! PRD section 5.16 catalogue names, optionally under a configured prefix.
//! Label values are `&'static str` from closed enums only.
//!
//! The three label-free lifecycle gauges observe the sample the elected
//! replica's refresh published into a [`LifecycleGaugeCell`]; a callback never
//! computes anything and observes nothing while no sample is published.

use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter, ObservableGauge};

use crate::config::MetricsConfig;
use crate::domain::ports::lifecycle_gauges::LifecycleCounts;
use crate::domain::ports::metrics::{
    DenialReason, QeMetrics, REASON_LABEL, SURFACE_LABEL, ValidationReason, ValidationSurface,
};
use crate::infra::lifecycle_gauges::{
    LifecycleGaugeCell, QUOTA_CAP_UNBOUNDED_TOTAL, QUOTA_CAP_ZERO_TOTAL,
    QUOTA_FOR_DIRECT_METRIC_TOTAL,
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
    /// Held so the observable gauges stay registered for the meter's life.
    _lifecycle_gauges: [ObservableGauge<u64>; 3],
}

// @cpt-algo:cpt-cf-quota-enforcement-algo-telemetry-emission:p1
impl QeMetricsMeter {
    /// Declare the instruments on `meter`. The lifecycle gauges observe the
    /// sample in `gauges`, the cell the elected replica's refresh publishes to.
    #[must_use]
    pub fn new(meter: &Meter, config: &MetricsConfig, gauges: Arc<LifecycleGaugeCell>) -> Self {
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
        // Label-free gauges over the published sample. Each callback reads the
        // cell and observes only when a sample is published: a withdrawn
        // sample (stale, not leader) yields no data point rather than zero.
        let lifecycle_gauges = [
            lifecycle_gauge(
                meter,
                &config.instrument_name(QUOTA_CAP_ZERO_TOTAL),
                "Active cap = 0 Quotas",
                gauges.clone(),
                |c| c.cap_zero,
            ),
            lifecycle_gauge(
                meter,
                &config.instrument_name(QUOTA_CAP_UNBOUNDED_TOTAL),
                "Active unbounded-cap Quotas",
                gauges.clone(),
                |c| c.cap_unbounded,
            ),
            lifecycle_gauge(
                meter,
                &config.instrument_name(QUOTA_FOR_DIRECT_METRIC_TOTAL),
                "Active Quotas declared on metrics currently classified Direct",
                gauges,
                |c| c.for_direct_metric,
            ),
        ];
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-closed
        Self {
            denials,
            contract_validation_failures,
            admitted_metric_violations,
            _lifecycle_gauges: lifecycle_gauges,
        }
    }

    /// Build the adapter on the process-global meter provider.
    ///
    /// When metrics are disabled the global provider is a no-op, so the
    /// instruments cost nothing and are built unconditionally.
    #[must_use]
    pub fn on_global_meter(config: &MetricsConfig, gauges: Arc<LifecycleGaugeCell>) -> Arc<Self> {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-export
        let scope = opentelemetry::InstrumentationScope::builder("quota-enforcement").build();
        let meter = opentelemetry::global::meter_with_scope(scope);
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-export
        Arc::new(Self::new(&meter, config, gauges))
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

/// One label-free observable gauge over the published lifecycle sample.
fn lifecycle_gauge(
    meter: &Meter,
    name: &str,
    description: &'static str,
    cell: Arc<LifecycleGaugeCell>,
    pick: fn(&LifecycleCounts) -> u64,
) -> ObservableGauge<u64> {
    meter
        .u64_observable_gauge(name.to_owned())
        .with_description(description)
        .with_callback(move |observer| {
            if let Some(counts) = cell.load() {
                observer.observe(pick(&counts), &[]);
            }
        })
        .build()
}

/// Build the adapter on the process-global meter provider.
#[must_use]
pub fn build_default_adapter(
    config: &MetricsConfig,
    gauges: Arc<LifecycleGaugeCell>,
) -> Arc<QeMetricsMeter> {
    QeMetricsMeter::on_global_meter(config, gauges)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
