//! Axum handlers + router for the tenant invoice-posting policy (VHP-1853).
//! `POST /bss-ledger/v1/posting-policy` appends an effective-dated version (the
//! missing-mapping mode + the AR-aging bucket thresholds); `GET …` reads the
//! caller tenant's effective policy. Tenant-scoped to the caller's own tenant
//! (no tenant in the path/body for the write). The write gates on
//! `(ledger_config, write)`, the read on `(ledger_config, read)` — the shared
//! config-plane resource (the FX revaluation mode shares it; dual-control policy
//! stays separate), not the `entry` data plane.

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
use crate::domain::instant::rfc3339;
use crate::domain::invoice::policy::{AgingThresholds, MissingMappingMode, PostingPolicy};
use crate::infra::storage::repo::PostingPolicyRepo;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the posting-policy operations.
const TAG: &str = "BSS Ledger Posting Policy";

/// Shared per-request state for the posting-policy routes. Constructed once at
/// `init()` and shared via `Extension<Arc<ApiState>>`.
#[derive(Clone)]
pub struct ApiState {
    /// The posting-policy repository (read effective + write a version).
    pub posting_policy: PostingPolicyRepo,
}

/// A new posting-policy version to write (VHP-1853). `effective_from` defaults to
/// now when omitted. An unknown `missing_mapping_mode`, or empty / non-positive /
/// non-monotone `ar_aging_thresholds`, is rejected (400), never coerced.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetPostingPolicyRequest {
    /// `SUSPENSE` (route unmapped items to suspense — the default) or `HARD_BLOCK`
    /// (reject a post that carries an unmapped item).
    pub missing_mapping_mode: String,
    /// AR-aging bucket upper-bounds: strictly increasing positive day counts
    /// (e.g. `[30, 60, 90]` → `current / 1-30 / 31-60 / 61-90 / 90+`).
    pub ar_aging_thresholds: Vec<i64>,
    /// When this version takes effect; defaults to now.
    #[serde(default, with = "rfc3339::option")]
    pub effective_from: Option<OffsetDateTime>,
}

/// The written posting-policy version (the minted `version` + the values it
/// carries; the resolver picks the latest `effective_from`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PostingPolicyResponse {
    /// The minted version (`max + 1`, `0` for the first).
    pub version: i64,
    /// The instant this version takes effect.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// `SUSPENSE` | `HARD_BLOCK`.
    pub missing_mapping_mode: String,
    /// The AR-aging bucket upper-bounds.
    pub ar_aging_thresholds: Vec<i64>,
}

/// The tenant's effective posting policy (read-surface) — the configured version
/// in force, or the gear defaults when the tenant has set none.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PostingPolicyView {
    /// `SUSPENSE` | `HARD_BLOCK`.
    pub missing_mapping_mode: String,
    /// The AR-aging bucket upper-bounds.
    pub ar_aging_thresholds: Vec<i64>,
}

/// The `?tenant_id=` query for `GET /posting-policy`: the tenant whose effective
/// policy to read — the caller's own when omitted.
#[derive(Debug, serde::Deserialize)]
struct PolicyQuery {
    tenant_id: Option<Uuid>,
}

/// Build the Axum router for the posting-policy surface and register its
/// operations with the supplied `OpenAPI` registry.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/posting-policy")
        .operation_id("bss_ledger.set_posting_policy")
        .summary("Set the tenant invoice-posting policy")
        .description(
            "Writes a new effective-dated invoice-posting policy version (the \
             missing-mapping mode + the AR-aging bucket thresholds) for the \
             caller's tenant (VHP-1853). Append-only: a new version supersedes; \
             the orchestrator / aging read pick the latest effective_from (highest \
             version on a tie). An unknown mode or empty / non-positive / \
             non-monotone thresholds are rejected (400), never coerced. Requires \
             `config_write.v1`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<SetPostingPolicyRequest>(openapi, "The policy version to write.")
        .handler(set_policy)
        .json_response_with_schema::<PostingPolicyResponse>(
            openapi,
            StatusCode::OK,
            "The written policy version.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/posting-policy")
        .operation_id("bss_ledger.get_posting_policy")
        .summary("Read the effective invoice-posting policy")
        .description(
            "Returns the tenant's EFFECTIVE invoice-posting policy: the version in \
             force now (latest `effective_from <= now`, highest `version` on a \
             tie), or the gear defaults (`SUSPENSE` + `30,60,90`) when the tenant \
             has set no policy row. `tenant_id` defaults to the caller's own. Gates \
             on `(ledger_config, read)` — the shared config-plane resource, not an \
             `entry` data-plane read; tenant-scoped (SQL-level BOLA) so a tenant \
             outside the caller's subtree reads the gear defaults (no leak). \
             Always `200`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "tenant_id",
            false,
            "Tenant whose effective policy to read (the caller's own by default)",
        )
        .handler(get_policy)
        .json_response_with_schema::<PostingPolicyView>(
            openapi,
            StatusCode::OK,
            "The effective invoice-posting policy (configured version or gear defaults).",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// `POST /posting-policy`: write a new effective-dated version. Gates on
/// `(posting_policy, write)`; validates the request into the domain types (400 on
/// a bad mode / thresholds) before the append.
async fn set_policy(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<SetPostingPolicyRequest>,
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
    let missing_mapping_mode = MissingMappingMode::parse(&body.missing_mapping_mode)
        .map_err(|e| CanonicalError::from(DomainError::InvalidRequest(e.to_string())))?;
    let aging_thresholds = AgingThresholds::new(body.ar_aging_thresholds.clone())
        .map_err(|e| CanonicalError::from(DomainError::InvalidRequest(e.to_string())))?;
    let policy = PostingPolicy {
        missing_mapping_mode,
        aging_thresholds,
    };
    let effective_from = body.effective_from.unwrap_or_else(OffsetDateTime::now_utc);
    let version = state
        .posting_policy
        .write_version(&scope, tenant_id, &policy, effective_from)
        .await
        .map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!("write posting policy: {e}")))
        })?;
    Ok((
        StatusCode::OK,
        Json(PostingPolicyResponse {
            version,
            effective_from,
            missing_mapping_mode: body.missing_mapping_mode,
            ar_aging_thresholds: body.ar_aging_thresholds,
        }),
    )
        .into_response())
}

/// `GET /posting-policy`: read the caller tenant's effective policy. Gates on
/// `(posting_policy, read)`; binds the compiled scope as the SQL-level BOLA
/// filter. Always `200` — the effective policy is the configured version or the
/// gear defaults.
async fn get_policy(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<PolicyQuery>,
) -> Result<Json<PostingPolicyView>, CanonicalError> {
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
        .posting_policy
        .read_effective_policy(&scope, tenant_id, OffsetDateTime::now_utc())
        .await
        .map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!("read posting policy: {e}")))
        })?;
    Ok(Json(PostingPolicyView {
        missing_mapping_mode: effective.missing_mapping_mode.as_str().to_owned(),
        ar_aging_thresholds: effective.aging_thresholds.bounds().to_vec(),
    }))
}
