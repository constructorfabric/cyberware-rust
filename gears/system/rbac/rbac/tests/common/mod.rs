//! Shared test fixtures for the PostgreSQL integration tests, the in-process
//! E2E module-startup harness, and the scope-validator integration test.
//!
//! Postgres-backed tests are `#[ignore]` by default; run them via
//! `cargo test -p cf-gears-rbac -- --ignored`.

// Shared test fixtures use raw `sqlx::PgPool` for direct SQL during seeding
// / tamper / introspection.
#![allow(clippy::expect_used, clippy::panic, clippy::doc_markdown, dead_code)]
#![allow(unknown_lints, de0706_no_direct_sqlx)]

/// Scope-validator test fakes for integration tests.
pub mod scope_fakes;

/// Build a `SecurityContext` for integration tests that hit the REST router.
/// The `["*"]` token scope flags the caller as a platform root so tests that
/// create artefacts in arbitrary tenants are not rejected by
/// `caller_scope_from_context`.
#[must_use]
fn test_security_context() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid::uuid!("11111111-2222-3333-4444-555555555555"))
        .subject_tenant_id(uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"))
        .subject_type("gts.cf.core.security.subject_user.v1~")
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("test SecurityContext must build")
}

/// Wrap an `axum::Router` in an `Extension<SecurityContext>` layer so
/// requests dispatched via `tower::ServiceExt::oneshot` arrive with a real
/// authenticated identity (matches the `require_authenticated` guard).
pub fn with_test_security_context(router: axum::Router) -> axum::Router {
    router.layer(axum::Extension(test_security_context()))
}

/// Build a `SecurityContext` for a non-root caller bound to `tenant`.
/// Omits the `["*"]` token scope so `is_first_party_root` returns
/// `false`; use this when a test needs to exercise the
/// `caller_scope_from_context` branch that rejects cross-tenant writes.
#[must_use]
pub fn non_root_security_context(tenant: uuid::Uuid) -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid::uuid!("11111111-2222-3333-4444-555555555555"))
        .subject_tenant_id(tenant)
        .subject_type("gts.cf.core.security.subject_user.v1~")
        // No `"*"` token scope on purpose — flags the caller as a
        // tenant-bound non-root caller for `is_first_party_root`.
        .token_scopes(vec!["rbac:read".to_owned(), "rbac:write".to_owned()])
        .build()
        .expect("non-root SecurityContext must build")
}

/// Wrap an `axum::Router` with a non-root `SecurityContext` bound to
/// `tenant`. Sister of `with_test_security_context` for the
/// cross-tenant authorisation regression tests.
pub fn with_non_root_security_context(router: axum::Router, tenant: uuid::Uuid) -> axum::Router {
    router.layer(axum::Extension(non_root_security_context(tenant)))
}

