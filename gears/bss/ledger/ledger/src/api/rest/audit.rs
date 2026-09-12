//! Axum handlers + router for the ledger's audit-retrieval REST surface (Slice
//! 6 Phase 2 Group 2C, architecture AC #8). Three reads under
//! `/bss-ledger/v1/ledger/audit`, all gated by PEP `(entry, audit_read)`:
//!
//! - `GET …/journal-entries/{entryId}` — the who/when/source/correlation dims of
//!   one posted entry (tenant-scoped to the caller's home tenant; 404 if absent
//!   or outside the caller's scope — no existence leak).
//! - `GET …/documents/{sourceDocType}/{sourceBusinessId}/history` — every
//!   `journal_entry` for that document plus any reversal / mapping-correction
//!   that links to one of them, ordered by `created_seq` (tenant-scoped).
//! - `GET …/tamper-status?targetScope=&reasonCode=` (+ `X-Investigation-Reason`
//!   header) — a scope's freeze rows + a derived `verified` flag. This endpoint
//!   carries the **cross-tenant elevation contract**: the read scope is resolved
//!   via [`CrossTenantGateway::resolve_read_scope`] INSIDE the transaction, so a
//!   cross-tenant read writes its `cross-tenant-access` forensic record in the
//!   SAME transaction as the read (or both roll back together).
//!
//! Routes register through `OperationBuilder` so `/openapi.json` lists each
//! operation; the router is merged in `register_rest` alongside the journal /
//! provisioning routers.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit_db::DbError;
use toolkit_db::secure::{AccessScope, DbTx, TxConfig};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::dto::{
    AuditEntryDto, AuditPackExportDto, AuditPackRequestDto, DocumentHistoryDto, ErasureRequestDto,
    ReidentifyRequestDto, ReidentifyResponseDto, TamperStatusDto,
};
use crate::api::rest::error::{authz_error_to_canonical, entry_not_found, pack_export_not_found};
use crate::infra::audit::retrieval::AuditRetrievalReader;
use crate::infra::authz::cross_tenant::{CrossTenantGateway, TargetScope};
use crate::infra::inquiry::AuditPackExporter;
use crate::infra::pii::ErasureService;
use crate::infra::storage::entity::audit_pack_export;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the audit-retrieval operations.
const TAG: &str = "BSS Ledger Audit";

/// The HTTP header carrying the forensic investigation reason for a cross-tenant
/// tamper-status read (free text; the machine `reasonCode` is a query param).
const INVESTIGATION_REASON_HEADER: &str = "X-Investigation-Reason";
/// Request header carrying the caller's correlation / trace id (a UUID). Threaded
/// into the secured-audit record's `correlation_id` (S-1) on the SCOPE-level
/// forensic paths (tamper-status, erasure, reidentify, audit-pack), which act on
/// a whole tenant / payer / many entries and so have no single `journal_entry` to
/// anchor on — here `correlation_id` is the request trace id, grouping all
/// forensic events of one investigation request, and makes
/// `idx_secured_audit_correlation` usable. (The annotation `metadata-change`
/// record does NOT use this header: it anchors on the annotated entry's own
/// `journal_entry.correlation_id`, so it joins back to that entry by
/// construction — see `journal_entries::set_entry_annotation`.) Optional: absent
/// or unparseable → `None` (the record is still written, just without a trace
/// key).
const CORRELATION_ID_HEADER: &str = "X-Correlation-Id";

/// Shared per-request state for the audit-retrieval routes. Built once at
/// `init()` and shared via `Extension<Arc<ApiState>>`. Carries the scoped
/// reader (entry / document-history / tamper-status reads) and the cross-tenant
/// elevation gateway (the tamper-status forensic gate).
#[derive(Clone)]
pub struct ApiState {
    /// Scoped audit reader (over the same `DBProvider` the gear uses).
    pub reader: AuditRetrievalReader,
    /// Cross-tenant elevation gateway (writes the `cross-tenant-access` forensic
    /// record in the tamper-status / audit-pack transaction).
    pub gateway: CrossTenantGateway,
    /// Audit-pack CSV exporter (Group 4A): the scoped filtered-inquiry read +
    /// the by-hand RFC-4180 CSV encoder, over the same `DBProvider`.
    pub exporter: AuditPackExporter,
    /// PII erasure + forensic re-identification service (Group 3A): tombstones a
    /// payer's PII map + records the `erasure` / `re-identification` secured-audit
    /// record over the same `DBProvider` the gear uses.
    pub erasure: ErasureService,
    /// Database provider the erasure / re-identify services open their own
    /// `SERIALIZABLE` transaction over (the audit reader carries one too, but the
    /// erasure path holds its own to keep the dependency explicit).
    pub db: toolkit_db::DBProvider<DbError>,
}

