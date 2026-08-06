//! Drives each configured topic's backend through one retention pass per tick.
//!
//! The removal itself belongs to the backend that owns the rows; this only
//! decides when a pass happens and what bounds it must end within. That split
//! is what lets a second backend bring its own enforcement rather than
//! inheriting the first one's, and what keeps a plugin free of timers and
//! spawned tasks.
//!
//! The same shape `ShardLoader::run` already uses for cache reclamation: a
//! driven pass on a paced loop, so a test forces exactly as many passes as it
//! wants and knows exactly that many happened.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use event_broker_sdk::RetentionRequest;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

use crate::config::EventBrokerConfig;
use crate::domain::backend::BackendResolver;
use crate::domain::model::Topic;
use crate::domain::resolution::EffectiveSettings;
use crate::domain::specification::SpecificationManager;

/// What one sweep across every topic did.
///
/// Counted rather than derived, like the per-partition figures underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Partitions a pass actually ran against.
    pub passes: u64,
    /// Events those passes removed, summed.
    pub removed_events: u64,
    /// Stored bytes those passes removed, summed.
    pub removed_bytes: u64,
    /// Passes that failed. A failed pass removes nothing, so the next sweep has
    /// the same work to do.
    pub failures: u64,
}

/// What a sweep does with one topic, and why.
enum TopicBounds {
    /// Resolution gives the topic settings, and they express a usable cutoff.
    Enforce {
        settings: EffectiveSettings,
        oldest_permitted: DateTime<Utc>,
    },
    /// Configuration carries settings whose duration cannot be subtracted from
    /// the clock at all. Skipped loudly rather than silently clamped.
    Unenforceable,
}

pub struct RetentionWorker {
    specs: Arc<dyn SpecificationManager>,
    backends: Arc<dyn BackendResolver>,
    config: EventBrokerConfig,
    tick: Duration,
}

impl RetentionWorker {
    /// Four arguments, all of mutually distinguishable types, so none can be
    /// passed in another's place - the same terms `ShardLoader::new` is
    /// constructed on.
    #[must_use]
    pub fn new(
        specs: Arc<dyn SpecificationManager>,
        backends: Arc<dyn BackendResolver>,
        config: EventBrokerConfig,
        tick: Duration,
    ) -> Self {
        Self {
            specs,
            backends,
            config,
            tick,
        }
    }

    /// One sweep: every topic, every partition its settings give it, one pass
    /// each.
    ///
    /// A pass that fails is counted and the sweep continues. One partition's
    /// backend being briefly unhappy is not a reason to leave every other
    /// partition on the instance unbounded until the next tick.
    pub async fn run_once(&self) -> SweepReport {
        // One instant for the whole sweep, so two partitions of the same topic
        // are held to the same cutoff no matter how long the sweep takes.
        let now = Utc::now();
        let mut report = SweepReport::default();

        for topic in self.specs.list_topics().await {
            match self.bounds_for(&topic, now) {
                TopicBounds::Enforce {
                    settings,
                    oldest_permitted,
                } => {
                    self.sweep_topic(&topic, &settings, oldest_permitted, &mut report)
                        .await;
                }
                TopicBounds::Unenforceable => {}
            }
        }

        if report.removed_events > 0 || report.failures > 0 {
            tracing::debug!(
                passes = report.passes,
                removed_events = report.removed_events,
                removed_bytes = report.removed_bytes,
                failures = report.failures,
                "retention sweep"
            );
        }
        report
    }

    /// What one topic is held to this sweep.
    fn bounds_for(&self, topic: &Topic, now: DateTime<Utc>) -> TopicBounds {
        let settings = match crate::domain::resolution::resolve(
            &self.config,
            &topic.id,
            // What the topic itself declares does not reach the ladder yet: the
            // cached specification still carries its retention as an unparsed
            // string. It joins when the cache holds the projection instead.
            &crate::domain::resolution::Declaration::default(),
        ) {
            Ok(settings) => settings,
            Err(err) => {
                // Every remaining variant is an operator mistake fixed in the
                // configuration file, so it says the same thing on every tick;
                // one topic's is not a reason to leave the others unbounded.
                tracing::warn!(topic = %topic.id, %err, "settings do not resolve; skipping");
                return TopicBounds::Unenforceable;
            }
        };
        let Some(oldest_permitted) = cutoff(now, settings.retention().value().duration) else {
            tracing::warn!(
                topic = %topic.id,
                "configured retention duration is too large to express as a cutoff; skipping"
            );
            return TopicBounds::Unenforceable;
        };
        TopicBounds::Enforce {
            settings,
            oldest_permitted,
        }
    }

    /// One topic, every partition its settings give it.
    ///
    /// Split out of [`Self::run_once`] so the "which topics are in scope"
    /// decision and the "what does one topic's pass look like" mechanics do not
    /// have to be read together.
    async fn sweep_topic(
        &self,
        topic: &Topic,
        settings: &EffectiveSettings,
        oldest_permitted: DateTime<Utc>,
        report: &mut SweepReport,
    ) {
        let backend = self.backends.resolve(topic);
        let topic_id = topic.id.to_string();
        for partition in 0..settings.partitions().value().max(&0).cast_unsigned() {
            let mut request =
                RetentionRequest::for_partition(&topic_id, partition, oldest_permitted);
            if let Some(size_bytes) = settings.retention().value().size_bytes {
                request = request.max_stored_bytes(size_bytes);
            }
            match backend
                .maintain(&SecurityContext::anonymous(), &request.build())
                .await
            {
                Ok(pass) => {
                    report.passes += 1;
                    report.removed_events += pass.removed_events;
                    report.removed_bytes += pass.removed_bytes;
                }
                Err(e) => {
                    report.failures += 1;
                    tracing::warn!(
                        topic = %topic_id,
                        partition,
                        error = %e,
                        "retention pass failed; the next sweep has the same work to do"
                    );
                }
            }
        }
    }

    /// Runs until cancelled, one sweep per tick.
    ///
    /// Paced unconditionally, unlike the loader's "a round that fetched
    /// something looks again immediately": there is no urgency here. A
    /// partition that just went past its bound can wait one tick, and sweeping
    /// again straight away would only re-scan partitions that were within their
    /// bounds a moment ago.
    pub async fn run(self, shutdown: CancellationToken) {
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            self.run_once().await;
            tokio::select! {
                () = tokio::time::sleep(self.tick) => {}
                () = shutdown.cancelled() => return,
            }
        }
    }

    /// Spawns [`Self::run`]. The handle is returned so a shutdown can join it
    /// rather than leaving a pass in flight against a closing pool.
    #[must_use]
    pub fn spawn(self, shutdown: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(self.run(shutdown))
    }
}

/// The instant a partition's oldest permitted event was stamped at.
///
/// `None` when the configured duration cannot be subtracted from the clock at
/// all - a bound so far in the past that enforcing it would be meaningless, and
/// better skipped loudly than silently clamped to "remove everything".
fn cutoff(now: DateTime<Utc>, duration: Duration) -> Option<DateTime<Utc>> {
    now.checked_sub_signed(TimeDelta::from_std(duration).ok()?)
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod retention_tests;
