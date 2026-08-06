//! One partition's resident segments.
//!
//! The map holds disjoint segments ordered by sequence, each accounting for a
//! span. A position inside some segment's span can be answered - with an event,
//! or with the knowledge that none exists. A position outside every span is
//! **unknown**, and a reader there must wait for a fetch rather than advance
//! past it: only a completed fetch may turn unknown into absent.
//!
//! Reclamation never waits on a reader. Segments are `Arc`, so dropping one from
//! the map leaves any reader still holding it able to finish reading it, and
//! its memory is freed when the last holder lets go. A reader can therefore
//! never block reclamation of anything except what it is currently holding -
//! which is what lets residency be bounded without a pinning protocol.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tokio::sync::{Notify, watch};

use crate::domain::model::Sequence;

use super::demand::{Demand, ReaderNeed, accounted, derive};
use super::reclaim::{ReclaimPolicy, SegmentSummary, plan};
use super::segment::Segment;
use super::span::AccountedSpan;
use crate::domain::streaming::read::{EventBatch, EventSlice, PartitionRead, ReadLimit};

/// Identifies one reader's registration for the lifetime of that registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReaderId(u64);

/// One reader's mutable state, shared between its handle and the registry.
///
/// Shared rather than copied into the map on every move, so `advance` is a
/// relaxed store and takes no lock at all. It runs once per delivered batch per
/// reader - at a thousand readers a partition that is the hottest write in the
/// module, and it was previously a `HashMap` insert under a mutex.
#[derive(Debug)]
struct ReaderState {
    offset: AtomicI64,
    scanning: AtomicI64,
    /// Woken when an absorb records a span covering this reader's next wanted
    /// sequence, and at no other time.
    ///
    /// Per reader rather than one channel per partition, because an absorb knows
    /// exactly which span it recorded and the registry knows where every reader
    /// stands - so the set of readers a fetch actually serves is computable, and
    /// waking anyone else is waste. `notify_one` also stores a permit when
    /// nobody is waiting yet, which closes the window between a reader deciding
    /// to park and actually parking.
    ///
    /// Shared rather than owned so several readers can be woken through one
    /// waker. A session holds a reader per partition it was assigned, across
    /// several topics, and needs to wake when *any* of them has something -
    /// selecting over one future per partition would cost O(partitions) per
    /// poll, where one shared waker costs nothing. Targeting survives at the
    /// granularity that matters: a fetch still wakes only the sessions it can
    /// serve, and a woken session has to poll its partitions anyway.
    ready: Arc<Notify>,
    /// Scans that saw this reader unserved and scheduled nothing for it.
    ///
    /// Lives on the reader because a derived demand has no identity from one
    /// scan to the next, so it has nowhere to keep a counter of its own. A
    /// demand inherits the worst starvation among the readers behind it, which
    /// is what lets an unpopular demand eventually outrank a popular one.
    starved_rounds: AtomicU32,
}

/// Where each registered reader has reached.
///
/// Separate from the cache so a [`ReaderHandle`] can hold it without a
/// reference cycle back to the cache that created it.
#[derive(Debug, Default)]
pub struct ReaderRegistry {
    next_id: AtomicU64,
    readers: Mutex<HashMap<ReaderId, Arc<ReaderState>>>,
}

