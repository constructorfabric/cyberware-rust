use event_broker_sdk::Sequence;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use event_broker_sdk::consumer::LOCAL_DB_OFFSET_STORE_MIGRATION_SQL;
use event_broker_sdk::dlq::{ConsumerDlqOutbox, DeadLetterEnvelope, DeadLetterRecord};
use event_broker_sdk::{
    CommitOffsetInTx, ConsumerGroupId, Fallback, LocalDbOffsetManager, OffsetStore, Position,
    RawEvent, TopicId,
};
use sea_orm::{ConnectionTrait, Database, Statement};
use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, Outbox, OutboxHandle, OutboxMessage, Partitions,
};
use uuid::Uuid;

use crate::consumer::common::wait_until;

const DLQ_QUEUE: &str = "showcase-consumer-dlq";
const DLQ_PARTITIONS: u32 = 4;
const TOPIC_GTS: &str = "gts.cf.core.events.topic.v1~example.showcase.outbox.dlq.v1";
const EVENT_TYPE_GTS: &str = "gts.cf.core.events.event.v1~example.showcase.outbox.dlq.v1~";
const GROUP_GTS: &str = "gts.cf.core.events.consumer_group.v1~example.showcase.outbox.dlq.v1";

static DB_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
struct RecordingDlqProcessor {
    envelopes: Arc<Mutex<Vec<DeadLetterEnvelope>>>,
}

#[async_trait]
impl LeasedMessageHandler for RecordingDlqProcessor {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        match DeadLetterEnvelope::from_slice(&msg.payload) {
            Ok(envelope) => {
                self.envelopes.lock().unwrap().push(envelope);
                MessageResult::Ok
            }
            Err(err) => MessageResult::Reject(err.to_string()),
        }
    }
}

struct DlqOutboxFixture {
    raw: sea_orm::DatabaseConnection,
    db: toolkit_db::Db,
    handle: OutboxHandle,
    envelopes: Arc<Mutex<Vec<DeadLetterEnvelope>>>,
}

impl DlqOutboxFixture {
    fn helper(&self) -> ConsumerDlqOutbox {
        ConsumerDlqOutbox::builder(Arc::clone(self.handle.outbox()))
            .queue(DLQ_QUEUE)
            .partitions(DLQ_PARTITIONS)
            .build()
    }

    async fn stop(self) {
        self.handle.stop().await;
        drop(self.raw);
        drop(self.db);
    }
}

async fn fixture() -> DlqOutboxFixture {
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_showcase_outbox_dlq_{seq}?mode=memory&cache=shared");
    let raw = Database::connect(&dsn).await.expect("raw sqlite connect");
    let backend = raw.get_database_backend();
    raw.execute_raw(Statement::from_string(
        backend,
        LOCAL_DB_OFFSET_STORE_MIGRATION_SQL.to_owned(),
    ))
    .await
    .expect("offset table");

    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await
    .expect("toolkit db");
    toolkit_db::migration_runner::run_migrations_for_testing(
        &db,
        toolkit_db::outbox::outbox_migrations(),
    )
    .await
    .expect("outbox migrations");

    let processor = RecordingDlqProcessor::default();
    let envelopes = processor.envelopes.clone();
    let handle = Outbox::builder(db.clone())
        .queue(DLQ_QUEUE, Partitions::of(DLQ_PARTITIONS as u16))
        .leased(processor)
        .start()
        .await
        .expect("outbox starts");

    DlqOutboxFixture {
        raw,
        db,
        handle,
        envelopes,
    }
}

fn rejected_event(offset: i64) -> RawEvent {
    RawEvent {
        id: Uuid::new_v4(),
        type_id: EVENT_TYPE_GTS.to_owned(),
        topic: TOPIC_GTS.to_owned(),
        tenant_id: Uuid::nil(),
        subject: format!("dlq-order-{offset}"),
        subject_type: "order".to_owned(),
        partition: 0,
        sequence: Sequence::assigned(offset),
        offset: Sequence::assigned(offset),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "reject": true, "offset": offset }),
    }
}

fn dead_letter(event: &RawEvent, group: ConsumerGroupId) -> DeadLetterRecord {
    DeadLetterRecord::builder(event, "permanent validation failure")
        .group_id(group)
        .topic_id(TopicId::from_gts(&event.topic))
        .attempts(6)
        .build()
}

fn coordinates() -> (ConsumerGroupId, TopicId) {
    (
        ConsumerGroupId::from_gts(GROUP_GTS),
        TopicId::from_gts(TOPIC_GTS),
    )
}

async fn load_position(
    manager: &LocalDbOffsetManager,
    group: &ConsumerGroupId,
    topic: &TopicId,
    partition: u32,
) -> Position {
    manager
        .load_position(group, topic, partition)
        .await
        .expect("load offset")
}

async fn wait_for_envelope(envelopes: &Arc<Mutex<Vec<DeadLetterEnvelope>>>) -> DeadLetterEnvelope {
    wait_until(|| !envelopes.lock().unwrap().is_empty()).await;
    envelopes.lock().unwrap()[0].clone()
}

