use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

/// Recreates `gm_http_cache` with a binary body plus the compression and
/// integrity metadata PRD 5.6 requires.
///
/// The migration runner applies migrations in **name** order, not registration
/// order, so this module's name must sort after `http_cache_next_page_034` —
/// otherwise the rebuild would run first and 034 would then try to add a
/// column that already exists.
///
/// The table is dropped rather than altered: it is a cache, so the worst a
/// reset costs is one full re-fetch, and altering a column's type is not
/// portable across the three supported engines.
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
DROP TABLE IF EXISTS gm_http_cache;
CREATE TABLE gm_http_cache (
    tenant_id UUID NOT NULL,
    cache_key VARCHAR(64) NOT NULL,
    url TEXT NOT NULL,
    status INTEGER NOT NULL,
    etag VARCHAR(255),
    last_modified VARCHAR(64),
    next_page TEXT,
    body BYTEA NOT NULL,
    compression VARCHAR(8) NOT NULL,
    content_hash VARCHAR(64) NOT NULL,
    fetched_at VARCHAR(64) NOT NULL,
    PRIMARY KEY (tenant_id, cache_key)
);
                "
            }
            sea_orm::DatabaseBackend::MySql => {
                r"
DROP TABLE IF EXISTS gm_http_cache;
CREATE TABLE gm_http_cache (
    tenant_id VARCHAR(36) NOT NULL,
    cache_key VARCHAR(64) NOT NULL,
    url TEXT NOT NULL,
    status INT NOT NULL,
    etag VARCHAR(255),
    last_modified VARCHAR(64),
    next_page TEXT,
    body LONGBLOB NOT NULL,
    compression VARCHAR(8) NOT NULL,
    content_hash VARCHAR(64) NOT NULL,
    fetched_at VARCHAR(64) NOT NULL,
    PRIMARY KEY (tenant_id, cache_key)
);
                "
            }
            sea_orm::DatabaseBackend::Sqlite => {
                r"
DROP TABLE IF EXISTS gm_http_cache;
CREATE TABLE gm_http_cache (
    tenant_id TEXT NOT NULL,
    cache_key TEXT NOT NULL,
    url TEXT NOT NULL,
    status INTEGER NOT NULL,
    etag TEXT,
    last_modified TEXT,
    next_page TEXT,
    body BLOB NOT NULL,
    compression TEXT NOT NULL,
    content_hash TEXT NOT NULL,
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
