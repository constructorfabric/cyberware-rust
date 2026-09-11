use crate::sequence::Sequence;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use toolkit_gts::gts_id;

use chrono::Utc;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::builder::{ConsumerRoute, ConsumerRouteHandlerKind};
use super::dispatcher::{
    PartitionSlowState, SlowConsumerReason, TopicPartitionKey, enqueue_partition_batch,
};
use super::runtime::RoutedBatchHandler;
use super::{
    BatchHandlerOutcome, ConsumerHandler, EventBatch, EventTypeRef, HandlerOutcome, RawEvent,
    SingleEventHandlerAdapter, TopicRef,
};
use crate::error::ConsumerError;
use crate::ids::TopicId;
#[cfg(feature = "test-util")]
use {
    super::dispatcher_test_util::*,
    super::{
        CommitOffset, ConnectionDropReason, ConsumerBuffering, ConsumerBuilder, ConsumerCommitMode,
        ConsumerGroupRef, ConsumerListenerSettings, ConsumerRuntimeEvent, ConsumerSlowDetection,
        Fallback, InMemoryOffsetManager, OffsetManagerError, OffsetStore, PartitionBufferState,
        Position, SlowConsumerTrigger,
    },
    crate::ids::ConsumerGroupId,
    std::collections::BTreeSet,
};

type SharedOffsets = Arc<Mutex<Vec<i64>>>;
type SharedNamedOffsets = Arc<Mutex<Vec<(&'static str, i64)>>>;
type PartitionCall = (String, u32, i64);
type SharedPartitionCalls = Arc<Mutex<Vec<PartitionCall>>>;
type TestPartitionBuffers =
    Arc<RwLock<HashMap<TopicPartitionKey, super::dispatcher::PartitionEventBuffer>>>;

fn raw_event(topic: &str, type_id: &str, offset: i64) -> RawEvent {
    raw_event_on(topic, type_id, 3, offset)
}

fn raw_event_on(topic: &str, type_id: &str, partition: u32, offset: i64) -> RawEvent {
    RawEvent {
        id: Uuid::new_v4(),
        type_id: type_id.to_owned(),
        topic: topic.to_owned(),
        tenant_id: Uuid::nil(),
        subject: format!("event-{offset}"),
        subject_type: "test".to_owned(),
        partition,
        sequence: Sequence::assigned(offset),
        offset: Sequence::assigned(offset),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "offset": offset }),
    }
}

struct RecordingSingleHandler {
    calls: SharedOffsets,
    outcome: HandlerOutcome,
}

#[async_trait::async_trait]
impl super::SingleEventHandler for RecordingSingleHandler {
    async fn handle(
        &self,
        event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        self.calls.lock().unwrap().push(event.offset.as_i64());
        Ok(self.outcome.clone())
    }
}

struct RecordingBatchHandler {
    name: &'static str,
    calls: SharedNamedOffsets,
    outcome: BatchHandlerOutcome,
}

#[async_trait::async_trait]
impl ConsumerHandler for RecordingBatchHandler {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        if let Some(event) = batch.next_event() {
            self.calls
                .lock()
                .unwrap()
                .push((self.name, event.offset.as_i64()));
        }
        Ok(self.outcome.clone())
    }
}

struct AckAllBatchHandler {
    calls: SharedPartitionCalls,
}

#[async_trait::async_trait]
impl ConsumerHandler for AckAllBatchHandler {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        let chunk = batch.next_chunk(batch.len());
        self.calls.lock().unwrap().extend(
            chunk
                .iter()
                .map(|event| (event.topic.clone(), event.partition, event.offset.as_i64())),
        );
        Ok(chunk
            .last()
            .map(|event| BatchHandlerOutcome::AdvanceThrough {
                offset: event.offset,
            })
            .unwrap_or(BatchHandlerOutcome::Success))
    }
}

#[test]
fn slow_state_detects_buffer_high_watermark_once() {
    let mut state = PartitionSlowState::default();

    assert!(
        state
            .observe_enqueue(3, Sequence::assigned(10), 4)
            .is_none()
    );
    let signal = state
        .observe_enqueue(4, Sequence::assigned(11), 4)
        .expect("high watermark triggers");

    assert_eq!(signal.reason, SlowConsumerReason::BufferHighWatermark);
    assert_eq!(signal.buffered_count, 4);
    assert_eq!(signal.latest_observed_offset, Some(Sequence::assigned(11)));
    assert!(
        state
            .observe_enqueue(5, Sequence::assigned(12), 4)
            .is_none()
    );
}

#[test]
fn slow_state_detects_handler_latency_strikes_and_resets_on_fast_completion() {
    let mut state = PartitionSlowState::default();
    let threshold = Duration::from_millis(50);

    assert!(
        state
            .observe_handler_completion(
                Duration::from_millis(75),
                threshold,
                2,
                Sequence::assigned(20)
            )
            .is_none()
    );
    assert!(
        state
            .observe_handler_completion(
                Duration::from_millis(10),
                threshold,
                2,
                Sequence::assigned(21)
            )
            .is_none()
    );

    assert!(
        state
            .observe_handler_completion(
                Duration::from_millis(75),
                threshold,
                2,
                Sequence::assigned(22)
            )
            .is_none()
    );
    let signal = state
        .observe_handler_completion(
            Duration::from_millis(80),
            threshold,
            2,
            Sequence::assigned(23),
        )
        .expect("latency strikes trigger");

    assert_eq!(signal.reason, SlowConsumerReason::HandlerLatencyStrikes);
    assert_eq!(signal.consecutive_slow_handlers, 2);
    assert_eq!(signal.last_delivered_offset, Some(Sequence::assigned(23)));
}