/// Build the Axum router for the audit-retrieval surface and register every
/// operation with the supplied `OpenAPI` registry. `state` is attached via an
/// `Extension` layer at the end (the registry sees the routes first).
#[allow(
    clippy::too_many_lines,
    reason = "flat operation-by-operation router registration — each endpoint is a self-contained OperationBuilder chain; splitting would obscure the surface"
)]
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::get("/bss-ledger/v1/ledger/audit/journal-entries/{entryId}")
        .operation_id("bss_ledger.audit_entry")
        .summary("Read the audit dims of a posted entry")
        .description(
            "Returns the who/when/source/correlation dims of the journal entry \
             `{entryId}` for the caller's own tenant (AC #8). An entry outside \
             the caller's authorized scope yields 404 (no existence leak). PEP \
             gate `(entry, audit_read)`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(audit_entry)
        .json_response_with_schema::<AuditEntryDto>(
            openapi,
            StatusCode::OK,
            "The entry's audit dims",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(
        "/bss-ledger/v1/ledger/audit/documents/{sourceDocType}/{sourceBusinessId}/history",
    )
    .operation_id("bss_ledger.audit_document_history")
    .summary("Read a source document's full posting history")
    .description(
        "Returns every journal entry for `(sourceDocType, sourceBusinessId)` in \
         the caller's own tenant plus any reversal / mapping-correction that \
         links to one of them, ordered by `created_seq`. PEP gate `(entry, \
         audit_read)`; tenant-scoped (SQL-level BOLA).",
    )
    .tag(TAG)
    .authenticated()
    .no_license_required()
    .handler(audit_document_history)
    .json_response_with_schema::<DocumentHistoryDto>(
        openapi,
        StatusCode::OK,
        "The document's posting history",
    )
    .error_401(openapi)
    .error_403(openapi)
    .error_500(openapi)
    .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/ledger/audit/tamper-status")
        .operation_id("bss_ledger.audit_tamper_status")
        .summary("Read a scope's tamper-status (cross-tenant elevation-gated)")
        .description(
            "Returns the scope-freeze rows + a derived `verified` flag for the \
             resolved scope. Routine (no `targetScope`, or the caller's own \
             tenant) reads the caller's own tenant. A cross-tenant `targetScope` \
             is forensic-gated: it requires the `(entry, audit_read)` role, an \
             `X-Investigation-Reason` header, and a `reasonCode` query, and \
             writes a `cross-tenant-access` secured-audit record in the SAME \
             transaction as the read. A reason-less or role-less cross-tenant \
             request is refused (400 MISSING_INVESTIGATION_REASON / 403 \
             CROSS_TENANT_ACCESS_DENIED) before any foreign row is read.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "target_scope",
            false,
            "Target tenant to open (defaults to the caller's own; a different \
             tenant triggers the forensic elevation gate)",
        )
        .query_param(
            "reason_code",
            false,
            "Machine-readable investigation reason code (required for a \
             cross-tenant read)",
        )
        .handler(audit_tamper_status)
        .json_response_with_schema::<TamperStatusDto>(
            openapi,
            StatusCode::OK,
            "The resolved scope's tamper-status",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/ledger/audit/erasure")
        .operation_id("bss_ledger.audit_erasure")
        .summary("Erase a payer's PII (GDPR right-to-erasure)")
        .description(
            "Tombstones the `payer_pii_map` row for `payer_tenant_id` \
             (`erased = true`) and records ONE `erasure` secured-audit record, in \
             one SERIALIZABLE transaction. NO journal row is touched — the \
             financial truth and its tamper-evidence chain stay byte-identical. \
             Idempotent: re-erasing an already-tombstoned payer is a no-op that \
             still records the audit event. PEP gate `(entry, erase)` \
             (DPO-scoped). A cross-tenant `target_scope` opens a different \
             tenant's PII map (§5): it requires a reason and an `(entry, erase)` \
             authorization for the target, else 400 MISSING_INVESTIGATION_REASON \
             / 403 CROSS_TENANT_ACCESS_DENIED.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ErasureRequestDto>(
            openapi,
            "The payer to erase + optional cross-tenant target_scope (the reason \
             is the X-Investigation-Reason header).",
        )
        .handler(audit_erasure)
        .no_content_response(StatusCode::NO_CONTENT, "The PII was erased (no body)")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/ledger/audit/reidentify")
        .operation_id("bss_ledger.audit_reidentify")
        .summary("Re-identify a payer's PII reference (forensic)")
        .description(
            "Returns the opaque `pii_ref` for `payer_tenant_id` (even of a \
             tombstoned payer — the documented investigator path) AFTER recording \
             ONE `re-identification` secured-audit record, in one SERIALIZABLE \
             transaction. Forensic-gated: both a `reason` and a `reason_code` are \
             required, else 400 MISSING_INVESTIGATION_REASON (before any read or \
             write). An absent map row is 404. PEP gate `(entry, reidentify)`. A \
             cross-tenant `target_scope` re-identifies against a different \
             tenant's PII map (§5), requiring an `(entry, reidentify)` \
             authorization for the target, else 403 CROSS_TENANT_ACCESS_DENIED.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<ReidentifyRequestDto>(
            openapi,
            "The payer to re-identify + reason_code + optional cross-tenant \
             target_scope (the free-text reason is the X-Investigation-Reason \
             header).",
        )
        .handler(audit_reidentify)
        .json_response_with_schema::<ReidentifyResponseDto>(
            openapi,
            StatusCode::OK,
            "The recovered PII reference",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/ledger/audit/packs")
        .operation_id("bss_ledger.audit_pack")
        .summary("Export a filtered audit pack as CSV (cross-tenant elevation-gated)")
        .description(
            "Filters posted entries by the supplied axes (payer / period / \
             account class / legal entity) and returns a CSV audit pack: a \
             header row plus one row per (entry, line) with the full linkage \
             columns, RFC-4180 quoted. Routine (no `target_scope`, or the \
             caller's own tenant) reads the caller's own tenant. A cross-tenant \
             `target_scope` is forensic-gated: it requires the `(entry, \
             audit_read)` role, an `X-Investigation-Reason` header, and a \
             `reason_code`, and writes a `cross-tenant-access` secured-audit \
             record in the SAME transaction as the read. A reason-less or \
             role-less cross-tenant request is refused (400 \
             MISSING_INVESTIGATION_REASON / 403 CROSS_TENANT_ACCESS_DENIED) \
             before any foreign row is read. PEP gate `(entry, audit_read)`. \
             Responds 202 Accepted with a `Location` to \
             `GET …/audit/packs/{exportId}` (the async export contract, §5/§10); \
             poll that resource for the job status and, once `succeeded`, the \
             materialized CSV.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<AuditPackRequestDto>(
            openapi,
            "The inquiry filter + optional cross-tenant target + reason_code.",
        )
        .handler(audit_pack)
        .json_response_with_schema::<AuditPackExportDto>(
            openapi,
            StatusCode::ACCEPTED,
            "Export accepted — poll the Location for the job status + CSV",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/ledger/audit/packs/{exportId}")
        .operation_id("bss_ledger.audit_pack_get")
        .summary("Poll an audit-pack export job")
        .description(
            "Returns the audit-pack export `{exportId}` in the caller's own \
             tenant: the job `status` and, once `succeeded`, the materialized CSV \
             + data-row count. An export outside the caller's authorized scope \
             yields 404 (no existence leak). PEP gate `(entry, audit_read)`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(audit_pack_get)
        .json_response_with_schema::<AuditPackExportDto>(
            openapi,
            StatusCode::OK,
            "The export job status (+ CSV when succeeded)",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

/// PEP `(entry, audit_read)` gate against the caller's HOME tenant, returning
/// the caller's compiled scope. The returned scope is the SQL-level BOLA filter
/// for the per-row audit reads. A deny maps to 403; an unreachable PDP to 503.
async fn audit_read_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<toolkit_security::AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::AUDIT_READ,
        // Reads pass `owner_tenant_id = None`: the PDP derives the scope from the
        // subject + role, and the returned scope is the SQL filter.
        None,
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// Authorize a cross-tenant elevation against the **target** tenant for `action`
/// (`audit_read` for packs/tamper-status, `erase` / `reidentify` for the PII
/// surfaces).
///
/// The routine home-tenant PEP gate only proves the caller may act on its OWN
/// tenant — it never validates the caller-supplied `targetScope`. Opening a
/// different tenant therefore runs a second, target-anchored PEP decision
/// (`owner_tenant_id = Some(target)`), reusing the write-path target-membership
/// assertion in [`crate::authz::access_scope`] (a target outside the caller's
/// authorized scope is denied there).
///
/// Returns `true` for the routine path (no target, or the target is the home
/// tenant) and for an authorized cross-tenant target. A PDP *deny* yields
/// `Ok(false)` — the caller then maps it to `CROSS_TENANT_ACCESS_DENIED` (403).
/// A PDP *unavailable* propagates as 503 (fail-closed: the elevation never
/// proceeds on an unreachable authority).
///
/// # Errors
/// [`CanonicalError`] (503) when the PDP is unreachable.
async fn cross_tenant_role_authorized(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    home_tenant: Uuid,
    target: Option<TargetScope>,
    action: &str,
) -> Result<bool, CanonicalError> {
    let Some(target) = target.filter(|t| t.tenant_id != home_tenant) else {
        return Ok(true);
    };
    match crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::ENTRY,
        action,
        // owner_tenant_id = the target tenant being opened: `access_scope`
        // denies a target outside the caller's compiled scope.
        Some(target.tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    {
        Ok(_) => Ok(true),
        Err(crate::authz::AuthzError::Denied(_)) => Ok(false),
        Err(e @ crate::authz::AuthzError::Unavailable(_)) => Err(authz_error_to_canonical(e)),
    }
}

/// The free-text investigation reason for a cross-tenant elevation — ALWAYS the
/// `X-Investigation-Reason` request header (§5), the single source of truth for
/// the reason written to the forensic record, across all four cross-tenant
/// endpoints (packs / tamper-status / erasure / reidentify). The machine
/// `reasonCode` travels separately (request body / query param).
fn investigation_reason(headers: &HeaderMap) -> Option<String> {
    headers
        .get(INVESTIGATION_REASON_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// The caller's correlation / trace id from the `X-Correlation-Id` request header
/// (S-1), parsed as a UUID. `None` when the header is absent or not a valid UUID —
/// the forensic record is still written, just without the trace key. Used by the
/// SCOPE-level forensic write paths in this module (tamper-status, erasure,
/// reidentify, audit-pack), which have no single `journal_entry` to anchor on. The
/// annotation path does NOT use this — it anchors on the annotated entry's own
/// `correlation_id` instead.
fn correlation_id_header(headers: &HeaderMap) -> Option<Uuid> {
    headers
        .get(CORRELATION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s.trim()).ok())
}

async fn audit_entry(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(entry_id): Path<Uuid>,
) -> Result<Json<AuditEntryDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = audit_read_scope(&enforcer, &ctx).await?;
    let tenant_id = ctx.subject_tenant_id();
    let record = state
        .reader
        .audit_entry(&scope, tenant_id, entry_id)
        .await
        .map_err(repo_to_canonical)?
        .ok_or_else(|| entry_not_found(entry_id))?;
    Ok(Json(AuditEntryDto::from(record)))
}

async fn audit_document_history(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path((source_doc_type, source_business_id)): Path<(String, String)>,
) -> Result<Json<DocumentHistoryDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = audit_read_scope(&enforcer, &ctx).await?;
    let tenant_id = ctx.subject_tenant_id();
    let records = state
        .reader
        .document_history(&scope, tenant_id, &source_doc_type, &source_business_id)
        .await
        .map_err(repo_to_canonical)?;
    Ok(Json(DocumentHistoryDto {
        entries: records.into_iter().map(AuditEntryDto::from).collect(),
    }))
}

/// `GET …/tamper-status` query params (the `X-Investigation-Reason` reason is a
/// header, not a query param). `target_scope` is the tenant to open; absent or
/// equal to the caller's own ⇒ routine.
#[derive(Debug, serde::Deserialize)]
struct TamperStatusQuery {
    target_scope: Option<Uuid>,
    reason_code: Option<String>,
}

async fn audit_tamper_status(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
    Query(query): Query<TamperStatusQuery>,
) -> Result<Json<TamperStatusDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // PEP `(entry, audit_read)` gate against the caller's HOME tenant (a deny is a
    // 403 here, before any read).
    let _home_scope = audit_read_scope(&enforcer, &ctx).await?;
    let home_tenant = ctx.subject_tenant_id();

    // The investigation reason is the `X-Investigation-Reason` request header
    // (read via the `HeaderMap` extractor — the toolkit `OperationBuilder` has no
    // header-param builder, so the header is read directly in the handler).
    let reason = investigation_reason(&headers);

    let target = query
        .target_scope
        .map(|tenant_id| TargetScope { tenant_id });
    // Cross-tenant elevation MUST authorize the caller for the TARGET tenant, not
    // just its own home tenant. A PDP deny becomes `CROSS_TENANT_ACCESS_DENIED`
    // (403) inside the gateway; the routine (home) path stays `true`.
    let role_authorized = cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        home_tenant,
        target,
        crate::authz::actions::AUDIT_READ,
    )
    .await?;
    let reason_code = query.reason_code.clone();
    let actor_ref = ctx.subject_id().to_string();
    let correlation_id = correlation_id_header(&headers);

    // The read scope is resolved + the forensic record written INSIDE one
    // transaction with the tamper-status read, so a cross-tenant read's
    // `cross-tenant-access` record and the read commit (or roll back) together.
    let gateway = state.gateway.clone();
    let reader = state.reader.clone();
    let result: Result<crate::infra::audit::retrieval::TamperStatusRecord, DbError> = reader
        .db()
        .db()
        .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
            let gateway = gateway.clone();
            let reader = reader.clone();
            let reason = reason.clone();
            let reason_code = reason_code.clone();
            let actor_ref = actor_ref.clone();
            Box::pin(async move {
                tamper_status_in_txn(
                    txn,
                    &gateway,
                    &reader,
                    home_tenant,
                    target,
                    role_authorized,
                    actor_ref.as_str(),
                    reason.as_deref(),
                    reason_code.as_deref(),
                    correlation_id,
                )
                .await
            })
        })
        .await;

    let record = result.map_err(decode_audit_error)?;
    Ok(Json(TamperStatusDto::from(record)))
}

/// `POST …/audit/erasure` — erase a payer's PII map. PEP `(entry, erase)` gates
/// the caller's capability against their own tenant. A cross-tenant
/// `target_scope` (§5) erases a DIFFERENT tenant's PII map: it requires a reason
/// (else `MISSING_INVESTIGATION_REASON`) and an `(entry, erase)` authorization
/// for the target (else `CROSS_TENANT_ACCESS_DENIED`). The `erasure`
/// secured-audit record is the forensic trail; it is written onto the actor's
/// HOME tenant chain (the map tombstone is scoped to the target tenant), the
/// same split [`crate::infra::authz::cross_tenant::CrossTenantGateway`] uses.
/// The free-text `reason` comes from the `X-Investigation-Reason` header. 204 on
/// success.
async fn audit_erasure(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
    CanonicalJson(body): CanonicalJson<ErasureRequestDto>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let caller_tenant = ctx.subject_tenant_id();

    // (entry, erase) PEP capability gate against the caller's OWN tenant (a write
    // path: owner_tenant_id = Some, require_constraints). Yields the routine
    // same-tenant scope.
    let home_scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::ERASE,
        Some(caller_tenant),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // The free-text reason is the `X-Investigation-Reason` header (§5), the same
    // source as packs / tamper-status / reidentify.
    let reason = investigation_reason(&headers).unwrap_or_default();
    let actor_ref = ctx.subject_id().to_string();

    // Resolve the tenant whose PII map is erased: the caller's own (routine), or
    // a forensic-gated cross-tenant target (§5). The cross-tenant elevation
    // decision (routine → deny-on-role → missing-reason → target scope) is the
    // shared `resolve_action_scope` contract, so this path can never drift from
    // re-identify or the read gateway's branch order.
    let target = body.target_scope.map(|tenant_id| TargetScope { tenant_id });
    let role_authorized = cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        caller_tenant,
        target,
        crate::authz::actions::ERASE,
    )
    .await?;
    let (data_tenant, data_scope) = crate::infra::authz::cross_tenant::resolve_action_scope(
        caller_tenant,
        &home_scope,
        target,
        role_authorized,
        Some(reason.as_str()),
    )
    .map_err(CanonicalError::from)?;

    // The `erasure` forensic record is written onto the actor's HOME chain
    // (`caller_tenant`/`home_scope`); the map tombstone is scoped to the target
    // (`data_tenant`/`data_scope`). For a routine erasure the two coincide.
    state
        .erasure
        .erase(
            &state.db,
            &ctx,
            &home_scope,
            caller_tenant,
            &data_scope,
            data_tenant,
            body.payer_tenant_id,
            actor_ref,
            reason,
            correlation_id_header(&headers),
        )
        .await
        .map_err(CanonicalError::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST …/audit/reidentify` — re-identify a payer's PII ref. PEP `(entry,
/// reidentify)` gates the caller's capability against their own tenant; both a
/// `reason` and a `reason_code` are always required (the forensic gate inside
/// the service → `MISSING_INVESTIGATION_REASON`). A cross-tenant `target_scope`
/// (§5) re-identifies against a DIFFERENT tenant's PII map: it requires an
/// `(entry, reidentify)` authorization for the target (else
/// `CROSS_TENANT_ACCESS_DENIED`). The `re-identification` secured-audit record
/// is the forensic trail; it is written onto the actor's HOME tenant chain (the
/// map read is scoped to the target tenant), the same split
/// [`crate::infra::authz::cross_tenant::CrossTenantGateway`] uses. Absent map row
/// → 404. 200 `{ pii_ref }`.
async fn audit_reidentify(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
    CanonicalJson(body): CanonicalJson<ReidentifyRequestDto>,
) -> Result<Json<ReidentifyResponseDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Cap the machine `reason_code` at the boundary (400) before any work.
    body.validate().map_err(CanonicalError::from)?;
    let caller_tenant = ctx.subject_tenant_id();
    // The free-text reason is the `X-Investigation-Reason` header (§5), the same
    // source as the other three cross-tenant endpoints; `reason_code` is in the
    // body. The reason/reason_code forensic gate runs inside the service.
    let reason = investigation_reason(&headers).unwrap_or_default();

    // (entry, reidentify) PEP capability gate against the caller's OWN tenant.
    let home_scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::REIDENTIFY,
        Some(caller_tenant),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Resolve the tenant to re-identify against: the caller's own (routine), or a
    // forensic-gated cross-tenant target (§5), via the shared
    // `resolve_action_scope` contract (same branch order as erasure / the read
    // gateway). The reason/reason_code forensic gate is also enforced inside the
    // service on both paths.
    let target = body.target_scope.map(|tenant_id| TargetScope { tenant_id });
    let role_authorized = cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        caller_tenant,
        target,
        crate::authz::actions::REIDENTIFY,
    )
    .await?;
    let (data_tenant, data_scope) = crate::infra::authz::cross_tenant::resolve_action_scope(
        caller_tenant,
        &home_scope,
        target,
        role_authorized,
        Some(reason.as_str()),
    )
    .map_err(CanonicalError::from)?;

    // The `re-identification` forensic record is written onto the actor's HOME
    // chain (`caller_tenant`/`home_scope`); the map read is scoped to the target
    // (`data_tenant`/`data_scope`). For a routine re-identify the two coincide.
    let actor_ref = ctx.subject_id().to_string();
    let pii_ref = state
        .erasure
        .reidentify(
            &state.db,
            &ctx,
            &home_scope,
            caller_tenant,
            &data_scope,
            data_tenant,
            body.payer_tenant_id,
            actor_ref,
            reason,
            body.reason_code,
            correlation_id_header(&headers),
        )
        .await
        .map_err(CanonicalError::from)?;
    Ok(Json(ReidentifyResponseDto { pii_ref }))
}

/// `POST …/audit/packs` — export a filtered audit pack as CSV. PEP `(entry,
/// audit_read)` gate (a passing gate IS the role authorization for the
/// cross-tenant elevation). The read scope is resolved + the forensic record
/// written INSIDE one transaction with the export read, so a cross-tenant pack's
/// `cross-tenant-access` record and the read commit (or roll back) together.
/// Async export contract (§5/§10): responds `202 Accepted` + a `Location`
/// header pointing at `…/audit/packs/{exportId}`, with an `AuditPackExportDto`
/// summary body (`csv` is `None` here). The client polls that `Location` with
/// `GET …/audit/packs/{exportId}` to retrieve the materialized CSV once the
/// export is `succeeded`. The build is synchronous for now (MVP), so the row is
/// already `succeeded` and the first poll returns the CSV — but the wire shape
/// is the durable async contract a future background worker slots behind.
async fn audit_pack(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    headers: HeaderMap,
    CanonicalJson(body): CanonicalJson<AuditPackRequestDto>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Cap the machine `reason_code` at the boundary (400) before any work.
    body.validate().map_err(CanonicalError::from)?;
    // PEP `(entry, audit_read)` gate against the caller's HOME tenant (a deny is a
    // 403 here, before any read).
    let _home_scope = audit_read_scope(&enforcer, &ctx).await?;
    let home_tenant = ctx.subject_tenant_id();

    // The free-text investigation reason is the `X-Investigation-Reason` header
    // (the machine `reason_code` is in the body — same split as tamper-status).
    let reason = investigation_reason(&headers);

    let target = body.target_scope.map(|tenant_id| TargetScope { tenant_id });
    // Cross-tenant elevation MUST authorize the caller for the TARGET tenant, not
    // just its own home tenant. A PDP deny becomes `CROSS_TENANT_ACCESS_DENIED`
    // (403) inside the gateway; the routine (home) path stays `true`.
    let role_authorized = cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        home_tenant,
        target,
        crate::authz::actions::AUDIT_READ,
    )
    .await?;
    let reason_code = body.reason_code.clone();
    let filter: crate::infra::inquiry::InquiryFilter = body.filter.into();
    let actor_ref = ctx.subject_id().to_string();
    let correlation_id = correlation_id_header(&headers);

    let gateway = state.gateway.clone();
    let exporter = state.exporter.clone();
    let result: Result<audit_pack_export::Model, DbError> = state
        .db
        .db()
        .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
            let gateway = gateway.clone();
            let exporter = exporter.clone();
            let reason = reason.clone();
            let reason_code = reason_code.clone();
            let actor_ref = actor_ref.clone();
            let filter = filter.clone();
            Box::pin(async move {
                audit_pack_in_txn(
                    txn,
                    &gateway,
                    &exporter,
                    home_tenant,
                    target,
                    role_authorized,
                    actor_ref.as_str(),
                    reason.as_deref(),
                    reason_code.as_deref(),
                    correlation_id,
                    &filter,
                )
                .await
            })
        })
        .await;

    let model = result.map_err(decode_audit_error)?;

    // Async contract (§5/§10): 202 + Location to the export resource. The build
    // is synchronous for now (MVP, contract-only), so the row is already
    // `succeeded` and the polled GET returns the CSV immediately — but the wire
    // shape is the durable async contract a future background worker slots behind
    // without a change.
    let location = format!("/bss-ledger/v1/ledger/audit/packs/{}", model.export_id);
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(AuditPackExportDto::summary(&model)),
    )
        .into_response())
}

