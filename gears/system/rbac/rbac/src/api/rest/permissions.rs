//! `GET /rbac/v1/permissions` — list the platform's permission catalog.
//!
//! Open to any authenticated caller. The endpoint does NOT enforce a
//! `read` permission of its own: permissions are platform metadata, and
//! treating them as protected would introduce a recursive bootstrap
//! (the catalog would need to grant `read` on itself).
//!
//! Pagination is cursor-based and stable: results are sorted by `id`
//! ascending; the cursor encodes the last-seen `id` (base64url) and the
//! next page starts at the first entry whose `id` is strictly greater.
//! Permissions have no `created_at`, so this endpoint uses an id-only
//! cursor rather than the `(created_at, id)` shape used for roles.
//!
//! The route is registered through `toolkit::api::operation_builder::OperationBuilder`
//! so the `OpenAPI` document published at `/openapi.json` includes
//! the permissions catalog endpoint.

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::Json;
use rbac_sdk::error::RbacServiceError;
use serde::Deserialize;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;

use toolkit::api::canonical_prelude::CanonicalError;

use toolkit_odata::{Page, PageInfo};

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::dto::AuthzPermissionDto;
use crate::api::rest::error::rbac_service_error_to_canonical;
use crate::domain::permission_catalog::{
    CatalogCursor, PermissionCatalog, PermissionCatalogError, PermissionCatalogFilter,
};
use crate::domain::role_definition::service::{DEFAULT_LIMIT, MAX_LIMIT};

/// `OpenAPI` tag applied to the permissions catalog endpoint.
const TAG: &str = "RBAC Permissions";

/// Shared per-request state for the permissions route.
#[derive(Clone)]
pub struct ApiState {
    /// Catalog handle shared with `CreateRoleDefinition` /
    /// `UpdateRoleDefinition` (constructed at `Gear::init()` time).
    pub catalog: Arc<dyn PermissionCatalog>,
}

/// Build the Axum router for `/rbac/v1/permissions` and register it
/// with the supplied `OpenAPI` registry.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::get("/rbac/v1/permissions")
        .operation_id("rbac.list_permissions")
        .summary("List permission catalog")
        .description(
            "List every permission declared by any registered module. \
             Catalog entries are platform metadata; any authenticated \
             caller may read them.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("action", false, "Exact-match filter on permission `action`")
        .query_param(
            "resource_type_prefix",
            false,
            "Prefix filter on permission `resource_type`",
        )
        .query_param_typed(
            "limit",
            false,
            "Maximum items per page (default 50, max 200)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_permissions)
        .json_response_with_schema::<Page<AuthzPermissionDto>>(
            openapi,
            StatusCode::OK,
            "Page of permissions",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_422(openapi)
        // The catalog read hits the TypesRegistry; a registry outage
        // surfaces as `DependencyUnavailable` → 503. Declare it.
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    action: Option<String>,
    resource_type_prefix: Option<String>,
    /// Page size; default 50, max 200.
    limit: Option<u32>,
    cursor: Option<String>,
}

async fn list_permissions(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Page<AuthzPermissionDto>>, CanonicalError> {
    // Listing the catalog still requires an authenticated caller —
    // without this guard a mis-wired deployment lacking the upstream
    // `AuthN` middleware would expose the full catalog unauthenticated.
    let _ = require_authenticated(extension_ctx)?;

    let limit = match query.limit {
        Some(0) | None => DEFAULT_LIMIT,
        Some(v) => v,
    };
    if limit > MAX_LIMIT {
        return Err(rbac_service_error_to_canonical(
            RbacServiceError::invalid_limit(u64::from(limit), u64::from(MAX_LIMIT)),
        ));
    }

    let filter = PermissionCatalogFilter {
        action: query.action,
        resource_type_prefix: query.resource_type_prefix,
    };
    // An empty `?cursor=` is treated as a malformed cursor (400), not as
    // "no cursor". This keeps `/permissions` consistent with the role
    // endpoints, which route `?cursor=` through the shared OData layer and
    // reject an empty string. A present cursor is always decoded; only an
    // absent `cursor` param means "first page".
    let cursor = match query.cursor.as_deref() {
        Some(s) => Some(CatalogCursor::decode(s).map_err(|e| {
            rbac_service_error_to_canonical(RbacServiceError::invalid_cursor(format!("{e}")))
        })?),
        None => None,
    };

    let page = state
        .catalog
        .list_permissions(filter, cursor, limit)
        .await
        .map_err(catalog_error_to_rbac)
        .map_err(rbac_service_error_to_canonical)?;

    // Both cursors come straight from the catalog page. `prev_cursor` is
    // `null` on the first page, `next_cursor` is `null` on the last —
    // the `toolkit_odata::Page` envelope serialises both explicitly
    // (no `has_more`; clients derive it from `next_cursor` presence).
    let next_cursor = page.next_cursor.as_ref().map(CatalogCursor::encode);
    let prev_cursor = page.prev_cursor.as_ref().map(CatalogCursor::encode);
    Ok(Json(Page {
        items: page
            .items
            .into_iter()
            .map(AuthzPermissionDto::from)
            .collect(),
        page_info: PageInfo {
            next_cursor,
            prev_cursor,
            limit: u64::from(limit),
        },
    }))
}

/// Catalog-error → SDK-error mapping: registry failures surface as
/// `DependencyUnavailable` (503); data-integrity failures as
/// `Internal` (500).
fn catalog_error_to_rbac(err: PermissionCatalogError) -> RbacServiceError {
    match err {
        PermissionCatalogError::Registry(_) => {
            RbacServiceError::dependency_unavailable("TypesRegistryClient")
        }
        PermissionCatalogError::Deserialize { id, cause } => RbacServiceError::internal(format!(
            "permission catalog: failed to deserialize instance '{id}': {cause}"
        )),
        PermissionCatalogError::InvalidCursor => {
            RbacServiceError::invalid_cursor("cursor does not reference a known catalog entry")
        }
    }
}