/// Read a non-2xx response, parse it as an RFC 9457 `application/problem+json`
/// document, and assert the error contract from `docs/DESIGN.md`: HTTP status
/// matches, the content-type is `application/problem+json`, the body's `status`
/// field equals the HTTP status, and the `type` URI contains
/// `expected_type_slug`.
///
/// The returned `serde_json::Value` is the parsed Problem body so callers
/// can chain variant-specific assertions (`context.field_violations[0].field`,
/// `context.violations[0].type`, `context.resource_type`, etc.) without
/// re-parsing.
///
/// `expected_type_slug` is matched with `str::contains` so callers can pin
/// either the canonical category emitted today (e.g. `"invalid_argument"`,
/// `"not_found"`, `"permission_denied"`, `"already_exists"`,
/// `"failed_precondition"`, `"unauthenticated"`) or, once the mapper switches
/// to per-error slugs, the slug from spec.md (e.g. `"error_role_definition_not_found"`).
pub async fn assert_problem(
    response: axum::response::Response,
    expected_status: axum::http::StatusCode,
    expected_type_slug: &str,
) -> JsonValue {
    use axum::body::to_bytes;
    use axum::http::header;

    let actual_status = response.status();
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_default();
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("read problem body");
    let value: JsonValue = serde_json::from_slice(&bytes).expect("problem body must be JSON");

    assert_eq!(
        actual_status, expected_status,
        "unexpected HTTP status; body={value}"
    );
    assert!(
        ct.contains("application/problem+json"),
        "expected application/problem+json content-type, got '{ct}'; body={value}"
    );
    let ty = value
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| panic!("Problem `type` must be a string; body={value}"));
    assert!(
        ty.contains(expected_type_slug),
        "Problem `type` MUST contain '{expected_type_slug}', got '{ty}'; body={value}"
    );
    let status_field = value
        .get("status")
        .and_then(JsonValue::as_u64)
        .unwrap_or_else(|| panic!("Problem `status` must be u64; body={value}"));
    assert_eq!(
        u16::try_from(status_field).expect("status fits u16"),
        expected_status.as_u16(),
        "Problem `status` MUST equal HTTP status; body={value}"
    );
    value
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use serde_json::{Value as JsonValue, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio_util::sync::CancellationToken;
use toolkit::GearCtx;
use toolkit::client_hub::ClientHub;
use toolkit::config::ConfigProvider;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use resource_group_sdk::api::ResourceGroupReadHierarchy;
use resource_group_sdk::models::{ResourceGroup, ResourceGroupMembership, ResourceGroupWithDepth};
use tenant_resolver_sdk::api::TenantResolverClient;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo,
};
use toolkit_canonical_errors::CanonicalError;
use types_registry::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};

use rbac::infra::storage::migrations::Migrator;

/// Wraps an active testcontainers Postgres container. Drop tears it down.
pub struct PostgresUnderTest {
    /// `postgres://` URL pointing at the ephemeral instance.
    pub url: String,
    /// `sqlx` pool — for raw SQL (`information_schema`, `EXPLAIN`, etc.).
    pub pool: PgPool,
    /// SeaORM connection — for entity-level code paths (seeder, upserts).
    pub sea: DatabaseConnection,
    /// Hold the container; test bodies may drop early to test reconnect.
    pub _container: ContainerAsync<Postgres>,
}

/// Bring up an ephemeral PostgreSQL container, run all RBAC migrations,
/// and return a fully-initialised fixture.
pub async fn bring_up_migrated_postgres() -> Result<PostgresUnderTest> {
    // Image tag comes from `test-containers`, the workspace's single source of
    // truth (docs/TESTING.md 4.4). `Postgres::default()` would take its tag
    // from `testcontainers-modules` instead, re-pinning the database through
    // `Cargo.lock` where no diff shows it.
    let postgres = test_containers::postgres()
        .with_env_var("POSTGRES_PASSWORD", "rbac_test_password")
        .with_env_var("POSTGRES_USER", "rbac_test_user")
        .with_env_var("POSTGRES_DB", "rbac_test_db");
    let container = postgres
        .start()
        .await
        .context("failed to start ephemeral PostgreSQL container")?;
    // Resolve the host from testcontainers rather than hard-coding 127.0.0.1:
    // under a remote/DinD Docker daemon (e.g. CI with `services: docker`) the
    // published port lives on the daemon host, not the test process loopback.
    // `get_host()` honours `DOCKER_HOST` / `TESTCONTAINERS_HOST_OVERRIDE`.
    let host = container
        .get_host()
        .await
        .context("failed to resolve Postgres container host")?;
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .context("failed to read mapped Postgres port")?;
    let url = format!("postgres://rbac_test_user:rbac_test_password@{host}:{port}/rbac_test_db");

    // sqlx for raw SQL, SeaORM for entity-level reads.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(20))
        .connect(&url)
        .await
        .context("failed to open sqlx PgPool against the test container")?;
    let sea = Database::connect(&url)
        .await
        .context("failed to open SeaORM connection against the test container")?;

    Migrator::up(&sea, None)
        .await
        .context("RBAC migrator::up(...) failed against the ephemeral Postgres")?;

    Ok(PostgresUnderTest {
        url,
        pool,
        sea,
        _container: container,
    })
}

