//! Axum handlers + router for the gear's tenant-scoped REST surface:
//! `POST /bss-ledger/v1/provisioning` (seed the ledger; target tenant in the
//! body) and `GET /bss-ledger/v1/accounts` (list the chart of accounts; seller
//! from `$filter=tenant_id eq …`, the caller's own by default).
//! Tenant is carried in body/query (not the path) per the vhp-core REST
//! convention (RBAC/RMS) — see the gear's invoice-posting impl design.
//!
//! The gear's first REST surface. Translates the provisioning request into the
//! in-process `LedgerClientV1::provision` call and
//! renders the per-grain created-vs-existing summary. Requests without an
//! authenticated `SecurityContext` are rejected with 401; the `billing-setup`
//! PEP gate (`(ledger, provision)` against the TARGET tenant, which must
//! lie in the caller's authorized subtree) rejects an unauthorized or
//! cross-tenant caller with 403 (503 if the PDP is unreachable).
//!
//! Routes are registered through `toolkit::api::operation_builder::OperationBuilder`
//! so the `OpenAPI` document at `/openapi.json` lists the operation with its
//! declared request / response schemas.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::{Json, Router, http::StatusCode};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::dto::{AccountInfoDto, ProvisioningRequestDto, ProvisioningResultDto};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{list_seller_tenant, reject_non_odata_list_params};
use crate::odata::{AccountInfoFilterField, AccountInfoOrderField};

/// `OpenAPI` tag applied to the provisioning operation.
const TAG: &str = "BSS Ledger Provisioning";

/// Shared per-request state for the provisioning route. Constructed once at
/// `init()` and shared via `Extension<Arc<ApiState>>`.
#[derive(Clone)]
pub struct ApiState {
    /// In-process data-access client (the gear's own local impl).
    pub client: std::sync::Arc<dyn bss_ledger_sdk::api::LedgerClientV1>,
}

/// Build the Axum router for the provisioning endpoint and register the
/// operation with the supplied `OpenAPI` registry. `state` is attached via an
/// `Extension` layer at the end so the registry sees the route definition
/// before the per-request state is bound.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/provisioning")
        .operation_id("bss_ledger.provision")
        .summary("Provision a seller legal-entity (idempotent, additive)")
        .description(
            "Seeds the chart of accounts, non-ISO currency scales, the \
             fiscal-calendar config, and the initial OPEN fiscal period for the \
             seller legal-entity named by the body's `tenant_id`. Re-calls are \
             additive: existing rows are a no-op.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ProvisioningRequestDto>(
            openapi,
            "Accounts, currency scales, and fiscal-calendar config to seed.",
        )
        .handler(provision)
        .json_response_with_schema::<ProvisioningResultDto>(
            openapi,
            StatusCode::OK,
            "Provisioning summary (created vs. existing per grain)",
        )
        // Malformed body / bad enum literals / out-of-range scale all surface
        // as canonical `InvalidArgument` = HTTP 400 (the canonical taxonomy has
        // no 422; this projects the architecture's RFC-9457 codes onto the
        // platform error layer, like every other vhp-core gear).
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/accounts")
        .operation_id("bss_ledger.list_accounts")
        .summary("List a tenant's chart of accounts (with ids)")
        .description(
            "Cursor-paginated list of the target tenant's chart-of-accounts \
             entries — each account's persistent id + coordinate — so a caller \
             can resolve the ids it posts to / reads balances for. The target is \
             `$filter=tenant_id eq <uuid>` (the caller's own tenant when omitted). \
             Supports OData `$filter` over `tenant_id`, `account_class`, `currency`, \
             `revenue_stream`, and `lifecycle_state`. Tenant-scoped: the \
             `$filter` ANDs the caller's authorized subtree, so a tenant outside \
             it yields an empty page (no leak, not a 403).",
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
        .handler(list_accounts)
        .with_odata_filter::<AccountInfoFilterField>()
        .with_odata_orderby::<AccountInfoOrderField>()
        .json_response_with_schema::<Page<AccountInfoDto>>(
            openapi,
            StatusCode::OK,
            "One page of the tenant's chart of accounts with ids",
        )
        // A malformed `tenant_id` query / `$filter` / cursor surfaces as a 400.
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400)
// BEFORE the in-handler `require_authenticated` gate, so a malformed body
// yields 400 even for an unauthenticated caller. Accepted: standard axum
// extractor ordering, and no authenticated-only data is disclosed.
async fn provision(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<ProvisioningRequestDto>,
) -> Result<Json<ProvisioningResultDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id` (tenant in body, not path).
    let tenant_id = body.tenant_id;
    // billing-setup PEP gate: authorize (ledger, provision) against the
    // TARGET tenant. require_constraints=true so the degraded flat-In PDP scope
    // must contain the target — a parent provisions a seller in its authorized
    // subtree; a target outside the caller's scope is a cross-tenant write and
    // is denied. Self-provision is the case target ∈ the caller's own subtree.
    crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::LEDGER,
        crate::authz::actions::PROVISION,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let req = body.into_request().map_err(CanonicalError::from)?;
    let outcome = state.client.provision(&ctx, req).await?;
    Ok(Json(outcome.into()))
}

/// `GET …/accounts`: seller from `$filter=tenant_id eq`, else the caller.
/// Named `?tenant_id=` is 400 (AM list contract).
#[allow(clippy::implicit_hasher)]
async fn list_accounts(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<AccountInfoDto>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    // Target tenant from `$filter`, the caller's own tenant by default. The
    // in-process client's PDP scope is the SQL-level BOLA filter (a tenant
    // outside the caller's subtree yields an empty page), so there is no
    // separate target-anchored gate here. The `$filter` ANDs that scope.
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    let page = state.client.list_accounts(&ctx, tenant_id, &odata).await?;
    Ok(Json(Page {
        items: page.items.into_iter().map(AccountInfoDto::from).collect(),
        page_info: page.page_info,
    }))
}
