#![cfg(feature = "test-util")]

//! Executable usage showcase for the event-broker SDK.
//!
//! Each test is a small service story. The important setup stays visible in the
//! test body so this file can be read as wiring guidance, not only as coverage.
//!
//! | Area | Shows | Core APIs | Proof |
//! | --- | --- | --- | --- |
//! | Mock setup | Register topic and event type | `MockBroker`, `MockBrokerHandle` | Topic/type can be used by SDK code |
//! | Direct producer | Publish typed event | `Producer`, `ProducerIdentity`, `DirectDeduplication` | Event is stored by mock |
//! | Persisted/batch producer | Persist-confirming and batch send | `publish_persisted`, `publish_batch` | Outcomes and stored events match |
//! | Chained producer | Broker-issued producer id | `ProducerMode::Chained` | Broker cursor advances |
//! | Consumer | In-memory offset delivery | `ConsumerBuilder`, `InMemoryOffsetManager` | Handler records event data |
//! | Routing | Topic/type dispatch | consumer routes and default handler | Correct handler receives each event |
//! | End-to-end | Producer -> mock -> consumer | producer and consumer together | Handler sees the produced subject |
//! | Outbox producer | Validate and drain durable enqueue | `DbProducer`, toolkit-db outbox | Enqueued row reaches mock |
//! | Multi-topic outbox | One queue for multiple topics | `DbProducer` topics + outbox queue | Both topics are delivered |

use std::borrow::Cow;
#[cfg(all(feature = "db", feature = "outbox"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use event_broker_sdk::mock::MockBroker;
#[cfg(all(feature = "db", feature = "outbox"))]
use event_broker_sdk::mock::MockBrokerHandle;
use event_broker_sdk::{
    ConsumerBuilder, ConsumerError, ConsumerGroupRef, DirectDeduplication, EventBrokerApi,
    EventTypeRef, Fallback, HandlerOutcome, InMemoryOffsetManager, IngestOutcome, Producer,
    ProducerIdentity, ProducerMode, RawEvent, SingleEventHandler, SubscriptionInterest, TopicRef,
    TypedEvent,
};
#[cfg(all(feature = "db", feature = "outbox"))]
use event_broker_sdk::{DbDeduplication, DbProducer, EventBrokerError};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep};
use toolkit_security::SecurityContext;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const ORDERS_TOPIC: &str = "gts.cf.core.events.topic.v1~example.sdk.usage.orders.v1";
#[cfg(all(feature = "db", feature = "outbox"))]
const BILLING_TOPIC: &str = "gts.cf.core.events.topic.v1~example.sdk.usage.billing.v1";
const ORDER_CREATED: &str = "gts.cf.core.events.event.v1~example.sdk.usage.created.v1~";
const ORDER_UPDATED: &str = "gts.cf.core.events.event.v1~example.sdk.usage.updated.v1~";
const TENANT_PARTITION: u32 = 2;
#[cfg(all(feature = "db", feature = "outbox"))]
const BILLING_CHARGED: &str = "gts.cf.core.events.event.v1~example.sdk.usage.charged.v1~";
const ORDER_SUBJECT: &str = "gts.cf.core.events.subject.v1~example.sdk.usage.order.v1";
#[cfg(all(feature = "db", feature = "outbox"))]
const BILLING_SUBJECT: &str = "gts.cf.core.events.subject.v1~example.sdk.usage.charge.v1";
#[cfg(all(feature = "db", feature = "outbox"))]
const PRODUCER_QUEUE: &str = "event-broker-producer";
/// `description` of the base event's `data` member, which every event type's
/// composed payload contract carries as its first branch. Spelled out rather
/// than read back from the declaration so that changing the base surfaces as a
/// failure here.
const BASE_DATA_DESCRIPTION: &str = "Event payload, validated at ingest against the resolved schema of the event's type. The only field where UTF-8 (or any non-ASCII bytes) is permitted; all other event fields are ASCII per platform convention. May be absent for body-less events (e.g., notification-only events whose semantics are fully carried by `type` + `subject`).";