impl ReaderRegistry {
    /// Recovers a poisoned guard rather than dropping the registration.
    ///
    /// Losing a registration silently is worse than continuing: the loader
    /// would size runway for readers it can no longer see, and reclamation
    /// would take spans a live reader still wants.
    fn readers(&self) -> MutexGuard<'_, HashMap<ReaderId, Arc<ReaderState>>> {
        self.readers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, offset: Sequence, ready: Arc<Notify>) -> (ReaderId, Arc<ReaderState>) {
        let id = ReaderId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let state = Arc::new(ReaderState {
            offset: AtomicI64::new(offset.as_i64()),
            scanning: AtomicI64::new(0),
            ready,
            starved_rounds: AtomicU32::new(0),
        });
        self.readers().insert(id, Arc::clone(&state));
        (id, state)
    }

    fn deregister(&self, id: ReaderId) {
        self.readers().remove(&id);
    }

    /// A snapshot of the registered states, taken under the lock and read
    /// outside it.
    fn snapshot(&self) -> Vec<Arc<ReaderState>> {
        self.readers().values().map(Arc::clone).collect()
    }

    /// Lowest registered position, or `None` when nothing is registered.
    fn slowest(&self) -> Option<Sequence> {
        self.snapshot()
            .iter()
            .map(|state| Sequence::assigned(state.offset.load(Ordering::Relaxed)))
            .min()
    }

    /// Registered positions, ascending.
    ///
    /// Sorted here, in the cold path, rather than keeping an ordered structure
    /// that every `advance` would have to maintain - `advance` runs once per
    /// delivered batch per reader, and this runs once per reclamation pass.
    fn positions_ascending(&self) -> Vec<Sequence> {
        let mut positions: Vec<Sequence> = self
            .snapshot()
            .iter()
            .map(|state| Sequence::assigned(state.offset.load(Ordering::Relaxed)))
            .collect();
        positions.sort_unstable();
        positions
    }

    /// What each registered reader wants next, with how long it has waited.
    fn needs(&self) -> Vec<(Arc<ReaderState>, ReaderNeed)> {
        self.snapshot()
            .into_iter()
            .map(|state| {
                let position = Sequence::assigned(state.offset.load(Ordering::Relaxed));
                let need = ReaderNeed::new(position)
                    .starved_for(state.starved_rounds.load(Ordering::Relaxed));
                (state, need)
            })
            .collect()
    }

    /// Wakes exactly the readers a span covers.
    ///
    /// The registry lock is taken to snapshot and released before any waker
    /// runs: waking under the lock would funnel every woken reader straight
    /// into contention for it.
    fn wake_covered(&self, span: AccountedSpan) {
        for state in self.snapshot() {
            if span.serves(Sequence::assigned(state.offset.load(Ordering::Relaxed))) {
                state.ready.notify_one();
            }
        }
    }

    fn count(&self) -> usize {
        self.readers().len()
    }
}

/// One reader's registration on one partition.
///
/// Carries a position and, when the session reports it, a scanning flag - a
/// derived bit rather than a measurement, so the cache holds nothing
/// subscription-shaped. Dropping the handle deregisters.
pub struct ReaderHandle {
    id: ReaderId,
    registry: Arc<ReaderRegistry>,
    /// The partition this reader reads. Holding it is what lets `read` live
    /// here rather than on the cache, which is the whole point of the seam: the
    /// position and the read are one call.
    ///
    /// Acyclic, and deliberately so. The cache holds `Arc<ReaderRegistry>`,
    /// and the registry holds `Arc<ReaderState>` - reader *state*, never
    /// handles. So no handle is reachable from the cache, and a dropped session
    /// releases its partitions.
    cache: Arc<PartitionCache>,
    newest_accounted: watch::Receiver<Sequence>,
    state: Arc<ReaderState>,
}

impl ReaderHandle {
    /// Whether anything is accounted for past this reader's position.
    ///
    /// A comparison against the published value, not an await - so a session
    /// can test every one of its partitions cheaply and await only when none
    /// is ready.
    #[must_use]
    pub fn has_data(&self) -> bool {
        *self.newest_accounted.borrow()
            > Sequence::assigned(self.state.offset.load(Ordering::Relaxed))
    }

    /// Resolves once a fetch has recorded a span covering what this reader
    /// wants next.
    ///
    /// **The read is authoritative; this is only advisory.** A caller must
    /// re-read immediately before parking here and must never park on a
    /// conclusion it drew earlier. The read consults the segment map; a wake
    /// merely says the map may have changed. Parking on a stale `Unknown`
    /// reintroduces exactly the race this is meant to close, because the absorb
    /// that would have woken the reader may already have happened.
    ///
    /// Not the accounted frontier, which cannot answer the question: a backfill
    /// below the frontier serves a reader without moving it, and a frontier
    /// ahead of a reader says nothing about whether the gap in front of that
    /// reader has been filled. Waiting on the frontier left a reader in a gap
    /// spinning - permanently awake and permanently unable to progress.
    ///
    /// An absorb that lands between the caller's last read and this call is not
    /// missed: the waker holds a permit.
    pub async fn wait(&self) {
        self.state.ready.notified().await;
    }

    /// This reader's position - exclusive, so the next wanted sequence is one
    /// past it.
    #[must_use]
    pub fn position(&self) -> Sequence {
        Sequence::assigned(self.state.offset.load(Ordering::Relaxed))
    }

