use event_broker_sdk::Sequence;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_broker_sdk::mock::stubs::test_ctx_for_tenant;
use event_broker_sdk::mock::{MockBroker, MockBrokerHandle};
use event_broker_sdk::{
    BatchHandlerOutcome, CommitOffset, ConnectionDropReason, ConsumerBuffering, ConsumerBuilder,
    ConsumerError, ConsumerGroupId, ConsumerGroupRef, ConsumerHandler, ConsumerRuntimeEvent,
    ConsumerRuntimeListener, EventBatch, EventBrokerApi, Fallback, InMemoryOffsetManager,
    OffsetManagerError, OffsetStore, Position, TopicId,
};
use uuid::Uuid;

use super::common::{TENANT, publish_json, wait_until};

const SLOW_TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.slow.v1";
const SLOW_EVENT_TYPE: &str = "gts.cf.core.events.event.v1~example.mock.showcase.slow.v1~";
const FAST_TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.fast.v1";
const FAST_EVENT_TYPE: &str = "gts.cf.core.events.event.v1~example.mock.showcase.fast.v1~";

#[derive(Clone, Default)]
struct RecordingRuntimeListener {
    events: Arc<Mutex<Vec<ConsumerRuntimeEvent>>>,
}

#[async_trait]
impl ConsumerRuntimeListener for RecordingRuntimeListener {
    async fn on_consumer_event(&self, event: &ConsumerRuntimeEvent) -> Result<(), ConsumerError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[derive(Clone)]
struct SequencedOffsetManager {
    timeline: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl OffsetStore for SequencedOffsetManager {
    async fn load_position(
        &self,
        _group: &ConsumerGroupId,
        _topic: &TopicId,
        _partition: u32,
    ) -> Result<Position, OffsetManagerError> {
        self.timeline.lock().unwrap().push("load");
        Ok(Position::Earliest)
    }
}

#[async_trait]
impl CommitOffset for SequencedOffsetManager {
    async fn commit(
        &self,
        _group: &ConsumerGroupId,
        _topic: &TopicId,
        _partition: u32,
        _offset: Sequence,
    ) -> Result<(), OffsetManagerError> {
        Ok(())
    }
}

struct SequencedBatchHandler {
    timeline: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl ConsumerHandler for SequencedBatchHandler {
    async fn handle_batch(
        &self,
        _batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        self.timeline.lock().unwrap().push("handle");
        Ok(BatchHandlerOutcome::Success)
    }
}

struct RecordingBatchHandler {
    subjects: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ConsumerHandler for RecordingBatchHandler {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        let chunk = batch.next_chunk(batch.len());
        self.subjects
            .lock()
            .unwrap()
            .extend(chunk.iter().map(|event| event.subject.clone()));
        Ok(chunk
            .last()
            .map(|event| BatchHandlerOutcome::AdvanceThrough {
                offset: event.offset,
            })
            .unwrap_or(BatchHandlerOutcome::Success))
    }
}

async fn broker_with_slow_and_fast_topics() -> (
    Arc<dyn EventBrokerApi>,
    MockBrokerHandle,
    toolkit_security::SecurityContext,
) {
    let mock = MockBroker::new();
    let control = MockBrokerHandle::from_broker(&mock);
    control.register_topic(SLOW_TOPIC, 2).await;
    control
        .register_event_type(
            SLOW_TOPIC,
            SLOW_EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control.register_topic(FAST_TOPIC, 1).await;
    control
        .register_event_type(
            FAST_TOPIC,
            FAST_EVENT_TYPE,
            serde_json::json!({ "type": "object" }),
            &[],
        )
        .await;
    control
        .set_heartbeat_interval(std::time::Duration::from_millis(10))
        .await;

    (
        Arc::new(mock),
        control,
        test_ctx_for_tenant(Uuid::parse_str(TENANT).expect("tenant uuid")),
    )
}

#[tokio::test]
async fn if_a_partition_is_slow_the_sdk_drops_drains_and_rejoins_from_offsets() {
    let (broker, _control, ctx) = broker_with_slow_and_fast_topics().await;
    let listener = RecordingRuntimeListener::default();
    let events = listener.events.clone();
    let timeline = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-slow-drop"))
        .topics([SLOW_TOPIC])
        .buffering(ConsumerBuffering {
            partition_capacity: 8,
            high_watermark: 1,
            low_watermark: 0,
        })
        .register_listener(listener)
        .offset_manager(SequencedOffsetManager {
            timeline: timeline.clone(),
        })
        .batch_handler(SequencedBatchHandler {
            timeline: timeline.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    wait_until(|| handle.subscription_ids().len() == 1).await;
    publish_json(
        &broker,
        &ctx,
        SLOW_EVENT_TYPE,
        "slow-0",
        Some(0),
        serde_json::json!({ "slow": true }),
    )
    .await;

    wait_until(|| {
        let observed = timeline.lock().unwrap();
        let Some(first_handle) = observed.iter().position(|entry| *entry == "handle") else {
            return false;
        };
        observed
            .iter()
            .skip(first_handle + 1)
            .any(|entry| *entry == "load")
    })
    .await;
    handle.stop().await.expect("consumer stops");

    let observed = timeline.lock().unwrap().clone();
    let first_load = observed
        .iter()
        .position(|entry| *entry == "load")
        .expect("initial offset load");
    let first_handle = observed
        .iter()
        .position(|entry| *entry == "handle")
        .expect("drained handler call");
    let rejoin_load = observed
        .iter()
        .enumerate()
        .skip(first_handle + 1)
        .find_map(|(idx, entry)| (*entry == "load").then_some(idx))
        .expect("offset load after rejoin");
    assert!(first_load < first_handle && first_handle < rejoin_load);

    let affected = events
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| {
            if let ConsumerRuntimeEvent::SubscriptionConnectionDropped {
                reason: ConnectionDropReason::SlowConsumer { topic, .. },
                affected,
                ..
            } = event
            {
                (topic == SLOW_TOPIC).then(|| {
                    affected
                        .iter()
                        .map(|slot| (slot.topic.clone(), slot.partition))
                        .collect::<BTreeSet<_>>()
                })
            } else {
                None
            }
        })
        .expect("slow-consumer connection drop event");
    assert_eq!(
        affected,
        BTreeSet::from([(SLOW_TOPIC.to_owned(), 0), (SLOW_TOPIC.to_owned(), 1)])
    );
}

#[tokio::test]
async fn if_a_topic_is_noisy_i_can_isolate_it_in_a_separate_consumer_handle() {
    let (broker, _control, ctx) = broker_with_slow_and_fast_topics().await;
    let slow_listener = RecordingRuntimeListener::default();
    let slow_events = slow_listener.events.clone();
    let slow_timeline = Arc::new(Mutex::new(Vec::new()));
    let fast_subjects = Arc::new(Mutex::new(Vec::new()));

    let slow_handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-noisy-slow"))
        .topics([SLOW_TOPIC])
        .buffering(ConsumerBuffering {
            partition_capacity: 8,
            high_watermark: 1,
            low_watermark: 0,
        })
        .register_listener(slow_listener)
        .offset_manager(SequencedOffsetManager {
            timeline: slow_timeline.clone(),
        })
        .batch_handler(SequencedBatchHandler {
            timeline: slow_timeline,
        })
        .start()
        .await
        .expect("slow consumer starts");

    let fast_handle = ConsumerBuilder::new(broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-noisy-fast"))
        .topics([FAST_TOPIC])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(RecordingBatchHandler {
            subjects: fast_subjects.clone(),
        })
        .start()
        .await
        .expect("fast consumer starts");

    wait_until(|| {
        slow_handle.subscription_ids().len() == 1 && fast_handle.subscription_ids().len() == 1
    })
    .await;

    publish_json(
        &broker,
        &ctx,
        SLOW_EVENT_TYPE,
        "slow-noisy",
        Some(0),
        serde_json::json!({ "slow": true }),
    )
    .await;
    publish_json(
        &broker,
        &ctx,
        FAST_EVENT_TYPE,
        "fast-independent",
        Some(0),
        serde_json::json!({ "fast": true }),
    )
    .await;

    wait_until(|| fast_subjects.lock().unwrap().as_slice() == ["fast-independent"]).await;
    wait_until(|| {
        slow_events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                ConsumerRuntimeEvent::SubscriptionConnectionDropped {
                    reason: ConnectionDropReason::SlowConsumer { topic, .. },
                    ..
                } if topic == SLOW_TOPIC
            )
        })
    })
    .await;

    slow_handle.stop().await.expect("slow consumer stops");
    fast_handle.stop().await.expect("fast consumer stops");

    assert_eq!(
        fast_subjects.lock().unwrap().as_slice(),
        ["fast-independent"],
        "a noisy topic in its own handle must not stop the independent topic handle"
    );
}
