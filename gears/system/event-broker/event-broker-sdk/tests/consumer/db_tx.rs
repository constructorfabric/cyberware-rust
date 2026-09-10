use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use event_broker_sdk::consumer::LOCAL_DB_OFFSET_STORE_MIGRATION_SQL;
use event_broker_sdk::dlq::DeadLetterRecord;
use event_broker_sdk::{
    CommitOffsetInTx, ConsumerBatching, ConsumerBuilder, ConsumerError, ConsumerGroupRef,
    EventBatch, EventBrokerError, Fallback, HandlerOutcome, LocalDbOffsetManager, OffsetStore,
    RawEvent, ResolvedPosition, TxCommitHandle, TxConsumerHandler, TxSingleEventHandler,
};
use sea_orm::{ConnectionTrait, Database, EntityTrait, PaginatorTrait, Set, Statement};
use toolkit_db::secure::{AccessScope, SecureInsertExt};
use uuid::Uuid;

use super::common::{publish_json, topic_fixture, wait_until};

const TX_SINGLE_TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.txsingle.v1";
const TX_SINGLE_EVENT: &str = "gts.cf.core.events.event.v1~example.mock.showcase.txsingle.v1~";
const TX_SINGLE_GROUP: &str =
    "gts.cf.core.events.consumer_group.v1~example.mock.showcase.txsingle.v1";
const TX_BATCH_TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.txbatch.v1";
const TX_BATCH_EVENT: &str = "gts.cf.core.events.event.v1~example.mock.showcase.txbatch.v1~";
const TX_BATCH_GROUP: &str =
    "gts.cf.core.events.consumer_group.v1~example.mock.showcase.txbatch.v1";

mod dlq_row {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "showcase_dead_letters")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub event_id: Uuid,
        pub topic: String,
        pub event_type: String,
        pub partition: i32,
        pub offset: i64,
        pub reason: String,
        pub payload: String,
        pub occurred_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl toolkit_db::secure::ScopableEntity for Entity {
        const IS_UNRESTRICTED: bool = true;

        fn tenant_col() -> Option<Self::Column> {
            None
        }

        fn resource_col() -> Option<Self::Column> {
            None
        }

        fn owner_col() -> Option<Self::Column> {
            None
        }

        fn type_col() -> Option<Self::Column> {
            None
        }

        fn resolve_property(_property: &str) -> Option<Self::Column> {
            None
        }
        fn scope_columns() -> Vec<Self::Column> {
            Vec::new()
        }
    }
}

struct TxSingleProjector {
    db: toolkit_db::Db,
    committed_offsets: Arc<Mutex<Vec<i64>>>,
}