#[cfg(all(feature = "db", feature = "outbox"))]
static USAGE_DB_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderCreated {
    order_id: Uuid,
    total_cents: i64,
}

impl TypedEvent for OrderCreated {
    const TYPE_ID: &'static str = ORDER_CREATED;
    const SUBJECT_TYPE: &'static str = ORDER_SUBJECT;
    const SOURCE: &'static str = "order-service";

    fn subject(&self) -> Cow<'_, str> {
        Cow::Owned(self.order_id.to_string())
    }

    fn tenant_id(&self) -> Option<Uuid> {
        Some(test_tenant())
    }
}

#[cfg(all(feature = "db", feature = "outbox"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InvalidOrderCreated {
    order_id: Uuid,
    total_cents: String,
}

#[cfg(all(feature = "db", feature = "outbox"))]
impl TypedEvent for InvalidOrderCreated {
    const TYPE_ID: &'static str = ORDER_CREATED;
    const SUBJECT_TYPE: &'static str = ORDER_SUBJECT;
    const SOURCE: &'static str = "order-service";

    fn subject(&self) -> Cow<'_, str> {
        Cow::Owned(self.order_id.to_string())
    }

    fn tenant_id(&self) -> Option<Uuid> {
        Some(test_tenant())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderUpdated {
    order_id: Uuid,
    status: String,
}

impl TypedEvent for OrderUpdated {
    const TYPE_ID: &'static str = ORDER_UPDATED;
    const SUBJECT_TYPE: &'static str = ORDER_SUBJECT;
    const SOURCE: &'static str = "order-service";

    fn subject(&self) -> Cow<'_, str> {
        Cow::Owned(self.order_id.to_string())
    }

    fn tenant_id(&self) -> Option<Uuid> {
        Some(test_tenant())
    }
}

#[cfg(all(feature = "db", feature = "outbox"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BillingCharged {
    charge_id: Uuid,
    total_cents: i64,
}

#[cfg(all(feature = "db", feature = "outbox"))]
impl TypedEvent for BillingCharged {
    const TYPE_ID: &'static str = BILLING_CHARGED;
    const SUBJECT_TYPE: &'static str = BILLING_SUBJECT;
    const SOURCE: &'static str = "billing-service";

    fn subject(&self) -> Cow<'_, str> {
        Cow::Owned(self.charge_id.to_string())
    }

    fn tenant_id(&self) -> Option<Uuid> {
        Some(test_tenant())
    }
}

struct RecordingHandler {
    subjects: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SingleEventHandler for RecordingHandler {
    async fn handle(
        &self,
        event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        self.subjects.lock().unwrap().push(event.subject);
        Ok(HandlerOutcome::Success)
    }
}

struct NamedHandler {
    name: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl SingleEventHandler for NamedHandler {
    async fn handle(
        &self,
        _event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        self.calls.lock().unwrap().push(self.name);
        Ok(HandlerOutcome::Success)
    }
}

/// A service can set up the mock broker contract before wiring producers or consumers.
///
/// Preconditions: the mock starts empty.
/// Expected: the registered topic and event type are visible through the EventBrokerApi API.
#[tokio::test]
async fn mock_registers_order_topic_and_event_type() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();
    let ctx = SecurityContext::anonymous();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;

    let topics = broker.list_topics(&ctx).await?;
    let event_type = broker.get_event_type(&ctx, ORDER_CREATED).await?;

