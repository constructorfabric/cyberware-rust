//! Axum handler + router for the ledger's recognition-run REST surface
//! (architecture §5, the ASC 606 S6 release). One write operation under
//! `/bss-ledger/v1`, tenant-scoped WITHOUT a tenant in the path (the vhp-core
//! convention, matching the payment / dispute surfaces): the `tenant_id` is in
//! the **body**.
//!
//! - `POST /recognition-runs` — trigger a recognition run for one fiscal period
//!   (release the period's due `PENDING` segments). The `(recognition, write)` PEP gate
//!   authorizes it against the body's `tenant_id` (a run posts `DR CL / CR
//!   Revenue` journal entries). Idempotent on the run-trigger `run_id` (`None` ⇒
//!   the ledger mints one): a replay returns the prior run reference (200)
//!   without re-running. When every due segment released in period order the run
//!   renders `200` (the run reference + the release tally); when a due segment's
//!   lower-period predecessor was not yet `DONE` the segment is parked `QUEUED`
//!   and the run renders **202** `recognition-period-queued` (a success/queued
//!   token, NOT a rejection — §4.6 ordering).
//!
//! The route registers through `OperationBuilder` so `/openapi.json` lists the
//! operation with its declared request / response schemas. Mirrors the
//! `record_dispute_phase` handler in [`crate::api::rest::disputes`].
//!
// TODO(VHP-1855 I4): the benidorm recognition-lifecycle e2e (deferred post →
// run → schedule GET → change) is cluster-gated and lives in the separate e2e
// suite; not implemented here (it needs a live cluster, out of this slice's
// build-only scope).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};
use bss_ledger_sdk::api::LedgerClientV1;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::AccessScope;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::local_client::map_odata_page_err;
use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::dto::{
    ChangeRecognitionScheduleRequest, RecognitionRunQueuedResponse, RecognitionRunResponse,
    RecognitionRunView, RecognitionScheduleListResponse, RecognitionScheduleResponse,
    RecognitionScheduleSummaryDto, RevenueDisaggregationResponse, ScheduleChangeResponse,
    TriggerRecognitionRunRequest,
};
use crate::api::rest::error::{
    authz_error_to_canonical, recognition_run_not_found, recognition_schedule_not_found,
};
use crate::api::rest::odata_list::{list_seller_tenant, reject_non_odata_list_params};
use crate::infra::storage::repo::RecognitionRepo;
use crate::odata::RecognitionRunFilterField;

/// `OpenAPI` tag applied to the recognition operations.
const TAG: &str = "BSS Ledger Recognition";

/// The recognition handler's dependency on the dual-control gate.
/// Taking the gate behind a trait makes the over-threshold schedule-change
/// path unit-testable with a stub — the router tests previously ran with
/// `approval = None`, so the treatment-gate ordering and the ACTIVE-filter
/// shipped untested. Production binds
/// [`ApprovalService`](crate::infra::approval::service::ApprovalService).
#[async_trait::async_trait]
pub trait RecognitionApprovalGate: Send + Sync {
    /// Resolve the tenant policy and decide whether `facts` crosses the D2
    /// threshold: `Some(approval_id)` (a `PENDING` approval was created — the handler
    /// returns `409`) or `None` (below threshold — proceed inline). Mirrors
    /// [`ApprovalService::gate`](crate::infra::approval::service::ApprovalService::gate).
    ///
    /// # Errors
    /// [`DomainError`](crate::domain::error::DomainError) on a storage failure.
    async fn gate(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        intent: crate::domain::approval::intent::ApprovalIntent,
        facts: crate::domain::approval::policy::OperationFacts,
        reason_code: String,
    ) -> Result<Option<Uuid>, crate::domain::error::DomainError>;
}

#[async_trait::async_trait]
impl RecognitionApprovalGate for crate::infra::approval::service::ApprovalService {
    async fn gate(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        intent: crate::domain::approval::intent::ApprovalIntent,
        facts: crate::domain::approval::policy::OperationFacts,
        reason_code: String,
    ) -> Result<Option<Uuid>, crate::domain::error::DomainError> {
        // Delegate to the inherent method (disambiguated from this trait method).
        crate::infra::approval::service::ApprovalService::gate(
            self,
            ctx,
            scope,
            intent,
            facts,
            reason_code,
        )
        .await
    }
}

