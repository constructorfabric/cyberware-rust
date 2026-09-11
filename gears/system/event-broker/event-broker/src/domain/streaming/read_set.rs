//! The partitions one session is reading, and whose turn it is.
//!
//! Fairness is rotation over the partitions that are *ready*, not over all of
//! them. A partition with nothing to read is skipped rather than consuming a
//! turn, so one busy partition cannot starve its siblings and an idle one costs
//! a comparison. When nothing is ready the session goes idle rather than
//! spinning - which is why `next_to_read` returns an `Option` rather than
//! always naming a partition.

use std::sync::Arc;

use crate::domain::model::Sequence;
use crate::domain::streaming::frames::Position;
use crate::domain::streaming::reader::PartitionReader;
use crate::domain::streaming::source::PartitionKey;

/// What one batch achieved, as the session measured it.
///
/// Both frontiers are reported because they diverge exactly when a filter
/// rejects events, and that divergence is the whole reason a progress frame
/// exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchOutcome {
    delivered_through: Sequence,
    examined_through: Sequence,
    matched: usize,
    examined: usize,
}

impl BatchOutcome {
    /// One argument, the position the batch examined up to; what was delivered
    /// and the counts are set through the builder so no pair can be transposed.
    #[must_use]
    pub fn builder(examined_through: Sequence) -> BatchOutcomeBuilder {
        BatchOutcomeBuilder {
            delivered_through: examined_through,
            examined_through,
            matched: 0,
            examined: 0,
        }
    }

    #[must_use]
    pub fn delivered_through(self) -> Sequence {
        self.delivered_through
    }

    #[must_use]
    pub fn examined_through(self) -> Sequence {
        self.examined_through
    }

    #[must_use]
    pub fn matched(self) -> usize {
        self.matched
    }

    /// Events the filter rejected: examined, and not delivered.
    #[must_use]
    pub fn skipped(self) -> usize {
        self.examined.saturating_sub(self.matched)
    }

    #[must_use]
    pub fn examined(self) -> usize {
        self.examined
    }
}

pub struct BatchOutcomeBuilder {
    delivered_through: Sequence,
    examined_through: Sequence,
    matched: usize,
    examined: usize,
}

impl BatchOutcomeBuilder {
    #[must_use]
    pub fn delivered_through(mut self, delivered: Sequence) -> Self {
        self.delivered_through = delivered;
        self
    }

    #[must_use]
    pub fn counts(mut self, matched: usize, examined: usize) -> Self {
        self.matched = matched;
        self.examined = examined.max(matched);
        self
    }

    #[must_use]
    pub fn build(self) -> BatchOutcome {
        BatchOutcome {
            delivered_through: self.delivered_through,
            // Nothing can be delivered without being examined, so the frontier
            // cannot trail the cursor.
            examined_through: self.examined_through.max(self.delivered_through),
            matched: self.matched,
            examined: self.examined,
        }
    }
}

/// One partition a session holds.
pub struct PartitionSlot {
    key: PartitionKey,
    /// Events examined and not delivered since this partition last reported a
    /// position. Reset when a progress frame carries it, so the measure is
    /// "owed a frame" rather than a total that trips on every check once it
    /// has been passed.
    undelivered: usize,
    /// One handle, not two. A second handle used to sit beside this one
    /// carrying the read, so a read went around the reader and the position was
    /// then pushed back through it as a separate call the caller had to
    /// remember - and forgetting it pinned the partition's memory silently. The
    /// reader is the seam.
    reader: Arc<dyn PartitionReader>,
    offset: Sequence,
    last_examined: Sequence,
    /// Set when this partition has had its turn and should yield to the others
    /// before taking another.
    throttled: bool,
}

impl PartitionSlot {
    /// Three arguments of mutually distinguishable types, so none can be passed
    /// in another's place. The starting offset is a chained setter because it
    /// has a sensible default - the beginning.
    #[must_use]
    pub fn new(key: PartitionKey, reader: Arc<dyn PartitionReader>) -> Self {
        Self {
            key,
            reader,
            undelivered: 0,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
            throttled: false,
        }
    }

    /// Seeds the cursor, which comes from the cursor store and never from a
    /// field carried on an assignment.
    #[must_use]
    pub fn starting_at(mut self, offset: Sequence) -> Self {
        self.offset = offset;
        self.last_examined = offset;
        // The reader owns the position it reads from, so seeding the slot alone
        // would leave the two disagreeing and the first read would start from
        // the beginning.
        self.reader.seek(offset);
        self
    }

    #[must_use]
    pub fn key(&self) -> &PartitionKey {
        &self.key
    }

    #[must_use]
    pub fn reader(&self) -> &Arc<dyn PartitionReader> {
        &self.reader
    }

    #[must_use]
    pub fn offset(&self) -> Sequence {
        self.offset
    }

    #[must_use]
    pub fn last_examined(&self) -> Sequence {
        self.last_examined
    }