    // A topic reports the instance document's own values; an event type's topic
    // binding is a resolved trait, and `data_schema` is the payload contract
    // composed out of the base event's `data` member and this type's narrowing.
    // The public SDK models carry no serde, so the shape is asserted field by
    // field; the JSON wire form is pinned in the rest wire-DTO tests.
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].id.as_ref(), ORDERS_TOPIC);
    assert_eq!(topics[0].description, format!("Mock topic {ORDERS_TOPIC}"));
    assert_eq!(topics[0].retention, None);

    assert_eq!(event_type.id.as_ref(), ORDER_CREATED);
    assert_eq!(event_type.topic.as_ref(), ORDERS_TOPIC);
    assert_eq!(event_type.partition_key, "/tenant_id");
    assert_eq!(event_type.description, None);
    assert_eq!(
        event_type.allowed_subject_types,
        vec![ORDER_SUBJECT.to_owned()]
    );
    assert_eq!(
        event_type.data_schema,
        serde_json::json!({
            "allOf": [
                {
                    "additionalProperties": true,
                    "default": null,
                    "description": BASE_DATA_DESCRIPTION,
                    "type": ["object", "null"],
                },
                order_created_schema(),
            ],
        })
    );

    Ok(())
}

/// An order service can use the mock as its Event Broker and publish a typed event.
///
/// Preconditions: the orders topic and schema are registered before `prepare_all`.
/// Expected: the event is accepted and appears in mock storage.
#[tokio::test]
async fn producer_stateless_publish_sends_typed_event() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;

    let producer = Producer::builder()
        .broker(Arc::clone(&broker))
        .security_context(SecurityContext::anonymous())
        .identity(
            ProducerIdentity::new()
                .source("order-service")
                .client_agent("order-service/1.0"),
        )
        .deduplication(DirectDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns(["gts.cf.core.events.event.v1~example.sdk.usage.*"])
        .prepare_all()
        .await?;

    let outcome = producer.publish(order_created()).await?;

    assert_eq!(outcome, IngestOutcome::Accepted);
    assert_eq!(handle.stored(ORDERS_TOPIC, TENANT_PARTITION).await.len(), 1);

    Ok(())
}

/// A producer can wait for broker-side persistence when the caller needs it.
///
/// Preconditions: the producer uses the same direct setup as normal publishing.
/// Expected: `publish_persisted` returns `Persisted`.
#[tokio::test]
async fn producer_persisted_publish_waits_for_storage() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;

    let producer = Producer::builder()
        .broker(Arc::clone(&broker))
        .security_context(SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DirectDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns([ORDER_CREATED])
        .prepare_all()
        .await?;

    let outcome = producer.publish_persisted(order_created()).await?;

    assert_eq!(outcome, IngestOutcome::Persisted);
    assert_eq!(handle.stored(ORDERS_TOPIC, TENANT_PARTITION).await.len(), 1);

    Ok(())
}

/// A producer can publish multiple typed events in one batch call.
///
/// Preconditions: all events satisfy the prepared schema and configured topic.
/// Expected: each event is accepted and stored.
#[tokio::test]
async fn producer_batch_publish_sends_multiple_events() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;

    let producer = Producer::builder()
        .broker(Arc::clone(&broker))
        .security_context(SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DirectDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns([ORDER_CREATED])
        .prepare_all()
        .await?;

    let outcomes = producer
        .publish_batch(vec![order_created(), order_created()])
        .await?;

    assert_eq!(
        outcomes,
        vec![IngestOutcome::Accepted, IngestOutcome::Accepted]
    );
    assert_eq!(handle.stored(ORDERS_TOPIC, TENANT_PARTITION).await.len(), 2);

    Ok(())
}

/// Chained mode obtains a broker-issued producer id and advances broker cursor state.
///
/// Preconditions: the producer opts into broker registration at startup.
/// Expected: the producer id comes from Event Broker and publishing advances the cursor.
#[tokio::test]
async fn producer_chained_mode_uses_broker_issued_id_and_sequences_events() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();
    let ctx = SecurityContext::anonymous();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;

    let producer = Producer::builder()
        .broker(Arc::clone(&broker))
        .security_context(ctx.clone())
        .identity(
            ProducerIdentity::new()
                .source("order-service")
                .client_agent("order-service/1.0"),
        )
        .deduplication(DirectDeduplication::register_on_start(
            ProducerMode::Chained,
        ))
        .topics([ORDERS_TOPIC])
        .event_type_patterns([ORDER_CREATED])
        .prepare_all()
        .await?;

    let producer_id = producer.producer_id().expect("broker issued producer id");
    producer.publish(order_created()).await?;

    let cursors = broker.get_producer_cursors(&ctx, producer_id).await?;
    assert_eq!(
        cursors.last_sequence(ORDERS_TOPIC, TENANT_PARTITION),
        Some(0),
    );
    assert_eq!(handle.stored(ORDERS_TOPIC, TENANT_PARTITION).await.len(), 1);

    Ok(())
}

