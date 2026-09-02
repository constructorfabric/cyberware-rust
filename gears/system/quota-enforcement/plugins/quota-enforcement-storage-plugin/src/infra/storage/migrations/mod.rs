//! Migrations of the storage plugin. Later features append migrations for
//! their tables; the foundation creates the schema metadata and the three
//! configuration tables.

use sea_orm_migration::MigratorTrait;

mod m0001_foundation;

/// Migrator for the storage plugin schema.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![Box::new(m0001_foundation::Migration)]
    }
}