#[tokio::test]
async fn partition_buffer_capacity_is_isolated_per_topic_partition() {
    let buffers: TestPartitionBuffers = Arc::new(RwLock::new(HashMap::new()));
    let topic = gts_id!("cf.core.events.topic.v1~example.dispatcher.buffer.x.v1");
    let topic_id = TopicId::from_gts(topic);
    let partition_zero = TopicPartitionKey::new(topic, topic_id, 0);
    let partition_one = TopicPartitionKey::new(topic, topic_id, 1);

    let first_partition_zero = enqueue_partition_batch(
        &buffers,
        partition_zero.clone(),
        raw_event_on(topic, "BufferEvent", 0, 10),
        1,
    )
    .await
    .expect("first event in partition 0 fits");
    assert_eq!(first_partition_zero.buffered_count, 1);

    let overflow_partition_zero = enqueue_partition_batch(
        &buffers,
        partition_zero,
        raw_event_on(topic, "BufferEvent", 0, 11),
        1,
    )
    .await
    .expect_err("partition 0 capacity is exhausted");
    assert!(
        overflow_partition_zero
            .to_string()
            .contains("partition buffer capacity 1 exceeded"),
        "unexpected overflow error: {overflow_partition_zero}"
    );

    let first_partition_one = enqueue_partition_batch(
        &buffers,
        partition_one,
        raw_event_on(topic, "BufferEvent", 1, 20),
        1,
    )
    .await
    .expect("partition 1 has independent capacity");
    assert_eq!(first_partition_one.buffered_count, 1);
}

fn route(topic: &str, event_type: Option<&str>) -> ConsumerRoute {
    ConsumerRoute {
        topic: TopicRef::gts(topic),
        event_type: event_type.map(EventTypeRef::gts),
        handler_kind: ConsumerRouteHandlerKind::Batch,
    }
}

#[tokio::test]
async fn single_handler_adapter_dispatches_one_event_batch() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let handler = SingleEventHandlerAdapter::new(Arc::new(RecordingSingleHandler {
        calls: calls.clone(),
        outcome: HandlerOutcome::Success,
    }));
    let events = vec![
        raw_event_on("orders", "OrderCreated", 7, 20),
        raw_event_on("orders", "OrderUpdated", 7, 21),
    ];
    let batch = EventBatch::new(&events);

    let outcome = handler.handle_batch(&batch, 1).await.unwrap();

    assert_eq!(*calls.lock().unwrap(), vec![20]);
    assert!(matches!(
        outcome,
        BatchHandlerOutcome::AdvanceThrough { offset } if offset == Sequence::assigned(20)
    ));
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(20))
    );
}

#[tokio::test]
async fn native_batch_handler_dispatches_multiple_events_from_one_partition() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let handler = AckAllBatchHandler {
        calls: calls.clone(),
    };
    let events = vec![
        raw_event_on("orders", "OrderCreated", 5, 30),
        raw_event_on("orders", "OrderUpdated", 5, 31),
        raw_event_on("orders", "OrderClosed", 5, 32),
    ];
    let batch = EventBatch::new(&events);

    let outcome = handler.handle_batch(&batch, 1).await.unwrap();

    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            ("orders".to_owned(), 5, 30),
            ("orders".to_owned(), 5, 31),
            ("orders".to_owned(), 5, 32),
        ]
    );
    assert!(matches!(
        outcome,
        BatchHandlerOutcome::AdvanceThrough { offset } if offset == Sequence::assigned(32)
    ));
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(30))
    );
}

#[tokio::test]
async fn routed_dispatch_prefers_exact_topic_and_event_type_route() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let routed = RoutedBatchHandler::new(
        Some(Arc::new(RecordingBatchHandler {
            name: "default",
            calls: calls.clone(),
            outcome: BatchHandlerOutcome::Success,
        })),
        vec![route("orders", Some("OrderCreated"))],
        vec![Arc::new(RecordingBatchHandler {
            name: "orders-created",
            calls: calls.clone(),
            outcome: BatchHandlerOutcome::Success,
        })],
    )
    .unwrap();
    let events = vec![raw_event("orders", "OrderCreated", 10)];
    let batch = EventBatch::new(&events);

    let outcome = routed.handle_batch(&batch, 1).await.unwrap();

    assert_eq!(*calls.lock().unwrap(), vec![("orders-created", 10)]);
    assert!(matches!(outcome, BatchHandlerOutcome::Success));
}

