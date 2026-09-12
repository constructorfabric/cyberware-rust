//! Axum handlers + router for the ledger's FX & multi-currency REST surface
//! (Slice 5, design §5). Two operations under `/bss-ledger/v1`, tenant-scoped
//! WITHOUT a tenant in the path (the vhp-core convention): the write carries
//! `tenant_id` in the **body**, the read takes a `?tenant_id=` **query** param.
//!
//! - `POST /fx/rates` — the SECONDARY manual / seed ingest of one rate into the
//!   local `ledger_fx_rate` store (the PRIMARY path is the `RateProviderV1`
//!   plugin pull via `RateSyncJob`, decision 2). Upsert-keyed on `(tenant, base,
//!   quote, provider)`; idempotent on `(tenant, base, quote, provider, as_of)`.
//!   `(ledger, provision)` PEP gate against the body's `tenant_id` — FX rates are
//!   ledger reference data, the same family as the currency scales seeded under
//!   `ledger/provision` (lean authz reuse, spec §5; no new resource).
//! - `GET /fx/rate-snapshots/{rate_id}` — read one immutable
//!   `ledger_fx_rate_snapshot` (the frozen rate a journal line's
//!   `rate_snapshot_ref` points at). `(ledger, read)` PEP gate; the compiled read
//!   scope is the SQL-level BOLA filter (a foreign-tenant or absent snapshot is
//!   the same 404, no existence leak).
//!
//! Routes register through `OperationBuilder` so `/openapi.json` lists each
//! operation with its declared request / response schemas. Mirrors
//! `payments::router` / `credit::router`.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::dto::{
    FxRateIngestRequest, FxRateIngestResponse, FxRateSnapshotResponse, RevaluationRunRequest,
    RevaluationRunResponse, RevaluationScopeOutcomeDto,
};
use crate::api::rest::error::{authz_error_to_canonical, rate_snapshot_not_found};
use crate::domain::error::DomainError;
use crate::domain::fx::revaluation_mode::RevaluationMode;
use crate::infra::fx::revaluation_run::{ScopeOutcome, ScopeStatus, UnrealizedRevaluationRun};
use crate::infra::storage::repo::{FxRepo, FxRevaluationModeRepo, NewFxRate};
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the FX operations.
const TAG: &str = "BSS Ledger FX";

/// Shared per-request state for the FX routes. Constructed once at `init()` and
/// shared via `Extension<Arc<ApiState>>`. Carries the FX repo — the ingest upsert
/// and the snapshot read go straight to it (there is no in-process client method
/// for reference-rate data, unlike the posting surfaces).
#[derive(Clone)]
pub struct ApiState {
    /// FX rate store + immutable snapshot repository.
    pub fx_repo: FxRepo,
    /// Mode-B unrealized-revaluation runner (the `POST /fx/revaluation-runs`
    /// trigger). `Arc` because the runner holds non-`Clone` posting/repo handles.
    pub revaluation_run: Arc<UnrealizedRevaluationRun>,
    /// Per-tenant FX revaluation mode (VHP-1986): the trigger resolves the target
    /// tenant's effective Mode A/B and only runs the revaluation for Mode B.
    pub fx_revaluation_mode: FxRevaluationModeRepo,
    /// The global `fx.revaluation_enabled` fleet default for a tenant with no
    /// explicit mode row (on→ModeB, off→ModeA).
    pub fleet_revaluation_enabled: bool,
}

