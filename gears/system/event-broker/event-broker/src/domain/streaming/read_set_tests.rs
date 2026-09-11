//! Pure: one stub reader per partition, no cache and no runtime.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use toolkit_gts::GtsInstanceId;

use crate::domain::model::Sequence;
use crate::domain::streaming::read::{PartitionRead, ReadLimit};
use crate::domain::streaming::reader::PartitionReader;
use crate::domain::streaming::source::PartitionKey;

use super::read_set::{BatchOutcome, PartitionSlot, ReadSet};

fn key(partition: i32) -> PartitionKey {
    PartitionKey::new(
        GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
            .expect("static topic id is valid"),
        partition,
    )
}

/// One stub, because there is one seam. This was a `StubReader` plus a
/// `StubSource` - two handles per partition, which is exactly the shape the
/// production code no longer has.
///
/// The read set decides *whose* turn it is, not what a read returns, so `read`
/// here reports `NothingNew` and records only that it was called and from where.
struct StubReader {
    ready: AtomicBool,
    position: AtomicI64,
    reads: AtomicI64,
}

impl StubReader {
    fn new(ready: bool) -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(ready),
            position: AtomicI64::new(0),
            reads: AtomicI64::new(0),
        })
    }

    fn position(&self) -> Sequence {
        Sequence::assigned(self.position.load(Ordering::Relaxed))
    }
}

impl PartitionReader for StubReader {
    fn has_data(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn read(&self, _limit: ReadLimit) -> PartitionRead {
        self.reads.fetch_add(1, Ordering::Relaxed);
        PartitionRead::NothingNew
    }

    fn seek(&self, offset: Sequence) {
        self.position.store(offset.as_i64(), Ordering::Relaxed);
    }

    fn report_scanning(&self, _scanning: bool) {}
}

fn slot(partition: i32, reader: Arc<StubReader>) -> PartitionSlot {
    PartitionSlot::new(key(partition), reader)
}

#[test]
fn an_empty_set_has_nothing_to_read() {
    let mut set = ReadSet::seed(Vec::new());

    assert!(set.is_empty());
    assert_eq!(set.next_to_read(), None);
}

#[test]
fn a_set_with_nothing_ready_says_go_idle() {
    let mut set = ReadSet::seed(vec![
        slot(0, StubReader::new(false)),
        slot(1, StubReader::new(false)),
    ]);

    // `None` means idle, not spin. Returning a partition here would make the
    // session read, find nothing, and come straight back.
    assert_eq!(set.next_to_read(), None);
    assert!(!set.any_ready());
}

#[test]
fn an_unready_partition_does_not_consume_a_turn() {
    let ready = StubReader::new(true);
    let mut set = ReadSet::seed(vec![
        slot(0, StubReader::new(false)),
        slot(1, Arc::clone(&ready)),
        slot(2, StubReader::new(false)),
    ]);

    // Skipped rather than rotated past, so a set with one busy partition and
    // many idle ones costs comparisons rather than wasted rounds.
    assert_eq!(set.next_to_read(), Some(1));
    assert_eq!(set.next_to_read(), Some(1));
}

#[test]
fn ready_partitions_are_taken_in_rotation() {
    let mut set = ReadSet::seed(vec![
        slot(0, StubReader::new(true)),
        slot(1, StubReader::new(true)),
        slot(2, StubReader::new(true)),
    ]);

    assert_eq!(set.next_to_read(), Some(0));
    assert_eq!(set.next_to_read(), Some(1));
    assert_eq!(set.next_to_read(), Some(2));
    assert_eq!(set.next_to_read(), Some(0), "and wraps");
}

#[test]
fn a_throttled_partition_yields_until_the_round_reopens() {
    let mut set = ReadSet::seed(vec![
        slot(0, StubReader::new(true)),
        slot(1, StubReader::new(true)),
    ]);

    assert_eq!(set.next_to_read(), Some(0));
    set.mark_throttled(0);

    assert_eq!(set.next_to_read(), Some(1));
    set.mark_throttled(1);
    assert_eq!(set.next_to_read(), None, "every partition has had its turn");

    set.open_round();
    assert!(set.next_to_read().is_some());
}

#[test]
fn recording_a_batch_advances_both_the_cursor_and_the_frontier() {
    let reader = StubReader::new(true);
    let mut set = ReadSet::seed(vec![slot(0, Arc::clone(&reader))]);

    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(140))
            .delivered_through(Sequence::assigned(100))
            .counts(3, 40)
            .build(),
    );

    let slot = set.slot(0).expect("slot");
    assert_eq!(slot.offset(), Sequence::assigned(100));
    assert_eq!(slot.last_examined(), Sequence::assigned(140));
    assert_eq!(
        slot.drift(),
        37,
        "forty examined, three delivered: the thirty-seven the filter rejected \
         are what a progress frame is owed on, and no arithmetic on the two \
         frontiers produces that number"
    );
    // Nothing is published from here any more: the reader advanced itself as
    // part of `read`. What this asserts is that the *slot* still tracks both
    // numbers, because `Position` reports them.
    // That the reader tracks the frontier rather than the cursor is now the
    // cache's property - see `a_read_advances_what_reclamation_retains_against`.
    assert_eq!(
        reader.position(),
        Sequence::assigned(0),
        "the read set must not move the reader"
    );
}

