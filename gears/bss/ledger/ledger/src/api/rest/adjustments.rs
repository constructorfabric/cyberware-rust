//! Axum handlers + router for the ledger's adjustment (credit-note / debit-note /
//! exposure) REST surface (Slice 3, design §4.2 / §4.3 / §4.7 / §5, Group E).
//! Three operations under `/bss-ledger/v1`, tenant-scoped WITHOUT a tenant in the
//! path (the vhp-core convention, matching the payment / dispute / recognition
//! surfaces): the writes carry `tenant_id` in the **body**, the exposure read
//! takes the invoice id in the path + the owning seller `tenant_id` in the query.
//!
//! - `POST /credit-notes` — post a compensating credit note against a posted
//!   invoice (DR `CONTRA_REVENUE`/`GOODWILL` + per-stream DR `CONTRACT_LIABILITY` +
//!   DR `TAX_PAYABLE`; CR `AR` capped at open AR + CR `REUSABLE_CREDIT` remainder).
//!   Idempotent on `credit_note_id`: a re-post replays (`200`), a fresh post is
//!   `201`. Rejected `CREDIT_NOTE_SPLIT_AMBIGUOUS` (indeterminable split) /
//!   `CREDIT_NOTE_EXCEEDS_HEADROOM` (over the invoice's headroom cap).
//! - `POST /debit-notes` — post an additional charge (DIRECT split: DR `AR` /
//!   CR `REVENUE` / CR `CONTRACT_LIABILITY` / CR `TAX_PAYABLE`), building the
//!   releasing schedule when it defers (D4) and **raising** the invoice's
//!   headroom. Idempotent on `debit_note_id`: re-post `200`, fresh `201`. A closed
//!   payer is rejected `PAYER_CLOSED`.
//! - `GET /invoices/{invoice_id}/exposure` — read the invoice's remaining
//!   credit-note headroom (`original + debit − credit`) + its true remaining open
//!   AR. `404` when no note has touched the invoice yet (no exposure row) or it is
//!   outside the caller's subtree (no existence leak).
//! - `POST /manual-adjustments` — post a GOVERNED manual adjustment (design §4.6):
//!   the ledger's escape hatch for corrections the typed flows do not cover
//!   (rounding residue, suspense / cash-clearing clean-up). The body's `action`
//!   selects a code-owned allow-list of account classes; `REVENUE` /
//!   `CONTRACT_LIABILITY` are off-limits and an unpaired `CONTRA_REVENUE` leg is
//!   rejected as an attempted write-off (`400 MANUAL_ADJUSTMENT_NOT_ALLOWED`,
//!   additionally captured + paged on the write-off path). A `reason_code` is
//!   mandatory; the preparer is the AUTHENTICATED subject (never the body). A
//!   governed adjustment whose gross (`Σ DR`) crosses the tenant's D2 threshold
//!   routes to dual-control (`409 DUAL_CONTROL_REQUIRED`, via the canonical-error
//!   ladder). Idempotent on `adjustment_id`: re-post `200`, fresh `201`.
//!
//! The three writes (credit note / debit note / manual adjustment) authorize under
//! `(entry, post)` — the SAME data-plane post action the recognition-run /
//! invoice-post writes use (each posts a balanced journal entry into the seller's
//! ledger); the exposure read under `(entry, read)` — the SAME action the balance /
//! schedule reads use (the exposure counters are drawn down from the `entry`
//! ledger). The credit / debit / manual handlers are concrete orchestrators (not
//! behind `LedgerClientV1`), so the `ApiState` carries them directly
//! (`Arc<CreditNoteHandler>` / `Arc<DebitNoteHandler>` /
//! `Arc<ManualAdjustmentHandler>`) + the `AdjustmentRepo` for the exposure read —
//! they re-gate nothing internally, so the handler-layer PEP gate is the authority
//! and threads the compiled scope into the post (the SQL-level BOLA filter),
//! mirroring `journal_entries::post_invoice`. The manual handler is the GATED
//! instance (dual-control over D2 → 409); the executor's un-gated replay handler is
//! a SEPARATE instance wired in `module`.
//!
//! The routes register through `OperationBuilder` so `/openapi.json` lists each
//! operation with its declared request / response schemas. Mirrors
//! [`crate::api::rest::recognition::router`].
//!
//! (Events / metrics emit — `billing.ledger.credit_note.posted`,
//! `ledger_credit_note_total{,_blocked}` / `ledger_debit_note_total` — are wired in
//! Group F; this Group-E surface emits none yet.)

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
    CreditNoteRequest, CreditNoteResponse, CreditNoteView, DebitNoteRequest, DebitNoteResponse,
    DebitNoteView, InvoiceExposureResponse, ManualAdjustmentRequest, ManualAdjustmentResponse,
};
use crate::api::rest::error::{
    authz_error_to_canonical, credit_note_not_found, debit_note_not_found,
    invoice_exposure_not_found,
};
use crate::api::rest::odata_list::{list_seller_tenant, reject_non_odata_list_params};
use crate::infra::adjustment::credit_note_service::CreditNoteHandler;
use crate::infra::adjustment::debit_note_service::DebitNoteHandler;
use crate::infra::adjustment::manual_adjustment_service::ManualAdjustmentHandler;
use crate::infra::storage::repo::AdjustmentRepo;
use crate::odata::{CreditNoteFilterField, DebitNoteFilterField};

