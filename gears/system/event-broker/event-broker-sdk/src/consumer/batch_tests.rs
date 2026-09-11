use crate::sequence::Sequence;
use chrono::Utc;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use toolkit_gts::gts_id;
use uuid::Uuid;

use super::progress::processed_count_from_outcome;
use super::{
    BatchHandlerOutcome, ConsumerHandler, EventBatch, HandlerOutcome, RawEvent, SingleEventHandler,
    SingleEventHandlerAdapter,
};
use crate::error::ConsumerError;

fn raw_event(offset: i64) -> RawEvent {
    RawEvent {
        id: Uuid::new_v4(),
        type_id: gts_id!("cf.core.events.event.v1~example.orders.order_created.x.v1~").to_owned(),
        topic: gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1").to_owned(),
        tenant_id: Uuid::nil(),
        subject: format!("order-{offset}"),
        subject_type: "order".to_owned(),
        partition: 7,
        sequence: Sequence::assigned(offset),
        offset: Sequence::assigned(offset),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "offset": offset }),
    }
}

#[test]
fn event_batch_reports_empty_state() {
    let events = Vec::new();
    let batch = EventBatch::new(&events);

    assert!(batch.is_empty());
    assert_eq!(batch.len(), 0);
    assert!(batch.next_event().is_none());
    assert!(batch.next_chunk(10).is_empty());
    assert!(batch.iter().next().is_none());
}

#[test]
fn event_batch_reads_without_mutating_progress() {
    let events = vec![raw_event(10), raw_event(11), raw_event(12)];
    let batch = EventBatch::new(&events);

    assert_eq!(batch.len(), 3);
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(10))
    );
    assert_eq!(
        batch
            .next_chunk(2)
            .iter()
            .map(|event| event.offset.as_i64())
            .collect::<Vec<_>>(),
        vec![10, 11]
    );
    assert_eq!(
        batch
            .iter()
            .map(|event| event.offset.as_i64())
            .collect::<Vec<_>>(),
        vec![10, 11, 12]
    );
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(10))
    );
}

struct RecordingSingleHandler {
    calls: Arc<Mutex<Vec<i64>>>,
    outcome: HandlerOutcome,
}

#[async_trait::async_trait]
impl SingleEventHandler for RecordingSingleHandler {
    async fn handle(
        &self,
        event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        self.calls.lock().unwrap().push(event.offset.as_i64());
        Ok(self.outcome.clone())
    }
}

#[tokio::test]
async fn single_handler_adapter_reports_one_event_processed_on_success() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(RecordingSingleHandler {
        calls: calls.clone(),
        outcome: HandlerOutcome::Success,
    });
    let adapter = SingleEventHandlerAdapter::new(handler);
    let events = vec![raw_event(40)];
    let batch = EventBatch::new(&events);

    let outcome = adapter.handle_batch(&batch, 1).await.unwrap();

    assert!(matches!(
        outcome,
        BatchHandlerOutcome::AdvanceThrough { offset } if offset == Sequence::assigned(40)
    ));
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(40))
    );
    assert_eq!(*calls.lock().unwrap(), vec![40]);
}

#[tokio::test]
async fn single_handler_adapter_reports_retry_without_progress() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(RecordingSingleHandler {
        calls: calls.clone(),
        outcome: HandlerOutcome::Retry {
            reason: "not yet".to_owned(),
        },
    });
    let adapter = SingleEventHandlerAdapter::new(handler);
    let events = vec![raw_event(41)];
    let batch = EventBatch::new(&events);

    let outcome = adapter.handle_batch(&batch, 1).await.unwrap();

    assert!(matches!(outcome, BatchHandlerOutcome::Retry { .. }));
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(41))
    );
    assert_eq!(*calls.lock().unwrap(), vec![41]);
}

/// Batch handler that reads a fixed-size chunk from the front of the batch,
/// records the offsets it saw on each call, and returns a scripted outcome per call.
struct ChunkingHandler {
    chunk: usize,
    seen: Arc<Mutex<Vec<Vec<i64>>>>,
    scripted: Arc<Mutex<VecDeque<BatchHandlerOutcome>>>,
}

