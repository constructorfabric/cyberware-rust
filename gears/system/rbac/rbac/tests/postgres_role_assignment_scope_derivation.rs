//! Integration tests for application-side derivation of `scope_depth` and
//! `tenant_id` on `role_assignments`:
//!
//! 1. Inserting via the repo lands `scope_depth = Scope::depth()` for each
//!    of the three canonical scope shapes (root / tenant / resource-group).
//! 2. Inserting via the repo lands `tenant_id = Scope::tenant_id()` for the
//!    same three shapes.
//! 3. `WHERE tenant_id = $1` picks `idx_role_assignments_tenant_id` (the
//!    partial B-tree index from `m_002`) — the evaluator's index pivot for
//!    tenant-prefix queries.

// Probes read back DB state and inspect plans via raw sqlx; not expressible
// through SecureORM, so DE0706 is silenced here.
#![cfg(test)]
#![allow(clippy::expect_used)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use anyhow::Result;
use serde_json::Value as JsonValue;
use sqlx::Row;
use toolkit_db::{ConnectOpts, connect_db};
use uuid::Uuid;

use rbac::domain::role_assignment_repo::{NewRoleAssignment, RoleAssignmentRepository};
use rbac::infra::storage::role_assignment_repo;
use rbac_sdk::models::{PrincipalType, Scope};

/// Walk an `EXPLAIN (FORMAT JSON)` tree looking for an `Index Name`
/// node matching `expected_index` (mirrors `postgres_indexes.rs`).
fn plan_uses_index(plan: &JsonValue, expected_index: &str) -> bool {
    fn walk(node: &JsonValue, expected: &str) -> bool {
        if let Some(name) = node.get("Index Name").and_then(JsonValue::as_str)
            && name == expected
        {
            return true;
        }
        if let Some(children) = node.get("Plans").and_then(JsonValue::as_array)
            && children.iter().any(|child| walk(child, expected))
        {
            return true;
        }
        false
    }
    if let Some(top) = plan.as_array().and_then(|a| a.first())
        && let Some(plan_node) = top.get("Plan")
    {
        return walk(plan_node, expected_index);
    }
    false
}

async fn fresh_repo_with_role() -> Result<(
    role_assignment_repo::RoleAssignmentRepository,
    toolkit_db::DBProvider<toolkit_db::DbError>,
    common::PostgresUnderTest,
    Uuid,
)> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let role_id = Uuid::now_v7();
    common::insert_canonical_built_in_role(&fixture.pool, role_id, "BuiltinForScopeDerivation")
        .await?;
    let db = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider = toolkit_db::DBProvider::<toolkit_db::DbError>::new(db);
    Ok((
        role_assignment_repo::RoleAssignmentRepository,
        provider,
        fixture,
        role_id,
    ))
}