    /// Reads forward from this reader's own position and advances it to what the
    /// read accounted for.
    ///
    /// The advance is part of the read rather than a second call, which is what
    /// makes "read without publishing" - a silent, permanent pin on this
    /// partition's memory - unwritable. Takes no lock of its own: the registry
    /// shares this reader's state rather than holding a copy of its position.
    ///
    /// Only a read that actually accounted for something moves the position or
    /// clears starvation. `NothingNew` and `Unknown` leave both alone: a reader
    /// must never advance over a sequence nothing has accounted for, and a
    /// reader nobody could serve is exactly what the starvation counter is
    /// counting.
    #[must_use]
    pub fn read(&self, limit: ReadLimit) -> PartitionRead {
        let from = self.position();
        let read = self.cache.read_from(from, limit);

        if let PartitionRead::Hit {
            accounted_through, ..
        } = &read
        {
            // Monotone under concurrent seeks: a reader is single-threaded, but
            // asserting the direction here is cheaper than debugging a cursor
            // that walked backwards.
            let advanced = from.max(*accounted_through);
            self.state
                .offset
                .store(advanced.as_i64(), Ordering::Relaxed);
            self.state.starved_rounds.store(0, Ordering::Relaxed);
        }

        read
    }

    /// Moves the position without reading, as `lseek` does - SEEK, and seeding
    /// at open. The only position change that may go backwards, which is why it
    /// is not folded into `read`.
    ///
    /// Clears starvation for the same reason a read does: a repositioned reader
    /// has no history of going unserved where it now stands.
    pub fn seek(&self, offset: Sequence) {
        self.state.offset.store(offset.as_i64(), Ordering::Relaxed);
        self.state.starved_rounds.store(0, Ordering::Relaxed);
    }

    /// The session's scanning classification. A derived boolean the session
    /// reports, not a selectivity measurement the cache keeps.
    pub fn report_scanning(&self, scanning: bool) {
        self.state
            .scanning
            .store(i64::from(scanning), Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.state.scanning.load(Ordering::Relaxed) != 0
    }

    #[must_use]
    pub fn offset(&self) -> Sequence {
        Sequence::assigned(self.state.offset.load(Ordering::Relaxed))
    }
}

impl Drop for ReaderHandle {
    fn drop(&mut self) {
        self.registry.deregister(self.id);
    }
}

/// A fetch's outcome, ready to be absorbed.
pub struct AbsorbedFetch {
    from: Sequence,
    through: Sequence,
    segment: Segment,
}

impl AbsorbedFetch {
    /// Built through a builder: `from` and `through` are both sequences, and a
    /// positional pair would let a caller invert the span silently.
    #[must_use]
    pub fn builder(segment: Segment) -> AbsorbedFetchBuilder {
        AbsorbedFetchBuilder {
            from: segment.from(),
            through: segment.through(),
            segment,
        }
    }
}

impl AbsorbedFetch {
    /// The fetch's segment, widened to the span the fetch accounted for.
    #[must_use]
    fn into_segment(self) -> Segment {
        self.segment.with_span(self.from, self.through)
    }
}

pub struct AbsorbedFetchBuilder {
    from: Sequence,
    through: Sequence,
    segment: Segment,
}

impl AbsorbedFetchBuilder {
    /// Widens the accounted span's start beyond the segment's own, for a fetch
    /// that proved a range empty below its first surviving event.
    #[must_use]
    pub fn accounted_from(mut self, from: Sequence) -> Self {
        self.from = from.min(self.from);
        self
    }

    /// Widens the accounted span's end beyond the segment's own.
    #[must_use]
    pub fn accounted_through(mut self, through: Sequence) -> Self {
        self.through = through.max(self.through);
        self
    }

    #[must_use]
    pub fn build(self) -> AbsorbedFetch {
        AbsorbedFetch {
            from: self.from,
            through: self.through,
            segment: self.segment,
        }
    }
}

/// A count of segments, the events in them, and their footprint.
///
/// `u64` rather than `usize` because the cumulative tallies are a *flow* rather
/// than a size: a partition passes far more bytes through its cache over a run
/// than it ever holds at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    segments: u64,
    events: u64,
    bytes: u64,
}

