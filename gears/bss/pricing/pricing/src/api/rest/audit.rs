//! `GET /bss-pricing/v1/audit` — the Auditor read over the audit trail
//! (`design/05-governance.md` §5, `inst-au-read`; D-12, D-125, D-135, D-178).
//!
//! # Why this route exists, and what was wrong until it did
//!
//! `pricing_audit_log` had a writer and **no reader on any mounted surface**, and
//! `infra/error_mapping.rs` justifies dropping the detail from the three 403 arms
//! on the ground that the attempt is already on the table as a `deny` record
//! carrying the id — *"a durable trail rather than a log line"*. A compensating
//! control nothing can read is not one. The design set names this surface, its
//! permission, its audience and its pagination contract, so what landed is the
//! route it names and not one shaped here.
//!
//! # `audit × read`, which is the same pair `/history` asks for
//!
//! D-12 confines both to the Auditor, so the pair is shared and the **content is
//! not**: `/history` answers `pricing_price` rows with the authoring principal off
//! their own column, and this answers before/after states, the approval trail and
//! the correlation id. A reader who can see one can see the other by D-12's own
//! reading, and that is the decision rather than an accident of this route's
//! gating.
//!
//! # No correlation edge, and `frontier`'s reason
//!
//! D-178's edge is applied inside each **mutating** router. Nothing here writes an
//! audit record or an outbox row, so an `AuditStamp` would be a layer with no
//! reader. It **reports** correlation ids; it establishes none.
//!
//! # The cursor is the engine's, not [`crate::api::rest::cursor`]'s
//!
//! That token is a single id and this walk's position is an instant, a segment and
//! a position within it (D-135 makes `seq` per-segment). So the token is
//! [`crate::infra::audit_read::encode`]'s, exactly as `/history` takes its own
//! engine's — a route may name an engine type and `DE0202` refuses the reverse.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::{Json, Router, http::StatusCode};
use bss_pricing_sdk::odata::{AuditFilterField, AuditOrderField};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::domain::instant::rfc3339;
use crate::infra::audit_read::{AuditEntry, AuditPage, AuditReader};
use time::OffsetDateTime;

const TAG: &str = "BSS Pricing Audit";

/// The path this surface answers on.
///
/// A `const` for its siblings' reason: `OperationBuilder` takes the literal, so a
/// census that spelled its paths as string literals is a census one rename walks
/// away from.
pub const AUDIT: &str = "/bss-pricing/v1/audit";

/// What this surface needs, and nothing else.
pub struct ApiState {
    /// `inst-au-read`'s page over `pricing_audit_log`.
    pub audit: AuditReader,
}

/// One audit record on the wire.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AuditEntryView {
    /// The audited subject's aggregate — this record's chain segment (D-135).
    /// Two records of one aggregate share it, which is how an auditor reads one
    /// subject's history out of a page of many.
    pub chain_id: Uuid,
    /// The record's position inside its segment, `0` at genesis.
    pub seq: i64,
    /// `mutation` or `rollup`.
    pub entry_kind: String,
    /// When it was recorded, UTC.
    // Typed rather than pre-rendered, and this is a `//` so it stays out of the
    // published schema: `recorded_at` is a machine instant this module's own note
    // exempts from the millisecond quantum, Postgres keeps it at microsecond
    // precision, and `$filter=recorded_at gt ...` binds at that precision - so a
    // millisecond-rendered string could not be fed back to resume a walk, and two
    // entries inside one millisecond read alike.
    #[serde(with = "rfc3339")]
    pub recorded_at: OffsetDateTime,
    /// The **pseudonymous** principal who acted — never a display name and never
    /// an email (`inst-au-pii`), which is what lets a 7-year store be read at all.
    pub actor_principal_id: Uuid,
    /// What was done, as the record holds it (D-158's `action` vocabulary).
    pub action: String,
    /// What kind of thing it was done to (D-158's `subject_kind` vocabulary).
    pub subject_kind: String,
    /// Which one.
    pub subject_ref: String,
    /// The subject's state before the mutation, as the record's own `jsonb`
    /// column holds it.
    pub before_state: Option<serde_json::Value>,
    /// The subject's state after it.
    pub after_state: Option<serde_json::Value>,
    /// The approval record the mutation ran under, when it had one — the trail
    /// `inst-au-complete` requires, and the id the 403 arms drop from their detail.
    pub approval_ref: Option<Uuid>,
    /// The correlation id of the request that caused it (D-178), which is what
    /// lets a reader pull every record of one operator call together.
    pub correlation_id: Option<Uuid>,
}

