//! Create the `role_definitions` table with its CHECK constraints,
//! uniqueness indexes, and performance indexes. The DDL branches on
//! `sea_orm::DatabaseBackend` so the same migration runs against
//! `Postgres` (production) and `SQLite` (smoke tests / dev demos).
//!
//! The `SQLite` variant drops the `Postgres`-only `pg_trgm` /
//! `text_pattern_ops` / `USING gin (...)` indexes and replaces `jsonb`
//! with `text` storing JSON. Functional correctness is preserved —
//! `LIKE` filters still work, JSON arrays are still validated via the
//! JSON1 `json_array_length()` function — only query-plan speed is
//! affected, and only on `SQLite` (a non-production backend).

use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// ---------------------------------------------------------------------------
// Postgres variant — canonical production schema.
// ---------------------------------------------------------------------------

const PG_CREATE_ROLE_DEFINITIONS: &str = "
CREATE TABLE role_definitions (
    id                 uuid          PRIMARY KEY,
    name               varchar(256)  NOT NULL,
    description        varchar(4096),
    is_built_in        boolean       NOT NULL DEFAULT false,
    permissions        jsonb         NOT NULL DEFAULT '[]'::jsonb,
    not_permissions    jsonb         NOT NULL DEFAULT '[]'::jsonb,
    assignable_scopes  jsonb         NOT NULL
        CONSTRAINT chk_role_definitions_assignable_scopes_nonempty
        CHECK (jsonb_array_length(assignable_scopes) > 0),
    owner_tenant_id    uuid
        CONSTRAINT chk_role_definitions_builtin_owner CHECK (
            (is_built_in AND owner_tenant_id IS NULL)
            OR (NOT is_built_in AND owner_tenant_id IS NOT NULL)
        ),
    created_at         timestamptz   NOT NULL DEFAULT now(),
    updated_at         timestamptz   NOT NULL DEFAULT now(),
    created_by         text          NOT NULL
)";

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE EXTENSION IF NOT EXISTS pg_trgm",
    PG_CREATE_ROLE_DEFINITIONS,
    "CREATE UNIQUE INDEX uq_role_name_per_tenant
        ON role_definitions (name, owner_tenant_id)
        WHERE owner_tenant_id IS NOT NULL",
    "CREATE UNIQUE INDEX uq_role_name_builtin
        ON role_definitions (name)
        WHERE owner_tenant_id IS NULL",
    "CREATE INDEX idx_role_definitions_owner_tenant
        ON role_definitions (owner_tenant_id)",
    "CREATE INDEX idx_role_definitions_is_built_in
        ON role_definitions (is_built_in)",
    "CREATE INDEX idx_role_definitions_name
        ON role_definitions USING gin (name gin_trgm_ops)",
    "CREATE INDEX idx_role_definitions_permissions
        ON role_definitions USING gin (permissions)",
    "CREATE INDEX idx_role_definitions_assignable_scopes
        ON role_definitions USING gin (assignable_scopes)",
    // Keyset index for `GET /rbac/v1/role-definitions`, which paginates by
    // `(created_at DESC, id DESC)` (see
    // `role_definition_repo::list`). Without this, broad or
    // unrestricted list requests fall back to a sequential scan + sort
    // instead of an index-backed keyset query.
    "CREATE INDEX idx_role_definitions_created_at_id
        ON role_definitions (created_at DESC, id DESC)",
];

// `DROP TABLE` cascades to every index and CHECK constraint defined on
// the table, so explicit `DROP INDEX` calls are unnecessary. `pg_trgm`
// is intentionally left in place — it's database-global and may be in
// use by other modules; `up` creates it with `IF NOT EXISTS` so a
// re-run is cheap.
const PG_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS role_definitions"];

// ---------------------------------------------------------------------------
// SQLite variant — non-production schema for smoke tests / demos.
// ---------------------------------------------------------------------------
//
// Differences from the Postgres variant, documented inline so a future
// reader knows what was dropped and why:
//
// * `jsonb` → `text`. SQLite has no JSONB type; JSON is stored as text
//   and queried via the JSON1 extension (linked into every modern
//   SQLite build, including the rusqlite the workspace uses).
// * `jsonb_array_length(col) > 0` → `json_array_length(col) > 0`.
// * `pg_trgm` extension and every `USING gin (...)` index dropped.
//   SQLite has no GIN, no trigram. `LIKE` filtering degrades to O(n)
//   scans — acceptable for the SQLite use cases (tests, dev).
// * `varchar(N)` kept for documentation; SQLite ignores the length
//   bound by design (TEXT is variable-length regardless).
// * `timestamptz NOT NULL DEFAULT now()` → `text NOT NULL DEFAULT (CURRENT_TIMESTAMP)`.
//   SeaORM decodes either into `chrono::DateTime<Utc>`; matching the
//   Postgres wire shape is not required because the rbac test harness
//   round-trips through SeaORM.
// * Partial unique indexes (`WHERE col IS NOT NULL`) and STORED
//   generated columns are supported on SQLite 3.8 / 3.31+ — kept
//   unchanged.

const SQLITE_CREATE_ROLE_DEFINITIONS: &str = "
CREATE TABLE role_definitions (
    id                 text          PRIMARY KEY,
    name               text          NOT NULL,
    description        text,
    is_built_in        boolean       NOT NULL DEFAULT false,
    permissions        text          NOT NULL DEFAULT '[]',
    not_permissions    text          NOT NULL DEFAULT '[]',
    assignable_scopes  text          NOT NULL
        CONSTRAINT chk_role_definitions_assignable_scopes_nonempty
        CHECK (json_array_length(assignable_scopes) > 0),
    owner_tenant_id    text
        CONSTRAINT chk_role_definitions_builtin_owner CHECK (
            (is_built_in AND owner_tenant_id IS NULL)
            OR (NOT is_built_in AND owner_tenant_id IS NOT NULL)
        ),
    created_at         text          NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    updated_at         text          NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    created_by         text          NOT NULL
)";

const SQLITE_UP_STATEMENTS: &[&str] = &[
    SQLITE_CREATE_ROLE_DEFINITIONS,
    "CREATE UNIQUE INDEX uq_role_name_per_tenant
        ON role_definitions (name, owner_tenant_id)
        WHERE owner_tenant_id IS NOT NULL",
    "CREATE UNIQUE INDEX uq_role_name_builtin
        ON role_definitions (name)
        WHERE owner_tenant_id IS NULL",
    "CREATE INDEX idx_role_definitions_owner_tenant
        ON role_definitions (owner_tenant_id)",
    "CREATE INDEX idx_role_definitions_is_built_in
        ON role_definitions (is_built_in)",
    "CREATE INDEX idx_role_definitions_created_at_id
        ON role_definitions (created_at DESC, id DESC)",
];

const SQLITE_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS role_definitions"];

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
}
