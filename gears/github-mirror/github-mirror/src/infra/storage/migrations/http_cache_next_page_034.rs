use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        // A 304 sometimes omits the Link header, so the next-page URL is kept
        // with the entry rather than re-derived on revalidation.
        let sql = match backend {
            sea_orm::DatabaseBackend::Postgres
            | sea_orm::DatabaseBackend::MySql
            | sea_orm::DatabaseBackend::Sqlite => {
                "ALTER TABLE gm_http_cache ADD COLUMN next_page TEXT;"
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

    /// Drops the whole table rather than the one column.
    ///
    /// `http_cache_rebuild_035` rolls back by dropping `gm_http_cache`, so by
    /// the time this runs the table is usually gone and dropping a column from
    /// it would fail. The table is a cache: recreating it costs one re-fetch,
    /// and `IF EXISTS` makes this safe in either order.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared("DROP TABLE IF EXISTS gm_http_cache;")
            .await?;
        Ok(())
    }
}