#[test]
fn a_saturating_filter_still_lets_the_cache_reclaim() {
    let reader = StubReader::new(true);
    let mut set = ReadSet::seed(vec![slot(0, Arc::clone(&reader))]);

    // One match in a hundred thousand. The frontier is what the loader keeps
    // resident, so it must be the examined position and not the cursor, or a
    // subscription delivering almost nothing would hold a whole partition.
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(100_000))
            .delivered_through(Sequence::assigned(8))
            .counts(1, 100_000)
            .build(),
    );

    assert_eq!(
        set.slot(0).map(PartitionSlot::last_examined),
        Some(Sequence::assigned(100_000))
    );
    assert_eq!(
        set.slot(0).map(PartitionSlot::offset),
        Some(Sequence::assigned(8))
    );
}

#[test]
fn a_batch_that_delivered_nothing_still_moves_the_frontier() {
    let mut set = ReadSet::seed(vec![slot(0, StubReader::new(true))]);
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(50))
            .delivered_through(Sequence::assigned(50))
            .build(),
    );

    // A read over a proven-absent range delivers nothing and has still covered
    // ground. Without this a saturating filter would look permanently stuck.
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(900))
            .delivered_through(Sequence::assigned(50))
            .build(),
    );

    let slot = set.slot(0).expect("slot");
    assert_eq!(slot.offset(), Sequence::assigned(50));
    assert_eq!(slot.last_examined(), Sequence::assigned(900));
}

#[test]
fn neither_position_ever_goes_backwards() {
    let mut set = ReadSet::seed(vec![slot(0, StubReader::new(true))]);
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(500))
            .delivered_through(Sequence::assigned(400))
            .build(),
    );

    // A late or duplicated outcome must not rewind a consumer's cursor.
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(10))
            .delivered_through(Sequence::assigned(5))
            .build(),
    );

    let slot = set.slot(0).expect("slot");
    assert_eq!(slot.offset(), Sequence::assigned(400));
    assert_eq!(slot.last_examined(), Sequence::assigned(500));
}

#[test]
fn an_outcome_cannot_report_a_frontier_behind_its_cursor() {
    let outcome = BatchOutcome::builder(Sequence::assigned(10))
        .delivered_through(Sequence::assigned(900))
        .build();

    // Everything delivered was examined, so this is normalised rather than
    // trusted.
    assert_eq!(outcome.examined_through(), Sequence::assigned(900));
}

