use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Adds `author_json` to `gm_issues` and `gm_pull_requests`: GitHub's `user`
/// object as it arrived, not just the login.
///
/// The `author_login` column added by `z_issue_pull_people_037` stays — it is
/// the indexable identity a `creator` filter would need, while this column is
/// what the GitHub-compatible surface hands back, so a client sees the same
/// avatar, profile URL and account type GitHub sends.
///
/// Named with a `z_` prefix because the migration runner applies migrations in
/// **name** order and this one alters tables created by `issues_002` and
/// `pull_requests_003`.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        let (json_type, exists) = match backend {
            sea_orm::DatabaseBackend::Postgres => ("TEXT", "IF NOT EXISTS "),
            sea_orm::DatabaseBackend::MySql => ("MEDIUMTEXT", ""),
            sea_orm::DatabaseBackend::Sqlite => ("TEXT", ""),
            other => {
                return Err(DbErr::Custom(format!(
                    "migration has no DDL for database backend {other:?}"
                )));
            }
        };

        for table in ["gm_issues", "gm_pull_requests"] {
            let sql = format!("ALTER TABLE {table} ADD COLUMN {exists}author_json {json_type};");
            conn.execute_unprepared(&sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for table in ["gm_issues", "gm_pull_requests"] {
            drop_column(manager, table, "author_json").await?;
        }
        Ok(())
    }
}
