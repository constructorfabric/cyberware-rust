//! This backend's tables, applied to the database this backend opens.
//!
//! They are not in the host gear's migration chain: that chain runs against the
//! platform-provided gear database, which keeps ingest and delivery metadata,
//! and the event log is not among them. Nothing raw is touched either way - the
//! definitions go through `toolkit_db`'s migration runner, against a connection
//! this crate owns.

use sea_orm_migration::MigrationTrait;

mod m20260819_000003_sqlite_backend;

/// Every table this backend owns, as one idempotent migration. Not exported:
/// the crate applies them itself, to the database it opened.
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![Box::new(m20260819_000003_sqlite_backend::Migration)]
}
