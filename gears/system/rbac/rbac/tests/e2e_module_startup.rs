//! End-to-end smoke verification for `RbacServiceGear`. Drives `init()`
//! the same way `toolkit`'s runtime does, then asserts schema, seeded
//! built-ins, `ClientHub` registration, and idempotency. An in-process
//! harness keeps `ClientHub` reachable from the test process.
//!
//! ```bash
//! cargo test -p cf-gears-rbac --test e2e_module_startup -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::panic, clippy::doc_markdown)]
// Test-only: bootstrap assertions read raw rows from `role_assignments`.
// SecureORM would require materialising the full entity stack in the
// test crate; raw sqlx is the pragmatic choice here.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

mod common;

use anyhow::{Context, Result};
use rbac::infra::seeder::BuiltinRoleSeeder;
use rbac::module::RbacServiceGear;
use rbac_sdk::api::RbacServiceClientV1;
use rbac_sdk::models::{GetSubjectRolesRequest, PrincipalType};
use toolkit::Gear;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::common::{
    ctx_missing_rg_client, ctx_missing_tenant_resolver, ctx_missing_types_registry,
    prepare_e2e_harness, prepare_e2e_harness_with_admin,
    prepare_e2e_harness_with_failing_types_registry, prepare_e2e_harness_with_grants,
    rebind_ctx_to_existing_db,
};
/// These tests assert against the full built-in roster, so they seed the
/// integration roles too (`Credstore Secret Operator`, `Usage Emitter`) —
/// the same choice a deployment running those gears makes in config.
const SEED_INTEGRATION_ROLES: bool = true;

/// Boots the module against PostgreSQL and asserts migrations ran, both
/// RBAC tables exist, and every canonical built-in role is seeded with
/// the foundation invariants.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn module_startup_seeds_canonical_built_ins() -> Result<()> {
    let harness = prepare_e2e_harness().await?;

    // --- Drive init() exactly the way toolkit's runtime does.
    let module = RbacServiceGear::default();
    module.init(&harness.ctx).await.expect(
        "RbacServiceGear::init() must succeed with all stubs registered and the DB pre-migrated",
    );

    // --- Assertion 1: both tables exist.
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name::text \
         FROM information_schema.tables \
         WHERE table_schema = 'public' \
           AND table_name IN ('role_definitions', 'role_assignments') \
         ORDER BY table_name",
    )
    .fetch_all(&harness.pool)
    .await?;
    let table_names: Vec<&str> = tables.iter().map(|(name,)| name.as_str()).collect();
    assert_eq!(
        table_names,
        vec!["role_assignments", "role_definitions"],
        "both RBAC tables MUST exist after init() returns"
    );

    // --- Assertion 2: one built-in row per canonical role is seeded. The
    //                  expected count derives from the catalog, so adding a
    //                  built-in role does not require editing this test.
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*)::bigint FROM role_definitions WHERE is_built_in = true")
            .fetch_one(&harness.pool)
            .await?;
    let expected = i64::try_from(BuiltinRoleSeeder::role_count(SEED_INTEGRATION_ROLES))
        .expect("roster size MUST fit in i64");
    assert_eq!(
        count, expected,
        "the canonical roster lists {expected} built-in roles; init() MUST seed all of them"
    );

    // --- Assertion 3: every expected name is present, and each row
    //                  carries the foundation invariants
    //                  (`is_built_in = true`, `owner_tenant_id IS NULL`,
    //                  `created_by = 'system'`).
    let rows: Vec<(String, bool, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT name::text, is_built_in, owner_tenant_id, created_by::text \
         FROM role_definitions \
         WHERE is_built_in = true \
         ORDER BY name",
    )
    .fetch_all(&harness.pool)
    .await?;
    let actual_names: Vec<&str> = rows.iter().map(|(name, _, _, _)| name.as_str()).collect();
    for expected in BuiltinRoleSeeder::role_names(SEED_INTEGRATION_ROLES) {
        assert!(
            actual_names.contains(&expected),
            "built-in role {expected} MUST be present after init(); actual: {actual_names:?}",
        );
    }
    for (name, is_built_in, owner_tenant_id, created_by) in &rows {
        assert!(
            *is_built_in,
            "built-in role {name} MUST have is_built_in = true (immutability boundary)",
        );
        assert!(
            owner_tenant_id.is_none(),
            "built-in role {name} MUST have owner_tenant_id IS NULL (immutability boundary); got {owner_tenant_id:?}",
        );
        assert_eq!(
            created_by, "system",
            "built-in role {name} MUST have created_by = 'system' (seeder invariant)",
        );
    }

    // --- Assertion 4: per-role `permissions` JSONB matches the canonical
    //                  roster (full DB round-trip; cast to text so sqlx
    //                  decodes without the `json` feature).
    let perm_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT name::text, permissions::text, assignable_scopes::text \
         FROM role_definitions \
         WHERE is_built_in = true \
         ORDER BY name",
    )
    .fetch_all(&harness.pool)
    .await?;
    for (name, permissions_str, assignable_scopes_str) in &perm_rows {
        let assignable_scopes: serde_json::Value = serde_json::from_str(assignable_scopes_str)
            .with_context(|| format!("failed to parse assignable_scopes JSON for role {name}"))?;
        let permissions: serde_json::Value = serde_json::from_str(permissions_str)
            .with_context(|| format!("failed to parse permissions JSON for role {name}"))?;

        // Every built-in role MUST have assignable_scopes = ["/"].
        assert_eq!(
            assignable_scopes,
            serde_json::json!(["/"]),
            "built-in role {name} MUST have assignable_scopes = [\"/\"]; \
             got {assignable_scopes:?}",
        );
        // permissions MUST be a non-empty JSON array for every built-in role.
        let empty_vec = vec![];
        let perms = permissions.as_array().unwrap_or(&empty_vec);
        assert!(
            !perms.is_empty(),
            "built-in role {name} MUST have at least one permission rule; \
             got empty permissions array",
        );
        // Every permission rule MUST have 'operation' and 'target_type' keys.
        for (i, rule) in perms.iter().enumerate() {
            assert!(
                rule.get("operation").is_some(),
                "built-in role {name} permission rule [{i}] MUST have 'operation' key",
            );
            assert!(
                rule.get("target_type").is_some(),
                "built-in role {name} permission rule [{i}] MUST have 'target_type' key",
            );
        }
    }

    Ok(())
}