/// Insert a minimal valid built-in role for tests that need an existing
/// `role_definitions` row (FK behaviour, assignment-level constraints).
/// Matches the seeder's invariants — `is_built_in = true`,
/// `owner_tenant_id = NULL`, `created_by = "system"`.
pub async fn insert_canonical_built_in_role(
    pool: &PgPool,
    id: uuid::Uuid,
    name: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO role_definitions (id, name, is_built_in, permissions, not_permissions, \
         assignable_scopes, owner_tenant_id, created_by) \
         VALUES ($1, $2, true, '[]'::jsonb, '[]'::jsonb, '[\"/\"]'::jsonb, NULL, 'system')",
    )
    .bind(id)
    .bind(name)
    .execute(pool)
    .await
    .with_context(|| format!("failed to seed test built-in role {name}"))?;
    Ok(())
}

/// Insert a minimal valid custom role definition for FK / uniqueness scenarios.
pub async fn insert_canonical_custom_role(
    pool: &PgPool,
    id: uuid::Uuid,
    name: &str,
    owner_tenant_id: uuid::Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO role_definitions (id, name, is_built_in, permissions, not_permissions, \
         assignable_scopes, owner_tenant_id, created_by) \
         VALUES ($1, $2, false, '[]'::jsonb, '[]'::jsonb, '[\"/\"]'::jsonb, $3, 'tester')",
    )
    .bind(id)
    .bind(name)
    .bind(owner_tenant_id)
    .execute(pool)
    .await
    .with_context(|| format!("failed to seed test custom role {name}"))?;
    Ok(())
}

// In-process E2E module-startup harness: builds a real `toolkit` `init()`
// driver inside the test process so tests can observe `ClientHub`
// registrations the production server hides.

/// Emit a complete `#[async_trait] impl Trait for Stub { ... }` whose
/// methods all panic with a uniform "stub-was-called-from-init()" message.
/// `custom = { ... }` is an escape hatch for methods that return an error
/// instead of panicking (e.g. `TenantResolverClient::get_ancestors` for
/// the evaluator-error path).
macro_rules! panic_in_init {
    (
        impl $trait_path:path => for $stub_ty:ident,
        stub_label = $stub:literal,
        methods = [
            $(
                async fn $name:ident ( $( $arg:ident : $arg_ty:ty ),* $(,)? )
                    -> $ret:ty ;
            )*
        ]
        $(, custom = { $($custom:tt)* } )?
        $(,)?
    ) => {
        #[async_trait]
        impl $trait_path for $stub_ty {
            $(
                async fn $name(&self, $( $arg : $arg_ty ),*) -> $ret {
                    panic!(concat!(
                        $stub, ": init() must not call ", stringify!($name)
                    ))
                }
            )*
            $( $($custom)* )?
        }
    };
}

/// Stub for the resource-group module's `ClientHub` contribution — the
/// narrow, PEP-bypassing `dyn ResourceGroupReadHierarchy` read contract
/// RBAC's `init()` resolves. `init()` only resolves the trait
/// object, so every method panics if reached.
pub struct StubResourceGroupClient;

panic_in_init! {
    impl ResourceGroupReadHierarchy => for StubResourceGroupClient,
    stub_label = "StubResourceGroupClient",
    methods = [
        async fn get_group_descendants(_ctx: &SecurityContext, _group_id: Uuid, _query: &ODataQuery)
            -> std::result::Result<Page<ResourceGroupWithDepth>, CanonicalError>;
        async fn get_group_ancestors(_ctx: &SecurityContext, _group_id: Uuid, _query: &ODataQuery)
            -> std::result::Result<Page<ResourceGroupWithDepth>, CanonicalError>;
        async fn list_groups(_ctx: &SecurityContext, _query: &ODataQuery)
            -> std::result::Result<Page<ResourceGroup>, CanonicalError>;
        async fn get_group(_ctx: &SecurityContext, _id: Uuid)
            -> std::result::Result<ResourceGroup, CanonicalError>;
        async fn list_memberships(_ctx: &SecurityContext, _query: &ODataQuery)
            -> std::result::Result<Page<ResourceGroupMembership>, CanonicalError>;
    ]
}

/// Stubbed `dyn TenantResolverClient`. `init()` only resolves the trait
/// object via `ClientHub::get`; it does NOT invoke any method.
pub struct StubTenantResolverClient;

