//! Add the author-identity columns to `role_assignments`.
//!
//! `created_by` alone cannot be resolved to a person: it carries no kind
//! (is the author a user or a machine?) and no tenant (whom would a
//! reader ask?). Both facts are available for free in the caller's
//! `SecurityContext` at create time, so they are stamped onto the row
//! and the *name* is resolved later, on the read path. Resolving the name
//! at write time would put an identity round trip on the create path,
//! which today has none — a role grant would start failing whenever the
//! `IdP` is slow.
//!
//! Deliberately nullable and un-backfilled:
//!
//! * pre-existing rows have neither fact and nothing can recover them —
//!   the subject id is all that was ever written;
//! * machine authors (the platform bootstrap's root-scope row) legitimately
//!   have no user identity to record.
//!
//! The read path treats NULL exactly as it treats a row written before
//! this migration: `created_by` is served with no `created_by_name`. That
//! is why there is no index either — the columns are never filtered or
//! ordered on, only read back with the row they belong to.
//!
//! The DDL branches on `sea_orm::DatabaseBackend` so the same migration
//! runs against `Postgres` (production) and `SQLite` (smoke tests /
//! demos), mirroring `m20260521_000002_create_role_assignments_table`.

use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// ---------------------------------------------------------------------------
// Postgres variant.
// ---------------------------------------------------------------------------
//
// `created_by_type` is `text` rather than an enum type for the same
// reason `principal_type` is: the closed set lives in the
// `rbac_sdk::models::PrincipalType` enum and is enforced at the
// application layer, so adding a kind never needs a DDL migration. The
// read mapper parses the column leniently (an unknown tag reads as "no
// author identity") so an older node can serve rows written by a newer
// one.
//
// `IF NOT EXISTS` for the same reason `down` uses `IF EXISTS`: the two
// `ALTER`s are separate statements, not one transaction, so a crash
// between them (lock timeout, evicted pod, full disk) leaves the first
// column added while the migration is still unrecorded. Without the
// clause the next startup would re-run `up` and fail permanently on
// `column "created_by_type" already exists`, with no way forward but
// manual DDL. With it, `up` is idempotent and the retry completes.
const PG_UP_STATEMENTS: &[&str] = &[
    "ALTER TABLE role_assignments ADD COLUMN IF NOT EXISTS created_by_type text",
    "ALTER TABLE role_assignments ADD COLUMN IF NOT EXISTS created_by_tenant_id uuid",
];

// Dropped in reverse order of creation, and `IF EXISTS` so a partially
// applied `up` (the second `ALTER` failed) still rolls back cleanly.
const PG_DOWN_STATEMENTS: &[&str] = &[
    "ALTER TABLE role_assignments DROP COLUMN IF EXISTS created_by_tenant_id",
    "ALTER TABLE role_assignments DROP COLUMN IF EXISTS created_by_type",
];

// ---------------------------------------------------------------------------
// SQLite variant.
// ---------------------------------------------------------------------------
//
// Two differences from the Postgres variant: `uuid` becomes `text`, which
// is how the initial migration already declares `tenant_id` (SeaORM
// serialises `Uuid` to text on SQLite); and the `ADD COLUMN` stays in its
// plain form because SQLite has no `IF NOT EXISTS` clause on `ALTER TABLE
// ... ADD COLUMN` (same asymmetry as the `down` statements below). The
// retry hazard the Postgres variant guards against is a production
// concern anyway — SQLite here backs smoke tests and demos, which start
// from an empty file.
const SQLITE_UP_STATEMENTS: &[&str] = &[
    "ALTER TABLE role_assignments ADD COLUMN created_by_type text",
    "ALTER TABLE role_assignments ADD COLUMN created_by_tenant_id text",
];

// SQLite gained `ALTER TABLE ... DROP COLUMN` in 3.35 (2021) and does not
// accept `IF EXISTS` on it, so the clause is omitted here.
const SQLITE_DOWN_STATEMENTS: &[&str] = &[
    "ALTER TABLE role_assignments DROP COLUMN created_by_tenant_id",
    "ALTER TABLE role_assignments DROP COLUMN created_by_type",
];

// ---------------------------------------------------------------------------
// Migration dispatch.
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        let statements: &[&str] = match backend {
            sea_orm::DatabaseBackend::Postgres => PG_UP_STATEMENTS,
            sea_orm::DatabaseBackend::Sqlite => SQLITE_UP_STATEMENTS,
            // `DatabaseBackend` is `#[non_exhaustive]` as of SeaORM 2.0, so the
            // unsupported case is a wildcard rather than a `MySql` arm: any backend
            // outside the PostgreSQL/SQLite pair this module targets must fail fast
            // instead of falling through to SQL written for a different dialect.
            other => {
                return Err(DbErr::Migration(format!(
                    "rbac migrations: unsupported database backend {other:?}"
                )));
            }
        };
        for sql in statements {
            conn.execute_raw(Statement::from_string(backend, (*sql).to_owned()))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        let statements: &[&str] = match backend {
            sea_orm::DatabaseBackend::Postgres => PG_DOWN_STATEMENTS,
            sea_orm::DatabaseBackend::Sqlite => SQLITE_DOWN_STATEMENTS,
            // See the `up` arm: an unknown backend fails fast rather than
            // running another dialect's DDL.
            other => {
                return Err(DbErr::Migration(format!(
                    "rbac migrations: unsupported database backend {other:?}"
                )));
            }
        };
        for sql in statements {
            conn.execute_raw(Statement::from_string(backend, (*sql).to_owned()))
                .await?;
        }
        Ok(())
    }
}
