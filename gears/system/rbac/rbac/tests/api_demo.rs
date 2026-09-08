//! End-to-end demo of the RBAC REST surface — walks every endpoint
//! against a testcontainers-backed PostgreSQL and prints the
//! request/response transcript. Run with:
//!
//! ```bash
//! cargo test -p cf-gears-rbac --test api_demo -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(test)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use tower::ServiceExt;
use uuid::{Uuid, uuid};

use rbac::domain::permission_catalog::{InMemoryPermissionCatalog, PermissionCatalog};
use rbac::domain::policy_enforcer::{MockPolicyEnforcer, ReadableScopes, ReadableScopesPred};
use rbac::domain::role_assignment::RoleAssignmentService;
use rbac::domain::role_definition::RoleDefinitionService;
use rbac::domain::scope_validator::ScopeValidator;
use rbac::domain::target_type_validator::AcceptAllTargetTypeValidator;
use rbac::infra::storage::role_assignment_repo;
use rbac::infra::storage::role_definition_repo;

mod common;
use common::scope_fakes as fakes;
use common::with_test_security_context;

const SUBJECT_TENANT: Uuid = uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

/// Status, headers, and raw body returned from `call()`.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn etag(&self) -> Option<String> {
        self.headers
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("response body must be JSON")
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers Postgres)"]
async fn api_demo_walks_full_surface() -> Result<()> {
    let (router, _fixture) = build_combined_router().await?;

    eprintln!("\n========================================");
    eprintln!(" RBAC REST API \u{2014} request/response demo");
    eprintln!("========================================");

    // 1. List the (empty) permission catalog.
    let step1 = call(
        &router,
        Method::GET,
        "/rbac/v1/permissions",
        &[],
        None,
        "Step 1 \u{2014} list permission catalog (empty seed)",
    )
    .await;
    assert_eq!(step1.status, StatusCode::OK, "Step 1: list permissions");

    // 2. Create a custom role definition.
    let create_body = serde_json::json!({
        "name": "Demo VM Reader",
        "description": "Read-only access to compute VMs for the demo walkthrough",
        "permissions": [
            { "operation": "read",  "target_type": "gts.cf.resources.compute.vm.v1~" }
        ],
        "not_permissions": [
            { "operation": "write", "target_type": "gts.cf.resources.compute.vm.v1~" }
        ],
        "assignable_scopes": [format!("/tenants/{SUBJECT_TENANT}")],
        "owner_tenant_id": SUBJECT_TENANT,
    });
    let create_resp = call(
        &router,
        Method::POST,
        "/rbac/v1/role-definitions",
        &[(header::CONTENT_TYPE.as_str(), "application/json")],
        Some(&create_body),
        "Step 2 \u{2014} create role definition",
    )
    .await;
    assert_eq!(create_resp.status, StatusCode::CREATED);
    let role_id = uuid_from(&create_resp.json(), "id");
    let role_etag = create_resp.etag().expect("create must return ETag");

    // 3. List role definitions (should include the new custom role).
    let step3 = call(
        &router,
        Method::GET,
        "/rbac/v1/role-definitions?limit=10",
        &[],
        None,
        "Step 3 \u{2014} list role definitions",
    )
    .await;
    assert_eq!(
        step3.status,
        StatusCode::OK,
        "Step 3: list role definitions"
    );

    // 4. Get the role definition by id.
    let get_uri = format!("/rbac/v1/role-definitions/{role_id}");
    let get_resp = call(
        &router,
        Method::GET,
        &get_uri,
        &[],
        None,
        "Step 4 \u{2014} get role definition by id",
    )
    .await;
    assert_eq!(
        get_resp.status,
        StatusCode::OK,
        "Step 4: get role definition"
    );
    let role_etag = get_resp.etag().unwrap_or(role_etag);

    // 5. Patch the role definition (must carry `If-Match`).
    let patch_body = serde_json::json!({
        "description": "Updated description \u{2014} patched in the demo walkthrough"
    });
    let patch_uri = format!("/rbac/v1/role-definitions/{role_id}");
    let patch_resp = call(
        &router,
        Method::PATCH,
        &patch_uri,
        &[
            (header::CONTENT_TYPE.as_str(), "application/json"),
            (header::IF_MATCH.as_str(), role_etag.as_str()),
        ],
        Some(&patch_body),
        "Step 5 \u{2014} patch role definition (with If-Match)",
    )
    .await;
    assert_eq!(
        patch_resp.status,
        StatusCode::OK,
        "Step 5: patch role definition"
    );
    let role_etag = patch_resp.etag().unwrap_or(role_etag);

    // 6. Create a role assignment for that role.
    let principal_id = Uuid::now_v7();
    let scope_path = format!("/tenants/{SUBJECT_TENANT}");
    let assign_body = serde_json::json!({
        "role_definition_id": role_id,
        "principal_id": principal_id.to_string(),
        "principal_type": "User",
        "scope": scope_path,
    });
    let assign_resp = call(
        &router,
        Method::POST,
        "/rbac/v1/role-assignments",
        &[(header::CONTENT_TYPE.as_str(), "application/json")],
        Some(&assign_body),
        "Step 6 \u{2014} create role assignment",
    )
    .await;
    assert_eq!(assign_resp.status, StatusCode::CREATED);
    let assign_id = uuid_from(&assign_resp.json(), "id");
    let assign_etag = assign_resp
        .etag()
        .expect("create assignment must return ETag");

    // 7. List role assignments.
    let step7 = call(
        &router,
        Method::GET,
        "/rbac/v1/role-assignments?limit=10",
        &[],
        None,
        "Step 7 \u{2014} list role assignments",
    )
    .await;
    assert_eq!(
        step7.status,
        StatusCode::OK,
        "Step 7: list role assignments"
    );

    // 8. Get the role assignment by id.
    let assign_uri = format!("/rbac/v1/role-assignments/{assign_id}");
    let assign_get_resp = call(
        &router,
        Method::GET,
        &assign_uri,
        &[],
        None,
        "Step 8 \u{2014} get role assignment by id",
    )
    .await;
    assert_eq!(
        assign_get_resp.status,
        StatusCode::OK,
        "Step 8: get role assignment"
    );
    let assign_etag = assign_get_resp.etag().unwrap_or(assign_etag);

    // 9. Delete the role assignment (If-Match required).
    let assign_del_uri = format!("/rbac/v1/role-assignments/{assign_id}");
    let step9 = call(
        &router,
        Method::DELETE,
        &assign_del_uri,
        &[(header::IF_MATCH.as_str(), assign_etag.as_str())],
        None,
        "Step 9 \u{2014} delete role assignment (with If-Match)",
    )
    .await;
    assert_eq!(
        step9.status,
        StatusCode::NO_CONTENT,
        "Step 9: delete role assignment"
    );

    // 10. Delete the role definition.
    let role_del_uri = format!("/rbac/v1/role-definitions/{role_id}");
    let step10 = call(
        &router,
        Method::DELETE,
        &role_del_uri,
        &[(header::IF_MATCH.as_str(), role_etag.as_str())],
        None,
        "Step 10 \u{2014} delete role definition (with If-Match)",
    )
    .await;
    assert_eq!(
        step10.status,
        StatusCode::NO_CONTENT,
        "Step 10: delete role definition"
    );

    eprintln!("\n========================================");
    eprintln!(" demo complete");
    eprintln!("========================================\n");
    Ok(())
}