/// Shared per-request state for the recognition routes. Constructed once at
/// `init()` and shared via `Extension<Arc<ApiState>>`. Carries the in-process
/// data-access client (the run trigger goes through it — the client gates the
/// PEP and orchestrates the run). Mirrors [`crate::api::rest::payments::ApiState`].
#[derive(Clone)]
pub struct ApiState {
    /// In-process data-access client (the gear's own local impl).
    pub client: Arc<dyn LedgerClientV1>,
    /// Dual-control gate (VHP-1852): a schedule change/cancel whose un-recognized
    /// deferred remainder crosses the D2 threshold routes to the preparer→approver
    /// queue (409); below-threshold changes apply inline. Taken behind
    /// [`RecognitionApprovalGate`] so the gated path is
    /// unit-testable with a stub; `None` skips the gate (router unit tests without a
    /// governance DB). Production binds the real `ApprovalService`.
    pub approval: Option<Arc<dyn RecognitionApprovalGate>>,
    /// The recognition repo — the `GET /recognition-runs` list +
    /// `GET /recognition-runs/{run_id}` by-id read source (the `recognition_run`
    /// row). A plain scoped read, mirroring [`crate::api::rest::disputes::ApiState`]'s
    /// `dispute_repo`. `Option` ONLY so the stub-based REST tests (which carry no DB)
    /// can build `ApiState` with `None` (mirrors `approval` above); production ALWAYS
    /// wires `Some` (see `module.rs`).
    pub recognition_repo: Option<RecognitionRepo>,
}

