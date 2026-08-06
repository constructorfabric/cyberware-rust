//! One span of a partition that the cache has fully accounted for.
//!
//! A segment records the span it has accounted for - `from..=through` - and
//! holds the events present within it. Sparseness inside the span is therefore
//! implicit and exact: a sequence in range that is not in `events` has been
//! deleted, and needs no marker to say so. Absence *outside* every segment is a
//! different thing entirely, and is unknown rather than absent.
//!
//! Recording the accounted span rather than a dense range is what keeps
//! adjacency an exact test: one segment's span ends immediately before the
//! next's begins, whatever holes either contains. No synthetic event is needed
//! to bridge a gap.

use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::domain::model::{Event, Sequence};
use crate::domain::streaming::read::{EventSlice, ReadLimit};

use super::span::AccountedSpan;

/// Per-event lookup data, kept beside the events rather than read out of them.
///
/// Sixteen bytes an entry, so a fetch-sized segment's index is a few kilobytes
/// and stays in cache where an array of ~350-byte events does not. Both the
/// sequence search and the byte bound read this and never touch an `Event`.
///
/// Bytes are cumulative rather than per-event so that a byte-bounded read is a
/// second `partition_point` over a monotone run instead of a walk that adds as
/// it goes, and so that any run's footprint is one subtraction. Cumulating is
/// exact here, unlike counting events across a sparse span, because every
/// event's footprint is known and none is missing.
#[derive(Debug, Clone, Copy)]
struct EventIndex {
    sequence: Sequence,
    cumulative_bytes: usize,
}

/// A span the cache has accounted for, and the events present in it.
pub struct Segment {
    from: Sequence,
    through: Sequence,
    events: Arc<[Event]>,
    /// Parallel to `events`: same length, same order, ascending in both fields.
    index: Arc<[EventIndex]>,
}