/// `GET …/audit/packs/{exportId}` — poll an audit-pack export job. Reads the
/// export row in the caller's own tenant (PEP `(entry, audit_read)` gate; SQL
/// BOLA via the scoped read), returning the job status and, once `succeeded`,
/// the materialized CSV. The forensic cross-tenant-access record (if any) was
/// written at create time, so this poll is a routine home-tenant read. 404 if
/// absent or scoped-out (no existence leak).
async fn audit_pack_get(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(export_id): Path<Uuid>,
) -> Result<Json<AuditPackExportDto>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = audit_read_scope(&enforcer, &ctx).await?;
    let tenant = ctx.subject_tenant_id();
    let model = state
        .exporter
        .find_export(&scope, tenant, export_id)
        .await
        .map_err(repo_to_canonical)?
        .ok_or_else(|| pack_export_not_found(export_id))?;
    Ok(Json(AuditPackExportDto::from(model)))
}

/// In-transaction body for the audit-pack elevation: resolve the read scope
/// (writing the forensic record on the cross-tenant path), then build the CSV
/// pack under it in the SAME transaction.
#[allow(
    clippy::too_many_arguments,
    reason = "the full elevation contract (home/target/actor/reason/reason_code/correlation) + the filter + the export, in one txn"
)]
async fn audit_pack_in_txn(
    txn: &DbTx<'_>,
    gateway: &CrossTenantGateway,
    exporter: &AuditPackExporter,
    home_tenant: Uuid,
    target: Option<TargetScope>,
    role_authorized: bool,
    actor_ref: &str,
    reason: Option<&str>,
    reason_code: Option<&str>,
    correlation_id: Option<Uuid>,
    filter: &crate::infra::inquiry::InquiryFilter,
) -> Result<audit_pack_export::Model, DbError> {
    // `role_authorized` is the target-anchored PEP decision computed at the REST
    // seam (`cross_tenant_role_authorized`); `false` makes the gateway reject the
    // elevation with `CROSS_TENANT_ACCESS_DENIED` before any foreign read.
    let read_scope = gateway
        .resolve_read_scope(
            txn,
            home_tenant,
            target,
            role_authorized,
            actor_ref,
            reason,
            reason_code,
            correlation_id,
        )
        .await?;
    let (csv, row_count) = exporter
        .export_csv_in_txn(txn, &read_scope, filter)
        .await
        .map_err(|e| crate::infra::posting::service::infra(format!("audit-pack export: {e}")))?;

    // Persist the materialized pack as an export row owned by the HOME tenant
    // (the same tenant the forensic record is written under), so the requester
    // polls it under its own scope. Born `succeeded` — the build is synchronous
    // (MVP, contract-only). The export row + the forensic record + the foreign
    // read all commit (or roll back) together in this transaction.
    // Surface an overflow as an error rather than silently saturating the
    // persisted count (a saturated `i64::MAX` would be a believable lie).
    let row_count = i64::try_from(row_count).map_err(|_| {
        crate::infra::posting::service::infra(format!(
            "audit-pack row_count {row_count} exceeds i64::MAX"
        ))
    })?;
    let now = OffsetDateTime::now_utc();
    let model = audit_pack_export::Model {
        export_id: Uuid::now_v7(),
        tenant_id: home_tenant,
        target_tenant_id: target.map_or(home_tenant, |t| t.tenant_id),
        status: "succeeded".to_owned(),
        reason_code: reason_code.map(str::to_owned),
        actor_ref: actor_ref.to_owned(),
        csv: Some(csv.into_bytes()),
        row_count,
        error_detail: None,
        created_at_utc: now,
        completed_at_utc: Some(now),
    };
    exporter
        .insert_export_in_txn(txn, &AccessScope::for_tenant(home_tenant), &model)
        .await
        .map_err(|e| crate::infra::posting::service::infra(format!("audit-pack persist: {e}")))?;
    Ok(model)
}

