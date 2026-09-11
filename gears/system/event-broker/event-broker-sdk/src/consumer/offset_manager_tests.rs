use crate::sequence::Sequence;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use toolkit_gts::GTS_ID_PREFIX;

use sea_orm::{ConnectionTrait, Database, Statement};
use uuid::Uuid;

use super::{
    CommitOffsetInTx, Fallback, LOCAL_DB_OFFSET_STORE_MIGRATION_SQL, LocalDbOffsetManager,
    OffsetStore, Position,
};
use crate::ids::{ConsumerGroupId, TopicId};

static DB_SEQ: AtomicU64 = AtomicU64::new(1);

fn group(name: &str) -> ConsumerGroupId {
    ConsumerGroupId::from_gts(&format!("{GTS_ID_PREFIX}cf.test.consumer.group.v1~{name}"))
}

fn topic(name: &str) -> TopicId {
    TopicId::from_gts(&format!("{GTS_ID_PREFIX}cf.test.events.topic.v1~{name}"))
}

async fn db_with_offsets_table() -> (sea_orm::DatabaseConnection, toolkit_db::Db) {
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_offsets_{seq}?mode=memory&cache=shared");
    let raw = Database::connect(&dsn).await.expect("raw sqlite connect");
    raw.execute_raw(Statement::from_string(
        raw.get_database_backend(),
        LOCAL_DB_OFFSET_STORE_MIGRATION_SQL.to_owned(),
    ))
    .await
    .expect("create offset table");

    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await
    .expect("toolkit db connect");

    (raw, db)
}

#[tokio::test]
async fn local_db_load_position_returns_fallback_when_no_row_exists() {
    let (_raw, db) = db_with_offsets_table().await;
    let manager = LocalDbOffsetManager::new(db, Fallback::Latest);

    let pos = manager
        .load_position(&group("billing"), &topic("orders"), 0)
        .await
        .expect("load position");

    assert_eq!(pos, Position::Latest);
}

#[tokio::test]
async fn local_db_load_position_uses_override_before_fallback() {
    let (_raw, db) = db_with_offsets_table().await;
    let orders = topic("orders");
    let manager = LocalDbOffsetManager::new(db, Fallback::Earliest)
        .with_overrides([((orders, 2), Sequence::assigned(41))]);

    let pos = manager
        .load_position(&group("billing"), &orders, 2)
        .await
        .expect("load position");

    assert_eq!(pos, Position::Exact(Sequence::assigned(41)));
}

#[tokio::test]
async fn local_db_commit_in_tx_upserts_and_load_reads_committed_row() {
    let (_raw, db) = db_with_offsets_table().await;
    let manager = Arc::new(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest));
    let group = group("billing");
    let topic = topic("orders");
    let tx_manager = manager.clone();

    db.transaction_ref(move |tx| {
        Box::pin(async move {
            tx_manager
                .commit_in_tx(tx, &group, &topic, 3, Sequence::assigned(99))
                .await
                .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
            Ok(())
        })
    })
    .await
    .expect("commit transaction");

    let pos = manager
        .load_position(&group, &topic, 3)
        .await
        .expect("load committed position");

    assert_eq!(pos, Position::Exact(Sequence::assigned(99)));
}

#[tokio::test]
async fn local_db_commit_in_tx_rollback_does_not_advance_position() {
    let (_raw, db) = db_with_offsets_table().await;
    let manager = Arc::new(LocalDbOffsetManager::new(db.clone(), Fallback::Earliest));
    let group = group("billing");
    let topic = topic("payments");
    let tx_manager = manager.clone();

    let result: Result<(), toolkit_db::DbError> = db
        .transaction_ref(move |tx| {
            Box::pin(async move {
                tx_manager
                    .commit_in_tx(tx, &group, &topic, 0, Sequence::assigned(12))
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Err(toolkit_db::DbError::InvalidConfig(
                    "force rollback".to_owned(),
                ))
            })
        })
        .await;

    assert!(result.is_err());
    let pos = manager
        .load_position(&group, &topic, 0)
        .await
        .expect("load position after rollback");

    assert_eq!(pos, Position::Earliest);
}

#[tokio::test]
async fn local_db_uuid_key_isolates_groups_topics_and_partitions() {
    let (_raw, db) = db_with_offsets_table().await;
    let manager = Arc::new(LocalDbOffsetManager::new(db.clone(), Fallback::Latest));
    let group_a = group("a");
    let group_b = group("b");
    let topic_a = topic("orders");
    let topic_b = topic("orders-archive");
    let tx_manager = manager.clone();

    db.transaction_ref(move |tx| {
        Box::pin(async move {
            tx_manager
                .commit_in_tx(tx, &group_a, &topic_a, 0, Sequence::assigned(10))
                .await
                .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
            tx_manager
                .commit_in_tx(tx, &group_a, &topic_a, 1, Sequence::assigned(11))
                .await
                .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
            tx_manager
                .commit_in_tx(tx, &group_b, &topic_a, 0, Sequence::assigned(20))
                .await
                .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
            tx_manager
                .commit_in_tx(tx, &group_a, &topic_b, 0, Sequence::assigned(30))
                .await
                .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
            Ok(())
        })
    })
    .await
    .expect("commit offsets");

    assert_eq!(
        manager.load_position(&group_a, &topic_a, 0).await.unwrap(),
        Position::Exact(Sequence::assigned(10))
    );
    assert_eq!(
        manager.load_position(&group_a, &topic_a, 1).await.unwrap(),
        Position::Exact(Sequence::assigned(11))
    );
    assert_eq!(
        manager.load_position(&group_b, &topic_a, 0).await.unwrap(),
        Position::Exact(Sequence::assigned(20))
    );
    assert_eq!(
        manager.load_position(&group_a, &topic_b, 0).await.unwrap(),
        Position::Exact(Sequence::assigned(30))
    );
}

#[tokio::test]
async fn local_db_load_position_preserves_db_failure_source() {
    let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
    let dsn = format!("sqlite:file:evbk_offsets_missing_{seq}?mode=memory&cache=shared");
    let _raw = Database::connect(&dsn).await.expect("raw sqlite connect");
    let db = toolkit_db::connect_db(
        &dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(1),
            ..toolkit_db::ConnectOpts::default()
        },
    )
    .await
    .expect("toolkit db connect");

    let manager = LocalDbOffsetManager::new(db, Fallback::Earliest);
    let err = manager
        .load_position(&ConsumerGroupId::new(Uuid::new_v4()), &topic("missing"), 0)
        .await
        .expect_err("missing table should fail");

    assert!(err.source().is_some(), "DB error source must be preserved");
}