impl Tally {
    fn of(segment: &Segment) -> Self {
        Self {
            segments: 1,
            events: u64::try_from(segment.event_count()).unwrap_or(u64::MAX),
            bytes: u64::try_from(segment.bytes()).unwrap_or(u64::MAX),
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            segments: self.segments.saturating_add(other.segments),
            events: self.events.saturating_add(other.events),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    fn minus(self, other: Self) -> Self {
        Self {
            segments: self.segments.saturating_sub(other.segments),
            events: self.events.saturating_sub(other.events),
            bytes: self.bytes.saturating_sub(other.bytes),
        }
    }

    /// Componentwise maximum, for tracking a high-water mark.
    fn highest(self, other: Self) -> Self {
        Self {
            segments: self.segments.max(other.segments),
            events: self.events.max(other.events),
            bytes: self.bytes.max(other.bytes),
        }
    }

    #[must_use]
    pub fn segments(self) -> u64 {
        self.segments
    }

    #[must_use]
    pub fn events(self) -> u64 {
        self.events
    }

    #[must_use]
    pub fn bytes(self) -> u64 {
        self.bytes
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.segments == 0
    }
}

/// The map and the totals that describe it, behind one lock.
///
/// The totals live here rather than in atomics for two reasons. Enforcing a
/// byte limit on every absorb needs the resident total in constant time, so it
/// has to be maintained incrementally rather than walked; and once it is
/// maintained incrementally it has to be updated in the same critical section
/// that mutates the map, or it can disagree with the map it describes. Splitting
/// the cumulative totals off into atomics after that would put one truth in two
/// mechanisms and leave nowhere to assert the invariant.
struct Residency {
    segments: BTreeMap<Sequence, Arc<Segment>>,
    live: Tally,
    peak: Tally,
    absorbed: Tally,
    reclaimed: Tally,
    passes: u64,
    freeing_passes: u64,
}

impl Residency {
    fn new() -> Self {
        Self {
            segments: BTreeMap::new(),
            live: Tally::default(),
            peak: Tally::default(),
            absorbed: Tally::default(),
            reclaimed: Tally::default(),
            passes: 0,
            freeing_passes: 0,
        }
    }

    fn record(&mut self, segment: &Segment) {
        let tally = Tally::of(segment);
        self.absorbed = self.absorbed.plus(tally);
        self.live = self.live.plus(tally);
        self.peak = self.peak.highest(self.live);
    }

    /// Removes `victims` from the map and tallies what left.
    fn take(&mut self, victims: &[Sequence]) -> Tally {
        let mut freed = Tally::default();
        for from in victims {
            if let Some(segment) = self.segments.remove(from) {
                freed = freed.plus(Tally::of(&segment));
            }
        }
        self.reclaimed = self.reclaimed.plus(freed);
        self.live = self.live.minus(freed);
        freed
    }

    fn summaries(&self) -> Vec<SegmentSummary> {
        self.segments
            .values()
            .map(|segment| {
                SegmentSummary::builder(segment.from())
                    .through(segment.through())
                    .events(segment.event_count())
                    .bytes(segment.bytes())
                    .build()
            })
            .collect()
    }

    /// Every accounted byte is either still in the map or was reclaimed from
    /// it. There is no third place for one to go - which is only true because
    /// an absorb is narrowed to a disjoint span and so can never displace a
    /// resident segment silently.
    fn balances(&self) -> bool {
        self.absorbed.bytes == self.reclaimed.bytes.saturating_add(self.live.bytes)
            && self.absorbed.events == self.reclaimed.events.saturating_add(self.live.events)
    }
}

/// Cumulative flow through one partition's cache, and what it holds now.
///
/// One snapshot taken under one lock hold, so the parts describe the same
/// instant. Separate accessor calls could not be composed into the flow
/// identity; this can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    absorbed: Tally,
    reclaimed: Tally,
    resident: Tally,
    peak: Tally,
    passes: u64,
    freeing_passes: u64,
}

impl CacheStats {
    /// The flow identity, over map membership.
    ///
    /// Deliberately *not* a statement about heap footprint: a segment reclaimed
    /// from the map stays alive while a reader is still reading it, which is
    /// what lets reclamation never wait on a reader. Actual footprint is this
    /// plus whatever dropped segments readers still hold.
    #[must_use]
    pub fn balances(self) -> bool {
        self.absorbed.bytes == self.reclaimed.bytes.saturating_add(self.resident.bytes)
            && self.absorbed.events == self.reclaimed.events.saturating_add(self.resident.events)
    }

    #[must_use]
    pub fn absorbed(self) -> Tally {
        self.absorbed
    }

    #[must_use]
    pub fn reclaimed(self) -> Tally {
        self.reclaimed
    }

    #[must_use]
    pub fn resident(self) -> Tally {
        self.resident
    }

    #[must_use]
    pub fn peak(self) -> Tally {
        self.peak
    }

    /// Reclamation passes run.
    #[must_use]
    pub fn passes(self) -> u64 {
        self.passes
    }

