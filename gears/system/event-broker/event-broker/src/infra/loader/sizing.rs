//! How many events each partition's next fetch may take.
//!
//! A single `fetch_max_events` shared by every partition is the one number
//! that cannot be right: a partition whose readers consume 50k events per
//! second and one that is nearly idle draw on the same connection pool out of
//! the same shard-wide memory, so a constant either starves the first or wastes
//! residency on the second. The allocator already computes the right
//! per-partition number - runway - so this module only has to reach it, and to
//! hold the state that reaching it requires.
//!
//! Nothing here is new policy. [`RunwaySizing`] turns an exogenous consumption
//! rate into a desired runway, [`ShardBudget`] fits those desires into the
//! shard's soft limit, and this module is the adapter between the loader's view
//! of a partition and those two. Keeping it free of its own arithmetic is what
//! keeps the control loop testable in one place.
//!
//! The granted runway is a **residency** target, not a fetch size: it says how
//! much a partition should hold ahead of its slowest reader. So the fetch is the
//! grant minus what is already held ahead of that reader. Using the grant
//! directly would re-fetch what is already resident on every round, and grow
//! more wasteful the warmer the partition got - the opposite of the intended
//! behaviour, since a warm partition is precisely one that needs less.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

use crate::domain::model::Sequence;
use crate::domain::streaming::source::PartitionKey;

use crate::infra::partition_cache::budget::{EstimatedBytesPerEvent, SegmentDemand, ShardBudget};
use crate::infra::partition_cache::runway::{EventsPerSecond, RunwayPolicy, RunwaySizing};

/// One partition's state as the sizer sees it.
///
/// A snapshot taken by the caller, never a handle: the sizer must not be able
/// to reach into a cache or a lock, because that is what lets the whole control
/// loop be simulated in a unit test.
#[derive(Debug, Clone)]
pub struct PartitionObservation {
    key: PartitionKey,
    readers: usize,
    consumption_rate: EventsPerSecond,
    scanning: bool,
    demand_from: Sequence,
    /// Events already resident ahead of the slowest reader.
    ///
    /// Subtracted from the granted runway, because the grant describes what the
    /// partition should *hold*, not what it should *fetch*.
    resident_ahead: usize,
    bytes_per_event: EstimatedBytesPerEvent,
}

impl PartitionObservation {
    /// The key is the one value with no sensible default, so it is the
    /// builder's only argument.
    #[must_use]
    pub fn builder(key: PartitionKey) -> PartitionObservationBuilder {
        PartitionObservationBuilder {
            key,
            readers: 0,
            consumption_rate: EventsPerSecond(0),
            scanning: false,
            demand_from: Sequence::assigned(0),
            resident_ahead: 0,
            bytes_per_event: EstimatedBytesPerEvent::cold(),
        }
    }

    #[must_use]
    pub fn key(&self) -> &PartitionKey {
        &self.key
    }

    /// How many readers one fetch here would serve. This is what decides who
    /// yields when the shard budget binds, so an observation that undercounts
    /// readers costs the partition runway.
    #[must_use]
    pub fn readers(&self) -> usize {
        self.readers
    }

    #[must_use]
    pub fn consumption_rate(&self) -> EventsPerSecond {
        self.consumption_rate
    }

    #[must_use]
    pub fn scanning(&self) -> bool {
        self.scanning
    }

    /// The first sequence the next fetch would return. Carried through to the
    /// grant so the allocator's output identifies a span rather than only a
    /// partition.
    #[must_use]
    pub fn demand_from(&self) -> Sequence {
        self.demand_from
    }

    #[must_use]
    pub fn resident_ahead(&self) -> usize {
        self.resident_ahead
    }

    #[must_use]
    pub fn bytes_per_event(&self) -> EstimatedBytesPerEvent {
        self.bytes_per_event
    }
}

/// Fields are private on [`PartitionObservation`], so this is the only way to
/// build one - `readers` and `demand_from` are both bare integers, which a
/// struct literal would let a caller transpose silently.
#[derive(Debug, Clone)]
pub struct PartitionObservationBuilder {
    key: PartitionKey,
    readers: usize,
    consumption_rate: EventsPerSecond,
    scanning: bool,
    demand_from: Sequence,
    resident_ahead: usize,
    bytes_per_event: EstimatedBytesPerEvent,
}

impl PartitionObservationBuilder {
    #[must_use]
    pub fn readers(mut self, readers: usize) -> Self {
        self.readers = readers;
        self
    }

    #[must_use]
    pub fn consumption_rate(mut self, rate: EventsPerSecond) -> Self {
        self.consumption_rate = rate;
        self
    }

    #[must_use]
    pub fn scanning(mut self, scanning: bool) -> Self {
        self.scanning = scanning;
        self
    }

    #[must_use]
    pub fn demand_from(mut self, from: Sequence) -> Self {
        self.demand_from = from;
        self
    }

    /// Events already held ahead of the slowest reader.
    #[must_use]
    pub fn resident_ahead(mut self, events: usize) -> Self {
        self.resident_ahead = events;
        self
    }

    #[must_use]
    pub fn bytes_per_event(mut self, estimate: EstimatedBytesPerEvent) -> Self {
        self.bytes_per_event = estimate;
        self
    }

