//! Axum handlers + router for the ledger's journal-entry / balance REST surface
//! (architecture §6). Eight operations under `/bss-ledger/v1`, all tenant-scoped
//! WITHOUT a tenant in the path (the vhp-core convention, matching the
//! provisioning surface): writes carry `tenant_id` in the **body**, reads take a
//! `?tenant_id=` **query** param (the caller's own tenant by default); both are
//! PDP-`In` scoped (SQL-level BOLA).
//!
//! Writes (post / reverse / mapping-correction):
//! - `POST /journal-entries` — body `tenant_id`; PEP `(entry, post)` against it;
//!   drives the `InvoicePostService` (metrics + suspense). `201` fresh /
//!   `200` idempotent replay.
//! - `POST /journal-entries/{entryId}/reversals` — `{entryId}` is the reversed
//!   entry id (NOT a tenant); tenant from the auth context; PEP `(entry,
//!   reverse)`. Reads the original, builds a strict line-negation reversal,
//!   posts it. A reverse-of-a-reversal is a 400 `CANNOT_REVERSE_REVERSAL`.
//! - `POST /journal-entries/{entryId}/mapping-corrections` — same gate; a
//!   reversal of the mis-mapped original followed by a corrected re-post.
//! - `PATCH /journal-entries/{entryId}/annotation` — PEP `(entry, annotate)`; sets
//!   ONE typed controlled non-financial note (`description`) on the entry (or a
//!   line). Records the current-state `entry_annotation` row + a `metadata-change`
//!   secured-audit record in one SERIALIZABLE txn; the journal stays byte-identical.
//!   A `description` carrying raw customer PII is refused `PII_IN_METADATA_VALUE`.
//!
//! Reads (`(entry, read)`, PDP-scoped):
//! - `GET /journal-entries/{entryId}` — tenant from the context.
//! - `GET /journal-lines?$filter=&$orderby=&cursor=&limit=` — canonical
//!   `$filter` over `tenant_id`/`payer_tenant_id`/`account_class`/`period_id`/`invoice_id`.
//! - `GET /balances?$filter=&$orderby=&cursor=&limit=` — `$filter` over
//!   `tenant_id`/`account_class`/`currency`.
//! - `GET /balances/ar-aging?tenant_id=&payer=` — buckets via
//!   [`crate::domain::invoice::aging::ar_aging`].
//!
//! Routes register through `OperationBuilder` so `/openapi.json` lists each
//! operation with its declared request / response schemas.

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
    ArAgingDto, BalanceDto, EntryAnnotationRequestDto, EntryDto, EntryHeaderView, LineDto,
    MappingCorrectionRequestDto, MaterializedScheduleDto, PostInvoiceRequestDto,
    PostInvoiceResponseDto, PostingRefDto, ReversalRequestDto,
};
use crate::api::rest::error::{
    authz_error_to_canonical, entry_not_found, reversal_error_to_canonical,
};
use crate::api::rest::odata_list::{
    list_seller_tenant, reject_non_odata_list_params, reject_non_odata_list_params_allowing,
};
use crate::domain::instant::to_naive_date;
use crate::domain::invoice::aging::ar_aging;
use crate::domain::invoice::builder::build_invoice_entry;
use crate::domain::invoice::mapping::resolve;
use crate::domain::invoice::policy::AgingThresholds;
use crate::domain::invoice::reversal::{build_mapping_correction, build_reversal};
use crate::infra::invoice_post::InvoicePoster;
use crate::infra::storage::repo::{JournalRepo, PostingPolicyRepo};
use crate::odata::{
    BalanceFilterField, JournalEntryFilterField, JournalLineFilterField, JournalLineOrderField,
};
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the journal-entry operations.
const TAG: &str = "BSS Ledger Journal";

