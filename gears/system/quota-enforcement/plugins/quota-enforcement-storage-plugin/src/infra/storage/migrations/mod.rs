//! Migrations of the storage plugin. The foundation creates the schema
//! metadata and the three configuration tables; `m0002` adds the Quota tables
//! of the quota-lifecycle feature; the toolkit outbox tables follow under
//! their own prefix. Later features append their tables here.

use sea_orm_migration::MigratorTrait;
use toolkit_db::outbox::outbox_migrations_with_prefix;

mod m0001_foundation;
mod m0002_quotas;

/// Prefix of the notification outbox tables (`qe_outbox_body`,
/// `qe_outbox_incoming`, ...). The toolkit joins prefix and suffix with an
/// underscore, so the prefix ends in `outbox`, not in `_`.
pub const OUTBOX_TABLE_PREFIX: &str = "qe_outbox";

/// Migrator for the storage plugin schema.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        let mut migrations: Vec<Box<dyn sea_orm_migration::MigrationTrait>> = vec![
            Box::new(m0001_foundation::Migration),
            Box::new(m0002_quotas::Migration),
        ];
        migrations.extend(outbox_migrations());
        migrations
    }
}

/// The outbox migrations under [`OUTBOX_TABLE_PREFIX`]. The prefix is a
/// constant the unit test below validates; the toolkit rejecting it would be
/// a programming error, not a runtime condition.
fn outbox_migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    #[allow(
        clippy::panic,
        reason = "a constant table prefix the unit test validates; no runtime input"
    )]
    outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX).unwrap_or_else(|e| {
        panic!("quota-enforcement outbox table prefix `{OUTBOX_TABLE_PREFIX}` is invalid: {e}")
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod migrations_tests {
    use super::*;

    #[test]
    fn the_outbox_prefix_is_accepted_by_the_toolkit() {
        assert!(outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX).is_ok());
        assert_eq!(Migrator::migrations().len(), 3);
    }
}
