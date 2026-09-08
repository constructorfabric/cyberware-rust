//! Gear-startup smoke for `RbacServiceGear` driven against in-memory
//! `SQLite` (no Docker), the non-`#[ignore]` sibling of the Postgres
//! `e2e_module_startup.rs`.
//!
//! Drives `Gear::init()` exactly the way the `toolkit` runtime does —
//! pre-migrated DB + all three upstream `ClientHub` stubs registered — then
//! asserts the seeder ran (built-ins present), the local client is
//! registered + reachable, the runtime is committed, and the
//! platform-admin bootstrap path runs when configured. Because it needs no
//! Docker it runs in the default `cargo test -p cf-gears-rbac` and counts toward the
//! gated coverage number (covers `module.rs`, `infra/seeder.rs`,
//! `infra/bootstrap.rs`).

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::client_hub::ClientHub;
use toolkit::config::ConfigProvider;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use rbac::infra::bootstrap::OWNER_ROLE_ID;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
// Bring the repo traits into scope (as `_`) so their methods resolve on the
// concrete structs below, without colliding with the same-named concrete types.
use rbac::domain::role_assignment_repo::{RoleAssignmentRepository as _, SubjectAssignmentsQuery};
use rbac::domain::role_definition_repo::RoleDefinitionRepository as _;
use rbac::infra::storage::role_assignment_repo::RoleAssignmentRepository;
use rbac::infra::storage::role_definition_repo::RoleDefinitionRepository;
use rbac::module::RbacServiceGear;
use rbac_sdk::api::RbacServiceClientV1;
use rbac_sdk::models::{GetSubjectRolesRequest, PrincipalType, Scope};
use tenant_resolver_sdk::api::TenantResolverClient;
use types_registry::TypesRegistryClient;

mod common;
use common::{
    InMemoryConfigProvider, StubResourceGroupClient, StubTenantResolverClient,
    StubTypesRegistryClient,
};

/// Assemble a `ClientHub` with all three upstream stubs registered.
fn stub_client_hub() -> Arc<ClientHub> {
    let client_hub = Arc::new(ClientHub::new());
    client_hub.register::<dyn ResourceGroupReadHierarchy>(Arc::new(StubResourceGroupClient));
    client_hub.register::<dyn TenantResolverClient>(Arc::new(StubTenantResolverClient));
    client_hub.register::<dyn TypesRegistryClient>(Arc::new(StubTypesRegistryClient));
    client_hub
}

/// Build a `GearCtx` over a fresh migrated in-memory SQLite DB. Returns
/// the ctx, the shared `ClientHub`, and a *clone* of the `DBProvider` so
/// the test can read rows the module wrote.
async fn sqlite_ctx(
    config: Arc<dyn ConfigProvider>,
) -> Result<(GearCtx, Arc<ClientHub>, DBProvider<DbError>)> {
    let provider = common::fresh_sqlite_provider().await?;
    let client_hub = stub_client_hub();
    let ctx = GearCtx::new(
        "rbac",
        Uuid::new_v4(),
        config,
        client_hub.clone(),
        CancellationToken::new(),
    )
    .with_db(provider.clone());
    Ok((ctx, client_hub, provider))
}

#[tokio::test]
async fn init_seeds_builtins_and_registers_local_client() -> Result<()> {
    let (ctx, client_hub, provider) =
        sqlite_ctx(Arc::new(InMemoryConfigProvider::rbac_enabled())).await?;

    let module = RbacServiceGear::default();
    module
        .init(&ctx)
        .await
        .expect("init() must succeed with all stubs registered and the DB pre-migrated");

    // Runtime committed (all ApiState slots + evaluator wired).
    assert!(
        module.runtime_is_populated(),
        "init() MUST commit the full RbacRuntime"
    );

    // Seeder ran: the canonical Owner built-in is present.
    let conn = provider.conn()?;
    let repo = RoleDefinitionRepository;
    let owner = repo
        .find_by_id(&conn, OWNER_ROLE_ID)
        .await
        .expect("find_by_id must succeed")
        .expect("Owner built-in MUST be seeded by init()");
    assert!(owner.is_built_in, "Owner row MUST carry is_built_in = true");

    // Local client registered + reaches the real evaluator.
    let client = client_hub
        .get::<dyn RbacServiceClientV1>()
        .expect("dyn RbacServiceClientV1 MUST be registered in ClientHub by init()");
    let probe_tenant = Uuid::new_v4();
    let probe_ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(probe_tenant)
        .subject_type("service")
        .build()
        .expect("probe ctx must build");
    let request = GetSubjectRolesRequest::new(
        "sqlite-e2e-subject",
        PrincipalType::User,
        Scope::tenant(probe_tenant),
        false,
    );
    let response = client
        .get_subject_roles(&probe_ctx, request)
        .await
        .expect("evaluator must return Ok for a subject with no assignments");
    assert!(
        response.roles.is_empty(),
        "synthetic subject MUST have no roles; got {:?}",
        response.roles
    );

    Ok(())
}

