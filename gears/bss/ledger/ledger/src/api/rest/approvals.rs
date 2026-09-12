//! Axum handlers + router for the dual-control approval queue (VHP-1852, Group G).
//! All operations live under `/bss-ledger/v1/approvals`, tenant-scoped to the
//! caller's own tenant (no tenant in the path/body). The decision routes
//! (approve / reject / request-changes / get / list) gate on the `(entry, approve)`
//! PEP permission (`entry_approve.v1`); the preparer routes (resubmit / cancel /
//! comments) follow impl-design §6 — "the preparer's originating right" — resolving
//! the `(entry, read)` plane for a non-approver and enforcing `actor == prepared_by`
//! server-side (the `preparer ≠ approver` rule lives in [`ApprovalService`]). The
//! over-threshold mutations that CREATE pending approvals are the retrofit gates in
//! the journal-entry / credit / dispute surfaces.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::canonical_json::CanonicalJson;
use crate::api::rest::dto::DualControlPolicyView;
use crate::api::rest::error::authz_error_to_canonical;
use crate::domain::approval::intent::ApprovalIntent;
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::infra::approval::service::ApprovalService;
use crate::infra::storage::entity::{dual_control_approval, dual_control_comment};
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the approval operations.
const TAG: &str = "BSS Ledger Approvals";

/// Shared per-request state for the approval routes. Constructed once at `init()`
/// and shared via `Extension<Arc<ApiState>>`.
#[derive(Clone)]
pub struct ApiState {
    /// The dual-control lifecycle engine.
    pub service: Arc<ApprovalService>,
}

// ─── DTOs ────────────────────────────────────────────────────────────────────

/// A dual-control approval rendered for the queue / detail views.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalDto {
    pub approval_id: Uuid,
    pub kind: String,
    pub state: String,
    pub revision: i32,
    pub business_key: String,
    pub reason_code: String,
    pub prepared_by: Uuid,
    #[serde(with = "rfc3339")]
    pub prepared_at: OffsetDateTime,
    pub approved_by: Option<Uuid>,
    #[serde(default, with = "rfc3339::option")]
    pub decided_at: Option<OffsetDateTime>,
    // Without this `time`'s own `Serialize` runs and writes its default
    // human-readable form (`2026-09-08 12:00:00.0 +00:00:00`), not the RFC 3339
    // string the `date-time` schema promises and `prepared_at` beside it emits.
    #[serde(with = "rfc3339")]
    pub expires_at: OffsetDateTime,
    pub amount_usd_eq_minor: Option<i64>,
}

impl From<dual_control_approval::Model> for ApprovalDto {
    fn from(m: dual_control_approval::Model) -> Self {
        Self {
            approval_id: m.approval_id,
            kind: m.kind,
            state: m.state,
            revision: m.revision,
            business_key: m.business_key,
            reason_code: m.reason_code,
            prepared_by: m.prepared_by,
            prepared_at: m.prepared_at,
            approved_by: m.approved_by,
            decided_at: m.decided_at,
            expires_at: m.expires_at,
            amount_usd_eq_minor: m.amount_usd_eq_minor,
        }
    }
}

/// One comment / decision-reason on an approval's thread.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalCommentDto {
    pub comment_id: Uuid,
    pub revision: i32,
    pub author_actor: Uuid,
    pub body: String,
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<dual_control_comment::Model> for ApprovalCommentDto {
    fn from(m: dual_control_comment::Model) -> Self {
        Self {
            comment_id: m.comment_id,
            revision: m.revision,
            author_actor: m.author_actor,
            body: m.body,
            created_at: m.created_at,
        }
    }
}

/// The approval queue list.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalListResponse {
    pub approvals: Vec<ApprovalDto>,
}

/// An approval's comment thread.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ApprovalThreadResponse {
    pub comments: Vec<ApprovalCommentDto>,
}

/// A mandatory-reason decision body (`reject` / `request-changes`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ReasonRequest {
    pub reason: String,
}