    #[must_use]
    pub fn position(&self) -> Position {
        Position::builder(self.key.topic.clone(), self.key.partition)
            .offset(self.offset)
            .last_examined(self.last_examined)
            .build()
    }

    /// Events this partition examined and did not deliver since it last
    /// reported a position. The measure a progress frame is owed on.
    #[must_use]
    pub fn drift(&self) -> usize {
        self.undelivered
    }

    fn is_ready(&self) -> bool {
        !self.throttled && self.reader.has_data()
    }
}

/// Every partition one session is reading.
pub struct ReadSet {
    slots: Vec<PartitionSlot>,
    /// Where the next rotation begins, so no partition is permanently first.
    cursor: usize,
}

impl ReadSet {
    /// One argument. A session opens one reader per assigned partition and hands
    /// the whole set over at once.
    #[must_use]
    pub fn seed(slots: Vec<PartitionSlot>) -> Self {
        Self { slots, cursor: 0 }
    }

    /// The next partition to read, or `None` when none is ready.
    ///
    /// `None` means go idle, not spin. Rotation advances only when a partition
    /// is chosen, so an unready one does not consume a turn - which is what lets
    /// a set with one busy partition and fifteen idle ones cost fifteen
    /// comparisons rather than fifteen wasted rounds.
    pub fn next_to_read(&mut self) -> Option<usize> {
        let count = self.slots.len();
        for step in 0..count {
            let index = (self.cursor + step) % count;
            if self.slots.get(index).is_some_and(PartitionSlot::is_ready) {
                self.cursor = (index + 1) % count;
                return Some(index);
            }
        }
        None
    }

    #[must_use]
    pub fn slot(&self, index: usize) -> Option<&PartitionSlot> {
        self.slots.get(index)
    }

    /// Records what a batch achieved, advancing both the cursor and the
    /// frontier, and publishing the position for the loader.
    pub fn record_batch(&mut self, index: usize, outcome: BatchOutcome) {
        if let Some(slot) = self.slots.get_mut(index) {
            // Monotonic. A read that delivered nothing but examined a
            // proven-absent range still moves the frontier, and neither value
            // may go backwards.
            slot.offset = slot.offset.max(outcome.delivered_through());
            slot.last_examined = slot.last_examined.max(outcome.examined_through());
            // Counted as the filter rejected them, not derived afterwards from
            // the two frontiers: their difference is a distance in sequence
            // space, and a partition's sequences are assigned contiguously but
            // not populated contiguously, so that distance is not a number of
            // events at all.
            slot.undelivered = slot.undelivered.saturating_add(outcome.skipped());
            // Nothing is published to the reader: `read` advanced it to the
            // same frontier as part of the read itself.
            // Deliberately nothing published here. The reader advanced itself
            // to the examined frontier as part of `read`, which is what the
            // loader sizes runway from and what reclamation retains against.
            //
            // That the reader tracks the *examined* frontier rather than the
            // delivered cursor is the load-bearing part: it will never read
            // below what it has already filtered past, so a subscription
            // matching one event in a million does not hold a whole partition
            // resident while delivering almost nothing. The persisted cursor
            // stays `offset`, so a *new* session resumes from there and
            // refetches whatever was reclaimed meanwhile - bounded memory now,
            // a refetch after a restart.
        }
    }

    /// Marks a partition as having had its turn.
    pub fn mark_throttled(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.throttled = true;
        }
    }

    /// Clears every throttle, opening a new round.
    pub fn open_round(&mut self) {
        for slot in &mut self.slots {
            slot.throttled = false;
        }
    }

    /// Whether any partition is ready to be read.
    #[must_use]
    pub fn any_ready(&self) -> bool {
        self.slots.iter().any(PartitionSlot::is_ready)
    }

    /// Drops every partition not in `retained`, keeping cursors for those that
    /// remain.
    ///
    /// A loss must not disturb the partitions a session keeps - their cursors
    /// carry on unchanged across a rebalance, which is what makes a non-terminal
    /// topology frame safe to continue from.
    pub fn retain(&mut self, retained: &[PartitionKey]) {
        self.slots.retain(|slot| retained.contains(slot.key()));
        self.cursor = 0;
    }

    #[must_use]
    pub fn list_positions(&self) -> Vec<Position> {
        self.slots.iter().map(PartitionSlot::position).collect()
    }

    /// The partitions whose frontier has run at least `threshold` beyond what
    /// they delivered.
    #[must_use]
    pub fn list_drifted(&mut self, threshold: usize) -> Vec<Position> {
        let mut drifted = Vec::new();
        for slot in &mut self.slots {
            if slot.drift() >= threshold {
                drifted.push(slot.position());
                // Reported, so the count starts again: what the next frame is
                // owed on is what has been examined since this one.
                slot.undelivered = 0;
            }
        }
        drifted
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }
}
