use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use event_broker_sdk::ProducerId;
use event_broker_sdk::api::EventBrokerApi;
use event_broker_sdk::error::EventBrokerError;
use event_broker_sdk::mock::{MockBroker, MockBrokerHandle, PartitionKeyFixture};
use event_broker_sdk::models::{Event, ProducerMeta, ResetScope};
use event_broker_sdk::producer::IngestOutcome;
use event_broker_sdk::producer::UnknownProducerAction;
use event_broker_sdk::{
    DbDeduplication, DbProducer, MissingProducerRegistration, ProducerIdentity, ProducerMode,
    TypedEvent, UnknownProducerRegistration,
};

const QUEUE: &str = "event-broker-producer";
/// Partition count `fixture_with_handle` registers its topics with, and therefore
/// the count its producers declare.
const FIXTURE_BROKER_PARTITIONS: u32 = 4;

/// The payload member `OrderCreated` carries for the tests that need to steer the
/// partition. Pointing an event type here is the interesting case: a bare field
/// name could not reach inside `data`.
const PAYLOAD_POINTER: &str = "/data/partition_key";

const TOPIC: &str = "gts.cf.core.events.topic.v1~example.sdk.outbox.orders.v1";
const TOPIC2: &str = "gts.cf.core.events.topic.v1~example.sdk.outbox.billing.v1";
const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~example.sdk.outbox.created.v1~";
const EVENT_TYPE2: &str = "gts.cf.core.events.event.v1~example.sdk.outbox.charged.v1~";
const SUBJECT_TYPE: &str = "gts.cf.core.events.subject.v1~example.sdk.outbox.order.v1";
const TENANT_PARTITION: u32 = 2;

static DB_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderCreated {
    order_id: Uuid,
    total_cents: i64,
    /// A payload member an event type can point its partition key at.
    #[serde(skip_serializing_if = "Option::is_none")]
    partition_key: Option<String>,
}

impl TypedEvent for OrderCreated {
    const TYPE_ID: &'static str = EVENT_TYPE;
    const SUBJECT_TYPE: &'static str = SUBJECT_TYPE;
    const SOURCE: &'static str = "order-service";

