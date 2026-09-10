//! Axum handlers + router for the ledger's refund (money-OUT) REST surface
//! (Slice 3, Phase 2, design §4.4 / §5 / §7, Group G). Three operations under
//! `/bss-ledger/v1`, tenant-scoped WITHOUT a tenant in the path (the vhp-core
//! convention, matching the adjustment / payment / dispute surfaces): the writes
//! carry `tenant_id` in the **body**, the by-id read takes the refund id in the
//! path + the owning seller `tenant_id` in the query.
//!
//! - `POST /refunds` — record one PSP refund phase (the money-out side that
//!   unwinds a settled receipt). Idempotent on `(psp_refund_id, phase)`: a re-post
//!   replays (`200`), a fresh post is `201`. A refund whose cash crosses the
//!   tenant's D2 threshold routes to dual-control (`409 DUAL_CONTROL_REQUIRED`). A
//!   refund whose origin payment has not landed is QUARANTINED
//!   (`202` + `refund-quarantined` body token), NEVER posted (design §4.4 / PRD
//!   L668) — distinct from queue-and-apply.
//! - `POST /refund-with-credit-note` — post a refund AND its paired S3 credit note
//!   ATOMICALLY in ONE transaction as two linked entries (K-3): both commit or
//!   neither, so AR is never overstated between them. `201` fresh / `200` replay /
//!   `409` over D2.
//! - `GET /refunds/{refund_id}` — read a recorded refund + its clearing state. The
//!   surrogate PK is `(tenant_id, refund_id)`, so the owning seller `tenant_id` is
//!   required in the query. `404` when no refund with that id exists (or it is
//!   outside the caller's subtree — no existence leak).
//!
//! All three authorize under `(entry, post)` / `(entry, read)` — the SAME
//! data-plane actions the credit/debit-note + invoice-post writes use (a refund
//! posts a balanced journal entry into the seller's ledger; the by-id read draws
//! from the `entry` ledger's `refund` record). This mirrors the Phase-1 notes
//! decision (`adjustments.rs`): a refund is not a separate authz resource, it
//! posts through the same `PostingService`. The `RefundHandler` is a concrete
//! orchestrator (not behind `LedgerClientV1`) that re-gates nothing internally, so
//! the handler-layer PEP gate is the authority and threads the compiled scope into
//! the post (the SQL-level BOLA filter), exactly as `adjustments::post_credit_note`.
//!
//! The routes register through `OperationBuilder` so `/openapi.json` lists each
//! operation with its declared request / response schemas. Mirrors
//! [`crate::api::rest::adjustments::router`].

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
use crate::api::rest::dto::{
    REFUND_DISPUTE_HELD_STATUS, REFUND_QUARANTINED_STATUS, RefundDisputeHeldResponse,
    RefundQuarantinedResponse, RefundRequest, RefundResponse, RefundView,
    RefundWithCreditNoteRequest, RefundWithCreditNoteResponse,
};
use crate::api::rest::error::{authz_error_to_canonical, refund_not_found};
use crate::api::rest::odata_list::{list_seller_tenant, reject_non_odata_list_params};
use crate::infra::adjustment::refund_service::{RefundHandler, RefundOutcome};
use crate::infra::storage::repo::AdjustmentRepo;
use crate::odata::{RefundFilterField, RefundOrderField};

/// `OpenAPI` tag applied to the refund operations.
const TAG: &str = "BSS Ledger Refunds";

/// Shared per-request state for the refund routes. Constructed once at `init()`
/// and shared via `Extension<Arc<ApiState>>`. Carries the GATED refund
/// orchestrator (a refund posts through it — caps + record + event, over-D2 →
/// dual-control, refund-before-payment → quarantine, and the atomic
/// `refund-with-credit-note` composite) + the `AdjustmentRepo` for the by-id read.
/// Mirrors [`crate::api::rest::adjustments::ApiState`].
#[derive(Clone)]
pub struct ApiState {
    /// The gated refund orchestrator (post / composite / quarantine / de-quarantine).
    pub refunds: Arc<RefundHandler>,
    /// The adjustment repo — the `GET /refunds/{id}` read source (the `refund` row).
    pub refund_repo: AdjustmentRepo,
}