#[async_trait]
impl TxSingleEventHandler<LocalDbOffsetManager> for TxSingleProjector {
    async fn handle(
        &self,
        event: RawEvent,
        _attempts: u16,
        commit: TxCommitHandle<LocalDbOffsetManager>,
    ) -> Result<HandlerOutcome, ConsumerError> {
        let db = self.db.clone();
        let offset = event.offset;
        db.transaction_ref(move |tx| {
            Box::pin(async move {
                commit
                    .commit_offset_in_tx(tx, offset)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await
        .map_err(|err| EventBrokerError::Internal(err.to_string()))?;

        self.committed_offsets.lock().unwrap().push(offset);
        Ok(HandlerOutcome::Success)
    }
}

struct TxBatchProjector {
    db: toolkit_db::Db,
    committed_offsets: Arc<Mutex<Vec<i64>>>,
}

#[async_trait]
impl TxConsumerHandler<LocalDbOffsetManager> for TxBatchProjector {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
        commit: TxCommitHandle<LocalDbOffsetManager>,
    ) -> Result<HandlerOutcome, ConsumerError> {
        let target_offset = batch
            .iter()
            .last()
            .ok_or_else(|| EventBrokerError::Internal("empty transactional batch".to_owned()))?
            .offset;
        let db = self.db.clone();

        db.transaction_ref(move |tx| {
            Box::pin(async move {
                commit
                    .commit_offset_in_tx(tx, target_offset)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await
        .map_err(|err| EventBrokerError::Internal(err.to_string()))?;

        self.committed_offsets.lock().unwrap().push(target_offset);
        Ok(HandlerOutcome::Success)
    }
}

static DB_SEQ: AtomicU64 = AtomicU64::new(1);

async fn db_with_showcase_tables() -> (sea_orm::DatabaseConnection, toolkit_db::Db) {
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_showcase_db_tx_{seq}?mode=memory&cache=shared");
    let raw = Database::connect(&dsn).await.expect("raw sqlite connect");
    let backend = raw.get_database_backend();
    raw.execute_raw(Statement::from_string(
        backend,
        LOCAL_DB_OFFSET_STORE_MIGRATION_SQL.to_owned(),
    ))
    .await
    .expect("offset table");
    raw.execute_raw(Statement::from_string(
        backend,
        r#"
CREATE TABLE IF NOT EXISTS showcase_dead_letters (
    event_id UUID NOT NULL PRIMARY KEY,
    topic TEXT NOT NULL,
    event_type TEXT NOT NULL,
    partition INTEGER NOT NULL,
    offset BIGINT NOT NULL,
    reason TEXT NOT NULL,
    payload TEXT NOT NULL,
    occurred_at TIMESTAMP NOT NULL
);
"#
        .to_owned(),
    ))
    .await
    .expect("dead-letter table");
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await
    .expect("toolkit db");

    (raw, db)
}

fn raw_event_for_dead_letter() -> event_broker_sdk::RawEvent {
    event_broker_sdk::RawEvent {
        id: Uuid::new_v4(),
        type_id: "gts.cf.core.events.event.v1~example.showcase.db.dlq.v1~".to_owned(),
        topic: "gts.cf.core.events.topic.v1~example.showcase.db.dlq.v1".to_owned(),
        tenant_id: Uuid::nil(),
        subject: "db-dlq-1".to_owned(),
        subject_type: "test".to_owned(),
        partition: 0,
        sequence: 42,
        offset: 42,
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "reject": true }),
    }
}

async fn insert_dead_letter<TX>(
    tx: &TX,
    record: &DeadLetterRecord,
) -> Result<(), toolkit_db::DbError>
where
    TX: toolkit_db::secure::DBRunner + Sync,
{
    dlq_row::Entity::insert(dlq_row::ActiveModel {
        event_id: Set(record.event_id),
        topic: Set(record.topic.clone()),
        event_type: Set(record.event_type.clone()),
        partition: Set(record.partition as i32),
        offset: Set(record.offset),
        reason: Set(record.reason.clone()),
        payload: Set(record.payload.to_string()),
        occurred_at: Set(record.occurred_at),
    })
    .secure()
    .scope_unchecked(&AccessScope::allow_all())
    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?
    .exec(tx)
    .await
    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
    Ok(())
}

async fn dead_letter_count(conn: &sea_orm::DatabaseConnection) -> u64 {
    dlq_row::Entity::find()
        .count(conn)
        .await
        .expect("count dead-letter rows")
}

#[tokio::test]
async fn if_i_want_transactional_single_event_handling_i_commit_the_delivered_offset_in_my_tx() {
    let fixture = topic_fixture(TX_SINGLE_TOPIC, TX_SINGLE_EVENT, 1).await;
    let (_raw, db) = db_with_showcase_tables().await;
    fixture.control.register_named_group(TX_SINGLE_GROUP).await;
    let group = event_broker_sdk::ConsumerGroupId::from_gts(TX_SINGLE_GROUP);
    let topic = event_broker_sdk::TopicId::from_gts(TX_SINGLE_TOPIC);
    let committed_offsets = Arc::new(Mutex::new(Vec::new()));
    let assertion_manager = LocalDbOffsetManager::new(db.clone(), Fallback::Earliest);

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::existing(group))
        .topics([TX_SINGLE_TOPIC])
        .offset_manager(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest))
        .handler(TxSingleProjector {
            db: db.clone(),
            committed_offsets: committed_offsets.clone(),
        })
        .start()
        .await
        .expect("transactional single consumer starts");

    publish_json(
        &fixture.broker,
        &fixture.ctx,
        TX_SINGLE_EVENT,
        "tx-single-1",
        None,
        serde_json::json!({ "tx": "single" }),
    )
    .await;

    wait_until(|| committed_offsets.lock().unwrap().len() == 1).await;
    handle.stop().await.expect("consumer stops");

    let committed = committed_offsets.lock().unwrap()[0];
    assert_eq!(
        assertion_manager
            .load_position(&group, &topic, 0)
            .await
            .unwrap(),
        ResolvedPosition::Exact(committed)
    );
}

#[tokio::test]
async fn if_i_want_transactional_batch_handling_i_commit_the_last_handled_offset_in_my_tx() {
    let fixture = topic_fixture(TX_BATCH_TOPIC, TX_BATCH_EVENT, 1).await;
    let (_raw, db) = db_with_showcase_tables().await;
    fixture.control.register_named_group(TX_BATCH_GROUP).await;
    let group = event_broker_sdk::ConsumerGroupId::from_gts(TX_BATCH_GROUP);
    let topic = event_broker_sdk::TopicId::from_gts(TX_BATCH_TOPIC);
    let committed_offsets = Arc::new(Mutex::new(Vec::new()));
    let assertion_manager = LocalDbOffsetManager::new(db.clone(), Fallback::Earliest);

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::existing(group))
        .topics([TX_BATCH_TOPIC])
        .batching(ConsumerBatching {
            max_events: 8,
            max_wait: Duration::from_millis(20),
        })
        .offset_manager(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest))
        .batch_handler(TxBatchProjector {
            db: db.clone(),
            committed_offsets: committed_offsets.clone(),
        })
        .start()
        .await
        .expect("transactional batch consumer starts");

    for subject in ["tx-batch-1", "tx-batch-2"] {
        publish_json(
            &fixture.broker,
            &fixture.ctx,
            TX_BATCH_EVENT,
            subject,
            Some(0),
            serde_json::json!({ "tx": "batch", "subject": subject }),
        )
        .await;
    }

    wait_until(|| !committed_offsets.lock().unwrap().is_empty()).await;
    handle.stop().await.expect("consumer stops");

    let committed = *committed_offsets.lock().unwrap().last().unwrap();
    assert_eq!(
        assertion_manager
            .load_position(&group, &topic, 0)
            .await
            .unwrap(),
        ResolvedPosition::Exact(committed)
    );
}

#[tokio::test]
async fn if_i_want_db_transactional_progress_i_write_business_rows_and_offset_together() {
    let (_raw, db) = db_with_showcase_tables().await;
    let manager = Arc::new(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest));
    let group = event_broker_sdk::ConsumerGroupId::from_gts("showcase-db-tx");
    let topic = event_broker_sdk::TopicId::from_gts("showcase-db-tx-topic");
    let tx_manager = manager.clone();