// ---------------------------------------------------------------------------
// Router assembly
// ---------------------------------------------------------------------------

async fn build_combined_router() -> Result<(Router, common::PostgresUnderTest)> {
    let fixture = common::bring_up_migrated_postgres().await?;

    let db_defs = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider_defs: DBProvider<DbError> = DBProvider::new(db_defs);
    let db_assigns = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider_assigns: DBProvider<DbError> = DBProvider::new(db_assigns);

    let role_repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let assignment_repo = Arc::new(role_assignment_repo::RoleAssignmentRepository);

    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_chain(&[
        SUBJECT_TENANT,
    ])) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg =
        Arc::new(fakes::FakeRbacRgRead::default()) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg.clone()));
    let target_type_validator: Arc<dyn rbac::domain::target_type_validator::TargetTypeValidator> =
        Arc::new(AcceptAllTargetTypeValidator::new());
    let listing_catalog: Arc<dyn PermissionCatalog> = Arc::new(
        InMemoryPermissionCatalog::with_pairs(std::iter::empty::<(String, String)>()),
    );

    let defs_service = Arc::new(RoleDefinitionService::new(
        provider_defs.clone(),
        Arc::clone(&role_repo),
        // The real assignment repo, so the demo's role-definition reads carry
        // an `assignment_count` computed over the rows it actually holds.
        Arc::clone(&assignment_repo),
        policy.clone(),
        scope_validator.clone(),
        Arc::clone(&target_type_validator),
    ));
    let defs_state = Arc::new(rbac::api::rest::role_definitions::ApiState {
        service: defs_service,
    });
    let assignments_service = Arc::new(RoleAssignmentService::new(
        provider_assigns,
        Arc::clone(&assignment_repo),
        Arc::clone(&role_repo),
        policy,
        scope_validator,
        rg,
    ));
    let assignments_state = Arc::new(rbac::api::rest::role_assignments::ApiState {
        service: assignments_service,
    });
    let permissions_state = Arc::new(rbac::api::rest::permissions::ApiState {
        catalog: listing_catalog,
    });

    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = Router::new()
        .merge(rbac::api::rest::role_definitions::router(
            defs_state, &openapi,
        ))
        .merge(rbac::api::rest::role_assignments::router(
            assignments_state,
            &openapi,
        ))
        .merge(rbac::api::rest::permissions::router(
            permissions_state,
            &openapi,
        ));
    let router = with_test_security_context(router);

    Ok((router, fixture))
}

