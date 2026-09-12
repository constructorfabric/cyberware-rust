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
CREATE TABLE IF NOT EXISTS gm_sync_watermarks (
    tenant_id UUID NOT NULL,
    repo_id BIGINT NOT NULL,
    family VARCHAR(64) NOT NULL,
    last_seen_updated_at VARCHAR(64),
    page1_etag VARCHAR(255),
    sweep_in_progress BOOLEAN NOT NULL,
    candidate_high_water VARCHAR(64),
    PRIMARY KEY (tenant_id, repo_id, family)
);
                "
            }
            sea_orm::DatabaseBackend::MySql => {
                r"
CREATE TABLE IF NOT EXISTS gm_sync_watermarks (
    tenant_id VARCHAR(36) NOT NULL,
    repo_id BIGINT NOT NULL,
    family VARCHAR(64) NOT NULL,
    last_seen_updated_at VARCHAR(64),
    page1_etag VARCHAR(255),
    sweep_in_progress BOOLEAN NOT NULL,
    candidate_high_water VARCHAR(64),
    PRIMARY KEY (tenant_id, repo_id, family)
);
                "
            }
            sea_orm::DatabaseBackend::Sqlite => {
                r"
CREATE TABLE IF NOT EXISTS gm_sync_watermarks (
    tenant_id TEXT NOT NULL,
    repo_id INTEGER NOT NULL,
    family TEXT NOT NULL,
    last_seen_updated_at TEXT,
    page1_etag TEXT,
    sweep_in_progress BOOLEAN NOT NULL,
    candidate_high_water TEXT,
    PRIMARY KEY (tenant_id, repo_id, family)
);
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
        conn.execute_unprepared("DROP TABLE IF EXISTS gm_sync_watermarks;")
            .await?;
        Ok(())
    }
}