/// `OpenAPI` tag applied to the adjustment operations.
const TAG: &str = "BSS Ledger Adjustments";

/// Shared per-request state for the adjustment routes. Constructed once at
/// `init()` and shared via `Extension<Arc<ApiState>>`. Carries the two concrete
/// in-transaction orchestrators (a credit/debit note posts through them) + the
/// `AdjustmentRepo` for the exposure read. Unlike the payment / recognition state
/// (which fronts the in-process `LedgerClientV1`), these handlers are concrete and
/// re-gate nothing — the handler-layer PEP gate is the authority.
#[derive(Clone)]
pub struct ApiState {
    /// The credit-note orchestrator (posts the compensating entry + schedule
    /// reduction + headroom bump + `credit_note` row, all in one txn).
    pub credit: Arc<CreditNoteHandler>,
    /// The debit-note orchestrator (posts the direct-split charge + schedule build
    /// + headroom raise + `debit_note` row, all in one txn).
    pub debit: Arc<DebitNoteHandler>,
    /// The GATED governed manual-adjustment orchestrator (design §4.6): runs the pure
    /// `govern` gate, the payer gate, the dual-control gate (over D2 → 409), and posts
    /// the balanced legs + the in-txn event sidecar. A SEPARATE instance from the
    /// executor's un-gated replay handler (which must never re-gate), mirroring the
    /// refund surface's gated/un-gated split.
    pub manual: Arc<ManualAdjustmentHandler>,
    /// The adjustment repo — the `GET …/exposure` read source (the
    /// `invoice_exposure` headroom row + the invoice's open AR).
    pub exposure_repo: AdjustmentRepo,
}

