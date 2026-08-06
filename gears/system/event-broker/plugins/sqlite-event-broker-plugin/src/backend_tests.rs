//! Retention: what a pass removes, what it leaves, and what it counts.
//!
//! Every pass is driven directly rather than waited for. The backend owns no
//! timer, so a test that wants three passes calls `maintain` three times and
//! knows exactly three happened.

use chrono::{DateTime, TimeDelta, Utc};
use event_broker_sdk::Sequence;
use event_broker_sdk::api::Position;
use event_broker_sdk::error::StorageBackendError;
use event_broker_sdk::models::Event;
use event_broker_sdk::{EventBrokerBackend, RetentionReport, RetentionRequest};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::SqliteEventBackend;
use crate::test_support::test_backend;

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
const PARTITION: u32 = 0;

async fn backend() -> SqliteEventBackend {
    test_backend().await
}

/// An event whose payload is `payload_bytes` bytes of filler, so a test can
/// aim at a byte bound instead of guessing at one.
fn event(payload_bytes: usize) -> Event {
    Event {
        id: Uuid::now_v7(),
        type_id: "gts.cf.core.events.event.v1~x.eb.t1.foo.v1".to_owned(),
        tenant_id: Uuid::now_v7(),
        source: "retention-tests".to_owned(),
        subject: "s".to_owned(),
        subject_type: "gts.x.eb.t1.subject.v1~".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: Some(serde_json::json!({ "filler": "x".repeat(payload_bytes) })),
        partition: Some(PARTITION),
        sequence: None,
        sequence_time: None,
        meta: None,
    }
}

async fn persist(backend: &SqliteEventBackend, count: usize, payload_bytes: usize) {
    let events: Vec<Event> = (0..count).map(|_| event(payload_bytes)).collect();
    backend
        .persist(&SecurityContext::anonymous(), TOPIC, PARTITION, &events)
        .await
        .expect("persist succeeds");
}

/// Every event still stored, ascending. Read through the backend's own read
/// path, so what a test asserts is what a reader would be served.
async fn stored(backend: &SqliteEventBackend) -> Vec<Event> {
    backend
        .read(
            &SecurityContext::anonymous(),
            TOPIC,
            PARTITION,
            Sequence::NONE,
            10_000,
        )
        .await
        .expect("read succeeds")
}

fn sequences(events: &[Event]) -> Vec<i64> {
    events
        .iter()
        .map(|e| {
            e.sequence
                .expect("a stored event carries its sequence")
                .as_i64()
        })
        .collect()
}

/// A pass that can remove nothing by either bound, used to observe the
/// partition's maintained figures without a bespoke accessor.
async fn observe(backend: &SqliteEventBackend) -> RetentionReport {
    let long_ago = Utc::now() - TimeDelta::hours(1);
    backend
        .maintain(
            &SecurityContext::anonymous(),
            &RetentionRequest::for_partition(TOPIC, PARTITION, long_ago).build(),
        )
        .await
        .expect("a pass with nothing to do still reports")
}

async fn prune(backend: &SqliteEventBackend, request: &RetentionRequest) -> RetentionReport {
    backend
        .maintain(&SecurityContext::anonymous(), request)
        .await
        .expect("retention pass succeeds")
}

/// The instant strictly between two batches: the `sequence_time` the first
/// event of the second batch was stamped with.
///
/// Deterministic where a sleep would not be - the cutoff is derived from what
/// was actually stored rather than from a wall-clock guess. The precondition it
/// rests on is asserted rather than assumed.
fn boundary_after(stored: &[Event], first_of_second_batch: usize) -> DateTime<Utc> {
    let last_of_first = stored[first_of_second_batch - 1]
        .sequence_time
        .expect("a stored event carries its sequence time");
    let first_of_second = stored[first_of_second_batch]
        .sequence_time
        .expect("a stored event carries its sequence time");
    assert!(
        last_of_first < first_of_second,
        "the two batches must be distinguishable in time for this test to mean \
         anything: {last_of_first} was not before {first_of_second}"
    );
    first_of_second
}

#[tokio::test]
async fn a_pass_over_a_partition_that_was_never_written_reports_an_empty_pass() {
    let backend = backend().await;

    assert_eq!(
        prune(
            &backend,
            &RetentionRequest::for_partition(TOPIC, PARTITION, Utc::now()).build(),
        )
        .await,
        RetentionReport::default()
    );
}

