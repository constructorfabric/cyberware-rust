//! Deciding which resident segments a reclamation pass takes, and why.
//!
//! Pure: this module sees sequence spans, byte counts and reader positions, and
//! never a segment, an event or a lock. The cache applies what is decided here.
//! Keeping the decision separate is what makes the clustering rule testable
//! without building a cache, a reader or a runtime.
//!
//! One rule drives the first two reasons a segment is taken. Reader positions
//! are clustered, each cluster is expanded into a window it is entitled to
//! keep, and every segment intersecting no window is reclaimable. "Below the
//! slowest reader" is not a separate rule; it is the degenerate case of lying
//! outside the lowest window. Only the byte limit is a genuinely separate
//! stage, because it takes spans that *are* wanted.

use crate::domain::model::Sequence;

use super::span::AccountedSpan;

/// How far a reader-free stretch must run before the middle of it is worth
/// taking.
///
/// Also the clustering rule, and deliberately the same number: two readers
/// farther apart than this are working different parts of the partition, so the
/// space between them is reclaimable - and two readers closer than this have
/// nothing between them worth taking. A separate "cluster span" knob would be
/// this one under another name, tunable into contradicting itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapThresholdEvents(pub usize);

/// What one partition's map may hold, measured against real bytes rather than
/// the allocator's estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidencyLimitBytes(pub usize);

/// What one reclamation pass is allowed to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimPolicy {
    gap_threshold_events: usize,
    residency_limit_bytes: usize,
}

impl ReclaimPolicy {
    /// Two arguments, but a count of events and a count of bytes cannot be
    /// transposed silently.
    #[must_use]
    pub fn new(gap: GapThresholdEvents, limit: ResidencyLimitBytes) -> Self {
        Self {
            gap_threshold_events: gap.0,
            residency_limit_bytes: limit.0,
        }
    }

    #[must_use]
    pub fn gap_threshold_events(self) -> usize {
        self.gap_threshold_events
    }

    #[must_use]
    pub fn residency_limit_bytes(self) -> usize {
        self.residency_limit_bytes
    }
}

impl Default for ReclaimPolicy {
    /// Sized from what a partition may hold rather than copied from elsewhere:
    /// a reader-free stretch only becomes worth taking once it is wider than
    /// the residency the partition could have devoted to it anyway, so the
    /// event cap doubles as the gap threshold.
    fn default() -> Self {
        Self {
            gap_threshold_events: 8192,
            residency_limit_bytes: 32 * 1024 * 1024,
        }
    }
}

/// The spans a pass must keep, derived from reader positions alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetainedWindows {
    /// Ascending and disjoint. Each is `(lower, upper]`: open below because a
    /// reader has already consumed its own position and will never return to
    /// it, closed above because the runway ahead is what it is entitled to.
    windows: Vec<(Sequence, Sequence)>,
}

/// The position `events` resident events past `from`, found by counting what is
/// actually there.
///
/// The threshold is a number of events, and a sequence span is not one: a
/// partition's sequence space is assigned contiguously but not populated
/// contiguously, so `from + events` may cover far more events than the
/// threshold names, or far fewer. Counting through the segments that hold them
/// is the only reading of "this many events ahead" that survives a sparse
/// partition.
///
/// Resolution is per segment, since a summary reports how many events a segment
/// holds without saying where in its span they sit: the answer is the end of the
/// segment in which the `events`-th resident event lies. Where the partition
/// holds fewer than that, the answer is the end of what it holds - never less
/// than `from` itself, so a window always covers the position a fetch is aimed
/// at.
#[must_use]
pub fn counted_forward(summaries: &[SegmentSummary], from: Sequence, events: usize) -> Sequence {
    // Walked in ascending order, not in the order the caller happened to hold
    // them: counting has to stop at the segment where the target event lies,
    // and a far segment visited first would stretch the answer past every
    // nearer one. Sorting here rather than asking every caller to is the
    // cheaper contract - a partition holds few segments, and reclamation runs
    // on a paced tick rather than per event.
    let mut ahead: Vec<&SegmentSummary> = summaries
        .iter()
        .filter(|summary| summary.through() > from)
        .collect();
    ahead.sort_unstable_by_key(|summary| summary.from());

    let mut counted = 0_usize;
    let mut reach = from;
    for summary in ahead {
        reach = summary.through();
        counted = counted.saturating_add(summary.events());
        if counted >= events {
            break;
        }
    }
    reach
}

