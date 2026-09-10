//! Output port for the lifecycle gauges (quota-lifecycle feature, "Lifecycle
//! Telemetry Gauges"; PRD section 5.16).
//!
//! The three gauges are label-free counts of active Quotas: `cap = 0`,
//! unbounded cap, and declared on a metric the registry currently classifies
//! `Direct`. They are storage-backed: the elected replica reads the counts,
//! joins them with the current classification, and publishes one sample
//! through this sink. `None` withdraws the sample, so a stale or demoted
//! replica exports no data point rather than a wrong number. The sink's only
//! implementation is the `LifecycleGaugeCell` in `infra::lifecycle_gauges`,
//! which observable-gauge callbacks read without any I/O.

use toolkit_macros::domain_model;

/// One sample of the three lifecycle gauges.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LifecycleCounts {
    /// Active Quotas with `cap = 0` (`quota_cap_zero_total`).
    pub cap_zero: u64,
    /// Active Quotas with an unbounded cap (`quota_cap_unbounded_total`).
    pub cap_unbounded: u64,
    /// Active Quotas whose metric is currently classified `Direct`
    /// (`quota_for_direct_metric_total`).
    pub for_direct_metric: u64,
}

/// Where the refresh publishes its sample.
pub trait LifecycleGaugeSink: Send + Sync {
    /// Publish `counts`, or withdraw the sample with `None`.
    fn publish(&self, counts: Option<LifecycleCounts>);
}

/// A sink that drops every sample.
#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopGaugeSink;

impl LifecycleGaugeSink for NoopGaugeSink {
    fn publish(&self, _counts: Option<LifecycleCounts>) {}
}