/// Shared per-request state for the journal-entry routes. Built once at `init()`
/// and shared via `Extension<Arc<ApiState>>`. Carries BOTH the in-process
/// data-access client (reads + `get_entry`) and the `InvoicePostService` (the
/// write orchestrator that emits metrics + handles suspense on the post path).
#[derive(Clone)]
pub struct ApiState {
    /// In-process data-access client (reads: entry / lines / balances / AR).
    pub client: Arc<dyn LedgerClientV1>,
    /// Invoice-post write port (writes: post / reversal / mapping-correction).
    /// A trait object so the router tests can stub the post path without a DB.
    pub posting: Arc<dyn InvoicePoster>,
    /// Dual-control lifecycle engine (VHP-1852): the reverse gate routes a
    /// high-value reversal to the preparer→approver queue instead of posting it.
    /// `None` disables the gate (router unit tests without a governance DB).
    pub approval: Option<Arc<crate::infra::approval::service::ApprovalService>>,
    /// Typed controlled-annotation write port (Group 2B; the `PATCH …/annotation`
    /// surface). Records the `entry_annotation` current-state row + a
    /// `metadata-change` secured-audit record in one `SERIALIZABLE` transaction.
    /// A trait object so the router tests can stub it without a DB.
    pub annotation: Arc<dyn crate::infra::annotation::AnnotationWriter>,
    /// The journal repo — the `GET /journal-entries` HEADER-list read source (R5):
    /// a plain scoped read over its own db clone, called DIRECTLY from the handler
    /// (NOT through `client`), mirroring the refund / dispute read-surface repos.
    /// The by-id `GET /journal-entries/{entryId}` read still goes through `client`.
    /// `Option` ONLY so the stub-based REST tests (which carry no DB) can build
    /// `ApiState` with `None` (mirrors `approval`); production ALWAYS wires `Some`
    /// (see `module.rs`).
    pub journal_repo: Option<JournalRepo>,
    /// Tenant posting-policy repo (VHP-1853) — the AR-aging read resolves the
    /// tenant's configured bucket thresholds through it. `Option` for the stub
    /// REST tests (`None` ⇒ the gear default buckets); production wires `Some`.
    pub posting_policy: Option<PostingPolicyRepo>,
}