/// In-transaction body for the tamper-status elevation: resolve the read scope
/// (writing the forensic record on the cross-tenant path), then read the
/// resolved scope's tamper-status under it.
#[allow(
    clippy::too_many_arguments,
    reason = "the full elevation contract (home/target/actor/reason/reason_code/correlation) + the read, in one txn"
)]
async fn tamper_status_in_txn(
    txn: &DbTx<'_>,
    gateway: &CrossTenantGateway,
    reader: &AuditRetrievalReader,
    home_tenant: Uuid,
    target: Option<TargetScope>,
    role_authorized: bool,
    actor_ref: &str,
    reason: Option<&str>,
    reason_code: Option<&str>,
    correlation_id: Option<Uuid>,
) -> Result<crate::infra::audit::retrieval::TamperStatusRecord, DbError> {
    // `role_authorized` is the target-anchored PEP decision computed at the REST
    // seam (`cross_tenant_role_authorized`); `false` makes the gateway reject the
    // elevation with `CROSS_TENANT_ACCESS_DENIED` before any foreign read.
    let read_scope = gateway
        .resolve_read_scope(
            txn,
            home_tenant,
            target,
            role_authorized,
            actor_ref,
            reason,
            reason_code,
            correlation_id,
        )
        .await?;

    // The tenant the resolved scope authorizes: the target on the cross-tenant
    // path, the home tenant on the routine path.
    let read_tenant = target.map_or(home_tenant, |t| t.tenant_id);
    reader
        .tamper_status_in_txn(txn, &read_scope, read_tenant)
        .await
        .map_err(|e| crate::infra::posting::service::infra(format!("tamper-status read: {e}")))
}

