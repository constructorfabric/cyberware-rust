//! `GET /bss-pricing/v1/history` — the price-history read
//! (`design/12-operator-efficiency.md` §3 `inst-he-read`, §5; D-12, D-125,
//! D-270).
//!
//! **Both of §5's history surfaces, and they are one walk.** The read serves
//! pages at D-125's server default of 100; the export serves **chunks** the
//! caller sizes, up to the same hard cap of 1,000, in the same commit order and
//! on the same cursor. `inst-he-export`'s own words are *"export streams the same
//! commit order in bounded chunks"*, and the engine says the rest:
//! [`HistoryExporter`] is one type for both *"because `inst-he-export` describes
//! the same order in bounded chunks and differs only in the page size a caller
//! asks for"*. What separates them is not the walk but the **permission** — see
//! the gate note below — and the SLO the chunk size is stated per: p95 <= 5s per
//! 100 records, so a full 1,000-row page is budgeted at 50s and is an export
//! shape rather than an interactive read (C-6).
//!
//! Neither opens a transaction and neither writes an audit record: `inst-he-nostore`
//! is normative, so there is no export **job** here, no artifact to collect and
//! nothing for §5's idempotency column to replay — see [`export_history`] for
//! that, stated rather than quietly dropped.
//!
//! # The correlation edge is here and has no reader, which is the honest trade
//!
//! Nothing in this module writes an audit record or an outbox row, so nothing here
//! needs an `AuditStamp` and no handler calls `require_correlation`. What brings it
//! into scope is the census, not a write:
//! `rest_authz::every_mutating_router_applies_the_correlation_edge` decides who
//! owes D-178's edge by scanning this directory's sources for a **mutating
//! builder**, and `OperationBuilder::post(HISTORY_EXPORT)` is one — the export is
//! a `POST` because §5 says so, and it is a **read** because `inst-he-nostore`
//! leaves it nothing to write.
//!
//! Two ways out, and the cheaper one is not the exemption. That census's own
//! value is that it "starts from the filesystem" rather than from a list somebody
//! maintains, and a per-router carve-out is precisely the maintained list it was
//! built to replace — one whose next entry is a router that really does write.
//! An unused layer costs one `Extension` insert per request and cannot be wrong.
//! So the edge is applied, and this paragraph is what stops a later reader
//! deleting it as dead: it is not dead, it is the census's premise being paid.
//!
//! # `audit × read`, and the actor is the row's own column
//!
//! **`audit × read`, not `plan × read`** — the handler asks it and `rest_authz`'s
//! census catalogues it that way. D-12's *original* reading is the pull the other
//! way: price history is plan and price data, so Finance reads it by construction.
//! What overrides that is stated at the gate itself: `/history` is the catalog audit
//! trail, so filing it under catalog read hands "who changed what, when" to every
//! holder of `plan × read` while `audit_read` grants nothing.
//!
//! What that leaves standing is the **source** rule, which is a different claim and
//! still exactly true: [`HistoryEntry::actor`](crate::infra::history::HistoryEntry::actor)
//! reads `pricing_price.created_by` and never `pricing_audit_log`. The two surfaces
//! now share a permission and not a store — see [`crate::api::rest::audit`], which
//! is the Slice 5 trail's own read — and a surface that took its actor from the
//! audit log would still be reading a store it is not the reader of.
//!
//! # The cursor is opaque on the wire because it is opaque in the engine
//!
//! `limit` and `cursor` are the two D-125 spellings. The token this surface hands
//! back is [`crate::infra::history::encode`]'s, not
//! [`crate::api::rest::cursor`]'s: the engine's position is an instant **and** a
//! row, which the id-only REST token cannot spell (D-270). The direction of the
//! dependency is what matters — a route may name an engine type, and `DE0202`
//! refuses the reverse, which is how the first draft of D-270 was caught.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Query};
use axum::{Json, Router, http::StatusCode};
use bss_pricing_sdk::odata::HistoryFilterField;
use serde::Deserialize;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::{OperationBuilderODataExt, ParamLocation, ParamSpec};
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::cursor;
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::domain::instant::format_rfc3339;
use crate::infra::history::{HistoryEntry, HistoryExporter, HistoryPage, HistoryPageRequest};