    /// Passes that actually took something.
    ///
    /// The number that means anything: a maintenance ticker fires constantly
    /// and mostly finds nothing, so counting attempts measures the ticker
    /// rather than the policy.
    #[must_use]
    pub fn freeing_passes(self) -> u64 {
        self.freeing_passes
    }
}

/// What one reclamation pass did, and what the map held when it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReclaimReport {
    dead: Tally,
    gapped: Tally,
    pressured: Tally,
    resident: Tally,
    limit_breached: bool,
}

impl ReclaimReport {
    /// Spans nobody can ask for again. Free.
    #[must_use]
    pub fn dead(self) -> Tally {
        self.dead
    }

    /// Spans taken from between reader clusters. A laggard will refetch these.
    #[must_use]
    pub fn gapped(self) -> Tally {
        self.gapped
    }

    /// Spans taken to hold the byte limit. Someone will refetch these soon.
    #[must_use]
    pub fn pressured(self) -> Tally {
        self.pressured
    }

    /// Everything taken. `gapped` plus `pressured` is the predicted refetch
    /// volume, which is what bounding residency this way actually costs.
    #[must_use]
    pub fn dropped(self) -> Tally {
        self.dead.plus(self.gapped).plus(self.pressured)
    }

    /// Measured under the same lock hold that did the taking, so a report never
    /// describes two different instants.
    #[must_use]
    pub fn resident(self) -> Tally {
        self.resident
    }

    /// The limit still stands breached: everything left is wanted next by some
    /// reader.
    #[must_use]
    pub fn limit_breached(self) -> bool {
        self.limit_breached
    }

    #[must_use]
    pub fn freed_anything(self) -> bool {
        !self.dropped().is_empty()
    }
}

/// One partition's resident segments, keyed by the first sequence each accounts
/// for.
pub struct PartitionCache {
    residency: RwLock<Residency>,
    newest_accounted: watch::Sender<Sequence>,
    readers: Arc<ReaderRegistry>,
    policy: ReclaimPolicy,
}

impl PartitionCache {
    #[must_use]
    /// Returns an `Arc`, because a cache is never held any other way: every
    /// reader holds the partition it reads (D27), and the loader holds it to
    /// absorb into. Handing back a bare `Self` only to have every caller wrap
    /// it made `track_reader` impossible to express.
    pub fn new() -> Arc<Self> {
        Self::with_reclaim_policy(ReclaimPolicy::default())
    }

    /// One argument, so no builder.
    #[must_use]
    pub fn with_reclaim_policy(policy: ReclaimPolicy) -> Arc<Self> {
        let (newest_accounted, _) = watch::channel(Sequence::NONE);
        Arc::new(Self {
            residency: RwLock::new(Residency::new()),
            newest_accounted,
            readers: Arc::new(ReaderRegistry::default()),
            policy,
        })
    }

    /// Recovers a poisoned guard rather than swallowing it.
    ///
    /// Nothing in a critical section here can panic: the arithmetic saturates
    /// and no caller-supplied code runs under the guard. Treating a poisoned
    /// lock as a failure is the worse answer, because the poison is sticky -
    /// one panic would otherwise make every later read answer `Unknown` and
    /// every later total report zero, for the life of the process, silently.
    fn residency(&self) -> RwLockReadGuard<'_, Residency> {
        self.residency
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn residency_mut(&self) -> RwLockWriteGuard<'_, Residency> {
        self.residency
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads forward from `offset`, across as many exactly-adjacent segments as
    /// the limit allows.
    ///
    /// Walking rather than serving one segment is what keeps a batch full now
    /// that segments are fetch-sized and never merged. It also collapses a
    /// retention-trimmed prefix into a single read instead of one read per
    /// fetch span, since a segment whose events are all deleted contributes
    /// accounting and no events.
    #[must_use]
    pub fn read_from(&self, offset: Sequence, limit: ReadLimit) -> PartitionRead {
        // Read before taking the map lock, rather than borrowing the watch
        // channel's own lock underneath it.
        let newest = self.newest_accounted();
        let residency = self.residency();
        let segments = &residency.segments;

        // The map is keyed by each segment's own start, so the candidate is
        // the last segment starting at or below the first position above the
        // reader - a segment that begins exactly there is the common case,
        // since a fetch aimed at a reader starts one above it. Whether that
        // candidate actually answers this reader is `serves`'s question, and
        // it needs no successor.
        let first_above = Sequence::assigned(offset.as_i64().saturating_add(1));
        let serving = segments
            .range(..=first_above)
            .next_back()
            .map(|(_, segment)| Arc::clone(segment))
            .filter(|segment| segment.serves(offset));

        let Some(mut current) = serving else {
            // Nothing serves this reader. Past the frontier of a non-empty
            // map that is the tail, and an append will land there. An empty map
            // is different: it knows nothing, so the position is unknown and
            // wants a fetch, not a wait for an append.
            if !segments.is_empty() && offset >= newest {
                return PartitionRead::NothingNew;
            }
            return PartitionRead::Unknown;
        };

        // Accumulated here and built once. The domain batch has no mutator, so
        // the running totals the residual limit needs live locally rather than
        // being read back out of a half-built value.
        let mut runs: Vec<EventSlice> = Vec::new();
        let mut taken_events = 0;
        let mut taken_bytes = 0;
        let mut frontier = offset;
        let mut cursor = offset;

        loop {
            let remaining = limit.less(taken_events, taken_bytes);
            if taken_events > 0 && remaining.is_filled() {
                break;
            }

            let run = current.read_after(cursor, remaining);
            frontier = run.frontier();
            // Exact, not a heuristic: a run whose frontier is the segment's
            // `through` cannot have left an event behind inside that span.
            let reached_end = frontier == current.through();
            taken_events = taken_events.saturating_add(run.len());
            taken_bytes = taken_bytes.saturating_add(run.bytes());
            runs.push(run);

            if !reached_end {
                break;
            }

            cursor = current.through();
            let next = segments
                .range((Excluded(cursor), Unbounded))
                .next()
                .map(|(_, segment)| Arc::clone(segment))
                .filter(|next| current.is_adjacent_to(next));
            let Some(next) = next else { break };
            current = next;
        }

        // An empty batch from a segment that accounts past the offset is still
        // a hit: the span's remainder was deleted, and the reader may step over
        // it rather than stall.
        PartitionRead::Hit {
            events: EventBatch::from_runs(runs),
            accounted_through: frontier,
        }
    }