/// Retry-extractor for the `SERIALIZABLE` tamper-status txn (mirrors
/// `infra::posting::service::as_db_err`): only a genuine `DbErr` is retryable;
/// the business-rejection sentinel is a non-retryable `DbErr::Custom`.
fn as_db_err(e: &DbError) -> Option<&sea_orm::DbErr> {
    match e {
        DbError::Sea(db_err) => Some(db_err),
        _ => None,
    }
}

/// Decode a `DbError` from the tamper-status txn into a `CanonicalError`: a
/// sentinel-tagged business rejection (`CrossTenantAccessDenied` /
/// `MissingInvestigationReason`) projects through the canonical ladder; any
/// other `DbError` is an infrastructure fault (500).
#[allow(
    clippy::needless_pass_by_value,
    reason = "error-map target for .map_err; the value is mapped, not retained"
)]
fn decode_audit_error(db_err: DbError) -> CanonicalError {
    CanonicalError::from(crate::infra::posting::service::decode_business_error(
        &db_err,
    ))
}

/// Map a [`RepoError`](crate::domain::model::RepoError) from a non-txn audit
/// read to a `CanonicalError` (an infrastructure fault ⇒ 500; the diagnostic
/// stays server-side).
#[allow(
    clippy::needless_pass_by_value,
    reason = "error-map target for .map_err"
)]
fn repo_to_canonical(e: crate::domain::model::RepoError) -> CanonicalError {
    CanonicalError::from(crate::domain::error::DomainError::Internal(e.to_string()))
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod audit_tests;
