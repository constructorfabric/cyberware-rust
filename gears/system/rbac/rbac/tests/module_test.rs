//! Gear-wiring integration tests for `RbacServiceGear`.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value as JsonValue, json};
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::client_hub::ClientHub;
use toolkit::config::ConfigProvider;
use toolkit::contracts::{DatabaseCapability, RestApiCapability};
use toolkit::{Gear, GearCtx};
use uuid::Uuid;

use rbac::module::{ROLE_ASSIGNMENT_SCHEMA_JSON, ROLE_DEFINITION_SCHEMA_JSON, RbacServiceGear};

/// Minimal in-memory `ConfigProvider` for module-level tests.
struct InMemoryConfigProvider {
    modules: HashMap<String, JsonValue>,
}

impl InMemoryConfigProvider {
    fn empty() -> Self {
        Self {
            modules: HashMap::new(),
        }
    }

    /// A present-but-invalid `rbac` config section: an unknown key, which
    /// `RbacServiceConfig`'s `deny_unknown_fields` rejects. Used to prove
    /// `init()` aborts on malformed config rather than swallowing the
    /// error and booting a mis-configured security module.
    fn with_rbac_malformed() -> Self {
        let mut modules = HashMap::new();
        modules.insert(
            "rbac".to_owned(),
            json!({ "config": { "totally_unknown_key": 1 } }),
        );
        Self { modules }
    }
}

impl ConfigProvider for InMemoryConfigProvider {
    fn get_gear_config(&self, gear_name: &str) -> Option<&JsonValue> {
        self.modules.get(gear_name)
    }
}

fn ctx_with(config: Arc<dyn ConfigProvider>) -> GearCtx {
    GearCtx::new(
        "rbac",
        Uuid::new_v4(),
        config,
        Arc::new(ClientHub::default()),
        CancellationToken::new(),
    )
}

#[test]
fn module_default_constructs_empty_slots() {
    let module = RbacServiceGear::default();
    assert!(
        !module.runtime_is_populated(),
        "default RbacServiceGear must not have a populated RbacRuntime"
    );
}

#[test]
fn database_capability_returns_full_migration_set() {
    // Asserts the migration list is non-empty so future additive migrations
    // do not accidentally drop the list to zero.
    let module = RbacServiceGear::default();
    let migrations = module.migrations();
    assert!(
        !migrations.is_empty(),
        "expected at least 1 RBAC migration, got {}",
        migrations.len()
    );
}

#[test]
fn vendored_schemas_are_valid_json() {
    // Catches a corrupt vendored schema before the platform tries to register it.
    let role_definition: JsonValue = serde_json::from_str(ROLE_DEFINITION_SCHEMA_JSON)
        .expect("role_definition schema is valid JSON");
    assert_eq!(
        role_definition.get("$id").and_then(JsonValue::as_str),
        Some("gts://gts.cf.core.rbac.role_definition.v1~"),
        "role_definition.v1.schema.json $id must match the canonical GTS identifier"
    );

    let role_assignment: JsonValue = serde_json::from_str(ROLE_ASSIGNMENT_SCHEMA_JSON)
        .expect("role_assignment schema is valid JSON");
    assert_eq!(
        role_assignment.get("$id").and_then(JsonValue::as_str),
        Some("gts://gts.cf.core.rbac.role_assignment.v1~"),
        "role_assignment.v1.schema.json $id must match the canonical GTS identifier"
    );
}

/// When the `rbac` entry is absent from the `gears:` block, `init()` MUST
/// short-circuit (no DB / `ClientHub` / types-registry call) — RBAC is
/// compiled in but unconfigured, so it stays inert rather than failing the
/// boot. This is the "safe when unconfigured" property; the explicit
/// `enabled` switch was removed (disable = omit the entry).
#[tokio::test]
async fn init_short_circuits_when_config_missing() {
    let module = RbacServiceGear::default();
    let ctx = ctx_with(Arc::new(InMemoryConfigProvider::empty()));
    module
        .init(&ctx)
        .await
        .expect("init() with absent config must short-circuit successfully");
    assert!(
        !module.runtime_is_populated(),
        "init() with absent config must NOT populate the RbacRuntime"
    );
}

/// A present-but-invalid `rbac` config section MUST abort `init()` with
/// an error — NOT be swallowed into a silent default-disabled boot. The
/// distinction matters: absent config → inert (above); malformed config →
/// loud failure, so a typo next to `enabled = true` can't silently disable
/// the whole authorization surface.
#[tokio::test]
async fn init_errors_on_malformed_config() {
    let module = RbacServiceGear::default();
    let ctx = ctx_with(Arc::new(InMemoryConfigProvider::with_rbac_malformed()));
    let result = module.init(&ctx).await;
    assert!(
        result.is_err(),
        "init() MUST return Err for a present-but-invalid `rbac` config \
         section (deny_unknown_fields), not silently boot disabled"
    );
    assert!(
        !module.runtime_is_populated(),
        "a failed init() MUST NOT leave a populated runtime"
    );
}

/// `register_rest()` MUST nest under `/rbac/v1` and mount NO handlers when the
/// runtime slot is unpopulated (init not run — e.g. RBAC absent from
/// config), verified via `GET /rbac/v1/...` returning 404.
#[tokio::test]
async fn rest_capability_nests_v1_with_no_handlers() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let module = RbacServiceGear::default();
    let openapi = OpenApiRegistryImpl::default();
    let ctx = ctx_with(Arc::new(InMemoryConfigProvider::empty()));
    let router: axum::Router = axum::Router::new();
    let registered = module
        .register_rest(&ctx, router, &openapi)
        .expect("register_rest must succeed");

    let response = registered
        .oneshot(
            Request::builder()
                .uri("/rbac/v1/role-definitions")
                .body(Body::empty())
                .expect("test request must be constructible"),
        )
        .await
        .expect("router must handle the request without panicking");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "GET /rbac/v1/role-definitions MUST return 404 \u{2014} the /rbac/v1 \
         prefix must be reachable with an unpopulated runtime but no \
         handlers must be mounted",
    );
}

