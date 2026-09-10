//! Storage-backed refresh of the lifecycle gauges (`features/quota-lifecycle.md`,
//! "Lifecycle Telemetry Gauges").
//!
//! The elected replica runs [`LifecycleGaugeRefresher::run`] as leader work.
//! Every `refresh` interval it reads the active-Quota counts from storage,
//! classifies each metric through the registry, and publishes one sample to
//! the sink; the observable gauges read that sample without any I/O. A sample
//! is published only after a refresh that succeeded in the current leadership
//! term. Failures keep the last sample until `stale_after` has passed without a
//! success, then withdraw it; a stale classification counts as a failure, so
//! it never renews the sample. Cancellation, an abort, or a drop of the future
//! withdraws the sample through a guard, and a refresh that completes after
//! cancellation is discarded.

use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use quota_enforcement_sdk::{MetricId, QuotaEnforcementStoragePluginV1};
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::ports::coordination::{LeaderWork, LeaderWorkFuture};
use crate::domain::ports::lifecycle_gauges::{LifecycleCounts, LifecycleGaugeSink};
use crate::domain::ports::metric_registry::{Classified, Freshness, MetricMode, MetricRegistry};

const LOG_TARGET: &str = "qe.lifecycle_gauges";

/// Timing of the refresh.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GaugeTiming {
    /// Interval between two refreshes.
    pub refresh: Duration,
    /// Budget of one refresh, storage read and classification together.
    pub refresh_deadline: Duration,
    /// Time without a successful refresh after which the sample is withdrawn.
    pub stale_after: Duration,
}

/// Why one refresh produced no sample.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefreshError {
    /// Storage did not answer.
    #[error("storage read failed: {0}")]
    Storage(DomainError),
    /// The registry did not answer or rejected a metric.
    #[error("metric classification failed: {0}")]
    Registry(DomainError),
    /// A classification came from the cache after a registry failure. Good
    /// enough for a write, not for a fresh sample.
    #[error("classification of {metric} is stale")]
    StaleClassification {
        /// The metric.
        metric: String,
    },
    /// The refresh exceeded its deadline.
    #[error("refresh exceeded {0:?}")]
    TimedOut(Duration),
    /// Leadership or the process ended mid-refresh.
    #[error("refresh cancelled")]
    Cancelled,
}

/// The refresh, one instance per process.
#[domain_model]
pub struct LifecycleGaugeRefresher {
    storage: Arc<dyn QuotaEnforcementStoragePluginV1>,
    metric_registry: Arc<dyn MetricRegistry>,
    sink: Arc<dyn LifecycleGaugeSink>,
    timing: GaugeTiming,
}

type Refresh = Pin<Box<dyn Future<Output = Result<LifecycleCounts, RefreshError>> + Send>>;
type Timer = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Withdraws the sample when the refresh loop ends for any reason: return,
/// cancellation, or a drop of the future by an abort.
struct Withdraw<'a>(&'a dyn LifecycleGaugeSink);

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.0.publish(None);
    }
}

fn never() -> Timer {
    Box::pin(pending())
}

impl LifecycleGaugeRefresher {
    /// Assemble the refresh.
    #[must_use]
    pub fn new(
        storage: Arc<dyn QuotaEnforcementStoragePluginV1>,
        metric_registry: Arc<dyn MetricRegistry>,
        sink: Arc<dyn LifecycleGaugeSink>,
        timing: GaugeTiming,
    ) -> Arc<Self> {
        Arc::new(Self {
            storage,
            metric_registry,
            sink,
            timing,
        })
    }

