//! Axum handlers + router for `GET/POST/DELETE /rbac/v1/role-assignments`.
//!
//! Differences from the role-definition handlers:
//!
//! * No `PATCH` — assignments are create-and-delete only.
//! * Invalid `principal_type` produces 400 `InvalidPrincipalType`.
//! * `If-Match` is mandatory on `DELETE`, optional on reads.
//!
//! Routes are registered through `toolkit::api::operation_builder::OperationBuilder`
//! so the `OpenAPI` document published at `/openapi.json` includes the
//! role-assignment operations.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Extension,
    http::{HeaderMap, HeaderValue, StatusCode, header::ETAG, header::LOCATION},
    response::IntoResponse,
};
use rbac_sdk::models::{PrincipalType, RoleAssignment};
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::dto::{CreateRoleAssignmentRequest, RoleAssignmentDto};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_odata::Page;

use crate::api::rest::error::{
    ResourceKind, rbac_service_error_to_canonical, rbac_service_error_to_canonical_for,
};

/// Per-module shorthand: every `?` boundary in this file owns
/// role-assignment endpoints, so generic-arm errors (`Validation`,
/// `Conflict`, `AuthorizationDenied`, `OptimisticConcurrency*`, …)
/// must stamp `…role_assignment…` in `context.resource_type` rather
/// than the default `…role_definition…`.
fn assignment_err(err: crate::domain::error::DomainError) -> CanonicalError {
    rbac_service_error_to_canonical_for(
        rbac_sdk::error::RbacServiceError::from(err),
        ResourceKind::RoleAssignment,
    )
}

/// Error mapping for the `create` path. The cross-aggregate FK error
/// (`RoleDefinitionMissing`) lifts to a 404 on the *referenced role
/// definition* so clients get the OpenAPI-documented shape — that arm
/// intentionally stamps the role-definition resource. **Every other variant is
/// a role-assignment error** and must stamp `…role_assignment…` via
/// `assignment_err`: the blanket `From<DomainError>` hardwires the
/// role-definition default, so routing the catch-all through it would report a
/// `Conflict` / `AuthorizationDenied` / `RoleAssignmentDuplicate` raised by
/// `create` as a role-definition error on the wire.
fn create_assignment_err(err: crate::domain::error::DomainError) -> CanonicalError {
    match err {
        crate::domain::error::DomainError::RoleDefinitionMissing { role_definition_id } => {
            rbac_service_error_to_canonical(
                rbac_sdk::error::RbacServiceError::RoleDefinitionNotFound {
                    id: role_definition_id,
                },
            )
        }
        other => assignment_err(other),
    }
}
use crate::api::service::lowering::lower_role_assignment;
use crate::domain::etag::{Etag, etag_for};
use crate::domain::role_assignment::{
    CreateRoleAssignmentRequest as CreateRoleAssignmentDomainRequest, HydratedRoleAssignment,
    ListRoleAssignmentsRequest as ListRoleAssignmentsDomainRequest,
};
use crate::module::ConcreteRoleAssignmentService;
use crate::odata::RoleAssignmentFilterField;

/// `OpenAPI` tag applied to every role-assignment operation.
const TAG: &str = "RBAC Role Assignments";

/// Shared per-request state for the role-assignment routes.
#[derive(Clone)]
pub struct ApiState {
    /// Domain service that owns the validate → enforce → write flow.
    /// Named through the concrete alias in `module.rs`: the repository
    /// traits take `<C: DBRunner>` and so are not dyn-compatible.
    pub service: Arc<ConcreteRoleAssignmentService>,
}

