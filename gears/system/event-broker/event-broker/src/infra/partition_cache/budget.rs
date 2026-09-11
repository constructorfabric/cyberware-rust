//! The shard's memory ceiling, spent by *allocating* runway across segments
//! rather than by reacting to overflow.
//!
//! Sizing and fitting are separate. [`super::runway::RunwaySizing`] turns one
//! segment's consumption rate into a desired runway, and is stateful because
//! damping needs history. [`ShardBudget::allocate`] fits those desires into
//! the ceiling, and is pure because fitting needs only the current demands.
//! Keeping them apart is what lets each be tested on its own.
//!
//! The allocator **targets** a soft limit; it does not enforce a ceiling. Its
//! arithmetic multiplies event counts by an *estimated* bytes-per-event, so it
//! can only aim. Actual residency is bounded by the cache, against measured
//! bytes, when it absorbs a fetch. That split is what makes an estimate
//! acceptable here: being wrong costs position within the soft-to-hard band,
//! never a breach of it.
//!
//! No step of the pressure ladder loses events: a released segment is re-read
//! before its reader advances. Only retention loses events, never memory
//! pressure.

use crate::domain::model::Sequence;
use crate::domain::streaming::source::PartitionKey;

use super::runway::RunwayPolicy;

/// An average event size, measured per partition rather than per segment.
///
/// Event size tracks the event type, which is a property of the topic, so
/// every segment of one partition draws on the same population - a per-segment
/// statistic would only halve the sample for no gain.
///
/// Clamped to `1..=`[`Self::MAX`]. Events are capped at 64 KiB combined
/// headers and payload (`docs/DESIGN.md`, "Constraints"), so a larger estimate
/// describes an event that cannot exist. Clamping errs conservative and cannot
/// make the allocator overcommit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EstimatedBytesPerEvent(usize);

impl EstimatedBytesPerEvent {
    /// The per-event hard size limit: 64 KiB.
    pub const MAX: usize = 65_536;

    /// What a partition with nothing resident is assumed to hold. The worst
    /// case, deliberately: a cold segment must not be handed runway that turns
    /// out to cost far more than assumed. A typical payload is nearer 1 KiB
    /// (`docs/PRD.md`), so this converges down quickly once anything is
    /// measured.
    #[must_use]
    pub fn cold() -> Self {
        Self(Self::MAX)
    }

    #[must_use]
    pub fn new(bytes: usize) -> Self {
        Self(bytes.clamp(1, Self::MAX))
    }

    #[must_use]
    pub fn get(self) -> usize {
        self.0
    }
}

/// What one segment is asking for, this recomputation.
///
/// Carries no shortfall measure, deliberately. Allocating on shortfall closes
/// a divergent loop through the reader's own throughput, and a type that
/// cannot express shortfall cannot accidentally use it.
#[derive(Debug, Clone)]
pub struct SegmentDemand {
    key: PartitionKey,
    segment_from: Sequence,
    readers: usize,
    desired_runway_events: usize,
    estimated_bytes_per_event: EstimatedBytesPerEvent,
}

impl SegmentDemand {
    /// The key is the one value with no sensible default, so it is the
    /// builder's only argument.
    #[must_use]
    pub fn builder(key: PartitionKey) -> SegmentDemandBuilder {
        SegmentDemandBuilder {
            key,
            segment_from: Sequence::assigned(0),
            readers: 0,
            desired_runway_events: 0,
            estimated_bytes_per_event: EstimatedBytesPerEvent::cold(),
        }
    }

    #[must_use]
    pub fn key(&self) -> &PartitionKey {
        &self.key
    }

    #[must_use]
    pub fn segment_from(&self) -> Sequence {
        self.segment_from
    }

    /// Residency serving more readers is worth more per byte, which is what
    /// decides who yields under pressure.
    #[must_use]
    pub fn readers(&self) -> usize {
        self.readers
    }

    #[must_use]
    pub fn desired_runway_events(&self) -> usize {
        self.desired_runway_events
    }

    #[must_use]
    pub fn estimated_bytes_per_event(&self) -> usize {
        self.estimated_bytes_per_event.get()
    }