/// After `init()`, `dyn RbacServiceClientV1` MUST be resolvable from
/// `ClientHub` and calls MUST reach the real evaluator.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn module_startup_registers_local_client_in_client_hub() -> Result<()> {
    let harness = prepare_e2e_harness().await?;

    let module = RbacServiceGear::default();
    module
        .init(&harness.ctx)
        .await
        .expect("init() must succeed in the harness");

    // --- Resolve `dyn RbacServiceClientV1` directly from ClientHub.
    let client = harness
        .client_hub
        .get::<dyn RbacServiceClientV1>()
        .expect("dyn RbacServiceClientV1 MUST be registered in ClientHub by init()");

    // --- Confirm the real evaluator is wired. The fake tenant resolver
    //     returns an empty ancestor chain, so the evaluator MUST return
    //     `Ok` with no roles for a synthetic subject that has no
    //     assignments. Accepting `Err(Internal { … })` here would let an
    //     unwired client pass whenever its message stayed generic.
    // The SDK trait requires a non-anonymous `SecurityContext`, and an
    // anonymous caller does not get past the adapter's authz gate, so the
    // probe uses a fresh tenant id with a matching tenant-scoped caller.
    let probe_tenant = Uuid::new_v4();
    let probe_ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(probe_tenant)
        .subject_type("service")
        .build()
        .expect("probe ctx must build");
    let request = GetSubjectRolesRequest::new(
        "e2e-subject",
        PrincipalType::User,
        rbac_sdk::models::Scope::tenant(probe_tenant),
        false,
    );
    let response = client
        .get_subject_roles(&probe_ctx, request)
        .await
        .expect("real evaluator must return Ok for a subject with no assignments");
    assert!(
        response.roles.is_empty(),
        "expected no roles for a synthetic subject; got {:?}",
        response.roles
    );

    Ok(())
}