    db.transaction_ref(move |tx| {
        Box::pin(async move {
            // Application business writes should use secure repositories with
            // this same `tx`; the offset save joins that transaction.
            tx_manager
                .commit_in_tx(tx, &group, &topic, 0, 41)
                .await
                .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
            Ok(())
        })
    })
    .await
    .expect("transaction commits");

    assert_eq!(
        manager.load_position(&group, &topic, 0).await.unwrap(),
        ResolvedPosition::Exact(41)
    );
}

#[tokio::test]
async fn if_the_business_transaction_rolls_back_the_offset_rolls_back_too() {
    let (_raw, db) = db_with_showcase_tables().await;
    let manager = Arc::new(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest));
    let group = event_broker_sdk::ConsumerGroupId::from_gts("showcase-db-tx-rollback");
    let topic = event_broker_sdk::TopicId::from_gts("showcase-db-tx-rollback-topic");
    let tx_manager = manager.clone();

    let result: Result<(), toolkit_db::DbError> = db
        .transaction_ref(move |tx| {
            Box::pin(async move {
                tx_manager
                    .commit_in_tx(tx, &group, &topic, 0, 77)
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Err(toolkit_db::DbError::InvalidConfig(
                    "force rollback".to_owned(),
                ))
            })
        })
        .await;

    assert!(result.is_err());
    assert_eq!(
        manager.load_position(&group, &topic, 0).await.unwrap(),
        ResolvedPosition::Earliest
    );
}

#[tokio::test]
async fn if_i_want_transactional_dlq_i_write_the_record_and_offset_in_one_transaction() {
    let (raw, db) = db_with_showcase_tables().await;
    let manager = Arc::new(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest));
    let group = event_broker_sdk::ConsumerGroupId::from_gts("showcase-db-dlq");
    let topic = event_broker_sdk::TopicId::from_gts("showcase-db-dlq-topic");
    let tx_manager = manager.clone();
    let event = raw_event_for_dead_letter();
    let record = DeadLetterRecord::builder(&event, "permanent validation failure")
        .group_id(group)
        .attempts(6)
        .build();

    db.transaction_ref(move |tx| {
        Box::pin(async move {
            insert_dead_letter(tx, &record).await?;
            tx_manager
                .commit_in_tx(tx, &group, &topic, event.partition, event.offset)
                .await
                .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
            Ok(())
        })
    })
    .await
    .expect("transaction commits");

    assert_eq!(dead_letter_count(&raw).await, 1);
    assert_eq!(
        manager
            .load_position(&group, &topic, event.partition)
            .await
            .unwrap(),
        ResolvedPosition::Exact(event.offset)
    );
}

#[tokio::test]
async fn if_the_transactional_dlq_rolls_back_neither_parking_nor_offset_is_durable() {
    let (raw, db) = db_with_showcase_tables().await;
    let manager = Arc::new(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest));
    let group = event_broker_sdk::ConsumerGroupId::from_gts("showcase-db-dlq-rollback");
    let topic = event_broker_sdk::TopicId::from_gts("showcase-db-dlq-rollback-topic");
    let tx_manager = manager.clone();
    let event = raw_event_for_dead_letter();
    let record = DeadLetterRecord::builder(&event, "permanent validation failure")
        .group_id(group)
        .attempts(6)
        .build();

    let result: Result<(), toolkit_db::DbError> = db
        .transaction_ref(move |tx| {
            Box::pin(async move {
                insert_dead_letter(tx, &record).await?;
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
    assert_eq!(dead_letter_count(&raw).await, 0);
    assert_eq!(
        manager
            .load_position(&group, &topic, event.partition)
            .await
            .unwrap(),
        ResolvedPosition::Earliest
    );
}
