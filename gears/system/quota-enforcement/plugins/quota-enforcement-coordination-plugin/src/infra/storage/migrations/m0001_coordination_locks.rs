//! Migration `m0001`: the `qe_coordination_locks` table.
//!
//! * `key` is the primary key. A new `LockScope` value needs no schema change.
//! * `locked_until` defaults to the Unix epoch so the steal filter
//!   `WHERE locked_until < NOW()` holds for a never-held row.
//! * `attempts` starts at `0`. The acquire path bumps it on every steal.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

const MYSQL_NOT_SUPPORTED: &str = "quota-enforcement-coordination-plugin: MySQL is not supported; \
    this migration set targets PostgreSQL and SQLite";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let sql = match manager.get_database_backend() {
            sea_orm::DatabaseBackend::Postgres => {
                "CREATE TABLE IF NOT EXISTS qe_coordination_locks ( \
                    key TEXT PRIMARY KEY, \
                    holder_id UUID NULL, \
                    locked_until TIMESTAMPTZ NOT NULL DEFAULT 'epoch', \
                    attempts INTEGER NOT NULL DEFAULT 0 \
                );"
            }
            sea_orm::DatabaseBackend::Sqlite => {
                // SQLite has no UUID or TIMESTAMPTZ types. SeaORM stores `Uuid`
                // as canonical TEXT and `OffsetDateTime` as ISO-8601 TEXT.
                "CREATE TABLE IF NOT EXISTS qe_coordination_locks ( \
                    key TEXT PRIMARY KEY, \
                    holder_id TEXT NULL, \
                    locked_until TEXT NOT NULL DEFAULT '1970-01-01 00:00:00+00:00', \
                    attempts INTEGER NOT NULL DEFAULT 0 \
                );"
            }
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        manager.get_connection().execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if matches!(
            manager.get_database_backend(),
            sea_orm::DatabaseBackend::MySql
        ) {
            return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned()));
        }
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS qe_coordination_locks;")
            .await?;
        Ok(())
    }
}