/// Idempotent restart: a second `init()` against the same DB MUST
/// succeed without changing the seeded built-in count. Migrations are
/// not re-run (once-per-process); only the seeder runs again.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn module_startup_is_idempotent_across_restarts() -> Result<()> {
    let harness = prepare_e2e_harness().await?;

    // --- First startup: seeds the canonical catalog.
    let module_first = RbacServiceGear::default();
    module_first
        .init(&harness.ctx)
        .await
        .expect("first init() must succeed");

    let (count_first,): (i64,) =
        sqlx::query_as("SELECT count(*)::bigint FROM role_definitions WHERE is_built_in = true")
            .fetch_one(&harness.pool)
            .await?;
    let expected_first = i64::try_from(BuiltinRoleSeeder::role_count(SEED_INTEGRATION_ROLES))
        .expect("roster size MUST fit in i64");
    assert_eq!(
        count_first, expected_first,
        "first startup must seed all {expected_first} canonical built-in roles"
    );

    let (max_updated_first,): (chrono::DateTime<chrono::Utc>,) =
        sqlx::query_as("SELECT max(updated_at) FROM role_definitions WHERE is_built_in = true")
            .fetch_one(&harness.pool)
            .await?;

    // --- Second startup: fresh module + ctx, same DB/ClientHub/stubs.
    let ctx_second = rebind_ctx_to_existing_db(&harness);
    let module_second = RbacServiceGear::default();
    module_second
        .init(&ctx_second)
        .await
        .expect("second init() must succeed with no changes to the DB schema or seed catalog");

    // --- Assertion: count is unchanged.
    let (count_second,): (i64,) =
        sqlx::query_as("SELECT count(*)::bigint FROM role_definitions WHERE is_built_in = true")
            .fetch_one(&harness.pool)
            .await?;
    assert_eq!(
        count_second, count_first,
        "second startup MUST NOT add new built-in rows; got count {count_second} (first was {count_first})",
    );

    // --- Assertion: the built-in invariants stayed intact.
    let (drift_count,): (i64,) = sqlx::query_as(
        "SELECT count(*)::bigint FROM role_definitions \
         WHERE is_built_in = false OR owner_tenant_id IS NOT NULL",
    )
    .fetch_one(&harness.pool)
    .await?;
    assert_eq!(
        drift_count, 0,
        "no built-in row may have its is_built_in / owner_tenant_id invariants flipped \
         during a re-seed (immutability boundary)",
    );

    // --- Assertion: the seeder re-touched rows (UPSERT path, not
    //              no-op) — second `max(updated_at)` MUST not regress.
    let (max_updated_second,): (chrono::DateTime<chrono::Utc>,) =
        sqlx::query_as("SELECT max(updated_at) FROM role_definitions WHERE is_built_in = true")
            .fetch_one(&harness.pool)
            .await?;
    assert!(
        max_updated_second >= max_updated_first,
        "second startup's max(updated_at) MUST NOT regress \
         (first: {max_updated_first}, second: {max_updated_second})",
    );

    // --- ClientHub re-registration is also idempotent: re-registering
    //              an existing key overwrites the slot rather than panicking.
    let _client = harness
        .client_hub
        .get::<dyn RbacServiceClientV1>()
        .expect("dyn RbacServiceClientV1 MUST stay registered after a second init()");

    Ok(())
}

/// `TypesRegistryClient::register` returning `RegisterResult::Err`
/// propagates as an `anyhow::Error` whose message names the offending
/// `gts_id`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn init_propagates_types_registry_err_with_gts_id() -> Result<()> {
    let harness = prepare_e2e_harness_with_failing_types_registry().await?;

    let module = rbac::module::RbacServiceGear::default();
    let err = module
        .init(&harness.ctx)
        .await
        .expect_err("init() MUST fail when TypesRegistryClient returns RegisterResult::Err");

    let msg = err.to_string();
    assert!(
        msg.contains("gts.cf.core.rbac.role_assignment.v1~") || msg.contains("role_assignment"),
        "init() error MUST identify the failing schema's gts_id; got: {msg}",
    );
    assert!(
        msg.contains("types-registry") || msg.contains("register"),
        "init() error MUST mention types-registry registration; got: {msg}",
    );

    Ok(())
}

