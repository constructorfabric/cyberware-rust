//! RBAC migration harness — asserts the post-migration schema by probing
//! `information_schema`, `pg_extension`, `pg_indexes`, `pg_constraint`,
//! and `pg_attribute`.
//!
//! ```bash
//! cargo test -p cf-gears-rbac --test postgres_migrations -- --ignored
//! ```

// PostgreSQL system catalogs are not surfaced through SecureORM, so the
// harness uses raw sqlx and DE0706 is silenced at file scope.
#![cfg(test)]
#![allow(clippy::doc_markdown)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use anyhow::Result;
use sqlx::Row;

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn extension_pg_trgm_is_installed() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let row = sqlx::query("SELECT 1::int AS marker FROM pg_extension WHERE extname = 'pg_trgm'")
        .fetch_optional(&db.pool)
        .await?;
    assert!(
        row.is_some(),
        "pg_trgm MUST be installed (m20260521_000001) \u{2014} required for the GIN trigram index on \
         role_definitions.name"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn role_definitions_table_exists_with_expected_columns() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let rows = sqlx::query(
        "SELECT column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns \
         WHERE table_name = 'role_definitions' \
         ORDER BY ordinal_position",
    )
    .fetch_all(&db.pool)
    .await?;
    let names: Vec<String> = rows.iter().map(|r| r.get("column_name")).collect();
    let expected = [
        "id",
        "name",
        "description",
        "is_built_in",
        "permissions",
        "not_permissions",
        "assignable_scopes",
        "owner_tenant_id",
        "created_at",
        "updated_at",
        "created_by",
    ];
    for column in expected {
        assert!(
            names.iter().any(|n| n == column),
            "role_definitions.{column} MUST exist. \
             Found columns: {names:?}",
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn role_assignments_table_exists_with_expected_columns() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let rows = sqlx::query(
        "SELECT column_name, data_type, is_nullable \
         FROM information_schema.columns \
         WHERE table_name = 'role_assignments' \
         ORDER BY ordinal_position",
    )
    .fetch_all(&db.pool)
    .await?;
    let names: Vec<String> = rows.iter().map(|r| r.get("column_name")).collect();
    for column in [
        "id",
        "role_definition_id",
        "principal_id",
        "principal_type",
        "scope",
        "scope_depth",
        "tenant_id",
        "created_at",
        "updated_at",
        "created_by",
        "created_by_type",
        "created_by_tenant_id",
    ] {
        assert!(
            names.iter().any(|n| n == column),
            "role_assignments.{column} MUST exist. \
             Found columns: {names:?}",
        );
    }

    // `scope_depth` and `tenant_id` are plain columns populated by the
    // application from the parsed `Scope`. Postgres marks
    // `GENERATED ALWAYS` columns with `attgenerated = 's'`; plain columns
    // leave the byte at its zero default, which `::text` renders as the
    // empty string. Guards against accidentally adding a SQL-side
    // derivation that would diverge from the application-side one.
    let rows = sqlx::query(
        "SELECT a.attname AS name, a.attgenerated::text AS attgenerated \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         WHERE c.relname = 'role_assignments' \
           AND a.attname IN ('scope_depth', 'tenant_id')",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows.len(),
        2,
        "pg_attribute MUST surface both scope_depth and tenant_id rows",
    );
    for row in &rows {
        let name: String = row.get("name");
        let attgenerated: String = row.get("attgenerated");
        assert!(
            attgenerated.is_empty(),
            "{name} MUST be a plain column (attgenerated='') - application-derived from scope, \
             not a SQL-side GENERATED column. Got attgenerated={attgenerated:?}",
        );
    }
    Ok(())
}

/// The author-identity pair from
/// `m20260824_000003_add_role_assignment_author_identity` must stay
/// **nullable**, and keep the same storage shapes the rest of the table
/// uses: the kind as `text` (the closed enum lives in Rust, so adding a
/// principal kind never needs DDL) and the home tenant as `uuid` (matching
/// the existing `tenant_id` column).
///
/// Nullability is the load-bearing half. Rows written before the migration
/// carry no author identity and nothing can recover it — the subject id is
/// all that was ever stored — and a machine author has no user identity to
/// record. A `NOT NULL`, with or without a default, would either fail the
/// migration on any populated cluster or invent an author that never
/// existed.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p rbac -- --ignored`"]
async fn role_assignments_author_identity_columns_are_nullable_and_typed() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let rows = sqlx::query(
        "SELECT column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns \
         WHERE table_name = 'role_assignments' \
           AND column_name IN ('created_by_type', 'created_by_tenant_id')",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows.len(),
        2,
        "both author-identity columns MUST exist on role_assignments \
         (m20260824_000003)",
    );
    for row in &rows {
        let name: String = row.get("column_name");
        let data_type: String = row.get("data_type");
        let is_nullable: String = row.get("is_nullable");
        let default: Option<String> = row.get("column_default");
        assert_eq!(
            is_nullable, "YES",
            "role_assignments.{name} MUST be nullable - pre-migration rows and \
             machine authors legitimately have no author identity",
        );
        assert!(
            default.is_none(),
            "role_assignments.{name} MUST NOT carry a DB default: a default would \
             backfill an author that never existed. Got {default:?}",
        );
        // `created_by_type` mirrors `principal_type`'s `text` shape; the
        // tenant mirrors `tenant_id`'s `uuid`.
        let expected = if name == "created_by_type" {
            "text"
        } else {
            "uuid"
        };
        assert_eq!(
            data_type, expected,
            "role_assignments.{name} MUST be {expected} (m20260824_000003)",
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn role_definitions_check_constraints_present() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let rows = sqlx::query(
        "SELECT pg_get_constraintdef(con.oid) AS def \
         FROM pg_constraint con \
         JOIN pg_class cls ON cls.oid = con.conrelid \
         WHERE cls.relname = 'role_definitions' AND con.contype = 'c'",
    )
    .fetch_all(&db.pool)
    .await?;
    let defs: Vec<String> = rows.iter().map(|r| r.get("def")).collect();
    assert!(
        defs.iter().any(|d| d.contains("jsonb_array_length")),
        "the non-empty assignable_scopes CHECK MUST be present (m20260521_000001). \
         Found: {defs:?}",
    );
    assert!(
        defs.iter().any(|d| d.contains("is_built_in")),
        "the is_built_in / owner_tenant_id bi-conditional CHECK MUST be present \
         (m20260521_000001). Found: {defs:?}",
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn role_assignments_fk_uses_on_delete_restrict() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    // `confdeltype = 'r'` ⇒ ON DELETE RESTRICT.
    let row = sqlx::query(
        "SELECT con.confdeltype::text AS confdeltype \
         FROM pg_constraint con \
         JOIN pg_class cls ON cls.oid = con.conrelid \
         WHERE cls.relname = 'role_assignments' AND con.contype = 'f'",
    )
    .fetch_one(&db.pool)
    .await?;
    let confdeltype: String = row.get("confdeltype");
    assert_eq!(
        confdeltype, "r",
        "role_assignments.role_definition_id FK MUST be ON DELETE RESTRICT (m_003)"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn every_named_index_from_design_72_is_present() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let rows = sqlx::query(
        "SELECT indexname FROM pg_indexes \
         WHERE schemaname = 'public' AND tablename IN ('role_definitions', 'role_assignments')",
    )
    .fetch_all(&db.pool)
    .await?;
    let names: Vec<String> = rows.iter().map(|r| r.get("indexname")).collect();
    let expected = [
        // Uniqueness
        "uq_role_name_per_tenant",
        "uq_role_name_builtin",
        "uq_assignment",
        // Performance
        "idx_role_definitions_owner_tenant",
        "idx_role_definitions_is_built_in",
        "idx_role_definitions_name",
        "idx_role_definitions_permissions",
        "idx_role_definitions_assignable_scopes",
        "idx_role_assignments_principal",
        "idx_role_assignments_principal_scope_depth",
        "idx_role_assignments_role",
        "idx_role_assignments_scope_prefix",
        "idx_role_assignments_scope_depth",
        // List keyset on `(created_at DESC, id DESC)`.
        "idx_role_definitions_created_at_id",
        "idx_role_assignments_created_at_id",
    ];
    for index in expected {
        assert!(
            names.iter().any(|n| n == index),
            "{index} MUST exist. Found indexes: {names:?}",
        );
    }
    Ok(())
}
