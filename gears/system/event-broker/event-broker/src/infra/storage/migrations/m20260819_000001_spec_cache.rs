//! `event_broker_spec_cache` table - `SpecificationManager`'s local
//! topic/event-type cache (eb-single-process-implementation D1). SQLite
//! only (decision log entry 7); no other backend branch needed.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(
            r"
CREATE TABLE IF NOT EXISTS event_broker_spec_cache (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    gts_id  TEXT NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS event_broker_spec_cache_gts_id_idx
    ON event_broker_spec_cache (gts_id);
",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS event_broker_spec_cache;")
            .await?;
        Ok(())
    }
}