    /// Records a fetch.
    ///
    /// Nothing is merged. Two exactly-adjacent segments stay two entries and a
    /// read walks from one into the other, because concatenating their storage
    /// would deep-copy every resident event on every absorb - the steady state
    /// is precisely the adjacent case - and because a single grown segment can
    /// only be reclaimed once every reader has passed all of it.
    pub fn absorb(&self, fetch: AbsorbedFetch) {
        let absorbed = fetch.into_segment();

        let recorded = {
            let mut residency = self.residency_mut();
            let Some(segment) = Self::narrow_to_unaccounted(&residency.segments, absorbed) else {
                // Every sequence the fetch accounted for was already accounted
                // for. Nothing to record, and nothing to publish.
                return;
            };

            debug_assert!(
                segment.index_is_consistent(),
                "a segment's index must describe the events it is parallel to"
            );

            let recorded = segment.span();
            residency.record(&segment);
            let displaced = residency.segments.insert(segment.from(), Arc::new(segment));
            debug_assert!(
                displaced.is_none(),
                "narrowing leaves the incoming span disjoint from every \
                 resident span, so an insert cannot displace one - and a \
                 displaced segment's bytes would leave residency uncounted"
            );
            debug_assert!(
                Self::spans_are_disjoint(&residency.segments),
                "resident spans must stay pairwise disjoint"
            );

            // Absorbing is the only thing that grows residency, so this is
            // where the byte limit has to be held rather than merely audited on
            // the next tick. Guarded on an O(1) total, though: consulting the
            // full plan on every absorb would make absorbing cost O(resident
            // segments), which is the shape this design gave up merging to
            // escape. Only an actual breach pays for a decision, and taking
            // dead spans is left to the maintenance pass.
            let limit = u64::try_from(self.policy.residency_limit_bytes()).unwrap_or(u64::MAX);
            if residency.live.bytes() > limit {
                Self::enforce(
                    &mut residency,
                    &self.readers.positions_ascending(),
                    &self.policy,
                );
            }
            debug_assert!(residency.balances());
            recorded
        };
        // After the guard is dropped, and only for the readers this span can
        // actually answer - by the same predicate reclamation uses to decide
        // what it must not take.
        self.readers.wake_covered(recorded);

        // Monotone, deliberately: recomputing this from the map would make it
        // *fall* when reclamation has dropped the top segment and a laggard
        // then refetches a lower span. A reader that had data would see
        // `has_data` go false and park in `wait` until the next append.
        let through = recorded.through();
        self.newest_accounted.send_if_modified(|current| {
            if through > *current {
                *current = through;
                return true;
            }
            false
        });
    }