    #[must_use]
    pub fn bytes_for(&self, runway_events: usize) -> usize {
        runway_events.saturating_mul(self.estimated_bytes_per_event.get())
    }
}

/// Fields are private on [`SegmentDemand`], so this is the only way to build
/// one - two of them are same-typed counts, which a struct literal would let a
/// caller transpose silently.
#[derive(Debug, Clone)]
pub struct SegmentDemandBuilder {
    key: PartitionKey,
    segment_from: Sequence,
    readers: usize,
    desired_runway_events: usize,
    estimated_bytes_per_event: EstimatedBytesPerEvent,
}

impl SegmentDemandBuilder {
    #[must_use]
    pub fn segment_from(mut self, offset: Sequence) -> Self {
        self.segment_from = offset;
        self
    }

    #[must_use]
    pub fn readers(mut self, readers: usize) -> Self {
        self.readers = readers;
        self
    }

    #[must_use]
    pub fn desired_runway(mut self, events: usize) -> Self {
        self.desired_runway_events = events;
        self
    }

    #[must_use]
    pub fn estimated_bytes_per_event(mut self, estimate: EstimatedBytesPerEvent) -> Self {
        self.estimated_bytes_per_event = estimate;
        self
    }

    #[must_use]
    pub fn build(self) -> SegmentDemand {
        SegmentDemand {
            key: self.key,
            segment_from: self.segment_from,
            readers: self.readers,
            desired_runway_events: self.desired_runway_events,
            estimated_bytes_per_event: self.estimated_bytes_per_event,
        }
    }
}

/// One segment's granted runway. `runway_events == 0` means released: hold
/// nothing, and let the reader's next read report its position unaccounted
/// for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunwayGrant {
    key: PartitionKey,
    segment_from: Sequence,
    runway_events: usize,
}

impl RunwayGrant {
    #[must_use]
    pub fn builder(key: PartitionKey) -> RunwayGrantBuilder {
        RunwayGrantBuilder {
            key,
            segment_from: Sequence::assigned(0),
            runway_events: 0,
        }
    }

    #[must_use]
    pub fn key(&self) -> &PartitionKey {
        &self.key
    }

    #[must_use]
    pub fn segment_from(&self) -> Sequence {
        self.segment_from
    }

    #[must_use]
    pub fn runway_events(&self) -> usize {
        self.runway_events
    }

    #[must_use]
    pub fn is_released(&self) -> bool {
        self.runway_events == 0
    }
}

#[derive(Debug, Clone)]
pub struct RunwayGrantBuilder {
    key: PartitionKey,
    segment_from: Sequence,
    runway_events: usize,
}

impl RunwayGrantBuilder {
    #[must_use]
    pub fn segment_from(mut self, offset: Sequence) -> Self {
        self.segment_from = offset;
        self
    }

    #[must_use]
    pub fn runway_events(mut self, events: usize) -> Self {
        self.runway_events = events;
        self
    }

    #[must_use]
    pub fn build(self) -> RunwayGrant {
        RunwayGrant {
            key: self.key,
            segment_from: self.segment_from,
            runway_events: self.runway_events,
        }
    }
}

/// The result of one recomputation: a grant per demand, in the order the
/// demands were supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    grants: Vec<RunwayGrant>,
}

impl Allocation {
    #[must_use]
    pub fn grants(&self) -> &[RunwayGrant] {
        &self.grants
    }

    #[must_use]
    pub fn runway_for(&self, key: &PartitionKey, segment_from: Sequence) -> Option<usize> {
        self.grants
            .iter()
            .find(|grant| grant.key() == key && grant.segment_from() == segment_from)
            .map(RunwayGrant::runway_events)
    }

    /// `demand` must be the same slice, in the same order, that produced this
    /// allocation - the pairing is positional, and a mismatched slice silently
    /// returns a wrong total.
    #[must_use]
    pub fn committed_bytes(&self, demand: &[SegmentDemand]) -> usize {
        self.grants
            .iter()
            .zip(demand.iter())
            .map(|(grant, want)| want.bytes_for(grant.runway_events()))
            .fold(0, usize::saturating_add)
    }