/// Build the Axum router for `/rbac/v1/role-assignments` and register
/// every operation with the supplied `OpenAPI` registry.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/rbac/v1/role-assignments")
        .operation_id("rbac.create_role_assignment")
        .summary("Create role assignment")
        .description(
            "Bind a role definition to a principal at a scope. Returns 409 \
             when the same `(role_definition_id, principal_id, principal_type, scope)` \
             tuple already exists.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateRoleAssignmentRequest>(
            openapi,
            "Assignment to create \u{2014} `principal_type` must be one of \
             `User`, `Group`, or `ServicePrincipal`.",
        )
        .handler(post_role_assignment)
        .json_response_with_schema::<RoleAssignmentDto>(
            openapi,
            StatusCode::CREATED,
            "Created role assignment (carries `ETag` and `Location` headers)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/rbac/v1/role-assignments")
        .operation_id("rbac.list_role_assignments")
        .summary("List role assignments")
        .description(
            "Cursor-paginated list, auto-filtered by the caller's `read` \
             permission on `gts.cf.core.rbac.role_assignment.v1~`. Callers \
             without that permission receive 200 with an empty `items` array. \
             Supports OData `$filter` over `id`, `principal_id`, \
             `principal_type`, `role_definition_id`, `scope`, `created_at`, \
             and `created_by`. UUID values may be written either bare \
             (`role_definition_id eq 0192...`) or quoted \
             (`principal_id eq '0192...'`) on both id fields. `created_by` is \
             unindexed, so filtering or ordering by it scans.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Maximum items per page (default 50, max 200)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_role_assignments)
        .with_odata_filter::<RoleAssignmentFilterField>()
        .json_response_with_schema::<Page<RoleAssignmentDto>>(
            openapi,
            StatusCode::OK,
            "Page of role assignments",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_422(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/rbac/v1/role-assignments/{id}")
        .operation_id("rbac.get_role_assignment")
        .summary("Get role assignment")
        .description(
            "Retrieve a single role assignment by ID. Returns 404 for \
             assignments the caller cannot read so the endpoint never \
             leaks existence.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Role assignment ID (UUIDv7)")
        .handler(get_role_assignment)
        .json_response_with_schema::<RoleAssignmentDto>(
            openapi,
            StatusCode::OK,
            "Role assignment (carries `ETag` header)",
        )
        .error_401(openapi)
        .error_404(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/rbac/v1/role-assignments/{id}")
        .operation_id("rbac.delete_role_assignment")
        .summary("Delete role assignment")
        .description(
            "Hard-delete an assignment. Requires the `If-Match` header \
             carrying the assignment's current `ETag`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Role assignment ID (UUIDv7)")
        .handler(delete_role_assignment)
        .no_content_response(StatusCode::NO_CONTENT, "Role assignment deleted")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        // Stale/missing `If-Match` surface as canonical
        // `FailedPrecondition` = HTTP 400 (the missing/stale distinction
        // rides in `context.violations[].type`), NOT 412/428.
        .error_400(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn post_role_assignment(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    crate::api::rest::canonical_json::CanonicalJson(body): crate::api::rest::canonical_json::CanonicalJson<CreateRoleAssignmentRequest>,
) -> Result<axum::response::Response, CanonicalError> {
    let security_context = require_authenticated(extension_ctx)?;
    let principal_type = parse_principal_type(&body.principal_type)?;
    // Parse the scope at the boundary so the root check uses the typed
    // `Scope::Root` variant, not a raw-string `trim() == "/"` compare.
    // Any canonical form the SDK parser accepts as root (including
    // future normalised variants) is handled here uniformly. The
    // domain still re-parses today; the typed compare is the
    // load-bearing guard.
    let scope = rbac_sdk::models::Scope::parse(&body.scope).map_err(|e| {
        rbac_service_error_to_canonical_for(
            rbac_sdk::error::RbacServiceError::invalid_scope_format(format!("{}: {e}", body.scope)),
            ResourceKind::RoleAssignment,
        )
    })?;
    // Assignments at the root scope `/` require an explicit
    // first-party root caller. A built-in role with
    // `assignable_scopes=["/"]` would otherwise let a tenant-scoped
    // caller create a root-scoped assignment, defeating the scope
    // hierarchy. Belt-and-braces with the policy enforcer downstream.
    if matches!(scope, rbac_sdk::models::Scope::Root)
        && !crate::domain::caller_scope::is_first_party_root(&security_context)
    {
        return Err(rbac_service_error_to_canonical_for(
            rbac_sdk::error::RbacServiceError::authorization_denied(
                "root-scoped role assignments require a first-party root caller",
            ),
            ResourceKind::RoleAssignment,
        ));
    }
    let model = state
        .service
        .create(
            &security_context,
            CreateRoleAssignmentDomainRequest {
                role_definition_id: body.role_definition_id,
                principal_id: body.principal_id,
                principal_type,
                // Hand the already-parsed `Scope` to the domain instead
                // of re-stringifying it: the domain DTO is typed, so
                // there is no second parse.
                scope,
            },
        )
        .await
        .map_err(create_assignment_err)?;
    // A create response is deliberately never hydrated: the write path
    // performs no identity read, so the 201 body carries ids only. The
    // caller reads the names back on the subsequent GET.
    render_single(
        StatusCode::CREATED,
        HydratedRoleAssignment::bare(model),
        true,
    )
}

async fn list_role_assignments(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    OData(query): OData,
) -> Result<Json<Page<RoleAssignmentDto>>, CanonicalError> {
    let security_context = require_authenticated(extension_ctx)?;

    // First-party root callers (`token_scopes == ["*"]`) see every
    // tenant's assignments; non-root callers are scoped to their own
    // tenant subtree. Mirrors the create-path branching in
    // `caller_scope_from_context` — write paths route root callers via
    // `CallerScope::Root`, so the read path must too, otherwise root
    // listings silently return only the bearer-token tenant's rows.
    let context_scope = if crate::domain::caller_scope::is_first_party_root(&security_context) {
        rbac_sdk::models::Scope::root()
    } else {
        rbac_sdk::models::Scope::tenant(security_context.subject_tenant_id())
    };

    let page = state
        .service
        .list_with_names(
            &security_context,
            ListRoleAssignmentsDomainRequest {
                context_scope,
                query,
            },
        )
        .await
        .map_err(assignment_err)?;
    let items = page
        .items
        .into_iter()
        .map(|row| RoleAssignmentDto::try_from(lower_role_assignment(row)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| rbac_service_error_to_canonical_for(e, ResourceKind::RoleAssignment))?;
    // Rebuild the page rather than `map_items` because the DTO conversion
    // is fallible (`TryFrom`). `page.page_info` carries the full cursor
    // envelope (`next_cursor`, `prev_cursor`, `limit`) from the shared
    // `paginate_odata` helper.
    Ok(Json(Page {
        items,
        page_info: page.page_info,
    }))
}

async fn get_role_assignment(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    crate::api::rest::canonical_path::CanonicalPath(id): crate::api::rest::canonical_path::CanonicalPath<Uuid>,
) -> Result<axum::response::Response, CanonicalError> {
    let security_context = require_authenticated(extension_ctx)?;
    let row = state
        .service
        .get_with_names(&security_context, id)
        .await
        .map_err(assignment_err)?;
    render_single(StatusCode::OK, row, false)
}

async fn delete_role_assignment(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    crate::api::rest::canonical_path::CanonicalPath(id): crate::api::rest::canonical_path::CanonicalPath<Uuid>,
    headers: HeaderMap,
) -> Result<axum::response::Response, CanonicalError> {
    // Authenticate before parsing `If-Match` so unauthenticated probes
    // always get a uniform 401 rather than a shape-dependent 400.
    let security_context = require_authenticated(extension_ctx)?;
    let if_match = extract_if_match(&headers)?;
    state
        .service
        .delete(&security_context, id, if_match)
        .await
        .map_err(assignment_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Render one assignment (plus whatever names were resolved for it).
///
/// The `ETag` and `Location` are derived from the persisted row's
/// `updated_at` / `id`, which now live under `row.model` — display names
/// are not part of the row and MUST NOT influence either header, so a
/// hydrated and an unhydrated render of the same row are
/// cache-equivalent.
fn render_single(
    status: StatusCode,
    row: HydratedRoleAssignment,
    include_location: bool,
) -> Result<axum::response::Response, CanonicalError> {
    let etag = etag_for(row.model.updated_at, row.model.id);
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(etag.as_str()) {
        headers.insert(ETAG, v);
    }
    if include_location
        && let Ok(v) = HeaderValue::from_str(&format!("/rbac/v1/role-assignments/{}", row.model.id))
    {
        headers.insert(LOCATION, v);
    }
    let lowered: RoleAssignment = lower_role_assignment(row);
    // `PrincipalType` is `#[non_exhaustive]`; an unmapped (future)
    // variant lowers to `RbacServiceError::Internal` rather than panicking
    // mid-request, surfacing as a 500 instead of a silent miscoercion.
    let dto: RoleAssignmentDto = lowered
        .try_into()
        .map_err(|e| rbac_service_error_to_canonical_for(e, ResourceKind::RoleAssignment))?;
    Ok((status, headers, Json(dto)).into_response())
}

fn extract_if_match(headers: &HeaderMap) -> Result<Option<Etag>, CanonicalError> {
    crate::api::rest::if_match::parse_if_match(headers)
}

/// Parse the wire-level `principal_type` string into the typed enum.
/// Unknown values surface as `InvalidPrincipalType` (400).
fn parse_principal_type(s: &str) -> Result<PrincipalType, CanonicalError> {
    s.parse::<PrincipalType>().map_err(|e| {
        rbac_service_error_to_canonical_for(
            rbac_sdk::error::RbacServiceError::invalid_principal_type(e.0),
            ResourceKind::RoleAssignment,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::create_assignment_err;
    use crate::domain::error::DomainError;
    use toolkit::api::canonical_prelude::Problem;
    use toolkit_gts::gts_id;
    use uuid::Uuid;

    /// A generic `create` error MUST stamp the role-assignment
    /// `resource_type`. Routing the catch-all through `From<DomainError>`
    /// would stamp the hardwired role-definition default instead.
    #[test]
    fn create_authorization_denied_stamps_role_assignment_resource() {
        let problem = Problem::from(create_assignment_err(DomainError::authorization_denied(
            "nope",
        )));
        assert_eq!(problem.status, Some(403));
        assert_eq!(
            problem.context["resource_type"],
            gts_id!("cf.core.rbac.role_assignment.v1~"),
            "a create-path AuthorizationDenied MUST stamp the role-assignment resource"
        );
    }

    /// A generic `Conflict` on the create path is a role-assignment
    /// conflict (e.g. a duplicate assignment), not a role-definition one.
    #[test]
    fn create_conflict_stamps_role_assignment_resource() {
        let problem = Problem::from(create_assignment_err(DomainError::Conflict {
            detail: "dup".to_owned(),
        }));
        assert_eq!(problem.status, Some(409));
        assert_eq!(
            problem.context["resource_type"],
            gts_id!("cf.core.rbac.role_assignment.v1~")
        );
    }

    /// The one intentional exception: a missing referenced role
    /// definition lifts to a 404 on the *role-definition* resource so
    /// clients get the documented shape.
    #[test]
    fn create_role_definition_missing_lifts_to_role_definition_not_found() {
        let id = Uuid::nil();
        let problem = Problem::from(create_assignment_err(DomainError::RoleDefinitionMissing {
            role_definition_id: id,
        }));
        assert_eq!(problem.status, Some(404));
        assert_eq!(
            problem.context["resource_type"],
            gts_id!("cf.core.rbac.role_definition.v1~"),
            "a missing referenced role definition is a role-definition 404"
        );
    }
}