#[tokio::test]
async fn init_is_idempotent_across_restarts() -> Result<()> {
    // First init seeds.
    let provider = common::fresh_sqlite_provider().await?;
    let client_hub = stub_client_hub();
    let mk_ctx = |hub: Arc<ClientHub>, db: DBProvider<DbError>| {
        GearCtx::new(
            "rbac",
            Uuid::new_v4(),
            Arc::new(InMemoryConfigProvider::rbac_enabled()) as Arc<dyn ConfigProvider>,
            hub,
            CancellationToken::new(),
        )
        .with_db(db)
    };

    let module_first = RbacServiceGear::default();
    module_first
        .init(&mk_ctx(client_hub.clone(), provider.clone()))
        .await
        .expect("first init() must succeed");

    // Second init against the same DB + hub: re-seed must not fail.
    let module_second = RbacServiceGear::default();
    module_second
        .init(&mk_ctx(client_hub.clone(), provider.clone()))
        .await
        .expect("second init() must succeed (idempotent re-seed + re-register)");

    // Owner built-in still present and unique.
    let conn = provider.conn()?;
    let repo = RoleDefinitionRepository;
    assert!(
        repo.find_by_id(&conn, OWNER_ROLE_ID)
            .await
            .expect("find_by_id")
            .is_some(),
        "Owner built-in MUST remain after a second init()"
    );
    Ok(())
}

#[tokio::test]
async fn init_with_admin_subject_runs_bootstrap() -> Result<()> {
    let subject = "sqlite-admin-subject";
    let (ctx, _hub, provider) = sqlite_ctx(Arc::new(
        InMemoryConfigProvider::rbac_enabled_with_admin(subject),
    ))
    .await?;

    let module = RbacServiceGear::default();
    module
        .init(&ctx)
        .await
        .expect("init() MUST succeed when platform_admin_subject_id is configured");
    assert!(module.runtime_is_populated());

    // Bootstrap wrote the Owner-at-root assignment for the admin subject.
    // Read it back through the assignment repo's evaluator-facing query.
    let conn = provider.conn()?;
    let assignment_repo = RoleAssignmentRepository;
    let rows = assignment_repo
        .get_subject_assignments(
            &conn,
            SubjectAssignmentsQuery {
                user_principal: Some((PrincipalType::User, subject.to_owned())),
                group_principals: Vec::new(),
                ancestor_scopes: Vec::new(),
                context_tenant_rg_prefix: String::new(),
                all_scopes: true,
            },
        )
        .await
        .expect("get_subject_assignments must succeed");
    assert!(
        rows.iter()
            .any(|r| r.role_definition_id == OWNER_ROLE_ID && r.scope == Scope::Root),
        "bootstrap MUST create an Owner-at-root assignment for the admin subject; got {rows:?}"
    );
    Ok(())
}

#[tokio::test]
async fn init_without_admin_subject_skips_bootstrap() -> Result<()> {
    let (ctx, _hub, provider) =
        sqlite_ctx(Arc::new(InMemoryConfigProvider::rbac_enabled())).await?;

    let module = RbacServiceGear::default();
    module
        .init(&ctx)
        .await
        .expect("init() MUST succeed even when platform_admin_subject_id is unset");

    // No bootstrap assignment was written.
    let conn = provider.conn()?;
    let assignment_repo = RoleAssignmentRepository;
    let rows = assignment_repo
        .get_subject_assignments(
            &conn,
            SubjectAssignmentsQuery {
                user_principal: Some((PrincipalType::User, "sqlite-admin-subject".to_owned())),
                group_principals: Vec::new(),
                ancestor_scopes: Vec::new(),
                context_tenant_rg_prefix: String::new(),
                all_scopes: true,
            },
        )
        .await
        .expect("get_subject_assignments must succeed");
    assert!(
        rows.is_empty(),
        "no bootstrap assignment MUST exist when platform_admin_subject_id is unset; got {rows:?}"
    );
    Ok(())
}
