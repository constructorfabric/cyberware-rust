use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_broker_sdk::{
    ConsumerBuilder, ConsumerError, ConsumerGroupRef, Fallback, HandlerOutcome,
    InMemoryOffsetManager, RawEvent, SingleEventHandler,
};

use super::common::{PublishJson, publish_json, topic_fixture, two_topic_fixture, wait_until};

const TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.single.v1";
const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~example.mock.showcase.single.v1~";

struct SingleEventProjector {
    offsets: Arc<Mutex<Vec<i64>>>,
    partitions: Arc<Mutex<Vec<u32>>>,
}

#[async_trait]
impl SingleEventHandler for SingleEventProjector {
    async fn handle(
        &self,
        event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        self.partitions.lock().unwrap().push(event.partition);
        self.offsets.lock().unwrap().push(event.offset.as_i64());
        Ok(HandlerOutcome::Success)
    }
}

#[tokio::test]
async fn if_i_want_a_simple_single_event_handler() {
    let fixture = topic_fixture(TOPIC, EVENT_TYPE, 1).await;
    let offsets = Arc::new(Mutex::new(Vec::new()));
    let partitions = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-single"))
        .topics([TOPIC])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(SingleEventProjector {
            offsets: offsets.clone(),
            partitions: partitions.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    publish_json(
        &fixture.broker,
        &fixture.ctx,
        EVENT_TYPE,
        "single-1",
        None,
        serde_json::json!({ "kind": "single" }),
    )
    .await;

    wait_until(|| offsets.lock().unwrap().len() == 1).await;
    handle.stop().await.expect("consumer stops");

    assert_eq!(offsets.lock().unwrap().len(), 1);
    assert_eq!(partitions.lock().unwrap()[0], 0);
}

/// The delivered event carries the partition the broker derived, not the value it
/// derived it from - the key is a property of the event type, so a consumer never
/// sees it.
#[tokio::test]
async fn if_i_publish_events_partitioned_by_a_payload_member_each_lands_on_its_partition() {
    let fixture = topic_fixture(TOPIC, EVENT_TYPE, 2).await;
    let offsets = Arc::new(Mutex::new(Vec::new()));
    let partitions = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::auto_anonymous(
            "showcase-single-partition-key",
        ))
        .topics([TOPIC])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(SingleEventProjector {
            offsets: offsets.clone(),
            partitions: partitions.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    for target in [0, 1] {
        super::common::publish_json_with_partition_key(PublishJson {
            broker: &fixture.broker,
            ctx: &fixture.ctx,
            event_type: EVENT_TYPE,
            subject: "single-1",
            partition_key: None,
            partition: Some(target),
            data: serde_json::json!({ "kind": "single" }),
        })
        .await;
    }

    wait_until(|| offsets.lock().unwrap().len() == 2).await;
    handle.stop().await.expect("consumer stops");

    let mut seen = partitions.lock().unwrap().clone();
    seen.sort_unstable();
    assert_eq!(seen, vec![0, 1]);
}

const ORDERS_TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.orders.v1";
const ORDERS_EVENT: &str = "gts.cf.core.events.event.v1~example.mock.showcase.orders.v1~";
const AUDIT_TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.audit.v1";
const AUDIT_EVENT: &str = "gts.cf.core.events.event.v1~example.mock.showcase.audit.v1~";

/// The `(topic, partition)` each delivered event was attributed to.
type Attributions = Arc<Mutex<Vec<(String, u32)>>>;

/// Records which `(topic, partition)` each delivered event was attributed to.
struct AttributionProjector {
    seen: Attributions,
}

#[async_trait]
impl SingleEventHandler for AttributionProjector {
    async fn handle(
        &self,
        event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        self.seen
            .lock()
            .unwrap()
            .push((event.topic.clone(), event.partition));
        Ok(HandlerOutcome::Success)
    }
}

/// An event carries no topic, so a consumer of two topics attributes each event by
/// resolving its type. Both topics use partition 0, which is exactly the case a
/// `partition`-only frame could not distinguish - transposing the two topics makes
/// this fail.
#[tokio::test]
async fn events_from_two_topics_are_each_attributed_to_their_own_topic() {
    let fixture =
        two_topic_fixture((ORDERS_TOPIC, ORDERS_EVENT), (AUDIT_TOPIC, AUDIT_EVENT), 1).await;
    let seen = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-two-topics"))
        .topics([ORDERS_TOPIC, AUDIT_TOPIC])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(AttributionProjector { seen: seen.clone() })
        .start()
        .await
        .expect("consumer starts");

    for (event_type, subject) in [(ORDERS_EVENT, "order-1"), (AUDIT_EVENT, "audit-1")] {
        publish_json(
            &fixture.broker,
            &fixture.ctx,
            event_type,
            subject,
            None,
            serde_json::json!({ "kind": "two-topic" }),
        )
        .await;
    }

    wait_until(|| seen.lock().unwrap().len() == 2).await;
    handle.stop().await.expect("consumer stops");

    let mut attributed = seen.lock().unwrap().clone();
    attributed.sort();
    assert_eq!(
        attributed,
        vec![(AUDIT_TOPIC.to_owned(), 0), (ORDERS_TOPIC.to_owned(), 0)]
    );
}
