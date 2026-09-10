//! Axum handlers + router for the exception-queue dashboard (Slice 7 Phase 2,
//! design §4.6 / §5).
//!
//! - `GET /bss-ledger/v1/exceptions` — list the tenant's exceptions (the Revenue
//!   Assurance dashboard / queue), cursor-paginated, with an `OData` `$filter` over
//!   `tenant_id` / `type` / `status` / `business_ref` / `period_id`. Gates on
//!   `RECONCILIATION:read`.
//! - `POST /bss-ledger/v1/exceptions/{exception_id}/resolution` — transition an
//!   OPEN exception: `ACK` / `RESOLVED` (operator triage), or `APPROVED_EXCEPTION`
//!   (Finance — **`GL_WRITEOFF_VARIANCE` only**, the one acknowledge-to-non-block
//!   kind, N-pay-5). Gates on `RECONCILIATION:resolve`.
//!
//! Tenant-scoped (SQL-level BOLA): the compiled `RECONCILIATION` scope is the
//! filter, so a foreign id reads as `None` ⇒ 404 (no existence leak).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::local_client::map_odata_page_err;
use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::error::{authz_error_to_canonical, exception_not_found};
use crate::api::rest::odata_list::{list_seller_tenant, reject_non_odata_list_params};
use crate::domain::error::DomainError;
use crate::domain::exception::{ExceptionStatus, ExceptionType};
use crate::domain::instant::rfc3339;
use crate::infra::storage::entity::exception_queue;
use crate::infra::storage::repo::ExceptionQueueRepo;
use crate::odata::{ExceptionFilterField, ExceptionOrderField};
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the exception operations.
const TAG: &str = "BSS Ledger Exceptions";

/// Shared per-request state for the exception routes.
#[derive(Clone)]
pub struct ApiState {
    /// The exception-queue repository (list / read / resolve).
    pub repo: ExceptionQueueRepo,
}

/// One exception-queue row, projected for the dashboard. PII-free (ids + codes).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ExceptionView {
    pub exception_id: Uuid,
    pub exception_type: String,
    pub business_ref: String,
    pub status: String,
    pub period_id: Option<String>,
    #[serde(with = "rfc3339")]
    pub opened_at: OffsetDateTime,
    #[serde(default, with = "rfc3339::option")]
    pub resolved_at: Option<OffsetDateTime>,
    pub resolved_by: Option<String>,
}

impl From<exception_queue::Model> for ExceptionView {
    fn from(m: exception_queue::Model) -> Self {
        Self {
            exception_id: m.exception_id,
            exception_type: m.exception_type,
            business_ref: m.business_ref,
            status: m.status,
            period_id: m.period_id,
            opened_at: m.opened_at,
            resolved_at: m.resolved_at,
            resolved_by: m.resolved_by,
        }
    }
}

/// Resolve-exception request body.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ResolveExceptionRequest {
    /// Target status: `ACK`, `RESOLVED`, or `APPROVED_EXCEPTION`
    /// (`GL_WRITEOFF_VARIANCE` only).
    pub status: String,
    /// Operator / Finance reason (audit context; recorded with the actor).
    pub reason: Option<String>,
}