/// A page of the audit trail.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AuditPageView {
    /// The page's records in commit order, oldest first.
    pub entries: Vec<AuditEntryView>,
    /// The token to pass as `cursor` for the next page, absent once the walk is
    /// exhausted. Absent on the last page rather than on the page after it, so a
    /// client stops without an extra round trip.
    pub next_cursor: Option<String>,
}

impl From<AuditPage> for AuditPageView {
    fn from(page: AuditPage) -> Self {
        Self {
            entries: page.entries.into_iter().map(audit_entry_view).collect(),
            next_cursor: page.next.map(crate::infra::audit_read::encode),
        }
    }
}

fn audit_entry_view(entry: AuditEntry) -> AuditEntryView {
    AuditEntryView {
        chain_id: entry.chain_id,
        seq: entry.seq,
        entry_kind: entry.entry_kind,
        recorded_at: entry.recorded_at,
        actor_principal_id: entry.actor_principal_id,
        action: entry.action,
        subject_kind: entry.subject_kind,
        subject_ref: entry.subject_ref,
        before_state: entry.before_state,
        after_state: entry.after_state,
        approval_ref: entry.approval_ref,
        correlation_id: entry.correlation_id,
    }
}

/// Build the Axum router for the audit read.
///
/// The declared errors are the ones this path can produce: 400 (a `limit` of zero
/// or a cursor that does not decode), 401, 403, 503 and 500. No 404 — an empty
/// trail is a page with no entries, not a missing resource, which is `/history`'s
/// reading and the frontier's.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get(AUDIT)
        .operation_id("bss_pricing.read_audit_trail")
        .summary("Read the immutable audit trail")
        .description(
            "Returns the tenant's audit trail in commit order, oldest first: every \
             append-only `pricing_audit_log` record with the pseudonymous principal who acted, \
             the instant, the before and after states, the approval record the mutation ran \
             under and the correlation id of the request that caused it. Denied attempts are \
             records like any other - a self-approval refused is a `deny` record naming the \
             approval it was attempted on, which is where an operator recovers the detail the \
             403 does not carry. Ordered by `(recorded_at, chain_id, seq)`, the cursor's own \
             key: `seq` counts within a chain segment (D-135), never across the tenant. \
             Paginated on an opaque cursor per D-125, with a server default page size and a \
             hard cap. **Auditor-only**: gates on `audit` x `read` (D-12), which carries no \
             read of live pricing and no write authority. `$filter` and `$orderby` are the AM \
             list contract. This surface is not the export - `audit` x `export` is a second \
             permission for a chunked shape, which `POST /bss-pricing/v1/history/export` is \
             the gear's one holder of.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(crate::api::rest::history::limit_param())
        .param(crate::api::rest::history::cursor_param())
        .handler(read_audit)
        .with_odata_filter::<AuditFilterField>()
        .with_odata_orderby::<AuditOrderField>()
        .json_response_with_schema::<AuditPageView>(
            openapi,
            StatusCode::OK,
            "A page of the audit trail in commit order, with the token for the next page when \
             one remains.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    router.layer(Extension(state))
}

/// `GET /audit`: the caller tenant's audit trail, one page.
///
/// `owner_tenant_id` is `None` — a read, so the PDP derives the scope from the
/// subject and its roles rather than trusting a caller-supplied tenant, and the
/// compiled scope becomes the SQL filter. `require_constraints = true` so an
/// unconstrained allow fails closed instead of exposing every tenant's trail,
/// which on this surface would be every tenant's before/after states.
#[allow(clippy::implicit_hasher)]
async fn read_audit(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<AuditPageView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::AUDIT,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let (entries, next_cursor) = state
        .audit
        .read_odata(&scope, ctx.subject_tenant_id(), &odata)
        .await
        .map_err(map_odata_page_err)?;

    Ok(Json(AuditPageView {
        entries: entries.into_iter().map(audit_entry_view).collect(),
        next_cursor,
    }))
}
