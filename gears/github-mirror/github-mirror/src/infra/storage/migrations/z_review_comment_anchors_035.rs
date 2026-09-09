use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Adds GitHub's multi-line diff anchors to `gm_review_comments`.
///
/// `position`/`original_position` (migration `review_comments_diff_anchors_029`)
/// are the deprecated single-line anchors; GitHub's own UI has positioned
/// inline comments with `line`/`side` — and `start_line`/`start_side` for a
/// multi-line selection — since 2022. Both sets are kept: the old one still
/// resolves comments mirrored before this column existed.
///
/// Named with a `z_` prefix because the migration runner applies migrations in
/// **name** order and this one alters a table created by `review_comments_006`.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        let numeric = ["line", "original_line", "start_line", "original_start_line"];
        let textual = ["side", "start_side", "subject_type"];

        for column in numeric {
            let sql = match backend {
                sea_orm::DatabaseBackend::Postgres => format!(
                    "ALTER TABLE gm_review_comments ADD COLUMN IF NOT EXISTS {column} BIGINT;"
                ),
                sea_orm::DatabaseBackend::MySql | sea_orm::DatabaseBackend::Sqlite => {
                    format!("ALTER TABLE gm_review_comments ADD COLUMN {column} BIGINT;")
                }
                other => {
                    return Err(DbErr::Custom(format!(
                        "migration has no DDL for database backend {other:?}"
                    )));
                }
            };
            conn.execute_unprepared(&sql).await?;
        }

        for column in textual {
            let sql = match backend {
                sea_orm::DatabaseBackend::Postgres => format!(
                    "ALTER TABLE gm_review_comments ADD COLUMN IF NOT EXISTS {column} VARCHAR(32);"
                ),
                sea_orm::DatabaseBackend::MySql | sea_orm::DatabaseBackend::Sqlite => {
                    format!("ALTER TABLE gm_review_comments ADD COLUMN {column} VARCHAR(32);")
                }
                other => {
                    return Err(DbErr::Custom(format!(
                        "migration has no DDL for database backend {other:?}"
                    )));
                }
            };
            conn.execute_unprepared(&sql).await?;
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            "line",
            "original_line",
            "start_line",
            "original_start_line",
            "side",
            "start_side",
            "subject_type",
        ] {
            drop_column(manager, "gm_review_comments", column).await?;
        }
        Ok(())
    }
}