    fn subject(&self) -> Cow<'_, str> {
        Cow::Owned(self.order_id.to_string())
    }

    fn tenant_id(&self) -> Option<Uuid> {
        Some(test_tenant())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BadOrderCreated {
    order_id: Uuid,
    total_cents: String,
}

impl TypedEvent for BadOrderCreated {
    const TYPE_ID: &'static str = EVENT_TYPE;
    const SUBJECT_TYPE: &'static str = SUBJECT_TYPE;
    const SOURCE: &'static str = "order-service";

    fn subject(&self) -> Cow<'_, str> {
        Cow::Owned(self.order_id.to_string())
    }

    fn tenant_id(&self) -> Option<Uuid> {
        Some(test_tenant())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BillingCharged {
    charge_id: Uuid,
    total_cents: i64,
}

impl TypedEvent for BillingCharged {
    const TYPE_ID: &'static str = EVENT_TYPE2;
    const SUBJECT_TYPE: &'static str = SUBJECT_TYPE;
    const SOURCE: &'static str = "billing-service";

    fn subject(&self) -> Cow<'_, str> {
        Cow::Owned(self.charge_id.to_string())
    }

    fn tenant_id(&self) -> Option<Uuid> {
        Some(test_tenant())
    }
}

struct NoopProcessor;

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for NoopProcessor {
    async fn handle(
        &self,
        _msg: &toolkit_db::outbox::OutboxMessage,
    ) -> toolkit_db::outbox::MessageResult {
        toolkit_db::outbox::MessageResult::Ok
    }
}

#[tokio::test]
async fn schema_validation_rejects_invalid_payload_before_enqueue() {
    let (db, broker) = fixture().await;
    let producer = stateless_producer(db.clone(), Arc::clone(&broker)).await;

    let err = producer
        .outbox_envelope(
            BadOrderCreated {
                order_id: Uuid::new_v4(),
                total_cents: "not-an-integer".to_owned(),
            },
            4,
        )
        .await
        .unwrap_err();

    assert!(matches!(err, EventBrokerError::EventDataInvalid { .. }));
}

#[tokio::test]
async fn lazy_validation_missing_schema_fails_without_broker_lookup() {
    let db = db().await;
    let broker = Arc::new(MockBroker::new());
    let producer = DbProducer::builder()
        .broker(broker)
        .db(db)
        .security_context(toolkit_security::SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DbDeduplication::stateless())
        .topics([TOPIC])
        .event_type_patterns(["gts.cf.core.events.event.v1~example.sdk.outbox.*"])
        .lazy_validation()
        .build()
        .await
        .unwrap();

    let err = producer.outbox_envelope(order(None), 4).await.unwrap_err();

    assert!(matches!(err, EventBrokerError::SchemaNotPrepared { .. }));
}

#[tokio::test]
async fn managed_missing_registration_can_fail_without_registering() {
    let (db, broker) = fixture().await;

    let result = DbProducer::builder()
        .broker(broker)
        .db(db)
        .security_context(toolkit_security::SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(
            DbDeduplication::managed(ProducerMode::Chained)
                .key("orders")
                .on_missing(MissingProducerRegistration::Fail),
        )
        .topics([TOPIC])
        .event_type_patterns([EVENT_TYPE])
        .prepare_all()
        .await;
    let err = match result {
        Ok(_) => panic!("missing managed registration must fail"),
        Err(err) => err,
    };

    assert!(matches!(
        err,
        EventBrokerError::InvalidProducerOptions { .. }
    ));
}

#[tokio::test]
async fn managed_client_agent_drift_fails_loudly() {
    let (db, broker) = fixture().await;
    let _first = managed_producer(db.clone(), Arc::clone(&broker)).await;

    let result = DbProducer::builder()
        .broker(broker)
        .db(db)
        .security_context(toolkit_security::SecurityContext::anonymous())
        .identity(
            ProducerIdentity::new()
                .source("order-service")
                .client_agent("order-service/2.0"),
        )
        .deduplication(
            DbDeduplication::managed(ProducerMode::Chained)
                .key("orders")
                .on_missing(MissingProducerRegistration::RegisterNew),
        )
        .topics([TOPIC])
        .event_type_patterns([EVENT_TYPE])
        .prepare_all()
        .await;
    let err = match result {
        Ok(_) => panic!("managed client-agent drift must fail"),
        Err(err) => err,
    };

    assert!(matches!(
        err,
        EventBrokerError::InvalidProducerOptions { .. }
    ));
}

#[tokio::test]
async fn managed_registration_requires_producer_migrations() {
    let (_, broker, _) = fixture_with_handle().await;
    let db = db_without_producer_migrations().await;

    let result = DbProducer::builder()
        .broker(broker)
        .db(db)
        .security_context(toolkit_security::SecurityContext::anonymous())
        .identity(
            ProducerIdentity::new()
                .source("order-service")
                .client_agent("order-service/1.0"),
        )
        .deduplication(
            DbDeduplication::managed(ProducerMode::Chained)
                .key("orders")
                .on_missing(MissingProducerRegistration::RegisterNew),
        )
        .topics([TOPIC])
        .event_type_patterns([EVENT_TYPE])
        .prepare_all()
        .await;
    let err = match result {
        Ok(_) => panic!("managed registration must require producer migrations"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("producer"),
        "unexpected migration error: {err}"
    );
}

#[tokio::test]
async fn unknown_producer_fail_policy_keeps_registration() {
    let (db, broker) = fixture().await;
    let producer =
        managed_producer_with_unknown(db, broker, UnknownProducerRegistration::Fail).await;
    let (_, before) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let before = envelope_json(&before);
    let producer_id = producer_id_from_envelope(&before);

    let action = producer.handle_unknown_producer(producer_id).await.unwrap();
    let (_, after) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let after = envelope_json(&after);

    assert_eq!(action, UnknownProducerAction::Fail);
    assert_eq!(before["producer_id"], after["producer_id"]);
    assert_eq!(before["generation"], after["generation"]);
}

#[tokio::test]
async fn unknown_producer_register_new_replaces_registration() {
    let (db, broker) = fixture().await;
    let producer =
        managed_producer_with_unknown(db, broker, UnknownProducerRegistration::RegisterNew).await;
    let (_, before) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let before = envelope_json(&before);
    let producer_id = producer_id_from_envelope(&before);

    let action = producer.handle_unknown_producer(producer_id).await.unwrap();
    let (_, after) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let after = envelope_json(&after);

    assert_eq!(action, UnknownProducerAction::Rotated);
    assert_ne!(before["producer_id"], after["producer_id"]);
    assert_eq!(before["generation"], 1);
    assert_eq!(after["generation"], 2);

    let action = producer.handle_unknown_producer(producer_id).await.unwrap();
    assert_eq!(action, UnknownProducerAction::AlreadyRotated);
}

/// The event type decides which member is hashed, so the same publish call yields
/// a different partition under a different pointer - and a producer cannot change
/// it per message.
#[tokio::test]
async fn the_event_types_declared_pointer_drives_the_broker_partition() {
    let (db, broker, handle) = fixture_with_handle().await;
    let by_tenant = stateless_producer(db.clone(), broker.clone()).await;

    let (_, tenant_envelope) = by_tenant.outbox_envelope(order(None), 4).await.unwrap();
    assert_eq!(
        envelope_json(&tenant_envelope)["broker_partition"],
        serde_json::json!(TENANT_PARTITION)
    );

    // Repointing must precede `prepare_all`, which caches the resolved type, so
    // the payload-pointed case needs its own producer.
    handle
        .set_partition_key(PartitionKeyFixture {
            event_type: EVENT_TYPE,
            pointer: PAYLOAD_POINTER,
        })
        .await;
    let by_payload = stateless_producer(db, broker).await;

    let (_, keyed_envelope) = by_payload
        .outbox_envelope(order(Some("explicit-key")), 4)
        .await
        .unwrap();
    assert_eq!(
        envelope_json(&keyed_envelope)["broker_partition"],
        serde_json::json!(3)
    );
}

#[tokio::test]
async fn same_topic_partition_maps_to_same_outbox_partition() {
    let (db, broker) = fixture().await;
    let producer = stateless_producer(db, broker).await;

    let (left, _) = producer.outbox_envelope(order(None), 8).await.unwrap();
    let (right, _) = producer.outbox_envelope(order(None), 8).await.unwrap();

    assert_eq!(left, right);
}

#[tokio::test]
async fn producer_outbox_and_broker_topic_partition_counts_can_differ() {
    let db = db().await;
    let mock = Arc::new(MockBroker::new());
    let handle = mock.handle();
    handle.register_topic(TOPIC, 16).await;
    handle.register_topic(TOPIC2, 4).await;
    handle
        .register_event_type(TOPIC, EVENT_TYPE, order_schema(), &[SUBJECT_TYPE])
        .await;
    handle
        .register_event_type(TOPIC2, EVENT_TYPE2, billing_schema(), &[SUBJECT_TYPE])
        .await;
    handle
        .set_partition_key(PartitionKeyFixture {
            event_type: EVENT_TYPE,
            pointer: PAYLOAD_POINTER,
        })
        .await;
    let broker: Arc<dyn EventBrokerApi> = mock;
    let producer = stateless_producer_with_broker_partitions(db, broker, 16).await;

    let event = order(Some("explicit-key"));
    let expected_broker_partition = 15;
    let expected_outbox_partition = 0;

    let (outbox_partition, envelope) = producer.outbox_envelope(event, 4).await.unwrap();
    let envelope = envelope_json(&envelope);

    assert_eq!(outbox_partition, expected_outbox_partition);
    assert_eq!(
        envelope["broker_partition"],
        serde_json::json!(expected_broker_partition)
    );
    assert!(outbox_partition < 4);
    assert!(expected_broker_partition < 16);
}

#[tokio::test]
async fn one_queue_can_carry_multiple_topics() {
    let (db, broker) = fixture().await;
    let producer = DbProducer::builder()
        .broker(broker)
        .db(db)
        .security_context(toolkit_security::SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DbDeduplication::stateless())
        .topics([TOPIC, TOPIC2])
        .event_type_patterns(["gts.cf.core.events.event.v1~example.sdk.outbox.*"])
        .prepare_all()
        .await
        .unwrap();

    let (orders_partition, orders) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let (billing_partition, billing) = producer
        .outbox_envelope(
            BillingCharged {
                charge_id: Uuid::new_v4(),
                total_cents: 30,
            },
            4,
        )
        .await
        .unwrap();

    assert!(orders_partition < 4);
    assert!(billing_partition < 4);
    assert_eq!(envelope_json(&orders)["topic"], TOPIC);
    assert_eq!(envelope_json(&billing)["topic"], TOPIC2);
}

#[tokio::test]
async fn managed_envelope_captures_registration_and_omits_final_chain_fields() {
    let (db, broker) = fixture().await;
    let producer = managed_producer(db, broker).await;

    let (_, envelope) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let json = envelope_json(&envelope);

    assert_eq!(json["version"], 1);
    assert_eq!(json["type"], EVENT_TYPE);
    assert_eq!(json["producer_mode"], "chained");
    assert!(json["producer_id"].as_str().is_some());
    assert_eq!(json["generation"], 1);
    assert!(json.get("previous").is_none());
    assert!(json.get("sequence").is_none());
    assert_eq!(
        json["diagnostic_metadata"]["sdk_client_agent"],
        "order-service/1.0"
    );

    let bytes = serde_json::to_vec(&envelope).unwrap();
    let round_tripped: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(round_tripped, json);
}

#[tokio::test]
async fn rotation_affects_future_envelopes_only() {
    let (db, broker) = fixture().await;
    let mut producer = managed_producer(db, broker).await;

    let (_, before) = producer.outbox_envelope(order(None), 4).await.unwrap();
    producer.rotate_registration().await.unwrap();
    let (_, after) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let before = envelope_json(&before);
    let after = envelope_json(&after);

    assert_ne!(before["producer_id"], after["producer_id"]);
    assert_eq!(before["generation"], 1);
    assert_eq!(after["generation"], 2);
}

#[tokio::test]
async fn reset_chain_preserves_registered_producer_id() {
    let (db, broker) = fixture().await;
    let producer = managed_producer(db, broker).await;

    let (_, before) = producer.outbox_envelope(order(None), 4).await.unwrap();
    producer.reset_chain(ResetScope::AllTopics).await.unwrap();
    let (_, after) = producer.outbox_envelope(order(None), 4).await.unwrap();

    assert_eq!(
        envelope_json(&before)["producer_id"],
        envelope_json(&after)["producer_id"]
    );
}

#[tokio::test]
async fn service_owned_lifecycle_registers_extra_queue_and_binds_producer_outbox() {
    let (db, broker, _) = fixture_with_handle().await;
    let producer = stateless_producer(db.clone(), broker).await;
    let event_outbox = producer
        .outbox_queue(QUEUE, toolkit_db::outbox::Partitions::of(4))
        .unwrap();
    let handle = event_outbox
        .register(toolkit_db::outbox::Outbox::builder(db.clone()))
        .queue("other-service-queue", toolkit_db::outbox::Partitions::of(2))
        .leased(NoopProcessor)
        .start()
        .await
        .unwrap();
    let producer_outbox = event_outbox.bind(&handle);
    let conn = db.conn().unwrap();

    let id = producer_outbox.enqueue(&conn, order(None)).await.unwrap();

    assert!(id.0 > 0);
    handle.stop().await;
}

#[tokio::test]
async fn convenience_start_drains_enqueued_event_to_broker() {
    let (db, broker, handle) = fixture_with_handle().await;
    let producer = stateless_producer(db.clone(), broker).await;
    let event_outbox = producer
        .outbox_queue(QUEUE, toolkit_db::outbox::Partitions::of(4))
        .unwrap();
    let producer_handle = event_outbox
        .start(toolkit_db::outbox::Outbox::builder(db.clone()))
        .await
        .unwrap();
    let conn = db.conn().unwrap();

    producer_handle
        .outbox()
        .enqueue(&conn, order(None))
        .await
        .unwrap();
    wait_for_stored(&handle, TOPIC, TENANT_PARTITION, 1).await;

    producer_handle.stop().await;
}

#[tokio::test]
async fn outbox_processor_treats_duplicate_as_ok() {
    let (db, broker, handle) = fixture_with_handle().await;
    let producer = managed_producer(db, Arc::clone(&broker)).await;
    let envelope = managed_envelope_payload(&producer).await;
    let json = envelope_json_from_bytes(&envelope);
    let producer_id = producer_id_from_envelope(&json);
    seed_chained_cursor(&broker, producer_id, 1).await;

    let result = producer.process_outbox_payload_for_test(envelope, 1).await;

    assert_message_ok(result);
    assert_eq!(handle.stored(TOPIC, TENANT_PARTITION).await.len(), 1);
}

#[tokio::test]
async fn outbox_processor_retries_transient_rate_limit() {
    let (db, broker, handle) = fixture_with_handle().await;
    let producer = stateless_producer(db, broker).await;
    let (_, envelope) = producer.outbox_envelope(order(None), 4).await.unwrap();
    handle.set_publish_rate_limit(Some(0)).await;

    let result = producer
        .process_outbox_payload_for_test(serde_json::to_vec(&envelope).unwrap(), 1)
        .await;

    assert_message_retry(result);
}

#[tokio::test]
async fn outbox_processor_rejects_malformed_payload() {
    let (db, broker) = fixture().await;
    let producer = stateless_producer(db, broker).await;

    let result = producer
        .process_outbox_payload_for_test(b"not-json".to_vec(), 1)
        .await;

    assert_message_reject(result, "decode producer envelope");
}

#[tokio::test]
async fn outbox_processor_recovers_chained_cursor_on_startup() {
    let (db, broker) = fixture().await;
    let producer = managed_producer(db, Arc::clone(&broker)).await;
    let envelope = managed_envelope_payload(&producer).await;
    let json = envelope_json_from_bytes(&envelope);
    let producer_id = producer_id_from_envelope(&json);
    seed_chained_cursor(&broker, producer_id, 2).await;

    let result = producer.process_outbox_payload_for_test(envelope, 4).await;
    let cursors = broker
        .get_producer_cursors(&toolkit_security::SecurityContext::anonymous(), producer_id)
        .await
        .unwrap();
    assert_message_ok(result);
    assert_eq!(cursors.last_sequence(TOPIC, TENANT_PARTITION), Some(4));
}

#[tokio::test]
async fn outbox_processor_recovers_chained_sequence_violation_by_refreshing_cursor() {
    let (db, broker) = fixture().await;
    let producer = managed_producer(db, Arc::clone(&broker)).await;
    let envelope = managed_envelope_payload(&producer).await;
    let json = envelope_json_from_bytes(&envelope);
    let producer_id = producer_id_from_envelope(&json);
    seed_chained_cursor(&broker, producer_id, 3).await;

    let result = producer
        .process_outbox_payload_with_cursor_for_test(envelope, 4, Some(-1))
        .await;
    let cursors = broker
        .get_producer_cursors(&toolkit_security::SecurityContext::anonymous(), producer_id)
        .await
        .unwrap();
    assert_message_ok(result);
    assert_eq!(cursors.last_sequence(TOPIC, TENANT_PARTITION), Some(4));
}

#[tokio::test]
async fn outbox_processor_rotates_future_registration_when_broker_forgot_producer() {
    let (db, broker, handle) = fixture_with_handle().await;
    let producer =
        managed_producer_with_unknown(db.clone(), broker, UnknownProducerRegistration::RegisterNew)
            .await;
    let (_, before) = producer.outbox_envelope(order(None), 4).await.unwrap();
    let before = envelope_json(&before);
    let producer_id = producer_id_from_envelope(&before);
    handle.forget_producer(producer_id).await;

    let event_outbox = producer
        .outbox_queue(QUEUE, toolkit_db::outbox::Partitions::of(4))
        .unwrap();
    let producer_handle = event_outbox
        .start(toolkit_db::outbox::Outbox::builder(db.clone()))
        .await
        .unwrap();
    let conn = db.conn().unwrap();

    producer_handle
        .outbox()
        .enqueue(&conn, order(None))
        .await
        .unwrap();
    let after =
        wait_for_rotated_registration(&producer, before["producer_id"].as_str().unwrap()).await;

    assert_eq!(after["generation"], 2);
    assert_ne!(before["producer_id"], after["producer_id"]);
    producer_handle.stop().await;
}

async fn fixture() -> (toolkit_db::Db, Arc<dyn EventBrokerApi>) {
    let (db, broker, _) = fixture_with_handle().await;
    (db, broker)
}

async fn fixture_with_handle() -> (toolkit_db::Db, Arc<dyn EventBrokerApi>, MockBrokerHandle) {
    let db = db().await;
    let mock = Arc::new(MockBroker::new());
    let handle = mock.handle();
    handle
        .register_topic(TOPIC, FIXTURE_BROKER_PARTITIONS)
        .await;
    handle
        .register_topic(TOPIC2, FIXTURE_BROKER_PARTITIONS)
        .await;
    handle
        .register_event_type(TOPIC, EVENT_TYPE, order_schema(), &[SUBJECT_TYPE])
        .await;
    handle
        .register_event_type(TOPIC2, EVENT_TYPE2, billing_schema(), &[SUBJECT_TYPE])
        .await;
    (db, mock, handle)
}

async fn db() -> toolkit_db::Db {
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_producer_outbox_{seq}?mode=memory&cache=shared");
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await
    .unwrap();
    let mut migrations = toolkit_db::outbox::outbox_migrations();
    migrations.extend(event_broker_sdk::producer_registration_migrations());
    toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations)
        .await
        .unwrap();
    db
}

async fn db_without_producer_migrations() -> toolkit_db::Db {
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_producer_outbox_no_reg_{seq}?mode=memory&cache=shared");
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await
    .unwrap();
    toolkit_db::migration_runner::run_migrations_for_testing(
        &db,
        toolkit_db::outbox::outbox_migrations(),
    )
    .await
    .unwrap();
    db
}

async fn stateless_producer(db: toolkit_db::Db, broker: Arc<dyn EventBrokerApi>) -> DbProducer {
    stateless_producer_with_broker_partitions(db, broker, FIXTURE_BROKER_PARTITIONS).await
}

/// A topic reports no partition count, so a producer declares the count its
/// broker is configured with. These fixtures register topics with a known count
/// and hand the producer the same one.
async fn stateless_producer_with_broker_partitions(
    db: toolkit_db::Db,
    broker: Arc<dyn EventBrokerApi>,
    broker_partitions: u32,
) -> DbProducer {
    DbProducer::builder()
        .broker(broker)
        .db(db)
        .security_context(toolkit_security::SecurityContext::anonymous())
        .identity(ProducerIdentity::new().source("order-service"))
        .deduplication(DbDeduplication::stateless())
        .topics([TOPIC])
        .event_type_patterns([EVENT_TYPE])
        .broker_partitions(broker_partitions)
        .prepare_all()
        .await
        .unwrap()
}

async fn managed_producer(db: toolkit_db::Db, broker: Arc<dyn EventBrokerApi>) -> DbProducer {
    managed_producer_with_unknown(db, broker, UnknownProducerRegistration::Fail).await
}

async fn managed_producer_with_unknown(
    db: toolkit_db::Db,
    broker: Arc<dyn EventBrokerApi>,
    unknown: UnknownProducerRegistration,
) -> DbProducer {
    DbProducer::builder()
        .broker(broker)
        .db(db)
        .security_context(toolkit_security::SecurityContext::anonymous())
        .identity(
            ProducerIdentity::new()
                .source("order-service")
                .client_agent("order-service/1.0"),
        )
        .deduplication(
            DbDeduplication::managed(ProducerMode::Chained)
                .key("orders")
                .on_missing(MissingProducerRegistration::RegisterNew)
                .on_unknown(unknown),
        )
        .topics([TOPIC])
        .event_type_patterns([EVENT_TYPE])
        .prepare_all()
        .await
        .unwrap()
}

fn order_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["order_id", "total_cents"],
        "properties": {
            "order_id": { "type": "string" },
            "total_cents": { "type": "integer" },
            "partition_key": { "type": "string" }
        }
    })
}