/// A consumer can read matching events from the mock with in-memory offsets.
///
/// Preconditions: the consumer subscribes to the orders topic and created event type.
/// Expected: the handler records the subject of the produced order.
#[tokio::test]
async fn consumer_with_in_memory_offsets_receives_event_from_mock() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;
    handle
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let subjects = Arc::new(Mutex::new(Vec::new()));
    let consumer = ConsumerBuilder::new(Arc::clone(&broker))
        .group(ConsumerGroupRef::auto_anonymous("usage-in-memory"))
        .subscription_interests([SubscriptionInterest::builder()
            .topic(TopicRef::gts(ORDERS_TOPIC))
            .types([EventTypeRef::gts(ORDER_CREATED)])
            .build()?])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(RecordingHandler {
            subjects: Arc::clone(&subjects),
        })
        .start()
        .await?;

    let producer = Producer::builder()
        .broker(Arc::clone(&broker))
        .security_context(SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DirectDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns([ORDER_CREATED])
        .prepare_all()
        .await?;

    let order = order_created();
    let subject = order.order_id.to_string();
    producer.publish(order).await?;

    wait_until(|| subjects.lock().unwrap().contains(&subject)).await;
    consumer.stop().await?;

    assert_eq!(subjects.lock().unwrap().as_slice(), [subject]);

    Ok(())
}

/// A consumer can route one event type to a specific handler and let another use default handling.
///
/// Preconditions: the consumer subscribes to the orders topic and registers a route for created events.
/// Expected: created and updated events reach different handlers.
#[tokio::test]
async fn consumer_routes_events_by_topic_and_type() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_UPDATED,
            order_updated_schema(),
            &[ORDER_SUBJECT],
        )
        .await;
    handle
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let calls = Arc::new(Mutex::new(Vec::new()));
    let consumer = ConsumerBuilder::new(Arc::clone(&broker))
        .group(ConsumerGroupRef::auto_anonymous("usage-routed"))
        .topics([ORDERS_TOPIC])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .default_handler(NamedHandler {
            name: "default",
            calls: Arc::clone(&calls),
        })
        .route()
        .topic(TopicRef::gts(ORDERS_TOPIC))
        .event_type(EventTypeRef::gts(ORDER_CREATED))
        .handler(NamedHandler {
            name: "created",
            calls: Arc::clone(&calls),
        })
        .start()
        .await?;

    let producer = Producer::builder()
        .broker(Arc::clone(&broker))
        .security_context(SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DirectDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns(["gts.cf.core.events.event.v1~example.sdk.usage.*"])
        .prepare_all()
        .await?;

    let order = order_created();
    producer.publish(order.clone()).await?;
    producer
        .publish(OrderUpdated {
            order_id: order.order_id,
            status: "paid".to_owned(),
        })
        .await?;

    wait_until(|| calls.lock().unwrap().len() == 2).await;
    consumer.stop().await?;

    assert_eq!(calls.lock().unwrap().as_slice(), ["created", "default"]);

    Ok(())
}

