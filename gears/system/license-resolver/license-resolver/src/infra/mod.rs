//! Infrastructure adapters for the license resolver.

pub mod metrics;
pub mod types_registry;

pub use metrics::LicenseMetricsMeter;
pub use types_registry::GtsContractRegistry;