    /// The part of `incoming` that no resident segment already accounts for.
    ///
    /// Disjointness is load-bearing now that nothing is merged: two segments
    /// whose spans overlapped would let one walk deliver the overlap twice, and
    /// would make the map's `next_back` lookup pick the wrong segment to serve
    /// a position from. A loader that respects `next_accounted_from` never
    /// produces an overlap; this keeps the invariant true even when something
    /// does.
    ///
    /// Narrowing rather than merging or replacing, because the two directions
    /// are not symmetric. Discarding knowledge is safe - a reader that needs
    /// the discarded span is answered `Unknown`, and the loader fetches it
    /// again. Claiming knowledge nobody established is what loses events.
    fn narrow_to_unaccounted(
        segments: &BTreeMap<Sequence, Arc<Segment>>,
        incoming: Segment,
    ) -> Option<Segment> {
        let mut from = incoming.from();

        // Resident spans are disjoint, so at most one can cover the start.
        if let Some(covering) = segments
            .range(..=from)
            .next_back()
            .map(|(_, segment)| segment)
            .filter(|segment| segment.accounts_for(from))
        {
            from = Sequence::assigned(covering.through().as_i64().saturating_add(1));
        }
        if from > incoming.through() {
            return None;
        }

        // Stop below the next resident span the remainder would run into,
        // rather than trying to fill the holes around it.
        let through = segments
            .range(from..=incoming.through())
            .next()
            .map_or(incoming.through(), |(next_from, _)| {
                Sequence::assigned(next_from.as_i64().saturating_sub(1))
            });

        incoming.trimmed_to(from, through)
    }

    /// Whether resident spans are pairwise disjoint, for the structural
    /// validator. Ordering comes free from the map; only the gap needs testing.
    fn spans_are_disjoint(segments: &BTreeMap<Sequence, Arc<Segment>>) -> bool {
        segments
            .values()
            .zip(segments.values().skip(1))
            .all(|(left, right)| left.through() < right.from())
    }

    /// Runs one reclamation pass: dead spans, then gaps between reader
    /// clusters, then the byte limit.
    ///
    /// Reclamation never waits on a reader. Segments are `Arc`, so dropping one
    /// from the map leaves a reader still holding it able to finish, and the
    /// memory is freed when the last holder lets go. A reader can therefore
    /// never block reclamation of anything but what it is currently reading -
    /// which is what lets residency be bounded without a pinning protocol.
    pub fn reclaim(&self) -> ReclaimReport {
        let readers = self.readers.positions_ascending();
        let mut residency = self.residency_mut();
        let report = Self::enforce(&mut residency, &readers, &self.policy);

        residency.passes = residency.passes.saturating_add(1);
        if report.freed_anything() {
            residency.freeing_passes = residency.freeing_passes.saturating_add(1);
        }
        debug_assert!(residency.balances());
        report
    }

    /// Applies one pass to `residency`. Shared by `reclaim` and `absorb`, so
    /// the limit is held by one implementation with two callers.
    fn enforce(
        residency: &mut Residency,
        readers: &[Sequence],
        policy: &ReclaimPolicy,
    ) -> ReclaimReport {
        let decided = plan(&residency.summaries(), readers, policy);

        let dead = residency.take(decided.dead());
        let gapped = residency.take(decided.gapped());
        let pressured = residency.take(decided.pressured());

        ReclaimReport {
            dead,
            gapped,
            pressured,
            resident: residency.live,
            limit_breached: decided.limit_breached(),
        }
    }

    /// What this partition wants fetched, and how badly.
    ///
    /// A scan rather than a query: it ages every reader nothing resident can
    /// answer, which is how starvation accumulates once per scan instead of
    /// once per scheduling decision. A reader that is subsequently served
    /// clears its own counter when it advances, so the count measures
    /// consecutive unserved scans.
    ///
    /// The registry lock is taken and released before the residency lock, never
    /// held across it - `absorb` acquires them the other way round.
    #[must_use]
    pub fn scan_demands(&self) -> Vec<Demand> {
        let needs = self.readers.needs();
        if needs.is_empty() {
            return Vec::new();
        }
        let frontier = self.newest_accounted();
        let summaries = self.residency().summaries();

        for (state, need) in &needs {
            if !accounted(&summaries, need.position()) {
                state.starved_rounds.fetch_add(1, Ordering::Relaxed);
            }
        }

        let wants: Vec<ReaderNeed> = needs.iter().map(|(_, need)| *need).collect();
        derive(&summaries, &wants, frontier)
    }

