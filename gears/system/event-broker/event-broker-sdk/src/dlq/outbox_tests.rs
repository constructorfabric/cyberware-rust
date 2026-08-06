use crate::sequence::Sequence;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use toolkit_gts::gts_id;

use chrono::Utc;
use uuid::Uuid;

use crate::consumer::RawEvent;
use crate::ids::ConsumerGroupId;

use super::{ConsumerDlqOutbox, DeadLetterEnvelope, DeadLetterRecord};

const DLQ_QUEUE: &str = "consumer-dlq";
const DLQ_PARTITIONS: u32 = 4;
const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1");
const EVENT_TYPE: &str = gts_id!("cf.core.events.event.v1~example.orders.rejected.x.v1~");

static DB_SEQ: AtomicU64 = AtomicU64::new(1);

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

fn raw_event(offset: i64) -> RawEvent {
    RawEvent {
        id: Uuid::new_v4(),
        type_id: EVENT_TYPE.to_owned(),
        topic: TOPIC.to_owned(),
        tenant_id: Uuid::new_v4(),
        subject: format!("order-{offset}"),
        subject_type: "order".to_owned(),
        partition: 6,
        sequence: Sequence::assigned(offset),
        offset: Sequence::assigned(offset),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "offset": offset }),
    }
}

fn dead_letter_record(offset: i64) -> DeadLetterRecord {
    DeadLetterRecord::builder(&raw_event(offset), "permanent failure")
        .group_id(ConsumerGroupId::from_gts(gts_id!(
            "cf.core.events.group.v1~example.orders.projector.x.v1"
        )))
        .attempts(3)
        .build()
}

async fn outbox_handle() -> (toolkit_db::outbox::OutboxHandle, toolkit_db::Db) {
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_dlq_outbox_{seq}?mode=memory&cache=shared");
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await
    .expect("connect toolkit db");
    toolkit_db::migration_runner::run_migrations_for_testing(
        &db,
        toolkit_db::outbox::outbox_migrations(),
    )
    .await
    .expect("outbox migrations");

    let handle = toolkit_db::outbox::Outbox::builder(db.clone())
        .queue(
            DLQ_QUEUE,
            toolkit_db::outbox::Partitions::of(DLQ_PARTITIONS as u16),
        )
        .leased(NoopProcessor)
        .start()
        .await
        .expect("outbox starts");
    (handle, db)
}

#[tokio::test]
async fn consumer_dlq_outbox_builder_keeps_queue_and_partitions() {
    let (handle, _db) = outbox_handle().await;
    let helper = ConsumerDlqOutbox::builder(Arc::clone(handle.outbox()))
        .queue(DLQ_QUEUE)
        .partitions(DLQ_PARTITIONS)
        .build();

    assert_eq!(helper.queue(), DLQ_QUEUE);
    assert_eq!(helper.partitions(), DLQ_PARTITIONS);
    handle.stop().await;
}

#[tokio::test]
async fn consumer_dlq_outbox_maps_consumed_event_partition_to_dlq_partition_count() {
    let (handle, _db) = outbox_handle().await;
    let helper = ConsumerDlqOutbox::builder(Arc::clone(handle.outbox()))
        .queue(DLQ_QUEUE)
        .partitions(DLQ_PARTITIONS)
        .build();
    let record = dead_letter_record(10);

    let actual = helper.partition_for_record(&record);

    assert_eq!(actual, record.partition % DLQ_PARTITIONS);
    assert!(actual < DLQ_PARTITIONS);
    handle.stop().await;
}

#[tokio::test]
async fn consumer_dlq_outbox_enqueues_dead_letter_envelope() {
    let (handle, db) = outbox_handle().await;
    let conn = db.conn().expect("db conn");
    let helper = ConsumerDlqOutbox::builder(Arc::clone(handle.outbox()))
        .queue(DLQ_QUEUE)
        .partitions(DLQ_PARTITIONS)
        .build();

    let message_id = helper
        .enqueue(&conn, dead_letter_record(11))
        .await
        .expect("enqueue succeeds");

    assert!(message_id.0 > 0);
    handle.stop().await;
}

#[tokio::test]
async fn consumer_dlq_outbox_maps_enqueue_failures_to_consumer_error() {
    let (handle, db) = outbox_handle().await;
    let conn = db.conn().expect("db conn");
    let helper = ConsumerDlqOutbox::builder(Arc::clone(handle.outbox()))
        .queue("missing-queue")
        .partitions(DLQ_PARTITIONS)
        .build();

    let err = helper
        .enqueue(&conn, dead_letter_record(12))
        .await
        .expect_err("missing queue should fail");

    assert!(err.to_string().contains("enqueue dead-letter envelope"));
    handle.stop().await;
}

#[test]
fn dead_letter_envelope_payload_type_is_used_by_processors() {
    assert_eq!(
        DeadLetterEnvelope::PAYLOAD_TYPE,
        "application/vnd.constructorfabric.event-broker.dlq+json"
    );
}