#[tokio::test]
async fn if_i_want_a_single_queue_dlq_i_enqueue_to_the_consumer_dlq_outbox() {
    let fixture = fixture().await;
    let helper = fixture.helper();
    let (group, topic) = coordinates();
    let event = rejected_event(10);
    let record = dead_letter(&event, group);
    let conn = fixture.db.conn().expect("db conn");

    helper
        .enqueue(&conn, record)
        .await
        .expect("dead-letter record enqueued");
    fixture.handle.outbox().flush();

    let envelope = wait_for_envelope(&fixture.envelopes).await;
    assert_eq!(envelope.group_id, Some(group));
    assert_eq!(envelope.topic_id, Some(topic));
    assert_eq!(envelope.topic, TOPIC_GTS);
    assert_eq!(envelope.subject, event.subject);
    assert_eq!(envelope.subject_type, event.subject_type);
    assert_eq!(envelope.payload, event.data);
    assert_eq!(envelope.reason, "permanent validation failure");

    fixture.stop().await;
}

#[tokio::test]
async fn if_i_want_transactional_dlq_i_enqueue_and_commit_offset_in_the_same_tx() {
    let fixture = fixture().await;
    let helper = fixture.helper();
    let manager = Arc::new(LocalDbOffsetManager::new(
        fixture.db.clone(),
        Fallback::Earliest,
    ));
    let (group, topic) = coordinates();
    let event = rejected_event(20);
    let record = dead_letter(&event, group);
    let tx_manager = Arc::clone(&manager);

    fixture
        .db
        .transaction_ref(|tx| {
            Box::pin(async move {
                helper
                    .enqueue(tx, record)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                tx_manager
                    .commit_in_tx(tx, &group, &topic, event.partition, event.offset)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await
        .expect("transaction commits");
    fixture.handle.outbox().flush();

    let envelope = wait_for_envelope(&fixture.envelopes).await;
    assert_eq!(envelope.offset, event.offset);
    assert_eq!(
        load_position(&manager, &group, &topic, event.partition).await,
        Position::Exact(event.offset)
    );

    fixture.stop().await;
}

#[tokio::test]
async fn if_the_dlq_transaction_rolls_back_neither_handoff_nor_offset_is_durable() {
    let fixture = fixture().await;
    let helper = fixture.helper();
    let manager = Arc::new(LocalDbOffsetManager::new(
        fixture.db.clone(),
        Fallback::Earliest,
    ));
    let (group, topic) = coordinates();
    let event = rejected_event(30);
    let record = dead_letter(&event, group);
    let tx_manager = Arc::clone(&manager);

    let result: Result<(), toolkit_db::DbError> = fixture
        .db
        .transaction_ref(|tx| {
            Box::pin(async move {
                helper
                    .enqueue(tx, record)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                tx_manager
                    .commit_in_tx(tx, &group, &topic, event.partition, event.offset)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Err(toolkit_db::DbError::InvalidConfig(
                    "force rollback".to_owned(),
                ))
            })
        })
        .await;

    assert!(result.is_err());
    fixture.handle.outbox().flush();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fixture.envelopes.lock().unwrap().is_empty());
    assert_eq!(
        load_position(&manager, &group, &topic, event.partition).await,
        Position::Earliest
    );

    fixture.stop().await;
}

#[tokio::test]
async fn if_the_main_transaction_rolled_back_i_open_a_new_tx_for_dlq_and_offset_skip() {
    let fixture = fixture().await;
    let helper = fixture.helper();
    let manager = Arc::new(LocalDbOffsetManager::new(
        fixture.db.clone(),
        Fallback::Earliest,
    ));
    let (group, topic) = coordinates();
    let event = rejected_event(40);
    let record = dead_letter(&event, group);

    let business_result: Result<(), toolkit_db::DbError> = fixture
        .db
        .transaction_ref(|_tx| {
            Box::pin(async move {
                Err(toolkit_db::DbError::InvalidConfig(
                    "business rollback".to_owned(),
                ))
            })
        })
        .await;
    assert!(business_result.is_err());

    let tx_manager = Arc::clone(&manager);
    fixture
        .db
        .transaction_ref(|tx| {
            Box::pin(async move {
                helper
                    .enqueue(tx, record)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                tx_manager
                    .commit_in_tx(tx, &group, &topic, event.partition, event.offset)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await
        .expect("dlq transaction commits");
    fixture.handle.outbox().flush();

    let envelope = wait_for_envelope(&fixture.envelopes).await;
    assert_eq!(envelope.offset, event.offset);
    assert_eq!(
        load_position(&manager, &group, &topic, event.partition).await,
        Position::Exact(event.offset)
    );

    fixture.stop().await;
}

#[tokio::test]
async fn if_dlq_handoff_fails_i_do_not_commit_the_source_offset() {
    let fixture = fixture().await;
    let broken_helper = ConsumerDlqOutbox::builder(Arc::clone(fixture.handle.outbox()))
        .queue("missing-dlq-queue")
        .partitions(DLQ_PARTITIONS)
        .build();
    let manager = Arc::new(LocalDbOffsetManager::new(
        fixture.db.clone(),
        Fallback::Earliest,
    ));
    let (group, topic) = coordinates();
    let event = rejected_event(50);
    let record = dead_letter(&event, group);
    let tx_manager = Arc::clone(&manager);

    let result: Result<(), toolkit_db::DbError> = fixture
        .db
        .transaction_ref(|tx| {
            Box::pin(async move {
                broken_helper
                    .enqueue(tx, record)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                tx_manager
                    .commit_in_tx(tx, &group, &topic, event.partition, event.offset)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await;

    assert!(result.is_err());
    assert!(fixture.envelopes.lock().unwrap().is_empty());
    assert_eq!(
        load_position(&manager, &group, &topic, event.partition).await,
        Position::Earliest
    );

    fixture.stop().await;
}