    /// Events resident ahead of `offset`.
    ///
    /// Counted across every segment that reaches past it, never derived from the
    /// span's ends: spans are sparse, so subtracting sequences gives an upper
    /// bound rather than a count - and a fetch sized against an upper bound
    /// under-fetches, which costs the reader a round trip.
    #[must_use]
    pub fn resident_events_after(&self, offset: Sequence) -> usize {
        self.residency()
            .segments
            .values()
            .filter(|segment| segment.through() > offset)
            .map(|segment| segment.events_after(offset))
            .fold(0, usize::saturating_add)
    }

    /// The worst starvation any registered reader has accumulated.
    ///
    /// Exposed because it is the number that says whether the scheduler is
    /// actually fair: a bounded maximum is the claim, and an unbounded one is
    /// the failure it hides.
    #[must_use]
    pub fn worst_starvation(&self) -> u32 {
        self.readers
            .snapshot()
            .iter()
            .map(|state| state.starved_rounds.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0)
    }

    /// One consistent snapshot of the flow through this cache.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let residency = self.residency();
        CacheStats {
            absorbed: residency.absorbed,
            reclaimed: residency.reclaimed,
            resident: residency.live,
            peak: residency.peak,
            passes: residency.passes,
            freeing_passes: residency.freeing_passes,
        }
    }

    /// Registers a reader at `offset`. Dropping the handle deregisters it.
    #[must_use]
    pub fn track_reader(self: &Arc<Self>, offset: Sequence) -> ReaderHandle {
        self.track_reader_sharing(offset, Arc::new(Notify::new()))
    }

    /// Registers a reader that wakes through `ready` rather than a waker of its
    /// own.
    ///
    /// For a session holding one reader per partition it was assigned: pass the
    /// same waker for all of them and a single await covers every partition,
    /// instead of a select over one future each. Two arguments, but a sequence
    /// and a waker cannot be transposed.
    #[must_use]
    pub fn track_reader_sharing(
        self: &Arc<Self>,
        offset: Sequence,
        ready: Arc<Notify>,
    ) -> ReaderHandle {
        let (id, state) = self.readers.register(offset, ready);
        ReaderHandle {
            id,
            registry: Arc::clone(&self.readers),
            cache: Arc::clone(self),
            newest_accounted: self.newest_accounted.subscribe(),
            state,
        }
    }

    /// Lowest registered reader position, the loader's runway input.
    #[must_use]
    pub fn slowest_reader(&self) -> Option<Sequence> {
        self.readers.slowest()
    }

    #[must_use]
    pub fn reader_count(&self) -> usize {
        self.readers.count()
    }

    #[must_use]
    pub fn watch_newest_accounted(&self) -> watch::Receiver<Sequence> {
        self.newest_accounted.subscribe()
    }

    #[must_use]
    pub fn newest_accounted(&self) -> Sequence {
        *self.newest_accounted.borrow()
    }

    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.residency().segments.len()
    }

    #[must_use]
    /// Now a constant-time read of a maintained total rather than a walk of the
    /// whole map, which is what lets a byte limit be checked on every absorb.
    pub fn resident_bytes(&self) -> usize {
        usize::try_from(self.residency().live.bytes()).unwrap_or(usize::MAX)
    }

    /// Accounted spans currently resident, ascending.
    #[must_use]
    pub fn spans(&self) -> Vec<(Sequence, Sequence)> {
        self.residency()
            .segments
            .values()
            .map(|segment| (segment.from(), segment.through()))
            .collect()
    }

    /// How many holders one resident segment's event storage has.
    ///
    /// Exposed deliberately: the claim that reclamation never waits on a
    /// reader, and that a dropped segment survives exactly as long as someone
    /// is reading it, is only checkable by counting holders.
    #[must_use]
    pub fn segment_holders(&self, from: Sequence) -> Option<usize> {
        self.residency()
            .segments
            .get(&from)
            .map(|segment| segment.storage_holders())
    }
}

/// `ReaderHandle` is the production `PartitionReader`. Every method already
/// exists; this states the conformance so a session can hold one behind the
/// domain trait and a test can substitute a stub.
impl crate::domain::streaming::reader::PartitionReader for ReaderHandle {
    fn has_data(&self) -> bool {
        Self::has_data(self)
    }

    fn read(&self, limit: ReadLimit) -> PartitionRead {
        Self::read(self, limit)
    }

    fn seek(&self, offset: Sequence) {
        Self::seek(self, offset);
    }

    fn report_scanning(&self, scanning: bool) {
        Self::report_scanning(self, scanning);
    }
}
