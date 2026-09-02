//! Infrastructure adapters: the canonical-error lift and the metrics meter.

pub mod canonical_mapping;
pub mod metrics;

pub use metrics::{QeMetricsMeter, build_default_adapter};
