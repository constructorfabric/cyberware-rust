//! Axum handler + router for the ledger's chargeback (dispute) REST surface
//! (architecture §4.5, the dispute state machine). One write operation + two
//! read operations under `/bss-ledger/v1`, tenant-scoped WITHOUT a tenant in the
//! path (the vhp-core convention, matching the payment surface): the write's
//! `tenant_id` is in the **body**, the by-id read takes the dispute id in the
//! path + the owning seller `tenant_id` in the query.
//!
//! - `POST /disputes/{dispute_id}/phases` — record one dispute phase. The
//!   `(dispute, write)` PEP gate authorizes it against the body's `tenant_id`.
//!   The LEDGER chooses the variant at `opened` from `funds_at_open`. Idempotent
//!   on `(dispute_id, cycle, phase)`: a re-post replays (`200`), a fresh phase is
//!   `201`.
//! - `GET /disputes/{dispute_id}` — read a dispute's current state (its variant,
//!   cycle, last phase, disputed amount, cash hold). The surrogate PK is
//!   `(tenant_id, dispute_id)`, so the owning seller `tenant_id` is required in
//!   the query. `404` when no such dispute exists (or it is outside the caller's
//!   subtree — no existence leak).
//! - `GET /disputes` — cursor-paginated list of the recorded disputes for a
//!   tenant, with an `OData` `$filter` over `payment_id` / `last_phase` / `variant`.
//!
//! The WRITE authorizes under `(dispute, write)`; the two READS under
//! `(dispute, read)` — symmetric with the write on the dispute's OWN resource, so a
//! chargeback-analyst role reads disputes without the `entry` data plane (the by-id /
//! list reads draw from the `ledger_dispute` current-state row). The read gate threads
//! the compiled scope into the repo (the SQL-level BOLA filter), exactly as
//! `refunds::get_refund` /
//! `refunds::list_refunds`.
//!
//! The routes register through `OperationBuilder` so `/openapi.json` lists each
//! operation with its declared request / response schemas. Mirrors the
//! `return_payment` handler in [`crate::api::rest::payments`] (write) and
//! [`crate::api::rest::refunds`] (the read surface).

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
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::local_client::map_odata_page_err;
use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::dto::{
    DisputePhaseQueuedResponse, DisputeView, RecordDisputePhaseRequest, RecordDisputePhaseResponse,
};
use crate::api::rest::error::{authz_error_to_canonical, dispute_not_found};
use crate::api::rest::odata_list::{list_seller_tenant, reject_non_odata_list_params};
use crate::infra::storage::repo::DisputeRepo;
use crate::odata::DisputeFilterField;

/// `OpenAPI` tag applied to the dispute operations.
const TAG: &str = "BSS Ledger Disputes";

/// Shared per-request state for the dispute routes. Constructed once at `init()`
/// and shared via `Extension<Arc<ApiState>>`. Carries the in-process data-access
/// client (the chargeback record goes through it — the client gates the PEP and
/// orchestrates the post). Mirrors [`crate::api::rest::payments::ApiState`].
#[derive(Clone)]
pub struct ApiState {
    /// In-process data-access client (the gear's own local impl).
    pub client: Arc<dyn LedgerClientV1>,
    /// Dual-control lifecycle engine (VHP-1852): the loss gate routes a
    /// high-value chargeback-loss to the preparer→approver queue. `None` disables
    /// the gate (router unit tests without a governance DB).
    pub approval: Option<Arc<crate::infra::approval::service::ApprovalService>>,
    /// The dispute current-state repo — the `GET /disputes` list +
    /// `GET /disputes/{id}` by-id read source (the `ledger_dispute` row). Mirrors
    /// [`crate::api::rest::refunds::ApiState`]'s `refund_repo`.
    pub dispute_repo: DisputeRepo,
}

