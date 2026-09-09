use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Adds the fields a GitHub client renders an issue or pull-request list with:
/// who wrote it, who it is assigned to, which labels it carries, and how many
/// comments it has (PRD 5.8's "schema-compatible with GitHub's responses").
///
/// Assignees and labels are stored as JSON arrays rather than join tables: the
/// mirror only ever hands them back with their issue, and a join table would
/// mean another write path per sync plus another family in reconciliation.
///
/// Named with a `z_` prefix because the migration runner applies migrations in
/// **name** order and this one alters tables created by `issues_002` and
/// `pull_requests_003`.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        let json_type = match backend {
            sea_orm::DatabaseBackend::MySql => "MEDIUMTEXT",
            sea_orm::DatabaseBackend::Postgres | sea_orm::DatabaseBackend::Sqlite => "TEXT",
            other => {
                return Err(DbErr::Custom(format!(
                    "migration has no DDL for database backend {other:?}"
                )));
            }
        };
        let exists = if backend == sea_orm::DatabaseBackend::Postgres {
            "IF NOT EXISTS "
        } else {
            ""
        };

        for table in ["gm_issues", "gm_pull_requests"] {
            for (column, ty) in [
                ("author_login", "VARCHAR(255)"),
                ("assignees_json", json_type),
                ("labels_json", json_type),
                ("comments_count", "BIGINT"),
                ("locked", "BOOLEAN"),
            ] {
                let sql = format!("ALTER TABLE {table} ADD COLUMN {exists}{column} {ty};");
                conn.execute_unprepared(&sql).await?;
            }
        }

        // Reviewers requested on a pull request have no issue equivalent.
        let sql = format!(
            "ALTER TABLE gm_pull_requests ADD COLUMN {exists}requested_reviewers_json {json_type};"
        );
        conn.execute_unprepared(&sql).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let columns = [
            "author_login",
            "assignees_json",
            "labels_json",
            "comments_count",
            "locked",
        ];
        for table in ["gm_issues", "gm_pull_requests"] {
            for column in columns {
                drop_column(manager, table, column).await?;
            }
        }
        drop_column(manager, "gm_pull_requests", "requested_reviewers_json").await
    }
}