#[tokio::test]
async fn a_pass_reports_what_it_removed_and_where_the_partition_now_stands() {
    let backend = backend().await;
    persist(&backend, 3, 10).await;
    persist(&backend, 2, 10).await;
    let before = stored(&backend).await;
    let cutoff = boundary_after(&before, 3);

    let report = prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, cutoff).build(),
    )
    .await;

    assert_eq!(report.removed_events, 3);
    assert_eq!(report.remaining_events, 2);
    assert_eq!(
        report.oldest_surviving_sequence,
        Some(Sequence::assigned(4))
    );
    assert!(report.removed_bytes > 0, "three events cost some bytes");
    assert_eq!(
        report.removed_bytes + report.remaining_bytes,
        observe_total(&before),
        "what a pass removed plus what it left is what was there"
    );
}

/// The stored size of a whole read-back run, summed the way the backend sums it
/// when it writes each row.
fn observe_total(events: &[Event]) -> u64 {
    events
        .iter()
        .map(|e| u64::try_from(crate::sizing::stored_bytes(e)).unwrap_or(0))
        .sum()
}

#[tokio::test]
async fn the_count_and_byte_total_track_what_was_inserted() {
    let backend = backend().await;
    persist(&backend, 4, 100).await;

    let report = observe(&backend).await;
    assert_eq!(report.removed_events, 0);
    assert_eq!(report.remaining_events, 4);
    assert_eq!(
        report.remaining_bytes,
        observe_total(&stored(&backend).await)
    );
    assert_eq!(
        report.oldest_surviving_sequence,
        Some(Sequence::assigned(1))
    );
}

#[tokio::test]
async fn after_a_prefix_removal_the_count_is_the_rows_remaining_not_a_span() {
    let backend = backend().await;
    persist(&backend, 5, 10).await;
    persist(&backend, 5, 10).await;
    let before = stored(&backend).await;
    assert_eq!(sequences(&before), (1..=10).collect::<Vec<_>>());
    let cutoff = boundary_after(&before, 5);

    prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, cutoff).build(),
    )
    .await;

    let after = stored(&backend).await;
    let surviving = sequences(&after);
    assert_eq!(surviving, vec![6, 7, 8, 9, 10]);

    let report = observe(&backend).await;
    assert_eq!(
        report.remaining_events,
        after.len() as u64,
        "the count is the rows actually still stored"
    );
    let highest = *surviving.last().expect("five events survived");
    assert_ne!(
        report.remaining_events,
        highest.cast_unsigned(),
        "the highest surviving sequence is 10 and the count is 5; a count taken \
         from the sequence counter would report 10"
    );
    assert_eq!(report.remaining_bytes, observe_total(&after));
}

#[tokio::test]
async fn the_duration_bound_removes_the_aged_prefix_and_keeps_a_contiguous_suffix() {
    let backend = backend().await;
    persist(&backend, 4, 10).await;
    persist(&backend, 3, 10).await;
    let before = stored(&backend).await;
    let cutoff = boundary_after(&before, 4);

    let report = prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, cutoff).build(),
    )
    .await;

    assert_eq!(report.removed_events, 4);
    let after = stored(&backend).await;
    assert_eq!(
        sequences(&after),
        vec![5, 6, 7],
        "exactly the aged events are gone, and what remains is a contiguous suffix"
    );
    assert_eq!(
        report.oldest_surviving_sequence,
        Some(Sequence::assigned(5))
    );
}

#[tokio::test]
async fn the_size_bound_removes_until_the_partition_is_within_it_and_then_stops() {
    let backend = backend().await;
    persist(&backend, 6, 200).await;
    let before = stored(&backend).await;
    let per_event = u64::try_from(crate::sizing::stored_bytes(&before[0])).expect("positive");
    // Room for three events and not a byte more, so the pass must remove three
    // and stop rather than emptying the partition.
    let cap = per_event * 3;

    let report = prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, distant_past())
            .max_stored_bytes(cap)
            .build(),
    )
    .await;

    assert_eq!(report.removed_events, 3);
    assert_eq!(report.remaining_events, 3);
    assert!(
        report.remaining_bytes <= cap,
        "the partition must end within its bound: {} > {cap}",
        report.remaining_bytes
    );
    assert_eq!(sequences(&stored(&backend).await), vec![4, 5, 6]);
}

/// A cutoff no stored event can be older than, so only the byte bound can fire.
fn distant_past() -> DateTime<Utc> {
    Utc::now() - TimeDelta::days(3650)
}

#[tokio::test]
async fn the_size_bound_fires_while_every_event_is_younger_than_the_duration() {
    let backend = backend().await;
    persist(&backend, 5, 200).await;
    let before = stored(&backend).await;
    let per_event = u64::try_from(crate::sizing::stored_bytes(&before[0])).expect("positive");

    let report = prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, distant_past())
            .max_stored_bytes(per_event * 2)
            .build(),
    )
    .await;

    assert_eq!(
        report.removed_events, 3,
        "nothing is old enough to remove, so only the byte bound can have fired"
    );
    assert_eq!(sequences(&stored(&backend).await), vec![4, 5]);
}

