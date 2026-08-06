//! Deciding what one partition needs fetched, and how badly.
//!
//! Pure, and the mirror of [`super::reclaim`]: that module decides what to drop
//! from a partition, this one decides what to pull into it. Both see spans,
//! reader positions and counts, never a segment or a lock.
//!
//! Demands are **derived**, never enqueued. A reader that misses sets a flag;
//! the work of turning that into fetches happens here, by inspection, once per
//! partition. The consequence is that coalescing stops being an algorithm and
//! becomes a property of the shape: there is one derivation per partition, so
//! the number of fetches tracks reader *clusters* rather than readers. A
//! thousand readers at the tail of one partition produce one demand.
//!
//! Because a derived demand has no identity from one scan to the next, it cannot
//! carry its own starvation counter. The counter lives on the reader, which does
//! have identity, and a demand inherits the worst of the readers behind it.

use crate::domain::model::Sequence;

use super::reclaim::SegmentSummary;

/// Why a fetch is being asked for.
///
/// Functional rather than descriptive: the tail poller's backoff applies to
/// [`Self::Tail`] alone. A partition backing off because its tail has not
/// materialised yet must never suppress a lagging reader's backfill, which is
/// for sequences the backend certainly holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchReason {
    /// Nothing is resident. The partition has just been created, or everything
    /// it held has been reclaimed.
    ColdStart,
    /// At or beyond the highest sequence anything has accounted for. Subject to
    /// the poller's backoff, because the events may not exist yet.
    Tail,
    /// Below the accounted frontier: a gap, or a span that was reclaimed and is
    /// wanted again. Always eligible - these sequences exist.
    Backfill,
}

/// One reader's unmet need, as the derivation sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderNeed {
    position: Sequence,
    starved_rounds: u32,
}

impl ReaderNeed {
    /// One argument: the position this reader has consumed. Not the sequence
    /// it wants next - that value is unknowable in a sparse space, and asking
    /// for it is what the old `+ 1` at every call site was guessing at.
    #[must_use]
    pub fn new(position: Sequence) -> Self {
        Self {
            position,
            starved_rounds: 0,
        }
    }

    /// How many scans have seen this reader unserved and done nothing for it.
    #[must_use]
    pub fn starved_for(mut self, rounds: u32) -> Self {
        self.starved_rounds = rounds;
        self
    }

    #[must_use]
    /// The position this reader has consumed. What it wants is whatever the
    /// store holds above this.
    pub fn position(self) -> Sequence {
        self.position
    }

    #[must_use]
    pub fn starved_rounds(self) -> u32 {
        self.starved_rounds
    }
}

/// One fetch a partition wants, and what it is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Demand {
    after: Sequence,
    readers_behind: usize,
    starved_rounds: u32,
    reason: FetchReason,
}

impl Demand {
    #[must_use]
    pub fn builder(after: Sequence) -> DemandBuilder {
        DemandBuilder {
            after,
            readers_behind: 0,
            starved_rounds: 0,
            reason: FetchReason::Tail,
        }
    }

    /// The position a fetch reads after. Exclusive, like every read: the store
    /// answers with whatever it holds above this, so nothing has to name the
    /// sequence it expects to come next.
    #[must_use]
    pub fn after(self) -> Sequence {
        self.after
    }

    /// How many readers one fetch here would serve. The fan-out ratio, per
    /// demand.
    #[must_use]
    pub fn readers_behind(self) -> usize {
        self.readers_behind
    }

    /// The worst starvation among the readers behind this demand, which is what
    /// lets an unpopular demand eventually outrank a popular one.
    #[must_use]
    pub fn starved_rounds(self) -> u32 {
        self.starved_rounds
    }

    #[must_use]
    pub fn reason(self) -> FetchReason {
        self.reason
    }

    /// Whether the tail poller's backoff governs this demand.
    ///
    /// True for anything speculative about whether the sequences exist at all -
    /// a tail that may not have been assigned yet, and a cold partition nothing
    /// is known about. Asking either every round hammers the backend for a
    /// partition that is simply idle.
    ///
    /// False only for a backfill, because something has already accounted for a
    /// span above it: those sequences exist, so a laggard must never be made to
    /// wait on a tail that does not.
    #[must_use]
    pub fn defers_to_backoff(self) -> bool {
        matches!(self.reason, FetchReason::Tail | FetchReason::ColdStart)
    }
}

pub struct DemandBuilder {
    after: Sequence,
    readers_behind: usize,
    starved_rounds: u32,
    reason: FetchReason,
}

impl DemandBuilder {
    #[must_use]
    pub fn readers_behind(mut self, readers: usize) -> Self {
        self.readers_behind = readers;
        self
    }

    #[must_use]
    pub fn starved_rounds(mut self, rounds: u32) -> Self {
        self.starved_rounds = rounds;
        self
    }

    #[must_use]
    pub fn reason(mut self, reason: FetchReason) -> Self {
        self.reason = reason;
        self
    }

    #[must_use]
    pub fn build(self) -> Demand {
        Demand {
            after: self.after,
            readers_behind: self.readers_behind,
            starved_rounds: self.starved_rounds,
            reason: self.reason,
        }
    }
}