#[async_trait::async_trait]
impl ConsumerHandler for ChunkingHandler {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        let chunk: Vec<i64> = batch
            .next_chunk(self.chunk)
            .iter()
            .map(|event| event.offset.as_i64())
            .collect();
        self.seen.lock().unwrap().push(chunk);
        Ok(self
            .scripted
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted outcome"))
    }
}

/// Drive a handler through the dispatcher's redelivery model: each round hands the
/// handler a fresh `EventBatch` over the not-yet-advanced tail; the committed frontier
/// advances by `processed_count_from_outcome` (Retry advances nothing). Returns the
/// per-round outcome tags once the batch is fully processed.
async fn drive_redelivery(events: &[RawEvent], handler: &ChunkingHandler, max_rounds: usize) {
    let mut remaining: Vec<RawEvent> = events.to_vec();
    let mut rounds = 0;
    while !remaining.is_empty() {
        rounds += 1;
        assert!(rounds <= max_rounds, "redelivery did not converge");
        let batch = EventBatch::new(&remaining);
        let outcome = handler.handle_batch(&batch, rounds as u16).await.unwrap();
        match processed_count_from_outcome(&outcome, &remaining).unwrap() {
            None => { /* Retry: frontier unchanged, redeliver the same tail */ }
            Some(n) => {
                remaining.drain(0..n);
            }
        }
    }
}

// chunk -> error(retry) -> retry(readvance from front) -> partial advance -> success.
// Demonstrates that "advance" is carried by the outcome offset + the committed frontier
// (redelivery re-anchors a fresh batch), NOT by any per-batch cursor.
#[tokio::test]
async fn chunk_retry_then_partial_advance_then_success_redelivers_from_front() {
    let events = vec![raw_event(10), raw_event(11), raw_event(12), raw_event(13)];
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = ChunkingHandler {
        chunk: 2,
        seen: seen.clone(),
        scripted: Arc::new(Mutex::new(VecDeque::from(vec![
            BatchHandlerOutcome::Retry {
                reason: "downstream unavailable".to_owned(),
            },
            BatchHandlerOutcome::AdvanceThrough {
                offset: Sequence::assigned(12),
            },
            BatchHandlerOutcome::Success,
        ]))),
    };

    drive_redelivery(&events, &handler, 5).await;

    // Round 1: front chunk [10,11], Retry -> frontier unchanged.
    // Round 2: SAME front chunk [10,11] (fresh batch, cursor re-anchored), advance through 12
    //          -> tail becomes [13].
    // Round 3: front chunk [13], Success -> done.
    assert_eq!(
        *seen.lock().unwrap(),
        vec![vec![10, 11], vec![10, 11], vec![13]],
    );
}

// A partial AdvanceThrough redelivers only the un-acked tail.
#[tokio::test]
async fn partial_advance_redelivers_only_unacked_tail() {
    let events = vec![raw_event(20), raw_event(21), raw_event(22)];
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = ChunkingHandler {
        chunk: 8,
        seen: seen.clone(),
        scripted: Arc::new(Mutex::new(VecDeque::from(vec![
            BatchHandlerOutcome::AdvanceThrough {
                offset: Sequence::assigned(21),
            },
            BatchHandlerOutcome::Success,
        ]))),
    };

    drive_redelivery(&events, &handler, 5).await;

    // Round 1 sees the whole batch; advancing through 21 leaves [22] for round 2.
    assert_eq!(*seen.lock().unwrap(), vec![vec![20, 21, 22], vec![22]]);
}

// A Retry advances nothing: the committed frontier is unchanged, so the same tail redelivers.
#[tokio::test]
async fn retry_outcome_leaves_frontier_unchanged() {
    let events = vec![raw_event(30), raw_event(31)];
    let batch = EventBatch::new(&events);
    let outcome = BatchHandlerOutcome::Retry {
        reason: "not yet".to_owned(),
    };

    let processed = processed_count_from_outcome(&outcome, &events).unwrap();

    assert_eq!(processed, None);
    // Nothing advanced -> the full batch would be redelivered from the front.
    assert_eq!(batch.next_chunk(events.len()).len(), 2);
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(30))
    );
}
