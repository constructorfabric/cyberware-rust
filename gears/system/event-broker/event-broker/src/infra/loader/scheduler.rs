//! Deciding which partition gets the next connection.
//!
//! Demands are derived per partition rather than enqueued per reader, so a round
//! costs one scan per partition however many readers there are. That is where
//! coalescing comes from: a thousand readers clustered at one partition's tail
//! produce one demand, and therefore one fetch.
//!
//! Three rules, each earning its place. **Round-robin over partitions**, never
//! priority by demand size - ranking partitions against each other by how much
//! they want is precisely how a noisy partition starves the rest. **In-flight
//! suppression per partition**, or every worker in the pool piles onto the same
//! hungry partition and issues the same fetch, reintroducing the uncoalesced
//! behaviour from the scheduler instead of from the readers. And **the tail
//! poller's backoff gates tail demands only**, because a backfill covers
//! sequences the backend certainly holds.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::domain::model::Sequence;
use crate::infra::partition_cache::accounting::account_for_fetch;
use crate::infra::partition_cache::cache::AbsorbedFetch;
use crate::infra::partition_cache::demand::{Demand, StarvationWeight, rank};
use crate::infra::partition_cache::segment::Segment;

use super::source::EventSource;
use super::topics::{Partition, TopicManager, TopicPolicy};

/// How the loader paces itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerPolicy {
    pool_permits: usize,
    starvation_weight: StarvationWeight,
    idle_rounds_before_retire: u64,
}

impl SchedulerPolicy {
    /// One argument, the thing that actually bounds the loader.
    #[must_use]
    pub fn with_pool(permits: usize) -> SchedulerPolicyBuilder {
        SchedulerPolicyBuilder {
            pool_permits: permits.max(1),
            starvation_weight: StarvationWeight::default(),
            idle_rounds_before_retire: 1024,
        }
    }

    #[must_use]
    pub fn pool_permits(self) -> usize {
        self.pool_permits
    }

    #[must_use]
    pub fn starvation_weight(self) -> StarvationWeight {
        self.starvation_weight
    }

    #[must_use]
    pub fn idle_rounds_before_retire(self) -> u64 {
        self.idle_rounds_before_retire
    }
}

impl Default for SchedulerPolicy {
    /// Sixteen connections, which is the per-instance pool the design is sized
    /// against.
    fn default() -> Self {
        Self::with_pool(16).build()
    }
}

pub struct SchedulerPolicyBuilder {
    pool_permits: usize,
    starvation_weight: StarvationWeight,
    idle_rounds_before_retire: u64,
}

impl SchedulerPolicyBuilder {
    #[must_use]
    pub fn starvation_weight(mut self, weight: StarvationWeight) -> Self {
        self.starvation_weight = weight;
        self
    }

    #[must_use]
    pub fn idle_rounds_before_retire(mut self, rounds: u64) -> Self {
        self.idle_rounds_before_retire = rounds;
        self
    }

    #[must_use]
    pub fn build(self) -> SchedulerPolicy {
        SchedulerPolicy {
            pool_permits: self.pool_permits,
            starvation_weight: self.starvation_weight,
            idle_rounds_before_retire: self.idle_rounds_before_retire,
        }
    }
}

/// What one round did.
///
/// `readers_served` against `fetches_issued` is the fan-out ratio, which is the
/// claim the architecture rests on. Kept as two counts rather than a quotient so
/// a caller can compare them by multiplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundReport {
    partitions_scanned: usize,
    fetches_issued: usize,
    readers_served: usize,
    events_fetched: usize,
    empty_fetches: usize,
    deferred_by_backoff: usize,
    suppressed_in_flight: usize,
    failures: usize,
}

impl RoundReport {
    #[must_use]
    pub fn partitions_scanned(self) -> usize {
        self.partitions_scanned
    }

    #[must_use]
    pub fn fetches_issued(self) -> usize {
        self.fetches_issued
    }

    /// Readers the issued fetches will answer, summed across them.
    #[must_use]
    pub fn readers_served(self) -> usize {
        self.readers_served
    }

    #[must_use]
    pub fn events_fetched(self) -> usize {
        self.events_fetched
    }

    /// Fetches that came back with nothing. Expected, not an error: a
    /// notification can precede the sequence being assigned.
    #[must_use]
    pub fn empty_fetches(self) -> usize {
        self.empty_fetches
    }

    #[must_use]
    pub fn deferred_by_backoff(self) -> usize {
        self.deferred_by_backoff
    }

    #[must_use]
    pub fn suppressed_in_flight(self) -> usize {
        self.suppressed_in_flight
    }

    #[must_use]
    pub fn failures(self) -> usize {
        self.failures
    }
}

/// What one fetch turned out to be.
enum Outcome {
    Fetched(usize),
    Empty,
    Failed,
}

pub struct DemandScheduler<S> {
    source: Arc<S>,
    topics: Arc<TopicManager>,
    policy: SchedulerPolicy,
    permits: Arc<Semaphore>,
    /// Where the next round starts, so no partition is permanently first.
    cursor: AtomicUsize,
    round: AtomicU64,
}

