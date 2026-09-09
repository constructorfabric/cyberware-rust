//! OpenTelemetry adapter for admission metrics.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::{InstrumentationScope, KeyValue};

use crate::domain::admission::vector::VectorDrift;
use crate::domain::ports::metrics::{AdmissionMetrics, RefusalStage, TerminalStatus};

/// Instrumentation scope shared by this gear's metrics.
pub const SCOPE: &str = "cf-gears-types-registry";

/// Bucket boundaries for `types_registry_activation_write_set`.
pub const ACTIVATION_WRITE_SET_BUCKETS: [f64; 10] =
    [0.0, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 256.0, 512.0];

/// Bucket boundaries (seconds) for `types_registry_operation_duration_seconds`.
pub const OPERATION_DURATION_BUCKETS_SECONDS: [f64; 10] =
    [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// The *shape* of a drift, never the identifier that drifted.
const fn drift_label(drift: &VectorDrift) -> &'static str {
    match drift {
        VectorDrift::Appeared { .. } => "appeared",
        VectorDrift::Vanished { .. } => "vanished",
        VectorDrift::Moved { .. } => "moved",
        VectorDrift::Refreshed { .. } => "refreshed",
        VectorDrift::CurrentProjectionMoved { .. } => "current_projection_moved",
    }
}

/// The OpenTelemetry rendering of [`AdmissionMetrics`].
#[derive(Debug)]
pub struct AdmissionMetricsMeter {
    /// Candidates terminalized by this pass, by status.
    candidates: Counter<u64>,
    /// `types_registry_refusals_total{stage,reason}`.
    refusals: Counter<u64>,
    /// Revalidation retries, by drift.
    revalidations: Counter<u64>,
    /// Dependents rewritten by one revision (SPEC §8.1 step 4.6).
    activation_write_set: Histogram<f64>,
    /// `types_registry_operation_duration_seconds` — one admission pass, wall-clock.
    operation_duration: Histogram<f64>,
}

impl AdmissionMetricsMeter {
    /// Declare every instrument on `meter`.
    #[must_use]
    pub fn new(meter: &Meter, prefix: &str) -> Self {
        Self {
            candidates: meter
                .u64_counter(format!("{prefix}_candidates_total"))
                .with_description(
                    "Candidates terminalized by this pass, by terminal status \
                     (succeeded / unchanged / failed)",
                )
                .build(),
            refusals: meter
                .u64_counter(format!("{prefix}_refusals_total"))
                .with_description(
                    "Refusals by the stage that refused (acceptance / admission) and the \
                     machine reason",
                )
                .build(),
            revalidations: meter
                .u64_counter(format!("{prefix}_revalidations_total"))
                .with_description(
                    "Revalidation retries taken after the commit-time revision-vector guard \
                     or an artifact write's compare-and-swap fired, by drift shape",
                )
                .build(),
            activation_write_set: meter
                .f64_histogram(format!("{prefix}_activation_write_set"))
                .with_description("Dependents whose effective artifacts one revision rewrote")
                .with_boundaries(ACTIVATION_WRITE_SET_BUCKETS.to_vec())
                .build(),
            operation_duration: meter
                .f64_histogram(format!("{prefix}_operation_duration_seconds"))
                .with_description("One admission pass over an operation, wall-clock")
                .with_boundaries(OPERATION_DURATION_BUCKETS_SECONDS.to_vec())
                .build(),
        }
    }
}

impl AdmissionMetrics for AdmissionMetricsMeter {
    fn candidate_terminalized(&self, status: TerminalStatus) {
        self.candidates
            .add(1, &[KeyValue::new("status", status.label())]);
    }

    fn refused(&self, stage: RefusalStage, reason: &'static str) {
        self.refusals.add(
            1,
            &[
                KeyValue::new("stage", stage.label()),
                // The static type enforces a closed label vocabulary.
                KeyValue::new("reason", reason),
            ],
        );
    }

    fn revalidation_retried(&self, drift: &VectorDrift) {
        self.revalidations
            .add(1, &[KeyValue::new("drift", drift_label(drift))]);
    }

    fn observe_activation_write_set(&self, refreshed: usize) {
        // The configured bound fits exactly in `f64` in practice.
        #[allow(clippy::cast_precision_loss)]
        self.activation_write_set.record(refreshed as f64, &[]);
    }

    fn observe_operation_duration(&self, elapsed: Duration) {
        self.operation_duration.record(elapsed.as_secs_f64(), &[]);
    }
}

/// Build the adapter from the current global `MeterProvider`.
#[must_use]
pub fn default_adapter(prefix: &str) -> Arc<AdmissionMetricsMeter> {
    let scope = InstrumentationScope::builder(SCOPE).build();
    Arc::new(AdmissionMetricsMeter::new(
        &opentelemetry::global::meter_with_scope(scope),
        prefix,
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metrics_tests.rs"]
mod metrics_tests;