    /// One refresh: the storage read and one classification per metric,
    /// bounded by the deadline and raced against `cancel`. Publishes nothing.
    ///
    /// # Errors
    ///
    /// The [`RefreshError`] that prevented a fresh sample.
    pub async fn refresh_once(
        &self,
        cancel: &CancellationToken,
    ) -> Result<LifecycleCounts, RefreshError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(RefreshError::Cancelled),
            outcome = tokio::time::timeout(self.timing.refresh_deadline, self.collect()) => {
                outcome.map_err(|_elapsed| RefreshError::TimedOut(self.timing.refresh_deadline))?
            }
        }
    }

    async fn collect(&self) -> Result<LifecycleCounts, RefreshError> {
        let counts = self
            .storage
            .read_active_quota_counts()
            .await
            .map_err(|e| RefreshError::Storage(DomainError::from(e)))?;
        let mut for_direct_metric = 0u64;
        for (metric, active) in &counts.by_metric {
            match self.metric_registry.describe(metric).await {
                Ok(Some(Classified {
                    descriptor,
                    freshness: Freshness::Fresh,
                })) => {
                    if descriptor.mode == MetricMode::Direct {
                        for_direct_metric = for_direct_metric.saturating_add(*active);
                    }
                }
                Ok(Some(Classified {
                    freshness: Freshness::Stale,
                    ..
                })) => {
                    return Err(RefreshError::StaleClassification {
                        metric: metric.as_str().to_owned(),
                    });
                }
                Ok(None) => warn_removed(metric, *active),
                Err(err) => return Err(RefreshError::Registry(err)),
            }
        }
        Ok(LifecycleCounts {
            cap_zero: counts.cap_zero,
            cap_unbounded: counts.cap_unbounded,
            for_direct_metric,
        })
    }

    /// The leader body: refresh on a schedule until `cancel` fires. Expiry,
    /// the schedule, an in-flight refresh, and cancellation are all live in
    /// every phase; the sample is withdrawn on exit however the loop ends.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let _withdraw = Withdraw(self.sink.as_ref());
        let mut refreshing = false;
        let mut in_flight: Refresh = Box::pin(pending());
        let mut expiry: Timer = never();
        let mut tick: Timer = Box::pin(tokio::time::sleep(Duration::ZERO));
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = &mut expiry => {
                    expiry = self.withdraw_stale();
                }
                outcome = &mut in_flight, if refreshing => {
                    refreshing = false;
                    in_flight = Box::pin(pending());
                    if let Some(rearmed) = self.absorb(outcome) {
                        expiry = rearmed;
                    }
                    tick = Box::pin(tokio::time::sleep(self.timing.refresh));
                }
                () = &mut tick, if !refreshing => {
                    refreshing = true;
                    let this = Arc::clone(&self);
                    let cancel = cancel.clone();
                    in_flight = Box::pin(async move { this.refresh_once(&cancel).await });
                    tick = never();
                }
            }
        }
    }

    /// No success for `stale_after`: withdraw the sample and disarm the timer,
    /// so an elapsed timer can never fire again or cancel a recovery refresh.
    fn withdraw_stale(&self) -> Timer {
        self.sink.publish(None);
        tracing::warn!(
            target: LOG_TARGET,
            stale_after = ?self.timing.stale_after,
            "lifecycle gauge sample withdrawn: no successful refresh"
        );
        never()
    }

    /// Publish a successful refresh and re-arm expiry; keep the last sample on
    /// a failure. Returns the new expiry timer when one was armed.
    fn absorb(&self, outcome: Result<LifecycleCounts, RefreshError>) -> Option<Timer> {
        match outcome {
            Ok(counts) => {
                self.sink.publish(Some(counts));
                Some(Box::pin(tokio::time::sleep(self.timing.stale_after)))
            }
            Err(err) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    error = %err,
                    "lifecycle gauge refresh failed; keeping the last sample"
                );
                None
            }
        }
    }

    /// The body as the coordinator runs it.
    #[must_use]
    pub fn leader_work(self: Arc<Self>) -> LeaderWork {
        Arc::new(move |cancel: CancellationToken| -> LeaderWorkFuture {
            let this = Arc::clone(&self);
            Box::pin(this.run(cancel))
        })
    }
}

/// The runtime half of "a persisted Quota whose metric was later removed is
/// flagged, never deactivated": every refresh names such metrics.
fn warn_removed(metric: &MetricId, active: u64) {
    tracing::warn!(
        target: LOG_TARGET,
        metric = %metric,
        active_quotas = active,
        "active Quotas reference a metric the types registry no longer knows"
    );
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gauges_tests.rs"]
mod gauges_tests;
