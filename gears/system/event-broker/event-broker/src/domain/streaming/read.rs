//! What a read of one partition returns, and how much of it may be taken.
//!
//! These live in `domain` because they are the *contract* a session reads
//! through, not an implementation detail of the thing that serves it. An earlier
//! draft put them in `infra/partition_cache` and had the domain trait name them,
//! which inverted the layers: `domain` would have depended on `infra` to state
//! its own interface.
//!
//! They are shared rather than converted at the boundary. A conversion would
//! mean copying, and copying a batch per session is exactly what the cache
//! exists to avoid - with a thousand sessions on one partition it would multiply
//! resident memory by the subscriber count.

use std::ops::Range;
use std::sync::Arc;

use crate::domain::model::{Event, Sequence};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxEvents(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxBytes(pub usize);

/// How much one read may return. Distinct argument types rather than two bare
/// counts, so the two bounds cannot be transposed silently.
#[derive(Debug, Clone, Copy)]
pub struct ReadLimit {
    max_events: usize,
    max_bytes: usize,
}

impl ReadLimit {
    #[must_use]
    pub fn new(events: MaxEvents, bytes: MaxBytes) -> Self {
        Self {
            max_events: events.0,
            max_bytes: bytes.0,
        }
    }

    /// Wide enough that only the caller's own position bounds a read.
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            max_events: usize::MAX,
            max_bytes: usize::MAX,
        }
    }

    #[must_use]
    pub fn max_events(self) -> usize {
        self.max_events
    }

    #[must_use]
    pub fn max_bytes(self) -> usize {
        self.max_bytes
    }

    /// What is left of this limit after `events` events totalling `bytes`.
    #[must_use]
    pub fn less(self, events: usize, bytes: usize) -> Self {
        Self {
            max_events: self.max_events.saturating_sub(events),
            max_bytes: self.max_bytes.saturating_sub(bytes),
        }
    }

    /// Whether nothing more may be taken.
    ///
    /// Only meaningful against a read that already returned something: a limit
    /// narrower than the next event must still yield that event, or a reader
    /// stalls forever on something it can never fit.
    #[must_use]
    pub fn is_filled(self) -> bool {
        self.max_events == 0 || self.max_bytes == 0
    }
}

/// A run of events within one resident span, keeping that span's storage alive
/// for as long as the run is held. Sharing rather than copying is the point:
/// many readers of one partition must not multiply its resident memory.
#[derive(Debug, Clone)]
pub struct EventSlice {
    events: Arc<[Event]>,
    range: Range<usize>,
    bytes: usize,
    frontier: Sequence,
}

impl EventSlice {
    /// Built through a builder rather than a constructor: four fields, two of
    /// them bare counts, and the storage is the only one with no sensible
    /// default.
    #[must_use]
    pub fn builder(events: Arc<[Event]>) -> EventSliceBuilder {
        EventSliceBuilder {
            events,
            range: 0..0,
            bytes: 0,
            frontier: Sequence::assigned(0),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    /// Footprint of the run, measured when the run was cut. Nothing
    /// re-measures, and nothing re-serializes.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The highest sequence this run lets a reader advance to.
    ///
    /// The end of the span it came from when the run reached that end, and the
    /// last *delivered* event's sequence when a read limit stopped it short. The
    /// distinction is the whole point: a run that ended at the span's end
    /// accounts for a deleted tail as well, while a run that ended at a limit
    /// accounts only for what it handed over. Advancing a reader past an event a
    /// limit withheld would lose it silently.
    #[must_use]
    pub fn frontier(&self) -> Sequence {
        self.frontier
    }

    #[must_use]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Event> {
        self.events
            .get(self.range.clone())
            .unwrap_or_default()
            .iter()
    }

    /// Copies the run out. Never on a read path - the whole point of a slice is
    /// that delivery does not copy.
    #[must_use]
    pub fn cloned(&self) -> Vec<Event> {
        self.iter().cloned().collect()
    }

    /// Highest sequence *present* in the run, or `None` when it is empty.
    ///
    /// Not the advance frontier - see [`Self::frontier`]. This is for
    /// observability; a reader that advances on this re-reads proven-absent
    /// tails forever.
    #[must_use]
    pub fn last_sequence(&self) -> Option<Sequence> {
        self.iter().next_back().and_then(|event| event.sequence)
    }
}

pub struct EventSliceBuilder {
    events: Arc<[Event]>,
    range: Range<usize>,
    bytes: usize,
    frontier: Sequence,
}

impl EventSliceBuilder {
    #[must_use]
    pub fn range(mut self, range: Range<usize>) -> Self {
        self.range = range;
        self
    }

