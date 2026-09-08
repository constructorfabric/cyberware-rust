//! Persistence-schema constraint integration tests — each test verifies
//! one CHECK / FK / UNIQUE constraint against an ephemeral PostgreSQL
//! container.

// Matching SQLSTATE codes requires raw `sqlx::Error::into_database_error()`
// (not exposed through SecureORM), so DE0706 is silenced here.
#![cfg(test)]
#![allow(clippy::expect_used, clippy::panic, clippy::doc_markdown)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use anyhow::Result;
use uuid::Uuid;

/// Confirm the error carries the expected SQLSTATE code.
fn expect_db_error_with_sqlstate(err: sqlx::Error, expected_state: &str, scenario: &str) {
    expect_db_error_with_any_sqlstate(err, &[expected_state], scenario);
}

/// Confirm the error carries one of `expected_states`.
///
/// Exists for conditions Postgres reports under different codes across
/// versions: an `ON DELETE RESTRICT` violation is `23001`
/// (`restrict_violation`) on `PostgreSQL` 18 and `23503`
/// (`foreign_key_violation`) on 17 and earlier. Pinning a single code there
/// asserts the server version, not the schema constraint this suite is
/// about — and the tests run against whatever tag `test-containers` pins.
fn expect_db_error_with_any_sqlstate(err: sqlx::Error, expected_states: &[&str], scenario: &str) {
    let db_err = err
        .into_database_error()
        .unwrap_or_else(|| panic!("{scenario}: expected a database error, got non-DB sqlx error"));
    let state = db_err
        .code()
        .unwrap_or_else(|| panic!("{scenario}: database error MUST carry a SQLSTATE code"));
    assert!(
        expected_states.contains(&state.as_ref()),
        "{scenario}: expected SQLSTATE one of {expected_states:?} (23514 = check_violation, \
         23505 = unique_violation, 23503 = foreign_key_violation, \
         23001 = restrict_violation), got {state}",
    );
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i1_empty_assignable_scopes_is_rejected() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let result = sqlx::query(
        "INSERT INTO role_definitions (id, name, is_built_in, permissions, not_permissions, \
         assignable_scopes, owner_tenant_id, created_by) \
         VALUES ($1, 'Empty Scope Role', true, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, NULL, 'tester')",
    )
    .bind(Uuid::new_v4())
    .execute(&db.pool)
    .await;
    let err = result.expect_err("I-1: empty assignable_scopes MUST be rejected");
    expect_db_error_with_sqlstate(err, "23514", "I-1: empty assignable_scopes");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i2_null_owner_tenant_id_for_non_built_in_is_rejected() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let result = sqlx::query(
        "INSERT INTO role_definitions (id, name, is_built_in, permissions, not_permissions, \
         assignable_scopes, owner_tenant_id, created_by) \
         VALUES ($1, 'Custom no-owner', false, '[]'::jsonb, '[]'::jsonb, '[\"/\"]'::jsonb, \
         NULL, 'tester')",
    )
    .bind(Uuid::new_v4())
    .execute(&db.pool)
    .await;
    let err = result.expect_err("I-2: NULL owner_tenant_id on a custom role MUST be rejected");
    expect_db_error_with_sqlstate(err, "23514", "I-2: NULL owner_tenant_id on custom role");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i3_non_null_owner_tenant_id_for_built_in_is_rejected() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let owner_tenant_id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO role_definitions (id, name, is_built_in, permissions, not_permissions, \
         assignable_scopes, owner_tenant_id, created_by) \
         VALUES ($1, 'BuiltIn with owner', true, '[]'::jsonb, '[]'::jsonb, '[\"/\"]'::jsonb, \
         $2, 'system')",
    )
    .bind(Uuid::new_v4())
    .bind(owner_tenant_id)
    .execute(&db.pool)
    .await;
    let err = result.expect_err("I-3: non-NULL owner_tenant_id on a built-in MUST be rejected");
    expect_db_error_with_sqlstate(err, "23514", "I-3: non-NULL owner_tenant_id on built-in");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i5_fk_restrict_blocks_referenced_delete() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let role_id = Uuid::new_v4();
    common::insert_canonical_built_in_role(&db.pool, role_id, "BuiltinForI5").await?;
    sqlx::query(
        "INSERT INTO role_assignments (id, role_definition_id, principal_id, principal_type, \
         scope, scope_depth, tenant_id, created_by) \
         VALUES ($1, $2, 'principal-1', 'User', '/', 1, NULL, 'tester')",
    )
    .bind(Uuid::new_v4())
    .bind(role_id)
    .execute(&db.pool)
    .await?;

    let result = sqlx::query("DELETE FROM role_definitions WHERE id = $1")
        .bind(role_id)
        .execute(&db.pool)
        .await;
    let err = result.expect_err("I-5: ON DELETE RESTRICT MUST block deletion of a referenced role");
    // Both codes mean "the referenced row is still there"; which one arrives
    // depends on the server version, not on the constraint under test.
    expect_db_error_with_any_sqlstate(err, &["23503", "23001"], "I-5: FK RESTRICT");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i6_duplicate_custom_role_name_within_same_tenant_is_rejected() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let owner_tenant_id = Uuid::new_v4();
    common::insert_canonical_custom_role(&db.pool, Uuid::new_v4(), "Auditor", owner_tenant_id)
        .await?;

    let result =
        common::insert_canonical_custom_role(&db.pool, Uuid::new_v4(), "Auditor", owner_tenant_id)
            .await;
    let err = result.expect_err("I-6: duplicate (name, owner_tenant_id) MUST be rejected");
    let downcast = err
        .downcast::<sqlx::Error>()
        .expect("expected the underlying sqlx::Error");
    expect_db_error_with_sqlstate(
        downcast,
        "23505",
        "I-6: duplicate custom role name in same tenant",
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i7_duplicate_built_in_role_name_is_rejected() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    common::insert_canonical_built_in_role(&db.pool, Uuid::new_v4(), "Owner").await?;

    let result = common::insert_canonical_built_in_role(&db.pool, Uuid::new_v4(), "Owner").await;
    let err = result.expect_err("I-7: duplicate built-in name MUST be rejected");
    let downcast = err
        .downcast::<sqlx::Error>()
        .expect("expected the underlying sqlx::Error");
    expect_db_error_with_sqlstate(downcast, "23505", "I-7: duplicate built-in name");
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i8_same_custom_role_name_across_different_tenants_is_accepted() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();

    common::insert_canonical_custom_role(&db.pool, Uuid::new_v4(), "Auditor", tenant_a)
        .await
        .expect("first tenant Auditor insert should succeed");
    common::insert_canonical_custom_role(&db.pool, Uuid::new_v4(), "Auditor", tenant_b)
        .await
        .expect(
            "I-8: the partial unique index uq_role_name_per_tenant is keyed on \
             (name, owner_tenant_id), so the same name in a different tenant MUST be accepted",
        );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i9_duplicate_role_assignment_is_rejected() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    let role_id = Uuid::new_v4();
    common::insert_canonical_built_in_role(&db.pool, role_id, "BuiltinForI9").await?;
    sqlx::query(
        "INSERT INTO role_assignments (id, role_definition_id, principal_id, principal_type, \
         scope, scope_depth, tenant_id, created_by) \
         VALUES ($1, $2, 'principal-1', 'User', '/tenants/00000000-0000-0000-0000-000000000001', \
         2, '00000000-0000-0000-0000-000000000001'::uuid, 'tester')",
    )
    .bind(Uuid::new_v4())
    .bind(role_id)
    .execute(&db.pool)
    .await?;

    let result = sqlx::query(
        "INSERT INTO role_assignments (id, role_definition_id, principal_id, principal_type, \
         scope, scope_depth, tenant_id, created_by) \
         VALUES ($1, $2, 'principal-1', 'User', '/tenants/00000000-0000-0000-0000-000000000001', \
         2, '00000000-0000-0000-0000-000000000001'::uuid, 'tester')",
    )
    .bind(Uuid::new_v4())
    .bind(role_id)
    .execute(&db.pool)
    .await;
    let err = result.expect_err(
        "I-9: duplicate (role_definition_id, principal_type, principal_id, scope) MUST be rejected",
    );
    expect_db_error_with_sqlstate(err, "23505", "I-9: duplicate role assignment");
    Ok(())
}