panic_in_init! {
    impl TenantResolverClient => for StubTenantResolverClient,
    stub_label = "StubTenantResolverClient",
    methods = [
        async fn get_tenant(_ctx: &SecurityContext, _id: TenantId)
            -> std::result::Result<TenantInfo, TenantResolverError>;
        async fn get_root_tenant(_ctx: &SecurityContext)
            -> std::result::Result<TenantInfo, TenantResolverError>;
        async fn get_tenants(_ctx: &SecurityContext, _ids: &[TenantId], _options: &GetTenantsOptions)
            -> std::result::Result<Vec<TenantInfo>, TenantResolverError>;
        async fn get_descendants(_ctx: &SecurityContext, _id: TenantId, _options: &GetDescendantsOptions)
            -> std::result::Result<GetDescendantsResponse, TenantResolverError>;
        async fn is_ancestor(_ctx: &SecurityContext, _ancestor_id: TenantId, _descendant_id: TenantId, _options: &IsAncestorOptions)
            -> std::result::Result<bool, TenantResolverError>;
    ],
    custom = {
        // The post-init evaluator test exercises `get_subject_roles`
        // which reaches here. Return an `Ok` response with an empty
        // ancestor chain so the evaluator can produce an `Ok` result
        // for a synthetic subject with no assignments; the test then
        // asserts the empty-roles property directly.
        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> std::result::Result<GetAncestorsResponse, TenantResolverError> {
            Ok(GetAncestorsResponse {
                tenant: tenant_resolver_sdk::models::TenantRef {
                    id,
                    status: tenant_resolver_sdk::models::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                ancestors: Vec::new(),
            })
        }
    }
}

/// Stubbed `dyn TypesRegistryClient`. `init()` calls `register(...)` once
/// with the two RBAC entity GTS schemas; the stub returns one
/// `RegisterResult::Ok` per input entity, preserving order.
pub struct StubTypesRegistryClient;

panic_in_init! {
    impl TypesRegistryClient => for StubTypesRegistryClient,
    stub_label = "StubTypesRegistryClient",
    methods = [
        async fn register_type_schemas(_type_schemas: Vec<JsonValue>)
            -> std::result::Result<Vec<RegisterResult>, CanonicalError>;
        async fn get_type_schema(_type_id: &str)
            -> std::result::Result<GtsTypeSchema, CanonicalError>;
        async fn get_type_schema_by_uuid(_type_uuid: Uuid)
            -> std::result::Result<GtsTypeSchema, CanonicalError>;
        async fn get_type_schemas(_type_ids: Vec<String>)
            -> HashMap<String, std::result::Result<GtsTypeSchema, CanonicalError>>;
        async fn get_type_schemas_by_uuid(_type_uuids: Vec<Uuid>)
            -> HashMap<Uuid, std::result::Result<GtsTypeSchema, CanonicalError>>;
        async fn list_type_schemas(_query: TypeSchemaQuery)
            -> std::result::Result<Vec<GtsTypeSchema>, CanonicalError>;
        async fn register_instances(_instances: Vec<JsonValue>)
            -> std::result::Result<Vec<RegisterResult>, CanonicalError>;
        async fn get_instance(_id: &str)
            -> std::result::Result<GtsInstance, CanonicalError>;
        async fn get_instance_by_uuid(_uuid: Uuid)
            -> std::result::Result<GtsInstance, CanonicalError>;
        async fn get_instances(_ids: Vec<String>)
            -> HashMap<String, std::result::Result<GtsInstance, CanonicalError>>;
        async fn get_instances_by_uuid(_uuids: Vec<Uuid>)
            -> HashMap<Uuid, std::result::Result<GtsInstance, CanonicalError>>;
        async fn list_instances(_query: InstanceQuery)
            -> std::result::Result<Vec<GtsInstance>, CanonicalError>;
    ],
    custom = {
        async fn register(
            &self,
            entities: Vec<JsonValue>,
        ) -> std::result::Result<Vec<RegisterResult>, CanonicalError> {
            // Return one Ok result per input carrying the `$id` from each schema.
            let results = entities
                .into_iter()
                .map(|entity| {
                    let gts_id = entity
                        .get("$id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("<unknown>").to_owned();
                    RegisterResult::Ok { gts_id }
                })
                .collect();
            Ok(results)
        }
    }
}

/// Stub `dyn TypesRegistryClient` that returns `RegisterResult::Err` for
/// the second entity in the batch — drives the gts-id-propagation error test.
pub struct StubTypesRegistryClientFailing;

panic_in_init! {
    impl TypesRegistryClient => for StubTypesRegistryClientFailing,
    stub_label = "StubTypesRegistryClientFailing",
    methods = [
        async fn register_type_schemas(_type_schemas: Vec<JsonValue>)
            -> std::result::Result<Vec<RegisterResult>, CanonicalError>;
        async fn get_type_schema(_type_id: &str)
            -> std::result::Result<GtsTypeSchema, CanonicalError>;
        async fn get_type_schema_by_uuid(_type_uuid: Uuid)
            -> std::result::Result<GtsTypeSchema, CanonicalError>;
        async fn get_type_schemas(_type_ids: Vec<String>)
            -> HashMap<String, std::result::Result<GtsTypeSchema, CanonicalError>>;
        async fn get_type_schemas_by_uuid(_type_uuids: Vec<Uuid>)
            -> HashMap<Uuid, std::result::Result<GtsTypeSchema, CanonicalError>>;
        async fn list_type_schemas(_query: TypeSchemaQuery)
            -> std::result::Result<Vec<GtsTypeSchema>, CanonicalError>;
        async fn register_instances(_instances: Vec<JsonValue>)
            -> std::result::Result<Vec<RegisterResult>, CanonicalError>;
        async fn get_instance(_id: &str)
            -> std::result::Result<GtsInstance, CanonicalError>;
        async fn get_instance_by_uuid(_uuid: Uuid)
            -> std::result::Result<GtsInstance, CanonicalError>;
        async fn get_instances(_ids: Vec<String>)
            -> HashMap<String, std::result::Result<GtsInstance, CanonicalError>>;
        async fn get_instances_by_uuid(_uuids: Vec<Uuid>)
            -> HashMap<Uuid, std::result::Result<GtsInstance, CanonicalError>>;
        async fn list_instances(_query: InstanceQuery)
            -> std::result::Result<Vec<GtsInstance>, CanonicalError>;
    ],
    custom = {
        async fn register(
            &self,
            entities: Vec<JsonValue>,
        ) -> std::result::Result<Vec<RegisterResult>, CanonicalError> {
            Ok(entities
                .into_iter()
                .enumerate()
                .map(|(i, entity)| {
                    let gts_id = entity.get("$id").and_then(|v| v.as_str()).map(String::from);
                    if i == 0 {
                        RegisterResult::Ok {
                            gts_id: gts_id.unwrap_or_default(),
                        }
                    } else {
                        RegisterResult::Err {
                            gts_id,
                            error: CanonicalError::internal(
                                "stub: synthetic registration failure for E2E error-path test",
                            )
                            .create(),
                        }
                    }
                })
                .collect())
        }
    }
}

/// In-memory `ConfigProvider`. The single map entry follows the
/// `gears.<name> = { config: { ... } }` shape that `gear_config_required`
/// reads.
pub struct InMemoryConfigProvider {
    modules: std::collections::HashMap<String, JsonValue>,
}

impl InMemoryConfigProvider {
    /// Build a provider with `rbac` present/active. An empty `config: {}` is
    /// enough for `init()` to run — presence in `gears:` is what makes the
    /// module run.
    #[must_use]
    pub(crate) fn rbac_enabled() -> Self {
        let mut modules = std::collections::HashMap::new();
        modules.insert("rbac".to_owned(), json!({ "config": {} }));
        Self { modules }
    }

    /// Build a provider with `rbac` present and the integration roles seeded,
    /// which is what a deployment running the credstore / usage-collector gears
    /// configures. The startup tests assert against the full canonical roster,
    /// so they need this rather than the empty `config: {}` above — with the
    /// default the seeder writes only the four core roles.
    #[must_use]
    pub(crate) fn rbac_enabled_with_integration_roles() -> Self {
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            "rbac".to_owned(),
            json!({ "config": { "seed_integration_roles": true } }),
        );
        Self { modules }
    }

    /// Build a provider that exercises both configured grant lists and a
    /// multi-entry `builtin_role_targets.platform`. The point of the two lists
    /// is the `principal_type` each writes, and the point of the list-valued
    /// target is that one catalog rule expands into one rule per entry —
    /// neither is observable without going through `init()` against a real
    /// database.
    #[must_use]
    pub(crate) fn rbac_enabled_with_grants(user_subject: &str, service_subject: &str) -> Self {
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            "rbac".to_owned(),
            json!({
                "config": {
                    "seed_integration_roles": true,
                    "user_grants": [
                        { "role": "Reader", "principal_id": user_subject }
                    ],
                    "service_principal_grants": [
                        { "role": "Usage Emitter", "principal_id": service_subject }
                    ],
                    "builtin_role_targets": {
                        "platform": ["gts.cf.*", "gts.vendor.*"],
                        "resources_family": ["gts.vendor.resources.*"]
                    }
                }
            }),
        );
        Self { modules }
    }

    /// Build a provider with `rbac` present and `platform_admin_subject_id`
    /// set to `subject_id` — exercises the platform-admin bootstrap path.
    #[must_use]
    pub(crate) fn rbac_enabled_with_admin(subject_id: &str) -> Self {
        let mut modules = std::collections::HashMap::new();
        modules.insert(
            "rbac".to_owned(),
            json!({
                "config": {
                    "platform_admin_subject_id": subject_id
                }
            }),
        );
        Self { modules }
    }
}