/// A produced event is visible to a consumer reading from the same mock broker.
///
/// Preconditions: producer and consumer share the same `Arc<dyn EventBrokerApi>`.
/// Expected: the consumer observes the exact subject produced by the order service.
#[tokio::test]
async fn producer_and_consumer_round_trip_order_created() -> TestResult {
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;
    handle
        .set_heartbeat_interval(Duration::from_millis(10))
        .await;

    let received = Arc::new(Mutex::new(Vec::new()));
    let consumer = ConsumerBuilder::new(Arc::clone(&broker))
        .group(ConsumerGroupRef::auto_anonymous("usage-round-trip"))
        .subscription_interests([SubscriptionInterest::builder()
            .topic(TopicRef::gts(ORDERS_TOPIC))
            .types([EventTypeRef::gts(ORDER_CREATED)])
            .build()?])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(RecordingHandler {
            subjects: Arc::clone(&received),
        })
        .start()
        .await?;

    let producer = Producer::builder()
        .broker(Arc::clone(&broker))
        .security_context(SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DirectDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns([ORDER_CREATED])
        .prepare_all()
        .await?;

    let order = order_created();
    let subject = order.order_id.to_string();
    producer.publish(order).await?;

    wait_until(|| received.lock().unwrap().contains(&subject)).await;
    consumer.stop().await?;

    assert_eq!(received.lock().unwrap().as_slice(), [subject]);

    Ok(())
}

/// A DB-aware producer rejects invalid payloads before a durable outbox row is written.
///
/// Preconditions: the producer prepared the registered JSON schema before enqueue.
/// Expected: enqueue fails locally and the mock broker stays empty.
#[cfg(all(feature = "db", feature = "outbox"))]
#[tokio::test]
async fn outbox_producer_validates_before_enqueue() -> TestResult {
    let db = sqlite_db_with_outbox_and_producer_migrations().await?;
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;

    let producer = DbProducer::builder()
        .broker(Arc::clone(&broker))
        .db(db.clone())
        .security_context(SecurityContext::anonymous())
        .identity(
            ProducerIdentity::new()
                .source("order-service")
                .client_agent("order-service/1.0"),
        )
        .deduplication(DbDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns([ORDER_CREATED])
        .prepare_all()
        .await?;

    let event_outbox =
        producer.outbox_queue(PRODUCER_QUEUE, toolkit_db::outbox::Partitions::of(4))?;
    let producer_handle = event_outbox
        .start(toolkit_db::outbox::Outbox::builder(db.clone()))
        .await?;
    let conn = db.conn()?;

    let err = producer_handle
        .outbox()
        .enqueue(&conn, invalid_order_created())
        .await
        .expect_err("invalid event must fail before enqueue");

    assert!(matches!(err, EventBrokerError::EventDataInvalid { .. }));
    assert!(
        handle
            .stored(ORDERS_TOPIC, TENANT_PARTITION)
            .await
            .is_empty()
    );
    producer_handle.stop().await;

    Ok(())
}

/// A running producer outbox processor drains a durable row into the mock broker.
///
/// Preconditions: toolkit-db owns the outbox lifecycle; the producer binding supplies the queue.
/// Expected: the enqueued order is eventually stored by the mock broker.
#[cfg(all(feature = "db", feature = "outbox"))]
#[tokio::test]
async fn outbox_producer_drains_queue_to_mock_broker() -> TestResult {
    let db = sqlite_db_with_outbox_and_producer_migrations().await?;
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;

    let producer = DbProducer::builder()
        .broker(Arc::clone(&broker))
        .db(db.clone())
        .security_context(SecurityContext::anonymous())
        .identity(
            ProducerIdentity::new()
                .source("order-service")
                .client_agent("order-service/1.0"),
        )
        .deduplication(DbDeduplication::stateless())
        .topics([ORDERS_TOPIC])
        .event_type_patterns([ORDER_CREATED])
        .prepare_all()
        .await?;

    let event_outbox =
        producer.outbox_queue(PRODUCER_QUEUE, toolkit_db::outbox::Partitions::of(4))?;
    let producer_handle = event_outbox
        .start(toolkit_db::outbox::Outbox::builder(db.clone()))
        .await?;
    let conn = db.conn()?;

    producer_handle
        .outbox()
        .enqueue(&conn, order_created())
        .await?;

    wait_for_stored(&handle, ORDERS_TOPIC, TENANT_PARTITION, 1).await;
    producer_handle.stop().await;

    Ok(())
}

/// One producer outbox queue can carry all topics configured on the `DbProducer`.
///
/// Preconditions: orders and billing topics are both configured on the producer.
/// Expected: both enqueued events are delivered under their own topics.
#[cfg(all(feature = "db", feature = "outbox"))]
#[tokio::test]
async fn single_outbox_queue_can_carry_multiple_topics() -> TestResult {
    let db = sqlite_db_with_outbox_and_producer_migrations().await?;
    let mock = Arc::new(MockBroker::new());
    let broker: Arc<dyn EventBrokerApi> = mock.clone();
    let handle = mock.handle();

    handle.register_topic(ORDERS_TOPIC, 4).await;
    handle.register_topic(BILLING_TOPIC, 4).await;
    handle
        .register_event_type(
            ORDERS_TOPIC,
            ORDER_CREATED,
            order_created_schema(),
            &[ORDER_SUBJECT],
        )
        .await;
    handle
        .register_event_type(
            BILLING_TOPIC,
            BILLING_CHARGED,
            billing_charged_schema(),
            &[BILLING_SUBJECT],
        )
        .await;

    let producer = DbProducer::builder()
        .broker(Arc::clone(&broker))
        .db(db.clone())
        .security_context(SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DbDeduplication::stateless())
        .topics([ORDERS_TOPIC, BILLING_TOPIC])
        .event_type_patterns(["gts.cf.core.events.event.v1~example.sdk.usage.*"])
        .prepare_all()
        .await?;

    let event_outbox =
        producer.outbox_queue(PRODUCER_QUEUE, toolkit_db::outbox::Partitions::of(4))?;
    let producer_handle = event_outbox
        .start(toolkit_db::outbox::Outbox::builder(db.clone()))
        .await?;
    let conn = db.conn()?;

    producer_handle
        .outbox()
        .enqueue(&conn, order_created())
        .await?;
    producer_handle
        .outbox()
        .enqueue(&conn, billing_charged())
        .await?;

    wait_for_stored(&handle, ORDERS_TOPIC, TENANT_PARTITION, 1).await;
    wait_for_stored(&handle, BILLING_TOPIC, TENANT_PARTITION, 1).await;
    producer_handle.stop().await;

    Ok(())
}

fn order_created_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["order_id", "total_cents"],
        "properties": {
            "order_id": { "type": "string" },
            "total_cents": { "type": "integer" }
        }
    })
}