/// A free comment / question body.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CommentRequest {
    pub body: String,
}

/// A resubmit body carrying the preparer's edited intent (the stored `intent`
/// jsonb shape). The kind cannot change; the approval returns to `PENDING`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ResubmitRequest {
    pub intent: serde_json::Value,
}

/// Optional `state` / `kind` filters for the queue list (query params).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ListQuery {
    pub state: Option<String>,
    pub kind: Option<String>,
}

/// A new dual-control threshold policy version to write (DC8). `effective_from`
/// defaults to now when omitted; out-of-range D2/A6/TTL is rejected (409, no clamp).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetDualControlPolicyRequest {
    pub d2_threshold_minor: i64,
    pub a6_backdating_biz_days: i32,
    pub pending_ttl_seconds: i64,
    #[serde(default, with = "rfc3339::option")]
    pub effective_from: Option<OffsetDateTime>,
}

/// The written dual-control policy version (the minted `version` + the thresholds
/// it carries; the resolver picks the latest `effective_from`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DualControlPolicyResponse {
    pub version: i64,
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    pub d2_threshold_minor: i64,
    pub a6_backdating_biz_days: i32,
    pub pending_ttl_seconds: i64,
}

/// The `?tenant_id=` query for `GET /dual-control-policy` (read-surface): the
/// tenant whose effective policy to read — the caller's own when omitted.
#[derive(Debug, serde::Deserialize)]
struct PolicyQuery {
    tenant_id: Option<Uuid>,
}

// ─── Router ──────────────────────────────────────────────────────────────────