impl ConfigProvider for InMemoryConfigProvider {
    fn get_gear_config(&self, gear_name: &str) -> Option<&JsonValue> {
        self.modules.get(gear_name)
    }
}

/// Output of [`prepare_e2e_harness`]: everything needed to drive
/// `Gear::init()` the way the `toolkit` runtime does. The container in
/// `_pg` ties teardown to the harness lifetime; `client_hub` is shared so
/// the test can `client_hub.get(...)` after `init()`.
pub struct E2eHarness {
    pub ctx: GearCtx,
    pub client_hub: Arc<ClientHub>,
    pub cancel: CancellationToken,
    pub url: String,
    pub pool: PgPool,
    /// Same `DBProvider` handle `init()` received via `ctx.db_required()`;
    /// stored so a re-init test can re-hand it without re-running migrations.
    pub db_provider: DBProvider<DbError>,
    /// The provider `ctx` was built from. A restart test MUST rebind with the
    /// same one: the seeder's `ON CONFLICT (id) DO UPDATE` rewrites
    /// `permissions` on every boot, so a second `init()` under a different
    /// config silently replaces what the first one seeded.
    pub config: Arc<dyn ConfigProvider>,
    pub _pg: ContainerAsync<Postgres>,
}

/// Shared bring-up for all three E2E harnesses: spin up Postgres, run
/// migrations, build the `ClientHub` with the supplied
/// `TypesRegistryClient` stub, and assemble a `GearCtx` against the
/// supplied `ConfigProvider`. Public wrappers below differ only in
/// those two slots.
async fn prepare_e2e_harness_with(
    config_provider: Arc<dyn ConfigProvider>,
    types_registry: Arc<dyn TypesRegistryClient>,
) -> Result<E2eHarness> {
    use toolkit::contracts::DatabaseCapability;

    // Pinned tag via `test-containers` — see `bring_up_migrated_postgres`.
    let postgres = test_containers::postgres()
        .with_env_var("POSTGRES_PASSWORD", "rbac_e2e_password")
        .with_env_var("POSTGRES_USER", "rbac_e2e_user")
        .with_env_var("POSTGRES_DB", "rbac_e2e_db");
    let container = postgres
        .start()
        .await
        .context("failed to start ephemeral PostgreSQL container for the E2E harness")?;
    // See `bring_up_migrated_postgres`: resolve the host via testcontainers so
    // the URL is correct under a remote/DinD Docker daemon, not just a local one.
    let host = container
        .get_host()
        .await
        .context("failed to resolve Postgres container host for the E2E harness")?;
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .context("failed to read mapped Postgres port for the E2E harness")?;
    let url = format!("postgres://rbac_e2e_user:rbac_e2e_password@{host}:{port}/rbac_e2e_db");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(20))
        .connect(&url)
        .await
        .context("failed to open sqlx PgPool for the E2E harness")?;

    let db = connect_db(&url, ConnectOpts::default())
        .await
        .context("failed to connect toolkit-db Db handle for the E2E harness")?;
    let module_for_migrations = rbac::module::RbacServiceGear::default();
    let migrations = module_for_migrations.migrations();
    run_migrations_for_testing(&db, migrations)
        .await
        .context("RBAC migrations failed during E2E harness bring-up")?;

    let db_provider: DBProvider<DbError> = DBProvider::new(db);

    let client_hub = Arc::new(ClientHub::new());
    let rg: Arc<dyn ResourceGroupReadHierarchy> = Arc::new(StubResourceGroupClient);
    client_hub.register::<dyn ResourceGroupReadHierarchy>(rg);
    let tenant: Arc<dyn TenantResolverClient> = Arc::new(StubTenantResolverClient);
    client_hub.register::<dyn TenantResolverClient>(tenant);
    client_hub.register::<dyn TypesRegistryClient>(types_registry);

    let cancel = CancellationToken::new();
    let ctx = GearCtx::new(
        "rbac",
        Uuid::new_v4(),
        config_provider.clone(),
        client_hub.clone(),
        cancel.clone(),
    )
    .with_db(db_provider.clone());

    Ok(E2eHarness {
        ctx,
        client_hub,
        cancel,
        url,
        pool,
        db_provider,
        config: config_provider,
        _pg: container,
    })
}

