use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Every mirrored table (PRD 5.6: "Every record MUST include an
/// `extracted_at` timestamp").
const TABLES: [&str; 26] = [
    "gm_repositories",
    "gm_issues",
    "gm_pull_requests",
    "gm_commits",
    "gm_comments",
    "gm_review_comments",
    "gm_reviews",
    "gm_labels",
    "gm_milestones",
    "gm_releases",
    "gm_branches",
    "gm_contributors",
    "gm_workflow_runs",
    "gm_pull_request_files",
    "gm_tags",
    "gm_commit_files",
    "gm_review_threads",
    "gm_commit_comments",
    "gm_issue_events",
    "gm_deployments",
    "gm_pull_request_commits",
    "gm_commit_statuses",
    "gm_workflow_jobs",
    "gm_issue_reactions",
    "gm_check_runs",
    "gm_issue_timeline",
];

/// Adds `extracted_at` — when a sync last wrote the row — to every mirrored
/// table. Doubles as the deletion-reconciliation watermark: a row whose
/// stamp predates the current sync was not seen by it.
///
/// The file is named with a `z_` prefix on purpose: the migration runner
/// applies migrations in **name** order, and this one alters tables created
/// by migrations up to `workflow_runs_013`, so its name must sort after all
/// of them.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        for table in TABLES {
            // Nullable on purpose: a row from before the column existed was
            // never stamped, and `NULL` says exactly that — reconciliation
            // reads it as older than any sync. SQLite has no timestamp type,
            // so the column is TEXT there; SeaORM decodes both.
            let sql = match backend {
                sea_orm::DatabaseBackend::Postgres => format!(
                    "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS extracted_at TIMESTAMPTZ;"
                ),
                sea_orm::DatabaseBackend::MySql => {
                    format!("ALTER TABLE {table} ADD COLUMN extracted_at DATETIME(6);")
                }
                sea_orm::DatabaseBackend::Sqlite => {
                    format!("ALTER TABLE {table} ADD COLUMN extracted_at TEXT;")
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
        for table in TABLES {
            drop_column(manager, table, "extracted_at").await?;
        }
        Ok(())
    }
}