// ---------------------------------------------------------------------------
// Tracer helpers
// ---------------------------------------------------------------------------

/// Dispatch one request and print the transcript on stderr.
async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&serde_json::Value>,
    label: &str,
) -> Reply {
    eprintln!("\n----------------------------------------");
    eprintln!(" {label}");
    eprintln!("----------------------------------------");

    let mut builder = Request::builder().method(method.clone()).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let req_body_bytes = match body {
        Some(json) => serde_json::to_vec(json).expect("serialise request body"),
        None => Vec::new(),
    };
    let request = builder
        .body(Body::from(req_body_bytes))
        .expect("build request");

    eprintln!("> {method} {uri}");
    for (name, value) in headers {
        eprintln!("> {name}: {value}");
    }
    if let Some(json) = body {
        let pretty = serde_json::to_string_pretty(json).expect("pretty request body");
        eprintln!(">");
        for line in pretty.lines() {
            eprintln!("> {line}");
        }
    }

    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router.oneshot must succeed");

    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 1_000_000)
        .await
        .expect("read response body")
        .to_vec();

    eprintln!();
    eprintln!("< HTTP/1.1 {}", parts.status);
    for (name, value) in &parts.headers {
        match value.to_str() {
            Ok(s) => eprintln!("< {name}: {s}"),
            Err(_) => eprintln!("< {name}: <non-ascii>"),
        }
    }
    if bytes.is_empty() {
        eprintln!("<");
        eprintln!("< (empty body)");
    } else if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        let pretty = serde_json::to_string_pretty(&json).expect("pretty response body");
        eprintln!("<");
        for line in pretty.lines() {
            eprintln!("< {line}");
        }
    } else if let Ok(s) = std::str::from_utf8(&bytes) {
        eprintln!("<");
        for line in s.lines() {
            eprintln!("< {line}");
        }
    } else {
        eprintln!("< <{} bytes of non-UTF-8 body>", bytes.len());
    }

    Reply {
        status: parts.status,
        headers: parts.headers,
        body: bytes,
    }
}

#[allow(clippy::panic)] // test helper: a missing field is a hard failure.
fn uuid_from(value: &serde_json::Value, field: &str) -> Uuid {
    let raw = value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("response body must carry a `{field}` field"));
    Uuid::parse_str(raw).expect("id must be a UUID")
}
