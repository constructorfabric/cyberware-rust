//! `Storage`'s durable namespaces (eb-single-process-implementation D2):
//! `consumer_group`, `cursor`, `producer`, `producer_sequence`. SQLite only
//! (decision log entry 7); no other backend branch needed.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
CREATE TABLE IF NOT EXISTS event_broker_consumer_group (
    id                  TEXT PRIMARY KEY NOT NULL,
    kind                TEXT NOT NULL,
    tenant_id           TEXT NOT NULL,
    owner_principal_id  TEXT NOT NULL,
    description         TEXT,
    created_at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS event_broker_consumer_group_tenant_idx
    ON event_broker_consumer_group (tenant_id);

CREATE TABLE IF NOT EXISTS event_broker_cursor (
    consumer_group  TEXT NOT NULL,
    topic_id        INTEGER NOT NULL,
    partition       INTEGER NOT NULL,
    tenant_id       TEXT NOT NULL,
    "offset"        INTEGER NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (consumer_group, topic_id, partition)
);

CREATE INDEX IF NOT EXISTS event_broker_cursor_tenant_idx
    ON event_broker_cursor (tenant_id);

CREATE TABLE IF NOT EXISTS event_broker_producer (
    id             TEXT PRIMARY KEY NOT NULL,
    tenant_id      TEXT NOT NULL,
    owner_id       TEXT NOT NULL,
    mode           TEXT NOT NULL,
    client_agent   TEXT NOT NULL,
    created_at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS event_broker_producer_tenant_idx
    ON event_broker_producer (tenant_id);

CREATE TABLE IF NOT EXISTS event_broker_producer_sequence (
    producer_id    TEXT NOT NULL,
    topic          TEXT NOT NULL,
    partition      INTEGER NOT NULL,
    tenant_id      TEXT NOT NULL,
    last_sequence  INTEGER NOT NULL,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (producer_id, topic, partition)
);

CREATE INDEX IF NOT EXISTS event_broker_producer_sequence_tenant_idx
    ON event_broker_producer_sequence (tenant_id);
"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared("DROP TABLE IF EXISTS event_broker_producer_sequence;")
            .await?;
        conn.execute_unprepared("DROP TABLE IF EXISTS event_broker_producer;")
            .await?;
        conn.execute_unprepared("DROP TABLE IF EXISTS event_broker_cursor;")
            .await?;
        conn.execute_unprepared("DROP TABLE IF EXISTS event_broker_consumer_group;")
            .await?;
        Ok(())
    }
}