    #[must_use]
    pub fn released_count(&self) -> usize {
        self.grants
            .iter()
            .filter(|grant| grant.is_released())
            .count()
    }
}

/// The soft limit: what the allocator aims at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftLimitBytes(pub usize);

/// The hard limit: what the cache refuses to exceed, measured against real
/// bytes rather than an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardLimitBytes(pub usize);

/// The shard's residency limits.
///
/// Two limits rather than one ceiling, because the allocator can only aim: it
/// multiplies event counts by an estimate. At or below the soft limit nothing
/// happens; between the two, the next recomputation shrinks targets without
/// forcing a trim; above the hard limit the cache trims immediately rather
/// than waiting for a tick. The band between them is what absorbs estimation
/// error and bursts.
#[derive(Debug, Clone)]
pub struct ShardBudget {
    soft_max_bytes: usize,
    hard_max_bytes: usize,
}

impl ShardBudget {
    /// Distinct argument types rather than two bare `usize`, so the limits
    /// cannot be transposed silently.
    ///
    /// A hard limit below the soft limit is meaningless, so it is raised to
    /// meet it rather than rejected - the caller has asked for a zero-width
    /// band, which is degenerate but not dangerous.
    #[must_use]
    pub fn new(soft: SoftLimitBytes, hard: HardLimitBytes) -> Self {
        Self {
            soft_max_bytes: soft.0,
            hard_max_bytes: hard.0.max(soft.0),
        }
    }

    #[must_use]
    pub fn soft_max_bytes(&self) -> usize {
        self.soft_max_bytes
    }

    #[must_use]
    pub fn hard_max_bytes(&self) -> usize {
        self.hard_max_bytes
    }

    /// The narrowest band that a single absorb cannot cross.
    ///
    /// One absorb adds at most `fetch_max_events` events, each at most
    /// [`EstimatedBytesPerEvent::MAX`], so a band at least this wide cannot be
    /// jumped from below the soft limit to above the hard one by one fetch.
    /// This is what makes the band's width derived rather than guessed.
    #[must_use]
    pub fn min_band_bytes(fetch_max_events: usize) -> usize {
        fetch_max_events.saturating_mul(EstimatedBytesPerEvent::MAX)
    }

    /// Whether the band is wide enough for `fetch_max_events`, for wiring-time
    /// validation.
    #[must_use]
    pub fn has_sufficient_band(&self, fetch_max_events: usize) -> bool {
        self.hard_max_bytes.saturating_sub(self.soft_max_bytes)
            >= Self::min_band_bytes(fetch_max_events)
    }

    /// Fits `demand` into the **soft** limit.
    ///
    /// Three regimes, which are the degradation ladder:
    ///
    /// 1. Everything fits - every segment gets what it asked for.
    /// 2. It does not, but everyone can be floored - each gets the floor, and
    ///    the remaining bytes are shared out in descending readers-per-byte
    ///    order up to each segment's desire. Throughput degrades; nothing is
    ///    lost.
    /// 3. Not everyone can be floored - segments are released, fewest readers
    ///    first, until the rest fit at the floor. A released segment is
    ///    re-read before its reader advances, so this still loses no events.
    #[must_use]
    pub fn allocate(&self, demand: &[SegmentDemand], policy: &RunwayPolicy) -> Allocation {
        if demand.is_empty() {
            return Allocation { grants: Vec::new() };
        }

        let desired_bytes = total_bytes(demand, SegmentDemand::desired_runway_events);
        if desired_bytes <= self.soft_max_bytes {
            return Self::grant_each(demand, SegmentDemand::desired_runway_events);
        }

        let floors: Vec<usize> = demand
            .iter()
            .map(|want| want.desired_runway_events().min(policy.floor_events))
            .collect();
        let floor_bytes = total_bytes(demand, |want| {
            want.desired_runway_events().min(policy.floor_events)
        });

        if floor_bytes > self.soft_max_bytes {
            return self.release_until_floors_fit(demand, &floors);
        }

        self.share_surplus_above_floors(demand, &floors, floor_bytes)
    }

