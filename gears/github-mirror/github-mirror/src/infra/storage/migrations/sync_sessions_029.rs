use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        let sql = match backend {
            sea_orm::DatabaseBackend::Postgres => {
                r"
CREATE TABLE IF NOT EXISTS gm_sync_sessions (
    tenant_id UUID NOT NULL,
    id UUID NOT NULL,
    repo_full_name VARCHAR(512) NOT NULL,
    repo_id BIGINT,
    status VARCHAR(32) NOT NULL,
    progress_percent INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    summary_json TEXT,
    created_at VARCHAR(64) NOT NULL,
    started_at VARCHAR(64),
    ended_at VARCHAR(64),
    PRIMARY KEY (tenant_id, id)
);
CREATE INDEX IF NOT EXISTS idx_gm_sync_sessions_tenant_created
    ON gm_sync_sessions (tenant_id, created_at);
                "
            }
            sea_orm::DatabaseBackend::MySql => {
                r"
CREATE TABLE IF NOT EXISTS gm_sync_sessions (
    tenant_id VARCHAR(36) NOT NULL,
    id VARCHAR(36) NOT NULL,
    repo_full_name VARCHAR(512) NOT NULL,
    repo_id BIGINT,
    status VARCHAR(32) NOT NULL,
    progress_percent INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    summary_json TEXT,
    created_at VARCHAR(64) NOT NULL,
    started_at VARCHAR(64),
    ended_at VARCHAR(64),
    PRIMARY KEY (tenant_id, id),
    KEY idx_gm_sync_sessions_tenant_created (tenant_id, created_at)
);
                "
            }
            sea_orm::DatabaseBackend::Sqlite => {
                r"
CREATE TABLE IF NOT EXISTS gm_sync_sessions (
    tenant_id TEXT NOT NULL,
    id TEXT NOT NULL,
    repo_full_name TEXT NOT NULL,
    repo_id INTEGER,
    status TEXT NOT NULL,
    progress_percent INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    summary_json TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    ended_at TEXT,
    PRIMARY KEY (tenant_id, id)
);
CREATE INDEX IF NOT EXISTS idx_gm_sync_sessions_tenant_created
    ON gm_sync_sessions (tenant_id, created_at);
                "
            }
            other => {
                return Err(DbErr::Custom(format!(
                    "migration has no DDL for database backend {other:?}"
                )));
            }
        };

        conn.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared("DROP TABLE IF EXISTS gm_sync_sessions;")
            .await?;
        Ok(())
    }
}