/// Build the Axum router for the dispute surface and register every operation
/// with the supplied `OpenAPI` registry. `state` is attached via an `Extension`
/// layer at the end so the registry sees the route definitions before the
/// per-request state is bound.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/disputes/{dispute_id}/phases")
        .operation_id("bss_ledger.record_dispute_phase")
        .summary("Record a chargeback dispute phase")
        .description(
            "Records one phase of a chargeback dispute on `{dispute_id}` for the \
             seller named by the body's `tenant_id`. The LEDGER chooses the \
             variant at `opened` from `funds_at_open` (`withheld` ⇒ cash-hold \
             DR DISPUTE_HOLD / CR CASH_CLEARING; `not_moved` ⇒ AR-reclass \
             ACTIVE→DISPUTED, AR-class-neutral). `phase` is one of opened / won / \
             lost / partial. Idempotent on `(dispute_id, cycle, phase)`: a re-post \
             returns the prior posting reference (200) instead of a new one (201). \
             A dispute records a card-network / bank event and lands even for a \
             closed payer. Rejected when the phase is not a legal transition from \
             the dispute's current state (INVALID_DISPUTE_PHASE). A won/lost whose \
             opened has not landed yet is durably QUEUED (202 dispute-phase-queued), \
             not rejected, and applied once the opened arrives.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("dispute_id", "The dispute being recorded.")
        .json_request::<RecordDisputePhaseRequest>(
            openapi,
            "The dispute phase to record + idempotency key.",
        )
        .handler(record_dispute_phase)
        .json_response_with_schema::<RecordDisputePhaseResponse>(
            openapi,
            StatusCode::CREATED,
            "Posting reference (201 fresh phase / 200 idempotent replay)",
        )
        .json_response_with_schema::<RecordDisputePhaseResponse>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior phase",
        )
        .json_response_with_schema::<DisputePhaseQueuedResponse>(
            openapi,
            StatusCode::ACCEPTED,
            "Out-of-order won/lost queued until its opened lands (dispute-phase-queued)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/disputes/{dispute_id}")
        .operation_id("bss_ledger.get_dispute")
        .summary("Read a chargeback dispute's current state")
        .description(
            "Returns the recorded chargeback dispute for `(tenant_id, dispute_id)` — \
             its chosen variant (CASH_HOLD ⇒ cash moved to DISPUTE_HOLD at open / \
             AR_RECLASS ⇒ AR reclassed ACTIVE→DISPUTED, no cash leg), its current \
             cycle + last phase (OPENED / WON / LOST), the disputed amount, and the \
             cash held in DISPUTE_HOLD at open (0 for AR_RECLASS). The surrogate PK \
             is `(tenant_id, dispute_id)`, so the owning seller `tenant_id` is \
             required in the query. Tenant-scoped (SQL-level BOLA): an unknown \
             dispute — or one outside the caller's authorized subtree — yields a 404 \
             (no existence leak). Mirrors `get_refund`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("dispute_id", "The dispute whose current state to read.")
        .query_param(
            "tenant_id",
            true,
            "The dispute's owning seller tenant (the dispute PK's tenant half).",
        )
        .handler(get_dispute)
        .json_response_with_schema::<DisputeView>(
            openapi,
            StatusCode::OK,
            "The dispute's current state",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/disputes")
        .operation_id("bss_ledger.list_disputes")
        .summary("List recorded disputes (cursor-paginated)")
        .description(
            "Cursor-paginated list of the recorded chargeback disputes for the \
             seller named by `$filter=tenant_id eq <uuid>` (the caller's own by \
             default). Supports OData `$filter` over `tenant_id`, `payment_id`, \
             `last_phase`, and `variant`. The `$filter` ANDs the caller's \
             authorized subtree, so disputes outside it are never returned \
             (SQL-level BOLA). Each item is the same `DisputeView` the by-id \
             read returns. Mirrors `list_refunds`.",
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
        .handler(list_disputes)
        .with_odata_filter::<DisputeFilterField>()
        .with_odata_orderby::<DisputeFilterField>()
        .json_response_with_schema::<Page<DisputeView>>(
            openapi,
            StatusCode::OK,
            "One page of recorded disputes",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// A dispute-phase outcome rendered with the right status: `201 Created` for a
/// fresh inline post, `200 OK` for an idempotent replay, or `202 Accepted` +
/// the `dispute-phase-queued` body for an out-of-order `won`/`lost` queued until
/// its `opened` lands (§4.7). Mirrors `payments::allocate_response`'s
/// status-varying rendering.
fn record_response(outcome: bss_ledger_sdk::DisputeOutcome) -> Response {
    match outcome {
        bss_ledger_sdk::DisputeOutcome::Recorded(recorded) => {
            let status = if recorded.posting.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (status, Json(RecordDisputePhaseResponse::from(recorded))).into_response()
        }
        bss_ledger_sdk::DisputeOutcome::Queued(queued) => (
            StatusCode::ACCEPTED,
            Json(DisputePhaseQueuedResponse::from(queued)),
        )
            .into_response(),
    }
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400)
// BEFORE the in-handler `require_authenticated` gate, so a malformed body yields
// 400 even for an unauthenticated caller (standard axum extractor ordering; no
// authenticated-only data is disclosed).
async fn record_dispute_phase(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(dispute_id): Path<String>,
    CanonicalJson(body): CanonicalJson<RecordDisputePhaseRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id`; `dispute_id` is the path id.
    let tenant_id = body.tenant_id;
    // (dispute, write) PEP gate against the TARGET tenant: a target outside the
    // caller's scope is a cross-tenant write and is denied. The in-process client
    // gates again (defence-in-depth, matching the payment surface).
    crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::DISPUTE,
        crate::authz::actions::WRITE,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Dual-control gate (VHP-1852): a chargeback-LOSS whose magnitude crosses the
    // D2 threshold routes to the preparer→approver queue (409); other phases, or a
    // below-threshold loss, post inline (unchanged).
    if body.phase.eq_ignore_ascii_case("lost")
        && let Some(approval) = &state.approval
    {
        let loss_intent = crate::domain::approval::intent::ApprovalIntent::ChargebackLoss(
            crate::domain::approval::intent::ChargebackLossIntent {
                tenant_id: body.tenant_id,
                payer_tenant_id: body.payer_tenant_id,
                payment_id: body.payment_id.clone(),
                dispute_id: dispute_id.clone(),
                invoice_id: body.invoice_id.clone(),
                cycle: body.cycle.unwrap_or(1),
                funds_at_open: body.funds_at_open.clone(),
                disputed_amount_minor: body.disputed_amount_minor,
                currency: body.currency.clone(),
            },
        );
        let loss_facts = crate::domain::approval::policy::OperationFacts {
            kind: crate::domain::approval::ApprovalKind::ChargebackLoss,
            amount_usd_eq_minor: Some(body.disputed_amount_minor),
            effective_at: None,
            has_outstanding_balance: false,
        };
        let scope = crate::authz::access_scope(
            &enforcer,
            &ctx,
            &crate::authz::resource_types::DISPUTE,
            crate::authz::actions::WRITE,
            Some(tenant_id),
            None,
            true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
        if let Some(approval_id) = approval
            .gate(
                &ctx,
                &scope,
                loss_intent,
                loss_facts,
                "chargeback-loss".to_owned(),
            )
            .await
            .map_err(CanonicalError::from)?
        {
            return Err(CanonicalError::from(
                crate::domain::error::DomainError::DualControlRequired(format!(
                    "chargeback loss requires dual-control approval: {approval_id}"
                )),
            ));
        }
    }

    let cmd = body.into_sdk(dispute_id)?;
    let outcome = state.client.record_dispute_phase(&ctx, cmd).await?;
    Ok(record_response(outcome))
}

/// `GET /disputes/{dispute_id}` query parameters: the dispute's owning seller
/// `tenant_id` (the dispute PK is `(tenant_id, dispute_id)`, so the tenant is
/// REQUIRED in the query — like the by-id refund / exposure reads).
#[derive(Debug, serde::Deserialize)]
struct DisputeQuery {
    tenant_id: Uuid,
}

async fn get_dispute(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(dispute_id): Path<String>,
    Query(query): Query<DisputeQuery>,
) -> Result<Json<DisputeView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (dispute, read) PEP gate against the dispute's owning tenant — the dispute's
    // OWN resource (symmetric with `dispute.write`, which records phases), so a
    // chargeback-analyst role reads disputes without the `entry` data plane. The
    // returned scope is the SQL-level BOLA filter the repo binds, so a foreign-tenant
    // dispute resolves to None ⇒ 404 (no existence leak), mirroring `refunds::get_refund`.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::DISPUTE,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // `read_dispute` returns `Result<Option<dispute::Model>, DomainError>` — the
    // same reader the refund dispute-hold pre-read uses; reused here for the by-id
    // read (no second by-id query). A scoped-out / absent row is a canonical 404.
    let dispute = state
        .dispute_repo
        .read_dispute(&scope, tenant_id, &dispute_id)
        .await?
        .ok_or_else(|| dispute_not_found(&dispute_id))?;
    Ok(Json(DisputeView::from(dispute)))
}

/// `GET /disputes`: seller from `$filter=tenant_id eq`, else the caller.
#[allow(clippy::implicit_hasher)]
async fn list_disputes(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<DisputeView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    // (dispute, read) PEP gate against the disputes' owning tenant — the dispute's
    // OWN resource (symmetric with `dispute.write`). The returned scope is the
    // SQL-level BOLA filter the repo binds, so the page never contains a foreign-tenant
    // dispute (no existence leak), mirroring `refunds::list_refunds`.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::DISPUTE,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let page = state
        .dispute_repo
        .list_disputes(&scope, tenant_id, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.into_iter().map(DisputeView::from).collect(),
        page_info: page.page_info,
    }))
}
