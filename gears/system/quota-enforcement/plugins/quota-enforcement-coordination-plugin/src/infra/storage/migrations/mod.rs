//! Migrations of the coordination plugin.

use sea_orm_migration::MigratorTrait;

mod m0001_coordination_locks;

/// Migrator for the `qe_coordination_locks` table.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![Box::new(m0001_coordination_locks::Migration)]
    }
}
