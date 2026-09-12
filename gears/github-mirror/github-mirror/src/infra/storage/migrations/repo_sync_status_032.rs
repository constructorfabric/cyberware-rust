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
CREATE TABLE IF NOT EXISTS gm_repo_sync_status (
    tenant_id UUID NOT NULL,
    repo_full_name VARCHAR(512) NOT NULL,
    repo_id BIGINT,
    status VARCHAR(32) NOT NULL,
    last_session_id UUID,
    last_synced_at VARCHAR(64),
    PRIMARY KEY (tenant_id, repo_full_name)
);
CREATE INDEX IF NOT EXISTS idx_gm_repo_sync_status_tenant_status
    ON gm_repo_sync_status (tenant_id, status);
                "
            }
            sea_orm::DatabaseBackend::MySql => {
                r"
CREATE TABLE IF NOT EXISTS gm_repo_sync_status (
    tenant_id VARCHAR(36) NOT NULL,
    repo_full_name VARCHAR(512) NOT NULL,
    repo_id BIGINT,
    status VARCHAR(32) NOT NULL,
    last_session_id VARCHAR(36),
    last_synced_at VARCHAR(64),
    PRIMARY KEY (tenant_id, repo_full_name),
    KEY idx_gm_repo_sync_status_tenant_status (tenant_id, status)
);
                "
            }
            sea_orm::DatabaseBackend::Sqlite => {
                r"
CREATE TABLE IF NOT EXISTS gm_repo_sync_status (
    tenant_id TEXT NOT NULL,
    repo_full_name TEXT NOT NULL,
    repo_id INTEGER,
    status TEXT NOT NULL,
    last_session_id TEXT,
    last_synced_at TEXT,
    PRIMARY KEY (tenant_id, repo_full_name)
);
CREATE INDEX IF NOT EXISTS idx_gm_repo_sync_status_tenant_status
    ON gm_repo_sync_status (tenant_id, status);
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
        conn.execute_unprepared("DROP TABLE IF EXISTS gm_repo_sync_status;")
            .await?;
        Ok(())
    }
}
