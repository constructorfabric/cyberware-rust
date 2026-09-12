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
CREATE TABLE IF NOT EXISTS gm_entity_fingerprints (
    tenant_id UUID NOT NULL,
    repo_id BIGINT NOT NULL,
    family VARCHAR(64) NOT NULL,
    entity_id VARCHAR(128) NOT NULL,
    fingerprint VARCHAR(64) NOT NULL,
    updated_at VARCHAR(64),
    node_id VARCHAR(128),
    child_counts_hash VARCHAR(64),
    last_refined_at VARCHAR(64),
    refinement_status VARCHAR(16) NOT NULL,
    PRIMARY KEY (tenant_id, repo_id, family, entity_id)
);
                "
            }
            sea_orm::DatabaseBackend::MySql => {
                r"
CREATE TABLE IF NOT EXISTS gm_entity_fingerprints (
    tenant_id VARCHAR(36) NOT NULL,
    repo_id BIGINT NOT NULL,
    family VARCHAR(64) NOT NULL,
    entity_id VARCHAR(128) NOT NULL,
    fingerprint VARCHAR(64) NOT NULL,
    updated_at VARCHAR(64),
    node_id VARCHAR(128),
    child_counts_hash VARCHAR(64),
    last_refined_at VARCHAR(64),
    refinement_status VARCHAR(16) NOT NULL,
    PRIMARY KEY (tenant_id, repo_id, family, entity_id)
);
                "
            }
            sea_orm::DatabaseBackend::Sqlite => {
                r"
CREATE TABLE IF NOT EXISTS gm_entity_fingerprints (
    tenant_id TEXT NOT NULL,
    repo_id INTEGER NOT NULL,
    family TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    updated_at TEXT,
    node_id TEXT,
    child_counts_hash TEXT,
    last_refined_at TEXT,
    refinement_status TEXT NOT NULL,
    PRIMARY KEY (tenant_id, repo_id, family, entity_id)
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
        conn.execute_unprepared("DROP TABLE IF EXISTS gm_entity_fingerprints;")
            .await?;
        Ok(())
    }
}
