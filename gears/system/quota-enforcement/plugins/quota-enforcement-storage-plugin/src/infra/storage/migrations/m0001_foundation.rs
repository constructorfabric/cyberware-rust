//! Migration `m0001`: schema metadata and the three configuration tables
//! (DESIGN section 3.7, "Bootstrap seeded state").
//!
//! Configuration rows use the sentinel key `*` for the platform default, so
//! every table has a real primary key and `NULL` never enters a key.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

const MYSQL_NOT_SUPPORTED: &str = "quota-enforcement-storage-plugin: MySQL is not supported; \
    this migration set targets PostgreSQL and SQLite";

const TABLES: [&str; 4] = [
    "qe_idempotency_retention_config",
    "qe_lease_capacity_config",
    "qe_contention_timeout_config",
    "qe_schema_meta",
];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let statements: &[&str] = match manager.get_database_backend() {
            sea_orm::DatabaseBackend::Postgres => &[
                "CREATE TABLE IF NOT EXISTS qe_schema_meta ( \
                    contract_major INTEGER PRIMARY KEY, \
                    applied_at TIMESTAMPTZ NOT NULL \
                );",
                "CREATE TABLE IF NOT EXISTS qe_contention_timeout_config ( \
                    metric_key TEXT PRIMARY KEY, \
                    timeout_ms BIGINT NOT NULL, \
                    updated_at TIMESTAMPTZ NOT NULL \
                );",
                "CREATE TABLE IF NOT EXISTS qe_lease_capacity_config ( \
                    tenant_key TEXT NOT NULL, \
                    metric_key TEXT NOT NULL, \
                    max_active_leases INTEGER NOT NULL, \
                    updated_at TIMESTAMPTZ NOT NULL, \
                    PRIMARY KEY (tenant_key, metric_key) \
                );",
                "CREATE TABLE IF NOT EXISTS qe_idempotency_retention_config ( \
                    tenant_key TEXT NOT NULL, \
                    metric_key TEXT NOT NULL, \
                    retention_seconds BIGINT NOT NULL, \
                    updated_at TIMESTAMPTZ NOT NULL, \
                    PRIMARY KEY (tenant_key, metric_key) \
                );",
            ],
            sea_orm::DatabaseBackend::Sqlite => &[
                // SQLite has no TIMESTAMPTZ type. SeaORM stores `OffsetDateTime`
                // as ISO-8601 TEXT.
                "CREATE TABLE IF NOT EXISTS qe_schema_meta ( \
                    contract_major INTEGER PRIMARY KEY, \
                    applied_at TEXT NOT NULL \
                );",
                "CREATE TABLE IF NOT EXISTS qe_contention_timeout_config ( \
                    metric_key TEXT PRIMARY KEY, \
                    timeout_ms INTEGER NOT NULL, \
                    updated_at TEXT NOT NULL \
                );",
                "CREATE TABLE IF NOT EXISTS qe_lease_capacity_config ( \
                    tenant_key TEXT NOT NULL, \
                    metric_key TEXT NOT NULL, \
                    max_active_leases INTEGER NOT NULL, \
                    updated_at TEXT NOT NULL, \
                    PRIMARY KEY (tenant_key, metric_key) \
                );",
                "CREATE TABLE IF NOT EXISTS qe_idempotency_retention_config ( \
                    tenant_key TEXT NOT NULL, \
                    metric_key TEXT NOT NULL, \
                    retention_seconds INTEGER NOT NULL, \
                    updated_at TEXT NOT NULL, \
                    PRIMARY KEY (tenant_key, metric_key) \
                );",
            ],
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        let conn = manager.get_connection();
        for sql in statements {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if matches!(
            manager.get_database_backend(),
            sea_orm::DatabaseBackend::MySql
        ) {
            return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned()));
        }
        let conn = manager.get_connection();
        for table in TABLES {
            conn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table};"))
                .await?;
        }
        Ok(())
    }
}
