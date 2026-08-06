//! Pure: no runtime, no clock, no storage. Every segment here is hand-built,
//! and several are deliberately sparse.

use chrono::Utc;
use serde_json::json;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{Event, Sequence};

use super::segment::Segment;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, ReadLimit};

fn gts(suffix: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(&format!("gts.cf.core.events.{suffix}")).expect("static gts id is valid")
}

fn event(sequence: i64) -> Event {
    let sequence = Sequence::assigned(sequence);
    Event {
        id: Uuid::nil(),
        r#type: crate::test_support::event_type_id(
            "gts.cf.core.events.event.v1~x.eb.orders.created.v1~",
        ),
        topic: gts("topic.v1~x.eb.orders.acme.v1"),
        tenant_id: Uuid::nil(),
        source: "test".to_owned(),
        subject: "test".to_owned(),
        subject_type: "test".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: json!({ "n": sequence }),
        meta: None,
        partition: Some(0),
        sequence: Some(sequence),
        sequence_time: None,
    }
}

/// Generous enough that only the segment's own span can bound a read.
fn unbounded() -> ReadLimit {
    ReadLimit::new(MaxEvents(usize::MAX), MaxBytes(usize::MAX))
}

fn segment(from: i64, through: i64, present: &[i64]) -> Segment {
    let from = Sequence::assigned(from);
    let through = Sequence::assigned(through);
    Segment::builder()
        .from(from)
        .through(through)
        .events(present.iter().copied().map(event).collect())
        .build()
}

fn sequences(slice: &crate::domain::streaming::read::EventSlice) -> Vec<i64> {
    slice
        .iter()
        .filter_map(|event| event.sequence)
        .map(Sequence::as_i64)
        .collect()
}

#[test]
fn a_dense_segment_reads_forward_from_an_offset() {
    let resident = segment(100, 104, &[100, 101, 102, 103, 104]);

    assert_eq!(
        sequences(&resident.read_after(Sequence::assigned(101), unbounded())),
        vec![102, 103, 104]
    );
}

#[test]
fn reading_from_before_the_span_returns_everything() {
    let resident = segment(100, 102, &[100, 101, 102]);

    assert_eq!(
        sequences(&resident.read_after(Sequence::assigned(0), unbounded())),
        vec![100, 101, 102]
    );
}

#[test]
fn reading_from_the_end_of_the_span_returns_nothing() {
    let resident = segment(100, 102, &[100, 101, 102]);

    assert!(
        resident
            .read_after(Sequence::assigned(102), unbounded())
            .is_empty()
    );
}

#[test]
fn a_read_steps_over_a_hole() {
    // 150..=159 accounted for and absent; the reader at 149 gets 160 next.
    let present: Vec<i64> = (100..=149).chain(160..=200).collect();
    let resident = segment(100, 200, &present);

    let read = resident.read_after(Sequence::assigned(149), unbounded());

    assert_eq!(
        read.iter().next().and_then(|event| event.sequence),
        Some(Sequence::assigned(160)),
        "the next event after a hole is the next present one"
    );
}

#[test]
fn a_read_positioned_inside_a_hole_resumes_after_it() {
    let resident = segment(100, 200, &[100, 200]);

    assert_eq!(
        sequences(&resident.read_after(Sequence::assigned(150), unbounded())),
        vec![200]
    );
}

#[test]
fn events_after_counts_and_does_not_subtract() {
    // The span covers 101 sequences but holds 3 events. Subtracting the span's
    // ends would report 100 and over-report what a reader can consume.
    let resident = segment(100, 200, &[100, 150, 200]);

    assert_eq!(resident.events_after(Sequence::assigned(99)), 3);
    assert_eq!(resident.events_after(Sequence::assigned(100)), 2);
    assert_eq!(resident.events_after(Sequence::assigned(150)), 1);
    assert_eq!(resident.events_after(Sequence::assigned(200)), 0);
}