impl RetainedWindows {
    /// Clusters `positions` and expands each cluster into the window it keeps.
    ///
    /// `positions` need be neither sorted nor deduplicated - it is copied and
    /// ordered here, so a caller cannot produce windows that misbehave.
    ///
    /// `summaries` is what the reach is counted through: the threshold is a
    /// number of events, so the window ends where that many resident events
    /// have been passed, not that many sequence numbers.
    #[must_use]
    pub fn from_positions(
        summaries: &[SegmentSummary],
        positions: &[Sequence],
        gap: GapThresholdEvents,
    ) -> Self {
        let mut sorted: Vec<Sequence> = positions.to_vec();
        sorted.sort_unstable();

        // At least the next position, whatever the threshold says. A reader at
        // `p` is fetched for at the first sequence after `p`, so a window that
        // excluded it would let a pass reclaim the very span a fetch is aimed
        // at - and `absorb` wakes readers a recorded span covers, so those
        // readers would wake, find it gone, and park with nothing left to wake
        // them. A threshold of zero must degrade to "keep only what is next",
        // never to "keep nothing".
        let events = gap.0.max(1);
        let mut windows: Vec<(Sequence, Sequence)> = Vec::new();

        for position in sorted {
            let reach = counted_forward(summaries, position, events);
            match windows.last_mut() {
                // Within reach of the cluster being built: extend it. Nothing
                // between the two is worth taking, by the definition of the
                // threshold.
                Some(last) if position <= last.1 => last.1 = last.1.max(reach),
                _ => windows.push((position, reach)),
            }
        }

        Self { windows }
    }

    /// Whether any window overlaps `from..=through`.
    #[must_use]
    pub fn intersects(&self, from: Sequence, through: Sequence) -> bool {
        self.windows
            .iter()
            .any(|(lower, upper)| through > *lower && from <= *upper)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    #[must_use]
    pub fn windows(&self) -> &[(Sequence, Sequence)] {
        &self.windows
    }
}

/// One resident segment reduced to what a reclamation decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentSummary {
    from: Sequence,
    through: Sequence,
    events: usize,
    bytes: usize,
}

impl SegmentSummary {
    /// Built through a builder: `from` and `through` are both sequences, and a
    /// positional pair would let a caller invert the span silently.
    #[must_use]
    pub fn builder(from: Sequence) -> SegmentSummaryBuilder {
        SegmentSummaryBuilder {
            from,
            through: from,
            events: 0,
            bytes: 0,
        }
    }

    #[must_use]
    pub fn from(self) -> Sequence {
        self.from
    }

    #[must_use]
    pub fn through(self) -> Sequence {
        self.through
    }

    #[must_use]
    pub fn events(self) -> usize {
        self.events
    }

    #[must_use]
    pub fn bytes(self) -> usize {
        self.bytes
    }

    /// The range this segment has accounted for.
    #[must_use]
    pub fn span(self) -> AccountedSpan {
        AccountedSpan::builder(self.from)
            .through(self.through)
            .build()
    }

    /// Whether a reader at `position` would be served from this segment on its
    /// very next read. Such a segment is never taken under byte pressure.
    #[must_use]
    fn is_next_for(self, position: Sequence) -> bool {
        self.span().serves(position)
    }
}

pub struct SegmentSummaryBuilder {
    from: Sequence,
    through: Sequence,
    events: usize,
    bytes: usize,
}

impl SegmentSummaryBuilder {
    #[must_use]
    pub fn through(mut self, through: Sequence) -> Self {
        self.through = through;
        self
    }

    #[must_use]
    pub fn events(mut self, events: usize) -> Self {
        self.events = events;
        self
    }

    #[must_use]
    pub fn bytes(mut self, bytes: usize) -> Self {
        self.bytes = bytes;
        self
    }

    #[must_use]
    pub fn build(self) -> SegmentSummary {
        SegmentSummary {
            from: self.from,
            through: self.through.max(self.from),
            events: self.events,
            bytes: self.bytes,
        }
    }
}

/// Which resident segments one pass takes, and why.
///
/// The three reasons carry different prices and are kept apart for that reason
/// alone. `dead` costs nothing - nobody can ask for those spans again. `gapped`
/// and `pressured` will both be refetched, so their sum is the predicted
/// refetch volume, which is the actual price of bounding residency this way.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReclaimPlan {
    dead: Vec<Sequence>,
    gapped: Vec<Sequence>,
    pressured: Vec<Sequence>,
    limit_breached: bool,
}

impl ReclaimPlan {
    #[must_use]
    pub fn dead(&self) -> &[Sequence] {
        &self.dead
    }

    #[must_use]
    pub fn gapped(&self) -> &[Sequence] {
        &self.gapped
    }

    #[must_use]
    pub fn pressured(&self) -> &[Sequence] {
        &self.pressured
    }

    /// The byte limit still stands breached: everything left is wanted next by
    /// some reader. Reported rather than forced, because forcing it would drop
    /// a span a reader is about to ask for and gain nothing.
    #[must_use]
    pub fn limit_breached(&self) -> bool {
        self.limit_breached
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dead.is_empty() && self.gapped.is_empty() && self.pressured.is_empty()
    }