#[tokio::test]
async fn routed_dispatch_uses_topic_catch_all_before_default_handler() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let routed = RoutedBatchHandler::new(
        Some(Arc::new(RecordingBatchHandler {
            name: "default",
            calls: calls.clone(),
            outcome: BatchHandlerOutcome::Success,
        })),
        vec![route("orders", None)],
        vec![Arc::new(RecordingBatchHandler {
            name: "orders-any",
            calls: calls.clone(),
            outcome: BatchHandlerOutcome::Success,
        })],
    )
    .unwrap();
    let events = vec![raw_event("orders", "OrderCancelled", 11)];
    let batch = EventBatch::new(&events);

    let outcome = routed.handle_batch(&batch, 1).await.unwrap();

    assert_eq!(*calls.lock().unwrap(), vec![("orders-any", 11)]);
    assert!(matches!(outcome, BatchHandlerOutcome::Success));
}

#[tokio::test]
async fn routed_dispatch_falls_back_to_default_handler() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let routed = RoutedBatchHandler::new(
        Some(Arc::new(RecordingBatchHandler {
            name: "default",
            calls: calls.clone(),
            outcome: BatchHandlerOutcome::Success,
        })),
        vec![route("payments", None)],
        vec![Arc::new(RecordingBatchHandler {
            name: "payments-any",
            calls: calls.clone(),
            outcome: BatchHandlerOutcome::Success,
        })],
    )
    .unwrap();
    let events = vec![raw_event("orders", "OrderCancelled", 12)];
    let batch = EventBatch::new(&events);

    let outcome = routed.handle_batch(&batch, 1).await.unwrap();

    assert_eq!(*calls.lock().unwrap(), vec![("default", 12)]);
    assert!(matches!(outcome, BatchHandlerOutcome::Success));
}

#[tokio::test]
async fn routed_dispatch_fails_visibly_without_matching_route_or_default() {
    let routed = RoutedBatchHandler::new(
        None,
        vec![route("payments", None)],
        vec![Arc::new(RecordingBatchHandler {
            name: "payments-any",
            calls: Arc::new(Mutex::new(Vec::new())),
            outcome: BatchHandlerOutcome::Success,
        })],
    )
    .unwrap();
    let events = vec![raw_event("orders", "OrderCancelled", 13)];
    let batch = EventBatch::new(&events);

    let err = routed.handle_batch(&batch, 1).await.unwrap_err();

    assert!(
        err.to_string().contains("no consumer route matched"),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(13))
    );
}

#[tokio::test]
async fn routed_dispatch_preserves_adjacent_event_order_across_routes() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let routed = RoutedBatchHandler::new(
        None,
        vec![
            route("orders", Some("OrderCreated")),
            route("orders", Some("OrderCancelled")),
        ],
        vec![
            Arc::new(RecordingBatchHandler {
                name: "created",
                calls: calls.clone(),
                outcome: BatchHandlerOutcome::Success,
            }),
            Arc::new(RecordingBatchHandler {
                name: "cancelled",
                calls: calls.clone(),
                outcome: BatchHandlerOutcome::Success,
            }),
        ],
    )
    .unwrap();
    let events = vec![
        raw_event_on("orders", "OrderCreated", 3, 40),
        raw_event_on("orders", "OrderCancelled", 3, 41),
    ];
    let batch = EventBatch::new(&events);
    let second_batch = EventBatch::new(&events[1..]);

    routed.handle_batch(&batch, 1).await.unwrap();
    routed.handle_batch(&second_batch, 1).await.unwrap();

    assert_eq!(
        *calls.lock().unwrap(),
        vec![("created", 40), ("cancelled", 41)]
    );
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(40))
    );
    assert_eq!(
        second_batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(41))
    );
}

