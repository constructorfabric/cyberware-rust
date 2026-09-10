use sea_orm_migration::prelude::*;

use super::support::drop_column;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Tables DESIGN gives a `node_id TEXT — GraphQL global ID` column.
const TABLES: [&str; 3] = ["gm_repositories", "gm_issues", "gm_pull_requests"];

/// Adds `node_id`, GitHub's GraphQL global id, to the three tables DESIGN
/// specifies it on. REST returns it on every entity, and it is what lets a
/// GraphQL response be matched to an already-mirrored row.
///
/// Named with a `z_` prefix because the migration runner applies migrations in
/// **name** order and this one alters tables created by earlier migrations.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();

        for table in TABLES {
            // Nullable: rows mirrored before this column existed have no
            // node id, and it cannot be derived from what was stored.
            let sql = match backend {
                sea_orm::DatabaseBackend::Postgres => {
                    format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS node_id VARCHAR(255);")
                }
                sea_orm::DatabaseBackend::MySql | sea_orm::DatabaseBackend::Sqlite => {
                    format!("ALTER TABLE {table} ADD COLUMN node_id VARCHAR(255);")
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
            drop_column(manager, table, "node_id").await?;
        }
        Ok(())
    }
}
