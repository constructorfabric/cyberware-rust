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
use crate::domain::ports::metrics::{DenialReason, QeMetrics, REASON_LABEL};

/// Catalogue name of the admission-denial counter.
pub const DENIAL_TOTAL: &str = "denial_total";

/// The foundation-owned instruments.
// @cpt-dod:cpt-cf-quota-enforcement-dod-telemetry-conventions:p1
pub struct QeMetricsMeter {
    denial_total: Counter<u64>,
}

// @cpt-algo:cpt-cf-quota-enforcement-algo-telemetry-emission:p1
impl QeMetricsMeter {
    /// Declare the instruments on `meter`.
    #[must_use]
    pub fn new(meter: &Meter, config: &MetricsConfig) -> Self {
        // @cpt-begin:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-closed
        // Only PRD 5.16 catalogue instruments are declared here.
        let denial_total = meter
            .u64_counter(config.instrument_name(DENIAL_TOTAL))
            .with_description("Admission denials by closed reason kind")
            .build();
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-closed
        Self { denial_total }
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
        self.denial_total
            .add(1, &[KeyValue::new(REASON_LABEL, reason.as_label())]);
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-highcard
        // @cpt-end:cpt-cf-quota-enforcement-algo-telemetry-emission:p1:inst-tel-emit
    }
}

impl QeMetrics for QeMetricsMeter {
    fn record_denial(&self, reason: DenialReason) {
        self.add_denial(reason);
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