#[test]
fn events_after_a_position_inside_a_hole_counts_what_remains() {
    let resident = segment(100, 200, &[100, 200]);

    assert_eq!(resident.events_after(Sequence::assigned(150)), 1);
}

#[test]
fn the_event_bound_limits_a_read() {
    let resident = segment(100, 109, &(100..=109).collect::<Vec<_>>());

    let read = resident.read_after(
        Sequence::assigned(99),
        ReadLimit::new(MaxEvents(3), MaxBytes(usize::MAX)),
    );

    assert_eq!(sequences(&read), vec![100, 101, 102]);
}

#[test]
fn the_byte_bound_limits_a_read_but_always_yields_one_event() {
    let resident = segment(100, 109, &(100..=109).collect::<Vec<_>>());

    // A bound below a single event's size must still make progress, or a
    // reader stalls forever on an event it can never fit.
    let read = resident.read_after(
        Sequence::assigned(99),
        ReadLimit::new(MaxEvents(100), MaxBytes(1)),
    );

    assert_eq!(read.len(), 1);
}

#[test]
fn a_segment_accounts_for_its_whole_span_including_holes() {
    let resident = segment(100, 200, &[100, 200]);

    assert!(resident.accounts_for(Sequence::assigned(100)));
    assert!(
        resident.accounts_for(Sequence::assigned(150)),
        "a hole inside the span is accounted for - known absent, not unknown"
    );
    assert!(resident.accounts_for(Sequence::assigned(200)));
    assert!(!resident.accounts_for(Sequence::assigned(99)));
    assert!(!resident.accounts_for(Sequence::assigned(201)));
}

#[test]
fn adjacency_is_exact_and_indifferent_to_holes() {
    let left = segment(100, 200, &[100, 200]);
    let right = segment(201, 300, &[250]);
    let gapped = segment(202, 300, &[250]);

    assert!(left.is_adjacent_to(&right));
    assert!(
        !left.is_adjacent_to(&gapped),
        "202 leaves 201 unaccounted for, so these must not merge"
    );
}

#[test]
fn a_builder_orders_and_deduplicates_what_it_is_given() {
    // A caller handing over unsorted or repeated events must not be able to
    // produce a segment whose lookups misbehave.
    let resident = Segment::builder()
        .from(Sequence::assigned(100))
        .through(Sequence::assigned(103))
        .events(vec![event(102), event(100), event(102), event(101)])
        .build();

    assert_eq!(
        sequences(&resident.read_after(Sequence::assigned(99), unbounded())),
        vec![100, 101, 102]
    );
    assert_eq!(resident.events_after(Sequence::assigned(99)), 3);
}

#[test]
fn an_empty_segment_is_well_behaved() {
    let resident = segment(100, 200, &[]);

    assert_eq!(resident.events_after(Sequence::assigned(99)), 0);
    assert!(
        resident
            .read_after(Sequence::assigned(99), unbounded())
            .is_empty()
    );
    assert!(
        resident.accounts_for(Sequence::assigned(150)),
        "a segment may account for a span in which everything was deleted"
    );
}

#[test]
fn a_through_below_from_is_normalised_rather_than_inverting_the_span() {
    let resident = Segment::builder()
        .from(Sequence::assigned(200))
        .through(Sequence::assigned(100))
        .events(vec![])
        .build();

    assert_eq!(resident.from(), Sequence::assigned(200));
    assert_eq!(resident.through(), Sequence::assigned(200));
}

/// One event carrying `payload`, otherwise identical to [`event`].
fn event_with_payload(sequence: i64, payload: &str) -> Event {
    Event {
        data: json!({ "body": payload }),
        ..event(sequence)
    }
}

#[test]
fn footprint_counts_the_whole_event_not_just_the_payload() {
    let payload = json!({ "n": 100 });
    let payload_bytes = payload.to_string().len();
    let resident = segment(100, 100, &[100]);

    // The envelope - two GTS ids, three strings, the struct itself - dwarfs a
    // small payload. Counting `data` alone let the cache hold far more than it
    // believed and made the residency limit under-enforce.
    assert!(
        resident.bytes() > payload_bytes * 4,
        "footprint {} must exceed the {payload_bytes}-byte payload by the \
         envelope, not merely equal it",
        resident.bytes()
    );
}

