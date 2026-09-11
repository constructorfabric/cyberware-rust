//! The shard's loader task: what actually drives fetching and reclamation.
//!
//! Without this nothing calls `DemandScheduler::run_round` or
//! `PartitionCache::reclaim`, so a session's caches stay empty and every read
//! reports the position unaccounted for. One task per instance serves every
//! group on it (D10).
//!
//! Reclamation is driven here rather than inside `run_round` deliberately.
//! `run_round` is a fetch pass and is skipped entirely when no partition wants
//! anything; reclamation must still run then, because a partition that has gone
//! quiet holding a full residency is exactly the case that needs freeing.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::scheduler::DemandScheduler;
use super::source::EventSource;
use super::topics::TopicManager;

/// How often the loader wakes when nothing has notified it.
///
/// A floor on latency rather than the mechanism for it: the notification seam
/// (`domain::notify`) is what makes delivery prompt. This tick is what keeps the
/// broker correct when a notification is lost, arrives before the backend
/// assigned the sequence, or never comes because the events were published
/// before the session opened.
pub const DEFAULT_TICK: Duration = Duration::from_millis(50);

pub struct ShardLoader<S> {
    scheduler: Arc<DemandScheduler<S>>,
    topics: Arc<TopicManager>,
    tick: Duration,
}

impl<S: EventSource + Send + Sync + 'static> ShardLoader<S> {
    /// Three arguments, all mutually distinguishable.
    #[must_use]
    pub fn new(
        scheduler: Arc<DemandScheduler<S>>,
        topics: Arc<TopicManager>,
        tick: Duration,
    ) -> Self {
        Self {
            scheduler,
            topics,
            tick,
        }
    }

    /// Runs until cancelled. One pass is a fetch round followed by a
    /// maintenance sweep.
    pub async fn run(self, shutdown: CancellationToken) {
        loop {
            if shutdown.is_cancelled() {
                return;
            }

            let report = self.scheduler.run_round().await;
            let freed = self.maintain();

            if report.fetches_issued() > 0 || freed > 0 {
                tracing::trace!(
                    fetches = report.fetches_issued(),
                    events = report.events_fetched(),
                    served = report.readers_served(),
                    failures = report.failures(),
                    bytes_freed = freed,
                    "loader round"
                );
            }

            // A round that fetched something is likely to find more immediately,
            // so only an idle round pays the tick.
            if report.fetches_issued() == 0 {
                tokio::select! {
                    () = tokio::time::sleep(self.tick) => {}
                    () = shutdown.cancelled() => return,
                }
            }
        }
    }

    /// Reclaims across every live partition. Returns bytes freed, summed over
    /// all three reasons - dead spans, gaps between reader clusters, and byte
    /// pressure.
    fn maintain(&self) -> u64 {
        self.topics
            .live()
            .iter()
            .map(|partition| {
                let report = partition.cache().reclaim();
                report
                    .dead()
                    .bytes()
                    .saturating_add(report.gapped().bytes())
                    .saturating_add(report.pressured().bytes())
            })
            .sum()
    }

    /// Spawns [`Self::run`]. The handle is returned so a shutdown can join it
    /// rather than leaving a fetch in flight against a closing pool.
    #[must_use]
    pub fn spawn(self, shutdown: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(self.run(shutdown))
    }
}