/// Spin up a fresh PostgreSQL container, run all RBAC migrations, and
/// return a [`E2eHarness`] with `enabled = true` ready for
/// `module.init(&harness.ctx).await`. All three upstream `ClientHub`
/// contracts are pre-registered with stubs.
pub async fn prepare_e2e_harness() -> Result<E2eHarness> {
    prepare_e2e_harness_with(
        Arc::new(InMemoryConfigProvider::rbac_enabled_with_integration_roles()),
        Arc::new(StubTypesRegistryClient),
    )
    .await
}

/// Build a second `GearCtx` pointing at the same DB + `ClientHub` as an
/// existing harness for idempotent-restart tests. Does NOT re-run migrations.
pub fn rebind_ctx_to_existing_db(harness: &E2eHarness) -> GearCtx {
    GearCtx::new(
        "rbac",
        Uuid::new_v4(),
        harness.config.clone(),
        harness.client_hub.clone(),
        harness.cancel.clone(),
    )
    .with_db(harness.db_provider.clone())
}

/// Open a fresh in-memory `SQLite` database and run the rbac migrations.
/// Each call returns its own `DBProvider` because `SeaORM` holds onto the
/// underlying pool — sharing between tests would race on `CREATE TABLE`.
/// Shared by the `sqlite_*` integration tests that drive the repos /
/// REST router without Docker.
pub async fn fresh_sqlite_provider() -> Result<DBProvider<DbError>> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .context("failed to open in-memory sqlite for rbac test")?;
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .context("rbac migrations failed on sqlite")?;
    Ok(DBProvider::new(db))
}