impl Segment {
    /// Built through [`SegmentBuilder`] rather than a constructor: `from` and
    /// `through` are both sequences, and a positional pair would let a caller
    /// transpose them and silently invert the span.
    #[must_use]
    pub fn builder() -> SegmentBuilder {
        SegmentBuilder {
            from: Sequence::assigned(0),
            through: Sequence::assigned(0),
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn from(&self) -> Sequence {
        self.from
    }

    #[must_use]
    pub fn through(&self) -> Sequence {
        self.through
    }

    /// Accounted footprint of every event held. One index read.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes_before(self.index.len())
    }

    /// Events present in the span. Counted, never derived from the span's ends.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.index.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Re-spans this segment without touching its events.
    ///
    /// A fetch often accounts for more than it returned - it proves a range
    /// empty below its first surviving event - and the widened segment holds
    /// exactly the same storage. Consuming and reusing the `Arc`s avoids
    /// copying a whole batch on every absorb.
    #[must_use]
    pub fn with_span(self, from: Sequence, through: Sequence) -> Self {
        Self {
            from: from.min(self.from),
            through: through.max(self.through),
            events: self.events,
            index: self.index,
        }
    }

    /// This segment narrowed to `from..=through`, or `None` when that span is
    /// empty.
    ///
    /// Returns itself untouched, copying nothing, when the given span already
    /// contains it - which is the only path a well-behaved loader takes. The
    /// copy on the narrowing path is the price of a backstop that should never
    /// fire.
    #[must_use]
    pub fn trimmed_to(self, from: Sequence, through: Sequence) -> Option<Self> {
        if from > through {
            return None;
        }
        if from <= self.from && through >= self.through {
            return Some(self);
        }

        let events = self
            .events
            .iter()
            .filter(|event| {
                event
                    .sequence
                    .is_some_and(|sequence| sequence >= from && sequence <= through)
            })
            .cloned()
            .collect();

        Some(
            Self::builder()
                .from(from)
                .through(through)
                .events(events)
                .build(),
        )
    }

    /// How many holders this segment's event storage has, itself included.
    ///
    /// The count that matters for reclamation: an [`EventSlice`] keeps the
    /// *storage* alive, not the segment wrapper, so counting the wrapper would
    /// measure the wrong thing.
    #[must_use]
    pub fn storage_holders(&self) -> usize {
        Arc::strong_count(&self.events)
    }

    /// The range this segment has accounted for.
    #[must_use]
    pub fn span(&self) -> AccountedSpan {
        AccountedSpan::builder(self.from)
            .through(self.through)
            .build()
    }

    /// Whether this segment answers what a reader at `position` wants next.
    /// Delegates to the span's own predicate, which is the single home for
    /// reader-versus-span questions.
    #[must_use]
    pub fn serves(&self, position: Sequence) -> bool {
        self.span().serves(position)
    }

    /// Whether this segment accounts for `sequence` - that is, whether a reader
    /// positioned there can be answered from it, either with an event or with
    /// the knowledge that none exists.
    #[must_use]
    pub fn accounts_for(&self, sequence: Sequence) -> bool {
        self.span().contains(sequence)
    }

    /// Whether this segment's span ends immediately before `next` begins, so a
    /// read may cross from one into the other.
    #[must_use]
    pub fn is_adjacent_to(&self, next: &Self) -> bool {
        self.span().is_adjacent_to(next.span())
    }

    /// Events after `offset`, up to `limit`.
    ///
    /// Two binary searches and no walk: one for the position, because the span
    /// is sparse so an offset does not imply an index, and one for the byte
    /// bound over the cumulative index.
    #[must_use]
    pub fn read_after(&self, offset: Sequence, limit: ReadLimit) -> EventSlice {
        let start = self.partition_point_after(offset);
        let base = self.bytes_before(start);
        let ceiling = base.saturating_add(limit.max_bytes());

        let by_bytes = self
            .index
            .partition_point(|entry| entry.cumulative_bytes <= ceiling);
        let by_events = start.saturating_add(limit.max_events());
        let mut end = by_bytes.min(by_events).min(self.index.len());

        // A byte bound must never return nothing while an event remains: the
        // reader would stall forever on an event it can never fit.
        if end == start && start < self.index.len() {
            end = start.saturating_add(1);
        }

        EventSlice::builder(Arc::clone(&self.events))
            .range(start..end)
            .bytes(self.bytes_before(end).saturating_sub(base))
            .frontier(self.frontier_at(end, offset))
            .build()
    }

    /// How many events are present after `offset`.
    ///
    /// Counted, never `through - offset`: the span is sparse, so subtracting
    /// sequences gives an upper bound rather than a count, and runway sized
    /// from an upper bound over-reports what a reader can actually consume.
    #[must_use]
    pub fn events_after(&self, offset: Sequence) -> usize {
        self.index.len() - self.partition_point_after(offset)
    }

    /// Index of the first event with a sequence strictly greater than `offset`.
    fn partition_point_after(&self, offset: Sequence) -> usize {
        self.index.partition_point(|entry| entry.sequence <= offset)
    }

    /// Cumulative footprint of everything before index `at`.
    fn bytes_before(&self, at: usize) -> usize {
        at.checked_sub(1)
            .and_then(|last| self.index.get(last))
            .map_or(0, |entry| entry.cumulative_bytes)
    }

    /// How far a run ending at index `end` accounts for.
    ///
    /// Reaching the end of the events means the rest of the span was deleted
    /// and is accounted for, so the whole span's `through` is safe. Stopping
    /// short means a limit intervened, and only what was delivered is safe.
    fn frontier_at(&self, end: usize, offset: Sequence) -> Sequence {
        if end == self.index.len() {
            return self.through;
        }
        end.checked_sub(1)
            .and_then(|last| self.index.get(last))
            .map_or(offset, |entry| entry.sequence)
    }

    /// Whether the index still describes the events it is parallel to.
    ///
    /// For the structural validator: lengths, strict sequence order, and
    /// agreement with the events themselves. Cheap enough to assert on every
    /// mutation in a debug build, which is where this class of bug is caught.
    #[must_use]
    pub fn index_is_consistent(&self) -> bool {
        self.index.len() == self.events.len()
            && self.index.windows(2).all(|pair| match pair {
                [left, right] => {
                    left.sequence < right.sequence
                        && left.cumulative_bytes <= right.cumulative_bytes
                }
                _ => true,
            })
            && self
                .index
                .iter()
                .zip(self.events.iter())
                .all(|(entry, event)| event.sequence == Some(entry.sequence))
    }
}

/// Conservative upper bound on one event's resident footprint.
///
/// Counted once, when a segment is built, and never on a read path. Two things
/// matter and measuring the payload alone got both wrong. The figure is what
/// the residency limit is enforced against, so counting only `data`
/// under-reports every event and lets the cache hold more than it believes -
/// for a small payload the envelope dominates, and two GTS ids alone run to
/// tens of bytes. And obtaining it must not cost a serialization, or the byte
/// bound becomes more expensive than the delivery it bounds.
///
/// Deliberately one number for two jobs. It bounds resident memory, and it
/// bounds a delivered batch, which are different quantities - wire bytes are
/// smaller than in-memory footprint. Erring high is safe for both: residency
/// never under-counts, and a batch merely comes out under its byte cap. An
/// encoder needing exact framing bytes should measure the frame it encoded.
fn event_footprint(event: &Event) -> usize {
    [
        std::mem::size_of::<Event>(),
        event.r#type.as_ref().len(),
        event.topic.as_ref().len(),
        event.source.len(),
        event.subject.len(),
        event.subject_type.len(),
        event.trace_parent.as_ref().map_or(0, String::len),
        json_footprint(&event.data),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

/// Heap footprint of a JSON tree.
///
/// Nulls, booleans and numbers live inline in their parent's node and add
/// nothing of their own. Recursion depth is bounded by the parse that produced
/// the tree, which enforces a nesting limit before any of this is reached.
fn json_footprint(value: &JsonValue) -> usize {
    match value {
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => 0,
        JsonValue::String(text) => text.len(),
        JsonValue::Array(items) => items
            .iter()
            .map(|item| std::mem::size_of::<JsonValue>().saturating_add(json_footprint(item)))
            .fold(0, usize::saturating_add),
        JsonValue::Object(entries) => entries
            .iter()
            .map(|(key, item)| {
                key.len()
                    .saturating_add(std::mem::size_of::<JsonValue>())
                    .saturating_add(json_footprint(item))
            })
            .fold(0, usize::saturating_add),
    }
}

/// Collects a segment's parts so the span's two ends are named at the call site
/// rather than positional.
pub struct SegmentBuilder {
    from: Sequence,
    through: Sequence,
    events: Vec<Event>,
}

impl SegmentBuilder {
    /// The segment begins with whatever the store held after `position` - the
    /// caller names the position it has, not the sequence it expects next.
    #[must_use]
    pub fn starting_after(mut self, position: Sequence) -> Self {
        self.from = Sequence::assigned(position.as_i64().saturating_add(1));
        self
    }

    /// First sequence of the accounted span.
    #[must_use]
    pub fn from(mut self, from: Sequence) -> Self {
        self.from = from;
        self
    }

    /// Last sequence of the accounted span, inclusive.
    #[must_use]
    pub fn through(mut self, through: Sequence) -> Self {
        self.through = through;
        self
    }

    /// Events present in the span, which may be sparse within it. Sorted and
    /// deduplicated on build, so a caller cannot produce a segment whose
    /// lookups misbehave.
    #[must_use]
    pub fn events(mut self, events: Vec<Event>) -> Self {
        self.events = events;
        self
    }

    #[must_use]
    pub fn build(mut self) -> Segment {
        self.events
            .sort_by_key(|event| event.sequence.unwrap_or(Sequence::NONE));
        self.events
            .dedup_by_key(|event| event.sequence.unwrap_or(Sequence::NONE));

        // Built after the sort and dedup, so the index describes the events in
        // the order they will actually be searched.
        let mut running: usize = 0;
        let index: Vec<EventIndex> = self
            .events
            .iter()
            .map(|event| {
                running = running.saturating_add(event_footprint(event));
                EventIndex {
                    sequence: event.sequence.unwrap_or(Sequence::NONE),
                    cumulative_bytes: running,
                }
            })
            .collect();

        Segment {
            from: self.from,
            through: self.through.max(self.from),
            events: self.events.into(),
            index: index.into(),
        }
    }
}
