use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Adds `pull_request_review_id` to `gm_review_comments`: the review an inline
/// comment belongs to, which is how a code-review client groups comments under
/// the review that produced them.
///
/// Named with a `z_` prefix because the migration runner applies migrations in
/// **name** order and this one alters a table created by `review_comments_006`.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        // Nullable: GitHub omits it for comments that belong to no review.
        let sql = match backend {
            sea_orm::DatabaseBackend::Postgres => {
                "ALTER TABLE gm_review_comments ADD COLUMN IF NOT EXISTS pull_request_review_id BIGINT;"
            }
            sea_orm::DatabaseBackend::MySql | sea_orm::DatabaseBackend::Sqlite => {
                "ALTER TABLE gm_review_comments ADD COLUMN pull_request_review_id BIGINT;"
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
        drop_column(manager, "gm_review_comments", "pull_request_review_id").await
    }
}
