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
CREATE TABLE IF NOT EXISTS gm_http_cache (
    tenant_id UUID NOT NULL,
    cache_key VARCHAR(64) NOT NULL,
    url TEXT NOT NULL,
    etag VARCHAR(255),
    last_modified VARCHAR(64),
    body TEXT NOT NULL,
    fetched_at VARCHAR(64) NOT NULL,
    PRIMARY KEY (tenant_id, cache_key)
);
                "
            }
            sea_orm::DatabaseBackend::MySql => {
                r"
CREATE TABLE IF NOT EXISTS gm_http_cache (
    tenant_id VARCHAR(36) NOT NULL,
    cache_key VARCHAR(64) NOT NULL,
    url TEXT NOT NULL,
    etag VARCHAR(255),
    last_modified VARCHAR(64),
    body LONGTEXT NOT NULL,
    fetched_at VARCHAR(64) NOT NULL,
    PRIMARY KEY (tenant_id, cache_key)
);
                "
            }
            sea_orm::DatabaseBackend::Sqlite => {
                r"
CREATE TABLE IF NOT EXISTS gm_http_cache (
    tenant_id TEXT NOT NULL,
    cache_key TEXT NOT NULL,
    url TEXT NOT NULL,
    etag TEXT,
    last_modified TEXT,
    body TEXT NOT NULL,
    fetched_at TEXT NOT NULL,
    PRIMARY KEY (tenant_id, cache_key)
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
        conn.execute_unprepared("DROP TABLE IF EXISTS gm_http_cache;")
            .await?;
        Ok(())
    }
}
