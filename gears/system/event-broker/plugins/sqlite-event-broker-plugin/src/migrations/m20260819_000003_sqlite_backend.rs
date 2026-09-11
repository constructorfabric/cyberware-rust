//! This backend's tables: `event_broker_event` (the log) and
//! `event_broker_partition_state` (next-sequence, outbox-retry dedup
//! bookkeeping, and the per-partition retention counters).
//!
//! One idempotent `CREATE TABLE IF NOT EXISTS`-shaped migration per table
//! family, never a raw connection. There is no versioned migration chain in
//! this gear family by design, so the counter columns are part of the base
//! table definition rather than a later `ALTER TABLE`: a database file created
//! before they existed is recreated rather than migrated.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `MigrationTrait` declares `manager: &SchemaManager` with a late-bound
// lifetime, and writing the anonymous lifetime the crate's `rust_2018_idioms`
// deny asks for makes the lifetime early-bound, which no longer matches the
// trait (E0195). The elision is forced by the trait, not a style choice.
#[allow(elided_lifetimes_in_paths)]
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r"
CREATE TABLE IF NOT EXISTS event_broker_event (
    id             TEXT PRIMARY KEY NOT NULL,
    type_id        TEXT NOT NULL,
    topic          TEXT NOT NULL,
    tenant_id      TEXT NOT NULL,
    source         TEXT NOT NULL,
    subject        TEXT NOT NULL,
    subject_type   TEXT NOT NULL,
    occurred_at    TEXT NOT NULL,
    trace_parent   TEXT,
    data           TEXT NOT NULL,
    partition      INTEGER NOT NULL,
    sequence       INTEGER NOT NULL,
    sequence_time  TEXT NOT NULL,
    stored_bytes   INTEGER NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS event_broker_event_topic_partition_sequence_idx
    ON event_broker_event (topic, partition, sequence);

CREATE INDEX IF NOT EXISTS event_broker_event_tenant_idx
    ON event_broker_event (tenant_id);

-- Retention removes an aged prefix of one partition, so it scans
-- (topic, partition) ordered by sequence_time. Without this index that scan is
-- a full table read at exactly the moment the table is largest.
CREATE INDEX IF NOT EXISTS event_broker_event_retention_idx
    ON event_broker_event (topic, partition, sequence_time);

-- A timestamp SEEK resolves to the first event in (topic, partition) whose
-- `occurred_at` is at or after the requested instant. Without this index that
-- lookup is a partition scan, which is exactly what a bounded seek must not be.
CREATE INDEX IF NOT EXISTS event_broker_event_occurred_at_idx
    ON event_broker_event (topic, partition, occurred_at);

CREATE TABLE IF NOT EXISTS event_broker_partition_state (
    topic                 TEXT NOT NULL,
    partition             INTEGER NOT NULL,
    next_sequence         INTEGER NOT NULL,
    last_chain_sequence   INTEGER,
    event_count           INTEGER NOT NULL DEFAULT 0,
    stored_bytes          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (topic, partition)
);
",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared("DROP TABLE IF EXISTS event_broker_partition_state;")
            .await?;
        conn.execute_unprepared("DROP TABLE IF EXISTS event_broker_event;")
            .await?;
        Ok(())
    }
}