/// Build the Axum router for the FX surface and register both operations with the
/// supplied `OpenAPI` registry. `state` is attached via an `Extension` layer at
/// the end so the registry sees the route definitions before the per-request
/// state is bound. Mirrors [`crate::api::rest::payments::router`].
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/fx/rates")
        .operation_id("bss_ledger.ingest_fx_rate")
        .summary("Ingest an FX rate into the local store (secondary manual / seed path)")
        .description(
            "Upserts one `base → quote` rate into the seller's local \
             `ledger_fx_rate` store for the tenant named by the body's \
             `tenant_id`. This is the SECONDARY ingest path — the primary is the \
             `RateProviderV1` adapter pull driven by the background rate-sync job. \
             Upsert-keyed on `(tenant, base, quote, provider)`: re-posting the same \
             tuple overwrites the quote (`rate_micro` / `as_of` / `fallback_order`), \
             so it is idempotent on `(tenant, base, quote, provider, as_of)`. \
             Rejected (400) on an empty/oversized currency or provider code, a \
             non-positive `rate_micro`, a negative `fallback_order`, or an identity \
             (`base == quote`) pair. `(ledger, provision)` PEP gate against the \
             body's `tenant_id`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<FxRateIngestRequest>(
            openapi,
            "The rate to upsert into the local store (tenant in the body).",
        )
        .handler(ingest_fx_rate)
        .json_response_with_schema::<FxRateIngestResponse>(
            openapi,
            StatusCode::OK,
            "The now-current stored rate (key + quote), echoed back",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/fx/rate-snapshots/{rate_id}")
        .operation_id("bss_ledger.read_fx_rate_snapshot")
        .summary("Read an immutable FX rate snapshot")
        .description(
            "Returns the immutable `ledger_fx_rate_snapshot` for `{rate_id}` — the \
             frozen rate a journal line's `rate_snapshot_ref` points at, \
             reproducing its exact lock-time translation. `?tenant_id=` defaults to \
             the caller's own. A snapshot that does not exist for `(tenant, \
             rate_id)`, or that lies outside the caller's authorized subtree, is \
             the same 404 (SQL-level BOLA, no existence leak). `(ledger, read)` PEP \
             gate.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("rate_id", "The snapshot's immutable rate id (uuid).")
        .query_param(
            "tenant_id",
            false,
            "Target tenant (defaults to the caller's own)",
        )
        .handler(read_fx_rate_snapshot)
        .json_response_with_schema::<FxRateSnapshotResponse>(
            openapi,
            StatusCode::OK,
            "The immutable rate snapshot",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/fx/revaluation-runs")
        .operation_id("bss_ledger.trigger_revaluation")
        .summary("Trigger an unrealized (Mode-B) revaluation for a period")
        .description(
            "Remeasures the tenant's open foreign-currency MONETARY positions \
             `{AR, UNALLOCATED, REUSABLE_CREDIT}` at the period-end rate against \
             their carried functional value, posting one functional-only \
             `FX_UNREALIZED` entry per scope that moved (design §4.5). Idempotent \
             per `(tenant, period_id, scope)` — re-posting the same period replays \
             the already-posted scopes. A no-op when the tenant is Mode-A \
             (`revaluation_enabled = false`). The reversal is a separate \
             next-period JE posted by the background job (decision 7). `period_id` \
             must be an OPEN period (the run posts into it). `(ledger, provision)` \
             PEP gate against the body's `tenant_id` (finance scope, the same \
             family as the rate ingest).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RevaluationRunRequest>(
            openapi,
            "The period to revalue (tenant in the body).",
        )
        .handler(trigger_revaluation)
        .json_response_with_schema::<RevaluationRunResponse>(
            openapi,
            StatusCode::OK,
            "Per-scope revaluation outcomes",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400) BEFORE
// the in-handler `require_authenticated` gate, so a malformed body yields 400 even
// for an unauthenticated caller (standard axum extractor ordering; no
// authenticated-only data is disclosed). Mirrors `payments::settle_payment`.
async fn ingest_fx_rate(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<FxRateIngestRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id` (tenant in body, not path).
    let tenant_id = body.tenant_id;
    // (ledger, provision) gate against the TARGET tenant: FX rates are ledger
    // reference data (the same family as currency scales seeded under
    // `ledger/provision`); a target outside the caller's authorized scope is a
    // cross-tenant write and is denied. The upsert re-scopes to `for_tenant` inside
    // the repo — this gate is the authorization.
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

    // Validate the body BEFORE the write (returns the defaulted `fallback_order`),
    // then move it into the row parameter object.
    let fallback_order = body.validate()?;
    let upsert = NewFxRate {
        tenant_id,
        base_currency: body.base_currency,
        quote_currency: body.quote_currency,
        provider: body.provider,
        rate_micro: body.rate_micro,
        as_of: body.as_of,
        fallback_order,
    };
    state
        .fx_repo
        .upsert_rate(&upsert)
        .await
        .map_err(|e| CanonicalError::from(DomainError::Internal(format!("fx rate ingest: {e}"))))?;

    Ok((
        StatusCode::OK,
        Json(FxRateIngestResponse {
            tenant_id: upsert.tenant_id,
            base_currency: upsert.base_currency,
            quote_currency: upsert.quote_currency,
            provider: upsert.provider,
            rate_micro: upsert.rate_micro,
            as_of: upsert.as_of,
            fallback_order: upsert.fallback_order,
        }),
    )
        .into_response())
}

/// `GET /fx/rate-snapshots/{rate_id}` query parameters. `tenant_id` defaults to
/// the caller's own; the compiled read scope is the SQL-level BOLA filter.
#[derive(Debug, serde::Deserialize)]
struct SnapshotQuery {
    tenant_id: Option<Uuid>,
}

async fn read_fx_rate_snapshot(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(rate_id): Path<Uuid>,
    Query(query): Query<SnapshotQuery>,
) -> Result<Json<FxRateSnapshotResponse>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // (ledger, read) gate: reads pass `owner_tenant_id = None` (the PDP derives the
    // scope from the subject + role; the returned scope is the SQL filter), pinning
    // the single snapshot row via `resource_id`. `require_constraints = true` so an
    // unconstrained allow fail-closes instead of leaking every tenant.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::LEDGER,
        crate::authz::actions::READ,
        None,
        Some(rate_id),
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let tenant_id = query.tenant_id.unwrap_or_else(|| ctx.subject_tenant_id());
    let snapshot = state
        .fx_repo
        .read_snapshot(&scope, tenant_id, rate_id)
        .await
        .map_err(|e| CanonicalError::from(DomainError::Internal(format!("fx snapshot read: {e}"))))?
        .ok_or_else(|| rate_snapshot_not_found(rate_id))?;

    Ok(Json(FxRateSnapshotResponse::from(snapshot)))
}

// `CanonicalJson` runs (and may 400) BEFORE the in-handler auth gate (standard
// axum extractor order; no authenticated-only data is disclosed).
async fn trigger_revaluation(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<RevaluationRunRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = body.tenant_id;
    // (ledger, provision) gate against the TARGET tenant: triggering a revaluation
    // run is a finance/ops action on the seller's books (the same family as the FX
    // rate ingest); the returned scope is the SQL-level write scope for the run.
    let scope = crate::authz::access_scope(
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

    body.validate()?;
    // VHP-1986 per-tenant gate: resolve the target tenant's effective Mode A/B (an
    // explicit row wins; else the fleet default). Mode A defers to the tenant's ERP
    // → every scope reports `disabled` (no double-count); Mode B runs the revaluation.
    let revalue = state
        .fx_revaluation_mode
        .read_effective_mode(&scope, tenant_id, OffsetDateTime::now_utc())
        .await
        .map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!(
                "read fx revaluation mode: {e}"
            )))
        })?
        .unwrap_or(RevaluationMode::fleet_default(
            state.fleet_revaluation_enabled,
        ))
        .revalues();
    let report = state
        .revaluation_run
        .run_period(&ctx, &scope, tenant_id, &body.period_id, revalue)
        .await
        .map_err(CanonicalError::from)?;

    let scopes = report.scopes.iter().map(scope_outcome_dto).collect();
    Ok((
        StatusCode::OK,
        Json(RevaluationRunResponse {
            period_id: report.period_id,
            scopes,
        }),
    )
        .into_response())
}

/// Map a runner [`ScopeOutcome`] to the wire DTO (status string + entry/grain
/// counts). The reversal statuses never arise on the forward `run_period` path but
/// are mapped for completeness (the enum is shared with the job's reversal pass).
fn scope_outcome_dto(outcome: &ScopeOutcome) -> RevaluationScopeOutcomeDto {
    let (status, entries, grains) = match &outcome.status {
        ScopeStatus::Disabled => ("disabled", 0, 0),
        ScopeStatus::NothingToPost => ("nothing_to_post", 0, 0),
        ScopeStatus::Posted { entries, grains } => (
            "posted",
            i64::try_from(*entries).unwrap_or(i64::MAX),
            i64::try_from(*grains).unwrap_or(i64::MAX),
        ),
        ScopeStatus::NothingToReverse => ("nothing_to_reverse", 0, 0),
        ScopeStatus::ReversalDeferred => ("reversal_deferred", 0, 0),
        ScopeStatus::Reversed { entries } => {
            ("reversed", i64::try_from(*entries).unwrap_or(i64::MAX), 0)
        }
    };
    RevaluationScopeOutcomeDto {
        scope: outcome.scope.as_token().to_owned(),
        status: status.to_owned(),
        entries,
        grains,
    }
}