/// Build the Axum router for the adjustment surface and register every operation
/// with the supplied `OpenAPI` registry. `state` is attached via an `Extension`
/// layer at the end so the registry sees the route definitions before the
/// per-request state is bound. Mirrors [`crate::api::rest::recognition::router`].
#[allow(clippy::too_many_lines)] // one OperationBuilder chain per route; splitting hurts readability
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/credit-notes")
        .operation_id("bss_ledger.post_credit_note")
        .summary("Post a credit note (compensating adjustment)")
        .description(
            "Posts a balanced compensating credit note against a posted invoice \
             for the seller named by the body's `tenant_id` (DR CONTRA_REVENUE — \
             or GOODWILL for an AR-only goodwill credit — + per-stream DR \
             CONTRACT_LIABILITY over the unreleased deferred + DR TAX_PAYABLE; CR \
             AR capped at the invoice's current open AR + CR REUSABLE_CREDIT for \
             any paid-invoice remainder, K-2). It never mutates the posted invoice \
             rows and, in the SAME txn, reduces the owning recognition schedule's \
             deferred total (so a later S6 run cannot re-recognize the credited- \
             back amount) and bumps the invoice's credit-note headroom counter. \
             `requested_deferred_minor` is the split INTENT (how much targets the \
             deferred balance). Idempotent on `credit_note_id`: a re-post returns \
             the prior posting reference (200) instead of a new one (201). Rejected \
             when the recognized-vs-deferred split is indeterminable \
             (CREDIT_NOTE_SPLIT_AMBIGUOUS — never a silent pro-rata) or the note \
             would push past the invoice's headroom \
             (CREDIT_NOTE_EXCEEDS_HEADROOM — route over-cap via goodwill/non- \
             revenue, never silently through S3).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreditNoteRequest>(
            openapi,
            "The credit note (amounts incl-tax, split intent, goodwill flag) + the \
             idempotency `credit_note_id`.",
        )
        .handler(post_credit_note)
        .json_response_with_schema::<CreditNoteResponse>(
            openapi,
            StatusCode::CREATED,
            "Posting reference (201 fresh post / 200 idempotent replay)",
        )
        .json_response_with_schema::<CreditNoteResponse>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior credit note",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/debit-notes")
        .operation_id("bss_ledger.post_debit_note")
        .summary("Post a debit note (additional charge)")
        .description(
            "Posts an additional charge against a posted invoice for the seller \
             named by the body's `tenant_id` — a DIRECT split mirroring the \
             invoice-post (DR AR incl-tax / CR REVENUE recognized-now / CR \
             CONTRACT_LIABILITY deferred per PO / CR TAX_PAYABLE). When the note \
             defers (`deferred_minor > 0`) it builds the releasing recognition \
             schedule in the SAME txn (D4 — the `recognition` spec is required) so \
             a later S6 run can release it, and it RAISES the invoice's headroom \
             (`debit_note_total += amount`), widening the room for later credit \
             notes (it can never trip the headroom cap). Idempotent on \
             `debit_note_id`: a re-post returns the prior posting reference (200) \
             instead of a new one (201). A closed payer cannot be charged \
             (PAYER_CLOSED).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<DebitNoteRequest>(
            openapi,
            "The debit note (amount incl-tax, deferred part + recognition spec) + \
             the idempotency `debit_note_id`.",
        )
        .handler(post_debit_note)
        .json_response_with_schema::<DebitNoteResponse>(
            openapi,
            StatusCode::CREATED,
            "Posting reference (201 fresh post / 200 idempotent replay)",
        )
        .json_response_with_schema::<DebitNoteResponse>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior debit note",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/manual-adjustments")
        .operation_id("bss_ledger.post_manual_adjustment")
        .summary("Post a governed manual adjustment (correction escape hatch)")
        .description(
            "Posts a GOVERNED manual adjustment for the seller named by the body's \
             `tenant_id` (design §4.6) — the ledger's escape hatch for corrections \
             the typed flows (invoice / settle / allocate / S3 notes / S4 \
             recognition) do not cover: rounding residue, suspense / cash-clearing \
             clean-up. The `action` selects a CODE-OWNED allow-list of account \
             classes the legs may touch; `REVENUE` and `CONTRACT_LIABILITY` are \
             globally off-limits (revenue changes route through S3/S4/S6) and an \
             unpaired `CONTRA_REVENUE` leg is rejected as an attempted (disguised \
             bad-debt) write-off — all `MANUAL_ADJUSTMENT_NOT_ALLOWED` (400), the \
             write-off additionally captured + paged. The legs MUST net to zero \
             (Σ DR == Σ CR) and a `reason_code` is mandatory (AC #14). The preparer \
             actor is the AUTHENTICATED subject (stamped server-side, never read from \
             the body); the approver is assigned by the dual-control flow. A governed \
             adjustment whose gross (Σ DR) crosses the tenant's D2 threshold routes \
             to dual-control (409 DUAL_CONTROL_REQUIRED) instead of posting inline. \
             Idempotent on `adjustment_id` (the `(tenant, MANUAL_ADJUSTMENT, \
             adjustment_id)` engine claim): a re-post returns the prior posting \
             reference (200) instead of a new one (201).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ManualAdjustmentRequest>(
            openapi,
            "The governed adjustment (action, balanced legs, reason) + the \
             idempotency `adjustment_id`.",
        )
        .handler(post_manual_adjustment)
        .json_response_with_schema::<ManualAdjustmentResponse>(
            openapi,
            StatusCode::CREATED,
            "Posting reference (201 fresh post / 200 idempotent replay)",
        )
        .json_response_with_schema::<ManualAdjustmentResponse>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior manual adjustment",
        )
        // The over-D2 dual-control path (409 DUAL_CONTROL_REQUIRED) flows through the
        // `From<DomainError> for CanonicalError` ladder (`DualControlRequired` →
        // `aborted`), like the refund surface — the platform `OperationBuilder` has no
        // `.error_409` helper, so it is not declared here (the 409 still returns).
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/invoices/{invoice_id}/exposure")
        .operation_id("bss_ledger.get_invoice_exposure")
        .summary("Read an invoice's credit-note headroom + remaining AR")
        .description(
            "Returns the invoice's credit-note HEADROOM (the `invoice_exposure` \
             counter: `original_total` seeded = posted AR incl-tax, plus the \
             running debit-note / credit-note totals, with \
             `remaining_headroom = original + debit − credit`, the slack in the AC \
             #24 CHECK) plus its TRUE remaining open AR (the payment-reduced \
             receivable a credit note's `CR AR` leg is capped at — SEPARATE from \
             the headroom, which never decreases with payments). The schedule PK is \
             `(tenant_id, invoice_id)`, so the owning seller `tenant_id` is required \
             in the query (like the recognition-schedule read). Tenant-scoped \
             (SQL-level BOLA): an invoice with no note posted yet (no exposure row) \
             — or one outside the caller's authorized subtree — yields a 404 (no \
             existence leak).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("invoice_id", "The posted invoice whose exposure to read.")
        .query_param(
            "tenant_id",
            true,
            "The invoice's owning seller tenant (the exposure PK's tenant half).",
        )
        .handler(get_invoice_exposure)
        .json_response_with_schema::<InvoiceExposureResponse>(
            openapi,
            StatusCode::OK,
            "The invoice's headroom counters + remaining open AR",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/credit-notes/{credit_note_id}")
        .operation_id("bss_ledger.get_credit_note")
        .summary("Read a recorded credit note")
        .description(
            "Returns the recorded credit note for `(tenant_id, credit_note_id)` — \
             its origin invoice (+ item ref), revenue stream, currency, incl-tax \
             amount, and the ex-tax recognized/deferred split parts (which do NOT \
             sum to the amount). The PK is `(tenant_id, credit_note_id)`, so the \
             owning seller `tenant_id` is required in the query. Tenant-scoped \
             (SQL-level BOLA): an unknown credit note — or one outside the caller's \
             authorized subtree — yields a 404 (no existence leak). Mirrors \
             `get_refund`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("credit_note_id", "The credit note whose record to read.")
        .query_param(
            "tenant_id",
            true,
            "The credit note's owning seller tenant (the credit_note PK's tenant half).",
        )
        .handler(get_credit_note)
        .json_response_with_schema::<CreditNoteView>(
            openapi,
            StatusCode::OK,
            "The recorded credit note",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/credit-notes")
        .operation_id("bss_ledger.list_credit_notes")
        .summary("List recorded credit notes (cursor-paginated)")
        .description(
            "Cursor-paginated list of the recorded credit notes for the seller \
             named by `$filter=tenant_id eq <uuid>` (the caller's own by default). \
             Supports OData `$filter` over `tenant_id`, `origin_invoice_id`, \
             `revenue_stream`, and `reason_code`. The `$filter` ANDs the caller's \
             authorized subtree, so credit notes outside it are never returned \
             (SQL-level BOLA). Each item is the same `CreditNoteView` the by-id \
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
        .handler(list_credit_notes)
        .with_odata_filter::<CreditNoteFilterField>()
        .with_odata_orderby::<CreditNoteFilterField>()
        .json_response_with_schema::<Page<CreditNoteView>>(
            openapi,
            StatusCode::OK,
            "One page of recorded credit notes",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/debit-notes/{debit_note_id}")
        .operation_id("bss_ledger.get_debit_note")
        .summary("Read a recorded debit note")
        .description(
            "Returns the recorded debit note (an additional charge) for `(tenant_id, \
             debit_note_id)` — its origin invoice, currency, incl-tax amount, and \
             the ex-tax recognized/deferred split parts (which do NOT sum to the \
             amount). The PK is `(tenant_id, debit_note_id)`, so the owning seller \
             `tenant_id` is required in the query. Tenant-scoped (SQL-level BOLA): an \
             unknown debit note — or one outside the caller's authorized subtree — \
             yields a 404 (no existence leak). Mirrors `get_credit_note`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("debit_note_id", "The debit note whose record to read.")
        .query_param(
            "tenant_id",
            true,
            "The debit note's owning seller tenant (the debit_note PK's tenant half).",
        )
        .handler(get_debit_note)
        .json_response_with_schema::<DebitNoteView>(
            openapi,
            StatusCode::OK,
            "The recorded debit note",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/debit-notes")
        .operation_id("bss_ledger.list_debit_notes")
        .summary("List recorded debit notes (cursor-paginated)")
        .description(
            "Cursor-paginated list of the recorded debit notes for the seller \
             named by `$filter=tenant_id eq <uuid>` (the caller's own by default). \
             Supports OData `$filter` over `tenant_id` and `origin_invoice_id`. \
             The `$filter` ANDs the caller's authorized subtree, so debit notes \
             outside it are never returned (SQL-level BOLA). Each item is the \
             same `DebitNoteView` the by-id read returns. Mirrors \
             `list_credit_notes`.",
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
        .handler(list_debit_notes)
        .with_odata_filter::<DebitNoteFilterField>()
        .with_odata_orderby::<DebitNoteFilterField>()
        .json_response_with_schema::<Page<DebitNoteView>>(
            openapi,
            StatusCode::OK,
            "One page of recorded debit notes",
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
// authenticated-only data is disclosed). Mirrors `journal_entries::post_invoice`.
async fn post_credit_note(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<CreditNoteRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id` (tenant in body, not path).
    let tenant_id = body.tenant_id;
    // (entry, post) PEP gate against the TARGET tenant: a credit note posts a
    // compensating journal entry into the seller's ledger (the SAME data-plane post
    // action as the run trigger / invoice post). A target outside the caller's
    // scope is a cross-tenant write and is denied. The returned scope threads into
    // the scoped post (the SQL-level BOLA filter); the handler re-gates nothing.
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
    // The split-ambiguous (CREDIT_NOTE_SPLIT_AMBIGUOUS) + over-headroom
    // (CREDIT_NOTE_EXCEEDS_HEADROOM) rejections flow through the single
    // `From<DomainError> for CanonicalError` ladder (`infra::error_mapping`) via
    // `?` — no REST-layer mapping needed.
    let reference = state
        .credit
        .post_credit_note(&ctx, &scope, req)
        .await
        .map_err(CanonicalError::from)?;
    Ok(credit_note_response(reference))
}

async fn post_debit_note(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<DebitNoteRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = body.tenant_id;
    // (entry, post) PEP gate against the TARGET tenant — same as the credit note.
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
    // `payer_open = true` at the REST seam: the gear has no payer-state reader here,
    // and the foundation account-lifecycle invariant is the authority for a
    // genuinely closed payer (a closed AR account is rejected at post time) —
    // byte-identical to `journal_entries::post_invoice`'s `payer_open = true` seam.
    // The gear-side fast-path payer gate (`!payer_open ⇒ PAYER_CLOSED`) is exercised
    // by the Group D integration test driving `post_debit_note(..., false)` directly.
    let reference = state
        .debit
        .post_debit_note(&ctx, &scope, req, /* payer_open */ true)
        .await
        .map_err(CanonicalError::from)?;
    Ok(debit_note_response(reference))
}

async fn post_manual_adjustment(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<ManualAdjustmentRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = body.tenant_id;
    // (entry, post) PEP gate against the TARGET tenant — same as the credit / debit
    // note: a governed manual adjustment posts a balanced journal entry into the
    // seller's ledger. A target outside the caller's scope is a cross-tenant write
    // and is denied. The returned scope threads into the scoped post (the SQL-level
    // BOLA filter); the handler re-gates nothing.
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

    // The preparer is the AUTHENTICATED subject (never the body); the approver is
    // assigned by the dual-control flow (into_domain sets it `None`).
    let req = body
        .into_domain(ctx.subject_id())
        .map_err(CanonicalError::from)?;
    // The governance reject (MANUAL_ADJUSTMENT_NOT_ALLOWED → 400) and the over-D2
    // dual-control route (DUAL_CONTROL_REQUIRED → 409) both flow through the single
    // `From<DomainError> for CanonicalError` ladder (`infra::error_mapping`) via `?`.
    let reference = state
        .manual
        .post_manual_adjustment(&ctx, &scope, req)
        .await
        .map_err(CanonicalError::from)?;
    Ok(manual_adjustment_response(reference))
}

/// A credit/debit-note posting rendered with the right status: `201 Created` for a
/// fresh post, `200 OK` for an idempotent replay of a prior note. Mirrors
/// `journal_entries::posting_response`.
fn credit_note_response(reference: bss_ledger_sdk::PostingRef) -> Response {
    let status = if reference.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (status, Json(CreditNoteResponse::from(reference))).into_response()
}

/// As [`credit_note_response`] but for the debit-note body.
fn debit_note_response(reference: bss_ledger_sdk::PostingRef) -> Response {
    let status = if reference.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (status, Json(DebitNoteResponse::from(reference))).into_response()
}

/// As [`credit_note_response`] but for the manual-adjustment body: `201 Created`
/// for a fresh governed post, `200 OK` for an idempotent replay of a prior
/// adjustment (the dual-control 409 flows through the `From<DomainError>` ladder).
fn manual_adjustment_response(reference: bss_ledger_sdk::PostingRef) -> Response {
    let status = if reference.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (status, Json(ManualAdjustmentResponse::from(reference))).into_response()
}

/// `GET /invoices/{invoice_id}/exposure` query parameters: the invoice's owning
/// seller `tenant_id` (the exposure PK is `(tenant_id, invoice_id)`, so the tenant
/// is REQUIRED in the query — like the by-id recognition-schedule read).
#[derive(Debug, serde::Deserialize)]
struct ExposureQuery {
    tenant_id: Uuid,
}

async fn get_invoice_exposure(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(invoice_id): Path<String>,
    Query(query): Query<ExposureQuery>,
) -> Result<Json<InvoiceExposureResponse>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (entry, read) PEP gate against the invoice's owning tenant — the SAME action
    // the balance / schedule reads run under (the exposure counters are drawn down
    // from the `entry` ledger). The returned scope is the SQL-level BOLA filter the
    // repo binds, so a foreign-tenant invoice resolves to None ⇒ 404 (no existence
    // leak), mirroring `recognition::get_recognition_schedule`.
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

    // The headroom row: `None` ⇒ absent (no note posted on this invoice yet) OR
    // scoped-out — a canonical 404 either way (no existence leak), mirroring
    // `recognition_schedule_not_found`.
    let exposure = state
        .exposure_repo
        .read_exposure_out_of_txn(&scope, tenant_id, &invoice_id)
        .await
        // A scoped read failure is an infra fault ⇒ Internal (→ 500), mirroring how
        // `CreditNoteHandler::read_ar_caps` maps its repo read errors. There is no
        // `From<RepoError> for DomainError`, so map explicitly.
        .map_err(|e| crate::domain::error::DomainError::Internal(format!("read exposure: {e}")))?
        .ok_or_else(|| invoice_exposure_not_found(&invoice_id))?;
    // The SEPARATE true remaining open AR (the `CR AR` cap). Scoped by the same
    // gate; an invoice with no open AR reads 0 (a fully-paid / fully-credited
    // invoice still has a headroom row).
    let open_ar = state
        .exposure_repo
        .read_open_ar_for_invoice_out_of_txn(&scope, tenant_id, &invoice_id)
        .await
        .map_err(|e| crate::domain::error::DomainError::Internal(format!("read open AR: {e}")))?;

    // Remaining headroom = original + debit − credit (the slack in the AC #24
    // CHECK), floored at 0 (the CHECK guarantees `credit <= original + debit`, so
    // this never goes negative; the saturating sub is defensive).
    let remaining_headroom_minor = exposure
        .original_total_minor
        .saturating_add(exposure.debit_note_total_minor)
        .saturating_sub(exposure.credit_note_total_minor)
        .max(0);
    Ok(Json(InvoiceExposureResponse {
        invoice_id: exposure.invoice_id,
        currency: exposure.currency,
        original_total_minor: exposure.original_total_minor,
        debit_note_total_minor: exposure.debit_note_total_minor,
        credit_note_total_minor: exposure.credit_note_total_minor,
        remaining_headroom_minor,
        open_ar_minor: open_ar,
    }))
}

/// `GET /credit-notes/{credit_note_id}` query parameters: the credit note's owning
/// seller `tenant_id` (the `credit_note` PK is `(tenant_id, credit_note_id)`, so the
/// tenant is REQUIRED in the query — like the by-id refund / exposure reads).
#[derive(Debug, serde::Deserialize)]
struct CreditNoteQuery {
    tenant_id: Uuid,
}

async fn get_credit_note(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(credit_note_id): Path<String>,
    Query(query): Query<CreditNoteQuery>,
) -> Result<Json<CreditNoteView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (entry, read) PEP gate against the credit note's owning tenant — the SAME
    // action the exposure / balance reads run under. The returned scope is the
    // SQL-level BOLA filter the repo binds, so a foreign-tenant credit note resolves
    // to None ⇒ 404 (no existence leak), mirroring `refunds::get_refund`.
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

    let note = state
        .exposure_repo
        .read_credit_note_out_of_txn(&scope, tenant_id, &credit_note_id)
        .await
        .map_err(|e| crate::domain::error::DomainError::Internal(format!("read credit_note: {e}")))?
        .ok_or_else(|| credit_note_not_found(&credit_note_id))?;
    Ok(Json(CreditNoteView::from(note)))
}

/// `GET /credit-notes`: seller from `$filter=tenant_id eq`, else the caller.
#[allow(clippy::implicit_hasher)]
async fn list_credit_notes(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<CreditNoteView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    // (entry, read) PEP gate against the credit notes' owning tenant — the SAME
    // action the by-id read / balances run under. The returned scope is the
    // SQL-level BOLA filter the repo binds, so the page never contains a
    // foreign-tenant credit note (no existence leak), mirroring `refunds::list_refunds`.
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
        .exposure_repo
        .list_credit_notes(&scope, tenant_id, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.into_iter().map(CreditNoteView::from).collect(),
        page_info: page.page_info,
    }))
}

/// `GET /debit-notes/{debit_note_id}` query parameters: the debit note's owning
/// seller `tenant_id` (the `debit_note` PK is `(tenant_id, debit_note_id)`, so the
/// tenant is REQUIRED in the query — like the by-id credit-note read).
#[derive(Debug, serde::Deserialize)]
struct DebitNoteQuery {
    tenant_id: Uuid,
}

async fn get_debit_note(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(debit_note_id): Path<String>,
    Query(query): Query<DebitNoteQuery>,
) -> Result<Json<DebitNoteView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id;
    // (entry, read) PEP gate against the debit note's owning tenant — the SAME
    // action the exposure / balance reads run under. The returned scope is the
    // SQL-level BOLA filter the repo binds, so a foreign-tenant debit note resolves
    // to None ⇒ 404 (no existence leak), mirroring `get_credit_note`.
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

    let note = state
        .exposure_repo
        .read_debit_note_out_of_txn(&scope, tenant_id, &debit_note_id)
        .await
        .map_err(|e| crate::domain::error::DomainError::Internal(format!("read debit_note: {e}")))?
        .ok_or_else(|| debit_note_not_found(&debit_note_id))?;
    Ok(Json(DebitNoteView::from(note)))
}

/// `GET /debit-notes`: seller from `$filter=tenant_id eq`, else the caller.
#[allow(clippy::implicit_hasher)]
async fn list_debit_notes(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<DebitNoteView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    // (entry, read) PEP gate against the debit notes' owning tenant — the SAME
    // action the by-id read / balances run under. The returned scope is the
    // SQL-level BOLA filter the repo binds, so the page never contains a
    // foreign-tenant debit note (no existence leak), mirroring `list_credit_notes`.
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
        .exposure_repo
        .list_debit_notes(&scope, tenant_id, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.into_iter().map(DebitNoteView::from).collect(),
        page_info: page.page_info,
    }))
}
