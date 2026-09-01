//! EXPLAIN-asserted index integration tests — each test seeds ~256 rows,
//! runs `ANALYZE`, forces `enable_seqscan = off`, then asserts the
//! expected index appears anywhere in the `EXPLAIN (FORMAT JSON)` tree.
//! Verifies the index CAN be used (independent of planner heuristics).

// Planner introspection (`SET LOCAL`, EXPLAIN) needs raw sqlx with no
// SecureORM equivalent, so DE0706 is silenced at file scope.
#![cfg(test)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use anyhow::Result;
use serde_json::Value as JsonValue;
use sqlx::Row;
use uuid::Uuid;

/// Run `EXPLAIN (FORMAT JSON) <query>` with `enable_seqscan = off` and
/// return the parsed plan. `SET LOCAL` requires an explicit transaction.
async fn explain_json(pool: &sqlx::PgPool, query: &str) -> Result<JsonValue> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await?;
    // sqlx 0.9 accepts only `&'static str`; this EXPLAIN wrapper is asserted safe because
    // every caller in this test passes a hard-coded query literal — no external input
    // reaches the formatted string.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "EXPLAIN (FORMAT JSON) {query}"
    )))
    .fetch_one(&mut *tx)
    .await?;
    let plan: JsonValue = row.get(0);
    tx.rollback().await?;
    Ok(plan)
}

/// Walk the plan tree for any node whose `Index Name` matches.
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

/// Insert `count` synthetic role definitions and run `ANALYZE`.
async fn seed_synthetic_role_definitions(pool: &sqlx::PgPool, count: usize) -> Result<()> {
    let owner_tenant_id = Uuid::new_v4();
    for i in 0..count {
        let permissions = serde_json::json!([
            { "operation": "read", "target_type": format!("gts.test.t{i}.v1~") }
        ]);
        let scopes = serde_json::json!([format!("/tenants/synth-{i}")]);
        sqlx::query(
            "INSERT INTO role_definitions (id, name, is_built_in, permissions, not_permissions, \
             assignable_scopes, owner_tenant_id, created_by) \
             VALUES ($1, $2, false, $3::jsonb, '[]'::jsonb, $4::jsonb, $5, 'synth')",
        )
        .bind(Uuid::new_v4())
        .bind(format!("admin-row-{i}"))
        .bind(permissions.to_string())
        .bind(scopes.to_string())
        .bind(owner_tenant_id)
        .execute(pool)
        .await?;
    }
    sqlx::query("ANALYZE role_definitions")
        .execute(pool)
        .await?;
    Ok(())
}

async fn seed_synthetic_role_assignments(pool: &sqlx::PgPool, count: usize) -> Result<()> {
    let role_id = Uuid::new_v4();
    common::insert_canonical_built_in_role(pool, role_id, "BuiltinForExplain").await?;
    for i in 0..count {
        let scope =
            format!("/tenants/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/resourceGroups/rg-{i:04}");
        // Path shape `/tenants/{T}/resourceGroups/{...}` → depth=4,
        // tenant_id=T. Raw INSERTs must supply both columns.
        sqlx::query(
            "INSERT INTO role_assignments (id, role_definition_id, principal_id, principal_type, \
             scope, scope_depth, tenant_id, created_by) \
             VALUES ($1, $2, $3, 'User', $4, 4, \
             'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee'::uuid, 'synth')",
        )
        .bind(Uuid::new_v4())
        .bind(role_id)
        .bind(format!("user-{i}"))
        .bind(scope)
        .execute(pool)
        .await?;
    }
    sqlx::query("ANALYZE role_assignments")
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i15_trigram_name_search_uses_gin_index() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    seed_synthetic_role_definitions(&db.pool, 256).await?;
    let plan = explain_json(
        &db.pool,
        "SELECT id FROM role_definitions WHERE name ILIKE '%admin%'",
    )
    .await?;
    assert!(
        plan_uses_index(&plan, "idx_role_definitions_name"),
        "I-15: trigram name search MUST use idx_role_definitions_name (the gin_trgm_ops GIN \
         index from the RBAC initial migration). Plan: {plan}",
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i16_scope_prefix_lookup_uses_text_pattern_ops_index() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    seed_synthetic_role_assignments(&db.pool, 256).await?;
    let plan = explain_json(
        &db.pool,
        "SELECT id FROM role_assignments WHERE scope LIKE '/tenants/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/%'",
    )
    .await?;
    assert!(
        plan_uses_index(&plan, "idx_role_assignments_scope_prefix"),
        "I-16: scope prefix lookup MUST use idx_role_assignments_scope_prefix (the \
         B-tree index from the RBAC initial migration). Plan: {plan}",
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i17_deepest_first_ordering_uses_scope_depth_index() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    seed_synthetic_role_assignments(&db.pool, 256).await?;
    let plan = explain_json(
        &db.pool,
        "SELECT id FROM role_assignments ORDER BY scope_depth DESC, id DESC LIMIT 100",
    )
    .await?;
    assert!(
        plan_uses_index(&plan, "idx_role_assignments_scope_depth"),
        "I-17: deepest-first ordering MUST use idx_role_assignments_scope_depth (the \
         (scope_depth DESC, id DESC) B-tree index from the RBAC initial migration backing the §3.2 hot path). \
         Plan: {plan}",
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i18_jsonb_containment_on_permissions_uses_gin_index() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    seed_synthetic_role_definitions(&db.pool, 256).await?;
    let plan = explain_json(
        &db.pool,
        "SELECT id FROM role_definitions WHERE permissions @> \
         '[{\"operation\":\"read\",\"target_type\":\"gts.test.t100.v1~\"}]'::jsonb",
    )
    .await?;
    assert!(
        plan_uses_index(&plan, "idx_role_definitions_permissions"),
        "I-18: JSONB containment on permissions MUST use idx_role_definitions_permissions \
         (the GIN index from the RBAC initial migration). Plan: {plan}",
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn i19_jsonb_containment_on_assignable_scopes_uses_gin_index() -> Result<()> {
    let db = common::bring_up_migrated_postgres().await?;
    seed_synthetic_role_definitions(&db.pool, 256).await?;
    let plan = explain_json(
        &db.pool,
        "SELECT id FROM role_definitions WHERE assignable_scopes @> '[\"/tenants/synth-100\"]'::jsonb",
    )
    .await?;
    assert!(
        plan_uses_index(&plan, "idx_role_definitions_assignable_scopes"),
        "I-19: JSONB containment on assignable_scopes MUST use \
         idx_role_definitions_assignable_scopes (the GIN index from the RBAC initial migration). Plan: {plan}",
    );
    Ok(())
}