/// Which readers no resident span can answer.
///
/// Separate from [`derive`] so the scheduler can age exactly these readers
/// without re-deriving, and so the two halves are testable apart.
#[must_use]
pub fn unserved(summaries: &[SegmentSummary], readers: &[ReaderNeed]) -> Vec<ReaderNeed> {
    readers
        .iter()
        .copied()
        .filter(|need| !accounted(summaries, need.position()))
        .collect()
}

/// Whether some resident span accounts for `sequence` - in which case a reader
/// there can be answered from memory, with an event or with the knowledge that
/// none exists.
#[must_use]
pub fn accounted(summaries: &[SegmentSummary], position: Sequence) -> bool {
    summaries
        .iter()
        .any(|summary| summary.span().serves(position))
}

/// Turns unmet needs into fetches, one per reader cluster.
///
/// One demand per distinct position, furthest-behind first.
///
/// No estimate of how far a fetch will reach: readers standing on the *same*
/// sequence want the same fetch, and grouping them by equality is exact. Which
/// other readers a fetch also serves is not predicted here at all - the fetch
/// records what it read, `PartitionCache::absorb` wakes exactly the readers
/// that span covers, and `unserved` drops their needs before the next round
/// derives anything. Sharing is a property of the answer rather than of the
/// plan, which is why nothing here has to measure the distance between two
/// readers - a distance whose events have not been fetched yet and therefore
/// cannot be counted.
///
/// `frontier` is the highest sequence anything has accounted for, which is what
/// separates a tail fetch from a backfill.
#[must_use]
pub fn derive(
    summaries: &[SegmentSummary],
    readers: &[ReaderNeed],
    frontier: Sequence,
) -> Vec<Demand> {
    let mut needs = unserved(summaries, readers);
    if needs.is_empty() {
        return Vec::new();
    }
    needs.sort_unstable_by_key(|need| need.position());

    let mut demands: Vec<Demand> = Vec::new();
    // The position being accumulated: where it is, how many readers stand
    // there, and the worst any of them has waited.
    let mut open: Option<(Sequence, usize, u32)> = None;

    for need in needs {
        match open {
            Some((position, count, starved)) if need.position() == position => {
                open = Some((
                    position,
                    count.saturating_add(1),
                    starved.max(need.starved_rounds()),
                ));
            }
            Some((wanted, count, starved)) => {
                demands.push(demand_at(summaries, wanted, count, starved, frontier));
                open = Some((need.position(), 1, need.starved_rounds()));
            }
            None => open = Some((need.position(), 1, need.starved_rounds())),
        }
    }
    if let Some((wanted, count, starved)) = open {
        demands.push(demand_at(summaries, wanted, count, starved, frontier));
    }

    demands
}

fn demand_at(
    summaries: &[SegmentSummary],
    from: Sequence,
    readers_behind: usize,
    starved_rounds: u32,
    frontier: Sequence,
) -> Demand {
    // `from` is the position a reader has consumed, so a reader standing *at*
    // the frontier is asking for what lies above it - speculative, because
    // nothing has accounted for anything up there yet. Only a position strictly
    // below the frontier is a backfill of sequences the backend certainly holds.
    let reason = if summaries.is_empty() {
        FetchReason::ColdStart
    } else if from >= frontier {
        FetchReason::Tail
    } else {
        FetchReason::Backfill
    };

    Demand::builder(from)
        .readers_behind(readers_behind)
        .starved_rounds(starved_rounds)
        .reason(reason)
        .build()
}

/// How many readers one scan of starvation is worth.
///
/// The knob between throughput and fairness, in units of the thing a fetch is
/// actually valued in. Zero is pure fan-out, under which a lone lagging reader
/// can be starved indefinitely by a popular tail. Very large approaches strict
/// starvation-first, which inverts the point of coalescing: a single laggard
/// would repeatedly preempt a fetch serving a thousand readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarvationWeight(pub usize);

impl Default for StarvationWeight {
    /// Ten reader-equivalents per scan. At the design point - roughly a thousand
    /// readers clustered at the tail of a partition - that lets a demand with a
    /// single reader behind it overtake them after about a hundred scans, so
    /// fan-out governs the normal case and fairness still has a bound.
    fn default() -> Self {
        Self(10)
    }
}

impl Demand {
    /// What serving this demand is worth, in reader-equivalents.
    ///
    /// Fan-out plus starvation credit, added rather than multiplied. Addition is
    /// what makes eventual service a guarantee: credit grows independently of
    /// how few readers are behind a demand, so any demand eventually outranks
    /// any fixed fan-out. Multiplying would scale a lonely demand by its own
    /// tiny base and leave it permanently behind a popular one.
    #[must_use]
    pub fn value(self, weight: StarvationWeight) -> usize {
        let credit = usize::try_from(self.starved_rounds)
            .unwrap_or(usize::MAX)
            .saturating_mul(weight.0);
        self.readers_behind.saturating_add(credit)
    }
}

/// Orders demands by what serving each is worth, most valuable first.
///
/// Fan-out leads, because a fetch serving a thousand readers is worth a
/// thousand times one serving a single reader, and coalescing exists precisely
/// to exploit that. Starvation only accumulates enough credit to overturn it
/// after the wait has gone on long enough, which is the fairness bound rather
/// than a fairness override.
pub fn rank(demands: &mut [Demand], weight: StarvationWeight) {
    demands.sort_by_key(|demand| {
        // Position last, so identical demands order reproducibly instead of by
        // however the map happened to iterate.
        (std::cmp::Reverse(demand.value(weight)), demand.after())
    });
}