    fn grant_each(
        demand: &[SegmentDemand],
        runway: impl Fn(&SegmentDemand) -> usize,
    ) -> Allocation {
        Allocation {
            grants: demand
                .iter()
                .map(|want| {
                    RunwayGrant::builder(want.key().clone())
                        .segment_from(want.segment_from())
                        .runway_events(runway(want))
                        .build()
                })
                .collect(),
        }
    }

    /// Regime 2 of the ladder.
    fn share_surplus_above_floors(
        &self,
        demand: &[SegmentDemand],
        floors: &[usize],
        floor_bytes: usize,
    ) -> Allocation {
        let mut granted: Vec<usize> = floors.to_vec();
        let mut remaining = self.soft_max_bytes.saturating_sub(floor_bytes);

        for index in by_value_per_byte(demand) {
            let Some(want) = demand.get(index) else {
                continue;
            };
            let Some(current) = granted.get(index).copied() else {
                continue;
            };

            let headroom = want.desired_runway_events().saturating_sub(current);
            if headroom == 0 || want.estimated_bytes_per_event() == 0 {
                continue;
            }

            let affordable = remaining.div_euclid(want.estimated_bytes_per_event());
            let extra = headroom.min(affordable);
            if extra == 0 {
                continue;
            }

            if let Some(slot) = granted.get_mut(index) {
                *slot = current.saturating_add(extra);
            }
            remaining = remaining.saturating_sub(want.bytes_for(extra));
        }

        Allocation {
            grants: demand
                .iter()
                .enumerate()
                .map(|(index, want)| {
                    RunwayGrant::builder(want.key().clone())
                        .segment_from(want.segment_from())
                        .runway_events(granted.get(index).copied().unwrap_or(0))
                        .build()
                })
                .collect(),
        }
    }

    /// Regime 3 of the ladder.
    fn release_until_floors_fit(&self, demand: &[SegmentDemand], floors: &[usize]) -> Allocation {
        let mut granted: Vec<usize> = floors.to_vec();
        let mut committed = demand
            .iter()
            .zip(floors.iter())
            .map(|(want, floor)| want.bytes_for(*floor))
            .fold(0, usize::saturating_add);

        // Least valuable first: fewest readers per byte yields soonest.
        for index in by_value_per_byte(demand).into_iter().rev() {
            if committed <= self.soft_max_bytes {
                break;
            }
            let Some(want) = demand.get(index) else {
                continue;
            };
            let Some(slot) = granted.get_mut(index) else {
                continue;
            };
            let freed = want.bytes_for(*slot);
            *slot = 0;
            committed = committed.saturating_sub(freed);
        }

        Allocation {
            grants: demand
                .iter()
                .enumerate()
                .map(|(index, want)| {
                    RunwayGrant::builder(want.key().clone())
                        .segment_from(want.segment_from())
                        .runway_events(granted.get(index).copied().unwrap_or(0))
                        .build()
                })
                .collect(),
        }
    }
}

/// Deterministic tie-break, so equal demands never allocate differently.
fn by_value_per_byte(demand: &[SegmentDemand]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..demand.len()).collect();
    order.sort_by(|left, right| {
        let (Some(a), Some(b)) = (demand.get(*left), demand.get(*right)) else {
            return left.cmp(right);
        };
        // Cross-multiplied so the ratio comparison stays exact in integers.
        let lhs = a.readers().saturating_mul(b.estimated_bytes_per_event());
        let rhs = b.readers().saturating_mul(a.estimated_bytes_per_event());
        rhs.cmp(&lhs)
            .then_with(|| a.segment_from().cmp(&b.segment_from()))
            .then_with(|| left.cmp(right))
    });
    order
}

fn total_bytes(demand: &[SegmentDemand], runway: impl Fn(&SegmentDemand) -> usize) -> usize {
    demand
        .iter()
        .map(|want| want.bytes_for(runway(want)))
        .fold(0, usize::saturating_add)
}
