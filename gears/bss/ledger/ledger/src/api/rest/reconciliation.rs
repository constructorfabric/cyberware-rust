//! Axum handlers + router for the reconciliation surface (Slice 7 Phase 3,
//! design §4.3 / §4 / §5). Two operations under `/bss-ledger/v1/ledger`,
//! tenant-scoped WITHOUT a tenant in the path (the vhp-core convention — the
//! tenant is the caller's auth context).
//!
//! - `POST /ledger/reconciliation-runs` — trigger one named reconciliation check
//!   (`AR_DERIVED` / `PAYMENTS_PSP` / `INVOICE_COMPLETENESS`) for the caller's own
//!   tenant + a period; returns the new `run_id`. The check reads + computes a
//!   variance and writes a durable `reconciliation_run` row (Slice 7 posts no
//!   financial entries — it reads, reconciles, and gates close). Gates on
//!   `RECONCILIATION:run`. The Payments↔PSP / invoice-completeness checks are
//!   inert until their control feed lands (a not-configured feed ⇒ 400, design §0
//!   decision 3).
//! - `GET /ledger/reconciliation-runs/{run_id}` — read one run's variance result.
//!   Gates on `RECONCILIATION:read`; the compiled read scope is the SQL-level BOLA
//!   filter, so a foreign-tenant or absent run is the same 404 (no existence leak).
//!
//! Routes register through `OperationBuilder` so `/openapi.json` lists each
//! operation with its declared request / response schemas. Mirrors
//! `exceptions::router` / `fx::router`.

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::error::{authz_error_to_canonical, reconciliation_run_not_found};
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::infra::reconciliation::{
    CHECK_AR_DERIVED, CHECK_INVOICE_COMPLETENESS, CHECK_PAYMENTS_PSP, ReconciliationFramework,
};
use crate::infra::storage::entity::reconciliation_run;
use crate::infra::storage::repo::ReconciliationRunRepo;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the reconciliation operations.
const TAG: &str = "BSS Ledger Reconciliation";

/// Shared per-request state for the reconciliation routes. Constructed once at
/// `init()` and shared via `Extension<Arc<ApiState>>`. Carries the framework (the
/// `run` trigger) + the run repo (the by-id read).
#[derive(Clone)]
pub struct ApiState {
    /// The reconciliation engine — `run_check` runs one named check + writes the
    /// durable run row. `Arc` because the framework holds non-`Clone` posting /
    /// feed handles.
    pub framework: Arc<ReconciliationFramework>,
    /// The reconciliation-run repository (by-id read; SQL-level BOLA).
    pub run_repo: ReconciliationRunRepo,
}

/// `POST /ledger/reconciliation-runs` request body: the period + the named check
/// to run for the caller's own tenant.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct TriggerReconciliationRunRequest {
    /// The fiscal `period_id` (`YYYYMM`) to reconcile.
    pub period_id: String,
    /// The named check: `AR_DERIVED`, `PAYMENTS_PSP`, or `INVOICE_COMPLETENESS`.
    pub check_type: String,
}

/// `POST /ledger/reconciliation-runs` response: the new run's id.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TriggerReconciliationRunResponse {
    /// The server-minted reconciliation-run id (the `GET …/{run_id}` key tail).
    pub run_id: Uuid,
}

/// One reconciliation-run row, projected for the read. PII-free (ids + variance).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReconciliationRunView {
    pub run_id: Uuid,
    pub period_id: String,
    pub check_type: String,
    pub variance_minor: i64,
    pub within_tolerance: bool,
    pub status: String,
    #[serde(with = "rfc3339")]
    pub at_utc: OffsetDateTime,
}

impl From<reconciliation_run::Model> for ReconciliationRunView {
    fn from(m: reconciliation_run::Model) -> Self {
        Self {
            run_id: m.run_id,
            period_id: m.period_id,
            check_type: m.check_type,
            variance_minor: m.variance_minor,
            within_tolerance: m.within_tolerance,
            status: m.status,
            at_utc: m.at_utc,
        }
    }
}