impl<S: EventSource + 'static> DemandScheduler<S> {
    #[must_use]
    pub fn new(source: Arc<S>, topics: Arc<TopicManager>, policy: SchedulerPolicy) -> Self {
        let permits = Arc::new(Semaphore::new(policy.pool_permits()));
        Self {
            source,
            topics,
            policy,
            permits,
            cursor: AtomicUsize::new(0),
            round: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn topics(&self) -> &Arc<TopicManager> {
        &self.topics
    }

    #[must_use]
    pub fn round(&self) -> u64 {
        self.round.load(Ordering::Relaxed)
    }

    /// Scans every live partition once and issues at most one fetch each.
    ///
    /// At most one per partition per round on purpose: a partition wanting three
    /// spans should not take three of sixteen connections while fifteen other
    /// partitions wait. Its other demands survive to the next round, and their
    /// readers accrue starvation credit meanwhile.
    pub async fn run_round(&self) -> RoundReport {
        let round = self.round.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let policy = self.topics.policy();
        let mut live = self.topics.live();
        if live.is_empty() {
            return RoundReport::default();
        }

        // Rotate the starting point so the same partition is not always served
        // first when the pool is the binding constraint.
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % live.len();
        live.rotate_left(start);

        let now = Instant::now();
        let mut report = RoundReport {
            partitions_scanned: live.len(),
            ..RoundReport::default()
        };
        let mut fetches: JoinSet<Outcome> = JoinSet::new();

        for partition in live {
            if let Some(chosen) = self.select(&partition, now, round, &mut report) {
                report.fetches_issued += 1;
                report.readers_served += chosen.readers_behind();
                fetches.spawn(Self::fetch(
                    Arc::clone(&self.source),
                    Arc::clone(&partition),
                    chosen,
                    policy,
                    Arc::clone(&self.permits),
                ));
            }
        }

        while let Some(joined) = fetches.join_next().await {
            match joined {
                Ok(Outcome::Fetched(events)) => report.events_fetched += events,
                Ok(Outcome::Empty) => report.empty_fetches += 1,
                Ok(Outcome::Failed) | Err(_) => report.failures += 1,
            }
        }

        self.topics
            .retire_idle(round, self.policy.idle_rounds_before_retire());
        report
    }

    /// The one demand this partition should have served, if any.
    fn select(
        &self,
        partition: &Arc<Partition>,
        now: Instant,
        round: u64,
        report: &mut RoundReport,
    ) -> Option<Demand> {
        if partition.is_claimed() {
            report.suppressed_in_flight += 1;
            return None;
        }

        let demands = partition.cache().scan_demands();
        if demands.is_empty() {
            return None;
        }

        let ready = partition.poll().is_ready(now);
        let mut eligible: Vec<Demand> = demands
            .into_iter()
            .filter(|demand| ready || !demand.defers_to_backoff())
            .collect();
        if eligible.is_empty() {
            report.deferred_by_backoff += 1;
            return None;
        }

        rank(&mut eligible, self.policy.starvation_weight());
        let chosen = eligible.first().copied()?;

        // Claimed only once there is something to do with it, so a partition
        // with nothing to fetch never blocks itself.
        if !partition.claim() {
            report.suppressed_in_flight += 1;
            return None;
        }
        partition.touch(round);
        Some(chosen)
    }

    /// Reads one span and records it. Holds a pool permit for the read only.
    async fn fetch(
        source: Arc<S>,
        partition: Arc<Partition>,
        chosen: Demand,
        policy: TopicPolicy,
        permits: Arc<Semaphore>,
    ) -> Outcome {
        // A demand already names the position to read after, and every read is
        // exclusive of the position it names, so there is nothing to convert.
        let after = chosen.after();
        let outcome = {
            let Ok(_permit) = permits.acquire().await else {
                partition.release();
                return Outcome::Failed;
            };
            source
                .read(partition.key(), after, policy.fetch_max_events())
                .await
        };

        let result = match outcome {
            Ok(events) if events.is_empty() => {
                // Not an error, and not proof of anything: the sequence may not
                // have been assigned yet. Ask again, later.
                partition
                    .poll()
                    .found_nothing(Instant::now(), policy.poll());
                Outcome::Empty
            }
            Ok(events) => {
                let count = events.len();
                Self::absorb(&partition, after, events, policy);
                partition.poll().found_events(policy.poll());
                Outcome::Fetched(count)
            }
            Err(_) => Outcome::Failed,
        };

        partition.release();
        result
    }

    /// Records what a fetch returned, and the span it thereby accounted for.
    ///
    /// The accounted span starts where the fetch was aimed, not at the first
    /// event returned: a fetch that skipped over deleted sequences has proven
    /// them absent, and a reader may step over them.
    fn absorb(
        partition: &Arc<Partition>,
        after: Sequence,
        events: Vec<crate::domain::model::Event>,
        policy: TopicPolicy,
    ) {
        let sequences: Vec<Sequence> = events.iter().filter_map(|event| event.sequence).collect();
        let accounting = account_for_fetch(after, &sequences, policy.fetch_max_events());

        let segment = Segment::builder()
            .starting_after(after)
            .through(accounting.accounted_through())
            .events(events)
            .build();
        partition
            .cache()
            .absorb(AbsorbedFetch::builder(segment).build());
    }
}
