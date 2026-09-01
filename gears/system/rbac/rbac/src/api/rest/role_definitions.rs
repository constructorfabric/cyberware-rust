//! Axum handlers + router for `GET/POST/PATCH/DELETE /rbac/v1/role-definitions`.
//!
//! Translates HTTP requests into command/query invocations and renders
//! responses (including `ETag` and `Location` headers). Requests without
//! an authenticated `SecurityContext` are rejected with 401; integration
//! tests must construct a real `SecurityContext` via
//! `SecurityContext::builder()`.
//!
//! Routes are registered through `toolkit::api::operation_builder::OperationBuilder`
//! so the `OpenAPI` document published at `/openapi.json` lists each
//! operation with its declared request / response schemas.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Extension,
    http::{HeaderMap, HeaderValue, StatusCode, header::ETAG, header::LOCATION},
    response::IntoResponse,
};
use rbac_sdk::models::RoleDefinition;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use toolkit::api::canonical_prelude::CanonicalError;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::dto::{
    CreateRoleDefinitionRequest, PatchRoleDefinitionRequest, RoleDefinitionDto,
    RoleDefinitionSummaryDto,
};
use crate::api::rest::error::rbac_service_error_to_canonical;
use crate::api::service::lowering::lower_role_definition;
use crate::domain::caller_scope::caller_scope_from_context;
use crate::domain::etag::{Etag, etag_for};
use crate::domain::role_definition::{
    CountedRoleDefinition, CreateRoleDefinitionRequest as CreateRoleDefinitionDomainRequest,
    ListRoleDefinitionsRequest as ListRoleDefinitionsDomainRequest,
    UpdateRoleDefinitionRequest as UpdateRoleDefinitionDomainRequest,
};
use crate::domain::role_definition_repo::RoleDefinitionPatch;
use crate::module::ConcreteRoleDefinitionService;
use crate::odata::RoleDefinitionFilterField;
use toolkit_odata::Page;

/// `OpenAPI` tag applied to every role-definition operation.
const TAG: &str = "RBAC Role Definitions";

/// Catalog-counts sibling of the collection route. The last segment is
/// **static**, so it is not an `{id}` value: axum 0.8 / `matchit` prefers a
/// static segment over a parameter, which is what lets
/// `…/role-definitions/summary` coexist with
/// `GET …/role-definitions/{id}`. That preference belongs to the routing
/// library rather than to this module, so it is pinned by
/// `summary_path_is_not_shadowed_by_get_by_id` instead of assumed.
///
/// Nothing needs reserving in the other direction — unlike the monitoring
/// gear's string source ids, a role definition's id is a `Uuid`, and
/// `summary` never parses as one. So no row can be made unreachable by this
/// route, and the reserved word only has to stay out of the UUID grammar,
/// which it does by construction. The match is case-sensitive, so
/// `…/role-definitions/SUMMARY` falls through to the by-id route and earns
/// that route's malformed-UUID 400 — pinned by
/// `summary_route_matches_the_reserved_word_case_sensitively`.
const SUMMARY_PATH: &str = "/rbac/v1/role-definitions/summary";

/// Shared per-request state for the role-definition routes. Constructed
/// once at `init()` and shared via `Extension<Arc<ApiState>>`.
#[derive(Clone)]
pub struct ApiState {
    /// Domain service that owns the validate → enforce → write flow.
    /// One singleton per process. Named through the concrete alias in
    /// `module.rs`: the repository traits take `<C: DBRunner>` and so are
    /// not dyn-compatible.
    pub service: Arc<ConcreteRoleDefinitionService>,
}