#[test]
fn only_partitions_past_the_threshold_are_reported_as_drifted() {
    let mut set = ReadSet::seed(vec![
        slot(0, StubReader::new(true)),
        slot(1, StubReader::new(true)),
    ]);
    // Partition 0 examined eleven hundred events it did not deliver; partition
    // 1 rejected fifty. The spans they cover are irrelevant - only what the
    // filter passed over counts.
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(1200))
            .delivered_through(Sequence::assigned(100))
            .counts(0, 1100)
            .build(),
    );
    set.record_batch(
        1,
        BatchOutcome::builder(Sequence::assigned(150))
            .delivered_through(Sequence::assigned(100))
            .counts(0, 50)
            .build(),
    );

    let drifted = set.list_drifted(1000);

    assert_eq!(drifted.len(), 1);
    assert_eq!(drifted.first().map(|p| p.partition), Some(0));
}

/// Reporting a position settles the debt: the next frame is owed on what has
/// been examined since, not on a total that would trip on every check once it
/// had been passed.
#[test]
fn reporting_a_drifted_partition_starts_its_count_again() {
    let mut set = ReadSet::seed(vec![slot(0, StubReader::new(true))]);
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(1200))
            .delivered_through(Sequence::assigned(100))
            .counts(0, 1100)
            .build(),
    );

    assert_eq!(set.list_drifted(1000).len(), 1);
    assert!(
        set.list_drifted(1000).is_empty(),
        "nothing new has been examined, so nothing is owed"
    );

    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(2400))
            .delivered_through(Sequence::assigned(100))
            .counts(0, 1000)
            .build(),
    );
    assert_eq!(
        set.list_drifted(1000).len(),
        1,
        "a further thousand rejected events owes a further frame"
    );
}

#[test]
fn retaining_keeps_the_cursors_of_surviving_partitions() {
    let mut set = ReadSet::seed(vec![
        slot(0, StubReader::new(true)),
        slot(1, StubReader::new(true)),
        slot(2, StubReader::new(true)),
    ]);
    set.record_batch(
        1,
        BatchOutcome::builder(Sequence::assigned(700))
            .delivered_through(Sequence::assigned(700))
            .build(),
    );

    set.retain(&[key(1)]);

    // A loss must not disturb what a session keeps - which is what makes
    // continuing after a non-terminal topology frame safe.
    assert_eq!(set.len(), 1);
    let slot = set.slot(0).expect("the surviving slot");
    assert_eq!(slot.key().partition, 1);
    assert_eq!(slot.offset(), Sequence::assigned(700));
}

#[test]
fn positions_are_reported_for_every_partition_held() {
    let mut set = ReadSet::seed(vec![
        slot(0, StubReader::new(true)),
        slot(1, StubReader::new(true)),
    ]);
    set.record_batch(
        0,
        BatchOutcome::builder(Sequence::assigned(30))
            .delivered_through(Sequence::assigned(20))
            .build(),
    );

    let positions = set.list_positions();

    assert_eq!(positions.len(), 2);
    assert_eq!(
        positions.first().map(|p| p.offset),
        Some(Sequence::assigned(20))
    );
    assert_eq!(
        positions.first().map(|p| p.last_examined),
        Some(Sequence::assigned(30))
    );
    assert_eq!(
        positions.get(1).map(|p| p.offset),
        Some(Sequence::assigned(0))
    );
}

#[test]
fn a_seeded_cursor_starts_the_frontier_with_it() {
    let set = ReadSet::seed(vec![
        slot(0, StubReader::new(true)).starting_at(Sequence::assigned(4096)),
    ]);

    // The starting position comes from the cursor store, and nothing below it
    // has been examined by *this* session either.
    let slot = set.slot(0).expect("slot");
    assert_eq!(slot.offset(), Sequence::assigned(4096));
    assert_eq!(slot.last_examined(), Sequence::assigned(4096));
    assert_eq!(slot.drift(), 0);
}