/// With an unpopulated runtime (init not run), `register_rest()` MUST
/// preserve sibling routes the framework stacked on the input router. Pins
/// the contract "produce an inert empty `/rbac/v1` namespace without
/// disturbing the caller's other routes".
#[tokio::test]
async fn rest_capability_preserves_sibling_routes_on_disabled_boot() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    let module = RbacServiceGear::default();
    let openapi = OpenApiRegistryImpl::default();
    let ctx = ctx_with(Arc::new(InMemoryConfigProvider::empty()));
    let router: axum::Router = axum::Router::new().route("/healthz", get(|| async { "ok" }));
    let registered = module
        .register_rest(&ctx, router, &openapi)
        .expect("register_rest must succeed");

    // Sibling route must survive.
    let healthz = registered
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("healthz request must be constructible"),
        )
        .await
        .expect("router must handle /healthz");
    assert_eq!(
        healthz.status(),
        StatusCode::OK,
        "GET /healthz MUST survive register_rest() with an unpopulated runtime",
    );

    // RBAC prefix is mounted but empty.
    let rbac = registered
        .oneshot(
            Request::builder()
                .uri("/rbac/v1/role-definitions")
                .body(Body::empty())
                .expect("rbac request must be constructible"),
        )
        .await
        .expect("router must handle /rbac/v1 request");
    assert_eq!(
        rbac.status(),
        StatusCode::NOT_FOUND,
        "GET /rbac/v1/role-definitions MUST return 404 with an unpopulated runtime",
    );
}

/// A typo in `name = "..."` on `#[toolkit::gear(...)]` is caught here
/// at unit-test time.
#[test]
fn module_name_attribute_matches_canonical_identifier() {
    assert_eq!(RbacServiceGear::MODULE_NAME, "rbac");
}

/// Event schemas are v2-deferred and MUST NOT be `include_str!`d in
/// `module.rs` until the Event Manager contract lands.
#[test]
fn event_schemas_are_not_included_in_module_source() {
    const MODULE_SOURCE: &str = include_str!("../src/module.rs");

    let deferred_schemas = [
        "role_definition_created.v1.schema.json",
        "role_definition_updated.v1.schema.json",
        "role_definition_deleted.v1.schema.json",
        "role_assignment_created.v1.schema.json",
        "role_assignment_deleted.v1.schema.json",
    ];

    for schema_filename in deferred_schemas {
        assert!(
            !MODULE_SOURCE.contains(schema_filename),
            "module.rs MUST NOT include_str! event schema '{schema_filename}' \
             — event schemas are v2-deferred; \
             only the two entity schemas (role_definition.v1, role_assignment.v1) \
             are registered in v1"
        );
    }
}

/// Every field the gear can serialize on a read must be a declared property
/// of the registered entity schema.
///
/// Both vendored schemas are `additionalProperties: false`, so a read-path
/// decoration the schema does not list makes the gear's own response invalid
/// against the contract it registered with GTS. That mismatch is invisible to
/// the type system and to the JSON-validity test above: it only shows up when
/// something actually validates a decorated payload. This does.
///
/// Fully decorated on purpose — every optional field set — because the fields
/// that break this are exactly the ones a bare fixture leaves unset.
#[test]
fn decorated_read_payloads_validate_against_the_registered_schemas() {
    use chrono::{TimeZone, Utc};
    use rbac_sdk::models::{PermissionRule, PrincipalType, RoleAssignment, RoleDefinition, Scope};

    let created = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("fixed timestamp is representable");
    let tenant = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);

    let role_definition = RoleDefinition::new(
        Uuid::from_u128(1),
        "Tenant Administrator",
        Some("Full control within the tenant".to_owned()),
        false,
        vec![PermissionRule::new(
            "read",
            "gts.cf.core.rbac.role_definition.v1~",
        )],
        vec![PermissionRule::new("delete", "gts.cf.core.rbac.*")],
        vec![Scope::tenant(tenant)],
        Some(tenant),
        created,
        created,
        "admin@example.com",
    )
    // The decoration the schema was missing.
    .with_assignment_count(Some(7));

    let role_assignment = RoleAssignment::new(
        Uuid::from_u128(2),
        Uuid::from_u128(1),
        "user-1",
        PrincipalType::User,
        Scope::tenant(tenant),
        created,
        created,
        "admin@example.com",
    )
    .with_principal_name(Some("Ada Lovelace".to_owned()))
    .with_created_by_name(Some("Grace Hopper".to_owned()))
    .with_role_definition_name(Some("Tenant Administrator".to_owned()));

    for (label, schema_json, payload) in [
        (
            "role_definition",
            ROLE_DEFINITION_SCHEMA_JSON,
            serde_json::to_value(&role_definition).expect("role definition serializes"),
        ),
        (
            "role_assignment",
            ROLE_ASSIGNMENT_SCHEMA_JSON,
            serde_json::to_value(&role_assignment).expect("role assignment serializes"),
        ),
    ] {
        let schema: JsonValue =
            serde_json::from_str(schema_json).expect("vendored schema is valid JSON");
        let validator = jsonschema::validator_for(&schema).expect("vendored schema compiles");
        let errors: Vec<String> = validator
            .iter_errors(&payload)
            .map(|err| format!("{}: {err}", err.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "{label} read payload is rejected by the schema the gear registers \
             for it — add the field as an optional property in \
             schemas/{label}.v1.schema.json (and re-copy to docs/schemas/): {errors:?}"
        );
    }
}
