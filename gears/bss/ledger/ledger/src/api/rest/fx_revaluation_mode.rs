//! Axum handlers + router for the per-tenant FX revaluation mode (VHP-1986).
//! `POST /bss-ledger/v1/fx/revaluation-mode` appends an effective-dated version
//! (`MODE_A` defer-to-ERP | `MODE_B` BSS-is-ledger-of-record); `GET …` reads the
//! tenant's effective mode. The write gates on `(ledger_config, write)`, the read
//! on `(ledger_config, read)` — the shared config-plane resource (it shares the
//! posting-policy config resource), not the `entry` data plane.

use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::error::authz_error_to_canonical;
use crate::domain::error::DomainError;
use crate::domain::fx::revaluation_mode::RevaluationMode;
use crate::domain::instant::rfc3339;
use crate::infra::storage::repo::FxRevaluationModeRepo;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the FX revaluation-mode operations.
const TAG: &str = "BSS Ledger FX Revaluation Mode";

/// Shared per-request state for the FX revaluation-mode routes. Constructed once
/// at `init()` and shared via `Extension<Arc<ApiState>>`.
#[derive(Clone)]
pub struct ApiState {
    /// The FX revaluation-mode repository (read effective + write a version).
    pub fx_revaluation_mode: FxRevaluationModeRepo,
}

/// A new FX revaluation-mode version to write (VHP-1986). `effective_from`
/// defaults to now when omitted. An unknown `revaluation_mode` is rejected (400),
/// never coerced.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetFxRevaluationModeRequest {
    /// `MODE_A` (defer the period-end revaluation to the tenant's ERP — the
    /// fail-safe default) or `MODE_B` (BSS is the ledger of record and runs the
    /// unrealized revaluation).
    pub revaluation_mode: String,
    /// When this version takes effect; defaults to now.
    #[serde(default, with = "rfc3339::option")]
    pub effective_from: Option<OffsetDateTime>,
}

/// The written FX revaluation-mode version (the minted `version` + the value it
/// carries; the resolver picks the latest `effective_from`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct FxRevaluationModeResponse {
    /// The minted version (`max + 1`, `0` for the first).
    pub version: i64,
    /// The instant this version takes effect.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// `MODE_A` | `MODE_B`.
    pub revaluation_mode: String,
}

/// The tenant's effective FX revaluation mode (read-surface) — the configured
/// version in force, or the gear default (`MODE_A`) when the tenant has set none.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct FxRevaluationModeView {
    /// `MODE_A` | `MODE_B`.
    pub revaluation_mode: String,
}

/// The `?tenant_id=` query for `GET /fx/revaluation-mode`: the tenant whose
/// effective mode to read — the caller's own when omitted.
#[derive(Debug, serde::Deserialize)]
struct ModeQuery {
    tenant_id: Option<Uuid>,
}

/// Build the Axum router for the FX revaluation-mode surface and register its
/// operations with the supplied `OpenAPI` registry.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/fx/revaluation-mode")
        .operation_id("bss_ledger.set_fx_revaluation_mode")
        .summary("Set the tenant FX revaluation mode")
        .description(
            "Writes a new effective-dated FX revaluation-mode version (`MODE_A` \
             defer-to-ERP | `MODE_B` BSS-is-ledger-of-record) for the caller's \
             tenant (VHP-1986). Append-only: a new version supersedes; the \
             revaluation job / period-close pick the latest effective_from (highest \
             version on a tie). An unknown mode is rejected (400), never coerced. \
             Requires `config_write.v1`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<SetFxRevaluationModeRequest>(openapi, "The mode version to write.")
        .handler(set_mode)
        .json_response_with_schema::<FxRevaluationModeResponse>(
            openapi,
            StatusCode::OK,
            "The written mode version.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/fx/revaluation-mode")
        .operation_id("bss_ledger.get_fx_revaluation_mode")
        .summary("Read the effective FX revaluation mode")
        .description(
            "Returns the tenant's EFFECTIVE FX revaluation mode: the version in \
             force now (latest `effective_from <= now`, highest `version` on a \
             tie), or the gear default (`MODE_A`, fail-safe off) when the tenant \
             has set no mode row. `tenant_id` defaults to the caller's own. Gates \
             on `(ledger_config, read)` — the shared config-plane resource, not \
             an `entry` data-plane read; tenant-scoped (SQL-level BOLA) so a tenant \
             outside the caller's subtree reads the gear default (no leak). Always \
             `200`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "tenant_id",
            false,
            "Tenant whose effective mode to read (the caller's own by default)",
        )
        .handler(get_mode)
        .json_response_with_schema::<FxRevaluationModeView>(
            openapi,
            StatusCode::OK,
            "The effective FX revaluation mode (configured version or gear default).",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// `POST /fx/revaluation-mode`: write a new effective-dated version. Gates on
/// `(fx_revaluation_mode, write)`; validates the request into the domain type (400
/// on a bad mode) before the append.
async fn set_mode(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<SetFxRevaluationModeRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = ctx.subject_tenant_id();
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::LEDGER_CONFIG,
        crate::authz::actions::WRITE,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let mode = RevaluationMode::parse(&body.revaluation_mode)
        .map_err(|e| CanonicalError::from(DomainError::InvalidRequest(e.to_string())))?;
    let effective_from = body.effective_from.unwrap_or_else(OffsetDateTime::now_utc);
    let version = state
        .fx_revaluation_mode
        .write_version(&scope, tenant_id, mode, effective_from)
        .await
        .map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!(
                "write fx revaluation mode: {e}"
            )))
        })?;
    Ok((
        StatusCode::OK,
        Json(FxRevaluationModeResponse {
            version,
            effective_from,
            revaluation_mode: body.revaluation_mode,
        }),
    )
        .into_response())
}

/// `GET /fx/revaluation-mode`: read the caller tenant's effective mode. Gates on
/// `(fx_revaluation_mode, read)`; binds the compiled scope as the SQL-level BOLA
/// filter. Always `200` — the effective mode is the configured version or the gear
/// default (`MODE_A`).
async fn get_mode(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<ModeQuery>,
) -> Result<Json<FxRevaluationModeView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id.unwrap_or_else(|| ctx.subject_tenant_id());
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::LEDGER_CONFIG,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let effective = state
        .fx_revaluation_mode
        .read_effective_mode(&scope, tenant_id, OffsetDateTime::now_utc())
        .await
        .map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!(
                "read fx revaluation mode: {e}"
            )))
        })?
        // The per-tenant configured mode, or the gear default `MODE_A` when unset.
        // (A fleet-wide `fx.revaluation_enabled` may elevate unconfigured tenants
        // to ModeB at the revaluation-job level; this read reports the per-tenant
        // configuration, not the fleet-resolved value.)
        .unwrap_or(RevaluationMode::ModeA);
    Ok(Json(FxRevaluationModeView {
        revaluation_mode: effective.as_str().to_owned(),
    }))
}