/// Build the Axum router for the refund surface and register every operation with
/// the supplied `OpenAPI` registry. `state` is attached via an `Extension` layer
/// at the end so the registry sees the route definitions before the per-request
/// state is bound. Mirrors [`crate::api::rest::adjustments::router`].
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/refunds")
        .operation_id("bss_ledger.post_refund")
        .summary("Record a PSP refund phase (money-out)")
        .description(
            "Records one phase of a PSP refund for the seller named by the body's \
             `tenant_id` — the money-out side that unwinds a settled receipt \
             (Pattern A on-account DR UNALLOCATED, or Pattern B restore-AR DR AR; \
             stage-1 CR REFUND_CLEARING, stage-2 DR REFUND_CLEARING / CR \
             CASH_CLEARING; NEVER DR CONTRACT_LIABILITY). Idempotent on \
             `(psp_refund_id, phase)`: a re-post returns the prior posting reference \
             (200) instead of a new one (201). A refund whose returned cash crosses \
             the tenant's D2 threshold routes to dual-control (409 \
             DUAL_CONTROL_REQUIRED). A refund whose origin payment has no resolvable \
             settlement is QUARANTINED (202 refund-quarantined) — never posted; it \
             only ever posts after an explicit de-quarantine that re-validates the \
             caps + the then-current threshold + the dispute state. An over-refund \
             past the settled/allocated cap is rejected (REFUND_EXCEEDS_SETTLED / \
             REFUND_EXCEEDS_ALLOCATED).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RefundRequest>(
            openapi,
            "The refund phase (pattern, phase, amount, origin payment) + the \
             idempotency `psp_refund_id`.",
        )
        .handler(post_refund)
        .json_response_with_schema::<RefundResponse>(
            openapi,
            StatusCode::CREATED,
            "Posting reference (201 fresh phase / 200 idempotent replay)",
        )
        .json_response_with_schema::<RefundResponse>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior refund phase",
        )
        .json_response_with_schema::<RefundQuarantinedResponse>(
            openapi,
            StatusCode::ACCEPTED,
            "Refund-before-payment quarantined until its origin lands (refund-quarantined)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/refund-with-credit-note")
        .operation_id("bss_ledger.post_refund_with_credit_note")
        .summary("Post a refund + its paired credit note atomically")
        .description(
            "Posts a S5 refund AND its paired S3 credit note in ONE transaction as \
             two linked entries (K-3) for the seller named by the body's refund \
             `tenant_id` — both commit or neither, so AR is never overstated between \
             them. The refund is gated over D2 like a plain stage-1 (the credit note \
             rides the same approval; a high-value composite routes to dual-control \
             as a unit, 409). A split-ambiguous / over-headroom credit note, an \
             over-refund cap, or a closed account rolls the WHOLE composite back. \
             The refund's origin settlement MUST resolve (a composite refunds a real \
             payment — an absent origin is a 404, NOT a quarantine).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RefundWithCreditNoteRequest>(
            openapi,
            "The refund + the paired credit note to post atomically.",
        )
        .handler(post_refund_with_credit_note)
        .json_response_with_schema::<RefundWithCreditNoteResponse>(
            openapi,
            StatusCode::CREATED,
            "Both posted entry references (201 fresh / 200 idempotent replay)",
        )
        .json_response_with_schema::<RefundWithCreditNoteResponse>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior composite (both halves)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/refunds/{refund_id}")
        .operation_id("bss_ledger.get_refund")
        .summary("Read a recorded refund + its clearing state")
        .description(
            "Returns the recorded refund for `(tenant_id, refund_id)` — its latest \
             lifecycle phase, pattern, amount, origin payment, and the two-stage \
             REFUND_CLEARING drain `clearing_state` (PENDING ⇒ stage-1 open / \
             SETTLED ⇒ drained or single-step / REVERSED ⇒ a PSP reject/void \
             line-negated the stage-1). The surrogate PK is `(tenant_id, \
             refund_id)`, so the owning seller `tenant_id` is required in the query. \
             Tenant-scoped (SQL-level BOLA): an unknown refund — or one outside the \
             caller's authorized subtree — yields a 404 (no existence leak).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "refund_id",
            "The refund whose record + clearing state to read.",
        )
        .query_param(
            "tenant_id",
            true,
            "The refund's owning seller tenant (the refund PK's tenant half).",
        )
        .handler(get_refund)
        .json_response_with_schema::<RefundView>(
            openapi,
            StatusCode::OK,
            "The recorded refund + its clearing state",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/refunds")
        .operation_id("bss_ledger.list_refunds")
        .summary("List recorded refunds (cursor-paginated)")
        .description(
            "Cursor-paginated list of the recorded refunds for the seller named \
             by `$filter=tenant_id eq <uuid>` (the caller's own by default). \
             Supports OData `$filter` over `tenant_id`, `payment_id`, \
             `psp_refund_id`, `phase`, `pattern`, `clearing_state`, and \
             `invoice_id`. The `$filter` ANDs the caller's authorized subtree, \
             so refunds outside it are never returned (SQL-level BOLA). Each item is \
             the same `RefundView` the by-id read returns.",
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
        .handler(list_refunds)
        .with_odata_filter::<RefundFilterField>()
        .with_odata_orderby::<RefundOrderField>()
        .json_response_with_schema::<Page<RefundView>>(
            openapi,
            StatusCode::OK,
            "One page of recorded refunds",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// A refund-record outcome rendered with the right status: `201 Created` for a
/// fresh post, `200 OK` for an idempotent replay, or `202 Accepted` + the
/// `refund-quarantined` body for a refund-before-payment quarantined until its
/// origin lands (§4.4). Mirrors `disputes::record_response`'s status-varying
/// rendering (the dual-control 409 flows through the `From<DomainError>` ladder).
fn record_response(outcome: RefundOutcome) -> Response {
    match outcome {
        RefundOutcome::Posted(reference) => {
            let status = if reference.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (status, Json(RefundResponse::from(reference))).into_response()
        }
        RefundOutcome::Quarantined(handle) => (
            StatusCode::ACCEPTED,
            Json(RefundQuarantinedResponse {
                status: REFUND_QUARANTINED_STATUS.to_owned(),
                flow: handle.flow,
                business_id: handle.business_id,
                quarantined_at: handle.quarantined_at,
            }),
        )
            .into_response(),
        // Refund-dispute-held (Z5-2, design §5): the origin payment has an OPEN
        // dispute, so the refund's cash leg was durably HELD (202), never posted.
        RefundOutcome::DisputeHeld(handle) => (
            StatusCode::ACCEPTED,
            Json(RefundDisputeHeldResponse {
                status: REFUND_DISPUTE_HELD_STATUS.to_owned(),
                flow: handle.flow,
                business_id: handle.business_id,
                held_at: handle.held_at,
            }),
        )
            .into_response(),
    }
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400) BEFORE
// the in-handler `require_authenticated` gate, so a malformed body yields 400 even
// for an unauthenticated caller (standard axum extractor ordering; no
// authenticated-only data is disclosed). Mirrors `adjustments::post_credit_note`.
async fn post_refund(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<RefundRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id` (tenant in body, not path).
    let tenant_id = body.tenant_id;
    // (entry, post) PEP gate against the TARGET tenant — the SAME data-plane post
    // action the credit/debit-note + invoice-post writes use (a refund posts a
    // balanced journal entry into the seller's ledger). A target outside the
    // caller's scope is a cross-tenant write and is denied; the returned scope
    // threads into the scoped post (the SQL-level BOLA filter).
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::POST,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let req = body.into_domain().map_err(CanonicalError::from)?;
    // `record_refund` resolves the origin settlement: present ⇒ post (over-D2 →
    // DualControlRequired → 409 via the ladder; over-refund cap → 400); absent ⇒
    // quarantine (202). All domain rejections flow through the single
    // `From<DomainError> for CanonicalError` ladder via `?`.
    let outcome = state
        .refunds
        .record_refund(&ctx, &scope, req)
        .await
        .map_err(CanonicalError::from)?;
    Ok(record_response(outcome))
}

async fn post_refund_with_credit_note(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<RefundWithCreditNoteRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the refund's `tenant_id`. The credit note targets the
    // same seller (the composite is one tenant's books); the (entry, post) gate is
    // on the refund's tenant.
    let tenant_id = body.refund.tenant_id;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::POST,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let refund = body.refund.into_domain().map_err(CanonicalError::from)?;
    let credit_note = body
        .credit_note
        .into_domain()
        .map_err(CanonicalError::from)?;
    // The composite refund's tenant + the credit note's tenant must be the same
    // seller (one tenant's books, one PEP gate). A cross-tenant pairing is rejected
    // as a malformed request (400) rather than silently posting two tenants' books.
    if credit_note.tenant_id != tenant_id {
        return Err(CanonicalError::from(
            crate::domain::error::DomainError::InvalidRequest(
                "refund-with-credit-note: the refund and credit note must target the same \
                 tenant"
                    .to_owned(),
            ),
        ));
    }
    let outcome = state
        .refunds
        .post_refund_with_credit_note(&ctx, &scope, refund, credit_note)
        .await
        .map_err(CanonicalError::from)?;
    let status = if outcome.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(RefundWithCreditNoteResponse {
            refund_entry_id: outcome.refund_entry_id,
            credit_note_entry_id: outcome.credit_note_entry_id,
            replayed: outcome.replayed,
        }),
    )
        .into_response())
}

/// `GET /refunds/{refund_id}` query parameters: the refund's owning seller
/// `tenant_id` (the refund PK is `(tenant_id, refund_id)`, so the tenant is
/// REQUIRED in the query — like the by-id exposure / recognition-schedule reads).
#[derive(Debug, serde::Deserialize)]
struct RefundQuery {
    tenant_id: Uuid,
}

async fn get_refund(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(refund_id): Path<String>,
    Query(query): Query<RefundQuery>,
) -> Result<Json<RefundView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (entry, read) PEP gate against the refund's owning tenant — the SAME action
    // the balance / exposure reads run under. The returned scope is the SQL-level
    // BOLA filter the repo binds, so a foreign-tenant refund resolves to None ⇒ 404
    // (no existence leak), mirroring `adjustments::get_invoice_exposure`.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let refund = state
        .refund_repo
        .read_refund_out_of_txn(&scope, tenant_id, &refund_id)
        .await
        .map_err(|e| crate::domain::error::DomainError::Internal(format!("read refund: {e}")))?
        .ok_or_else(|| refund_not_found(&refund_id))?;
    Ok(Json(RefundView::from(refund)))
}

/// `GET /refunds`: seller from `$filter=tenant_id eq`, else the caller.
#[allow(clippy::implicit_hasher)]
async fn list_refunds(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<RefundView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    // (entry, read) PEP gate against the refunds' owning tenant — the SAME action
    // the by-id read / balances run under. The returned scope is the SQL-level BOLA
    // filter the repo binds, so the page never contains a foreign-tenant refund (no
    // existence leak), mirroring `get_refund` / `journal_entries::list_lines`.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let page = state
        .refund_repo
        .list_refunds(&scope, tenant_id, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.into_iter().map(RefundView::from).collect(),
        page_info: page.page_info,
    }))
}