/// `init()` returns a descriptive error when `TenantResolverClient` is
/// absent from `ClientHub` (no Docker; uses SQLite).
#[tokio::test]
#[ignore = "grouped with E2E tests for consistency"]
async fn init_fails_with_descriptive_error_when_tenant_resolver_missing() -> Result<()> {
    let (ctx, _hub) = ctx_missing_tenant_resolver().await?;
    let module = rbac::module::RbacServiceGear::default();
    let err = module
        .init(&ctx)
        .await
        .expect_err("init() MUST fail when TenantResolverClient is absent from ClientHub");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("TenantResolverClient"),
        "error MUST name the missing contract; got: {msg}",
    );
    assert!(
        msg.contains("tenant-resolver"),
        "error MUST mention the tenant-resolver module; got: {msg}",
    );

    Ok(())
}

/// `init()` returns a descriptive error when `ResourceGroupReadHierarchy`
/// is absent from `ClientHub`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn init_fails_with_descriptive_error_when_rg_client_missing() -> Result<()> {
    let (ctx, _hub) = ctx_missing_rg_client().await?;
    let module = rbac::module::RbacServiceGear::default();
    let err = module
        .init(&ctx)
        .await
        .expect_err("init() MUST fail when ResourceGroupReadHierarchy is absent from ClientHub");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("ResourceGroupReadHierarchy"),
        "error MUST name the missing contract; got: {msg}",
    );
    assert!(
        msg.contains("resource-group"),
        "error MUST mention the resource-group module; got: {msg}",
    );

    Ok(())
}

/// `init()` returns a descriptive error when `TypesRegistryClient` is
/// absent from `ClientHub`.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn init_fails_with_descriptive_error_when_types_registry_missing() -> Result<()> {
    let (ctx, _hub) = ctx_missing_types_registry().await?;
    let module = rbac::module::RbacServiceGear::default();
    let err = module
        .init(&ctx)
        .await
        .expect_err("init() MUST fail when TypesRegistryClient is absent from ClientHub");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("TypesRegistryClient"),
        "error MUST name the missing contract; got: {msg}",
    );
    assert!(
        msg.contains("types-registry"),
        "error MUST mention the types-registry module; got: {msg}",
    );

    Ok(())
}

/// Bootstrap smoke: `init()` with `platform_admin_subject_id` creates
/// the Owner-at-`/` assignment.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn init_with_admin_subject_creates_owner_assignment() -> Result<()> {
    use rbac::infra::bootstrap::{OWNER_ROLE_ID, SYSTEM_BOOTSTRAP_CREATED_BY};
    use sqlx::Row;

    let subject = "user-e2e-admin";
    let harness = prepare_e2e_harness_with_admin(subject).await?;

    let module = RbacServiceGear::default();
    module
        .init(&harness.ctx)
        .await
        .expect("init() MUST succeed when platform_admin_subject_id is configured");

    let row = sqlx::query(
        "SELECT role_definition_id, principal_type, scope, created_by \
         FROM role_assignments WHERE principal_id = $1",
    )
    .bind(subject)
    .fetch_one(&harness.pool)
    .await
    .expect("8.1: a role_assignments row MUST exist for the configured subject after init()");

    let role_def_id: uuid::Uuid = row.get("role_definition_id");
    assert_eq!(
        role_def_id, OWNER_ROLE_ID,
        "8.1: role_definition_id MUST be the canonical Owner UUID"
    );
    assert_eq!(row.get::<String, _>("scope"), "/", "8.1: scope MUST be '/'");
    assert_eq!(
        row.get::<String, _>("principal_type"),
        "User",
        "8.1: principal_type MUST be 'User'"
    );
    assert_eq!(
        row.get::<String, _>("created_by"),
        SYSTEM_BOOTSTRAP_CREATED_BY,
        "8.1: created_by MUST be 'system-bootstrap'"
    );

    Ok(())
}

/// Bootstrap smoke: `init()` without `platform_admin_subject_id`
/// succeeds and writes no assignment row (soft-skip with warning log).
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn init_without_admin_subject_skips_bootstrap() -> Result<()> {
    use sqlx::Row;

    let harness = prepare_e2e_harness().await?;

    let module = RbacServiceGear::default();
    module
        .init(&harness.ctx)
        .await
        .expect("8.2: init() MUST succeed even when platform_admin_subject_id is unset");

    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM role_assignments WHERE created_by = 'system-bootstrap'",
    )
    .fetch_one(&harness.pool)
    .await?;
    let n: i64 = row.get("n");
    assert_eq!(
        n, 0,
        "8.2: no role_assignments rows with 'system-bootstrap' attribution \
         MUST exist when platform_admin_subject_id is unset"
    );

    Ok(())
}