#[test]
fn footprint_still_grows_with_the_payload() {
    let small = Segment::builder()
        .from(Sequence::assigned(100))
        .through(Sequence::assigned(100))
        .events(vec![event_with_payload(100, "x")])
        .build();
    let large = Segment::builder()
        .from(Sequence::assigned(100))
        .through(Sequence::assigned(100))
        .events(vec![event_with_payload(100, &"x".repeat(4096))])
        .build();

    assert!(
        large.bytes() > small.bytes() + 4000,
        "the payload must be counted too: small {}, large {}",
        small.bytes(),
        large.bytes()
    );
}

#[test]
fn a_segments_footprint_is_the_sum_of_its_runs() {
    let resident = segment(100, 104, &[100, 101, 102, 103, 104]);

    let head = resident.read_after(
        Sequence::assigned(99),
        ReadLimit::new(MaxEvents(2), MaxBytes(usize::MAX)),
    );
    let tail = resident.read_after(Sequence::assigned(101), unbounded());

    assert_eq!(head.len(), 2);
    assert_eq!(tail.len(), 3);
    assert_eq!(
        head.bytes() + tail.bytes(),
        resident.bytes(),
        "the cumulative index must partition the segment's bytes exactly"
    );
}

#[test]
fn an_empty_segment_has_no_footprint() {
    let resident = segment(100, 200, &[]);

    assert_eq!(resident.bytes(), 0);
    assert_eq!(resident.event_count(), 0);
}

#[test]
fn the_index_survives_sorting_deduplication_and_respanning() {
    let scrambled = Segment::builder()
        .from(Sequence::assigned(100))
        .through(Sequence::assigned(104))
        .events(vec![event(103), event(100), event(103), event(101)])
        .build();

    assert!(scrambled.index_is_consistent());
    assert_eq!(scrambled.event_count(), 3, "the duplicate 103 is dropped");

    let widened = scrambled.with_span(Sequence::assigned(50), Sequence::assigned(200));
    assert!(
        widened.index_is_consistent(),
        "re-spanning reuses the storage, so the index must still describe it"
    );
}

#[test]
fn a_run_that_reaches_the_segments_end_accounts_for_the_deleted_tail() {
    // Everything after 100 in the span was deleted, and the fetch proved it.
    let resident = segment(100, 200, &[100]);

    let read = resident.read_after(Sequence::assigned(99), unbounded());

    assert_eq!(sequences(&read), vec![100]);
    assert_eq!(
        read.frontier(),
        Sequence::assigned(200),
        "the run reached the end, so the proven-absent tail is accounted for"
    );
}

#[test]
fn a_run_stopped_by_a_limit_accounts_only_for_what_it_delivered() {
    let resident = segment(100, 109, &(100..=109).collect::<Vec<_>>());

    let read = resident.read_after(
        Sequence::assigned(99),
        ReadLimit::new(MaxEvents(3), MaxBytes(usize::MAX)),
    );

    assert_eq!(sequences(&read), vec![100, 101, 102]);
    assert_eq!(
        read.frontier(),
        Sequence::assigned(102),
        "advancing to the span's `through` here would skip 103..=109 silently"
    );
    assert_eq!(
        read.last_sequence(),
        Some(Sequence::assigned(102)),
        "the two agree when a limit truncated the run"
    );
}

#[test]
fn the_frontier_and_the_last_sequence_disagree_on_a_deleted_tail() {
    let resident = segment(100, 200, &[100]);

    let read = resident.read_after(Sequence::assigned(99), unbounded());

    // The distinction the two accessors exist to draw: one is how far the
    // reader may advance, the other is what it actually received.
    assert_eq!(read.frontier(), Sequence::assigned(200));
    assert_eq!(read.last_sequence(), Some(Sequence::assigned(100)));
}
