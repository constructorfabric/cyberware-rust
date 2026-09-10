//! Migration `m0002`: the Quota tables of the quota-lifecycle feature (DESIGN
//! section 3.7, `qe_quotas`, `qe_quota_allocation_counters`, and the minimal
//! `qe_operation_log`).
//!
//! `qe_quotas` holds one row per Quota; `id` is a `UUIDv7`, so ascending id is
//! creation order and the list cursor. Caps live in `0..=i64::MAX` under a
//! check constraint; JSON columns (`notification_thresholds`, `metadata`) are
//! text the plugin serializes canonically. `qe_quota_allocation_counters` is
//! the in-flight counter of allocation Quotas, one row per allocation Quota,
//! created with it; consumption counters arrive with consumption-operations.
//! `qe_operation_log` records who did what to which Quota.
//!
//! `SQLite` has no `UUID`, `TIMESTAMPTZ`, or `BIGINT` types: `SeaORM` stores
//! `Uuid` as canonical `TEXT`, `OffsetDateTime` as ISO-8601 `TEXT`, and every
//! integer as `INTEGER`.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

const MYSQL_NOT_SUPPORTED: &str = "quota-enforcement-storage-plugin: MySQL is not supported; \
    this migration set targets PostgreSQL and SQLite";

/// Drop order: children before `qe_quotas`.
const TABLES: [&str; 3] = [
    "qe_quota_allocation_counters",
    "qe_operation_log",
    "qe_quotas",
];

const POSTGRES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS qe_quotas ( \
        id UUID PRIMARY KEY, \
        tenant_id UUID NOT NULL, \
        projection_type TEXT NOT NULL, \
        subject_id TEXT NOT NULL, \
        metric TEXT NOT NULL, \
        quota_type TEXT NOT NULL, \
        period TEXT NULL, \
        enforcement_mode TEXT NOT NULL, \
        cap BIGINT NULL CHECK (cap IS NULL OR cap >= 0), \
        notification_thresholds TEXT NOT NULL, \
        validity_start TIMESTAMPTZ NULL, \
        validity_end TIMESTAMPTZ NULL, \
        fail_open_hint BOOLEAN NOT NULL, \
        metadata TEXT NOT NULL, \
        source TEXT NOT NULL, \
        status TEXT NOT NULL CHECK (status IN ('active', 'deactivated')), \
        constraint_contract_type TEXT NOT NULL, \
        constraint_contract_version INTEGER NOT NULL, \
        record_version INTEGER NOT NULL CHECK (record_version >= 1), \
        created_at TIMESTAMPTZ NOT NULL, \
        updated_at TIMESTAMPTZ NOT NULL \
    );",
    "CREATE INDEX IF NOT EXISTS idx_qe_quotas_tenant_status_metric \
        ON qe_quotas (tenant_id, status, metric, id);",
    "CREATE INDEX IF NOT EXISTS idx_qe_quotas_tenant_subject \
        ON qe_quotas (tenant_id, projection_type, subject_id, id);",
    "CREATE INDEX IF NOT EXISTS idx_qe_quotas_status_metric_projection \
        ON qe_quotas (status, metric, projection_type, cap);",
    "CREATE TABLE IF NOT EXISTS qe_quota_allocation_counters ( \
        quota_id UUID PRIMARY KEY REFERENCES qe_quotas(id) ON DELETE RESTRICT, \
        tenant_id UUID NOT NULL, \
        in_flight BIGINT NOT NULL CHECK (in_flight >= 0), \
        record_version INTEGER NOT NULL CHECK (record_version >= 1), \
        updated_at TIMESTAMPTZ NOT NULL \
    );",
    "CREATE TABLE IF NOT EXISTS qe_operation_log ( \
        id UUID PRIMARY KEY, \
        tenant_id UUID NOT NULL, \
        quota_id UUID NULL, \
        operation TEXT NOT NULL, \
        actor_subject_id UUID NOT NULL, \
        actor_subject_type TEXT NULL, \
        record_version INTEGER NULL, \
        detail TEXT NOT NULL, \
        occurred_at TIMESTAMPTZ NOT NULL \
    );",
    "CREATE INDEX IF NOT EXISTS idx_qe_operation_log_occurred \
        ON qe_operation_log (occurred_at);",
    "CREATE INDEX IF NOT EXISTS idx_qe_operation_log_tenant_quota \
        ON qe_operation_log (tenant_id, quota_id, occurred_at);",
];

const SQLITE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS qe_quotas ( \
        id TEXT PRIMARY KEY NOT NULL, \
        tenant_id TEXT NOT NULL, \
        projection_type TEXT NOT NULL, \
        subject_id TEXT NOT NULL, \
        metric TEXT NOT NULL, \
        quota_type TEXT NOT NULL, \
        period TEXT NULL, \
        enforcement_mode TEXT NOT NULL, \
        cap INTEGER NULL CHECK (cap IS NULL OR cap >= 0), \
        notification_thresholds TEXT NOT NULL, \
        validity_start TEXT NULL, \
        validity_end TEXT NULL, \
        fail_open_hint BOOLEAN NOT NULL, \
        metadata TEXT NOT NULL, \
        source TEXT NOT NULL, \
        status TEXT NOT NULL CHECK (status IN ('active', 'deactivated')), \
        constraint_contract_type TEXT NOT NULL, \
        constraint_contract_version INTEGER NOT NULL, \
        record_version INTEGER NOT NULL CHECK (record_version >= 1), \
        created_at TEXT NOT NULL, \
        updated_at TEXT NOT NULL \
    );",
    "CREATE INDEX IF NOT EXISTS idx_qe_quotas_tenant_status_metric \
        ON qe_quotas (tenant_id, status, metric, id);",
    "CREATE INDEX IF NOT EXISTS idx_qe_quotas_tenant_subject \
        ON qe_quotas (tenant_id, projection_type, subject_id, id);",
    "CREATE INDEX IF NOT EXISTS idx_qe_quotas_status_metric_projection \
        ON qe_quotas (status, metric, projection_type, cap);",
    "CREATE TABLE IF NOT EXISTS qe_quota_allocation_counters ( \
        quota_id TEXT PRIMARY KEY NOT NULL REFERENCES qe_quotas(id) ON DELETE RESTRICT, \
        tenant_id TEXT NOT NULL, \
        in_flight INTEGER NOT NULL CHECK (in_flight >= 0), \
        record_version INTEGER NOT NULL CHECK (record_version >= 1), \
        updated_at TEXT NOT NULL \
    );",
    "CREATE TABLE IF NOT EXISTS qe_operation_log ( \
        id TEXT PRIMARY KEY NOT NULL, \
        tenant_id TEXT NOT NULL, \
        quota_id TEXT NULL, \
        operation TEXT NOT NULL, \
        actor_subject_id TEXT NOT NULL, \
        actor_subject_type TEXT NULL, \
        record_version INTEGER NULL, \
        detail TEXT NOT NULL, \
        occurred_at TEXT NOT NULL \
    );",
    "CREATE INDEX IF NOT EXISTS idx_qe_operation_log_occurred \
        ON qe_operation_log (occurred_at);",
    "CREATE INDEX IF NOT EXISTS idx_qe_operation_log_tenant_quota \
        ON qe_operation_log (tenant_id, quota_id, occurred_at);",
];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let statements = match manager.get_database_backend() {
            sea_orm::DatabaseBackend::Postgres => POSTGRES,
            sea_orm::DatabaseBackend::Sqlite => SQLITE,
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