#[tokio::test]
async fn the_duration_bound_fires_while_the_partition_is_well_under_its_size() {
    let backend = backend().await;
    persist(&backend, 3, 10).await;
    persist(&backend, 3, 10).await;
    let before = stored(&backend).await;
    let cutoff = boundary_after(&before, 3);

    let report = prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, cutoff)
            .max_stored_bytes(u64::from(u32::MAX))
            .build(),
    )
    .await;

    assert_eq!(
        report.removed_events, 3,
        "the partition is nowhere near its byte bound, so only age can have fired"
    );
    assert_eq!(sequences(&stored(&backend).await), vec![4, 5, 6]);
}

#[tokio::test]
async fn a_partition_with_no_size_bound_grows_past_any_byte_figure() {
    let backend = backend().await;
    persist(&backend, 20, 1000).await;

    let report = prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, distant_past()).build(),
    )
    .await;

    assert_eq!(report.removed_events, 0);
    assert_eq!(report.remaining_events, 20);
    assert!(
        report.remaining_bytes > 20_000,
        "twenty kilobyte-ish events, kept: {}",
        report.remaining_bytes
    );
    assert_eq!(sequences(&stored(&backend).await).len(), 20);
}

#[tokio::test]
async fn several_driven_passes_converge_and_then_do_nothing() {
    let backend = backend().await;
    persist(&backend, 6, 200).await;
    let per_event =
        u64::try_from(crate::sizing::stored_bytes(&stored(&backend).await[0])).expect("positive");
    let request = RetentionRequest::for_partition(TOPIC, PARTITION, distant_past())
        .max_stored_bytes(per_event * 2)
        .build();

    let first = prune(&backend, &request).await;
    assert_eq!(first.removed_events, 4);
    assert_eq!(first.remaining_events, 2);

    // Driven, not scheduled: nothing ran between these two calls, so a second
    // pass finding nothing to do is evidence the first converged rather than
    // evidence of timing.
    for pass in 2..=3 {
        let report = prune(&backend, &request).await;
        assert_eq!(
            report.removed_events, 0,
            "pass {pass} had nothing left to remove"
        );
        assert_eq!(report.remaining_events, 2);
    }
    assert_eq!(sequences(&stored(&backend).await), vec![5, 6]);
}

#[tokio::test]
async fn one_partitions_bounds_do_not_reach_another() {
    let backend = backend().await;
    let other_partition: u32 = 1;
    persist(&backend, 3, 10).await;
    let events: Vec<Event> = (0..3).map(|_| event(10)).collect();
    backend
        .persist(
            &SecurityContext::anonymous(),
            TOPIC,
            other_partition,
            &events,
        )
        .await
        .expect("persist succeeds");

    prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, Utc::now()).build(),
    )
    .await;

    assert!(
        stored(&backend).await.is_empty(),
        "partition 0 was emptied by its own pass"
    );
    let untouched = backend
        .read(
            &SecurityContext::anonymous(),
            TOPIC,
            other_partition,
            Sequence::NONE,
            10_000,
        )
        .await
        .expect("read succeeds");
    assert_eq!(
        sequences(&untouched),
        vec![1, 2, 3],
        "bounds apply per partition, independently of every other partition"
    );
}

// -- resolve: which position a cursor may take -------------------------------
//
// The backend answers all four forms because it is the only thing that knows
// what it holds. A returned position is one a cursor *may* hold - never a
// claim that an event occupies it.

async fn resolve(backend: &SqliteEventBackend, position: Position) -> Sequence {
    backend
        .resolve(&SecurityContext::anonymous(), TOPIC, PARTITION, position)
        .await
        .expect("the position resolves")
}

async fn resolve_err(backend: &SqliteEventBackend, position: Position) -> StorageBackendError {
    backend
        .resolve(&SecurityContext::anonymous(), TOPIC, PARTITION, position)
        .await
        .expect_err("the position is inadmissible")
}

#[tokio::test]
async fn a_never_written_partition_admits_only_no_position() {
    let backend = backend().await;

    assert_eq!(resolve(&backend, Position::Latest).await, Sequence::NONE);
    assert_eq!(resolve(&backend, Position::Earliest).await, Sequence::NONE);
    assert_eq!(
        resolve(&backend, Position::Exact(Sequence::NONE)).await,
        Sequence::NONE
    );
    // The scenario's case: -1 is below the floor on a fresh partition.
    assert!(matches!(
        resolve_err(&backend, Position::Exact(Sequence::assigned(-1))).await,
        StorageBackendError::OffsetOutOfRange { .. }
    ));
    assert!(matches!(
        resolve_err(&backend, Position::Exact(Sequence::assigned(1))).await,
        StorageBackendError::OffsetOutOfRange { .. }
    ));
}

