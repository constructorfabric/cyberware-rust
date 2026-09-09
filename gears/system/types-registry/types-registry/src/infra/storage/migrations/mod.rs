//! Database migrations for the Types Registry gear.
//!
//! The initial migration creates nine P0 tables. A later migration adds
//! `coordination_state`; federation still owns `source_claim` and `routing` (SPEC §9).
//!
//! Outbox tables are **not** created here. They come from
//! `toolkit_db::outbox::outbox_migrations_with_prefix("types_registry_outbox")`,
//! which the gear's `DatabaseCapability::migrations()` appends.

use sea_orm_migration::MigratorTrait;

mod m20260817_000001_initial;
mod m20260904_000002_coordination_state;

/// Migrator for the Types Registry managed-state schema.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![
            Box::new(m20260817_000001_initial::Migration),
            Box::new(m20260904_000002_coordination_state::Migration),
        ]
    }
}
