//! Adds `types_registry__coordination_state` and seeds `entity_write_order`.
//!
//! This is separate from the already-deployed initial migration. Creation and seeding
//! are idempotent, preserving an existing sequence. SQL is lowered per backend.

use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS types_registry__coordination_state (
        state_name  varchar(64) NOT NULL,
        state_seq   bigint      NOT NULL,
        updated_at  timestamptz NOT NULL,

        CONSTRAINT pk_tr_coordination_state PRIMARY KEY (state_name),
        CONSTRAINT ck_tr_coordination_state_seq CHECK (state_seq >= 0)
    )",
    "INSERT INTO types_registry__coordination_state (state_name, state_seq, updated_at)
        VALUES ('entity_write_order', 0, now()) ON CONFLICT DO NOTHING",
];

const SQLITE_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS types_registry__coordination_state (
        state_name  TEXT    NOT NULL,
        state_seq   INTEGER NOT NULL,
        updated_at  TEXT    NOT NULL,

        CONSTRAINT pk_tr_coordination_state PRIMARY KEY (state_name),
        CONSTRAINT ck_tr_coordination_state_seq CHECK (state_seq >= 0)
    )",
    "INSERT OR IGNORE INTO types_registry__coordination_state (state_name, state_seq, updated_at)
        VALUES ('entity_write_order', 0, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
];

const MYSQL_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS types_registry__coordination_state (
        state_name  VARCHAR(64) NOT NULL,
        state_seq   BIGINT      NOT NULL,
        updated_at  DATETIME(6) NOT NULL,

        CONSTRAINT pk_tr_coordination_state PRIMARY KEY (state_name),
        CONSTRAINT ck_tr_coordination_state_seq CHECK (state_seq >= 0)
    )",
    "INSERT INTO types_registry__coordination_state (state_name, state_seq, updated_at)
        VALUES ('entity_write_order', 0, UTC_TIMESTAMP(6))
        ON DUPLICATE KEY UPDATE state_seq = state_seq, updated_at = updated_at",
];

const DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS types_registry__coordination_state"];

/// The statement list for `backend`, or a refusal naming it.
fn up_statements(backend: sea_orm::DatabaseBackend) -> Result<&'static [&'static str], DbErr> {
    match backend {
        sea_orm::DatabaseBackend::Postgres => Ok(PG_UP_STATEMENTS),
        sea_orm::DatabaseBackend::Sqlite => Ok(SQLITE_UP_STATEMENTS),
        sea_orm::DatabaseBackend::MySql => Ok(MYSQL_UP_STATEMENTS),
        other => Err(DbErr::Migration(format!(
            "types-registry migrations support Postgres, SQLite and MySQL only; \
             got unsupported database backend {other:?}"
        ))),
    }
}

#[cfg(test)]
#[path = "m20260904_000002_coordination_state_tests.rs"]
mod coordination_state_tests;

#[allow(elided_lifetimes_in_paths)]
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        for sql in up_statements(backend)? {
            conn.execute_raw(Statement::from_string(backend, (*sql).to_owned()))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        _ = up_statements(backend)?;
        let conn = manager.get_connection();
        for sql in DOWN_STATEMENTS {
            conn.execute_raw(Statement::from_string(backend, (*sql).to_owned()))
                .await?;
        }
        Ok(())
    }
}