    /// Every segment key the plan takes, in no particular order.
    #[must_use]
    pub fn victims(&self) -> Vec<Sequence> {
        let mut all = self.dead.clone();
        all.extend_from_slice(&self.gapped);
        all.extend_from_slice(&self.pressured);
        all
    }
}

/// Decides a pass from summaries and reader positions alone.
///
/// `readers` may be a snapshot taken before the map was locked. Staleness is
/// safe in both directions by construction: a reader that has advanced only
/// makes this decision more conservative than it needed to be, and a reader
/// whose span is taken anyway is answered `Unknown` and refetches, which is
/// always a legal answer.
#[must_use]
pub fn plan(
    summaries: &[SegmentSummary],
    readers: &[Sequence],
    policy: &ReclaimPolicy,
) -> ReclaimPlan {
    let mut plan = ReclaimPlan::default();

    // A partition whose last reader deregistered for an instant during a
    // rebalance must not be flushed. With nobody reading, residency is bounded
    // by the byte limit alone.
    if readers.is_empty() {
        apply_pressure(summaries, readers, policy, &mut plan);
        return plan;
    }

    let windows = RetainedWindows::from_positions(
        summaries,
        readers,
        GapThresholdEvents(policy.gap_threshold_events()),
    );
    let slowest = readers.iter().copied().min().unwrap_or(Sequence::NONE);

    for summary in summaries {
        if windows.intersects(summary.from(), summary.through()) {
            continue;
        }
        // Ownership is deliberately not consulted. A reader mid-read on a
        // segment keeps its storage alive through its own `Arc`, so taking it
        // from the map is safe - and waiting for readers is the pinning
        // protocol this design exists to avoid.
        if summary.through() <= slowest {
            plan.dead.push(summary.from());
        } else {
            plan.gapped.push(summary.from());
        }
    }

    apply_pressure(summaries, readers, policy, &mut plan);

    // The invariant the unified predicate exists to protect, checked at the one
    // place it can be violated. `absorb` wakes exactly the readers a recorded
    // span serves; if a pass could take a span that serves a live reader, that
    // reader would wake, find nothing, and park with nobody left to wake it.
    // Three separate predicates used to have to agree for this to hold - now one
    // does, and this catches a future divergence rather than a stalled reader in
    // production catching it for us.
    debug_assert!(
        plan.victims().iter().all(|taken| {
            summaries
                .iter()
                .find(|summary| summary.from() == *taken)
                .is_none_or(|summary| !readers.iter().any(|at| summary.span().serves(*at)))
        }),
        "a pass took a span that some reader reads next"
    );

    plan
}

/// Takes further segments, most speculative first, until residency is under the
/// limit.
///
/// "Most speculative" is the distance from a segment's start back to the
/// nearest reader at or below it: a span far ahead of every reader is prefetch
/// that has not been needed yet, so dropping it costs a refetch that had not
/// been earned. A span some reader will be served from on its next read is
/// never taken - dropping that guarantees an immediate refetch of the very
/// thing just discarded.
fn apply_pressure(
    summaries: &[SegmentSummary],
    readers: &[Sequence],
    policy: &ReclaimPolicy,
    plan: &mut ReclaimPlan,
) {
    let taken: Vec<Sequence> = plan.victims();
    let mut resident: usize = summaries
        .iter()
        .filter(|summary| !taken.contains(&summary.from()))
        .map(|summary| summary.bytes())
        .fold(0, usize::saturating_add);

    if resident <= policy.residency_limit_bytes() {
        return;
    }

    let mut candidates: Vec<&SegmentSummary> = summaries
        .iter()
        .filter(|summary| !taken.contains(&summary.from()))
        .filter(|summary| !readers.iter().any(|at| summary.is_next_for(*at)))
        .collect();

    candidates.sort_by_key(|summary| {
        // How far ahead of the nearest reader behind it this segment sits, in
        // events actually resident between the two. A sequence distance would
        // rank a wide, near-empty span above a dense one right in front of a
        // reader, which is the opposite of what a pass should take first.
        let lead = readers
            .iter()
            .copied()
            .filter(|at| *at < summary.from())
            .max()
            .map_or(summary.events(), |nearest| {
                summaries
                    .iter()
                    .filter(|between| {
                        between.through() > nearest && between.through() <= summary.from()
                    })
                    .map(|between| between.events())
                    .fold(0_usize, usize::saturating_add)
            });
        // Descending lead, then descending start, so the order is total and
        // reproducible rather than dependent on the map's iteration.
        (std::cmp::Reverse(lead), std::cmp::Reverse(summary.from()))
    });

    for summary in candidates {
        if resident <= policy.residency_limit_bytes() {
            return;
        }
        resident = resident.saturating_sub(summary.bytes());
        plan.pressured.push(summary.from());
    }

    plan.limit_breached = resident > policy.residency_limit_bytes();
}