/// Build a `GearCtx` with no `TenantResolverClient` registered, for
/// missing-dep tests that assert `init()` returns a descriptive error.
pub async fn ctx_missing_tenant_resolver() -> Result<(GearCtx, Arc<ClientHub>)> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .context("failed to open sqlite for missing-dep test")?;
    let db_provider: DBProvider<DbError> = DBProvider::new(db);

    let client_hub = Arc::new(ClientHub::new());
    // Deliberately omit TenantResolverClient.
    let rg: Arc<dyn ResourceGroupReadHierarchy> = Arc::new(StubResourceGroupClient);
    client_hub.register::<dyn ResourceGroupReadHierarchy>(rg);
    let types: Arc<dyn TypesRegistryClient> = Arc::new(StubTypesRegistryClient);
    client_hub.register::<dyn TypesRegistryClient>(types);

    let ctx = GearCtx::new(
        "rbac",
        Uuid::new_v4(),
        Arc::new(InMemoryConfigProvider::rbac_enabled()),
        client_hub.clone(),
        CancellationToken::new(),
    )
    .with_db(db_provider);
    Ok((ctx, client_hub))
}

/// Like [`ctx_missing_tenant_resolver`] but omits `ResourceGroupReadHierarchy`.
pub async fn ctx_missing_rg_client() -> Result<(GearCtx, Arc<ClientHub>)> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .context("failed to open sqlite for missing-dep test")?;
    let db_provider: DBProvider<DbError> = DBProvider::new(db);

    let client_hub = Arc::new(ClientHub::new());
    let tenant: Arc<dyn TenantResolverClient> = Arc::new(StubTenantResolverClient);
    client_hub.register::<dyn TenantResolverClient>(tenant);
    let types: Arc<dyn TypesRegistryClient> = Arc::new(StubTypesRegistryClient);
    client_hub.register::<dyn TypesRegistryClient>(types);
    // Deliberately NO ResourceGroupReadHierarchy.

    let ctx = GearCtx::new(
        "rbac",
        Uuid::new_v4(),
        Arc::new(InMemoryConfigProvider::rbac_enabled()),
        client_hub.clone(),
        CancellationToken::new(),
    )
    .with_db(db_provider);
    Ok((ctx, client_hub))
}