/// Build the Axum router for `/rbac/v1/role-definitions` and register
/// every operation with the supplied `OpenAPI` registry. `state` is
/// attached via an `Extension` layer at the end so the registry sees
/// route definitions before the per-request state is bound.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/rbac/v1/role-definitions")
        .operation_id("rbac.create_role_definition")
        .summary("Create role definition")
        .description(
            "Create a custom role definition. Built-in roles cannot be created \
             through this endpoint.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateRoleDefinitionRequest>(
            openapi,
            "Role definition to create \u{2014} Allow rules go in `permissions`, \
             Deny rules in `not_permissions`.",
        )
        .handler(post_role_definition)
        .json_response_with_schema::<RoleDefinitionDto>(
            openapi,
            StatusCode::CREATED,
            "Created role definition (carries `ETag` and `Location` headers)",
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

    router = OperationBuilder::get("/rbac/v1/role-definitions")
        .operation_id("rbac.list_role_definitions")
        .summary("List role definitions")
        .description(
            "Cursor-paginated list of built-in and custom role definitions. \
             Built-ins are visible to every authenticated caller; custom \
             roles require `read` on `gts.cf.core.rbac.role_definition.v1~` \
             within the owning tenant subtree. Each row carries \
             `assignment_count`, the number of role assignments using that \
             role **within the caller's own assignment-read visibility**; the \
             field is omitted for a caller who can read no assignments at \
             all, and `0` means \"visible and unused\".",
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
        .handler(list_role_definitions)
        .json_response_with_schema::<Page<RoleDefinitionDto>>(
            openapi,
            StatusCode::OK,
            "Page of role definitions",
        )
        .with_odata_filter::<RoleDefinitionFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_422(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(SUMMARY_PATH)
        .operation_id("rbac.get_role_definitions_summary")
        .summary("Role definitions summary")
        .description(
            "Built-in / custom role counts for the roles catalog, computed \
             under the caller's own visibility: `built_in` is the shared \
             built-in catalog (visible to every authenticated caller) and \
             `custom` covers the custom roles the caller may read. `total` is \
             `built_in + custom`. No `$filter` and no pagination - this is a \
             plain summary of the rows the list endpoint would page through.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(get_role_definitions_summary)
        .json_response_with_schema::<RoleDefinitionSummaryDto>(
            openapi,
            StatusCode::OK,
            "Role-definition counts by kind",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_422(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/rbac/v1/role-definitions/{id}")
        .operation_id("rbac.get_role_definition")
        .summary("Get role definition")
        .description(
            "Retrieve a single role definition by ID. Returns 404 for \
             unauthorized custom roles to avoid information leakage. Carries \
             the same caller-visibility-bounded `assignment_count` as the \
             list endpoint.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Role definition ID (UUIDv7)")
        .handler(get_role_definition)
        .json_response_with_schema::<RoleDefinitionDto>(
            openapi,
            StatusCode::OK,
            "Role definition (carries `ETag` header)",
        )
        .error_401(openapi)
        .error_404(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch("/rbac/v1/role-definitions/{id}")
        .operation_id("rbac.update_role_definition")
        .summary("Update role definition")
        .description(
            "Apply a partial update to a custom role definition. \
             Built-in roles are immutable. Requires the `If-Match` header \
             carrying the role's current `ETag`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Role definition ID (UUIDv7)")
        .json_request::<PatchRoleDefinitionRequest>(
            openapi,
            "Partial update \u{2014} set only the fields to change. Immutable \
             fields (`id`, `is_built_in`, `owner_tenant_id`, \
             `created_at`, `created_by`) are rejected with 400.",
        )
        .handler(patch_role_definition)
        .json_response_with_schema::<RoleDefinitionDto>(
            openapi,
            StatusCode::OK,
            "Updated role definition (carries refreshed `ETag` header)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        // Optimistic-concurrency failures (stale or missing
        // `If-Match`) surface as canonical `FailedPrecondition` = HTTP 400
        // (declared above), NOT 412/428. The missing/stale distinction
        // rides in `context.violations[].type` (`PRECONDITION_REQUIRED` /
        // `PRECONDITION_FAILED`).
        .error_422(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/rbac/v1/role-definitions/{id}")
        .operation_id("rbac.delete_role_definition")
        .summary("Delete role definition")
        .description(
            "Delete a custom role definition. Built-in roles are immutable \
             and cannot be deleted; deletion is rejected when the role has \
             active assignments. Requires the `If-Match` header.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Role definition ID (UUIDv7)")
        .handler(delete_role_definition)
        .no_content_response(StatusCode::NO_CONTENT, "Role definition deleted")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
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

async fn post_role_definition(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    crate::api::rest::canonical_json::CanonicalJson(body): crate::api::rest::canonical_json::CanonicalJson<CreateRoleDefinitionRequest>,
) -> Result<axum::response::Response, CanonicalError> {
    let security_context = require_authenticated(extension_ctx)?;
    let caller_scope = caller_scope_from_context(&security_context);

    // Parse `assignable_scopes` at the wire boundary so a parse failure
    // produces a typed `invalid_scope_format` (422) and the domain
    // receives typed `Scope`s — matches the PATCH path and removes the
    // domain's redundant re-parse.
    let assignable_scopes: Vec<rbac_sdk::models::Scope> = body
        .assignable_scopes
        .iter()
        .map(|s| {
            rbac_sdk::models::Scope::parse(s).map_err(|e| {
                rbac_service_error_to_canonical(
                    rbac_sdk::error::RbacServiceError::invalid_scope_format(format!("{s}: {e}")),
                )
            })
        })
        .collect::<Result<_, _>>()?;

    let model = state
        .service
        .create(
            &security_context,
            CreateRoleDefinitionDomainRequest {
                caller_scope,
                name: body.name,
                // Normalise empty descriptions to `None` so the DB
                // stores `NULL` rather than `''`; the round-trip
                // through `Option<String>` on read stays unambiguous.
                description: body.description.filter(|s| !s.is_empty()),
                permissions: body.permissions.into_iter().map(Into::into).collect(),
                not_permissions: body.not_permissions.into_iter().map(Into::into).collect(),
                assignable_scopes,
                owner_tenant_id: body.owner_tenant_id,
            },
        )
        .await?;
    // A create response deliberately carries no `assignment_count`: the write
    // path performs no count (a brand-new role has none, but saying so would
    // still need a PDP call the write path does not make), so the creator
    // reads the number back on the next GET.
    Ok(render_single(
        StatusCode::CREATED,
        CountedRoleDefinition::bare(model),
        true,
    ))
}

async fn list_role_definitions(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    OData(query): OData,
) -> Result<Json<Page<RoleDefinitionDto>>, CanonicalError> {
    let security_context = require_authenticated(extension_ctx)?;
    let caller_scope = caller_scope_from_context(&security_context);

    let page = state
        .service
        .list_with_counts(
            &security_context,
            ListRoleDefinitionsDomainRequest {
                caller_scope,
                query,
            },
        )
        .await?;
    // `map_items` preserves `page_info` (`next_cursor`, `prev_cursor`,
    // `limit`) computed by the shared `paginate_odata` helper, so the
    // full cursor envelope now reaches the wire.
    Ok(Json(page.map_items(|m| lower_role_definition(m).into())))
}

async fn get_role_definition(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    crate::api::rest::canonical_path::CanonicalPath(id): crate::api::rest::canonical_path::CanonicalPath<Uuid>,
) -> Result<axum::response::Response, CanonicalError> {
    let security_context = require_authenticated(extension_ctx)?;
    // The count's visibility is derived from the caller's own scope, exactly
    // as on the list path — a root token holder counts across every tenant
    // they can read, a tenant-scoped caller inside their own subtree.
    let caller_scope = caller_scope_from_context(&security_context);
    let row = state
        .service
        .get_with_counts(&security_context, id, &caller_scope)
        .await?;
    Ok(render_single(StatusCode::OK, row, false))
}

/// `GET /rbac/v1/role-definitions/summary` — built-in / custom counts for
/// the roles catalog, under the caller's own visibility.
///
/// Deliberately takes no [`OData`] extractor: adding one would advertise a
/// `$filter` surface this endpoint does not honour, and a filter clause the
/// summary cannot apply is worse than no filter at all — the caller would
/// read numbers computed over a different row set than they asked for.
async fn get_role_definitions_summary(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
) -> Result<Json<RoleDefinitionSummaryDto>, CanonicalError> {
    let security_context = require_authenticated(extension_ctx)?;
    let caller_scope = caller_scope_from_context(&security_context);
    let counts = state
        .service
        .summary(&security_context, &caller_scope)
        .await?;
    Ok(Json(counts.into()))
}

async fn patch_role_definition(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    crate::api::rest::canonical_path::CanonicalPath(id): crate::api::rest::canonical_path::CanonicalPath<Uuid>,
    headers: HeaderMap,
    crate::api::rest::canonical_json::CanonicalJson(body): crate::api::rest::canonical_json::CanonicalJson<PatchRoleDefinitionRequest>,
) -> Result<axum::response::Response, CanonicalError> {
    // Authenticate before any body/header inspection so unauthenticated
    // probes always get a uniform 401 rather than a shape-dependent 400.
    let security_context = require_authenticated(extension_ctx)?;
    // Do NOT short-circuit here. The immutable-field check moves
    // into the service so authz runs first and an unauthorized caller's
    // response is byte-identical to the missing-row 404. We detect
    // the offending field at the boundary because the typed domain
    // patch doesn't carry the immutable fields, then pass the
    // first-hit name through to the service.
    let immutable_field_attempted = first_immutable_field(&body);
    let if_match = extract_if_match(&headers)?;

    // Parse `assignable_scopes` at the wire boundary so the typed
    // `RoleDefinitionPatch` carries `Vec<Scope>`. Format errors surface
    // as 422 here rather than as an internal mapping error from the
    // service handler.
    let assignable_scopes: Option<Vec<rbac_sdk::models::Scope>> = body
        .assignable_scopes
        .map(|scopes| {
            scopes
                .into_iter()
                .map(|s| {
                    rbac_sdk::models::Scope::parse(&s).map_err(|e| {
                        rbac_service_error_to_canonical(
                            rbac_sdk::error::RbacServiceError::invalid_scope_format(format!(
                                "{s}: {e}"
                            )),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    let model = state
        .service
        .update(
            &security_context,
            UpdateRoleDefinitionDomainRequest {
                id,
                if_match,
                patch: RoleDefinitionPatch {
                    name: body.name,
                    // Normalise: `Some(Some(""))` → `Some(None)` (clear)
                    // so the DB carries NULL, not ''. `Some(Some(s))`
                    // with non-empty `s` stays a set; `None` is "leave
                    // unchanged".
                    description: body
                        .description
                        .map(|inner| inner.filter(|s| !s.is_empty())),
                    permissions: body
                        .permissions
                        .map(|rs| rs.into_iter().map(Into::into).collect()),
                    not_permissions: body
                        .not_permissions
                        .map(|rs| rs.into_iter().map(Into::into).collect()),
                    assignable_scopes,
                },
                immutable_field_attempted,
            },
        )
        .await?;
    // Like the create path: an update performs no count, so the field is
    // omitted rather than being re-derived at extra PDP cost on a write.
    Ok(render_single(
        StatusCode::OK,
        CountedRoleDefinition::bare(model),
        false,
    ))
}

async fn delete_role_definition(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    crate::api::rest::canonical_path::CanonicalPath(id): crate::api::rest::canonical_path::CanonicalPath<Uuid>,
    headers: HeaderMap,
) -> Result<axum::response::Response, CanonicalError> {
    // Authenticate before parsing `If-Match` so unauthenticated probes
    // always get a uniform 401 rather than a shape-dependent 400.
    // Matches `delete_role_assignment`.
    let security_context = require_authenticated(extension_ctx)?;
    let if_match = extract_if_match(&headers)?;
    state
        .service
        .delete(&security_context, id, if_match)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Render one role definition (plus whatever count was computed for it).
///
/// The `ETag` and `Location` are derived from the persisted row's
/// `updated_at` / `id`, which live under `row.model` — the assignment count
/// is not part of the row and MUST NOT influence either header, so a counted
/// and an uncounted render of the same row stay cache-equivalent. (It also
/// could not safely do so: the count changes when a *different* table
/// changes, so folding it into the `ETag` would break the concurrency
/// contract on `PATCH` / `DELETE`.)
fn render_single(
    status: StatusCode,
    row: CountedRoleDefinition,
    include_location: bool,
) -> axum::response::Response {
    let etag = etag_for(row.model.updated_at, row.model.id);
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(etag.as_str()) {
        headers.insert(ETAG, v);
    }
    if include_location
        && let Ok(v) = HeaderValue::from_str(&format!("/rbac/v1/role-definitions/{}", row.model.id))
    {
        headers.insert(LOCATION, v);
    }
    let role: RoleDefinition = lower_role_definition(row);
    let dto: RoleDefinitionDto = role.into();
    (status, headers, Json(dto)).into_response()
}

fn extract_if_match(headers: &HeaderMap) -> Result<Option<Etag>, CanonicalError> {
    crate::api::rest::if_match::parse_if_match(headers)
}

fn first_immutable_field(body: &PatchRoleDefinitionRequest) -> Option<&'static str> {
    if body.id.is_some() {
        return Some("id");
    }
    if body.is_built_in.is_some() {
        return Some("is_built_in");
    }
    if body.owner_tenant_id.is_some() {
        return Some("owner_tenant_id");
    }
    if body.created_at.is_some() {
        return Some("created_at");
    }
    if body.created_by.is_some() {
        return Some("created_by");
    }
    None
}
