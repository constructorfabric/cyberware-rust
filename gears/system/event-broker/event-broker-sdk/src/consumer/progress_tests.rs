use crate::sequence::Sequence;
use chrono::Utc;
use toolkit_gts::gts_id;
use uuid::Uuid;

use super::dispatcher::PartitionCursor;
use super::progress::{processed_count_from_delivered_offset, processed_count_from_outcome};
use super::{BatchHandlerOutcome, RawEvent};

fn event_with_offset(offset: i64) -> RawEvent {
    RawEvent {
        id: Uuid::new_v4(),
        type_id: gts_id!("cf.core.events.event.v1~example.progress.test.x.v1~").to_owned(),
        topic: gts_id!("cf.core.events.topic.v1~example.progress.test.x.v1").to_owned(),
        tenant_id: Uuid::nil(),
        subject: format!("event-{offset}"),
        subject_type: "test".to_owned(),
        partition: 0,
        sequence: Sequence::assigned(offset),
        offset: Sequence::assigned(offset),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "offset": offset }),
    }
}

fn delivered_sparse_batch() -> Vec<RawEvent> {
    [10, 17, 19].into_iter().map(event_with_offset).collect()
}

#[test]
fn advance_through_counts_delivered_prefix_for_sparse_offsets() {
    let events = delivered_sparse_batch();

    let processed = processed_count_from_outcome(
        &BatchHandlerOutcome::AdvanceThrough {
            offset: Sequence::assigned(17),
        },
        &events,
    )
    .expect("handled offset is in delivered batch");

    assert_eq!(processed, Some(2));
}

#[test]
fn full_batch_success_counts_all_delivered_events() {
    let events = delivered_sparse_batch();

    let processed = processed_count_from_outcome(&BatchHandlerOutcome::Success, &events)
        .expect("success always maps to full batch");

    assert_eq!(processed, Some(3));
}

#[test]
fn invalid_advance_through_offset_is_rejected_without_progress_count() {
    let events = delivered_sparse_batch();

    let err = processed_count_from_outcome(
        &BatchHandlerOutcome::AdvanceThrough {
            offset: Sequence::assigned(11),
        },
        &events,
    )
    .expect_err("offset not delivered in this batch must be rejected");

    assert!(err.to_string().contains("not present in delivered offsets"));
}

#[test]
fn retry_outcome_counts_no_delivered_progress() {
    let events = delivered_sparse_batch();

    let processed = processed_count_from_outcome(
        &BatchHandlerOutcome::Retry {
            reason: "try later".to_owned(),
        },
        &events,
    )
    .expect("retry is valid");

    assert_eq!(processed, None);
}

#[test]
fn cursor_advances_to_delivered_offset_not_first_offset_plus_count() {
    let events = delivered_sparse_batch();
    let mut cursor = PartitionCursor::default();

    let processed = processed_count_from_outcome(
        &BatchHandlerOutcome::AdvanceThrough {
            offset: Sequence::assigned(17),
        },
        &events,
    )
    .expect("handled offset is in delivered batch")
    .expect("partial outcome has progress");
    let frontier = cursor.advance_through_delivered_prefix(&events[..processed]);

    assert_eq!(frontier, Sequence::assigned(17));
    assert_eq!(cursor.latest_offset(), Sequence::assigned(17));
}

#[test]
fn tx_committed_offset_counts_delivered_prefix_for_sparse_offsets() {
    let events = delivered_sparse_batch();

    let processed = processed_count_from_delivered_offset(Sequence::assigned(17), &events)
        .expect("tx committed offset is in delivered batch");

    assert_eq!(processed, 2);
}

#[test]
fn tx_committed_offset_rejects_offsets_outside_delivered_batch() {
    let events = delivered_sparse_batch();

    let err = processed_count_from_delivered_offset(Sequence::assigned(20), &events)
        .expect_err("tx committed offset must be delivered in the current batch");

    assert!(err.to_string().contains("not present in delivered offsets"));
}
