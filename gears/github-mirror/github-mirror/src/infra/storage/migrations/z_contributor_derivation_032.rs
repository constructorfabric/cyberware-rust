use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Adds what DESIGN's `cpt-cf-github-mirror-dbtable-contributors` requires of
/// a derived contributor: the association roles the person was seen in
/// (`roles`, comma-separated) and the window they were seen across
/// (`first_seen_at`/`last_seen_at`).
///
/// Named with a `z_` prefix because the migration runner applies migrations
/// in **name** order and this one alters a table created by
/// `contributors_012`.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        // Nullable throughout: a row written before this migration knows
        // neither its roles nor when it was first seen, and guessing would
        // be worse than saying nothing.
        let statements: [&str; 3] = match backend {
            sea_orm::DatabaseBackend::Postgres => [
                "ALTER TABLE gm_contributors ADD COLUMN IF NOT EXISTS roles TEXT;",
                "ALTER TABLE gm_contributors ADD COLUMN IF NOT EXISTS first_seen_at TIMESTAMPTZ;",
                "ALTER TABLE gm_contributors ADD COLUMN IF NOT EXISTS last_seen_at TIMESTAMPTZ;",
            ],
            sea_orm::DatabaseBackend::MySql => [
                "ALTER TABLE gm_contributors ADD COLUMN roles MEDIUMTEXT;",
                "ALTER TABLE gm_contributors ADD COLUMN first_seen_at DATETIME(6);",
                "ALTER TABLE gm_contributors ADD COLUMN last_seen_at DATETIME(6);",
            ],
            sea_orm::DatabaseBackend::Sqlite => [
                "ALTER TABLE gm_contributors ADD COLUMN roles TEXT;",
                "ALTER TABLE gm_contributors ADD COLUMN first_seen_at TEXT;",
                "ALTER TABLE gm_contributors ADD COLUMN last_seen_at TEXT;",
            ],
            other => {
                return Err(DbErr::Custom(format!(
                    "migration has no DDL for database backend {other:?}"
                )));
            }
        };

        for sql in statements {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in ["roles", "first_seen_at", "last_seen_at"] {
            drop_column(manager, "gm_contributors", column).await?;
        }
        Ok(())
    }
}