#[tokio::test]
async fn routed_dispatch_retry_does_not_advance_past_earlier_unprocessed_event() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let routed = RoutedBatchHandler::new(
        None,
        vec![
            route("orders", Some("OrderCreated")),
            route("orders", Some("OrderCancelled")),
        ],
        vec![
            Arc::new(RecordingBatchHandler {
                name: "created",
                calls: calls.clone(),
                outcome: BatchHandlerOutcome::Retry {
                    reason: "try later".to_owned(),
                },
            }),
            Arc::new(RecordingBatchHandler {
                name: "cancelled",
                calls: calls.clone(),
                outcome: BatchHandlerOutcome::Success,
            }),
        ],
    )
    .unwrap();
    let events = vec![
        raw_event_on("orders", "OrderCreated", 3, 50),
        raw_event_on("orders", "OrderCancelled", 3, 51),
    ];
    let batch = EventBatch::new(&events);

    let outcome = routed.handle_batch(&batch, 1).await.unwrap();

    assert!(matches!(outcome, BatchHandlerOutcome::Retry { .. }));
    assert_eq!(*calls.lock().unwrap(), vec![("created", 50)]);
    assert_eq!(
        batch.next_event().map(|event| event.offset),
        Some(Sequence::assigned(50))
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn runtime_dispatch_never_mixes_topics_or_partitions_in_handler_batches() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle, PartitionKeyFixture};
    use crate::models::Event;
    use std::collections::BTreeSet;

    const ORDERS_TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.orders.v1");
    const PAYMENTS_TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.payments.v1");
    const ORDERS_EVENT: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.order.v1~");
    const PAYMENTS_EVENT: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.payment.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(ORDERS_TOPIC, 2).await;
    control.register_topic(PAYMENTS_TOPIC, 2).await;
    control
        .register_event_type(
            ORDERS_TOPIC,
            ORDERS_EVENT,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .register_event_type(
            PAYMENTS_TOPIC,
            PAYMENTS_EVENT,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    // Both types route by `/subject`, a member this test varies. Under the default
    // tenant pointer every event would land on one partition per topic, and the
    // per-`(topic, partition)` batch scope would never be exercised across two.
    for event_type in [ORDERS_EVENT, PAYMENTS_EVENT] {
        control
            .set_partition_key(PartitionKeyFixture {
                event_type,
                pointer: "/subject",
            })
            .await;
    }
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let recorder = BatchScopeRecorder::default();
    let scopes = recorder.scopes.clone();
    let violations = recorder.violations.clone();

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("batch-scope"))
        .topics([ORDERS_TOPIC, PAYMENTS_TOPIC])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(recorder)
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    for (_topic, event_type, partition) in [
        (ORDERS_TOPIC, ORDERS_EVENT, 0),
        (ORDERS_TOPIC, ORDERS_EVENT, 1),
        (PAYMENTS_TOPIC, PAYMENTS_EVENT, 0),
        (PAYMENTS_TOPIC, PAYMENTS_EVENT, 1),
    ] {
        broker
            .publish(
                &ctx,
                &Event {
                    id: Uuid::new_v4(),
                    type_id: event_type.to_owned(),
                    tenant_id: ctx.subject_tenant_id(),
                    source: "consumer.dispatcher.test".to_owned(),
                    subject: partition_key_for_partition(partition, 2),
                    subject_type: "test".to_owned(),
                    occurred_at: Utc::now(),
                    trace_parent: None,
                    data: Some(serde_json::json!({ "partition": partition })),
                    partition: None,
                    sequence: None,
                    sequence_time: None,
                    meta: None,
                },
            )
            .await
            .expect("event published");
    }

    for _ in 0..100 {
        if scopes.lock().unwrap().len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.stop().await.expect("consumer stops");

    assert_eq!(*violations.lock().unwrap(), Vec::<String>::new());
    let recorded = scopes.lock().unwrap().clone();
    assert_eq!(recorded.len(), 4);
    assert!(recorded.iter().all(|(_, _, len)| *len == 1));
    let topics = recorded
        .iter()
        .map(|(topic, _, _)| topic.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(topics, BTreeSet::from([ORDERS_TOPIC, PAYMENTS_TOPIC]));
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn parallelism_creates_independent_slots_with_shared_group_and_interests() {
    use crate::EventBrokerApi;
    use crate::ids::ConsumerGroupId;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use std::collections::{BTreeSet, HashSet};

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.parallel.v1");
    const GROUP: &str =
        gts_id!("cf.core.events.consumer_group.v1~example.mock.consumer.parallel.v1");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 4).await;
    control.register_named_group(GROUP).await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let group = ConsumerGroupId::from_gts(GROUP);
    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let handle = ConsumerBuilder::new(broker)
        .group(ConsumerGroupRef::id(group))
        .topics([TOPIC])
        .parallelism(2)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(AckAllBatchHandler {
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .start()
        .await
        .expect("consumer starts");

    let subscriptions = wait_for_parallel_assignments(&handle, &control, &group, 2, 4).await;

    let members = control.members(&group).await;
    let member_set = members.iter().copied().collect::<HashSet<_>>();
    let subscription_set = subscriptions.iter().copied().collect::<HashSet<_>>();
    assert_eq!(
        member_set, subscription_set,
        "parallel slots should join the same consumer group"
    );

    let mut all_assigned = BTreeSet::new();
    for sub_id in &subscriptions {
        let assignment = control.assignment(*sub_id).await;
        assert_eq!(
            assignment.len(),
            2,
            "each of two slots should own half of the four partitions"
        );
        for slot in assignment {
            assert_eq!(slot.topic, TOPIC);
            all_assigned.insert((slot.topic, slot.partition));
        }
    }
    assert_eq!(
        all_assigned,
        (0..4)
            .map(|partition| (TOPIC.to_owned(), partition))
            .collect::<BTreeSet<_>>(),
        "parallel slots should cover every partition exactly once"
    );

    handle.stop().await.expect("consumer stops");
}

#[cfg(feature = "test-util")]
async fn wait_for_parallel_assignments(
    handle: &super::runtime::ConsumerHandle,
    control: &crate::mock::MockBrokerHandle,
    group: &ConsumerGroupId,
    expected_slots: usize,
    expected_total_partitions: usize,
) -> Vec<crate::ids::SubscriptionId> {
    for _ in 0..100 {
        let subscriptions = handle.subscription_ids();
        let members = control.members(group).await;
        if subscriptions.len() == expected_slots && members.len() == expected_slots {
            let mut assigned_total = 0usize;
            let mut all_non_empty = true;
            for sub_id in &subscriptions {
                let assignment = control.assignment(*sub_id).await;
                assigned_total += assignment.len();
                all_non_empty &= !assignment.is_empty();
            }
            if all_non_empty && assigned_total == expected_total_partitions {
                return subscriptions;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "parallel consumer did not expose {expected_slots} assigned subscription slots; ids={:?}, members={:?}",
        handle.subscription_ids(),
        control.members(group).await
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn slow_detection_emits_listener_events_and_drops_subscription_stream() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.slow.v1");
    const EVENT_TYPE: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.slow.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 1).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let listener = RecordingRuntimeListener::default();
    let recorded = listener.events.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("slow-drop"))
        .topics([TOPIC])
        .buffering(ConsumerBuffering {
            partition_capacity: 8,
            high_watermark: 1,
            low_watermark: 0,
        })
        .register_listener(listener)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(AckAllBatchHandler {
            calls: calls.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    broker
        .publish(
            &ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: EVENT_TYPE.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                source: "consumer.dispatcher.test".to_owned(),
                subject: "slow-subject".to_owned(),
                subject_type: "test".to_owned(),
                occurred_at: Utc::now(),
                trace_parent: None,
                data: Some(serde_json::json!({ "slow": true })),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");

    for _ in 0..100 {
        let events = recorded.lock().unwrap().clone();
        let has_state = events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::PartitionBufferStateChanged { state }
                    if state.state == PartitionBufferState::SlowDetected
            )
        });
        let has_drop = events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::SubscriptionConnectionDropped {
                    reason: ConnectionDropReason::SlowConsumer { .. },
                    affected,
                    ..
                } if !affected.is_empty()
            )
        });
        if has_state && has_drop {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    handle.stop().await.expect("consumer stops");

    let events = recorded.lock().unwrap();
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::PartitionBufferStateChanged { state }
                    if state.state == PartitionBufferState::SlowDetected
                        && state.topic == TOPIC
                        && state.buffered_count == 1
            )
        }),
        "missing slow buffer state event: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::SubscriptionConnectionDropped {
                    reason: ConnectionDropReason::SlowConsumer { topic, .. },
                    affected,
                    ..
                } if topic == TOPIC && !affected.is_empty()
            )
        }),
        "missing slow connection drop event: {events:?}"
    );
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(topic, partition, _)| topic == TOPIC && *partition == 0),
        "slow event should be drained through the handler before rejoin"
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn slow_drop_reports_other_assignments_owned_by_same_subscription_slot() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;
    use std::collections::BTreeSet;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.affected.v1");
    const EVENT_TYPE: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.affected.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 2).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let listener = RecordingRuntimeListener::default();
    let recorded = listener.events.clone();

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("affected-assignments"))
        .topics([TOPIC])
        .buffering(ConsumerBuffering {
            partition_capacity: 8,
            high_watermark: 1,
            low_watermark: 0,
        })
        .register_listener(listener)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(AckAllBatchHandler {
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    broker
        .publish(
            &ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: EVENT_TYPE.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                source: "consumer.dispatcher.test".to_owned(),
                subject: "affected-subject".to_owned(),
                subject_type: "test".to_owned(),
                occurred_at: Utc::now(),
                trace_parent: None,
                data: Some(serde_json::json!({ "slow": true })),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");

    let affected = wait_for_slow_drop_affected_assignments(&recorded).await;
    handle.stop().await.expect("consumer stops");

    assert_eq!(
        affected,
        BTreeSet::from([(TOPIC.to_owned(), 0), (TOPIC.to_owned(), 1)]),
        "slow partition should report every assignment on the dropped subscription slot"
    );
}

#[cfg(feature = "test-util")]
async fn wait_for_slow_drop_affected_assignments(
    recorded: &SharedRuntimeEvents,
) -> BTreeSet<(String, u32)> {
    for _ in 0..100 {
        let events = recorded.lock().unwrap().clone();
        if let Some(affected) = events.iter().find_map(|event| {
            if let ConsumerRuntimeEvent::SubscriptionConnectionDropped {
                reason: ConnectionDropReason::SlowConsumer { .. },
                affected,
                ..
            } = event
            {
                Some(
                    affected
                        .iter()
                        .map(|slot| (slot.topic.clone(), slot.partition))
                        .collect::<BTreeSet<_>>(),
                )
            } else {
                None
            }
        }) {
            return affected;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "did not observe slow-drop affected assignments; events={:?}",
        recorded.lock().unwrap()
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn runtime_listener_observes_representative_non_dlq_event_variants() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.listener_all.v1");
    const EVENT_TYPE: &str =
        gts_id!("cf.core.events.event.v1~example.mock.broker.listener_all.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 1).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let listener = RecordingRuntimeListener::default();
    let recorded = listener.events.clone();
    let handler_calls = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("listener-all"))
        .topics([TOPIC])
        .buffering(ConsumerBuffering {
            partition_capacity: 8,
            high_watermark: 1,
            low_watermark: 0,
        })
        .retry_base(Duration::from_millis(1))
        .retry_max(Duration::from_millis(1))
        .register_listener(listener)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(FailingThenCommitBatchHandler {
            failures_remaining: Arc::new(Mutex::new(1)),
            calls: handler_calls.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    broker
        .publish(
            &ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: EVENT_TYPE.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                source: "consumer.dispatcher.test".to_owned(),
                subject: "listener-all-subject".to_owned(),
                subject_type: "test".to_owned(),
                occurred_at: Utc::now(),
                trace_parent: None,
                data: Some(serde_json::json!({ "representative": true })),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");

    let expected_before_stop = BTreeSet::from([
        RuntimeEventKind::SubscriptionJoining,
        RuntimeEventKind::SubscriptionStarted,
        RuntimeEventKind::SubscriptionRejoining,
        RuntimeEventKind::SubscriptionConnectionDropped,
        RuntimeEventKind::AssignmentChanged,
        RuntimeEventKind::ProgressAdvanced,
        RuntimeEventKind::PartitionBufferStateChanged,
        RuntimeEventKind::HandlerBatchStarted,
        RuntimeEventKind::HandlerBatchCompleted,
        RuntimeEventKind::HandlerFailed,
        RuntimeEventKind::OffsetLoaded,
        RuntimeEventKind::OffsetCommitted,
        RuntimeEventKind::RetryScheduled,
    ]);
    wait_for_runtime_event_kinds(&recorded, &expected_before_stop).await;

    handle.stop().await.expect("consumer stops");

    let expected_after_stop = expected_before_stop
        .into_iter()
        .chain([RuntimeEventKind::SubscriptionTerminated])
        .collect::<BTreeSet<_>>();
    wait_for_runtime_event_kinds(&recorded, &expected_after_stop).await;

    assert_eq!(*handler_calls.lock().unwrap(), vec![2]);
}

#[cfg(feature = "test-util")]
async fn wait_for_runtime_event_kinds(
    recorded: &SharedRuntimeEvents,
    expected: &BTreeSet<RuntimeEventKind>,
) -> BTreeSet<RuntimeEventKind> {
    for _ in 0..100 {
        let observed = recorded
            .lock()
            .unwrap()
            .iter()
            .map(runtime_event_kind)
            .collect::<BTreeSet<_>>();
        if expected.is_subset(&observed) {
            return observed;
        }
        drop(observed);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "missing representative runtime event kinds; expected={expected:?}, events={:?}",
        recorded.lock().unwrap()
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn listener_failure_does_not_commit_drop_or_stop_consumer_events() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.listener_failure.v1");
    const EVENT_TYPE: &str =
        gts_id!("cf.core.events.event.v1~example.mock.broker.listener_failure.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 1).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let recorder = RecordingRuntimeListener::default();
    let recorded_events = recorder.events.clone();
    let handler_calls = Arc::new(Mutex::new(Vec::new()));
    let offset_manager = RecordingCommitOffsetManager::default();
    let commits = offset_manager.commits.clone();

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("listener-failure"))
        .topics([TOPIC])
        .commit_mode(ConsumerCommitMode::manual())
        .register_listener(FailingRuntimeListener)
        .register_listener(recorder)
        .offset_manager(offset_manager)
        .batch_handler(AckAllBatchHandler {
            calls: handler_calls.clone(),
        })
        .start()
        .await
        .expect("consumer starts despite listener that will fail");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    for offset in 0..2 {
        broker
            .publish(
                &ctx,
                &Event {
                    id: Uuid::new_v4(),
                    type_id: EVENT_TYPE.to_owned(),
                    tenant_id: ctx.subject_tenant_id(),
                    source: "consumer.dispatcher.test".to_owned(),
                    subject: format!("listener-failure-{offset}"),
                    subject_type: "test".to_owned(),
                    occurred_at: Utc::now(),
                    trace_parent: None,
                    data: Some(serde_json::json!({ "offset": offset })),
                    partition: None,
                    sequence: None,
                    sequence_time: None,
                    meta: None,
                },
            )
            .await
            .expect("event published");
    }

    for _ in 0..100 {
        if handler_calls.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let active_subscriptions = handle.subscription_ids();
    handle.stop().await.expect("consumer stops cleanly");

    let calls = handler_calls.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        2,
        "listener failure must not drop consumer events before handler dispatch"
    );
    let commits = commits.lock().unwrap().clone();
    // Nothing beyond "no position" may be committed. This used to read
    // `*offset < 0`, from when the mock reported -1 for a partition it had
    // never scanned; the floor of the cursor space is 0, so the same intent -
    // no *delivered* event's position is committed - is that every recorded
    // offset is still the empty one.
    assert!(
        commits
            .iter()
            .all(|(_, _, _, offset)| *offset == Sequence::NONE.as_i64()),
        "listener failure must not durably advance delivered event offsets by itself: {commits:?}"
    );
    assert!(
        !active_subscriptions.is_empty(),
        "listener failure must not stop the consumer by itself"
    );
    let events = recorded_events.lock().unwrap();
    assert!(
        !events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::SubscriptionConnectionDropped { .. }
            )
        }),
        "listener failure must not drop the subscription stream: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::HandlerBatchCompleted {
                    outcome: BatchHandlerOutcome::AdvanceThrough { .. },
                    ..
                }
            )
        }),
        "the recording listener should still observe handler success after another listener fails"
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn slow_listener_timeout_does_not_block_runtime_delivery_or_handler_processing() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.listener_timeout.v1");
    const EVENT_TYPE: &str =
        gts_id!("cf.core.events.event.v1~example.mock.broker.listener_timeout.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 1).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let recorder = RecordingRuntimeListener::default();
    let recorded_events = recorder.events.clone();
    let handler_calls = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("listener-timeout"))
        .topics([TOPIC])
        .listener_settings(ConsumerListenerSettings {
            channel_capacity: 8,
            timeout: Duration::from_millis(1),
        })
        .register_listener(SlowRuntimeListener {
            delay: Duration::from_secs(60),
        })
        .register_listener(recorder)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(AckAllBatchHandler {
            calls: handler_calls.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    broker
        .publish(
            &ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: EVENT_TYPE.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                source: "consumer.dispatcher.test".to_owned(),
                subject: "listener-timeout-subject".to_owned(),
                subject_type: "test".to_owned(),
                occurred_at: Utc::now(),
                trace_parent: None,
                data: Some(serde_json::json!({ "timeout": true })),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let handler_done = !handler_calls.lock().unwrap().is_empty();
            let listener_done = recorded_events.lock().unwrap().iter().any(|event| {
                matches!(
                    event,
                    ConsumerRuntimeEvent::HandlerBatchCompleted {
                        outcome: BatchHandlerOutcome::AdvanceThrough { .. },
                        ..
                    }
                )
            });
            if handler_done && listener_done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("slow listener timeout should let processing continue");

    handle.stop().await.expect("consumer stops");

    assert_eq!(handler_calls.lock().unwrap().len(), 1);
    assert!(
        recorded_events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ConsumerRuntimeEvent::SubscriptionStarted { .. })),
        "recording listener should receive events after the slow listener times out"
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn handler_latency_strikes_emit_listener_events_and_drop_subscription_stream() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.latency.v1");
    const EVENT_TYPE: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.latency.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 1).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let listener = RecordingRuntimeListener::default();
    let recorded = listener.events.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("latency-drop"))
        .topics([TOPIC])
        .buffering(ConsumerBuffering {
            partition_capacity: 8,
            high_watermark: 8,
            low_watermark: 0,
        })
        .slow_detection(ConsumerSlowDetection {
            handler_latency: Duration::from_millis(1),
            handler_strikes: 1,
        })
        .register_listener(listener)
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(SleepingBatchHandler {
            calls: calls.clone(),
            delay: Duration::from_millis(10),
        })
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    broker
        .publish(
            &ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: EVENT_TYPE.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                source: "consumer.dispatcher.test".to_owned(),
                subject: "latency-subject".to_owned(),
                subject_type: "test".to_owned(),
                occurred_at: Utc::now(),
                trace_parent: None,
                data: Some(serde_json::json!({ "slow": "handler" })),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");

    for _ in 0..100 {
        let events = recorded.lock().unwrap().clone();
        let has_latency_drop = events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::SubscriptionConnectionDropped {
                    reason: ConnectionDropReason::SlowConsumer {
                        trigger: SlowConsumerTrigger::HandlerLatencyStrikes,
                        ..
                    },
                    affected,
                    ..
                } if !affected.is_empty()
            )
        });
        if has_latency_drop {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    handle.stop().await.expect("consumer stops");

    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(topic, partition, _)| topic == TOPIC && *partition == 0),
        "slow handler should process the event before latency drop"
    );
    let events = recorded.lock().unwrap();
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::PartitionBufferStateChanged { state }
                    if state.state == PartitionBufferState::SlowDetected
                        && state.topic == TOPIC
                        && state.trigger == Some(SlowConsumerTrigger::HandlerLatencyStrikes)
                        && state.consecutive_slow_handlers == 1
            )
        }),
        "missing latency slow buffer state event: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::SubscriptionConnectionDropped {
                    reason: ConnectionDropReason::SlowConsumer {
                        topic,
                        trigger: SlowConsumerTrigger::HandlerLatencyStrikes,
                        ..
                    },
                    affected,
                    ..
                } if topic == TOPIC && !affected.is_empty()
            )
        }),
        "missing latency connection drop event: {events:?}"
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn slow_drop_drains_buffer_before_rejoin_load_position() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.drain_rejoin.v1");
    const EVENT_TYPE: &str =
        gts_id!("cf.core.events.event.v1~example.mock.broker.drain_rejoin.v1~");

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 1).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let timeline = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("drain-rejoin"))
        .topics([TOPIC])
        .buffering(ConsumerBuffering {
            partition_capacity: 8,
            high_watermark: 1,
            low_watermark: 0,
        })
        .commit_mode(ConsumerCommitMode::manual())
        .offset_manager(SequencedOffsetManager {
            timeline: timeline.clone(),
        })
        .batch_handler(SequencedBatchHandler {
            timeline: timeline.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    broker
        .publish(
            &ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: EVENT_TYPE.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                source: "consumer.dispatcher.test".to_owned(),
                subject: "drain-rejoin-subject".to_owned(),
                subject_type: "test".to_owned(),
                occurred_at: Utc::now(),
                trace_parent: None,
                data: Some(serde_json::json!({ "slow": "high-watermark" })),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");

    let observed = wait_for_drain_rejoin_timeline(&timeline).await;
    handle.stop().await.expect("consumer stops");

    let first_load = observed
        .iter()
        .position(|entry| *entry == "load")
        .expect("initial load recorded");
    let first_handle = observed
        .iter()
        .position(|entry| *entry == "handle")
        .expect("buffer drain handler recorded");
    let second_load_after_handle = observed
        .iter()
        .enumerate()
        .skip(first_handle + 1)
        .find_map(|(idx, entry)| (*entry == "load").then_some(idx))
        .expect("rejoin load recorded after drain");

    assert!(
        first_load < first_handle && first_handle < second_load_after_handle,
        "expected load -> handle -> load ordering, got {observed:?}"
    );
}

#[cfg(feature = "test-util")]
async fn wait_for_drain_rejoin_timeline(timeline: &SharedTimeline) -> Vec<&'static str> {
    for _ in 0..100 {
        let observed = timeline.lock().unwrap().clone();
        let Some(first_handle) = observed.iter().position(|entry| *entry == "handle") else {
            drop(observed);
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        if observed
            .iter()
            .skip(first_handle + 1)
            .any(|entry| *entry == "load")
        {
            return observed;
        }
        drop(observed);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "did not observe drain-before-rejoin sequence; timeline={:?}",
        timeline.lock().unwrap()
    );
}

#[cfg(feature = "test-util")]
#[tokio::test]
async fn async_auto_commit_uses_resolved_group_topic_partition_and_frontier_offset() {
    use crate::EventBrokerApi;
    use crate::mock::stubs::test_ctx_for_tenant;
    use crate::mock::{MockBroker, MockBrokerHandle};
    use crate::models::Event;

    const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.mock.broker.audit.v1");
    const EVENT_TYPE: &str = gts_id!("cf.core.events.event.v1~example.mock.broker.event.v1~");

    #[derive(Clone, Default)]
    struct RecordingOffsetManager {
        commits: SharedCommits,
    }

    #[async_trait::async_trait]
    impl OffsetStore for RecordingOffsetManager {
        async fn load_position(
            &self,
            _group: &ConsumerGroupId,
            _topic: &TopicId,
            _partition: u32,
        ) -> Result<Position, OffsetManagerError> {
            Ok(Position::Earliest)
        }
    }

    #[async_trait::async_trait]
    impl CommitOffset for RecordingOffsetManager {
        async fn commit(
            &self,
            group: &ConsumerGroupId,
            topic: &TopicId,
            partition: u32,
            offset: Sequence,
        ) -> Result<(), OffsetManagerError> {
            self.commits.lock().expect("recording commits").push((
                *group,
                *topic,
                partition,
                offset.as_i64(),
            ));
            Ok(())
        }
    }

    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(TOPIC, 1).await;
    control
        .register_event_type(
            TOPIC,
            EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let broker: Arc<dyn EventBrokerApi> = Arc::new(mock);
    let ctx = test_ctx_for_tenant(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
    let offset_manager = RecordingOffsetManager::default();
    let commits = offset_manager.commits.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("auto-commit-metadata"))
        .topics([TOPIC])
        .commit_mode(ConsumerCommitMode::auto(Duration::from_millis(5)))
        .offset_manager(offset_manager)
        .batch_handler(RecordingBatchHandler {
            name: "default",
            calls,
            outcome: BatchHandlerOutcome::Success,
        })
        .start()
        .await
        .expect("consumer starts");

    for _ in 0..100 {
        if handle.subscription_ids().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    broker
        .publish(
            &ctx,
            &Event {
                id: Uuid::new_v4(),
                type_id: EVENT_TYPE.to_owned(),
                tenant_id: ctx.subject_tenant_id(),
                source: "consumer.dispatcher.test".to_owned(),
                subject: "subject-1".to_owned(),
                subject_type: "test".to_owned(),
                occurred_at: Utc::now(),
                trace_parent: None,
                data: Some(serde_json::json!({ "ok": true })),
                partition: None,
                sequence: None,
                sequence_time: None,
                meta: None,
            },
        )
        .await
        .expect("event published");

    let commit = wait_for_first_non_negative_commit(&commits).await;
    handle.stop().await.expect("consumer stops");

    assert_ne!(commit.0, ConsumerGroupId::new(Uuid::nil()));
    assert_eq!(commit.1, TopicId::from_gts(TOPIC));
    assert_eq!(commit.2, 0);
    assert!(commit.3 >= 0);
}

#[cfg(feature = "test-util")]
async fn wait_for_first_non_negative_commit(
    commits: &SharedCommits,
) -> (ConsumerGroupId, TopicId, u32, i64) {
    for _ in 0..100 {
        if let Some(commit) = commits
            .lock()
            .expect("recording commits")
            .iter()
            .copied()
            .find(|commit| commit.3 >= 0)
        {
            return commit;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("auto-commit did not persist a recorded event offset");
}