    #[must_use]
    pub fn bytes(mut self, bytes: usize) -> Self {
        self.bytes = bytes;
        self
    }

    #[must_use]
    pub fn frontier(mut self, frontier: Sequence) -> Self {
        self.frontier = frontier;
        self
    }

    #[must_use]
    pub fn build(self) -> EventSlice {
        EventSlice {
            events: self.events,
            range: self.range,
            bytes: self.bytes,
            frontier: self.frontier,
        }
    }
}

/// Events from one or more exactly-adjacent resident spans, ascending, copied
/// nowhere.
///
/// A sequence of runs rather than one contiguous slice because spans are never
/// physically merged: adjacency is derived, so a read spanning four fetches
/// borrows four storages instead of concatenating them. Callers see one flat
/// iterator and never learn how many runs it took.
#[derive(Debug, Clone, Default)]
pub struct EventBatch {
    runs: Vec<EventSlice>,
    events: usize,
    bytes: usize,
}

impl EventBatch {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Collects runs into a batch, dropping empty ones.
    ///
    /// An empty run is real and means something - a span whose contents were all
    /// deleted contributes accounting rather than events - but it is not part of
    /// what was delivered, so a caller counting [`Self::runs`] should not see it.
    #[must_use]
    pub fn from_runs(runs: Vec<EventSlice>) -> Self {
        let kept: Vec<EventSlice> = runs.into_iter().filter(|run| !run.is_empty()).collect();
        let events = kept
            .iter()
            .map(EventSlice::len)
            .fold(0, usize::saturating_add);
        let bytes = kept
            .iter()
            .map(EventSlice::bytes)
            .fold(0, usize::saturating_add);
        Self {
            runs: kept,
            events,
            bytes,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events == 0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events
    }

    /// Total footprint of the events in this batch, by the same measure the read
    /// limit was applied against.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many spans this read crossed. Observability: a persistently rising
    /// value means fetch size and read size disagree.
    #[must_use]
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    #[must_use]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Event> {
        self.runs.iter().flat_map(EventSlice::iter)
    }

    /// Highest sequence *present* in the batch. Not the reader's advance
    /// frontier - that is [`PartitionRead::Hit::accounted_through`], which may be
    /// higher because it also covers proven-absent tails.
    #[must_use]
    pub fn last_sequence(&self) -> Option<Sequence> {
        self.runs.last().and_then(EventSlice::last_sequence)
    }
}

/// What a read of one partition found.
#[derive(Debug)]
pub enum PartitionRead {
    /// Served from one or more exactly-adjacent spans accounting for the
    /// requested position.
    Hit {
        events: EventBatch,
        /// How far the reader may advance its scan frontier.
        ///
        /// The last span walked, when the walk consumed that span whole, and the
        /// last *delivered* sequence when a read limit stopped the walk inside
        /// it. Never the newest sequence known: advancing past an event a limit
        /// withheld loses it, and advancing across a span nobody has accounted
        /// for loses whatever is in it.
        accounted_through: Sequence,
    },
    /// The position is accounted for and nothing follows it yet: the tail.
    NothingNew,
    /// Nothing accounts for the position. The reader must wait for a fetch; it
    /// may not advance.
    ///
    /// Carries nothing. It once reported where accounting resumed, so a reader
    /// could tell the loader what to fetch - but demands are derived from what
    /// is resident rather than reported by readers, so the loader already knows.
    /// What is left is the reader-side safety property: this position cannot be
    /// answered, and stepping over it would lose whatever is in it.
    Unknown,
}
