//! `LifecycleGaugeCell`: the [`LifecycleGaugeSink`] the observable gauges
//! read from.
//!
//! The elected replica's refresh stores a sample; each gauge callback loads
//! it lock-free and observes only when a sample is present. Storage and
//! registry I/O stay in the refresh, never in a callback.

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::domain::ports::lifecycle_gauges::{LifecycleCounts, LifecycleGaugeSink};

/// Catalogue name of the active `cap = 0` gauge.
pub const QUOTA_CAP_ZERO_TOTAL: &str = "quota_cap_zero_total";

/// Catalogue name of the active unbounded-cap gauge.
pub const QUOTA_CAP_UNBOUNDED_TOTAL: &str = "quota_cap_unbounded_total";

/// Catalogue name of the active Quotas-on-`Direct`-metrics gauge.
pub const QUOTA_FOR_DIRECT_METRIC_TOTAL: &str = "quota_for_direct_metric_total";

/// The published sample, or nothing.
#[derive(Debug, Default)]
pub struct LifecycleGaugeCell(ArcSwapOption<LifecycleCounts>);

impl LifecycleGaugeCell {
    /// The current sample, if one is published.
    #[must_use]
    pub fn load(&self) -> Option<LifecycleCounts> {
        self.0.load_full().map(|counts| *counts)
    }
}

impl LifecycleGaugeSink for LifecycleGaugeCell {
    fn publish(&self, counts: Option<LifecycleCounts>) {
        self.0.store(counts.map(Arc::new));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "lifecycle_gauges_tests.rs"]
mod lifecycle_gauges_tests;