fn order_updated_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["order_id", "status"],
        "properties": {
            "order_id": { "type": "string" },
            "status": { "type": "string" }
        }
    })
}

#[cfg(all(feature = "db", feature = "outbox"))]
fn billing_charged_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["charge_id", "total_cents"],
        "properties": {
            "charge_id": { "type": "string" },
            "total_cents": { "type": "integer" }
        }
    })
}

fn order_created() -> OrderCreated {
    OrderCreated {
        order_id: Uuid::new_v4(),
        total_cents: 1200,
    }
}

#[cfg(all(feature = "db", feature = "outbox"))]
fn invalid_order_created() -> InvalidOrderCreated {
    InvalidOrderCreated {
        order_id: Uuid::new_v4(),
        total_cents: "not-an-integer".to_owned(),
    }
}

#[cfg(all(feature = "db", feature = "outbox"))]
fn billing_charged() -> BillingCharged {
    BillingCharged {
        charge_id: Uuid::new_v4(),
        total_cents: 1200,
    }
}

fn test_tenant() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < deadline, "condition was not observed");
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(all(feature = "db", feature = "outbox"))]
async fn wait_for_stored(handle: &MockBrokerHandle, topic: &str, partition: u32, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if handle.stored(topic, partition).await.len() >= count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} stored events on {topic}:{partition}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(all(feature = "db", feature = "outbox"))]
async fn sqlite_db_with_outbox_and_producer_migrations() -> TestResult<toolkit_db::Db> {
    let seq = USAGE_DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_usage_showcase_{seq}?mode=memory&cache=shared");
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await?;
    let mut migrations = toolkit_db::outbox::outbox_migrations();
    migrations.extend(event_broker_sdk::producer_registration_migrations());
    toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations).await?;
    Ok(db)
}