fn new_input(role: Uuid, principal_id: &str, scope: Scope) -> NewRoleAssignment {
    NewRoleAssignment {
        role_definition_id: role,
        principal_id: principal_id.to_owned(),
        principal_type: PrincipalType::User,
        scope,
        created_by: "tester".to_owned(),
        // These cases assert the scope_depth / tenant_id derivation, not
        // the author identity.
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

/// Fixed UUIDs used across the scope-derivation cases below.
const TENANT_A: Uuid = Uuid::from_u128(0x0195_f2b6_aaaa_4000_8000_0000_0000_0001_u128);
const RG_A: Uuid = Uuid::from_u128(0x0195_f2b6_cccc_4000_8000_0000_0000_0003_u128);

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn root_scope_writes_depth_1_and_null_tenant_id() -> Result<()> {
    let (repo, provider, fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let assignment = repo
        .create(&conn, new_input(role, "principal-root", Scope::root()))
        .await
        .expect("create root assignment");

    let row = sqlx::query("SELECT scope_depth, tenant_id FROM role_assignments WHERE id = $1")
        .bind(assignment.id)
        .fetch_one(&fixture.pool)
        .await?;
    let depth: i32 = row.get("scope_depth");
    let tenant_id: Option<Uuid> = row.get("tenant_id");

    assert_eq!(
        depth, 1,
        "root scope MUST land scope_depth = 1 (Scope::root().depth())",
    );
    assert!(
        tenant_id.is_none(),
        "root scope MUST land tenant_id = NULL so it is excluded from any WHERE tenant_id = $1 query",
    );
    Ok(())
}

/// Tenant scope `/tenants/T` → repo writes `scope_depth = 2`, `tenant_id = T`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn tenant_scope_writes_depth_2_and_tenant_uuid() -> Result<()> {
    let (repo, provider, fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let assignment = repo
        .create(
            &conn,
            new_input(role, "principal-tenant", Scope::tenant(TENANT_A)),
        )
        .await
        .expect("create tenant assignment");

    let row = sqlx::query("SELECT scope_depth, tenant_id FROM role_assignments WHERE id = $1")
        .bind(assignment.id)
        .fetch_one(&fixture.pool)
        .await?;
    let depth: i32 = row.get("scope_depth");
    let tenant_id: Option<Uuid> = row.get("tenant_id");

    assert_eq!(
        depth, 2,
        "tenant scope MUST land scope_depth = 2 (Scope::tenant(T).depth())",
    );
    assert_eq!(
        tenant_id,
        Some(TENANT_A),
        "tenant scope MUST land tenant_id = T (Scope::tenant(T).tenant_id())",
    );
    Ok(())
}

/// Resource-group scope `/tenants/T/resourceGroups/RG` → repo writes
/// `scope_depth = 4`, `tenant_id = T` (the parent tenant, NOT `RG`).
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn resource_group_scope_writes_depth_4_and_parent_tenant_uuid() -> Result<()> {
    let (repo, provider, fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;
    let assignment = repo
        .create(
            &conn,
            new_input(role, "principal-rg", Scope::resource_group(TENANT_A, RG_A)),
        )
        .await
        .expect("create RG assignment");

    let row = sqlx::query("SELECT scope_depth, tenant_id FROM role_assignments WHERE id = $1")
        .bind(assignment.id)
        .fetch_one(&fixture.pool)
        .await?;
    let depth: i32 = row.get("scope_depth");
    let tenant_id: Option<Uuid> = row.get("tenant_id");

    assert_eq!(
        depth, 4,
        "RG scope MUST land scope_depth = 4 (Scope::resource_group(T, RG).depth())",
    );
    assert_eq!(
        tenant_id,
        Some(TENANT_A),
        "RG scope MUST land tenant_id = T (the parent tenant) \u{2014} RG-scoped rows must still pivot \
         through the tenant index",
    );
    Ok(())
}

/// `WHERE tenant_id = $1` picks `idx_role_assignments_tenant_id`. Uses
/// `enable_seqscan = off` to test that the index CAN be used. Guards
/// against a regression where the index is accidentally dropped or the
/// `tenant_id` column changes shape in a way that defeats the planner.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn tenant_id_equality_lookup_uses_index() -> Result<()> {
    let (repo, provider, fixture, role) = fresh_repo_with_role().await?;
    let conn = provider.conn()?;

    // Seed enough rows for the planner to have stats.
    for i in 0..256_u32 {
        let scope = Scope::resource_group(TENANT_A, Uuid::new_v4());
        repo.create(&conn, new_input(role, &format!("user-{i}"), scope))
            .await
            .expect("seed RG assignment");
    }
    sqlx::query("ANALYZE role_assignments")
        .execute(&fixture.pool)
        .await?;

    let mut tx = fixture.pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await?;
    // sqlx 0.9 accepts only `&'static str`; asserted safe because the only interpolated
    // value is the `TENANT_A` compile-time `Uuid` constant declared in this test.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "EXPLAIN (FORMAT JSON) SELECT id FROM role_assignments WHERE tenant_id = '{TENANT_A}'"
    )))
    .fetch_one(&mut *tx)
    .await?;
    let plan: JsonValue = row.get(0);
    tx.rollback().await?;

    assert!(
        plan_uses_index(&plan, "idx_role_assignments_tenant_id"),
        "WHERE tenant_id = $1 MUST use idx_role_assignments_tenant_id (the partial B-tree index \
         from m_002). Plan: {plan}",
    );
    Ok(())
}