    #[must_use]
    pub fn build(self) -> PartitionObservation {
        PartitionObservation {
            key: self.key,
            readers: self.readers,
            consumption_rate: self.consumption_rate,
            scanning: self.scanning,
            demand_from: self.demand_from,
            resident_ahead: self.resident_ahead,
            bytes_per_event: self.bytes_per_event,
        }
    }
}

/// Turns observations into fetch sizes, and owns the damping state that doing
/// so requires.
pub struct FetchSizer {
    budget: ShardBudget,
    policy: RunwayPolicy,
    /// Per-partition, because [`RunwaySizing`] carries the smoothed latency and
    /// the previous target, and both are meaningless across partitions - one
    /// partition's latency spike must not step-limit another's target.
    sizing: HashMap<PartitionKey, RunwaySizing>,
}

impl FetchSizer {
    /// Two arguments, but of unmistakably different types, so no caller can
    /// transpose them.
    #[must_use]
    pub fn new(budget: ShardBudget, policy: RunwayPolicy) -> Self {
        Self {
            budget,
            policy,
            sizing: HashMap::new(),
        }
    }

    /// How many partitions currently carry damping state. Exposed for
    /// observability and for the tests that pin the state to the observed set.
    #[must_use]
    pub fn tracked_partitions(&self) -> usize {
        self.sizing.len()
    }

    /// How many events each partition's next fetch may take.
    ///
    /// A partition absent from the result, or present with `0`, must not be
    /// fetched this round: `0` is the allocator releasing it under pressure,
    /// and a released segment is re-read before its reader advances, so
    /// skipping it costs latency and never events.
    ///
    /// `refill_latency` is one shard-wide observation rather than a per-
    /// partition one because it measures the loader's queueing, and queueing is
    /// shared: the connection pool is per instance, so a partition's refill
    /// waits behind every other partition's.
    #[must_use]
    pub fn size(
        &mut self,
        observed: &[PartitionObservation],
        refill_latency: Duration,
    ) -> HashMap<PartitionKey, usize> {
        // Dropped before anything is sized, so a partition that has been
        // retired stops paying for its history immediately. Left to grow, this
        // map would retain an entry per partition the instance has ever served.
        self.forget_unobserved(observed);

        let demand = self.demand_for(observed, refill_latency);
        let allocation = self.budget.allocate(&demand, &self.policy);
        let resident: HashMap<&PartitionKey, usize> = observed
            .iter()
            .map(|partition| (partition.key(), partition.resident_ahead()))
            .collect();

        allocation
            .grants()
            .iter()
            .map(|grant| {
                // Spelled out rather than passing `runway_events` through: the
                // two happen to coincide today, and a reader of this line
                // should not have to know that to see that a release means no
                // fetch.
                let target = if grant.is_released() {
                    0
                } else {
                    grant.runway_events()
                };
                // The grant says how much to *hold*; what is already held ahead
                // of the slowest reader is not worth fetching again. Saturating,
                // so a partition holding more than its grant asks for nothing
                // rather than wrapping into an enormous fetch.
                let events =
                    target.saturating_sub(resident.get(grant.key()).copied().unwrap_or_default());
                (grant.key().clone(), events)
            })
            .collect()
    }

    fn forget_unobserved(&mut self, observed: &[PartitionObservation]) {
        let live: HashSet<&PartitionKey> = observed.iter().map(PartitionObservation::key).collect();
        self.sizing.retain(|key, _| live.contains(key));
    }

    /// One demand per distinct partition.
    ///
    /// Repeats are dropped rather than summed: the result is keyed by
    /// partition, so a second demand for the same key could only overwrite the
    /// first, while still having charged its bytes against the shard budget and
    /// depressed every other partition's share.
    fn demand_for(
        &mut self,
        observed: &[PartitionObservation],
        refill_latency: Duration,
    ) -> Vec<SegmentDemand> {
        let mut seen: HashSet<PartitionKey> = HashSet::with_capacity(observed.len());
        let mut demand = Vec::with_capacity(observed.len());

        for partition in observed {
            if !seen.insert(partition.key().clone()) {
                continue;
            }

            let desired =
                Self::desired_runway(&mut self.sizing, &self.policy, partition, refill_latency);

            demand.push(
                SegmentDemand::builder(partition.key().clone())
                    .segment_from(partition.demand_from())
                    .readers(partition.readers())
                    .desired_runway(desired)
                    .estimated_bytes_per_event(partition.bytes_per_event())
                    .build(),
            );
        }

        demand
    }

    /// Associated rather than a method, so the borrow of `sizing` does not
    /// conflict with the borrow of `policy` in the caller's loop.
    fn desired_runway(
        sizing: &mut HashMap<PartitionKey, RunwaySizing>,
        policy: &RunwayPolicy,
        partition: &PartitionObservation,
        refill_latency: Duration,
    ) -> usize {
        // A first observation seeds the state from the latency actually
        // observed and from the floor. Seeding the target at zero would let the
        // first recomputation jump straight to the bandwidth-delay product of a
        // single latency sample, which is the least trustworthy sample there
        // is; starting at the floor makes a new partition ramp instead.
        sizing
            .entry(partition.key().clone())
            .or_insert_with(|| RunwaySizing::new(refill_latency, policy.floor_events))
            .next_target(
                policy,
                partition.consumption_rate(),
                refill_latency,
                partition.scanning(),
            )
    }
}