/// Build the Axum router for the exception surface.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::get("/bss-ledger/v1/exceptions")
        .operation_id("bss_ledger.list_exceptions")
        .summary("List the tenant's close-blocking exceptions (cursor-paginated)")
        .description(
            "Cursor-paginated list of exception-queue rows (the Revenue Assurance \
             dashboard / queue) for the seller named by `$filter=tenant_id eq \
             <uuid>` (the caller's own by default). Supports OData `$filter` over \
             `tenant_id`, `type` (e.g. RECON_MISMATCH), `status` (OPEN / ACK / \
             RESOLVED / APPROVED_EXCEPTION), `business_ref`, and `period_id`. The \
             `$filter` ANDs the caller's authorized subtree, so exceptions outside \
             it are never returned (SQL-level BOLA). Each item is the same \
             `ExceptionView`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Maximum items per page (default 25, max 200)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_exceptions)
        .with_odata_filter::<ExceptionFilterField>()
        .with_odata_orderby::<ExceptionOrderField>()
        .json_response_with_schema::<Page<ExceptionView>>(
            openapi,
            StatusCode::OK,
            "One page of the tenant's exceptions.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/exceptions/{exception_id}/resolution")
        .operation_id("bss_ledger.resolve_exception")
        .summary("Resolve / acknowledge / approve an exception")
        .description(
            "Transitions an OPEN exception: `ACK` / `RESOLVED` (operator triage) or \
             `APPROVED_EXCEPTION` (Finance — GL_WRITEOFF_VARIANCE only, the one \
             acknowledge-to-non-block kind). A resolved exception no longer blocks \
             period close.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("exception_id", "The exception being resolved.")
        .json_request::<ResolveExceptionRequest>(openapi, "The target status + reason.")
        .handler(resolve_exception)
        .json_response_with_schema::<ExceptionView>(
            openapi,
            StatusCode::OK,
            "The resolved exception.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// `GET …/exceptions`: list the caller tenant's exceptions (RECONCILIATION:read),
/// cursor-paginated. The `$filter` (`type` / `status` / `business_ref` /
/// `period_id`) ANDs the caller's compiled read scope, so the page never contains
/// a foreign-tenant exception (SQL-level BOLA, no existence leak). Mirrors
/// `refunds::list_refunds`.
#[allow(clippy::implicit_hasher)]
async fn list_exceptions(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<ExceptionView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECONCILIATION,
        crate::authz::actions::READ,
        Some(tenant),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let page = state
        .repo
        .list_page(&scope, tenant, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.into_iter().map(ExceptionView::from).collect(),
        page_info: page.page_info,
    }))
}

/// `POST …/exceptions/{id}/resolution`: transition an OPEN exception
/// (RECONCILIATION:resolve). Validates the target status + the
/// `APPROVED_EXCEPTION`-is-GL-writeoff-only rule, then applies the transition.
async fn resolve_exception(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(exception_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<ResolveExceptionRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECONCILIATION,
        crate::authz::actions::RESOLVE,
        Some(tenant),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Parse + validate the target status. Only the three resolution states are
    // reachable from a handler — you cannot resolve an exception back to OPEN.
    let target = ExceptionStatus::parse(&body.status).filter(|s| {
        matches!(
            s,
            ExceptionStatus::Ack | ExceptionStatus::Resolved | ExceptionStatus::ApprovedException
        )
    });
    let Some(target) = target else {
        return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
            "invalid resolution status {:?} (expected ACK, RESOLVED, or APPROVED_EXCEPTION)",
            body.status
        ))));
    };

    // Read the row (scoped) — a foreign / unknown id is a 404 (no existence leak).
    let row = state
        .repo
        .read(&scope, tenant, exception_id)
        .await?
        .ok_or_else(|| exception_not_found(exception_id))?;

    // Only an OPEN exception is resolvable (a terminal row is not re-transitioned).
    if row.status != ExceptionStatus::Open.as_str() {
        return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
            "exception {exception_id} is {} (already resolved)",
            row.status
        ))));
    }

    // `APPROVED_EXCEPTION` is the GL-writeoff acknowledge-to-non-block path ONLY
    // (N-pay-5): every other type resolves via ACK / RESOLVED.
    if target == ExceptionStatus::ApprovedException
        && row.exception_type != ExceptionType::GlWriteoffVariance.as_str()
    {
        return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
            "APPROVED_EXCEPTION is only valid for GL_WRITEOFF_VARIANCE (got {})",
            row.exception_type
        ))));
    }

    let actor = ctx.subject_id().to_string();
    state
        .repo
        .resolve_one(&scope, tenant, exception_id, target.as_str(), &actor)
        .await?;

    // Re-read for the resolved view (status / resolved_at / resolved_by now set).
    let resolved = state
        .repo
        .read(&scope, tenant, exception_id)
        .await?
        .ok_or_else(|| exception_not_found(exception_id))?;
    Ok((StatusCode::OK, Json(ExceptionView::from(resolved))).into_response())
}