const TAG: &str = "BSS Pricing History";

/// The path this surface answers on.
///
/// A `const` for its siblings' reason: `OperationBuilder` takes the literal, so
/// the route-shape rule binds where the literal is, and a route census that
/// spelled its paths as string literals is a census one rename walks away from.
pub const HISTORY: &str = "/bss-pricing/v1/history";

/// §5's export path — the same walk, in the chunk sizes the export SLO is stated
/// per (`inst-he-export`, D-125).
pub const HISTORY_EXPORT: &str = "/bss-pricing/v1/history/export";

/// What this surface needs, and nothing else.
pub struct ApiState {
    /// §1.7's chronological read over the append-only price rows.
    pub history: HistoryExporter,
}

/// The two D-125 query parameters, both optional.
#[derive(Debug, Default, Deserialize)]
pub struct HistoryQuery {
    /// Page size. Absent means D-125's server default; above the cap it clamps.
    pub limit: Option<String>,
    /// The opaque token the previous page handed back.
    pub cursor: Option<String>,
}

/// One history record on the wire.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct HistoryEntryView {
    /// The append-only row this record is of.
    pub price_id: Uuid,
    /// The plan it prices.
    pub plan_id: Uuid,
    /// Its lifecycle state at read time — `draft`, `published` or `superseded`.
    pub lifecycle_state: String,
    /// The pseudonymous principal who authored the row, from its **own**
    /// column (D-12).
    pub actor: String,
    /// When the row was authored, UTC.
    pub authored_at: String,
    /// The row's scheduled intervals in `from` order, each with its own state.
    /// Empty when the row has never been scheduled.
    pub effective: Vec<EffectiveIntervalView>,
}

/// One scheduled interval of a history record.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct EffectiveIntervalView {
    /// Start of the interval, UTC.
    pub from: String,
    /// End of the interval, UTC; absent when the interval is open-ended.
    pub to: Option<String>,
    /// The window's state — a cancelled or expired interval is **included** and
    /// distinguishable, history being a record of what was and not of what is.
    pub state: String,
}

/// A page of history.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct HistoryPageView {
    /// The page's records, oldest first.
    pub entries: Vec<HistoryEntryView>,
    /// The token to pass as `cursor` for the next page, or absent when the walk
    /// is exhausted. Absent on the last page rather than on the page after it,
    /// so a client stops without an extra round trip.
    pub next_cursor: Option<String>,
}

impl From<HistoryPage> for HistoryPageView {
    fn from(page: HistoryPage) -> Self {
        Self {
            entries: page.entries.iter().map(history_entry_view).collect(),
            next_cursor: page.next.map(crate::infra::history::encode),
        }
    }
}

fn history_entry_view(entry: &HistoryEntry) -> HistoryEntryView {
    HistoryEntryView {
        price_id: entry.record.price_id,
        plan_id: entry.record.scope_key.plan_id().get(),
        lifecycle_state: entry.record.lifecycle_state.as_str().to_owned(),
        actor: entry.actor().to_string(),
        authored_at: format_rfc3339(entry.record.created_at_utc),
        effective: entry
            .effective
            .iter()
            .map(|interval| EffectiveIntervalView {
                from: format_rfc3339(interval.from),
                to: interval.to.map(format_rfc3339),
                state: interval.state.as_str().to_owned(),
            })
            .collect(),
    }
}