/// Like [`prepare_e2e_harness`] but with a configured
/// `platform_admin_subject_id` so the platform-admin bootstrap step runs.
pub async fn prepare_e2e_harness_with_admin(subject_id: &str) -> Result<E2eHarness> {
    prepare_e2e_harness_with(
        Arc::new(InMemoryConfigProvider::rbac_enabled_with_admin(subject_id)),
        Arc::new(StubTypesRegistryClient),
    )
    .await
}

/// Like [`prepare_e2e_harness`] but with both grant lists configured and a
/// two-entry platform target list.
pub async fn prepare_e2e_harness_with_grants(
    user_subject: &str,
    service_subject: &str,
) -> Result<E2eHarness> {
    prepare_e2e_harness_with(
        Arc::new(InMemoryConfigProvider::rbac_enabled_with_grants(
            user_subject,
            service_subject,
        )),
        Arc::new(StubTypesRegistryClient),
    )
    .await
}

/// Like [`ctx_missing_tenant_resolver`] but omits `TypesRegistryClient`.
pub async fn ctx_missing_types_registry() -> Result<(GearCtx, Arc<ClientHub>)> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .context("failed to open sqlite for missing-dep test")?;
    let db_provider: DBProvider<DbError> = DBProvider::new(db);

    let client_hub = Arc::new(ClientHub::new());
    let rg: Arc<dyn ResourceGroupReadHierarchy> = Arc::new(StubResourceGroupClient);
    client_hub.register::<dyn ResourceGroupReadHierarchy>(rg);
    let tenant: Arc<dyn TenantResolverClient> = Arc::new(StubTenantResolverClient);
    client_hub.register::<dyn TenantResolverClient>(tenant);
    // Deliberately NO TypesRegistryClient.

    let ctx = GearCtx::new(
        "rbac",
        Uuid::new_v4(),
        Arc::new(InMemoryConfigProvider::rbac_enabled()),
        client_hub.clone(),
        CancellationToken::new(),
    )
    .with_db(db_provider);
    Ok((ctx, client_hub))
}

/// Harness with `TypesRegistryClient` replaced by
/// [`StubTypesRegistryClientFailing`] — verifies `init()` propagates the
/// failing-`gts_id` in the returned error.
pub async fn prepare_e2e_harness_with_failing_types_registry() -> Result<E2eHarness> {
    prepare_e2e_harness_with(
        Arc::new(InMemoryConfigProvider::rbac_enabled()),
        Arc::new(StubTypesRegistryClientFailing),
    )
    .await
}