/// After a successful `init()` with the `rbac` block present, the
/// `RbacRuntime` is committed.
///
/// One assertion covers every slot: `init()` is all-or-nothing (see
/// `runtime_is_populated`'s doc — it commits the complete runtime or returns
/// `Err`, so partial init is not representable). The `runtime` field is private
/// with no per-slot accessor, so an integration test genuinely cannot inspect
/// individual slots.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn runtime_populated_after_init_when_enabled() -> Result<()> {
    let harness = prepare_e2e_harness().await?;
    let module = rbac::module::RbacServiceGear::default();
    module.init(&harness.ctx).await?;
    assert!(
        module.runtime_is_populated(),
        "init() with the rbac block present MUST commit the full RbacRuntime \
         (role-definition + role-assignment + permissions ApiState, scope \
         validator, evaluator)"
    );
    Ok(())
}

/// Configured grants land as rows whose `principal_type` comes from the list
/// they were written in, and a multi-entry `builtin_role_targets.platform`
/// expands `Owner`'s single catalog rule into one rule per entry.
///
/// Both are only observable end to end: the type decides whether the evaluator
/// ever finds the grant, and the expansion decides whether `Owner` covers the
/// deployment's own vendor *and* RBAC's own `gts.cf.core.…` types.
#[tokio::test]
#[ignore = "needs Docker; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn init_writes_configured_grants_with_the_right_principal_type() -> Result<()> {
    use sqlx::Row;

    let user_subject = "user-e2e-reader";
    let service_subject = "svc-e2e-emitter";
    let harness = prepare_e2e_harness_with_grants(user_subject, service_subject).await?;

    let module = RbacServiceGear::default();
    module
        .init(&harness.ctx)
        .await
        .expect("init() MUST succeed with both grant lists configured");

    for (subject, expected_type) in [
        (user_subject, "User"),
        (service_subject, "ServicePrincipal"),
    ] {
        let row = sqlx::query(
            "SELECT principal_type, scope, scope_depth, tenant_id, created_by \
             FROM role_assignments WHERE principal_id = $1",
        )
        .bind(subject)
        .fetch_one(&harness.pool)
        .await
        .unwrap_or_else(|e| panic!("a grant row MUST exist for {subject}: {e}"));

        assert_eq!(
            row.get::<String, _>("principal_type"),
            expected_type,
            "the grant list decides principal_type, and a mismatch is a silent deny"
        );
        assert_eq!(row.get::<String, _>("scope"), "/");
        assert_eq!(row.get::<i32, _>("scope_depth"), 1);
        assert!(
            row.get::<Option<uuid::Uuid>, _>("tenant_id").is_none(),
            "a root-scope grant MUST leave tenant_id NULL"
        );
        assert_eq!(row.get::<String, _>("created_by"), "system-bootstrap");
    }

    // Re-running init() must not duplicate either grant.
    let ctx_second = rebind_ctx_to_existing_db(&harness);
    RbacServiceGear::default()
        .init(&ctx_second)
        .await
        .expect("second init() must succeed");
    let (grants,): (i64,) = sqlx::query_as(
        "SELECT count(*)::bigint FROM role_assignments WHERE principal_id = ANY($1)",
    )
    .bind(vec![user_subject.to_owned(), service_subject.to_owned()])
    .fetch_one(&harness.pool)
    .await?;
    assert_eq!(
        grants, 2,
        "configured grants MUST be idempotent across boots"
    );

    // Owner's one catalog rule became one rule per configured platform entry.
    let (owner_permissions,): (serde_json::Value,) =
        sqlx::query_as("SELECT permissions FROM role_definitions WHERE name = 'Owner'")
            .fetch_one(&harness.pool)
            .await?;
    let targets: Vec<&str> = owner_permissions
        .as_array()
        .expect("permissions MUST be a JSON array")
        .iter()
        .map(|rule| {
            rule.get("target_type")
                .and_then(serde_json::Value::as_str)
                .expect("every rule MUST carry target_type")
        })
        .collect();
    assert_eq!(
        targets,
        vec!["gts.cf.*", "gts.vendor.*"],
        "each platform entry MUST seed its own rule, in config order"
    );

    Ok(())
}