fn billing_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["charge_id", "total_cents"],
        "properties": {
            "charge_id": { "type": "string" },
            "total_cents": { "type": "integer" }
        }
    })
}

fn order(partition_key: Option<&str>) -> OrderCreated {
    OrderCreated {
        order_id: Uuid::new_v4(),
        total_cents: 10,
        partition_key: partition_key.map(str::to_owned),
    }
}

fn envelope_json(envelope: &impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(envelope).unwrap()
}

fn envelope_json_from_bytes(envelope: &[u8]) -> serde_json::Value {
    serde_json::from_slice(envelope).unwrap()
}

fn producer_id_from_envelope(envelope: &serde_json::Value) -> ProducerId {
    ProducerId(Uuid::parse_str(envelope["producer_id"].as_str().unwrap()).unwrap())
}

fn test_tenant() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
}

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

async fn wait_for_rotated_registration(
    producer: &DbProducer,
    old_producer_id: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let (_, envelope) = producer.outbox_envelope(order(None), 4).await.unwrap();
        let json = envelope_json(&envelope);
        if json["producer_id"].as_str() != Some(old_producer_id) {
            return json;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for producer registration rotation"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

async fn managed_envelope_payload(producer: &DbProducer) -> Vec<u8> {
    let (_, envelope) = producer.outbox_envelope(order(None), 4).await.unwrap();
    serde_json::to_vec(&envelope).unwrap()
}

async fn seed_chained_cursor(
    broker: &Arc<dyn EventBrokerApi>,
    producer_id: ProducerId,
    last_sequence: i64,
) {
    let ctx = toolkit_security::SecurityContext::anonymous();
    for sequence in 1..=last_sequence {
        let previous = sequence - 1;
        let previous = if previous == 0 { -1 } else { previous };
        let outcome = broker
            .publish(
                &ctx,
                &chained_event_for_sequence(producer_id, sequence, previous),
            )
            .await
            .unwrap();
        assert_eq!(outcome, IngestOutcome::Accepted);
    }
}

fn chained_event_for_sequence(producer_id: ProducerId, sequence: i64, previous: i64) -> Event {
    Event {
        id: Uuid::new_v4(),
        type_id: EVENT_TYPE.to_owned(),
        tenant_id: test_tenant(),
        source: "order-service".to_owned(),
        subject: Uuid::new_v4().to_string(),
        subject_type: SUBJECT_TYPE.to_owned(),
        occurred_at: chrono::Utc::now(),
        trace_parent: None,
        data: Some(serde_json::json!({
            "order_id": Uuid::new_v4(),
            "total_cents": 10
        })),
        partition: None,
        sequence: None,
        sequence_time: None,
        meta: Some(ProducerMeta {
            version: 1,
            producer_id: Some(producer_id.0),
            previous: Some(previous),
            sequence: Some(sequence),
            partition_hint: Some(TENANT_PARTITION),
        }),
    }
}

fn assert_message_ok(result: toolkit_db::outbox::MessageResult) {
    assert!(matches!(result, toolkit_db::outbox::MessageResult::Ok));
}

fn assert_message_retry(result: toolkit_db::outbox::MessageResult) {
    assert!(matches!(result, toolkit_db::outbox::MessageResult::Retry));
}

fn assert_message_reject(result: toolkit_db::outbox::MessageResult, expected: &str) {
    match result {
        toolkit_db::outbox::MessageResult::Reject(reason) => {
            assert!(
                reason.contains(expected),
                "expected reject reason to contain {expected:?}, got {reason:?}"
            );
        }
        other => panic!("expected reject, got {other:?}"),
    }
}