/// Build the Axum router for the journal-entry surface and register every
/// operation with the supplied `OpenAPI` registry. `state` is attached via an
/// `Extension` layer at the end so the registry sees the route definitions
/// before the per-request state is bound.
#[allow(clippy::too_many_lines)] // one builder chain per operation; flat is clearer than helpers
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/journal-entries")
        .operation_id("bss_ledger.post_invoice")
        .summary("Post a fully-recognized invoice (Variant A)")
        .description(
            "Builds + posts the balanced direct-split entry (DR AR / CR Revenue \
             per stream / CR Tax) for the invoice in the body. The target seller \
             ledger is the body's `tenant_id`. Idempotent on the invoice id: a \
             re-post returns the prior posting reference (200) instead of a new \
             one (201).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PostInvoiceRequestDto>(openapi, "The invoice to post.")
        .handler(post_invoice)
        .json_response_with_schema::<PostInvoiceResponseDto>(
            openapi,
            StatusCode::CREATED,
            "Posting reference + the recognition schedules materialized by the post \
             (201 fresh post)",
        )
        .json_response_with_schema::<PostInvoiceResponseDto>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior post (with its materialized schedules)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/journal-entries/{entryId}/reversals")
        .operation_id("bss_ledger.reverse_entry")
        .summary("Reverse a posted entry (strict line-negation)")
        .description(
            "Posts the reversing entry for `{entryId}`: the same accounts with \
             each line's side flipped and its amount kept positive. The tenant is \
             the caller's own. Reversing an entry that is itself a reversal is \
             rejected (400 CANNOT_REVERSE_REVERSAL) — correct forward by \
             re-posting, never stack reversals.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ReversalRequestDto>(openapi, "Reversal reason + optional target period.")
        .handler(reverse_entry)
        .json_response_with_schema::<PostingRefDto>(
            openapi,
            StatusCode::CREATED,
            "Posting reference of the reversal (201 / 200 replay)",
        )
        .json_response_with_schema::<PostingRefDto>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior post",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/journal-entries/{entryId}/mapping-corrections")
        .operation_id("bss_ledger.correct_mapping")
        .summary("Correct a mis-mapped entry (reverse + corrected re-post)")
        .description(
            "Clears the mis-mapped original `{entryId}` with a strict reversal, \
             then re-posts the corrected lines (MAPPING_CORRECTION, keyed on the \
             invoice id + a stable correction id so a retry replays). The tenant \
             is the caller's own.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<MappingCorrectionRequestDto>(
            openapi,
            "Correction reason + the corrected lines.",
        )
        .handler(correct_mapping)
        .json_response_with_schema::<PostingRefDto>(
            openapi,
            StatusCode::CREATED,
            "Posting reference of the corrected re-post (201 / 200 replay)",
        )
        .json_response_with_schema::<PostingRefDto>(
            openapi,
            StatusCode::OK,
            "Idempotent replay of a prior post",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/journal-entries/{entryId}")
        .operation_id("bss_ledger.get_entry")
        .summary("Read a posted entry with its lines")
        .description(
            "Returns the journal entry `{entryId}` (header + lines + audit dims) \
             for the caller's own tenant. An entry outside the caller's authorized \
             subtree yields 404 (no existence leak).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(get_entry)
        .json_response_with_schema::<EntryDto>(openapi, StatusCode::OK, "The entry with its lines")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/journal-entries")
        .operation_id("bss_ledger.list_journal_entries")
        .summary("List journal entry headers (cursor-paginated)")
        .description(
            "Cursor-paginated list of journal entry HEADERS for the seller named \
             by `$filter=tenant_id eq <uuid>` (the caller's own by default). \
             Supports OData `$filter` over `tenant_id`, `source_doc_type`, \
             `source_business_id`, and `period_id` — the header-only dims that \
             enable cross-cuts like \"all `MANUAL_ADJUSTMENT` entries\" or \
             \"all `REFUND` / `CREDIT_NOTE` entries\" (these live on the entry \
             HEADER, not on `journal_line`). Each item is a lightweight \
             `EntryHeaderView` (NO lines, NO hash-chain fields); read the full \
             entry with its lines via `GET /journal-entries/{entryId}`. The \
             `$filter` ANDs the caller's authorized subtree, so headers outside it \
             are never returned (SQL-level BOLA).",
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
        .handler(list_journal_entries)
        .with_odata_filter::<JournalEntryFilterField>()
        .with_odata_orderby::<JournalEntryFilterField>()
        .json_response_with_schema::<Page<EntryHeaderView>>(
            openapi,
            StatusCode::OK,
            "One page of journal entry headers",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch("/bss-ledger/v1/journal-entries/{entryId}/annotation")
        .operation_id("bss_ledger.set_entry_annotation")
        .summary("Set a controlled non-financial annotation")
        .description(
            "Sets ONE typed controlled non-financial note (`description`) on the entry \
             (or one of its lines via `target_kind=LINE` + `target_line_id`). The note \
             is screened for raw customer PII before any write (PII_IN_METADATA_VALUE). \
             Every change is recorded in the `entry_annotation` current-state row + a \
             secured-audit record, all under one SERIALIZABLE transaction. The gate is \
             `(entry, annotate)` against the entry's tenant.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<EntryAnnotationRequestDto>(openapi, "The annotation to set.")
        .handler(set_entry_annotation)
        .no_content_response(
            StatusCode::NO_CONTENT,
            "The metadata change was recorded (no body)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/journal-lines")
        .operation_id("bss_ledger.list_lines")
        .summary("List journal lines (cursor-paginated)")
        .description(
            "Cursor-paginated list of journal lines for the seller named by \
             `$filter=tenant_id eq <uuid>` (the caller's own by default). \
             Supports OData `$filter` over `tenant_id`, `payer_tenant_id`, \
             `account_class`, `period_id`, and `invoice_id`. The `$filter` ANDs \
             the caller's authorized subtree, so rows outside it are never \
             returned (SQL-level BOLA).",
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
        .handler(list_lines)
        .with_odata_filter::<JournalLineFilterField>()
        .with_odata_orderby::<JournalLineOrderField>()
        .json_response_with_schema::<Page<LineDto>>(
            openapi,
            StatusCode::OK,
            "One page of journal lines",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/balances")
        .operation_id("bss_ledger.list_balances")
        .summary("List account-balance cache rows (cursor-paginated)")
        .description(
            "Cursor-paginated list of the account-balance cache rows for the \
             seller named by `$filter=tenant_id eq <uuid>` (the caller's own by \
             default). Supports OData `$filter` over `tenant_id`, `account_class` \
             and `currency`. The `$filter` ANDs the caller's authorized subtree, \
             so rows outside it are excluded (SQL-level BOLA). Each row carries \
             BOTH the transaction-currency `balance_minor` and the Slice-5 \
             functional valuation (`functional_balance_minor` / \
             `functional_currency`); the latter is `null` on a single-currency \
             grain, where the functional value equals `balance_minor` by identity \
             (`?valuation=functional` fallback, P1 decision 8).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "valuation",
            false,
            "Valuation lens: `transaction` (default) or `functional`. Both columns \
             are always returned; this selects which the client should read \
             (functional falls back to the transaction balance on a \
             single-currency grain).",
        )
        .query_param_typed(
            "limit",
            false,
            "Maximum items per page (default 25, max 200)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_balances)
        .with_odata_filter::<BalanceFilterField>()
        .with_odata_orderby::<BalanceFilterField>()
        .json_response_with_schema::<Page<BalanceDto>>(
            openapi,
            StatusCode::OK,
            "One page of account-balance cache rows in scope",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/balances/ar-aging")
        .operation_id("bss_ledger.ar_aging")
        .summary("AR aging buckets")
        .description(
            "Buckets the open per-invoice AR for the `tenant_id` query (the \
             caller's own by default), optionally narrowed to one `payer`, into \
             days-past-due buckets (current / 1-30 / 31-60 / 61-90 / 90+) per \
             (payer, currency).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "tenant_id",
            false,
            "Target tenant (defaults to the caller's own)",
        )
        .query_param("payer", false, "Narrow to one payer tenant id")
        .handler(ar_aging_handler)
        .json_response_with_schema::<ArAgingDto>(openapi, StatusCode::OK, "The aged AR buckets")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// A posting reference rendered with the right status: `201 Created` for a fresh
/// post, `200 OK` for an idempotent replay of a prior post.
fn posting_response(reference: bss_ledger_sdk::PostingRef) -> Response {
    let status = if reference.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (status, Json(PostingRefDto::from(reference))).into_response()
}

/// The invoice-post response: like [`posting_response`] (`201` fresh / `200`
/// replay) but carrying the recognition schedules the post materialized so a
/// REST client learns the server-minted `schedule_id`(s) (`schedules` is empty
/// for a point-in-time invoice).
fn post_invoice_response(
    reference: bss_ledger_sdk::PostingRef,
    schedules: Vec<MaterializedScheduleDto>,
) -> Response {
    let status = if reference.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let body = PostInvoiceResponseDto {
        entry_id: reference.entry_id,
        created_seq: reference.created_seq,
        replayed: reference.replayed,
        schedules,
    };
    (status, Json(body)).into_response()
}

// The `CanonicalJson` extractor runs (and may reject with a canonical 400)
// BEFORE the in-handler `require_authenticated` gate, so a malformed body yields
// 400 even for an unauthenticated caller (standard axum extractor ordering; no
// authenticated-only data is disclosed).
async fn post_invoice(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<PostInvoiceRequestDto>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // The target seller is the body's `tenant_id` (tenant in body, not path).
    let tenant_id = body.tenant_id;
    // Captured before `into_domain` consumes the body: whether to look up the
    // materialized schedules after the post (skip the extra read for a
    // point-in-time invoice that mints none), and the invoice id to filter by.
    let has_recognition = body.items.iter().any(|it| it.recognition.is_some());
    let invoice_id = body.invoice_id.clone();
    // (entry, post) PEP gate against the TARGET tenant: a parent posts into a
    // seller in its authorized subtree; a target outside the caller's scope is a
    // cross-tenant write and is denied. The returned scope threads into the
    // scoped post (the SQL-level BOLA filter).
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

    // `posted_by_actor_id` is the authenticated subject, stamped server-side —
    // never read from the body (matching reverse_entry/correct_mapping, which
    // also stamp the actor from `ctx.subject_id()`).
    let inv = body
        .into_domain(ctx.subject_id())
        .map_err(CanonicalError::from)?;

    // Dual-control gate (VHP-1852, Group J): a post whose `effective_at` predates
    // the tenant's A6 backdating window is a *material backdating* — route it to
    // the preparer→approver queue (409) instead of posting inline. The ledger
    // object does not exist yet (the gate fires before the post) and the source
    // invoice is external (pushed in the body, never pulled), so the whole post is
    // snapshotted into the intent for replay on approve. A current-dated post (the
    // common case) resolves to `None` and stays single-actor, unchanged.
    if let Some(approval) = &state.approval {
        let intent = crate::domain::approval::intent::ApprovalIntent::MaterialBackdating(
            crate::domain::approval::intent::BackdatedPost::Invoice(
                crate::domain::approval::intent::BackdatedInvoiceSnapshot::from(&inv),
            ),
        );
        let facts = crate::domain::approval::policy::OperationFacts {
            kind: crate::domain::approval::ApprovalKind::MaterialBackdating,
            amount_usd_eq_minor: Some(inv.gross_minor()),
            effective_at: Some(inv.effective_at),
            has_outstanding_balance: false,
        };
        let reason = format!(
            "material backdating of invoice {} to {}",
            inv.invoice_id, inv.effective_at
        );
        if let Some(approval_id) = approval
            .gate(&ctx, &scope, intent, facts, reason)
            .await
            .map_err(CanonicalError::from)?
        {
            return Err(CanonicalError::from(
                crate::domain::error::DomainError::DualControlRequired(format!(
                    "material backdating requires dual-control approval: {approval_id}"
                )),
            ));
        }
    }

    // The 201/200 response echoes the materialized recognition schedules, which
    // the in-process client reads under `(recognition, read)` (see
    // `list_recognition_schedules` → `read_recognition_scope`). Preflight that same
    // grant HERE — before the post commits — so a post-only caller fails fast with
    // 403 instead of committing the invoice and THEN 403-ing on the echo read (a
    // committed-but-failed response with ambiguous retry semantics). Only a
    // deferred invoice echoes a schedule, so only it additionally requires
    // `(recognition, read)`. Mirror the echo read's args exactly (no target-tenant
    // anchor) so the preflight neither over- nor under-approves relative to it.
    if has_recognition {
        crate::authz::access_scope(
            &enforcer,
            &ctx,
            &crate::authz::resource_types::RECOGNITION,
            crate::authz::actions::READ,
            None,
            None,
            /* require_constraints */ true,
        )
        .await
        .map_err(authz_error_to_canonical)?;
    }

    // `payer_open = true` at the REST seam: the gear has no payer-state reader,
    // and the foundation account-lifecycle invariant is the authority for a
    // genuinely closed payer (a closed AR account is rejected at post time). The
    // gear-side fast-path payer gate is exercised by the Group C integration
    // test, which drives `InvoicePostService::post_invoice` directly.
    let reference = state
        .posting
        .post_invoice(&ctx, &scope, &inv, true)
        .await
        .map_err(CanonicalError::from)?;
    // Surface the schedule_id(s) this post materialized so a REST client can read
    // / change them without subscribing to the event bus (the ids are minted
    // server-side in the post txn). Read-after-commit by `(tenant, invoice_id)` —
    // covers both a fresh post and an idempotent replay; skipped entirely for a
    // point-in-time invoice that mints no schedule. The same `(entry, read)` scope
    // the by-id / list reads run under.
    let schedules = if has_recognition {
        state
            .client
            .list_recognition_schedules(&ctx, tenant_id, Some(invoice_id), None)
            .await?
            .schedules
            .into_iter()
            .map(MaterializedScheduleDto::from)
            .collect()
    } else {
        Vec::new()
    };
    Ok(post_invoice_response(reference, schedules))
}

async fn reverse_entry(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(entry_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<ReversalRequestDto>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Cap the unbounded audit `reason` free text at the boundary (400) before any work.
    body.validate().map_err(CanonicalError::from)?;
    // Tenant from the auth context; the scoped `get_entry` read enforces BOLA
    // (a foreign entry resolves to None ⇒ 404, no existence leak).
    let tenant_id = ctx.subject_tenant_id();
    let original = state
        .client
        .get_entry(&ctx, tenant_id, entry_id)
        .await?
        .ok_or_else(|| entry_not_found(entry_id))?;

    // (entry, reverse) gate against the original's owning tenant.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::REVERSE,
        Some(original.tenant_id),
        None,
        true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Dual-control gate (VHP-1852): a reversal whose magnitude crosses the D2
    // threshold routes to the preparer→approver queue (409) instead of posting
    // inline; below threshold it stays single-actor (unchanged).
    if let Some(approval) = &state.approval {
        let reverse_amount: i64 = original
            .lines
            .iter()
            .filter(|l| l.side == bss_ledger_sdk::Side::Debit)
            .map(|l| l.amount_minor)
            .sum();
        let intent = crate::domain::approval::intent::ApprovalIntent::Reverse(
            crate::domain::approval::intent::ReverseIntent {
                entry_id: original.entry_id,
                into_period_id: body.period_id.clone(),
                effective_at: body.effective_at,
                reason: body.reason.clone(),
            },
        );
        let facts = crate::domain::approval::policy::OperationFacts {
            kind: crate::domain::approval::ApprovalKind::Reverse,
            amount_usd_eq_minor: Some(reverse_amount),
            effective_at: None,
            has_outstanding_balance: false,
        };
        if let Some(approval_id) = approval
            .gate(&ctx, &scope, intent, facts, body.reason.clone())
            .await
            .map_err(CanonicalError::from)?
        {
            return Err(CanonicalError::from(
                crate::domain::error::DomainError::DualControlRequired(format!(
                    "reversal requires dual-control approval: {approval_id}"
                )),
            ));
        }
    }

    let into_period = body.period_id.unwrap_or_else(|| original.period_id.clone());
    let effective_on = body
        .effective_at
        .unwrap_or_else(|| to_naive_date(OffsetDateTime::now_utc()));
    let reversal = build_reversal(
        &original,
        into_period,
        effective_on,
        ctx.subject_id(),
        original.correlation_id,
    )
    .map_err(reversal_error_to_canonical)?;

    let reference = state
        .posting
        .post_reversal(&ctx, &scope, reversal, Some(body.reason))
        .await
        .map_err(CanonicalError::from)?;
    Ok(posting_response(reference))
}

/// `MAPPING_CORRECTION` is **two posts in two transactions** — a reversal of the
/// mis-mapped original, then a corrected re-post — made safe by **idempotent
/// retry**, not a single atomic transaction (architecture §4.2 / I-13). The
/// `correction_id = hash(original, reversal)` is computed from the **replayed**
/// reversal id, so re-issuing the same request after a crash between the two
/// posts replays the reversal (no double) and deterministically re-keys the
/// correction (posts or replays) — the flow self-heals on retry. Residual: a
/// crash with no retry leaves a dangling reversal (accepted trade-off of the
/// idempotent-retry design; true single-txn atomicity would need a foundation
/// multi-entry-per-txn engine the architecture deliberately avoided).
async fn correct_mapping(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(entry_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<MappingCorrectionRequestDto>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = ctx.subject_tenant_id();
    let original = state
        .client
        .get_entry(&ctx, tenant_id, entry_id)
        .await?
        .ok_or_else(|| entry_not_found(entry_id))?;

    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::REVERSE,
        Some(original.tenant_id),
        None,
        true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Re-map the corrected items to their GL targets (parse class literals at
    // the boundary ⇒ 400 on a bad literal) before any ledger effect.
    let corrected_items = body
        .corrected_items_into_domain()
        .map_err(CanonicalError::from)?;

    let into_period = body
        .period_id
        .clone()
        .unwrap_or_else(|| original.period_id.clone());
    let effective_on = body
        .effective_at
        .unwrap_or_else(|| to_naive_date(OffsetDateTime::now_utc()));

    // Cross-currency mapping-correction is not yet supported: the reversal half
    // (step 1) clears the original's functional at the original rate, but the
    // corrected re-post (step 2) is freshly rebuilt and would re-book
    // transaction-only — dropping the invoice's functional (a silent functional
    // drift on the corrected grains, which `m031` cannot catch because they seed as
    // single-currency). Reject up front until Slice 7 carries the functional onto
    // the corrected lines; single-currency corrections are unaffected.
    if original
        .lines
        .iter()
        .any(|l| l.functional_currency.is_some())
    {
        return Err(CanonicalError::from(
            crate::domain::error::DomainError::FxOperationUnsupported(format!(
                "cross-currency mapping correction for entry {} is not yet supported \
                 (the corrected re-post cannot carry the original functional rate — Slice 7)",
                original.entry_id
            )),
        ));
    }

    // 1. Reverse the mis-mapped original (clears the bad mapping).
    let reversal = build_reversal(
        &original,
        into_period.clone(),
        effective_on,
        ctx.subject_id(),
        original.correlation_id,
    )
    .map_err(reversal_error_to_canonical)?;
    let reversal_ref = state
        .posting
        // A mapping-correction's reversal leg is internal plumbing, not a §6
        // reversal — pass `None` so it announces no `entry.reversed`.
        .post_reversal(&ctx, &scope, reversal, None)
        .await
        .map_err(CanonicalError::from)?;

    // 2. Re-post the corrected lines (MAPPING_CORRECTION). The invoice id is the
    //    original's AR `source_business_id`; the corrected lines are rebuilt as a
    //    fresh direct-split entry, then re-keyed under the correction.
    let invoice_id = &original.source_business_id;
    let mapped: Vec<_> = corrected_items.iter().map(resolve).collect();
    let rebuilt = corrected_invoice(&original, invoice_id, &corrected_items, &mapped);
    let correction = build_mapping_correction(
        &original,
        reversal_ref.entry_id,
        invoice_id,
        into_period,
        effective_on,
        ctx.subject_id(),
        original.correlation_id,
        rebuilt.lines,
    );
    // The corrected re-post's lines carry nil placeholder account_ids (freshly
    // built); `post_correction` binds them from the chart before posting.
    let reference = state
        .posting
        .post_correction(&ctx, &scope, correction)
        .await
        .map_err(CanonicalError::from)?;
    Ok(posting_response(reference))
}

/// Build the corrected re-post lines as a direct-split entry over the original's
/// payer/seller, so `build_mapping_correction` can re-key + re-head them. The
/// corrected lines still need their chart `account_id`s bound; that binding is
/// the `InvoicePostService`'s job on the post path, so the rebuilt entry carries
/// the nil placeholders the builder emits.
fn corrected_invoice(
    original: &bss_ledger_sdk::EntryView,
    invoice_id: &str,
    items: &[crate::domain::invoice::builder::InvoiceItem],
    mapped: &[crate::domain::invoice::mapping::MappedLine],
) -> bss_ledger_sdk::PostEntry {
    // Reconstruct a minimal PostedInvoice over the original's dims. The payer +
    // seller come from the original AR line / entry tenant; tax is empty (the
    // correction re-books the recognized revenue split — a tax correction is a
    // separate reversal).
    let payer = original
        .lines
        .iter()
        .find(|l| l.account_class == bss_ledger_sdk::AccountClass::Ar)
        .map_or(original.tenant_id, |l| l.payer_tenant_id);
    let inv = crate::domain::invoice::builder::PostedInvoice {
        invoice_id: invoice_id.to_owned(),
        payer_tenant_id: payer,
        resource_tenant_id: None,
        seller_tenant_id: original.tenant_id,
        effective_at: original.effective_at,
        due_date: None,
        period_id: original.period_id.clone(),
        items: items.to_vec(),
        tax: Vec::new(),
        posted_by_actor_id: original.posted_by_actor_id,
        correlation_id: original.correlation_id,
    };
    build_invoice_entry(&inv, mapped)
}

async fn get_entry(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<EntryDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Tenant from the context; the client's PDP `In` scope is the SQL-level BOLA
    // filter (a foreign entry resolves to None ⇒ 404).
    let tenant_id = ctx.subject_tenant_id();
    let entry = state
        .client
        .get_entry(&ctx, tenant_id, entry_id)
        .await?
        .ok_or_else(|| entry_not_found(entry_id))?;
    Ok(Json(EntryDto::from(entry)))
}

/// `PATCH /journal-entries/{entryId}/annotation`: set the typed controlled
/// `description` note on the entry (or one of its lines).
///
/// Flow: authenticate → read the entry (404 `entry_not_found` if absent or
/// outside the caller's subtree — the scoped read is the SQL-level BOLA) → PEP
/// `(entry, annotate)` gate against the entry's OWN tenant → resolve the target
/// (`ENTRY` ⇒ the entry id + its period; `LINE` ⇒ a line of the entry) →
/// `AnnotationService::set` (pre-write PII screen, then one SERIALIZABLE upsert
/// + audit write). 204 No Content on success.
///
/// The `metadata-change` secured-audit record's `correlation_id` is the
/// annotated entry's OWN `journal_entry.correlation_id` (read back above), NOT a
/// request trace header — so the forensic record joins back to the entry it
/// annotated by construction (S-1 cross-trace), independent of any client header.
async fn set_entry_annotation(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(entry_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<EntryAnnotationRequestDto>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Cap the unbounded `description` / `reason` free text at the boundary (400).
    body.validate().map_err(CanonicalError::from)?;

    let caller_tenant = ctx.subject_tenant_id();
    let entry = state
        .client
        .get_entry(&ctx, caller_tenant, entry_id)
        .await?
        .ok_or_else(|| entry_not_found(entry_id))?;

    // (entry, annotate) PEP gate against the entry's OWN owning tenant.
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::ANNOTATE,
        Some(entry.tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let kind_literal = body.target_kind.as_deref().unwrap_or("ENTRY");
    let target = crate::infra::annotation::AnnotationTarget::parse(kind_literal)
        .map_err(CanonicalError::from)?;
    let target_id = match target {
        crate::infra::annotation::AnnotationTarget::Entry => entry.entry_id,
        crate::infra::annotation::AnnotationTarget::Line => {
            let line_id = body.target_line_id.ok_or_else(|| {
                crate::domain::error::DomainError::InvalidRequest(
                    "target_line_id is required when target_kind = LINE".to_owned(),
                )
            })?;
            if !entry.lines.iter().any(|l| l.line_id == line_id) {
                return Err(entry_not_found(line_id));
            }
            line_id
        }
    };

    let actor_ref = ctx.subject_id().to_string();
    state
        .annotation
        .set(
            &ctx,
            &scope,
            entry.tenant_id,
            target_id,
            entry.period_id.clone(),
            target,
            body.description,
            actor_ref,
            Some(body.reason),
            // S-1 cross-trace: anchor the audit record on the annotated entry's
            // OWN correlation_id, so the `metadata-change` record joins back to
            // `journal_entry` by construction (not by a client-supplied header).
            Some(entry.correlation_id),
        )
        .await
        .map_err(CanonicalError::from)?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /journal-lines`: seller from `$filter=tenant_id eq`, else the caller.
#[allow(clippy::implicit_hasher)]
async fn list_lines(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<LineDto>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    let page = state.client.list_lines(&ctx, tenant_id, &odata).await?;
    Ok(Json(Page {
        items: page.items.into_iter().map(LineDto::from).collect(),
        page_info: page.page_info,
    }))
}

/// `GET /journal-entries`: cursor-paginated list of journal entry HEADERS (R5).
///
/// Unlike `list_lines` (which goes through `state.client`, the client gating the
/// PEP internally), this calls `state.journal_repo` DIRECTLY, so the handler-layer
/// `(entry, read)` gate is the authority — mirroring `disputes::list_disputes` /
/// `refunds::list_refunds`. The returned scope is the SQL-level BOLA filter the
/// repo binds, so the page never contains a foreign-tenant header (no existence
/// leak). The `$filter` over `source_doc_type` / `source_business_id` / `period_id`
/// is additive over that scope (it never replaces it).
#[allow(clippy::implicit_hasher)]
async fn list_journal_entries(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<EntryHeaderView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    // (entry, read) PEP gate against the target tenant — the SAME action the by-id
    // entry read / journal-lines / balances run under. The returned scope is the
    // SQL-level BOLA filter the repo binds, so the page never contains a foreign-
    // tenant header (no existence leak), mirroring `disputes::list_disputes`.
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
        .journal_repo
        .as_ref()
        .ok_or_else(|| CanonicalError::internal("journal repository not configured").create())?
        .list_entries(&scope, tenant_id, &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.into_iter().map(EntryHeaderView::from).collect(),
        page_info: page.page_info,
    }))
}

/// `GET /balances`: seller from `$filter=tenant_id eq`, else the caller.
/// `valuation` stays a named lens (not a filter).
#[allow(clippy::implicit_hasher)]
async fn list_balances(
    Extension(state): Extension<Arc<ApiState>>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<BalanceDto>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params_allowing(&extras, &["valuation"])?;
    let tenant_id = list_seller_tenant(odata.filter.as_deref(), ctx.subject_tenant_id())?;
    let page = state.client.list_balances(&ctx, tenant_id, &odata).await?;
    Ok(Json(Page {
        items: page.items.into_iter().map(BalanceDto::from).collect(),
        page_info: page.page_info,
    }))
}

/// `GET /balances/ar-aging` query parameters. NOTE: ar-aging is a **computed
/// bucket aggregate** (a report), not a paginated row collection — it folds the
/// open per-invoice AR into days-past-due buckets per `(payer, currency)`. It
/// deliberately keeps plain `?tenant_id=&payer=` query params (no `OData`
/// `$filter` / `Page` envelope); the row-collection lists (`journal-lines` /
/// `balances` / `accounts`) are the `OData` surfaces.
#[derive(Debug, serde::Deserialize)]
struct ArAgingQuery {
    tenant_id: Option<Uuid>,
    payer: Option<Uuid>,
}

async fn ar_aging_handler(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<ArAgingQuery>,
) -> Result<Json<ArAgingDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id.unwrap_or_else(|| ctx.subject_tenant_id());
    let rows = state
        .client
        .list_ar_invoice_balances(&ctx, tenant_id, query.payer)
        .await?;
    // VHP-1853: bucket by the tenant's configured AR-aging thresholds (the gear
    // default when no policy row / a stub state). The policy read is scoped on
    // `ENTRY:read` — the same grant the AR balance read above already requires,
    // so this adds no new authorization surface.
    let thresholds = match &state.posting_policy {
        Some(repo) => {
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
            repo.read_effective_policy(&scope, tenant_id, OffsetDateTime::now_utc())
                .await
                .map_err(|e| {
                    CanonicalError::from(crate::domain::error::DomainError::Internal(format!(
                        "read posting policy: {e}"
                    )))
                })?
                .aging_thresholds
        }
        None => AgingThresholds::default(),
    };
    let buckets = ar_aging(&rows, to_naive_date(OffsetDateTime::now_utc()), &thresholds);
    Ok(Json(ArAgingDto::from(buckets)))
}

#[cfg(test)]
#[path = "journal_entries_tests.rs"]
mod metadata_tests;