/// Build the Axum router for the recognition surface and register every
/// operation with the supplied `OpenAPI` registry. `state` is attached via an
/// `Extension` layer at the end so the registry sees the route definitions
/// before the per-request state is bound.
#[allow(clippy::too_many_lines)] // one builder chain per operation; flat is clearer than helpers
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/recognition-runs")
        .operation_id("bss_ledger.trigger_recognition_run")
        .summary("Trigger an ASC 606 recognition run for a fiscal period")
        .description(
            "Releases the period's due PENDING recognition segments for the \
             seller named by the body's `tenant_id`, posting one balanced \
             DR CONTRACT_LIABILITY / CR REVENUE entry per segment (the S6 \
             release). Idempotent on the run-trigger `run_id` (`None` ⇒ the \
             ledger mints one): a replay returns the prior run reference (200) \
             without starting a second run. When every due segment released in \
             period order the run returns 200 (the run reference + release \
             tally). When a due segment's lower-period predecessor is not yet \
             DONE that segment is parked QUEUED for a later drain (202 \
             `recognition-period-queued`, §4.6 ordering) instead of being \
             released out of order. A segment whose own target period has CLOSED \
             releases into the tenant's current open period instead (§4.3 E-2). \
             Rejected when a per-schedule over-recognition cap is exceeded \
             (OVER_RECOGNITION) or a required account class is not provisioned.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<TriggerRecognitionRunRequest>(
            openapi,
            "The fiscal period to run + optional idempotency key.",
        )
        .handler(trigger_recognition_run)
        .json_response_with_schema::<RecognitionRunResponse>(
            openapi,
            StatusCode::OK,
            "The run reference + release tally (ran in period order / idempotent replay)",
        )
        .json_response_with_schema::<RecognitionRunQueuedResponse>(
            openapi,
            StatusCode::ACCEPTED,
            "One or more due segments parked QUEUED out-of-order (recognition-period-queued)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/revenue/disaggregation")
        .operation_id("bss_ledger.revenue_disaggregation")
        .summary("Disaggregate recognized ASC 606 revenue by stream")
        .description(
            "Returns the revenue RECOGNIZED for the `tenant_id` query (the \
             caller's own by default), disaggregated by (period_id, \
             revenue_stream) and ordered by (period_id, revenue_stream). The \
             source is recognized revenue = the DONE recognition segments (each a \
             posted DR CONTRACT_LIABILITY / CR REVENUE release), summed per \
             (period, stream). Omit `period_id` for all periods, or pass one \
             `YYYYMM` to narrow to it. The read is tenant-scoped (SQL-level BOLA): \
             a target outside the caller's authorized subtree yields no entries.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "tenant_id",
            false,
            "Target seller tenant (defaults to the caller's own)",
        )
        .query_param("period_id", false, "Narrow to one fiscal period (YYYYMM)")
        .handler(revenue_disaggregation)
        .json_response_with_schema::<RevenueDisaggregationResponse>(
            openapi,
            StatusCode::OK,
            "Recognized revenue disaggregated by (period, stream)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/recognition-schedules/{schedule_id}")
        .operation_id("bss_ledger.get_recognition_schedule")
        .summary("Read an ASC 606 recognition schedule's lifecycle view")
        .description(
            "Returns the recognition schedule named by the path `schedule_id` for \
             the `tenant_id` query (the schedule PK is `(tenant_id, schedule_id)`; \
             the tenant is in the query, like the disaggregation report). The body \
             is the schedule header (status, version, revenue_stream, currency, \
             total_deferred_minor, recognized_minor, the originating \
             source_invoice_id + source_invoice_item_ref invoice-link anchor, \
             po_allocation_group, subscription_ref, policy_ref) plus its segments \
             (segment_no, period_id, amount_minor, status), ordered by segment_no \
             (period order). The read is tenant-scoped (SQL-level BOLA): a schedule \
             outside the caller's authorized subtree — or simply absent — yields a \
             404 (no existence leak).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("schedule_id", "The recognition schedule to read.")
        .query_param(
            "tenant_id",
            true,
            "The schedule's owning seller tenant (the PK's tenant half).",
        )
        .handler(get_recognition_schedule)
        .json_response_with_schema::<RecognitionScheduleResponse>(
            openapi,
            StatusCode::OK,
            "The schedule header + its segments",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/recognition-schedules")
        .operation_id("bss_ledger.list_recognition_schedules")
        .summary("List ASC 606 recognition schedules (discovery)")
        .description(
            "Lists the recognition schedule HEADERS for the `tenant_id` query, \
             optionally narrowed to one originating `invoice_id` \
             (`source_invoice_id`) and/or one `revenue_stream`. This is the \
             discovery surface for the server-minted `schedule_id` (also echoed in \
             the invoice-post response): with it a REST client can find the id, \
             then read (`GET …/{schedule_id}`) or change \
             (`POST …/{schedule_id}/changes`) a specific schedule. Header-only — no \
             segments (the by-id read carries those). Tenant-scoped (SQL-level \
             BOLA): schedules outside the caller's authorized subtree are silently \
             excluded. An empty list is a normal 200 (not a 404).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "tenant_id",
            true,
            "The schedules' owning seller tenant (the PK's tenant half).",
        )
        .query_param(
            "invoice_id",
            false,
            "Optional: narrow to one originating invoice (`source_invoice_id`).",
        )
        .query_param(
            "revenue_stream",
            false,
            "Optional: narrow to one revenue stream.",
        )
        .handler(list_recognition_schedules)
        .json_response_with_schema::<RecognitionScheduleListResponse>(
            openapi,
            StatusCode::OK,
            "The matching schedule headers (possibly empty)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/recognition-schedules/{schedule_id}/changes")
        .operation_id("bss_ledger.change_recognition_schedule")
        .summary("Change or cancel an ASC 606 recognition schedule")
        .description(
            "Changes or cancels the ACTIVE recognition schedule named by the path \
         `schedule_id` for the seller in the body's `tenant_id` (the `(recognition, \
         write)` PEP gate authorizes it — a change marks/mints schedule state). \
         The upstream modification `treatment` is gated FIRST: `prospective` / \
         `separate_contract` apply; `catch_up` or any unknown value is rejected \
         MODIFICATION_TREATMENT_REVIEW with no state change (the ledger never \
         silently treats a modification as prospective, §3.6). A `cancel` marks \
         the schedule CANCELLED (the unreleased deferred remainder stays as \
         CONTRACT_LIABILITY; no auto-reversal). A `replace` marks the old schedule \
         REPLACED and mints a NEW schedule version (a fresh `schedule_id`, \
         `version + 1`) that re-plans the REMAINING deferred over `new_segments` \
         (prospective — already-recognized revenue is not unwound, no compensating \
         entry). Idempotent on `change_id`. Emits billing.ledger.schedule.changed. \
         A path form (not a `:change` custom method).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("schedule_id", "The ACTIVE recognition schedule to change.")
        .json_request::<ChangeRecognitionScheduleRequest>(
            openapi,
            "The change (cancel / replace), the idempotency `change_id`, the \
         modification `treatment`, and (for a replace) the replacement segments.",
        )
        .handler(change_recognition_schedule)
        .json_response_with_schema::<ScheduleChangeResponse>(
            openapi,
            StatusCode::OK,
            "The change outcome (the original + successor schedule ids and the \
         original's resulting status)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/recognition-runs/{run_id}")
        .operation_id("bss_ledger.get_recognition_run")
        .summary("Read a recorded ASC 606 recognition run")
        .description(
            "Returns the recorded recognition run named by the path `run_id` for \
             the `tenant_id` query — the orchestration wrapper that released a \
             period's due PENDING segments (each a posted DR CONTRACT_LIABILITY / \
             CR REVENUE entry). The body is the run's `period_id`, `status` \
             (RUNNING ⇒ in-flight / DONE ⇒ completed / FAILED ⇒ aborted), and \
             `started_at_utc`. The run PK folds `period_id` (a client may reuse one \
             `run_id` across two periods), so the owning seller `tenant_id` is \
             required in the query. Tenant-scoped (SQL-level BOLA): an unknown run — \
             or one outside the caller's authorized subtree — yields a 404 (no \
             existence leak). Mirrors `get_dispute`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("run_id", "The recognition run to read.")
        .query_param(
            "tenant_id",
            true,
            "The run's owning seller tenant (the run PK's tenant half).",
        )
        .handler(get_recognition_run)
        .json_response_with_schema::<RecognitionRunView>(
            openapi,
            StatusCode::OK,
            "The recorded recognition run",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/recognition-runs")
        .operation_id("bss_ledger.list_recognition_runs")
        .summary("List recorded ASC 606 recognition runs (cursor-paginated)")
        .description(
            "Cursor-paginated list of the recorded recognition runs for the \
             seller named by `$filter=tenant_id eq <uuid>` (the caller's own by \
             default). Supports OData `$filter` over `tenant_id`, `run_id`, \
             `period_id`, and `status`. The `$filter` ANDs the caller's \
             authorized subtree, so runs outside it are never returned \
             (SQL-level BOLA). Each item is the same `RecognitionRunView` the \
             by-id read returns. Mirrors `list_disputes`.",
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
        .handler(list_recognition_runs)
        .with_odata_filter::<RecognitionRunFilterField>()
        .with_odata_orderby::<RecognitionRunFilterField>()
        .json_response_with_schema::<Page<RecognitionRunView>>(
            openapi,
            StatusCode::OK,
            "One page of recorded recognition runs",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// A recognition-run outcome rendered with the right status: `200 OK` + the run
/// reference when the run released its due segments in period order (or replayed
/// a prior run), or `202 Accepted` + the `recognition-period-queued` body when
/// the run had to park one or more out-of-order segments for a later drain
/// (§4.6). Mirrors `payments::allocate_response`'s status-varying rendering.
fn run_response(outcome: bss_ledger_sdk::RecognitionRunOutcome) -> Response {
    match outcome {
        bss_ledger_sdk::RecognitionRunOutcome::Ran(run_ref) => {
            (StatusCode::OK, Json(RecognitionRunResponse::from(run_ref))).into_response()
        }
        bss_ledger_sdk::RecognitionRunOutcome::Queued(queued) => (
            StatusCode::ACCEPTED,
            Json(RecognitionRunQueuedResponse::from(queued)),
        )
            .into_response(),
    }
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400)
// BEFORE the in-handler `require_authenticated` gate, so a malformed body yields
// 400 even for an unauthenticated caller (standard axum extractor ordering; no
// authenticated-only data is disclosed).
async fn trigger_recognition_run(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<TriggerRecognitionRunRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id` (tenant in body, not path).
    let tenant_id = body.tenant_id;
    // (recognition, write) PEP gate against the TARGET tenant: a run posts journal
    // entries into the seller's ledger, so it authorizes the data-plane post
    // action; a target outside the caller's scope is a cross-tenant write and is
    // denied. The in-process client gates again (defence-in-depth, matching the
    // payment / dispute surfaces).
    crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECOGNITION,
        crate::authz::actions::WRITE,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let cmd = body.into_sdk()?;
    let outcome = state.client.trigger_recognition_run(&ctx, cmd).await?;
    Ok(run_response(outcome))
}

/// `GET /revenue/disaggregation` query parameters. The target tenant (the
/// caller's own when omitted) + an optional `period_id` narrowing. NOTE:
/// disaggregation is a **computed aggregate report** (a grouped SUM over the
/// recognized segments), not a paginated row collection — it keeps plain
/// `?tenant_id=&period_id=` query params (no `OData` `$filter` / `Page` envelope),
/// mirroring `journal_entries::ar_aging_handler`.
#[derive(Debug, serde::Deserialize)]
struct DisaggregationQuery {
    tenant_id: Option<Uuid>,
    period_id: Option<String>,
}

async fn revenue_disaggregation(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<DisaggregationQuery>,
) -> Result<Json<RevenueDisaggregationResponse>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id.unwrap_or_else(|| ctx.subject_tenant_id());
    // (recognition, read) PEP gate against the TARGET tenant — the SAME action a
    // balance / journal-line LIST reads under (the recognized revenue is drawn
    // down from the `entry` ledger). Defence-in-depth: the in-process client
    // gates again + binds the compiled scope as the SQL-level BOLA filter (a
    // target outside the caller's subtree yields no entries), matching
    // `trigger_recognition_run`.
    crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECOGNITION,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let report = state
        .client
        .list_revenue_disaggregation(
            &ctx,
            bss_ledger_sdk::RevenueDisaggregationQuery {
                tenant_id,
                period_id: query.period_id,
            },
        )
        .await?;
    Ok(Json(RevenueDisaggregationResponse::from(report)))
}

/// `GET /recognition-schedules/{schedule_id}` query parameters: the schedule's
/// owning seller `tenant_id` (the schedule PK is `(tenant_id, schedule_id)`, so
/// the tenant is REQUIRED in the query — unlike the disaggregation report, which
/// defaults it to the caller's own). `schedule_id` is the path param.
#[derive(Debug, serde::Deserialize)]
struct ScheduleQuery {
    tenant_id: Uuid,
}

async fn get_recognition_schedule(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(schedule_id): Path<String>,
    Query(query): Query<ScheduleQuery>,
) -> Result<Json<RecognitionScheduleResponse>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (recognition, read) PEP gate against the schedule's owning tenant — the SAME
    // action the disaggregation read / balance LIST run under (the recognition
    // tables are drawn down from the `entry` ledger). Defence-in-depth: the
    // in-process client gates again + binds the compiled scope as the SQL-level
    // BOLA filter (a schedule outside the caller's subtree resolves to None ⇒ 404).
    crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECOGNITION,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    // `None` ⇒ absent OR scoped-out — a canonical 404 either way (no existence
    // leak), mirroring `journal_entries::get_entry`'s `entry_not_found`. NOT the
    // fiscal-period `PeriodNotFound` domain variant — this is a fresh canonical
    // not-found problem keyed on the schedule id.
    let view = state
        .client
        .get_recognition_schedule(&ctx, tenant_id, schedule_id.clone())
        .await?
        .ok_or_else(|| recognition_schedule_not_found(&schedule_id))?;
    Ok(Json(RecognitionScheduleResponse::from(view)))
}

/// `GET /recognition-schedules` query parameters: the owning seller `tenant_id`
/// (REQUIRED — the schedule PK's tenant half), optionally narrowed to one
/// originating `invoice_id` (`source_invoice_id`) and/or one `revenue_stream`.
#[derive(Debug, serde::Deserialize)]
struct ListSchedulesQuery {
    tenant_id: Uuid,
    #[serde(default)]
    invoice_id: Option<String>,
    #[serde(default)]
    revenue_stream: Option<String>,
}

async fn list_recognition_schedules(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<ListSchedulesQuery>,
) -> Result<Json<RecognitionScheduleListResponse>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (recognition, read) PEP gate against the target tenant — the SAME gate as the
    // by-id read; the in-process client re-derives the compiled scope as the
    // SQL-level BOLA filter, so foreign-tenant schedules are excluded (an empty
    // list, never an existence leak).
    crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECOGNITION,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let list = state
        .client
        .list_recognition_schedules(&ctx, tenant_id, query.invoice_id, query.revenue_stream)
        .await?;
    Ok(Json(RecognitionScheduleListResponse {
        schedules: list
            .schedules
            .into_iter()
            .map(RecognitionScheduleSummaryDto::from)
            .collect(),
        truncated: list.truncated,
    }))
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400) BEFORE
// the in-handler `require_authenticated` gate (standard axum extractor ordering;
// no authenticated-only data is disclosed). Always renders `200` on success (the
// change applied, or an idempotent replay); the treatment-review / unknown-action
// / bad-segments rejections are RFC 9457 problems from the client.
async fn change_recognition_schedule(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(schedule_id): Path<String>,
    CanonicalJson(body): CanonicalJson<ChangeRecognitionScheduleRequest>,
) -> Result<Json<ScheduleChangeResponse>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id`; `schedule_id` is the path id.
    let tenant_id = body.tenant_id;
    // (recognition, write) PEP gate against the TARGET tenant: a schedule change marks /
    // mints recognition-schedule state in the seller's ledger (same data-plane
    // post action as the run trigger). A target outside the caller's scope is a
    // cross-tenant write and is denied. The in-process client gates again
    // (defence-in-depth, matching the run / dispute surfaces).
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECOGNITION,
        crate::authz::actions::WRITE,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let cmd = body.into_sdk(schedule_id)?;

    // Treatment gate FIRST (design §4.6; the `change_service` "Treatment gate
    // FIRST" contract): a `catch_up` / unknown / unmarked treatment is a review
    // with NO state change. It MUST refuse here — before the dual-control gate
    // below can durably park a PENDING approval. The
    // change-service re-gates the treatment in-txn (defence-in-depth); the gate is
    // pure, so running it twice is free.
    crate::domain::recognition::change::gate_treatment(&cmd.treatment)
        .map_err(CanonicalError::from)?;

    // Dual-control gate (VHP-1852): a schedule change/cancel whose un-recognized
    // deferred remainder (the revenue it re-plans / strands) crosses the D2
    // threshold routes to the preparer→approver queue (409 DUAL_CONTROL_REQUIRED);
    // a below-threshold change applies inline. The remainder is read from the
    // schedule; an absent/scoped-out OR non-ACTIVE schedule skips the gate. The
    // non-ACTIVE skip is load-bearing for idempotency: an
    // already-applied change_id leaves the original schedule REPLACED, so a replay
    // falls through to the change-service and returns the idempotent 200 instead of
    // recomputing the stale remainder and re-raising a spurious 409. Only an ACTIVE
    // schedule is changeable, so only an ACTIVE schedule can gate. Mirrors the
    // chargeback-loss gate in `disputes`.
    if let Some(approval) = &state.approval
        && let Some(view) = state
            .client
            .get_recognition_schedule(&ctx, tenant_id, cmd.schedule_id.clone())
            .await?
        && view.status == crate::domain::status::SCHEDULE_STATUS_ACTIVE
    {
        let affected = view
            .total_deferred_minor
            .saturating_sub(view.recognized_minor);
        let intent = crate::domain::approval::intent::ApprovalIntent::RecognitionScheduleChange(
            crate::domain::approval::intent::RecognitionScheduleChangeIntent {
                tenant_id: cmd.tenant_id,
                schedule_id: cmd.schedule_id.clone(),
                change_id: cmd.change_id.clone(),
                action: cmd.action.clone(),
                treatment: cmd.treatment.clone(),
                new_segments: cmd.new_segments.as_ref().map(|segs| {
                    segs.iter()
                        .map(
                            |s| crate::domain::approval::intent::RecognitionChangeSegment {
                                period_id: s.period_id.clone(),
                                amount_minor: s.amount_minor,
                            },
                        )
                        .collect()
                }),
            },
        );
        let facts = crate::domain::approval::policy::OperationFacts {
            kind: crate::domain::approval::ApprovalKind::RecognitionScheduleChange,
            amount_usd_eq_minor: Some(affected),
            effective_at: None,
            has_outstanding_balance: false,
        };
        if let Some(approval_id) = approval
            .gate(
                &ctx,
                &scope,
                intent,
                facts,
                "recognition-schedule-change".to_owned(),
            )
            .await
            .map_err(CanonicalError::from)?
        {
            return Err(CanonicalError::from(
                crate::domain::error::DomainError::DualControlRequired(format!(
                    "recognition schedule change requires dual-control approval: {approval_id}"
                )),
            ));
        }
    }

    let result = state.client.change_recognition_schedule(&ctx, cmd).await?;
    Ok(Json(ScheduleChangeResponse::from(result)))
}

/// `GET /recognition-runs/{run_id}` query parameters: the run's owning seller
/// `tenant_id` (the run PK folds `period_id`, but the by-id read keys on the
/// surrogate `run_id` under the tenant, so the tenant is REQUIRED in the query —
/// like the by-id refund / schedule reads). `run_id` is the path param (a `Uuid`).
#[derive(Debug, serde::Deserialize)]
struct RunQuery {
    tenant_id: Uuid,
}

async fn get_recognition_run(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(run_id): Path<Uuid>,
    Query(query): Query<RunQuery>,
) -> Result<Json<RecognitionRunView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (recognition, read) PEP gate against the run's owning tenant — the SAME action the
    // schedule / disaggregation reads run under (the recognition tables are drawn
    // from the `entry` ledger). The returned scope is the SQL-level BOLA filter the
    // repo binds, so a foreign-tenant run resolves to None ⇒ 404 (no existence
    // leak), mirroring `disputes::get_dispute`.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECOGNITION,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let run = state
        .recognition_repo
        .as_ref()
        .ok_or_else(|| CanonicalError::internal("recognition repository not configured").create())?
        .read_run_out_of_txn(&scope, tenant_id, run_id)
        .await
        .map_err(|e| {
            crate::domain::error::DomainError::Internal(format!("read recognition run: {e}"))
        })?
        .ok_or_else(|| recognition_run_not_found(run_id))?;
    Ok(Json(RecognitionRunView::from(run)))
}

/// `GET /recognition-runs`: seller from `$filter=tenant_id eq`, else the caller.
#[allow(clippy::implicit_hasher)]
async fn list_recognition_runs(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<RecognitionRunView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    // (recognition, read) PEP gate against the runs' owning tenant — the SAME action the
    // by-id read / schedule list run under. The returned scope is the SQL-level
    // BOLA filter the repo binds, so the page never contains a foreign-tenant run
    // (no existence leak), mirroring `disputes::list_disputes`.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::RECOGNITION,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let page = state
        .recognition_repo
        .as_ref()
        .ok_or_else(|| CanonicalError::internal("recognition repository not configured").create())?
        .list_runs(&scope, tenant_id, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page
            .items
            .into_iter()
            .map(RecognitionRunView::from)
            .collect(),
        page_info: page.page_info,
    }))
}
