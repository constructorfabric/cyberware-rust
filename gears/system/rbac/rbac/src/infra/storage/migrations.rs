//! Postgres migration set for the RBAC module. Greenfield chain run in
//! dependency order (extension → `role_definitions` → `role_assignments`,
//! since `role_assignments.role_definition_id` references `role_definitions`).
//! `down` reverses in catalog-safe order.
//!
//! Ordering is positional in [`Migrator::migrations`], not alphabetical, so
//! every additive migration goes at the *end* of the vector: an `ALTER
//! TABLE` cannot run before the `CREATE TABLE` it amends.

pub mod m20260521_000001_create_role_definitions_table;
pub mod m20260521_000002_create_role_assignments_table;
pub mod m20260824_000003_add_role_assignment_author_identity;

use sea_orm_migration::prelude::*;

/// Aggregate `MigratorTrait` for the RBAC module.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260521_000001_create_role_definitions_table::Migration),
            Box::new(m20260521_000002_create_role_assignments_table::Migration),
            // Additive: two nullable author-identity columns on
            // `role_assignments`. MUST stay after the `CREATE TABLE` above.
            Box::new(m20260824_000003_add_role_assignment_author_identity::Migration),
        ]
    }
}