/// Build the Axum router for the approval surface and register every operation
/// with the supplied `OpenAPI` registry.
#[allow(clippy::too_many_lines)] // one builder chain per operation; flat is clearer than helpers
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-ledger/v1/approvals/{approval_id}/approve")
        .operation_id("bss_ledger.approve_approval")
        .summary("Approve a pending dual-control approval")
        .description(
            "Approves the PENDING approval `{approval_id}`: executes the stored \
             mutation idempotently, then marks APPROVED with a same-transaction \
             decision audit. The approver MUST differ from the preparer \
             (409 SELF_APPROVAL_FORBIDDEN).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval being decided.")
        .handler(approve)
        .json_response_with_schema::<ApprovalDto>(openapi, StatusCode::OK, "The approved approval.")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/approvals/{approval_id}/reject")
        .operation_id("bss_ledger.reject_approval")
        .summary("Reject a pending dual-control approval")
        .description(
            "Rejects the PENDING approval with a mandatory reason; the mutation never runs.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval being decided.")
        .json_request::<ReasonRequest>(openapi, "The mandatory rejection reason.")
        .handler(reject)
        .json_response_with_schema::<ApprovalDto>(openapi, StatusCode::OK, "The rejected approval.")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/approvals/{approval_id}/request-changes")
        .operation_id("bss_ledger.request_changes_approval")
        .summary("Return a pending approval to the preparer for rework")
        .description(
            "Returns the PENDING approval to the preparer (NEEDS_REWORK) with a \
             mandatory reason; the preparer edits the intent and resubmits.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval being returned.")
        .json_request::<ReasonRequest>(openapi, "The mandatory rework reason.")
        .handler(request_changes)
        .json_response_with_schema::<ApprovalDto>(openapi, StatusCode::OK, "The returned approval.")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/approvals/{approval_id}/resubmit")
        .operation_id("bss_ledger.resubmit_approval")
        .summary("Resubmit a returned approval with an edited intent")
        .description(
            "Preparer-only: edits the intent of a NEEDS_REWORK approval and returns \
             it to PENDING (kind cannot change). The approval is never auto-applied.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval being resubmitted.")
        .json_request::<ResubmitRequest>(openapi, "The edited intent.")
        .handler(resubmit)
        .json_response_with_schema::<ApprovalDto>(
            openapi,
            StatusCode::OK,
            "The resubmitted approval.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/approvals/{approval_id}/cancel")
        .operation_id("bss_ledger.cancel_approval")
        .summary("Cancel an active approval (preparer only)")
        .description("Preparer-only: withdraws an active (PENDING/NEEDS_REWORK) approval.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval being cancelled.")
        .handler(cancel)
        .json_response_with_schema::<ApprovalDto>(
            openapi,
            StatusCode::OK,
            "The cancelled approval.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/approvals/{approval_id}/comments")
        .operation_id("bss_ledger.comment_approval")
        .summary("Add a comment / question to an approval thread")
        .description(
            "Appends a free comment (no state change) to the approval's append-only thread.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval being commented on.")
        .json_request::<CommentRequest>(openapi, "The comment body.")
        .handler(add_comment)
        .json_response_with_schema::<ApprovalThreadResponse>(
            openapi,
            StatusCode::CREATED,
            "The updated thread.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/approvals/{approval_id}/comments")
        .operation_id("bss_ledger.list_approval_comments")
        .summary("Read an approval's comment thread")
        .description("Returns the approval's comment thread, oldest-first.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval whose thread to read.")
        .handler(thread)
        .json_response_with_schema::<ApprovalThreadResponse>(
            openapi,
            StatusCode::OK,
            "The comment thread.",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/approvals/{approval_id}")
        .operation_id("bss_ledger.get_approval")
        .summary("Read a single approval")
        .description("Returns the approval `{approval_id}` for the caller's tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("approval_id", "The approval to read.")
        .handler(get)
        .json_response_with_schema::<ApprovalDto>(openapi, StatusCode::OK, "The approval.")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/approvals")
        .operation_id("bss_ledger.list_approvals")
        .summary("List the dual-control approval queue")
        .description("Lists the caller tenant's approvals (newest-first), optionally filtered by state / kind.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(list)
        .json_response_with_schema::<ApprovalListResponse>(
            openapi,
            StatusCode::OK,
            "The approval queue.",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-ledger/v1/dual-control-policy")
        .operation_id("bss_ledger.set_dual_control_policy")
        .summary("Set the tenant dual-control threshold policy")
        .description(
            "Writes a new effective-dated dual-control threshold version (the D2 \
             amount threshold, the A6 backdating window, the pending TTL) for the \
             caller's tenant (DC8). Append-only: a new version supersedes; the \
             resolver picks the latest effective_from (highest version on a tie). \
             Out-of-range D2/A6/TTL is rejected (409 DUAL_CONTROL_POLICY_OUT_OF_RANGE), \
             never clamped. Requires `dual_control_policy_write.v1`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<SetDualControlPolicyRequest>(openapi, "The threshold version to write.")
        .handler(set_policy)
        .json_response_with_schema::<DualControlPolicyResponse>(
            openapi,
            StatusCode::OK,
            "The written policy version.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-ledger/v1/dual-control-policy")
        .operation_id("bss_ledger.get_dual_control_policy")
        .summary("Read the effective dual-control threshold policy")
        .description(
            "Returns the tenant's EFFECTIVE dual-control threshold policy (read-\
             surface): the version in force now (the latest `effective_from <= now`, \
             highest `version` on a tie) — its D2 amount threshold, A6 backdating \
             window, and pending TTL — or the ratified platform defaults when the \
             tenant has set no policy row (`is_default = true`, with `version` / \
             `effective_from` null). `tenant_id` defaults to the caller's own. Gates \
             on `(dual_control_policy, read)` — the policy is its OWN resource (a \
             governance-officer read; its writer gates on `dual_control_policy.write`), \
             not an `entry` data-plane read; tenant-scoped (SQL-level BOLA) so a tenant \
             outside the caller's subtree reads the platform defaults (the thresholds \
             are public constants — no existence/value leak). Always `200`.",
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
        .json_response_with_schema::<DualControlPolicyView>(
            openapi,
            StatusCode::OK,
            "The effective dual-control threshold policy (configured version or platform defaults).",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

// ─── Handlers ──────────────────────────────────────────────────────────────

/// `(entry, approve)` PEP gate against the caller's own tenant; returns the
/// compiled scope every approval operation runs under.
async fn approve_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::APPROVE,
        Some(ctx.subject_tenant_id()),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// Resolve the caller's access to the approval surface. An `entry_approve.v1`
/// holder is an **approver** (`is_approver = true`) — may act on any approval in the
/// tenant. A caller who lacks it falls back to the `(entry, read)` plane as a
/// **preparer** (`is_approver = false`) — may act only on an approval they prepared,
/// and the per-route check then enforces `actor == prepared_by`. Mirrors impl-design
/// §6: resubmit/cancel are the preparer's originating right,
/// comments are `entry_approve.v1`-or-preparer. A PDP outage (`Unavailable`)
/// propagates; only an authorization `Denied` demotes to the preparer path.
async fn approval_access(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<(AccessScope, bool), CanonicalError> {
    let tenant = ctx.subject_tenant_id();
    match crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::ENTRY,
        crate::authz::actions::APPROVE,
        Some(tenant),
        None,
        /* require_constraints */ true,
    )
    .await
    {
        Ok(scope) => Ok((scope, true)),
        Err(crate::authz::AuthzError::Denied(_)) => {
            let scope = crate::authz::access_scope(
                enforcer,
                ctx,
                &crate::authz::resource_types::ENTRY,
                crate::authz::actions::READ,
                Some(tenant),
                None,
                /* require_constraints */ true,
            )
            .await
            .map_err(authz_error_to_canonical)?;
            Ok((scope, false))
        }
        Err(e) => Err(authz_error_to_canonical(e)),
    }
}

/// Authorize a comment-thread route (`entry_approve.v1`-or-preparer): an approver
/// may touch any approval in the tenant; a preparer (non-approver) must have
/// prepared THIS one. Reads the row under the caller's (tenant-isolated) scope and
/// checks `prepared_by` — so a preparer cannot read or post on another preparer's
/// thread.
async fn authorize_thread_participant(
    state: &ApiState,
    ctx: &SecurityContext,
    scope: &AccessScope,
    approval_id: Uuid,
    is_approver: bool,
) -> Result<(), CanonicalError> {
    if is_approver {
        return Ok(());
    }
    let row = state
        .service
        .get(ctx, scope, approval_id)
        .await?
        .ok_or_else(|| {
            CanonicalError::from(DomainError::ApprovalNotFound(format!(
                "approval {approval_id}"
            )))
        })?;
    if row.prepared_by != ctx.subject_id() {
        return Err(CanonicalError::from(DomainError::ApprovalNotActionable(
            "only the preparer or an entry_approve.v1 holder may participate in this \
             approval's comment thread"
                .to_owned(),
        )));
    }
    Ok(())
}

/// `(dual_control_policy, write)` PEP gate against the caller's own tenant — the
/// policy's OWN resource (a governance-officer grant), distinct from `ledger`
/// provisioning and from `entry_approve` so a per-operation approver cannot rewrite
/// the thresholds that gate them.
async fn policy_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::DUAL_CONTROL_POLICY,
        crate::authz::actions::WRITE,
        Some(ctx.subject_tenant_id()),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// Write a new tenant dual-control threshold policy version (DC8). `200` with the
/// minted version; `409 DUAL_CONTROL_POLICY_OUT_OF_RANGE` on an out-of-range value.
async fn set_policy(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    CanonicalJson(body): CanonicalJson<SetDualControlPolicyRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = policy_scope(&enforcer, &ctx).await?;
    let effective_from = body.effective_from.unwrap_or_else(OffsetDateTime::now_utc);
    let version = state
        .service
        .set_policy(
            &ctx,
            &scope,
            body.d2_threshold_minor,
            body.a6_backdating_biz_days,
            body.pending_ttl_seconds,
            effective_from,
        )
        .await?;
    Ok((
        StatusCode::OK,
        Json(DualControlPolicyResponse {
            version,
            effective_from,
            d2_threshold_minor: body.d2_threshold_minor,
            a6_backdating_biz_days: body.a6_backdating_biz_days,
            pending_ttl_seconds: body.pending_ttl_seconds,
        }),
    )
        .into_response())
}

/// `GET /dual-control-policy` (read-surface): read the tenant's EFFECTIVE
/// dual-control threshold policy. Gates on `(dual_control_policy, read)` — the
/// policy is its OWN resource (a governance-officer read; its writer gates on
/// `dual_control_policy.write`), NOT an `entry` data-plane read — and binds the
/// compiled scope as the
/// SQL-level BOLA filter, so a tenant outside the caller's subtree reads as no
/// rows ⇒ the ratified platform defaults (the thresholds are public constants — no
/// existence/value leak). `tenant_id` defaults to the caller's own. Always `200`:
/// the effective policy is the configured version in force or, absent a row, the
/// ratified platform defaults.
async fn get_policy(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<PolicyQuery>,
) -> Result<Json<DualControlPolicyView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant_id = query.tenant_id.unwrap_or_else(|| ctx.subject_tenant_id());
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::DUAL_CONTROL_POLICY,
        crate::authz::actions::READ,
        Some(tenant_id),
        None,
        /* require_constraints */ true,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let effective = state
        .service
        .read_effective_policy(&scope, tenant_id, OffsetDateTime::now_utc())
        .await?;
    Ok(Json(DualControlPolicyView::from_effective(effective)))
}

/// Re-read the approval after a state action and render it (`200`).
async fn approval_response(
    state: &ApiState,
    ctx: &SecurityContext,
    scope: &AccessScope,
    approval_id: Uuid,
) -> Result<Response, CanonicalError> {
    let row = state
        .service
        .get(ctx, scope, approval_id)
        .await?
        .ok_or_else(|| {
            CanonicalError::from(DomainError::ApprovalNotFound(format!(
                "approval {approval_id}"
            )))
        })?;
    Ok((StatusCode::OK, Json(ApprovalDto::from(row))).into_response())
}

async fn approve(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = approve_scope(&enforcer, &ctx).await?;
    state.service.approve(&ctx, &scope, approval_id).await?;
    approval_response(&state, &ctx, &scope, approval_id).await
}

async fn reject(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<ReasonRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = approve_scope(&enforcer, &ctx).await?;
    state
        .service
        .reject(&ctx, &scope, approval_id, body.reason)
        .await?;
    approval_response(&state, &ctx, &scope, approval_id).await
}

async fn request_changes(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<ReasonRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = approve_scope(&enforcer, &ctx).await?;
    state
        .service
        .request_changes(&ctx, &scope, approval_id, body.reason)
        .await?;
    approval_response(&state, &ctx, &scope, approval_id).await
}

async fn resubmit(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<ResubmitRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // resubmit is the preparer's originating right (impl-design §6): a non-approver
    // resolves the read plane, and `ApprovalService::resubmit` enforces
    // `actor == prepared_by`, so an approver who is not the preparer is rejected.
    let (scope, _) = approval_access(&enforcer, &ctx).await?;
    let intent: ApprovalIntent = serde_json::from_value(body.intent).map_err(|e| {
        CanonicalError::from(DomainError::InvalidRequest(format!(
            "resubmit intent is not a valid approval intent: {e}"
        )))
    })?;
    // The threshold snapshot is recomputed inside `resubmit` (DC17) against the
    // edited intent + the policy in force — the handler no longer fabricates one.
    state
        .service
        .resubmit(&ctx, &scope, approval_id, intent)
        .await?;
    approval_response(&state, &ctx, &scope, approval_id).await
}

async fn cancel(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // cancel is the preparer's originating right (impl-design §6): the service
    // enforces `actor == prepared_by`, so the read plane suffices for the gate.
    let (scope, _) = approval_access(&enforcer, &ctx).await?;
    state.service.cancel(&ctx, &scope, approval_id).await?;
    approval_response(&state, &ctx, &scope, approval_id).await
}

async fn add_comment(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
    CanonicalJson(body): CanonicalJson<CommentRequest>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Comments are "entry_approve.v1 OR preparer" (impl-design §6): an approver may
    // post on any approval; a preparer only on one they prepared.
    let (scope, is_approver) = approval_access(&enforcer, &ctx).await?;
    authorize_thread_participant(&state, &ctx, &scope, approval_id, is_approver).await?;
    state
        .service
        .add_comment(&ctx, &scope, approval_id, body.body)
        .await?;
    let comments = state.service.thread(&ctx, &scope, approval_id).await?;
    let dto = ApprovalThreadResponse {
        comments: comments.into_iter().map(ApprovalCommentDto::from).collect(),
    };
    Ok((StatusCode::CREATED, Json(dto)).into_response())
}

async fn thread(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Reading the thread is "entry_approve.v1 OR preparer": the preparer must see
    // the request-changes reason (it lives only as a thread comment) to rework.
    let (scope, is_approver) = approval_access(&enforcer, &ctx).await?;
    authorize_thread_participant(&state, &ctx, &scope, approval_id, is_approver).await?;
    let comments = state.service.thread(&ctx, &scope, approval_id).await?;
    let dto = ApprovalThreadResponse {
        comments: comments.into_iter().map(ApprovalCommentDto::from).collect(),
    };
    Ok((StatusCode::OK, Json(dto)).into_response())
}

async fn get(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(approval_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = approve_scope(&enforcer, &ctx).await?;
    approval_response(&state, &ctx, &scope, approval_id).await
}

async fn list(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = approve_scope(&enforcer, &ctx).await?;
    let rows = state
        .service
        .list(&ctx, &scope, query.state.as_deref(), query.kind.as_deref())
        .await?;
    let dto = ApprovalListResponse {
        approvals: rows.into_iter().map(ApprovalDto::from).collect(),
    };
    Ok((StatusCode::OK, Json(dto)).into_response())
}

#[cfg(test)]
mod dto_wire_tests {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use uuid::Uuid;

    use super::ApprovalDto;

    // Every instant on the DTO must be RFC 3339 on the wire. `expires_at` was
    // the one field without the serde attribute, so it rendered through `time`'s
    // default form and contradicted the `date-time` schema beside its siblings.
    #[test]
    fn every_instant_on_the_approval_dto_is_rfc3339() {
        let at = OffsetDateTime::from_unix_timestamp(1_757_000_000).expect("fixed instant");
        let dto = ApprovalDto {
            approval_id: Uuid::nil(),
            kind: "refund".to_owned(),
            state: "pending".to_owned(),
            revision: 1,
            business_key: "k".to_owned(),
            reason_code: "r".to_owned(),
            prepared_by: Uuid::nil(),
            prepared_at: at,
            approved_by: None,
            decided_at: Some(at),
            expires_at: at,
            amount_usd_eq_minor: None,
        };
        let json = serde_json::to_value(&dto).expect("serialize");
        for field in ["prepared_at", "decided_at", "expires_at"] {
            let raw = json[field]
                .as_str()
                .unwrap_or_else(|| panic!("{field} must be a string"));
            // **The rendering, and then the round-trip.** Parsing the field back
            // and comparing instants was the whole test, and it holds for every
            // RFC 3339 spelling of the same moment — `…20+00:00`, `…20.000000Z`,
            // `…20Z` — so the `date-time` wire form it exists to pin was the one
            // thing it could not fail on.
            assert_eq!(
                raw, "2025-09-04T15:33:20.000000Z",
                "{field} is `format_rfc3339`'s rendering, byte for byte"
            );
            let back = OffsetDateTime::parse(raw, &Rfc3339)
                .unwrap_or_else(|e| panic!("{field} must be RFC 3339, got {raw:?}: {e}"));
            assert_eq!(back, at, "{field} round-trips to the same instant");
        }
    }
}
