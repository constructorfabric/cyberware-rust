use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Adds `patch` — the unified diff text GitHub returns per changed file — to
/// `gm_pull_request_files`, so a client can render a file's diff from the
/// mirrored row instead of fetching the diff separately.
///
/// Wide types on purpose: a single file's patch runs to tens of kilobytes, and
/// `MySQL`'s plain `TEXT` caps at 64 KB. GitHub itself omits the field for very
/// large diffs, so the column stays nullable.
///
/// Named with a `z_` prefix because the migration runner applies migrations in
/// **name** order and this one alters a table created by
/// `pull_request_files_014`.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        let sql = match backend {
            sea_orm::DatabaseBackend::Postgres => {
                "ALTER TABLE gm_pull_request_files ADD COLUMN IF NOT EXISTS patch TEXT;"
            }
            sea_orm::DatabaseBackend::MySql => {
                "ALTER TABLE gm_pull_request_files ADD COLUMN patch MEDIUMTEXT;"
            }
            sea_orm::DatabaseBackend::Sqlite => {
                "ALTER TABLE gm_pull_request_files ADD COLUMN patch TEXT;"
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
        drop_column(manager, "gm_pull_request_files", "patch").await
    }
}