#[tokio::test]
async fn a_populated_partition_admits_from_below_the_floor_to_the_ceiling() {
    let backend = backend().await;
    persist(&backend, 3, 16).await;

    // Sequences start at 1, so three events give a ceiling of 3 and a floor
    // cursor of 0 - one below the oldest still stored.
    assert_eq!(
        resolve(&backend, Position::Latest).await,
        Sequence::assigned(3)
    );
    assert_eq!(resolve(&backend, Position::Earliest).await, Sequence::NONE);
    for admissible in [0, 1, 2, 3] {
        assert_eq!(
            resolve(&backend, Position::Exact(Sequence::assigned(admissible))).await,
            Sequence::assigned(admissible)
        );
    }
    assert!(matches!(
        resolve_err(&backend, Position::Exact(Sequence::assigned(4))).await,
        StorageBackendError::OffsetOutOfRange { .. }
    ));
}

#[tokio::test]
async fn an_erased_prefix_lifts_the_floor_but_not_the_ceiling() {
    let backend = backend().await;
    persist(&backend, 3, 16).await;
    let all = stored(&backend).await;
    // Remove the first two by age.
    prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, boundary_after(&all, 2)).build(),
    )
    .await;

    assert_eq!(sequences(&stored(&backend).await), vec![3]);
    assert_eq!(
        resolve(&backend, Position::Earliest).await,
        Sequence::assigned(2),
        "one below the oldest still stored"
    );
    assert_eq!(
        resolve(&backend, Position::Latest).await,
        Sequence::assigned(3),
        "the ceiling is unmoved by the erasure"
    );
    assert!(matches!(
        resolve_err(&backend, Position::Exact(Sequence::assigned(1))).await,
        StorageBackendError::OffsetOutOfRange { .. }
    ));
}

#[tokio::test]
async fn an_emptied_partition_admits_only_its_ceiling() {
    let backend = backend().await;
    persist(&backend, 3, 16).await;
    prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, Utc::now()).build(),
    )
    .await;

    assert!(stored(&backend).await.is_empty());
    // Nothing remains for delivery to resume from, so "I have processed
    // everything" is the only position that can be honoured.
    assert_eq!(
        resolve(&backend, Position::Earliest).await,
        Sequence::assigned(3)
    );
    assert_eq!(
        resolve(&backend, Position::Latest).await,
        Sequence::assigned(3)
    );
    assert!(matches!(
        resolve_err(&backend, Position::Exact(Sequence::assigned(2))).await,
        StorageBackendError::OffsetOutOfRange { .. }
    ));
}

#[tokio::test]
async fn the_ceiling_never_moves_backwards() {
    let backend = backend().await;
    persist(&backend, 3, 16).await;
    let before = resolve(&backend, Position::Latest).await;

    // Erase everything, newest included.
    prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, Utc::now()).build(),
    )
    .await;

    assert_eq!(
        resolve(&backend, Position::Latest).await,
        before,
        "a cursor a consumer already holds must not stop being admissible"
    );
}

#[tokio::test]
async fn a_timestamp_resolves_to_the_position_before_the_first_event_at_or_after_it() {
    let backend = backend().await;
    persist(&backend, 3, 16).await;
    let all = stored(&backend).await;
    let second_occurred = all[1].occurred_at;

    assert_eq!(
        resolve(&backend, Position::At(second_occurred)).await,
        Sequence::assigned(1),
        "delivery begins at the second event, so the cursor is the first"
    );
}

#[tokio::test]
async fn a_timestamp_beyond_every_event_resolves_to_the_ceiling() {
    let backend = backend().await;
    persist(&backend, 3, 16).await;
    let far_future = Utc::now() + TimeDelta::days(3650);

    assert_eq!(
        resolve(&backend, Position::At(far_future)).await,
        resolve(&backend, Position::Latest).await,
        "nothing that recent means only what arrives next"
    );
}

#[tokio::test]
async fn a_timestamp_before_the_retention_floor_resolves_to_the_floor() {
    let backend = backend().await;
    persist(&backend, 3, 16).await;
    let all = stored(&backend).await;
    prune(
        &backend,
        &RetentionRequest::for_partition(TOPIC, PARTITION, boundary_after(&all, 2)).build(),
    )
    .await;
    let long_ago = Utc::now() - TimeDelta::days(3650);

    assert_eq!(
        resolve(&backend, Position::At(long_ago)).await,
        resolve(&backend, Position::Earliest).await,
        "a timestamp older than anything retained begins at the oldest retained"
    );
}