/// D-125's page size, declared once for every cursor walk in the gear.
///
/// `pub(crate)` for `plans::idempotency_key_param`'s reason and on its precedent:
/// the contract is the gear's rather than this route's, and two spellings of one
/// parameter's description are two answers to what a generated client's docs say
/// about it. This surface was the first D-125 walk to declare its parameters at
/// all, which is why the spelling lives here.
pub(crate) fn limit_param() -> ParamSpec {
    ParamSpec {
        name: "limit".to_owned(),
        location: ParamLocation::Query,
        required: false,
        description: Some(
            "Page size. Absent takes the server default; a value above the cap is clamped \
             rather than refused, the cap being a server limit and the page size the export \
             SLO is stated per (D-125). Zero is refused: a page of zero rows never advances."
                .to_owned(),
        ),
        param_type: "integer".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// D-125's opaque page token, declared once — [`limit_param`]'s note.
pub(crate) fn cursor_param() -> ParamSpec {
    ParamSpec {
        name: "cursor".to_owned(),
        location: ParamLocation::Query,
        required: false,
        description: Some(
            "The opaque token this same operation returned as `next_cursor`. GET history and \
             GET audit mint an OData CursorV1; POST history/export mints the history-engine \
             token. The two encodings are not interchangeable."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// Build the Axum router for the price-history surface.
///
/// The declared errors are the ones this path can produce: 400 (a `limit` of zero
/// or a cursor that does not decode), 401, 403, 503 and 500. No 404 — an empty
/// history is a page with no entries, not a missing resource, exactly as the
/// frontier's empty reading is a 200.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get(HISTORY)
        .operation_id("bss_pricing.read_price_history")
        .summary("Read immutable price history")
        .description(
            "Returns the tenant's price history in commit order, oldest first: every \
             append-only `pricing_price` row with the principal who authored it, the instant \
             it was authored, and the scheduled intervals it has held. The read adds no store \
             - the Foundation's immutability IS the history (`inst-he-nostore`) - and it \
             paginates on an opaque cursor per D-125, with a server default page size and a \
             hard cap. Superseded and cancelled records are included and distinguishable: \
             history records what was, not what is. Gates on `audit` x `read` (D-12): this is \
             the catalog audit trail, so it is Auditor-only rather than Finance-readable, and \
             `plan` x `read` grants no access to it. The separate Slice 5 trail over \
             `pricing_audit_log` has its own read (`GET /bss-pricing/v1/audit`) and is never \
             this surface's source for the actor.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(limit_param())
        .param(cursor_param())
        .handler(read_history)
        .with_odata_filter::<HistoryFilterField>()
        .with_odata_orderby::<HistoryFilterField>()
        .json_response_with_schema::<HistoryPageView>(
            openapi,
            StatusCode::OK,
            "A page of history in commit order, with the token for the next page when one \
             remains.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::post(HISTORY_EXPORT)
        .operation_id("bss_pricing.export_price_history")
        .summary("Export immutable price history in bounded chunks")
        .description(
            "Streams the tenant's price history in the same commit order the interactive read serves, in bounded chunks: one call returns one chunk and the token for the next, and a client walks the token to the end of the history. Identical records and identical order; the cursor is this surface's history-engine token, not the OData CursorV1 GET /history mints, and the two are not interchangeable. The remaining difference is the chunk size a caller asks for, which is what the SLO is stated per (p95 <= 5s per 100 records, scaling linearly to the D-125 hard cap of 1,000). That is why this is a separate surface rather than a larger `limit` on the read: a full 1,000-row page is budgeted at 50s and is an export shape, never an interactive read, so the read keeps the server default of 100 and this is where a caller asks for more. It adds no store - the Foundation's immutability IS the history (`inst-he-nostore`), so an export is a read and produces no job, no file and no artifact to collect later. Gates on `audit` x `export` (D-12): bulk extraction of a seven-year actor trail is grantable separately from reading it, which is the whole reason the action exists beside `audit` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(limit_param())
        .param(cursor_param())
        .handler(export_history)
        .json_response_with_schema::<HistoryPageView>(
            openapi,
            StatusCode::OK,
            "One chunk of history in commit order, with the token for the next chunk when one remains.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
        .layer(Extension(state))
}

/// `GET /history`: the caller tenant's price history, one page.
///
/// `owner_tenant_id` is `None` — a read, so the PDP derives the scope from the
/// subject and its roles rather than trusting a caller-supplied tenant, and the
/// compiled scope becomes the SQL filter. `require_constraints = true` so an
/// unconstrained allow fails closed instead of exposing every tenant's history.
#[allow(clippy::implicit_hasher)]
async fn read_history(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<HistoryPageView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        // The audit trail is its own disclosure, so it is its own permission:
        // `audit_read` ("Read the catalog audit trail") is declared for exactly
        // this surface. Gating on `plan x read` handed the trail -- who changed
        // what, when, and to what -- to every holder of catalog read, and left
        // the declared permission granting nothing.
        &crate::authz::resource_types::AUDIT,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let (entries, next_cursor) = state
        .history
        .read_odata(&scope, ctx.subject_tenant_id(), &odata)
        .await
        .map_err(map_odata_page_err)?;

    Ok(Json(HistoryPageView {
        entries: entries.iter().map(history_entry_view).collect(),
        next_cursor,
    }))
}

/// `POST /history/export`: one chunk of the caller tenant's price history
/// (`inst-he-export`).
///
/// # The same engine, and deliberately not a second one
///
/// [`HistoryExporter`]'s own doc settles this: *"One type for both, because
/// `inst-he-export` describes the same order in bounded chunks and differs only
/// in the page size a caller asks for. Two types would be two walk orders free to
/// disagree, over a store whose whole claim is that the sequence of rows is the
/// truth."* So this handler is [`read_history`] with a different gate over the
/// same walk order.
///
/// The permission is not the only thing that differs on the wire, though, and
/// [`cursor_param`] is the one that says so: this route mints the
/// history-engine token from [`HistoryPageRequest`], while `GET /history` mints
/// an `OData` `CursorV1`. Same order, same rows, two encodings — a token from one
/// route does not decode on the other, which is a `400` rather than a silently
/// wrong page. One shared engine was the claim; one shared cursor never was.
///
/// # `POST` with no body, and the two parameters in the query
///
/// §5 gives this path the `POST` verb, which is what a caller sends. The chunk it
/// wants is named the way every other D-125 walk in the gear names one — `limit`
/// and `cursor`, through [`limit_param`] and [`cursor_param`], the single
/// spelling of that contract. A request **body** carrying the same two would be a
/// second spelling of the one pagination contract, and the export SLO is stated
/// per the value those two produce.
///
/// # §5's idempotency column has no operand on this surface, and that is stated
/// rather than papered over
///
/// The API table gives this row a *client key*. A client key buys a replay, a
/// replay needs somewhere to have stored the first answer, and
/// `inst-he-nostore` — normative, and restated in §6 ("History/export reads
/// existing append-only structures — no new store") — leaves this surface no
/// store to put one in. It also needs nothing from one: this is a read over an
/// append-only table, so re-issuing it is not a second act to be deduplicated.
/// So no `Idempotency-Key` is declared, because declaring a header the server
/// ignores is what
/// `module_test::a_read_route_declares_no_precondition_header` exists to refuse.
/// Whether the column is a residue of an asynchronous export shape the design set
/// forbids elsewhere is a question for the design set, not something to be
/// settled by a handler.
async fn export_history(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryPageView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        // `audit` x **`export`**, and the second half of that pair is the point.
        // `/history` is the catalog audit trail (see this module's own gate note),
        // and `actions::EXPORT`'s own doc states the distinction it was declared
        // for: "Export the audit trail. Distinct from READ so bulk extraction of a
        // seven-year actor trail is grantable separately from reading it." A
        // chunked walk of the whole >= 7-year store at 1,000 records a call **is**
        // that bulk extraction, so gating it on `read` would have made the export
        // permission grant nothing and withholding it protect nothing --
        // `rest_authz`'s own argument, which carried this pair as the gear's one
        // catalogued-but-unasked permission until this route arrived.
        &crate::authz::resource_types::AUDIT,
        crate::authz::actions::EXPORT,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let request = HistoryPageRequest::parse(
        cursor::parse_limit(query.limit.as_deref())?,
        query.cursor.as_deref(),
    )
    .map_err(CanonicalError::from)?;

    let page = state
        .history
        .read_page(&scope, ctx.subject_tenant_id(), request)
        .await
        .map_err(|e| CanonicalError::from(crate::infra::storage::repo_failure(&e)))?;

    Ok(Json(HistoryPageView::from(page)))
}