/// Build the Axum router for the reconciliation surface and register both
/// operations with the supplied `OpenAPI` registry. `state` is attached via an
/// `Extension` layer at the end so the registry sees the route definitions before
/// the per-request state is bound. Mirrors [`crate::api::rest::exceptions::router`].
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/ledger/reconciliation-runs")
        .operation_id("bss_ledger.trigger_reconciliation_run")
        .summary("Trigger a reconciliation check for a period")
        .description(
            "Runs one named reconciliation check (`AR_DERIVED`, `PAYMENTS_PSP`, or \
             `INVOICE_COMPLETENESS`) for the caller's own tenant + `period_id`, \
             writing a durable `reconciliation_run` row with its variance result. \
             An out-of-tolerance run opens a close-blocking exception + raises an \
             alarm. Slice 7 posts no financial entries — it reads, reconciles, and \
             gates close. The Payments↔PSP / invoice-completeness checks are inert \
             until their control feed lands (a not-configured feed ⇒ 400). \
             `(reconciliation, run)` PEP gate against the caller's own tenant.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<TriggerReconciliationRunRequest>(
            openapi,
            "The period + the named check to run.",
        )
        .handler(trigger_reconciliation_run)
        .json_response_with_schema::<TriggerReconciliationRunResponse>(
            openapi,
            StatusCode::CREATED,
            "The new reconciliation run's id (its canonical URL is in `Location`).",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/ledger/reconciliation-runs/{run_id}")
        .operation_id("bss_ledger.read_reconciliation_run")
        .summary("Read a reconciliation run's variance result")
        .description(
            "Returns one `reconciliation_run` for `{run_id}` — the check type, the \
             reconciled `variance_minor`, whether it is within tolerance, the run \
             status, and when it ran. Tenant-scoped (SQL-level BOLA): a run that \
             does not exist for the caller's tenant, or that lies outside the \
             caller's authorized subtree, is the same 404 (no existence leak). \
             `(reconciliation, read)` PEP gate.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("run_id", "The reconciliation run's id (uuid).")
        .handler(read_reconciliation_run)
        .json_response_with_schema::<ReconciliationRunView>(
            openapi,
            StatusCode::OK,
            "The reconciliation run.",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// Validate a `check_type` literal against the three known checks at the boundary
/// (a bad literal is a clean 400, not a deep `run_check` rejection).
fn validate_check_type(check_type: &str) -> Result<(), DomainError> {
    if matches!(
        check_type,
        CHECK_AR_DERIVED | CHECK_PAYMENTS_PSP | CHECK_INVOICE_COMPLETENESS
    ) {
        Ok(())
    } else {
        Err(DomainError::InvalidRequest(format!(
            "unknown check_type {check_type:?} (expected \"{CHECK_AR_DERIVED}\", \
             \"{CHECK_PAYMENTS_PSP}\", or \"{CHECK_INVOICE_COMPLETENESS}\")"
        )))
    }
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400) BEFORE
// the in-handler `require_authenticated` gate, so a malformed body yields 400 even
// for an unauthenticated caller (standard axum extractor ordering; no
// authenticated-only data is disclosed). Mirrors `fx::ingest_fx_rate`.
async fn trigger_reconciliation_run(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<TriggerReconciliationRunRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The reconciliation targets the caller's own ledger (the run is a
    // self-service Revenue-Assurance action on the caller's books).
    let tenant = ctx.subject_tenant_id();
    // (reconciliation, run) gate against the caller's own tenant: triggering a
    // check is a write (it persists a run row); the membership assertion fail-closes
    // a target outside the caller's authorized scope.
    crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECONCILIATION,
        crate::authz::actions::RUN,
        Some(tenant),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Reject an unknown check_type at the boundary (a clean 400).
    validate_check_type(&body.check_type)?;

    let run_id = state
        .framework
        .run_check(&ctx, tenant, &body.period_id, &body.check_type)
        .await
        .map_err(CanonicalError::from)?;

    // 201 Created + a `Location` to the new run's by-id read (the server minted the
    // `run_id`), mirroring how `refunds::post_refund` returns a created resource.
    let location = format!("/bss-ledger/v1/ledger/reconciliation-runs/{run_id}");
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(TriggerReconciliationRunResponse { run_id }),
    )
        .into_response())
}

/// `GET …/reconciliation-runs/{run_id}`: read one run (RECONCILIATION:read).
async fn read_reconciliation_run(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<ReconciliationRunView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // (reconciliation, read) gate: reads pass `owner_tenant_id = None` (the PDP
    // derives the scope from the subject + role; the returned scope is the SQL
    // filter), pinning the single run row via `resource_id`. `require_constraints =
    // true` so an unconstrained allow fail-closes instead of leaking every tenant.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECONCILIATION,
        crate::authz::actions::READ,
        None,
        Some(run_id),
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let tenant = ctx.subject_tenant_id();
    let row = state
        .run_repo
        .read(&scope, tenant, run_id)
        .await?
        .ok_or_else(|| reconciliation_run_not_found(run_id))?;

    Ok(Json(ReconciliationRunView::from(row)))
}
