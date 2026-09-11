//! Database migration registry for the `event-broker` gear
//! (eb-single-process-implementation). No versioned migration *chain* by
//! design (decision log entry 24-25: this is ongoing implementation with no
//! existing deployment to protect) - but schema setup still goes through the
//! platform-mandated `DatabaseCapability` seam, one idempotent
//! `CREATE TABLE IF NOT EXISTS`-shaped migration per table, never a raw
//! connection (`libs/toolkit/src/contracts.rs`'s `DatabaseCapability` rule).
//!
//! This database holds ingest and delivery metadata only: the specification
//! cache, the `Storage` facade's tables, and the ingest outbox. The event log
//! is not among them - a storage backend opens the database its own options
//! name and applies its own tables to it, so a topic's events can live
//! somewhere this gear's metadata does not.

use sea_orm_migration::prelude::*;

mod m20260819_000001_spec_cache;
mod m20260819_000002_storage_facade;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        let mut migrations: Vec<Box<dyn MigrationTrait>> = vec![
            Box::new(m20260819_000001_spec_cache::Migration),
            Box::new(m20260819_000002_storage_facade::Migration),
        ];
        // `toolkit_db::outbox::outbox_migrations()` already returns the
        // correct `Vec<Box<dyn MigrationTrait>>` shape `DatabaseCapability::
        // migrations()` expects (design.md D5) - the ingest outbox's own
        // table family, applied the same idempotent way as every other
        // table this gear owns.
        migrations.extend(toolkit_db::outbox::outbox_migrations());
        migrations
    }
}
