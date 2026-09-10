//! The authz gate, proved over the **set** rather than per route.
//!
//! Everything before this proved the gate one route at a time, which leaves two
//! holes an in-process suite can close and only closes deliberately.
//!
//! The first is a route gated on the **wrong pair**. No allow/deny test can see
//! it: a `plan x read` gate on a mutating route allows exactly the callers a
//! `plan x write` gate would in every fixture that grants both, and denies
//! exactly the ones it would in every fixture that grants neither. Only a
//! resolver that records what it was asked can tell. That is the census below.
//!
//! The second is a route added later **with no gate at all**. A per-route suite
//! grows a hole the moment somebody adds a route and forgets its test; a census
//! driven from the registered path set fails instead.
//!
//! # What this suite cannot prove, and does not pretend to
//!
//! **The role matrix is the PDP's, not this gear's.** Which role holds
//! `plan x write`, and whether `FinanceReviewer` can read what it approves
//! (D-61's reviewability invariant), are decisions in
//! `05-governance.md`'s role table that this gear neither implements nor can
//! observe: it asks a question and binds the answer. Nothing here substitutes
//! for testing that table where it lives.
//!
//! **Nothing here is an isolation-level property.** Everything runs over
//! `sqlite::memory:`, which serializes writers.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::approvals::{
    APPROVAL, APPROVAL_APPROVE, APPROVAL_COUNTS, APPROVAL_REJECT, APPROVAL_WITHDRAW, APPROVALS,
};
use bss_pricing::api::rest::audit::AUDIT;
use bss_pricing::api::rest::bulk_imports::{BULK_IMPORT, BULK_IMPORT_ABORT, BULK_IMPORTS};
use bss_pricing::api::rest::bundles::{BUNDLE_BY_ID, BUNDLE_PUBLISH, BUNDLES};
use bss_pricing::api::rest::catalog_skus::{CATALOG_SKUS, CATALOG_TAX_CATEGORIES};
use bss_pricing::api::rest::customer_groups::{
    CUSTOMER_GROUP_MEMBER, CUSTOMER_GROUP_MEMBER_MOVE, CUSTOMER_GROUP_MEMBERS,
    CUSTOMER_GROUP_MEMBERS_MOVE, CUSTOMER_GROUP_TAXONOMY,
};
use bss_pricing::api::rest::cutovers::{PLAN_CUTOVERS, PRICE_GRANDFATHER_UNTIL};
use bss_pricing::api::rest::frontier::{CATALOG_VERSION_REF, FRONTIER};
use bss_pricing::api::rest::gl_codes::GL_CODES;
use bss_pricing::api::rest::history::{HISTORY, HISTORY_EXPORT};
use bss_pricing::api::rest::migrated_origin_snapshots::MIGRATED_ORIGIN_SNAPSHOT;
use bss_pricing::api::rest::migrations::{MIGRATION_BY_ID, MIGRATIONS};
use bss_pricing::api::rest::overlays::{PRICE_OVERLAY_BY_ID, PRICE_OVERLAY_SUBMIT, PRICE_OVERLAYS};
use bss_pricing::api::rest::plans::{PLAN_ABANDON, PLAN_CLONE, PLANS, PLANS_COUNTS};
use bss_pricing::api::rest::preview::PLAN_PREVIEW;
use bss_pricing::api::rest::prices::{PLAN_PRICE, PLAN_PRICES};
use bss_pricing::api::rest::publish::PLAN_PUBLISH;
use bss_pricing::api::rest::repricing_runs::{REPRICING_RUN, REPRICING_RUN_ABORT, REPRICING_RUNS};
use bss_pricing::api::rest::retirement::PLAN_RETIRE;
use bss_pricing::api::rest::rounding_policies::ROUNDING_POLICIES;
use bss_pricing::api::rest::rounding_policy::ROUNDING_POLICY;
use bss_pricing::api::rest::supersessions::PLAN_SUPERSESSIONS;
use bss_pricing::api::rest::tax_display_policy::TAX_DISPLAY_POLICY;
use bss_pricing::api::rest::taxonomies::{TAXONOMY, TAXONOMY_VALUE, TAXONOMY_VALUES};
use bss_pricing::api::rest::threshold_policy::APPROVAL_THRESHOLD_POLICY;
use bss_pricing::api::rest::windows::{
    PLAN_COVERAGE, PLAN_SELLABILITY, PRICE_WINDOW, PRICE_WINDOWS, PRICE_WINDOWS_LIST,
};
use bss_pricing::authz::{actions, labels};
use bss_pricing::domain::approval::ApprovalState;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::read_model::SubjectRef;
use bss_pricing::infra::storage::repo::{PendingVersionRow, catalog_version_ref_repo};
use rest_support::{
    Harness, approval_row, body_json, mutable_planes, plan_count, plan_row_version, price_rows,
    request, seed_draft_plan, seed_price, with_headers,
};
use uuid::Uuid;

/// One row of the census: a route, and the catalogued pair it must gate on.
///
/// The pairs come from [`bss_pricing::authz`]'s consts, never from string
/// literals, so a label renamed in one place cannot be "fixed" in the other. The
/// **source** is `05-governance.md`'s endpoint -> `(resource, action)` map:
/// `POST/PATCH /bss-pricing/v1/plans*`, `POST …/prices*` and `DELETE …/prices/{id}`
/// are `plan x write`; `GET /bss-pricing/v1/plans*`, `GET …/prices` and the
/// frontier are `plan x read`; the entrance is `plan x publish`, spelled
/// `publish (submit for publish)` there; and the approval surface is
/// `approval x read` on its two reads and `approval x approve` on all three
/// decisions.
///
/// **`plan x publish` is not `plan x write`, and the matrix is why.**
/// `ProductManager` holds `plan x write/read` and deliberately **not**
/// `plan x publish`, so a publish route gated on `write` would hand the
/// entrance to a role the role table excludes from it — and no allow/deny test
/// can see the difference, because every fixture that grants one grants the
/// other.
struct Route {
    /// HTTP method.
    method: &'static str,
    /// The registered path template.
    path: &'static str,
    /// The catalogued resource label.
    resource_type: &'static str,
    /// The catalogued action.
    action: &'static str,
    /// Whether driving it changes state (so a denial has something to observe).
    mutating: bool,
}

#[allow(
    clippy::too_many_lines,
    reason = "a flat catalogue whose length IS its content: one row per route, each naming the \
              (resource, action) pair that route must ask for. Splitting it would have to split \
              on something other than its own meaning -- reads from writes, or slice from slice \
              -- and then a reader checking whether a route is catalogued has two places to \
              look and a census has two ways to be incomplete. It crossed the limit when Slice \
              12's history read was catalogued."
)]
fn census() -> Vec<Route> {
    let mut rows = vec![
        Route {
            method: "GET",
            path: FRONTIER,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: CATALOG_VERSION_REF,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        // `audit x read` per D-12 **as amended by D-328** (2026-08-17): `/history`
        // is the catalog audit trail, not plan data beside it. D-12's original
        // reading -- price history is plan and price data, so Finance reads it by
        // construction -- stood in this comment until 2026-08-18, three months
        // after the row below stopped implementing it.
        //
        // Withdrawn rather than reworded, because of what this file is: every
        // allow/deny property here is driven *from* this census, so a maintainer
        // who trusted the comment and moved the row to `labels::PLAN` would take
        // the fixture with it and redden nothing -- while exposing a >= 7-year
        // actor trail to every holder of `plan x read`. That is this file's own
        // stated hazard, made concrete.
        Route {
            method: "GET",
            path: HISTORY,
            resource_type: labels::AUDIT,
            action: actions::READ,
            mutating: false,
        },
        // §5's export of that same trail (`inst-he-export`). **`audit x export`,
        // and it is the gear's first route to ask for that pair** — the one
        // catalogued permission that granted nothing until it existed, which
        // `every_catalogued_permission_pair_is_asked_for_by_some_route` carried as
        // its single exemption. The action's own doc is the argument: bulk
        // extraction of a seven-year actor trail is grantable separately from
        // reading it, and a chunked walk of the whole store at 1,000 records a
        // call is exactly that.
        //
        // **`mutating: false` on a `POST`**, which no other row here is. The verb
        // is §5's; the act is a read, because `inst-he-nostore` gives this surface
        // no store to write to and no job to enqueue. A row claiming otherwise
        // would put it in `every_mutating_route_is_denied_with_the_state_unchanged`,
        // which would then assert that a read changed nothing — true, and true of
        // every read, so it would measure nothing.
        Route {
            method: "POST",
            path: HISTORY_EXPORT,
            resource_type: labels::AUDIT,
            action: actions::EXPORT,
            mutating: false,
        },
        // That separate surface (`inst-au-read`, Z13-8). Same pair as the row
        // above and a different store: D-12 confines both to the Auditor, and
        // this one answers before/after states and the approval trail.
        Route {
            method: "GET",
            path: AUDIT,
            resource_type: labels::AUDIT,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: PLANS_BY_ID,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        // The collection read. `plan x read` exactly as the by-id read is, and
        // the row is here rather than being taken as obvious because the pair a
        // *collection* asks for is the one place a listing can go wrong
        // invisibly: it passes `resource_id: None`, so what the PDP compiles is
        // the tenant filter the whole walk runs under.
        Route {
            method: "GET",
            path: PLANS,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        // D-360: the counts read asks the list's exact pair, once.
        Route {
            method: "GET",
            path: PLANS_COUNTS,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "POST",
            path: PLANS,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        // Slice 8. Authoring is `bundle x write`; **publish is `plan x publish`
        // only** — D-11, because under the conjunction the design set first
        // stated, only CatalogAdmin could publish a bundle while S8 §1.3
        // promises FinanceManager can. Catalogued here so the gate is a fact
        // this census asserts rather than a comment in a handler.
        Route {
            method: "POST",
            path: BUNDLES,
            resource_type: labels::BUNDLE,
            action: actions::WRITE,
            mutating: true,
        },
        // The bundle collection read. `bundle x read`, the pair the by-id read
        // below already asks for — and not the `plan x read` the *publish* route
        // gates on, which is a different act on a different resource. Catalogued
        // rather than taken as obvious for the plan collection's reason: a
        // listing passes `resource_id: None`, so the pair it asks for is what
        // compiles the tenant filter the whole walk runs under.
        Route {
            method: "GET",
            path: BUNDLES,
            resource_type: labels::BUNDLE,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: BUNDLE_BY_ID,
            resource_type: labels::BUNDLE,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PATCH",
            path: BUNDLE_BY_ID,
            resource_type: labels::BUNDLE,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: BUNDLE_PUBLISH,
            resource_type: labels::PLAN,
            action: actions::PUBLISH,
            mutating: true,
        },
        Route {
            method: "PATCH",
            path: PLANS_BY_ID,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: PLAN_ABANDON,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: PLAN_CLONE,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        // The bulk plane, on `plan` — S5 §3's own filing, and it took three
        // corrections to get here. These three rows read `historical_import`
        // until 2026-08-17 because the handlers asked for it, and this census
        // records what a handler asks: the pair check below compares the two
        // descriptions of one implementation, so it was green throughout. What
        // the design set says is `plan x read` for the `GET` (S5 §3's read row
        // names the path outright) and, for both mutating surfaces, *"the
        // **same** `plan x write` / `publish` — bulk is authoring at scale (and
        // abort is un-authoring at scale), no new authority"*. The wrong filing
        // was not merely untidy: `historical_import` was the restricted
        // backdating grant, "never included in a default role" (S5 §3 step 5),
        // so the three roles S5's matrix gives `plan x write` were answered 403
        // on a plane the same table hands them.
        Route {
            method: "POST",
            path: BULK_IMPORTS,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "GET",
            path: BULK_IMPORT,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "POST",
            path: BULK_IMPORT_ABORT,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        // Slice 12's mass repricing. S12 §5's AuthZ column reads `plan x write`
        // for the open and `plan x read` for the progress endpoint, which is the
        // bulk import's split one path over and for its reason: a run's journal is
        // price data, and an operator who may read prices may read what a run did
        // to them.
        Route {
            method: "POST",
            path: REPRICING_RUNS,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "GET",
            path: REPRICING_RUN,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        // D-37's abort. `plan x write`, the same authority the open takes: S5 §3's
        // bulk row is explicit that abort is un-authoring at scale and no new
        // authority, which is why the bulk import's own abort row above reads the
        // same.
        Route {
            method: "POST",
            path: REPRICING_RUN_ABORT,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: PLAN_PRICES,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "GET",
            path: PLAN_PRICES,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: PLAN_PRICE,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PATCH",
            path: PLAN_PRICE,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "DELETE",
            path: PLAN_PRICE,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "GET",
            path: PLAN_COVERAGE,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        // The gate's surface is `plan x read` like the coverage report: it answers
        // about a plan's published content, and the AuthZ catalog puts read on the
        // plan. It is deliberately **not** a narrower pair - there is no
        // `sellability` label - and it needs no new authz vocabulary.
        Route {
            method: "GET",
            path: PLAN_SELLABILITY,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        // Slice 7's three mutating surfaces. All three are `plan x write` and not
        // `plan x publish`, although each is a publish *unit* (D-99): the pair is
        // what the AuthZ catalog's endpoint map assigns to the path, and §5 files
        // these three under the owned scheduling surface rather than under the
        // publish route. A window mutation therefore needs no `publish` grant, and
        // that is the catalog's call, not this test's.
        Route {
            method: "POST",
            path: PRICE_WINDOWS,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "PATCH",
            path: PRICE_WINDOW,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "DELETE",
            path: PRICE_WINDOW,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        // The window collection read. `plan x read`, which is what `GET
        // …/coverage` and `GET …/sellability` already ask for on this surface —
        // there is no `price` label in the catalogue at all, so a listing over
        // the window plane is a read of the plan plane by the only vocabulary
        // this gear has. Catalogued rather than taken as obvious for the reason
        // the plan collection's row gives: a listing passes `resource_id: None`,
        // so the pair it asks for is what compiles the tenant filter the whole
        // walk runs under.
        Route {
            method: "GET",
            path: PRICE_WINDOWS_LIST,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        // The supersession unit (D-88). **`plan x write`, not `plan x publish`** — S5's
        // endpoint map puts it there beside the cutover, and the reason is the same one
        // the window mutations rest on: what `publish` guards is the *entrance*, and a
        // supersession that commits does so under an approval this route does not grant
        // itself. Gating it on `publish` would deny it to `ProductManager`, whom the role
        // matrix does grant `plan x write`.
        Route {
            method: "POST",
            path: PLAN_SUPERSESSIONS,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        // The grandfathering cutover (D-100), on the **same** pair and for the same
        // argument — S5's endpoint map puts the two side by side, and this one commits
        // under an approval it does not grant itself either. No new authz vocabulary:
        // the catalog already carries every label and action a cutover needs.
        Route {
            method: "POST",
            path: PLAN_CUTOVERS,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        // The horizon door (S7 §4's `inst-gs-bound`/`inst-gs-tighten`), on the same
        // pair as the cutover that creates the generation it bounds — S5's endpoint
        // map names it in that very cell, so this route adds no authz vocabulary
        // either. It asks the pair **twice**, for the window `PATCH`'s reason: its
        // path names a row and the question is about the row's plan. See
        // `FURTHER_QUESTIONS`.
        Route {
            method: "PATCH",
            path: PRICE_GRANDFATHER_UNTIL,
            resource_type: labels::PLAN,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: PLAN_PUBLISH,
            resource_type: labels::PLAN,
            action: actions::PUBLISH,
            mutating: true,
        },
        Route {
            method: "GET",
            path: APPROVALS,
            resource_type: labels::APPROVAL,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: APPROVAL_COUNTS,
            resource_type: labels::APPROVAL,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: APPROVAL,
            resource_type: labels::APPROVAL,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "POST",
            path: APPROVAL_APPROVE,
            resource_type: labels::APPROVAL,
            action: actions::APPROVE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: APPROVAL_REJECT,
            resource_type: labels::APPROVAL,
            action: actions::APPROVE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: APPROVAL_WITHDRAW,
            resource_type: labels::APPROVAL,
            action: actions::APPROVE,
            mutating: true,
        },
        // The threshold policy is `approval_policy`, **never** `config`, and the
        // separation is the point rather than a taxonomy choice: a config admin
        // holds `config × write` and must not be able to weaken the thresholds that
        // decide whether their own changes need a second principal. No allow/deny
        // fixture can see the difference — every fixture granting one grants the
        // other — which is why it is asserted here, the way `plan × publish` is.
        Route {
            method: "GET",
            path: APPROVAL_THRESHOLD_POLICY,
            resource_type: labels::APPROVAL_POLICY,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PUT",
            path: APPROVAL_THRESHOLD_POLICY,
            resource_type: labels::APPROVAL_POLICY,
            action: actions::WRITE,
            mutating: true,
        },
    ];
    rows.extend(retirement_routes());
    rows.extend(migration_routes());
    rows.extend(synthesis_routes());
    rows.extend(overlay_routes());
    rows.extend(config_routes());
    rows
}

/// Slice 4's two config-plane routes, extracted for [`overlay_routes`]' reason:
/// [`census`] crossed the line cap again when they were added, and one more
/// extraction is honest where a lint allow would not be. The roster is still
/// `census()`.
fn config_routes() -> Vec<Route> {
    vec![
        // Slice 4's preview. `plan × preview` and **not** `plan × read`: §2 and
        // §10 both make the grant an extra assignment the default role matrix
        // does not carry, so filing it under `read` would hand it to every
        // holder of the ordinary catalog read. Invisible to any allow/deny
        // fixture — the same shape as `plan × publish`, and asserted for the
        // same reason.
        Route {
            method: "GET",
            path: PLAN_PREVIEW,
            resource_type: labels::PLAN,
            action: actions::PREVIEW,
            mutating: false,
        },
        // Slice 4's taxonomies are the other half of that separation, and the
        // contrast is the reason they sit here rather than with the authoring
        // routes. They gate on `config`, which is exactly what the threshold
        // policy must **not** gate on: a CatalogAdmin holding `config × write`
        // declares regions and brands and cannot, by that same grant, touch the
        // thresholds deciding whether their own changes need a second principal.
        // No allow/deny fixture can see the difference, so it is asserted.
        Route {
            method: "GET",
            path: TAXONOMY,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
        // D-353: the whole-set `PUT` is removed; the per-value routes are the same
        // resource one value wide, so they gate on the same pair — `config`, never
        // `approval_policy`.
        Route {
            method: "POST",
            path: TAXONOMY_VALUES,
            resource_type: labels::CONFIG,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "GET",
            path: TAXONOMY_VALUE,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PATCH",
            path: TAXONOMY_VALUE,
            resource_type: labels::CONFIG,
            action: actions::WRITE,
            mutating: true,
        },
        // Slice 9's own taxonomy (`inst-cg-taxonomy`), on its own route and its
        // own `customer_group` gate — **not** `config`. This pair is the whole
        // point of the separate route: a `CatalogAdmin` holding `config x write`
        // (declares regions, brands, partners, org tiers) must not, by that same
        // grant, touch the customer-group vocabulary — per-payer commercial data
        // is segregated (`05-governance.md`'s endpoint map, and
        // `api::rest::customer_groups`'s module doc). No allow/deny fixture can
        // see a route gated on the wrong label; only this census and
        // `SelectiveResolver`-driven cases in `rest_customer_groups.rs` can.
        Route {
            method: "GET",
            path: CUSTOMER_GROUP_TAXONOMY,
            resource_type: labels::CUSTOMER_GROUP,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PUT",
            path: CUSTOMER_GROUP_TAXONOMY,
            resource_type: labels::CUSTOMER_GROUP,
            action: actions::WRITE,
            mutating: true,
        },
        // Task 6's membership mutations, on the **same** `customer_group x write`
        // pair the taxonomy `PUT` above asks for — `dod-customer-group`'s MUST that
        // every committed membership mutation is its own publish unit through the
        // Foundation engine (D-06). No new authz vocabulary.
        // D-322's read. `customer_group × read`, the pair its taxonomy GET
        // already uses — a membership list is the same resource read.
        Route {
            method: "GET",
            path: CUSTOMER_GROUP_MEMBERS,
            resource_type: labels::CUSTOMER_GROUP,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "POST",
            path: CUSTOMER_GROUP_MEMBERS,
            resource_type: labels::CUSTOMER_GROUP,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "PATCH",
            path: CUSTOMER_GROUP_MEMBER,
            resource_type: labels::CUSTOMER_GROUP,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: CUSTOMER_GROUP_MEMBER_MOVE,
            resource_type: labels::CUSTOMER_GROUP,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: CUSTOMER_GROUP_MEMBERS_MOVE,
            resource_type: labels::CUSTOMER_GROUP,
            action: actions::WRITE,
            mutating: true,
        },
        // C4's enforcement mode. `config`, like the taxonomies and unlike the
        // approval-threshold policy — this one softens how one publish rule
        // reports one missing external fact; that one decides whether a change
        // needs a second principal at all.
        Route {
            method: "GET",
            path: TAX_DISPLAY_POLICY,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PUT",
            path: TAX_DISPLAY_POLICY,
            resource_type: labels::CONFIG,
            action: actions::WRITE,
            mutating: true,
        },
        // The tenant's default rounding policy (D-320). `config` for the same
        // reason as its neighbour above: it supplies a default publish would
        // otherwise demand row by row, and decides nothing about who may approve
        // what.
        Route {
            method: "GET",
            path: ROUNDING_POLICY,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PUT",
            path: ROUNDING_POLICY,
            resource_type: labels::CONFIG,
            action: actions::WRITE,
            mutating: true,
        },
        // D-334's declared vocabulary, `config` like every taxonomy: it narrows
        // what may be authored and decides nothing about who approves what.
        Route {
            method: "GET",
            path: ROUNDING_POLICIES,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PUT",
            path: ROUNDING_POLICIES,
            resource_type: labels::CONFIG,
            action: actions::WRITE,
            mutating: true,
        },
        // D-356's GL-code vocabulary, `config` for the rounding vocabulary's
        // reason: it narrows what a descriptor may name and decides nothing about
        // who approves what.
        Route {
            method: "GET",
            path: GL_CODES,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PUT",
            path: GL_CODES,
            resource_type: labels::CONFIG,
            action: actions::WRITE,
            mutating: true,
        },
        // A pass-through of another gear's browse list. `config` x `read` like
        // the taxonomies beside it: it is a suggestion source for authoring and
        // decides nothing, and it is not `plan` because no plan is named.
        Route {
            method: "GET",
            path: CATALOG_SKUS,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: CATALOG_TAX_CATEGORIES,
            resource_type: labels::CONFIG,
            action: actions::READ,
            mutating: false,
        },
    ]
}

/// Slice 9's four overlay routes, extracted so [`census`] stays under the line
/// cap rather than growing a lint allow.
///
/// A function per slice would be the wrong shape — the census is one roster and
/// the properties below range over all of it — but one extraction at the point
/// the cap bit is honest, and the roster is still `census()`.
/// Slice 11's retirement surface — a function of its own for `overlay_routes`'
/// reason, which is that `census` has a line budget and a slice's routes are the
/// natural unit to lift out of it.
///
/// **The one route in this census whose action is neither `write` nor
/// `publish`.** `plan × retire` exists so that holding the authority to change a
/// plan is not holding the authority to end it: the flip is irreversible, it
/// stops all new sales, and the plan can never publish again to correct it.
fn retirement_routes() -> Vec<Route> {
    vec![Route {
        method: "POST",
        path: PLAN_RETIRE,
        resource_type: labels::PLAN,
        action: actions::RETIRE,
        mutating: true,
    }]
}

/// Slice 11's migration plane (§5).
///
/// Four rows and deliberately not six. `POST .../start` and `.../complete` are
/// specified (D-65) and their storage half is built, but they are **not mounted**:
/// `/start` must run D-36's execution-time re-resolution and that has no input in
/// this system, so a mounted `/start` would hand Subscriptions an exclusion set
/// computed from nothing. A census row for an unmounted route would fail the
/// registered-vs-census equality above, which is the census doing its job.
///
/// The `DELETE` is `plan x migrate` rather than a delete-shaped grant because it
/// **cancels**: D-34 rescoped it and the row is never removed.
fn migration_routes() -> Vec<Route> {
    vec![
        Route {
            method: "POST",
            path: MIGRATIONS,
            resource_type: labels::PLAN,
            action: actions::MIGRATE,
            mutating: true,
        },
        // The collection read. `plan x read`, the by-id read's pair, and
        // **not** `plan x migrate`: listing what is scheduled is not the
        // authority to schedule, and the role matrix grants the two apart.
        Route {
            method: "GET",
            path: MIGRATIONS,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: MIGRATION_BY_ID,
            resource_type: labels::PLAN,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "DELETE",
            path: MIGRATION_BY_ID,
            resource_type: labels::PLAN,
            action: actions::MIGRATE,
            mutating: true,
        },
    ]
}

/// D-102's `migrated-origin` read surface.
///
/// **`plan x read`, and the object is deliberately not the subscription.** This
/// is the one route in the gear whose authz object differs from its path object:
/// the content is the catalog's, just content no `CatalogVersion` addresses, so
/// it is served under the same authority as the read model. Asking the PDP about
/// a subscription id as a `plan` resource would be a question about an object of
/// the wrong type - which is exactly the mis-gating this census exists to catch.
fn synthesis_routes() -> Vec<Route> {
    vec![Route {
        method: "GET",
        path: MIGRATED_ORIGIN_SNAPSHOT,
        resource_type: labels::PLAN,
        action: actions::READ,
        mutating: false,
    }]
}

fn overlay_routes() -> Vec<Route> {
    vec![
        // Slice 9's overlay half. All five are `price_overlay` x its own action
        // and deliberately **not** `plan`: the AuthZ catalog gives overlays a
        // resource of their own, and D-61's reviewability invariant is why the
        // approving role needs `price_overlay x read` — a reviewer of an
        // always-material overlay mutation who could not read overlays would be
        // approving a document they cannot see. Filing these under `plan` would
        // hand overlay authorship to every holder of `plan x write`, which no
        // allow/deny fixture can see.
        //
        // The submit is `write` and **not** `publish`: what `publish` guards is
        // the plan entrance, and an overlay submit publishes nothing by itself —
        // it opens an approval unit (D-50). Gating it on `plan x publish` would
        // also deny it to every role that holds overlay authoring and no plan
        // authority at all.
        Route {
            method: "POST",
            path: PRICE_OVERLAYS,
            resource_type: labels::PRICE_OVERLAY,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "GET",
            path: PRICE_OVERLAYS,
            resource_type: labels::PRICE_OVERLAY,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "GET",
            path: PRICE_OVERLAY_BY_ID,
            resource_type: labels::PRICE_OVERLAY,
            action: actions::READ,
            mutating: false,
        },
        Route {
            method: "PATCH",
            path: PRICE_OVERLAY_BY_ID,
            resource_type: labels::PRICE_OVERLAY,
            action: actions::WRITE,
            mutating: true,
        },
        Route {
            method: "POST",
            path: PRICE_OVERLAY_SUBMIT,
            resource_type: labels::PRICE_OVERLAY,
            action: actions::WRITE,
            mutating: true,
        },
    ]
}

/// `plans::PLAN` under a name that does not collide with `labels::PLAN`.
const PLANS_BY_ID: &str = bss_pricing::api::rest::plans::PLAN;

/// The seeded world every case drives against, and the concrete request for a
/// route template.
#[derive(Clone)]
struct Seeded {
    /// The draft plan every row is aimed at.
    plan: Uuid,
    /// Its one draft price row.
    price: Uuid,
    /// The pending unit the approval rows are aimed at.
    approval: Uuid,
    /// One `scheduled` window on [`Seeded::price`], so the two window routes that
    /// address a window by id reach their gate instead of failing to parse a path.
    window: Uuid,
    /// One bundle on [`Seeded::plan`], so the two routes that address a bundle
    /// by id reach their gate instead of failing to parse a path — the same
    /// reason [`Seeded::window`] exists.
    bundle: Uuid,
    /// One overlay draft, so the three routes that address an overlay by id
    /// reach their gate rather than a 404 — [`Seeded::bundle`]'s reason.
    overlay: Uuid,
    /// One pending catalog-version handle against [`Seeded::plan`], so
    /// `GET /catalog-version/refs/{pendingRef}` reaches a 200 for the owner
    /// rather than a 404. The handle is a registry token, not a UUID.
    pending_ref: String,
    /// A brand value the tenant declares (`{value}` on the per-value taxonomy
    /// routes, D-353); `drive` fills `{class}` with `brand`, so this is its pair.
    brand_value: String,
}

/// The world the whole census is driven against: a draft plan with a shape and
/// one price row, **and a pending approval unit over it**.
///
/// The unit is opened through the service rather than through the publish route
/// on purpose. The route would first have to pass the validation pipeline, so
/// the seed would have to become a publishable plan — a different world from the
/// one the eight authoring rows have always been driven against, and changing it
/// to reach the approval rows would silently re-stage every case in the file.
/// What the approval rows need from the seed is one thing only: an id that
/// resolves to a record in the caller's tenant, so a refusal is the gate's and
/// never a 404's.
async fn seed(harness: &Harness) -> Seeded {
    let plan_id = Uuid::now_v7();
    seed_draft_plan(harness, plan_id).await;
    harness.attach_shape(plan_id, 0).await;
    let price = seed_price(harness, plan_id, "EU").await;
    let approval_id = Uuid::now_v7();
    harness
        .governance
        .approvals
        .submit(
            &harness.scope(),
            harness.tenant,
            bss_pricing::domain::scope_key::PlanId::new(plan_id),
            approval_id,
            serde_json::json!({ "material": true, "reason": "noConfiguredThreshold" }),
            rest_support::seed_stamp(),
        )
        .await
        .expect("open the pending unit the approval rows are driven against");
    let window = rest_support::seed_window(harness, price.price_id).await;
    let bundle = harness
        .state
        .bundles
        .create(
            &harness.scope(),
            bss_pricing::infra::storage::repo::NewBundle {
                bundle_id: Uuid::now_v7(),
                tenant_id: harness.tenant,
                plan_id: bss_pricing::domain::scope_key::PlanId::new(plan_id),
                price_basis: bss_pricing::domain::bundle::PriceBasis::SumOfParts,
                invoice_itemization: bss_pricing::domain::bundle::InvoiceItemization::Aggregate,
            },
            rest_support::seed_stamp(),
        )
        .await
        .expect("seed the bundle the two bundle-by-id routes address");
    let overlay = Uuid::now_v7();
    harness
        .state
        .overlays
        .create(
            &harness.scope(),
            bss_pricing::infra::storage::repo::NewOverlay {
                price_overlay_id: overlay,
                tenant_id: harness.tenant,
                scope: bss_pricing::domain::overlay::ScopeSelector::Global,
                precedence: 10,
                interval: bss_pricing::domain::overlay::OverlayInterval::default(),
                tax_basis: bss_pricing::domain::overlay::TaxBasis::DelegatedTariffs,
                disclosure: bss_pricing::domain::overlay::Disclosure::Restricted,
                target_ref: bss_pricing::domain::overlay::TargetRef { plans: Vec::new() },
            },
            vec![bss_pricing::domain::overlay::OverlayLine {
                line_id: Uuid::now_v7(),
                key: bss_pricing::domain::overlay::LineKey::list_default(),
                adjustment: bss_pricing::domain::overlay::Adjustment::Discount(
                    bss_pricing::domain::overlay::Magnitude::PercentBp(1000),
                ),
            }],
            rest_support::seed_stamp(),
        )
        .await
        .expect("seed the overlay the two overlay-by-id routes address");
    let pending_ref = "dev-local-census-v1".to_owned();
    catalog_version_ref_repo::record_pending(
        &harness.db.conn().expect("conn"),
        &harness.scope(),
        PendingVersionRow::for_subject(
            harness.tenant,
            pending_ref.clone(),
            &SubjectRef::Plan(plan_id),
            Some(0),
            Some(LifecycleState::Published),
            rest_support::at(10),
        ),
    )
    .await
    .expect("seed the pending handle the catalog-version-ref read addresses");

    let brand_value = "acme";
    harness
        .state
        .taxonomies
        .declare_value(
            &harness.scope(),
            harness.tenant,
            bss_pricing::domain::taxonomy::TaxonomyClass::Brand,
            bss_pricing::domain::taxonomy::TaxonomyEntry {
                value: bss_pricing::domain::overlay::ScopeValue::new(brand_value)
                    .expect("a non-blank value"),
                display_name: "Acme".to_owned(),
                state: bss_pricing::domain::taxonomy::TaxonomyState::Active,
                tax: None,
            },
            rest_support::seed_stamp(),
        )
        .await
        .expect("declare the seeded brand value");
    Seeded {
        plan: plan_id,
        price: price.price_id,
        approval: approval_id,
        window,
        bundle,
        overlay,
        pending_ref,
        brand_value: brand_value.to_owned(),
    }
}

/// A request's optional JSON body and its preconditions.
type DrivenBody = (Option<serde_json::Value>, Vec<(&'static str, &'static str)>);

/// Fill a route template with the seeded ids and a well-formed body.
///
/// The precondition is deliberately **the current one** — on a plan route the
/// revision-qualified `"0-3"` the seeded draft stands at after `attach_shape`,
/// on a price route the row's own `"0"` — so a refusal in any of these cases is
/// the gate's and never a stale tag's. The entrance takes the plan-plane tag for
/// the same reason.
fn drive(
    route: &Route,
    seeded: &Seeded,
    version: &'static str,
    key: &'static str,
) -> axum::http::Request<axum::body::Body> {
    let path = route
        .path
        .replace("{planId}", &seeded.plan.to_string())
        .replace("{priceId}", &seeded.price.to_string())
        .replace("{approvalId}", &seeded.approval.to_string())
        .replace("{windowId}", &seeded.window.to_string())
        .replace("{bundleId}", &seeded.bundle.to_string())
        .replace("{overlayId}", &seeded.overlay.to_string())
        .replace("{pendingRef}", &seeded.pending_ref)
        // **Not a seeded id, and it does not need to be.** Both migration item
        // routes ask the PDP *before* they read the row, so a schedule that does
        // not exist still drives the gate — which is the only thing this file
        // asserts about them. Seeding one would add a fixture whose absence
        // nothing here could detect. A well-formed UUID is required all the same:
        // `Path<Uuid>` rejects a malformed segment with a 400 that never reaches
        // a gate, which is exactly what the four properties below reddened with
        // before this line existed.
        .replace("{migrationId}", "00000000-0000-4000-8000-000000000fa1")
        // The bulk run: a well-formed id nothing points at, which is what the
        // gate properties need — they assert the refusal, and a run that existed
        // would let a denied call be mistaken for a missing one.
        .replace("{operationId}", "00000000-0000-4000-8000-000000000b17")
        // The repricing run, on the line above's reasoning exactly. It is a
        // **different** segment because it is a different value: the progress
        // endpoint is addressed by the caller's `run_id`, which the run carries as
        // its client key, and never by the minted `operationId` its sibling uses.
        // The read asks the PDP before it looks the run up, so an id nothing
        // points at still drives the gate.
        .replace("{runId}", "00000000-0000-4000-8000-000000000f00")
        // Not a seeded id either, and for the same reason: the read surface asks
        // the PDP before it reads, and a 404 for a subscription that was never
        // synthesized is the contract (`inst-sy-firstrating`) rather than a gap
        // in this fixture.
        .replace("{subscriptionRef}", "00000000-0000-4000-8000-0000000005ab")
        // The taxonomy class is a **resource selector**, not a seeded id: any of
        // the four addressable segments drives the same handler through the same
        // gate. `brand` is used because it is the one Slice 9's overlay scope
        // actually reads, so a denial here is a denial on the path an operator
        // is really walking.
        .replace("{class}", "brand")
        .replace("{value}", &seeded.brand_value)
        // Task 6's membership routes. `{group}` is a taxonomy **value**, not a
        // seeded id — `group_membership_repo` validates no taxonomy universe
        // (its own module doc), so any non-blank token drives the gate; a
        // fixed literal is used for `{class}`'s reason. `{id}`/`{payerId}` are
        // well-formed ids nothing points at — every membership route asks the
        // PDP before it resolves either, so an absent membership still drives
        // the gate, `{migrationId}`'s reasoning.
        .replace("{group}", "gold")
        .replace("{id}", "00000000-0000-4000-8000-0000000005c1")
        .replace("{payerId}", "00000000-0000-4000-8000-0000000005c2");
    // The sellability surface requires all three of §5's query parameters and
    // parses them **before** it asks the PDP. That is the layer's **one stated
    // exception** to gate-before-parse (`api::rest`'s section of that name, which
    // names this route and rules on it): the gate is `plan × read` over a *market*,
    // and a caller who omitted `currency` has asked a question that names no
    // subject for a gate to be about. So the request driven here has to carry all
    // three, or every property in this file would be asserted against a 400 that
    // never reached a gate. Found by mounting the route and watching
    // `every_route_asks_the_catalogued_pair` and the PDP-outage case both redden.
    //
    // This read "`schedule_window`'s ordering, and for its reason" until
    // 2026-08-20, and that ordering was **withdrawn on 2026-08-18** —
    // `schedule_window` gates first now, precisely because reading the request
    // first answered 400 to a caller with no authority. Citing it here made the
    // exception look like a house pattern instead of the single argued case it is;
    // `the_sellability_exception_is_the_ordering_it_is_declared_to_be` below pins
    // what the exception does and does not permit.
    let path = if route.path == PLAN_SELLABILITY {
        format!("{path}?at=2099-01-01T00:00:00Z&currency=EUR&region=EU")
    } else {
        path
    };
    let (body, headers) = body_for(route, seeded, version, key);
    with_headers(route.method, &path, body, &headers)
}

/// The well-formed body and preconditions [`drive`] sends for one route.
///
/// Split out of `drive` to keep it under `clippy::too_many_lines` — `census`'s
/// own reason for the sibling lint's `allow` applies here too: the catalogue
/// is one match arm per route and its length **is** its content, so there is
/// nothing to split on but the function boundary itself.
fn body_for(
    route: &Route,
    seeded: &Seeded,
    version: &'static str,
    key: &'static str,
) -> DrivenBody {
    match (route.method, route.path) {
        // Slice 9's five. The `POST` takes an idempotency key and a whole
        // overlay; the by-id `GET` takes none; the `PATCH` takes the **overlay
        // revision's** tag, which the seeded draft stands at as `"0-0"`; the
        // submit takes neither, since its concurrency is the approval unit's.
        ("POST", PRICE_OVERLAYS) => (
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 42,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [{
                    "adjustment_kind": "discount",
                    "magnitude_kind": "percent_bp",
                    "adjustment_value": 1000,
                }],
            })),
            vec![("idempotency-key", key)],
        ),
        ("PATCH", PRICE_OVERLAY_BY_ID) => (
            Some(serde_json::json!({
                "revision": 0,
                "lines": [{
                    "adjustment_kind": "discount",
                    "magnitude_kind": "percent_bp",
                    "adjustment_value": 1500,
                }],
            })),
            vec![("if-match", "\"0-0\"")],
        ),
        ("POST", PRICE_OVERLAY_SUBMIT) => (Some(serde_json::json!({ "revision": 0 })), Vec::new()),
        ("POST", PLANS) => (
            Some(serde_json::json!({ "plan_tier": "gold" })),
            vec![("idempotency-key", key)],
        ),
        // Slice 8's three. The composition route takes the **plan revision's**
        // tag, which is the plane the seeded draft stands on; the publish route
        // takes neither a tag nor a key, since its concurrency is the approval
        // unit's and its idempotency is per revision.
        ("POST", BUNDLES) => (
            Some(serde_json::json!({
                "plan_id": Uuid::now_v7(),
                "price_basis": "sum_of_parts",
            })),
            vec![("idempotency-key", key)],
        ),
        ("PATCH", BUNDLE_BY_ID) => (
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [],
            })),
            vec![("if-match", version)],
        ),
        ("POST", BUNDLE_PUBLISH) => (
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            vec![],
        ),
        ("POST", PLAN_PRICES) => (
            Some(serde_json::json!({
                "scope_key": {
                    "currency": "USD",
                    "region": "US",
                    "phase": rest_support::seeded_phase().get().to_string(),
                    "price_eligibility": "all_subscriptions",
                    "charge_kind": "recurring",
                    "cohort": serde_json::Value::Null
                },
                "content": { "model_kind": "flat", "amount_minor": 100 }
            })),
            vec![("idempotency-key", key)],
        ),
        ("PATCH", p) if p == PLANS_BY_ID => (
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            vec![("if-match", version)],
        ),
        ("PATCH", PLAN_PRICE) => (
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 7 } })),
            vec![("if-match", "\"0\"")],
        ),
        // A well-formed instant and a well-formed tag, both deliberately wrong for
        // the seeded row — the refusal under test is the gate's, and a body the
        // handler could act on would let a later reader mistake a domain refusal
        // for one.
        ("PATCH", PRICE_GRANDFATHER_UNTIL) => (
            Some(serde_json::json!({ "grandfather_until": "2030-01-01T00:00:00Z" })),
            vec![("if-match", "\"0\"")],
        ),
        ("POST", PLAN_ABANDON | PLAN_PUBLISH) => (None, vec![("if-match", version)]),
        // The clone holds no version of the source — it reads the source's
        // current revision and writes nothing to it — so the precondition it
        // carries is the idempotency key, not an `If-Match`.
        // Three keyed writes with no body: the clone reads its source from the
        // path, and the abort names its run there. The bulk submit is below,
        // because it carries one.
        ("POST", PLAN_CLONE | BULK_IMPORT_ABORT) => (None, vec![("idempotency-key", key)]),
        // An empty batch is enough: the gate runs before the rows are looked at,
        // which is the property under test.
        ("POST", BULK_IMPORTS) => (
            Some(serde_json::json!({ "rows": [] })),
            vec![("idempotency-key", key)],
        ),
        // The repricing run, and it carries **no** idempotency header: S12 §5's
        // column for this surface is the `run_id` body member. The body is
        // otherwise well-formed and its changeover is dated 2099, so a refusal
        // here is the gate's and never the submit floor's — the fixtures' standing
        // rule, and load-bearing on this route because the instant is judged
        // against the wall clock. An unconstrained selector is deliberate: the
        // gate runs before the expansion, so a run that would be refused
        // `RUN_SELECTOR_EMPTY` on the allowed path still proves the denial.
        ("POST", REPRICING_RUNS) => (
            Some(serde_json::json!({
                "run_id": "00000000-0000-4000-8000-000000000f01",
                "selector": { "currency": "USD" },
                "adjustment": {
                    "adjustment_kind": "discount",
                    "magnitude_kind": "percent_bp",
                    "adjustment_value": 500,
                },
                "changeover": "2099-08-20T00:00:00Z",
            })),
            Vec::new(),
        ),
        ("DELETE", PLAN_PRICE) => (None, vec![("if-match", "\"0\"")]),
        // The `Idempotency-Key` is required on the schedule (D-171) and is read
        // before anything is resolved, so a request without one never reaches the
        // gate this suite is about.
        ("POST", PRICE_WINDOWS) => (
            Some(serde_json::json!({
                "effective_from": "2099-01-01T00:00:00Z",
                "reason_code": "authz-probe"
            })),
            vec![("idempotency-key", key)],
        ),
        // The supersession body is parsed before the gate for `schedule_window`'s
        // reason, so a request without one would never reach the PDP this suite is
        // about. It carries **no** idempotency header: S5's column for this surface is
        // the act's own identity, not a client key.
        ("POST", PLAN_SUPERSESSIONS) => (
            Some(serde_json::json!({
                "predecessor_price_id": seeded.price.to_string(),
                "changeover": "2099-08-20T00:00:00Z",
                "successor": { "model_kind": "flat", "amount_minor": 100 },
                "reason_code": "authz-probe"
            })),
            vec![],
        ),
        // The cutover's body, on the supersession's reasoning exactly: parsed after
        // the gate, and no idempotency header because S5's column for it is the act's
        // own identity — `(planId, key-set hash, cutover instant)` since D-28.
        ("POST", PLAN_CUTOVERS) => (
            Some(serde_json::json!({
                "predecessor_price_id": seeded.price.to_string(),
                "cutover_at": "2099-08-20T00:00:00Z",
                "successor": { "model_kind": "flat", "amount_minor": 100 },
                "reason_code": "authz-probe"
            })),
            vec![],
        ),
        // The retirement probe rides the **dry-run** arm, which is the default and
        // the one this census wants: what is under test is the gate, and a probe
        // that confirmed would open an approval unit over an irreversible act for
        // every principal the matrix walks.
        ("POST", PLAN_RETIRE) => (
            Some(serde_json::json!({
                "dry_run": true,
                "reason_code": "authz-probe"
            })),
            vec![],
        ),
        // The membership `PATCH` takes the same well-formed body and the same
        // wrong-but-well-formed tag as the price-window `PATCH` — merged into
        // one arm on `clippy::match_same_arms`'s say-so, not a coincidence
        // worth two spellings.
        ("PATCH", PRICE_WINDOW | CUSTOMER_GROUP_MEMBER) => (
            Some(serde_json::json!({ "effective_to": "2099-06-01T00:00:00Z" })),
            vec![("if-match", "\"0\"")],
        ),
        // The reason is mandatory on a reject (`inst-as-reject`), and a reject
        // without one is refused **after** the gate — but a body that carries it
        // is what makes this the same request an operator sends.
        // A well-formed proposal, so a refusal here is the gate's and never
        // `THRESHOLD_INVALID`'s. One entry is the minimum a version can carry.
        ("PUT", APPROVAL_THRESHOLD_POLICY) => (
            Some(serde_json::json!({
                "effective_from": "2099-01-01T00:00:00Z",
                "entries": [{ "currency": "EUR", "absolute_minor": 500 }]
            })),
            Vec::new(),
        ),
        // A well-formed whole-set replacement, so a refusal here is the gate's
        // and never a body or precondition complaint. The `If-Match` is
        // deliberately a **wrong-but-well-formed** tag: the header is parsed
        // before the tag is compared but **after** the gate, so a denied caller
        // is told they may not write rather than that their tag is stale — which
        // is the ordering `every_route_asks_the_catalogued_pair` depends on, and
        // the ordering this row would silently stop asserting if the header were
        // omitted instead.
        ("PUT", TAX_DISPLAY_POLICY) => (
            Some(serde_json::json!({ "mode": "fail_closed" })),
            vec![(
                "if-match",
                "\"0000000000000000000000000000000000000000000000000000000000000000\"",
            )],
        ),
        ("PUT", TAXONOMY) => (
            Some(serde_json::json!({
                "values": [{ "value": "acme", "display_name": "Acme" }]
            })),
            vec![(
                "if-match",
                "\"0000000000000000000000000000000000000000000000000000000000000000\"",
            )],
        ),
        // The customer-group taxonomy's `PUT`, on `TAXONOMY`'s own reasoning: a
        // well-formed whole-set replacement under a wrong-but-well-formed tag, so
        // a refusal here is the gate's and never a body or precondition
        // complaint.
        ("PUT", CUSTOMER_GROUP_TAXONOMY) => (
            Some(serde_json::json!({
                "values": [{ "value": "gold", "display_name": "Gold" }]
            })),
            vec![(
                "if-match",
                "\"0000000000000000000000000000000000000000000000000000000000000000\"",
            )],
        ),
        // Task 6's two `POST`s take an idempotency key; the `PATCH` is merged
        // into `PRICE_WINDOW`'s arm above (identical body and tag).
        ("POST", CUSTOMER_GROUP_MEMBERS) => (
            Some(serde_json::json!({
                "payer_tenant_id": Uuid::now_v7(),
                "effective_from": "2099-01-01T00:00:00Z"
            })),
            vec![("idempotency-key", key)],
        ),
        ("POST", CUSTOMER_GROUP_MEMBER_MOVE) => (
            Some(serde_json::json!({ "effective_from": "2099-01-01T00:00:00Z" })),
            vec![("idempotency-key", key)],
        ),
        ("POST", CUSTOMER_GROUP_MEMBERS_MOVE) => (
            Some(serde_json::json!({
                "payer_ids": [Uuid::now_v7(), Uuid::now_v7()],
                "effective_from": "2099-01-01T00:00:00Z"
            })),
            vec![("idempotency-key", key)],
        ),
        ("POST", APPROVAL_REJECT) => (
            Some(serde_json::json!({ "reason": "not signed off" })),
            Vec::new(),
        ),
        // No body and no preconditions. `DELETE /price-windows/{windowId}` reaches
        // here **deliberately** rather than through an arm of its own: it carries
        // neither an `Idempotency-Key` nor an `If-Match`, which is §5's own
        // idempotency column for that surface, so an arm naming it would be
        // identical to this one and would only invite somebody to add a header to
        // it. The three read routes reach here for the same reason.
        _ => (None, Vec::new()),
    }
}

// ---------------------------------------------------------------------------
// 1. The census matches what the router registered.
// ---------------------------------------------------------------------------

/// Every operation the gear's routers register, keyed `METHOD:path`.
///
/// Built from the routers themselves, not from [`census`], because a census that
/// checked itself would stay green for exactly the route it was meant to catch:
/// one added to the registry and left out of the table. **Every** router and not
/// the two authoring ones, because a census built from a subset of the routers is
/// the same hole one level up — it was two routers until 2026-08-04, so the
/// set-level properties below ranged over 8 of the gear's routes while
/// `module.rs` claimed they ranged over all of them.
///
/// The list is nonetheless hand-written, because each router takes a differently
/// typed state and no loop can build them. That is the hole
/// [`every_mounted_router_is_merged_into_both_censuses`] closes: a new
/// `pub fn router` under `src/api/rest/**` fails that test until this function and
/// `module_test.rs`'s twin have both been told about it. It was open until
/// 2026-08-04 and was found by mounting a sixth router and watching **both**
/// censuses stay green.
async fn registered_paths() -> Vec<String> {
    use std::sync::Arc;
    use toolkit::api::OpenApiRegistryImpl;

    let harness = Harness::new().await;
    let openapi = OpenApiRegistryImpl::new();
    drop(
        bss_pricing::api::rest::frontier::router(Arc::clone(&harness.frontier), &openapi)
            // Slice 12's history read, in this census for the frontier's reason:
            // the census is about paths, and a router absent from it is a router
            // whose path nothing here checks.
            .merge(bss_pricing::api::rest::history::router(
                Arc::clone(&harness.history),
                &openapi,
            ))
            // Slice 5's Auditor read, in this census for the history read's reason.
            .merge(bss_pricing::api::rest::audit::router(
                Arc::clone(&harness.audit),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::bulk_imports::router(
                Arc::new(bss_pricing::api::rest::bulk_imports::ApiState {
                    authoring: Arc::clone(&harness.state),
                }),
                &openapi,
            ))
            // Slice 12's mass repricing, in this census for the bulk import's
            // reason: the census is about paths, and a router absent from it is a
            // router whose path nothing here checks.
            .merge(bss_pricing::api::rest::repricing_runs::router(
                Arc::new(bss_pricing::api::rest::repricing_runs::ApiState {
                    authoring: Arc::clone(&harness.state),
                    // The harness's own lane. This census refuses before any run is
                    // accepted, so nothing is ever enqueued on it — but a second
                    // lane here would be a second answer to which applier this
                    // router's accepted runs reach.
                    apply_lane: harness.apply_lane.clone(),
                }),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::plans::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::prices::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::overlays::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            // The submit route publishes (D-234), so it is mounted on the
            // governance state apart from its authoring siblings. It is in this
            // census because the census is about paths, and the path did not move.
            .merge(bss_pricing::api::rest::overlays::governance_router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::taxonomies::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::customer_groups::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::customer_groups::governance_router(
                Arc::clone(&harness.membership),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::tax_display_policy::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::rounding_policy::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::rounding_policies::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::gl_codes::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::catalog_skus::router(
                Arc::new(bss_pricing::api::rest::catalog_skus::ApiState {
                    catalog: Arc::new(
                        bss_pricing::domain::ports::UnconfiguredProductCatalogClientV1,
                    ),
                    source: "unconfigured",
                }),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::preview::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::bundles::router(
                Arc::clone(&harness.state),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::windows::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::supersessions::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::cutovers::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::retirement::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::migrations::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::migrated_origin_snapshots::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::approvals::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::publish::router(
                Arc::clone(&harness.governance),
                &openapi,
            ))
            .merge(bss_pricing::api::rest::threshold_policy::router(
                Arc::clone(&harness.governance),
                &openapi,
            )),
    );
    let mut paths: Vec<String> = openapi
        .operation_specs
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    paths.sort();
    paths
}

#[tokio::test]
async fn the_census_covers_every_route_the_routers_register() {
    // A route added later without a census row fails here, which is the whole
    // point of a census. `tests/module_test.rs` owns the wider property - that
    // the registered set is exactly the set `declared_paths()` names; this one
    // owns the narrower and more useful one, that every route the gear mounts has
    // a catalogued `(resource_type, action)` pair beside it.
    let registered = registered_paths().await;
    let mut expected: Vec<String> = census()
        .iter()
        .map(|route| format!("{}:{}", route.method, route.path))
        .collect();
    expected.sort();

    assert_eq!(
        registered, expected,
        "the routers register a path the census does not cover, or the other way round"
    );
    // **Four** labels and no fifth. This used to read "every surface in this group is
    // on the `plan` label", which stopped being true the day the approval surface
    // was mounted, "two labels and no third", which stopped being true the day the
    // threshold policy was, and "three and no fourth", which stopped being true the
    // day Slice 8 mounted the bundle authoring surface — so it is stated as the set
    // it is, and a census row on a label nobody decided about fails here rather than
    // reading as coverage.
    //
    // `bundle` is the fourth, and it is deliberately **not** `plan`: authoring a
    // composition is `bundle x write` (ProductManager, CatalogAdmin) while
    // publishing it is `plan x publish` **only** (D-11). Filing the authoring routes
    // under `plan` would hand composition authorship to every holder of
    // `plan x write`, and filing the publish route under `bundle` would take the
    // entrance away from FinanceManager, whom S8 §1.3 promises it. Neither is
    // visible to an allow/deny fixture, which is why both are asserted here.
    //
    // `approval_policy` is the third and it is deliberately **not** `config`: the
    // segregation of duties is the whole reason the catalog carries two labels, and
    // a policy route filed under `config` would hand a config admin the thresholds
    // that govern their own changes. That is invisible to every allow/deny fixture,
    // which is why it is asserted here.
    //
    // `audit` is the sixth, and it arrived as a correction rather than as a new
    // surface. `/history` -- the catalog audit trail -- was filed under `plan ×
    // read`, so every holder of catalog read could read who changed what and
    // when, while `audit_read` ("Read the catalog audit trail") was grantable and
    // conferred nothing.
    //
    // It was invisible to every allow/deny fixture AND to
    // `every_route_asks_the_catalogued_pair`, because that check compares a route
    // against this census and the census recorded the pair the route asked for.
    // Two descriptions of one implementation cannot disagree; what caught it was
    // `every_grantable_label_is_enforced_by_some_route`, which reads the
    // *grantable* catalog instead.
    //
    // **`historical_import` was a seventh member of this set and is not one now.**
    // The same 2026-08-14 wave that moved `/history` moved `/bulk-imports` off
    // `plan` and onto it, arguing that "a backdated import rewrites what the
    // catalog says was true in the past". S5 §3 never said that about these three
    // routes: its read row names `GET …/bulk-imports/{id}` under `plan × read`
    // and its bulk row files the mutating pair under the *same* `plan × write`
    // with "no new authority". `historical_import` belonged to the backdating
    // flow, which D-330 has since struck outright -- so the label is gone, and
    // the routes are back where the endpoint map always had them. The reading
    // this comment used to carry is withdrawn rather than edited around: what
    // caught the second error was the label guard again, from the other side.
    //
    // `config` is the fifth, mounted by Slice 4's taxonomy pair, and it is the
    // other side of that same separation. It is the label the segregation
    // argument above has always been *about* — until now the catalog declared
    // `config` and nothing was filed under it, so the sentence "a policy route
    // filed under `config` would hand a config admin the thresholds" named a
    // holder no route created. Now something does: a CatalogAdmin with
    // `config × write` declares regions, brands, partners and org tiers, and
    // holds nothing on `approval_policy`.
    let used: std::collections::BTreeSet<&str> =
        census().iter().map(|route| route.resource_type).collect();
    assert_eq!(
        used,
        std::collections::BTreeSet::from([
            labels::AUDIT,
            labels::PLAN,
            labels::BUNDLE,
            labels::PRICE_OVERLAY,
            labels::APPROVAL,
            labels::APPROVAL_POLICY,
            labels::CONFIG,
            // Slice 9's own taxonomy: `customer_group` gains its first route
            // consumer here — the permission pair already existed
            // (`gts/permissions.rs`) and gated nothing until now.
            labels::CUSTOMER_GROUP,
        ]),
        "a census row on a label this gear has not mounted before needs a decision, not a row"
    );
}

/// Every router the REST layer defines is merged into **every** place that has to
/// mount it — the production mount and the three test-side ones.
///
/// # The hole this closes, and how it was found
///
/// Four functions merge routers by hand, because each router takes a differently
/// typed state and no loop can build them: `module.rs`'s `register_rest` (the
/// production mount), `registered_paths` above, `module_test.rs`'s
/// `registered_operations`, and `rest_support`'s `Harness` app. Every set-level
/// property in this file and the whole route census in `module_test.rs` range over
/// what those functions build — so a router none of them merges is a router **no
/// census can see**, and the guards read as coverage while covering nothing.
///
/// That was live on 2026-08-04. A sixth router (`windows`) was mounted in
/// `module.rs` and the census in `module_test.rs` — the one whose doc promised "a
/// path registered without a line here fails the census" — **stayed green**,
/// because its own router list did not include it. The route was registered,
/// declared, catalogued, and invisible to both censuses; the harness did not mount
/// it either, so every request to it answered 404 without ever reaching the gate.
///
/// So the scan is over the **source text** of the four functions' files — the
/// `scannable()` text and not the raw bytes, see below. It cannot check that the
/// merge is correct — only that the name appears — which is exactly the guarantee
/// needed: it forces an author who adds a router to visit all four, and the
/// censuses then do the rest.
///
/// # It reads stripped source, and that was the second repair (Z12-8)
///
/// The scan read the raw file until 2026-08-14, so a merge **commented out**
/// still carried its own needle and the guard passed over the exact regression it
/// was written for. Its two sibling scans in this file had used `scannable()`
/// from the start and said why; this one was the holdout. Verified rather than
/// argued: commenting `module_test.rs`'s `taxonomies` merge out left the raw-text
/// version green and reddens this one, naming the file and the router.
///
/// # It keys on the function, not the module, and that was a repair
///
/// The scan used to select a file by `pub fn router(` and then look for
/// `rest::{module}::router(`. **One needle per module** — so a file's *second*
/// router was never given one, and `overlays::governance_router` (D-234's publish
/// route, the one mounted on `GovernanceState` apart from its siblings) was
/// outside the guard entirely. `api::rest::windows`' module doc had already
/// spelled out the rule this violates — *"a router named anything else is
/// precisely the hole this exists to close. One module, one router, one census
/// entry"* — and `windows` moved its whole module rather than grow a second
/// router for exactly that reason. `overlays` grew one anyway.
///
/// Nothing was broken when this was found: all four sites did merge it. But the
/// forcing function could not have told anyone otherwise, and the next second
/// router — mounted in `module.rs` alone, absent from `census()` and from
/// `registered_paths`, so the set-equality check compares two sets that both lack
/// it — would reproduce 2026-08-04 exactly, with every guard green. So the scan
/// now enumerates **every** `pub fn …router(` in each file and needles each by its
/// own name.
#[test]
fn every_mounted_router_is_merged_into_both_censuses() {
    let mounts = [
        "src/module.rs",
        "tests/rest_authz.rs",
        "tests/module_test.rs",
        "tests/rest_support/mod.rs",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    // `(module, function)`, because a module may declare more than one router and
    // the pair is what a mount site names. Keying on the module alone is the bug
    // this scan carried: see the doc above.
    let mut routers: Vec<(String, String)> = Vec::new();
    for path in rest_sources() {
        let text = std::fs::read_to_string(&path).expect("a readable REST source");
        let module = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a named source file");
        // `src/api/rest.rs` itself declares no router; every other file that does
        // is named for its module.
        for line in text.lines() {
            let Some(after) = line.trim_start().strip_prefix("pub fn ") else {
                continue;
            };
            // Stop at whichever comes first, so a future `router<S>(…)` is read as
            // `router` rather than silently skipped.
            let name = after
                .split(['(', '<'])
                .next()
                .unwrap_or_default()
                .trim_end();
            if !name.ends_with("router") {
                continue;
            }
            routers.push((module.to_owned(), name.to_owned()));
        }
    }
    routers.sort();
    // **The anti-vacuity floor, derived from the population it guards.** A
    // constant floor is satisfied by a parse that finds the same few files it
    // always found; what this scan must never do is *miss* a module — a router
    // mounted in production and invisible to every census. Every REST module that
    // declares an operation declares it on a
    // router, so the set of modules holding an `OperationBuilder` is what the scan
    // owes, and a module in one set and not the other is either a router this
    // parse cannot see or a parse that has stopped working.
    let declaring: std::collections::BTreeSet<String> = rest_sources()
        .iter()
        .filter(|path| scannable(path).contains("OperationBuilder::"))
        .map(|path| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .expect("a named source file")
                .to_owned()
        })
        .collect();
    let scanned: std::collections::BTreeSet<String> =
        routers.iter().map(|(module, _)| module.clone()).collect();
    assert_eq!(
        scanned, declaring,
        "every module that declares an operation declares a router this scan must see; the scan \
         found {routers:?}"
    );
    // A module with two routers is the case this scan exists to cover, so the
    // *pairs* must be distinct while the module names need not be.
    let distinct: std::collections::BTreeSet<&(String, String)> = routers.iter().collect();
    assert_eq!(
        distinct.len(),
        routers.len(),
        "the scan found the same (module, router) twice, which means the parse is wrong: {routers:?}"
    );

    for (module, function) in &routers {
        let needle = format!("rest::{module}::{function}(");
        for mount in mounts {
            // `scannable()` and not the raw text, for the reason its own doc gives
            // and which this scan was the last holdout against: a merge commented
            // out leaves the needle in the file, so the raw-text version passed
            // over precisely the 2026-08-04 regression the doc above recounts.
            // Probed by commenting `module_test.rs`'s `taxonomies` merge out: raw
            // text stayed green, this reddens.
            let text = scannable(&root.join(mount));
            assert!(
                text.contains(&needle),
                "{mount} does not merge the `{module}::{function}` router: `{needle}` appears nowhere in it, so every census and every gate property built there is blind to its routes"
            );
        }
    }
}

/// Every `.rs` file at or under `src/api/rest`, **recursively**.
///
/// Recursive because the earlier version of this scan read one directory level,
/// so any file in a future `src/api/rest/**` subdirectory evaded it entirely -
/// and a guard that a reorganisation switches off silently is a guard that reads
/// as coverage to everyone who greps for it.
/// One source split into `(function name, function body)`, so a property about a
/// **router** is asked of each router rather than of the file that holds them.
///
/// The same shape as `module_test`'s `function_texts`, kept local: that one keys
/// by name across the whole layer and would collapse the two `router` functions
/// this scan exists to tell apart.
fn function_chunks(text: &str) -> Vec<(String, String)> {
    let mut starts: Vec<usize> = Vec::new();
    let mut at = 0;
    while let Some(hit) = text[at..].find("fn ") {
        let start = at + hit;
        at = start + 3;
        // `fn` must stand alone, not be the tail of an identifier like `authz_fn`.
        if start > 0 && !text.as_bytes()[start - 1].is_ascii_whitespace() {
            continue;
        }
        starts.push(start);
    }
    let mut out = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        let body = &text[*start..end];
        let name: String = body["fn ".len()..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        out.push((name, body.to_owned()));
    }
    out
}

fn rest_sources() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/rest");
    let mut found = vec![root.with_extension("rs")];
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the REST layer is where it has always been") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// The source of `path` truncated at each line's first `//`, joined into one
/// string so a declaration split across lines is one subject.
///
/// Comments are stripped because this file's own prose names the type it bans, and
/// a scan that matched its own explanation would have to be weakened until it
/// matched nothing. **Attributes are not stripped**, so a needle chosen for a scan
/// over this text has to survive appearing inside a `#[serde(...)]` or
/// `#[schema(...)]` argument.
fn scannable(path: &std::path::Path) -> String {
    let stripped = std::fs::read_to_string(path)
        .expect("readable source")
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join(" ");
    normalized(&stripped)
}

/// One line, whitespace collapsed and **removed around every `::`**.
///
/// The path separator is where a formatter is free to break, so
/// `AccessScope\n    ::allow_all()` and `AccessScope::allow_all()` are one
/// construction written two ways. A scan that could not see that was one a
/// `cargo fmt` could switch off.
fn normalized(source: &str) -> String {
    source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ::", "::")
        .replace(":: ", "::")
}

#[test]
fn no_handler_can_build_an_access_scope_of_its_own() {
    // What makes "the authz gate before the repository" falsifiable rather than a
    // claim about source order.
    //
    // `every_mutating_route_is_denied_with_the_state_unchanged` proves no
    // **write** precedes the gate. It cannot prove that no **read** does, and a
    // read is what leaks a catalog the design set calls commercially sensitive:
    // a repository call taking `AccessScope::allow_all()` inserted above the PEP
    // call leaves every suite in this crate green.
    //
    // The real guarantee is structural - every repository method takes a
    // `&AccessScope`, and the only producer of one on a REST path is
    // `crate::authz::access_scope`, which *is* the gate.
    //
    // # What the earlier version of this scan could not see
    //
    // It matched the literal `AccessScope::` line by line over one directory
    // level, so four things evaded it: an alias (`use … AccessScope as Reach`), a
    // qualified call (`<AccessScope>::for_tenant`), a declaration split across
    // lines, and any file in a subdirectory. This one bans the **import** rather
    // than one spelling of the call: a file that cannot name the type in a value
    // position cannot construct one, whatever it calls it - and a type position
    // (`scope: &AccessScope`) needs the import too, which is why the test asserts
    // what the import is *used for* rather than that it is absent.
    let mut offenders: Vec<String> = Vec::new();
    let files = rest_sources();
    assert!(files.len() > 5, "the scan found almost nothing: {files:?}");

    for path in &files {
        let source = scannable(path);
        for pattern in access_scope_constructions(&source) {
            offenders.push(format!("{}: {pattern}", path.display()));
        }
    }

    assert!(
        offenders.is_empty(),
        "a handler that builds its own AccessScope has bypassed the PEP; the only producer on \
         a REST path is `crate::authz::access_scope`:\n{}",
        offenders.join("\n")
    );
}

/// Every `AccessScope` construction `source` contains, as the pattern that caught
/// it.
///
/// **One spelling, used by the scan and by the meta-test that falsifies it.** Two
/// copies of the pattern list are two rules: the meta-test proves a property of
/// its own copy, so a pattern deleted from the scan leaves it green and the guard
/// whose job is to keep the `AccessScope` ban falsifiable stops guarding the code
/// that enforces it.
///
/// The patterns are the three ways the type is named in a value position: a path
/// call, a qualified path call, and a struct literal. A file that cannot name the
/// type — under its own name or an alias — cannot construct one, which is why
/// [`access_scope_names`] is what bounds the search.
fn access_scope_constructions(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for alias in access_scope_names(source) {
        for pattern in [
            format!("{alias}::"),
            format!("<{alias}>::"),
            format!("{alias} {{"),
        ] {
            if source.contains(&pattern) {
                found.push(pattern);
            }
        }
    }
    found
}

/// Every name `source` can refer to `AccessScope` by — the type's own name, plus
/// whatever an `as` clause renamed it to.
///
/// A file that never imports the type at all yields nothing to look for, which is
/// correct: it cannot construct one.
fn access_scope_names(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    if !source.contains("AccessScope") {
        return names;
    }
    names.push("AccessScope".to_owned());
    // `use … AccessScope as Reach` — the alias is the token after `as`.
    let mut rest = source;
    while let Some(at) = rest.find("AccessScope as ") {
        let tail = &rest[at + "AccessScope as ".len()..];
        let alias: String = tail
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !alias.is_empty() {
            names.push(alias);
        }
        rest = tail;
    }
    names
}

#[test]
fn the_scan_would_catch_each_evasion_the_earlier_one_missed() {
    // A guard's own falsifiability. Each string below is a construction the
    // line-by-line literal scan let through, and each must be caught now -
    // otherwise the fix is a comment.
    for evasion in [
        "use toolkit_db::secure::AccessScope as Reach; let s = Reach::allow_all();",
        "let s = <AccessScope>::for_tenant(t);",
        "use toolkit_db::secure::AccessScope; let s = AccessScope\n    ::allow_all();",
        // The struct literal, which needs no associated function at all.
        "use toolkit_db::secure::AccessScope; let s = AccessScope { tenant_id: t };",
    ] {
        let joined = normalized(&evasion.lines().collect::<Vec<_>>().join(" "));
        assert!(
            !access_scope_constructions(&joined).is_empty(),
            "this evasion is not caught: {evasion}"
        );
    }
}

#[test]
fn the_scan_reads_more_than_one_directory_level() {
    // The other half of the earlier scan's blindness. `read_dir` is not
    // recursive, so a `src/api/rest/**` subdirectory was invisible - and this
    // asserts the walk descends rather than that it happens to find enough files.
    let files = rest_sources();
    assert!(
        files.iter().any(|path| path.ends_with("api/rest/plans.rs")),
        "the walk missed a file it must see: {files:?}"
    );
    assert!(
        files.iter().any(|path| path.ends_with("api/rest.rs")),
        "including the module root: {files:?}"
    );
}

/// The four verbs that make a `router()` a **mutating** one, in the spelling the
/// builder registers them under.
const MUTATING_BUILDERS: &[&str] = &[
    "OperationBuilder::post(",
    "OperationBuilder::patch(",
    "OperationBuilder::put(",
    "OperationBuilder::delete(",
];

/// The fifth verb, which is deliberately **not** in [`MUTATING_BUILDERS`].
///
/// The correlation-edge scan wants the four above and only those. The
/// authentication scan wants all five, because a read served to an anonymous
/// caller is the disclosure this gear's design set calls commercially sensitive —
/// so the two scans take the same list plus this.
const READ_BUILDER: &str = "OperationBuilder::get(";

#[test]
fn every_mutating_router_applies_the_correlation_edge() {
    // D-178's edge is applied inside each mutating router's own `router()` rather
    // than where the routers are merged, so that it travels with the routes. The
    // cost of that choice is that a **new** router can be written without it, and
    // nothing in the crate noticed: the route suites drive the four routers this
    // harness names, so a fifth would evade every one of them.
    //
    // The consequence is not a missing column. `require_correlation` runs at the
    // top of every handler, **before** the authz gate, so a route mounted without
    // the layer answers **500 to a caller who should have got 403** - the gate
    // never runs, and the refusal an operator sees is an internal fault.
    //
    // Scanned rather than driven, because the property is about a router that
    // does not exist yet. The two suites that could observe it - the census here
    // and `module_test`'s registered-path set - both start from a list somebody
    // maintains; this starts from the filesystem.
    // **Per router function, not per file.** The property is a router's, and two
    // files export two mutating routers each - `overlays.rs` and
    // `customer_groups.rs` both pair a `router()` with a `governance_router()`,
    // merged separately by `module.rs`. A file-scoped `source.contains` is
    // satisfied by *either* of them, so one router carrying the edge hid the
    // other, and the gear could have shipped a governance plane answering 500
    // where it owed 403 with this test green. Both carry it today; the defect was
    // in the guard.
    let files = rest_sources();
    assert!(files.len() > 5, "the scan found almost nothing: {files:?}");

    // Every public router entry point, mutating or not. The complement of
    // `mutating` in this is what the anti-vacuity assertion below is a roster of.
    let mut entry_points = Vec::new();
    let mut mutating = Vec::new();
    let mut offenders = Vec::new();
    for path in &files {
        let chunks = function_chunks(&scannable(path));
        // The unit is the **public router entry point** — `router`,
        // `governance_router` — because that is what `module.rs` merges and what
        // the layer is applied to. A private helper that mounts routes and hands
        // the `Router` back (`cutovers::mounted_horizon_door`,
        // `windows::mounted_window_mutations`) is covered by whichever entry point
        // calls it, so the scan follows one level of that call rather than
        // reporting the helper.
        for (name, body) in &chunks {
            if !(name == "router" || name.ends_with("_router")) {
                continue;
            }
            let mut reach = body.clone();
            for (helper, helper_body) in &chunks {
                if helper != name && body.contains(&format!("{helper}(")) {
                    reach.push_str(helper_body);
                }
            }
            let module = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("a named source file");
            entry_points.push(format!("{module}::{name}"));
            if !MUTATING_BUILDERS.iter().any(|verb| reach.contains(verb)) {
                continue;
            }
            // The full path for an offender, because that is what an author has to
            // open; the module for the rosters, because that is what a reader names.
            let site = format!("{}::{name}", path.display());
            mutating.push(format!("{module}::{name}"));
            // The edge itself must be on the entry point: a helper cannot apply a
            // layer to a router it has already returned.
            if !body.contains("correlation::establish") {
                offenders.push(site);
            }
        }
    }

    // **The anti-vacuity floor, derived from the population it guards.** A
    // constant `>= 6` is satisfied by a parse that keeps finding the same six, and
    // it says nothing about the routers it did not see. What is falsifiable is the
    // *complement*: every router entry point this gear declares registers a
    // mutating verb but one, so the non-mutating set is a roster — a router that
    // stopped registering one lands in it, and a scan that found nothing at all
    // renders it empty against a roster that is not.
    //
    // The entries are the read-only surfaces: nothing behind them writes an audit
    // record or an outbox row, so they have no field for a correlation to satisfy
    // and deliberately carry no edge. A name arriving here is a decision, not a
    // scan result — and the roster is transcribed rather than filtered by a
    // predicate over the module name, so a mutating router *renamed* to look
    // read-only still lands in it.
    let read_only: Vec<&str> = entry_points
        .iter()
        .filter(|site| !mutating.contains(site))
        .map(String::as_str)
        .collect();
    assert_eq!(
        read_only,
        [
            "audit::router",
            "catalog_skus::router",
            "frontier::router",
            "migrated_origin_snapshots::router",
            "preview::router",
        ],
        "the scan found the entry points {entry_points:?}, of which it read {mutating:?} as \
         mutating"
    );
    assert!(
        offenders.is_empty(),
        "a router registers a mutating operation and does not apply \
         `correlation::establish`; its handlers will answer 500 where they owe 403, and every \
         record they write would carry a per-record correlation (D-178):\n{}",
        offenders.join("\n")
    );
}

/// The wire token for the submit arm is spelled **once**, in `api::rest::publish`
/// (Z13-2).
///
/// `publish::OUTCOME_SUBMITTED` is `pub(crate)` and its own doc gives the rule in
/// as many words: *"the window surface answers the same word for the same act …
/// Two spellings of one outcome would make a client's `match` depend on which
/// route it called."* Six DTOs carry that word on a `pub outcome: String`, and
/// three of them used to build it from a literal of their own — `cutovers`,
/// `retirement` and `supersessions`.
///
/// # Why a scan and not an assertion over the responses
///
/// The suites already pin the literal on all five surfaces
/// (`rest_cutovers.rs`, `rest_retirement.rs`, `rest_supersessions.rs`), so every
/// one of them stays green when the const is renamed and three endpoints keep
/// answering the old word. A test comparing a response body against
/// `OUTCOME_SUBMITTED` would not see it either: both sides are the same string
/// today, which is precisely the drift that has not happened yet. What is
/// falsifiable is the **number of places the token is written**, so that is what
/// this checks.
///
/// # It scans `scannable()` and not the raw text
///
/// Five of these files explain the token in prose — this one included — and a
/// scan matching its own explanation would have to be weakened until it matched
/// nothing — which was
/// `every_mounted_router_is_merged_into_both_censuses`' own defect one property
/// over until Z12-8 routed it through `scannable()` too. Comments are stripped
/// first, so what is counted is code.
///
/// # `OUTCOME_PUBLISHED` is deliberately not in scope
///
/// `"published"` is two vocabularies wearing one spelling: the publish arm's
/// **outcome** token, and `LifecycleState::Published` / `OverlayLifecycle::Published`
/// as stored in a column. A scan over that word would drag the lifecycle
/// renderers into an outcome rule and be answered by weakening it. `submitted_for_approval`
/// belongs to the outcome vocabulary alone — no column in this gear holds it (the
/// approval register's own state token is `submitted`) — which is what makes this
/// one bannable outright.
#[test]
fn the_submit_outcome_token_is_written_in_exactly_one_module() {
    const TOKEN: &str = "\"submitted_for_approval\"";

    let sources = rest_sources();
    assert!(
        sources.len() > 5,
        "the scan found almost nothing: {sources:?}"
    );

    let mut spellers: Vec<String> = Vec::new();
    for path in &sources {
        if scannable(path).contains(TOKEN) {
            spellers.push(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .expect("a named source file")
                    .to_owned(),
            );
        }
    }
    spellers.sort();

    assert_eq!(
        spellers,
        vec!["publish.rs".to_owned()],
        "the submit outcome token is written outside `api::rest::publish`, where \
         `OUTCOME_SUBMITTED` is declared `pub(crate)` for exactly this reason. Import the const \
         (`crate::api::rest::publish::OUTCOME_SUBMITTED`, as `windows`, `overlays`, `bundles` and \
         `customer_groups` do) rather than re-spelling the word: a rename of the const otherwise \
         leaves these endpoints answering the old one, with every suite green because the suites \
         pin the literal too"
    );
}

// ---------------------------------------------------------------------------
// 1b. The gate that runs before every other one: authentication.
//
// Numbered out of order deliberately. `require_authenticated` is the FIRST thing
// every handler does — before the PDP call, before the repository — and it is the
// gate a per-route case cannot keep up with: a route mounted without one arrives
// with no unauthenticated proof and nothing says so. The sections below range over
// `census()` instead, so a new row is bound the moment it is added.
//
// Two halves, for the same reason section 1 has two: the behavioural half drives
// the census, and the source half starts from the filesystem so that a route
// whose census row does not exist yet is still bound.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unauthenticated_caller_is_refused_on_every_route() {
    // 401, and not 403: an anonymous caller is not a caller the PDP refused, and
    // a route that answered 403 here would be one whose gate ran on a context it
    // built for itself.
    //
    // # Why each case carries an authenticated twin
    //
    // `Harness::anonymous()` differs from `allowed()` in exactly one thing — no
    // security context — and its PDP is the same allowing one. That is what makes
    // the pairing sound: if the 401 came from anything else about the request (a
    // path template left unfilled, a body the extractor rejects, a precondition
    // header the route parses before the gate), the twin would fail too, and the
    // loop would be reporting a malformed request as a proof of authentication.
    // Thirteen authz scenarios in this crate were once simultaneously green and
    // untestable for want of that control, so it is not optional here.
    for route in census() {
        let harness = Harness::new().await;
        let seeded = seed(&harness).await;

        let anonymous = harness
            .anonymous()
            .send(drive(&route, &seeded, "\"0-3\"", "anonymous-key"))
            .await;
        assert_eq!(
            anonymous.status(),
            StatusCode::UNAUTHORIZED,
            "{} {} served a caller with no authenticated context",
            route.method,
            route.path
        );

        // The positive control. A different idempotency key, so the twin is a
        // request of its own and not a replay of one the gate refused.
        let authenticated = harness
            .allowed()
            .send(drive(&route, &seeded, "\"0-3\"", "authenticated-key"))
            .await;
        assert!(
            authenticated.status() != StatusCode::UNAUTHORIZED
                && authenticated.status() != StatusCode::FORBIDDEN,
            "{} {} refuses the authenticated twin with {}, so the 401 above is evidence about \
             the request and not about authentication",
            route.method,
            route.path,
            authenticated.status()
        );
    }
}

#[test]
fn every_registered_operation_has_a_handler_that_demands_a_context() {
    // The source half, and the one that binds a route whose census row nobody has
    // written yet.
    //
    // Counted rather than merely looked for, which is where this departs from
    // `every_mutating_router_applies_the_correlation_edge`. The correlation edge
    // is applied once **per router**, so presence of `correlation::establish`
    // anywhere in the file is the whole property. `require_authenticated` is
    // called once **per handler**, so a file that already contains one call
    // satisfies a presence scan for every route added to it afterwards — the exact
    // hole this gear grew, one router at a time. Equality against the number of
    // registered operations is the property at the granularity the call is made
    // at.
    //
    // What it cannot see is *which* handler is missing the call, only that one is;
    // `an_unauthenticated_caller_is_refused_on_every_route` above is the half that
    // names the route, for every route the census knows about.
    let builders: Vec<&str> = MUTATING_BUILDERS
        .iter()
        .copied()
        .chain(std::iter::once(READ_BUILDER))
        .collect();

    let mut registering = 0usize;
    let mut operations = 0usize;
    let mut offenders = Vec::new();
    for path in rest_sources() {
        let source = scannable(&path);
        let registered: usize = builders
            .iter()
            .map(|verb| source.matches(verb).count())
            .sum();
        if registered == 0 {
            continue;
        }
        registering += 1;
        operations += registered;
        // `require_authenticated(` and not the bare name: the import spells the
        // name too, and a file that imports it and calls it nowhere is the very
        // thing being looked for.
        let demanded = source.matches("require_authenticated(").count();
        if demanded != registered {
            offenders.push(format!(
                "{}: {registered} registered operation(s), {demanded} call(s) to \
                 require_authenticated",
                path.display()
            ));
        }
    }

    // Anti-vacuity, both dimensions: a scan that found no registering file, or one
    // that found files but no operations in them, would pass having measured
    // nothing. The floors are well under what the gear mounts, so they bound the
    // scan without pinning a count the next slice invalidates.
    assert!(
        registering >= 20,
        "the scan sees {registering} route-registering files; this gear has more"
    );
    assert!(
        operations >= 40,
        "the scan sees {operations} registered operations; this gear has more"
    );
    assert!(
        offenders.is_empty(),
        "a registered operation's handler does not demand an authenticated context, so it \
         serves an anonymous caller or builds a security context of its own; \
         `require_authenticated` is called once per handler and the counts must match:\n{}",
        offenders.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 2. A recording PDP: the pair each route actually asked about.
// ---------------------------------------------------------------------------

/// One question a route asks the PDP **after** its catalogued gate.
struct FurtherQuestion {
    /// The route that asks it — the census's own key.
    method: &'static str,
    /// The registered path template.
    path: &'static str,
    /// The label the question is about.
    resource_type: &'static str,
    /// The action the question is about.
    action: &'static str,
    /// Whether it demands a **constrained** allow.
    ///
    /// Not assumed, and that is the point of carrying it here: it is `true` for
    /// every gate in this gear and `false` for exactly one question, the
    /// withdraw's, deliberately — see the row. A property that asserted `true`
    /// across the board would be armed wider than the claim and would have to be
    /// weakened the first time it met the real code.
    require_constraints: bool,
}

/// Every question the census's routes ask after their gate, in the order asked.
///
/// Four routes ask a second, and until this table existed all four were bound by
/// nothing: the census records one pair per route and the property below read
/// `asked.first()`, so anything asked second could be about any label, any action,
/// and constrained or not, and stay green. That is the census's own failure mode
/// one position along.
///
/// A table beside the census rather than a field on [`Route`], because a stale row
/// here is *catchable* — `every_further_question_belongs_to_a_route_that_asks_one`
/// asserts both directions, where a per-row default on every census row would leave
/// a question that stopped being asked looking exactly like one never asked.
const FURTHER_QUESTIONS: &[FurtherQuestion] = &[
    // The three window mutations ask **their own pair twice**: once for the window
    // row, once for the plan whose coverage it moves. The same pair twice is still
    // two questions — an implementation that asked the second about `plan × read`
    // would let a caller who may only read a plan re-shape its coverage, and
    // nothing else in the crate looks at the second one.
    FurtherQuestion {
        method: "POST",
        path: PRICE_WINDOWS,
        resource_type: labels::PLAN,
        action: actions::WRITE,
        require_constraints: true,
    },
    FurtherQuestion {
        method: "PATCH",
        path: PRICE_WINDOW,
        resource_type: labels::PLAN,
        action: actions::WRITE,
        require_constraints: true,
    },
    FurtherQuestion {
        method: "DELETE",
        path: PRICE_WINDOW,
        resource_type: labels::PLAN,
        action: actions::WRITE,
        require_constraints: true,
    },
    // The horizon door, for the identical reason and with the identical shape: its
    // path addresses a price row, and the row read that resolves the plan needs a
    // scope of its own. Both questions are the catalogued pair and both are
    // constrained — the second is the gate on the plan whose published content
    // moves.
    FurtherQuestion {
        method: "PATCH",
        path: PRICE_GRANDFATHER_UNTIL,
        resource_type: labels::PLAN,
        action: actions::WRITE,
        require_constraints: true,
    },
    // The withdraw's, and the only question in the gear that is **not a gate**.
    // `inst-as-void` names the withdrawer as "the submitter (or a CatalogAdmin)",
    // and nothing at this layer can answer a question about roles, so
    // `CatalogAdmin` is asked as `plan × publish` — tenant-wide in *plan*
    // (`resource_id: None`), because the question is whether the principal is a
    // catalog authority at all and a denial narrows their authority to their own
    // units rather than refusing the request (`api::rest::approvals`, and
    // `rest_approvals.rs`'s bespoke pin of the same call).
    //
    // **`require_constraints: true`, and it read `false` until 2026-08-18.** The
    // old value was defended here as *"the only thing that makes this question safe
    // to leave unconstrained is that it does not gate"*. That was sound about the
    // risk it named — an unconstrained allow on a *gate* filters nothing — and
    // silent about the one that was live: `access_scope` conjoins its
    // tenant-membership assertion with the same flag, so `false` also disabled the
    // check on the `Some(subject_tenant)` the call passes one line down. A question
    // that does not gate can still hand out an authority, and this one hands out
    // `WithdrawAuthority::CatalogAuthority` — which `authorize_decision` accepts in
    // place of being the submitter. Tenant-wide in *plan* is not tenant-wide in
    // *tenant* (review T-1).
    //
    // The sequence is still compared as a whole, for the half of the old argument
    // that survives: if this question ever moved to first position it would be a
    // gate, and position is part of the claim.
    FurtherQuestion {
        method: "POST",
        path: APPROVAL_WITHDRAW,
        resource_type: labels::PLAN,
        action: actions::PUBLISH,
        require_constraints: true,
    },
];

/// The full sequence a route must ask — catalogued gate first, then its table
/// rows — as `(resource_type, action, require_constraints)` triples.
///
/// The gate's own `require_constraints` is `true` and is not table-driven: an
/// unconstrained allow on a gate is a scope that filters nothing, which is
/// section 5's whole subject.
fn expected_questions(route: &Route) -> Vec<(&'static str, &'static str, bool)> {
    let mut asked = vec![(route.resource_type, route.action, true)];
    asked.extend(
        FURTHER_QUESTIONS
            .iter()
            .filter(|q| q.method == route.method && q.path == route.path)
            .map(|q| (q.resource_type, q.action, q.require_constraints)),
    );
    asked
}

#[tokio::test]
async fn every_route_asks_the_catalogued_pair() {
    // The one property no allow/deny test can reach. A mutating route gated on
    // `plan x read` behaves identically under every fixture that grants both and
    // under every fixture that grants neither; only what it ASKED separates them.
    //
    // # Every question, not the first one
    //
    // This used to read `asked.first()` and assert three things about it, which
    // bound the first question of each route and nothing else. Four routes ask a
    // second (see [`FURTHER_QUESTIONS`]), and one of them — the withdraw — asks
    // about the publish entrance, the highest authority this gear gates on. The
    // sequence is now compared **whole and in order**, `require_constraints`
    // included, so a question added, removed, reordered or re-labelled fails here.
    //
    // # What it still cannot bind, and why that is not papered over
    //
    // A question behind a branch this seed does not take is never recorded, so it
    // is neither asserted nor catalogued here. The first **two** are
    // are a pair rather than the one this comment used to name: both
    // `POST /approvals/{approvalId}/approve` and `POST /approvals/{approvalId}/reject`
    // ask `plan × write` a second time when the unit they decided is a **repricing
    // run's batch unit** (`approvals::apply_approved_repricing_run` and
    // `approvals::advance_run_to_rejected`), and `seed` opens a plan-content unit.
    // Recording either needs a second seeded world rather than a table row — a row
    // for a question the drive cannot reach would fail the equality below on every
    // run.
    //
    // **Both are now pinned**, in
    // `rest_repricing_runs::both_batch_decisions_ask_approval_approve_then_a_fresh_plan_write`,
    // where the batch unit exists: the whole sequence in order,
    // `require_constraints` and `resource_id` included. The absence of a
    // `FURTHER_QUESTIONS` row for either route is what makes that case a
    // discrimination — this property pins *one* question on a plan-content unit and
    // that one pins *two* on a batch unit, so the second is provably the bulk arm's.
    // The reject half was not named as owed at all until 2026-08-20.
    // D-364 adds two more branch-only questions: taxonomy list/detail ask
    // approval-read only when pending taxonomy units exist. This seed has none.
    // `rest_taxonomies::taxonomy_pending_preview_tracks_multiple_proposals_and_atomic_apply`
    // pins config-read then one constrained approval-read for multiple units;
    // its sibling access tests cover denial, resource scopes and PDP outage.
    for route in census() {
        let harness = Harness::new().await;
        let seeded = seed(&harness).await;
        let (client, seen) = harness.recording();

        let response = client
            .send(drive(&route, &seeded, "\"0-3\"", "census-key"))
            .await;
        assert!(
            response.status() != StatusCode::UNAUTHORIZED
                && response.status() != StatusCode::FORBIDDEN,
            "{} {} was refused before it reached the gate",
            route.method,
            route.path
        );

        let asked = seen.lock().expect("recorder");
        let questions: Vec<(&str, &str, bool)> = asked
            .iter()
            .map(|request| {
                (
                    request.resource.resource_type.as_str(),
                    request.action.name.as_str(),
                    request.context.require_constraints,
                )
            })
            .collect();
        assert!(
            !questions.is_empty(),
            "{} {} asked the PDP nothing",
            route.method,
            route.path
        );
        assert_eq!(
            questions,
            expected_questions(&route),
            "{} {} does not ask the catalogued sequence of \
             (resource, action, require_constraints)",
            route.method,
            route.path
        );
    }
}

/// **The layer's one exception to gate-before-parse, pinned as the ordering it is
/// declared to be** (`api::rest`'s "Gate before parse, and the one stated
/// exception"; review 2026-08-19 on `windows.rs:1584`).
///
/// `GET /plans/{planId}/sellability` parses its three market parameters *before* it
/// asks the PDP, and every other handler in the layer does the opposite. Until this
/// case existed the ordering was asserted nowhere: `drive` above appends the market
/// so the census's routes all reach their gate, which means the census pins the
/// ordering's *consequence for well-formed requests* and says nothing about what a
/// malformed one does.
///
/// Both halves are here because the exception is only defensible as a pair. A
/// caller who sent the market and holds no `plan × read` grant is refused **403 by
/// the gate** — so the parse is not standing in front of the gate for authorized
/// requests — and a caller who omitted `at` is refused **400 with the PDP never
/// asked**, which is the exception itself. What that costs an attacker is nothing:
/// `market_of` reads the query and nothing else, so the 400 varies neither with
/// authority nor with whether the plan exists, and a caller probing their own
/// authority need only send a well-formed market, which is free.
///
/// To redden this: move `market_of(&query)?` below `access_scope` in
/// `get_plan_sellability`. The third block then answers 403 with one recorded
/// question instead of 400 with none — which is what the *rest* of the layer must
/// do, and what this route is argued out of.
#[tokio::test]
async fn the_sellability_exception_is_the_ordering_it_is_declared_to_be() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let path = PLAN_SELLABILITY.replace("{planId}", &plan_id.to_string());
    let market = format!("{path}?at=2099-01-01T00:00:00Z&currency=EUR&region=EU");

    let refused = harness.denied().send(request("GET", &market, None)).await;
    assert_eq!(
        refused.status(),
        StatusCode::FORBIDDEN,
        "a well-formed market reaches the gate, and the gate is what answers"
    );

    let (client, seen) = harness.recording();
    let answered = client.send(request("GET", &market, None)).await;
    // **The exact status, not the three-way exclusion it replaced.** `!= 401 && != 403
    // && != 400` is satisfied by a 500 and by a 404, so a `get_plan_sellability` that
    // gated correctly and then failed in the store proved this block's claim -- that
    // the parse is not standing in front of the gate for authorized requests -- about
    // the question list only, never about the caller being served (2026-08-21 review,
    // finding 8).
    assert_eq!(
        answered.status(),
        StatusCode::OK,
        "the well-formed read is answered, not merely un-refused: {}",
        answered.status()
    );
    let questions: Vec<(String, String, bool)> = seen
        .lock()
        .expect("recorder")
        .iter()
        .map(|request| {
            (
                request.resource.resource_type.clone(),
                request.action.name.clone(),
                request.context.require_constraints,
            )
        })
        .collect();
    assert_eq!(
        questions,
        vec![(labels::PLAN.to_owned(), actions::READ.to_owned(), true)],
        "one question, the catalogued pair, constrained"
    );

    let (client, seen) = harness.recording();
    let malformed = client
        .send(request(
            "GET",
            &format!("{path}?currency=EUR&region=EU"),
            None,
        ))
        .await;
    assert_eq!(
        malformed.status(),
        StatusCode::BAD_REQUEST,
        "an absent `at` names no market, so there is no subject for a gate to be about"
    );
    assert!(
        seen.lock().expect("recorder").is_empty(),
        "and the PDP is not asked at all - that is the exception, stated exactly"
    );
}

/// **A question is anchored to its own object's id or to nothing — never to some
/// other object's id.**
///
/// The census binds three of the gate's four PDP-visible arguments —
/// `resource_type`, `action` and `require_constraints` — and `resource_id` was not
/// one of them. Grepping this file for the word found three hits and all three were
/// **prose**: comments explaining that a collection read passes `None`. So the
/// census reasoned about the axis in exactly the place it declined to assert it,
/// and a route that anchored to the wrong object, or to a caller-supplied id, passed
/// every property in the file.
///
/// It was not hypothetical. `PATCH` and `DELETE /plans/{planId}/prices/{priceId}`
/// asked `plan × write` with the **price row's** id, `POST /bundles/{id}/publish`
/// asked `plan × publish` with the bundle's, and `DELETE /migrations/{id}` asked
/// `plan × migrate` with the migration's — while `windows.rs` states the rule at
/// length and restructured three handlers rather than break it. A mis-anchored id
/// cannot widen a scope (`resource_id` is an *input* to the decision, and an unknown
/// property compiles fail-closed), so the direction is availability: a role
/// definition constraining `plan × write` to one plan denies against a price row's
/// id. It arms the day such a definition is written, which
/// `authz::SUPPORTED_PROPERTIES` advertises the property precisely so one can be.
///
/// # Why this shape rather than an `Anchor` per census row
///
/// A hand-decided anchor per census row is one chance per row to transcribe the
/// code rather than the contract — and a roster read off the implementation binds
/// nothing, which is the largest recurring class of finding on this surface. What is asserted instead is the *invariant*, per **label**: the id a
/// `plan` question carries must be a plan's, a `bundle` question's the bundle's, a
/// `price_overlay` question's the overlay's, an `approval` question's the unit's.
/// Every seeded id in the fixture is a distinct `Uuid`, so anchoring to a price row,
/// a window, a bundle, an overlay, an approval, a migration or a bulk run each fails
/// here, and no row has to say so. For the labels this fixture seeds no object of —
/// `customer_group`, `config`, `approval_policy`, `audit` — the weaker half still
/// holds: the anchor may not be any *other* seeded object's id.
#[tokio::test]
async fn a_question_is_never_anchored_to_another_objects_id() {
    for route in census() {
        let harness = Harness::new().await;
        let seeded = seed(&harness).await;
        let (client, seen) = harness.recording();

        let response = client
            .send(drive(&route, &seeded, "\"0-3\"", "census-anchor"))
            .await;
        assert!(
            response.status() != StatusCode::UNAUTHORIZED
                && response.status() != StatusCode::FORBIDDEN,
            "{} {} was refused before it reached the gate",
            route.method,
            route.path
        );

        let asked = seen.lock().expect("recorder");
        for request in asked.iter() {
            // **Every label, not only `plan`.** This loop skipped every question
            // whose `resource_type` was not `labels::PLAN` until 2026-08-20, so the
            // anchors of the `bundle`, `price_overlay`, `approval`,
            // `customer_group`, `config`, `approval_policy` and `audit` questions
            // were asserted by nothing -- the same mis-anchoring on a
            // `bundle x read` or an `approval x approve` question was invisible,
            // even though `Seeded` already carries a distinct uuid per object type.
            let label = request.resource.resource_type.as_str();
            let expected = expected_anchor(label, &seeded);
            let Some(anchored) = request.resource.id.as_ref() else {
                // `None` is always legitimate: a collection read, or a batch whose
                // rows span objects and whose ids it mints. What is refused is
                // anchoring to the *wrong* thing.
                continue;
            };
            let Some(expected) = expected else {
                // A label this fixture seeds no object for (`customer_group`,
                // `config`, `approval_policy`, `audit`): there is nothing to
                // compare an anchor against, and inventing one would be
                // transcribing the implementation. What *is* asserted is that the
                // id is none of the **other** seeded objects' -- a question anchored
                // to the plan, the price row, the window, the bundle, the overlay
                // or the approval unit is anchored to an object of the wrong type
                // whatever its own label.
                let anchored = anchored.to_string();
                let stranger = seeded_ids(&seeded)
                    .into_iter()
                    .find(|(_, id)| *id == anchored);
                assert!(
                    stranger.is_none(),
                    "{} {} asks a `{label}` question anchored to the seeded {} -- an object of \
                     another type",
                    route.method,
                    route.path,
                    stranger.map(|(name, _)| name).unwrap_or_default()
                );
                continue;
            };
            assert_eq!(
                anchored.to_string(),
                expected,
                "{} {} asks a `{label}` question anchored to `{anchored}`, which is not the \
                 seeded {label} object {expected}. The authz catalog's endpoint map puts this \
                 action on that object, so a role definition constraining it by `resource_id` is \
                 evaluated against an object of the wrong type and denies",
                route.method,
                route.path,
            );
        }
    }
}

/// The object a question carrying `label` must be anchored to, when this fixture
/// seeds one.
///
/// `None` means "this fixture seeds no object of that type", not "any id will do" —
/// the caller still refuses an anchor that names one of the *other* seeded objects.
fn expected_anchor(label: &str, seeded: &Seeded) -> Option<String> {
    match label {
        labels::PLAN => Some(seeded.plan.to_string()),
        labels::BUNDLE => Some(seeded.bundle.to_string()),
        labels::PRICE_OVERLAY => Some(seeded.overlay.to_string()),
        labels::APPROVAL => Some(seeded.approval.to_string()),
        _ => None,
    }
}

/// Every object the fixture seeds, by name, so a wrong anchor can be named rather
/// than merely refused.
fn seeded_ids(seeded: &Seeded) -> Vec<(&'static str, String)> {
    vec![
        ("plan", seeded.plan.to_string()),
        ("price row", seeded.price.to_string()),
        ("approval unit", seeded.approval.to_string()),
        ("window", seeded.window.to_string()),
        ("bundle", seeded.bundle.to_string()),
        ("overlay", seeded.overlay.to_string()),
    ]
}

#[test]
fn every_further_question_belongs_to_a_route_that_asks_one() {
    // The table's own staleness guard — `no_exemption_outlives_the_surface_it_was_
    // waiting_for`'s shape, one construct over.
    //
    // The forward direction (a question asked and not catalogued) is enforced by
    // the sequence equality above and needs nothing here. What needs a test is a
    // row for a route that no longer exists: `expected_questions` only ever fires
    // on a route it matches, so such a row is compared against nothing and reads
    // as coverage to anyone who greps for it.
    let routes: std::collections::BTreeSet<(&str, &str)> =
        census().iter().map(|r| (r.method, r.path)).collect();

    let orphans: Vec<&str> = FURTHER_QUESTIONS
        .iter()
        .filter(|q| !routes.contains(&(q.method, q.path)))
        .map(|q| q.path)
        .collect();
    assert!(
        orphans.is_empty(),
        "these rows catalogue a further PDP question for a route the census does not have, so \
         nothing compares them to anything: {orphans:?}"
    );

    // Anti-vacuity. An empty table makes the sequence property above assert
    // exactly what `asked.first()` did, and it would do it silently.
    assert!(
        FURTHER_QUESTIONS.len() >= 4,
        "the table has shrunk below the four routes known to ask a further question; if a route \
         stopped asking, that is a decision and belongs in a comment beside it"
    );
    // **This assertion has inverted, and the inversion is the record of T-1.** It
    // required that *some* row be unconstrained — the withdraw's authority
    // question was the one, and the flag was carried per row to hold that single
    // variation. Its own message said: *"if the withdraw's authority question
    // became a gate, that is the finding."* It did, on 2026-08-18: `false` was not
    // only accepting an allow that filters nothing, it was disabling
    // `access_scope`'s tenant-membership check, and a catalog authority compiled to
    // another tenant was closing units here.
    //
    // So the invariant replaces the variation, and it is the stronger claim:
    // `authz`'s rule is unconditional — `require_constraints` must be `true` on
    // **every** authorizing path. The field stays per row rather than being
    // dropped for a constant, because it is compared against what the PDP was
    // *actually* asked (`request.context.require_constraints`, below), so it still
    // catches a route that starts passing `false`. What it no longer carries is
    // disagreement among the expectations.
    assert!(
        FURTHER_QUESTIONS.iter().all(|q| q.require_constraints),
        "a further question passing `require_constraints: false` disables the tenant-membership \
         assertion in `access_scope` as well as accepting a constraint-free allow; `authz`'s rule \
         admits no exception, and the withdraw route is why"
    );
}

// ---------------------------------------------------------------------------
// 3. Denied, and untouched.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_mutating_route_is_denied_with_the_state_unchanged() {
    // "The authz gate before the repository" is a claim about source order until
    // somebody looks at the store. A handler that wrote first and checked second
    // would produce the same 403.
    for route in census().into_iter().filter(|route| route.mutating) {
        let harness = Harness::new().await;
        let seeded = seed(&harness).await;
        let plans_before = plan_count(&harness).await;
        let version_before = plan_row_version(&harness, seeded.plan, 0).await;
        let prices_before = price_rows(&harness, seeded.plan).await;
        // The approval plane's own readback. Without it the three decision routes
        // would be in this loop asserting nothing about themselves: a handler that
        // decided the unit and then checked the gate moves no plan row and no
        // price row, so every assertion below would hold.
        let unit_before = approval_row(&harness, seeded.approval).await.state;
        // Every other plane a census route can write, over `entity::MODULES`.
        // Most mutating routes write a plane outside the plan, price and approval
        // readbacks above, so without this a handler that wrote first and gated
        // second moved none of the three and this loop stayed green.
        let stores_before = mutable_planes(&harness).await;
        // The plane-level twin of the `unit_before` anti-tautology assertion
        // below. `approval` was added as a whole plane on 2026-08-18 because the
        // single-field readback could not see a refusal that OPENS A NEW UNIT --
        // two mutating census routes do exactly that on success -- and a plane
        // that happened to be empty would compare `[] == []` for every route and
        // prove nothing at all.
        assert!(
            !stores_before["approval"].is_empty(),
            "the seed must leave an approval unit, or the approval plane's comparison is \
             vacuous for every route in this loop"
        );

        let response = harness
            .denied()
            .send(drive(&route, &seeded, "\"0-3\"", "denied-key"))
            .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{} {}",
            route.method,
            route.path
        );
        assert_eq!(
            plan_count(&harness).await,
            plans_before,
            "a row was created"
        );
        assert_eq!(
            plan_row_version(&harness, seeded.plan, 0).await,
            version_before,
            "a version moved"
        );
        let prices_after = price_rows(&harness, seeded.plan).await;
        assert_eq!(prices_after.len(), prices_before.len(), "a price row moved");
        // Every plane, whole. The per-plane comparison below subsumes the head-only
        // price check this used to make, and it is asserted plane by plane rather
        // than as one map so a failure names which store the denied call reached.
        let stores_after = mutable_planes(&harness).await;
        for (plane, before) in &stores_before {
            assert_eq!(
                stores_after.get(plane),
                Some(before),
                "{} {} was refused and still wrote the {plane} plane",
                route.method,
                route.path
            );
        }
        assert_eq!(
            approval_row(&harness, seeded.approval).await.state,
            unit_before,
            "a refused decision decided the unit anyway"
        );
        assert_eq!(
            unit_before,
            ApprovalState::Submitted,
            "the seeded unit must be pending, or `unit_before` is a tautology"
        );
    }
}

/// **Every route, not only the mutating ones: a PDP `decision: false` is a 403.**
///
/// `every_mutating_route_is_denied_with_the_state_unchanged` is the only
/// census-driven case that drives `harness.denied()`, and it filters `census()` to
/// `route.mutating` — so until 2026-08-20 none of the rows carrying
/// `mutating: false` had any proof that a denial becomes a refusal.
///
/// A read handler that consumed the PDP answer and rendered a denial as an empty
/// `200` instead would satisfy `every_route_asks_the_catalogued_pair` (it still
/// asks) and `a_pdp_outage_fails_closed_on_every_route` (an outage is an `Err`, not
/// a deny), and roughly twenty read routes — the overlay list, the taxonomy and
/// policy reads, preview, coverage, sellability, the window list, the migration
/// reads, the repricing-run read, the plan and bundle collections, the group-members
/// list — had no bespoke deny case anywhere in the crate either.
///
/// The state-unchanged half stays where it is: it needs the store snapshots, and a
/// read has nothing to snapshot. This case is the status half over the whole set.
///
/// **One harness for the whole census, deliberately.** The state-unchanged property
/// is what needs a clean world per route, and it has one; this case asserts only the
/// status, which no earlier route's denial can move — a denial that *did* write is
/// caught by the sibling above, on its own fresh fixture. One migrated database per
/// census row for a status assertion would be tens of seconds of gate for nothing.
#[tokio::test]
async fn every_route_is_denied_with_a_403() {
    let harness = Harness::new().await;
    let seeded = seed(&harness).await;

    for route in census() {
        let response = harness
            .denied()
            .send(drive(&route, &seeded, "\"0-3\"", "denied-all-key"))
            .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{} {} did not refuse a PDP denial",
            route.method,
            route.path
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Unavailable fails closed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pdp_outage_fails_closed_on_every_route() {
    // A policy engine that cannot answer must never degrade into an allow, and
    // the caller must be told to retry rather than told it is forbidden.
    //
    // # Each 503 is paired with a working-PDP twin
    //
    // Asserting `SERVICE_UNAVAILABLE` alone is satisfied by a route that answers
    // 503 for a reason of its own — an unconfigured downstream port, a store fault
    // reachable from the seeded world — while never consulting the PDP at all. The
    // nearest control, `every_route_asks_the_catalogued_pair`, only asserts the
    // status is neither 401 nor 403, and a 503 satisfies that. So the identical
    // request is driven under `allowed()` in the same iteration and must **not** be
    // 503, the way the 401 case pairs each refusal with an authenticated twin.
    for route in census() {
        let harness = Harness::new().await;
        let seeded = seed(&harness).await;

        let response = harness
            .unavailable()
            .send(drive(&route, &seeded, "\"0-3\"", "outage-key"))
            .await;

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{} {} must fail closed, not open",
            route.method,
            route.path
        );

        // The twin. A **fresh** harness only where it is needed: a mutating route's
        // outage call is the one that could have left the world altered, and a read's
        // provably could not -- so the reads reuse the fixture and the writes do not.
        let twin_harness;
        let twin_seeded;
        let (twin_host, twin_world) = if route.mutating {
            twin_harness = Harness::new().await;
            twin_seeded = seed(&twin_harness).await;
            (&twin_harness, &twin_seeded)
        } else {
            (&harness, &seeded)
        };
        let twin = twin_host
            .allowed()
            .send(drive(&route, twin_world, "\"0-3\"", "twin-key"))
            .await;
        assert_ne!(
            twin.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{} {} answers 503 under a **working** PDP too, so the outage assertion \
             above proves nothing about failing closed",
            route.method,
            route.path
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Constraints are required.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unconstrained_allow_is_refused_on_a_read_and_on_a_write() {
    // An allow with no constraints compiles to a scope that filters nothing,
    // which is every tenant's price book. `require_constraints = true` is what
    // makes that a denial rather than an exposure.
    let harness = Harness::new().await;
    let seeded = seed(&harness).await;

    let read = harness
        .unconstrained()
        .send(request("GET", &format!("{PLANS}/{}", seeded.plan), None))
        .await;
    let write = harness
        .unconstrained()
        .send(with_headers(
            "POST",
            PLANS,
            Some(serde_json::json!({ "plan_tier": "gold" })),
            &[("idempotency-key", "unconstrained")],
        ))
        .await;

    assert_eq!(read.status(), StatusCode::FORBIDDEN);
    assert_eq!(write.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        plan_count(&harness).await,
        1,
        "the unconstrained write must have created nothing"
    );
}

// ---------------------------------------------------------------------------
// 6. Cross-tenant, both directions.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_foreign_tenants_plan_reads_exactly_like_an_absent_one() {
    let harness = Harness::new().await;
    let seeded = seed(&harness).await;

    let baseline = harness
        .allowed()
        .send(request("GET", &format!("{PLANS}/{}", seeded.plan), None))
        .await;
    assert_eq!(
        baseline.status(),
        StatusCode::OK,
        "without the owner's 200 the 404 below would be consistent with mere absence"
    );

    let foreign = harness
        .other_tenant()
        .send(request("GET", &format!("{PLANS}/{}", seeded.plan), None))
        .await;
    let random = harness
        .other_tenant()
        .send(request("GET", &format!("{PLANS}/{}", Uuid::now_v7()), None))
        .await;

    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    assert_eq!(random.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(foreign).await.get("type"),
        body_json(random).await.get("type"),
        "the two answers must be indistinguishable, or the surface leaks whose catalog exists"
    );
}

/// [`Seeded`] with every id replaced by a fresh one, so the absent arm below goes
/// through **the same** [`drive`] the other two do.
///
/// Hand-building the path was the first attempt and it was wrong: `drive` also
/// supplies the query parameters several reads require, so a hand-rolled path
/// answered `400` for a missing parameter and the property reddened on its own
/// request rather than on the surface. Same code, different ids.
fn absent_ids(seeded: &Seeded) -> Seeded {
    let mut fresh = seeded.clone();
    fresh.plan = Uuid::now_v7();
    fresh.price = Uuid::now_v7();
    fresh.approval = Uuid::now_v7();
    fresh.window = Uuid::now_v7();
    fresh.bundle = Uuid::now_v7();
    fresh.overlay = Uuid::now_v7();
    "dev-local-absent-v1".clone_into(&mut fresh.pending_ref);
    "absent-value".clone_into(&mut fresh.brand_value);
    fresh
}

/// **A foreign tenant's object reads exactly like an absent one — on every route
/// that names one** (review T-5).
///
/// This property existed for `GET /plans/{planId}` alone, while the four beside it
/// range over `census()`. That is this file's own stated hazard, applied to
/// itself: *"a per-route suite grows a hole the moment somebody adds a route and
/// forgets its test; a census driven from the registered path set fails instead."*
/// The by-id reads with no cross-tenant case at all included the migration item,
/// the repricing run, the bulk import, the migrated-origin snapshot, the window
/// list, the group members and the overlay list.
///
/// **Structurally the gear is sound here today**, which is why this is coverage
/// rather than a defect: every repository call takes an `&AccessScope` and
/// `no_handler_can_build_an_access_scope_of_its_own` bans a handler minting one. It
/// is filed because the doctrine is that a per-route proof of a set-level property
/// is a hole waiting for the next route — and the shape it would take is a handler
/// that resolves its path object *before* narrowing, then reports a typed refusal
/// naming the row's own state: `409 NOT_SUPERSEDABLE` for another tenant's price
/// id and `404` for a random one is a probe for which uuids name real rows.
///
/// **Status and the problem `type`, not merely "both are 4xx".** Comparing status
/// alone would pass a pair of 403s that differ in their code, which is exactly the
/// distinguishing signal this forbids. The owner's 2xx is the anti-tautology
/// control: without it, a route that refused everyone would satisfy the case.
///
/// **Which routes are actually under it** is now stated rather than implied. The
/// loop was filtered on `route.path.contains('{')` and `continue`d silently whenever
/// the owner's read was not a success, so seven of the census's by-id reads never
/// reached an assertion — four of them the very routes the doc above names as having
/// had no cross-tenant case — and two more compared a request with itself. Both
/// escapes are now failures unless the route is listed in
/// [`BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE`] with its reason, and a listed route
/// that becomes coverable fails too.
/// The path segments [`absent_ids`] actually varies.
///
/// A route whose template carries **none** of these builds the *same* request for
/// the `foreign` and the `absent` arm — byte for byte — so the loop below would
/// compare a request with itself and both assertions would hold by construction.
/// The filter was `route.path.contains('{')` until 2026-08-20, which admitted
/// `GET /taxonomies/{class}` and `GET /customer-groups/{group}/members`, whose only
/// parameter `drive` fills with the fixed literals `brand` and `gold`: two routes
/// read as cross-tenant coverage where none was measured.
const VARIED_SEGMENTS: &[&str] = &[
    "{planId}",
    "{priceId}",
    "{approvalId}",
    "{windowId}",
    "{bundleId}",
    "{overlayId}",
    "{pendingRef}",
    "{value}",
];

/// By-id reads this property cannot cover, and **why**, one row per route.
///
/// An exemption is a debt, `LABELS_WITHOUT_A_SURFACE_YET`'s discipline applied to a
/// census loop. Two shapes land here:
///
///   * the route's only path parameter is a fixed literal `drive` supplies and
///     `absent_ids` does not vary (`{class}`, `{group}`, `{migrationId}`,
///     `{operationId}`, `{runId}`, `{subscriptionRef}`), so the `foreign` and
///     `absent` requests are identical;
///   * the fixture seeds no row the segment names, so the **owner's** read answers
///     404 and there is no control.
///
/// The loop used to `continue` silently on the second shape, so a route that
/// stopped being readable simply left the property — and the routes the property
/// was written for (the migration item, the repricing run, the bulk run, the
/// migrated-origin snapshot) were skipped on every run while the property read as
/// closed. Now a skip that is not listed here fails, and a listed route that
/// *becomes* coverable fails too.
///
/// **What is still owed** is the fixture, not the assertion: seeding a bulk run, a
/// repricing run, a migration schedule and a migrated-origin snapshot — and adding
/// their ids to [`Seeded`] so `absent_ids` can vary them — would empty most of this
/// list. That is a change to the world every property in this file is driven
/// against, so it is a group of its own rather than a line here.
///
/// **A listed row is not an uncovered route.** What an exemption records is that
/// *this* fixture cannot stage the comparison; four of these are covered by a
/// bespoke case in the suite that owns the object, where the fixture already builds
/// one — `rest_bulk_imports::a_foreign_tenants_run_reads_like_an_unknown_one`,
/// `rest_repricing_runs::a_foreign_tenants_run_reads_like_an_unknown_one`,
/// `rest_migrations::a_foreign_tenant_is_refused_like_an_unknown_migration_and_the_owner_is_not`
/// and `rest_preview::a_foreign_tenants_plan_previews_like_an_absent_one`. They are
/// not struck here: the loop asserts a listed route that becomes coverable *by this
/// fixture*, and striking one it still cannot stage turns a truthful exemption into
/// a red case.
///
/// The three without a bespoke twin: `{class}` and `{group}` have no absent state
/// to compare against anywhere, so their cross-tenant claim is the emptiness of the
/// *body* — a different property, and their own suites' — and
/// `GET /migrated-origin-snapshots/{ref}` needs a synthesized snapshot no suite
/// builds.
const BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE: &[(&str, &str)] = &[
    // The four by-id reads the property's own doc names as having had no
    // cross-tenant case: `drive` fills their segment with a fixed literal nothing
    // points at, so the `foreign` and `absent` requests are the same bytes and the
    // owner answers 404 either way. These are the rows the seeding debt above buys.
    ("GET", BULK_IMPORT),
    ("GET", REPRICING_RUN),
    ("GET", MIGRATION_BY_ID),
    ("GET", MIGRATED_ORIGIN_SNAPSHOT),
    // `{class}` and `{group}` are **resource selectors**, not ids: `drive` fills
    // them with `brand` and `gold` (its own comments say why), and `absent_ids` has
    // nothing to vary. A taxonomy class that does not exist is a 400 rather than a
    // 404, so "absent" is not a state these surfaces have — the cross-tenant claim
    // for them is the emptiness of the *body*, which is a different property and
    // belongs to their own suites.
    ("GET", TAXONOMY),
    ("GET", CUSTOMER_GROUP_MEMBERS),
    // The preview read answers **400** to its owner here: §5 requires `currency`
    // and `region` and `drive` supplies query parameters only for the sellability
    // route, so this fixture drives the preview with a request it refuses before it
    // reads anything. Recorded rather than papered over — the other census
    // properties are asserted against that 400 too, which is sound for them (each
    // only needs the gate to run first) and is why it went unnoticed.
    ("GET", PLAN_PREVIEW),
];

#[tokio::test]
async fn a_foreign_tenants_object_reads_like_an_absent_one_on_every_by_id_read() {
    for route in census()
        .into_iter()
        .filter(|route| !route.mutating && route.path.contains('{'))
    {
        let exempt = BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE.contains(&(route.method, route.path));
        if !VARIED_SEGMENTS
            .iter()
            .any(|segment| route.path.contains(segment))
        {
            assert!(
                exempt,
                "{} {} carries no segment `absent_ids` varies, so its `foreign` and `absent` \
                 requests are byte-identical and this loop would compare a request with itself. \
                 Vary the segment, or list the route in \
                 BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE with the reason",
                route.method, route.path
            );
            continue;
        }

        let harness = Harness::new().await;
        let seeded = seed(&harness).await;

        let owner = harness
            .allowed()
            .send(drive(&route, &seeded, "\"0-3\"", "owner-key"))
            .await;
        let foreign = harness
            .other_tenant()
            .send(drive(&route, &seeded, "\"0-3\"", "foreign-key"))
            .await;
        let absent = harness
            .other_tenant()
            .send(drive(&route, &absent_ids(&seeded), "\"0-3\"", "absent-key"))
            .await;

        // The control. A route the owner cannot read either says nothing about
        // whether the two refusals below are distinguishable -- but it is an
        // **exemption**, not a silent skip: a route that stops being readable has
        // left the property, and that is a finding rather than a shrug.
        if !owner.status().is_success() {
            let status = owner.status();
            assert!(
                exempt,
                "{} {}: the owner's read answered {status}, so there is no control and this \
                 route is not under the property. Seed the row it names, or list it in \
                 BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE with the reason",
                route.method, route.path
            );
            continue;
        }
        assert!(
            !exempt,
            "{} {} is listed in BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE and is now coverable: \
             the owner reads it and `absent_ids` varies its id. Strike the row",
            route.method, route.path
        );

        let foreign_status = foreign.status();
        let absent_status = absent.status();
        assert_eq!(
            foreign_status, absent_status,
            "{} {}: a foreign tenant's object answers {foreign_status} and an absent one \
             {absent_status}; the difference is a probe for which ids name real rows",
            route.method, route.path
        );
        assert_eq!(
            body_json(foreign).await.get("type"),
            body_json(absent).await.get("type"),
            "{} {}: the two refusals must be indistinguishable in the problem document too, \
             or the code distinguishes what the status does not",
            route.method,
            route.path
        );
    }
}

/// Mutating by-id routes whose cross-tenant refusal this fixture cannot stage,
/// and **why**, one row per route.
///
/// [`BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE`]'s discipline on the write half: an
/// exemption is a debt and a listed route that becomes coverable fails too, so
/// the list can only shrink by being paid.
const BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE: &[(&str, &str)] = &[
    // `drive` fills these segments with a fixed literal `absent_ids` does not
    // vary — an operation id, a run id, a migration id, a taxonomy class, a group
    // and a payer — so the `foreign` and the `absent` request are the same bytes
    // and this loop would compare a request with itself. `{class}` and `{group}`
    // are resource *selectors* rather than ids and have no absent state at all;
    // the rest are ids the fixture seeds no row for, which is the seeding debt
    // `BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE` records for their read halves.
    //
    // **A row here is a debt of this fixture, not an uncovered route, and none of
    // them is strikeable.** The loop below asserts `exempt` for a route whose
    // segments `VARIED_SEGMENTS` does not carry, and again for one whose owner this
    // seed cannot drive to a success — so striking a row that a *sibling* suite
    // pays turns a truthful exemption into a red case. Every row therefore names
    // where its cross-tenant property lives, and what remains owed here is the
    // fixture.
    //
    // Paid in
    // `rest_bulk_imports::a_foreign_tenants_run_cannot_be_aborted_and_the_owners_identical_call_can`
    // and its `rest_repricing_runs` twin, each staging a real `committing` run,
    // driving the abort as `other_tenant()` and reading the run back off the store
    // against a positive control. What the sibling cases buy is that the sweep —
    // which releases another tenant's row locks and decides its rows — is not
    // resting on an exemption.
    ("POST", BULK_IMPORT_ABORT),
    ("POST", REPRICING_RUN_ABORT),
    // Paid in `rest_migrations::a_foreign_tenant_is_refused_like_an_unknown_migration_and_the_owner_is_not`,
    // whose fixture schedules a real migration and varies the id.
    ("DELETE", MIGRATION_BY_ID),
    // **A selector rather than an id, so there is no by-id property to pay
    // anywhere.** `{class}` and `{group}` name a taxonomy and a group rather than a
    // row and have no absent state a foreign answer could be compared against, and
    // `{payerId}` has none either: the move *onboards* a payer it finds no
    // membership for rather than refusing one (`rest_customer_groups::
    // a_first_enrollment_landing_now_is_onboarding_and_still_lands`). The
    // cross-tenant claim on these is that a foreign caller's write lands in the
    // foreign caller's own tenant — the emptiness of a body, which is a different
    // property and their own suite's.
    // D-353's per-value taxonomy routes (the whole-set `PUT` that stood here is
    // removed). The `POST` names `{class}` alone — a resource selector, above — so
    // its cross-tenant claim is the same emptiness-of-body one. The `PATCH`
    // asserts the **value's own** tag, which only a read of that value hands out
    // and this fixture performs no read; its cross-tenant twin is
    // `rest_taxonomies::a_foreign_tenants_value_reads_and_patches_like_an_absent_one`.
    ("POST", TAXONOMY_VALUES),
    ("PATCH", TAXONOMY_VALUE),
    ("POST", CUSTOMER_GROUP_MEMBERS),
    ("POST", CUSTOMER_GROUP_MEMBER_MOVE),
    ("POST", CUSTOMER_GROUP_MEMBERS_MOVE),
    // Paid in `rest_customer_groups::a_foreign_tenant_cannot_adjust_this_tenants_membership`,
    // which takes the membership id off the enrollment's own response so the
    // `foreign` and `absent` arms can differ in the id whose tenant is in question.
    ("PATCH", CUSTOMER_GROUP_MEMBER),
    // **The seed leaves a draft, and these routes act on a published revision.**
    // The owner's own call answers `404 current plan revision … not found`, so a
    // refusal of the foreign caller would be that same 404 and would say nothing
    // about tenancy. Paying them *here* means publishing the seeded plan, which
    // moves the world every property in this file is driven against — the debt
    // `BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE` names, one verb over. Each is paid in
    // the suite that owns the object, whose own fixture publishes:
    // `rest_windows::a_foreign_tenant_cannot_schedule_a_window_on_this_tenants_row`
    // and its `…_cannot_shorten_…` and `…_cannot_cancel_this_tenants_window`
    // siblings, `rest_supersessions::a_foreign_tenant_cannot_supersede_this_tenants_plan`,
    // `rest_cutovers::a_foreign_tenant_cannot_cut_over_this_tenants_plan`,
    // `rest_grandfather_until::a_foreign_tenant_is_refused_like_an_absent_row_and_the_owner_is_not`,
    // `rest_retirement::a_foreign_tenant_cannot_retire_this_tenants_plan` and
    // `rest_plans::a_foreign_tenants_plan_cannot_be_cloned_and_reads_like_an_absent_one`.
    ("POST", PRICE_WINDOWS),
    ("PATCH", PRICE_WINDOW),
    ("DELETE", PRICE_WINDOW),
    ("POST", PLAN_SUPERSESSIONS),
    ("POST", PLAN_CUTOVERS),
    ("PATCH", PRICE_GRANDFATHER_UNTIL),
    ("POST", PLAN_RETIRE),
    ("POST", PLAN_CLONE),
    // Refused by their own rule on the seeded world, not by a missing subject:
    // the draft plan does not satisfy the publish rules (`AMOUNT_PLACEMENT_INVALID`
    // and three more), the bundle references no published component
    // (`COMPONENT_UNPUBLISHED`), and the seeded unit's pin no longer matches its
    // subject (`APPROVAL_CONTENT_MISMATCH`) — which is why `reject` and `withdraw`,
    // exempt from the pin, are under the property and `approve` is not. Each is
    // paid on a world its own rules pass on, in
    // `rest_publish::a_foreign_tenants_plan_cannot_be_published`,
    // `rest_bundles::a_foreign_tenant_cannot_publish_this_tenants_bundle` and
    // `rest_approvals::a_foreign_tenants_unit_cannot_be_decided_or_read`.
    ("POST", PLAN_PUBLISH),
    ("POST", BUNDLE_PUBLISH),
    ("POST", APPROVAL_APPROVE),
    // **The owner is answered `PRICE_WINDOW_SCHEDULED`**, and it is the fixture that
    // puts them there: the seeded row carries a window, and
    // `price_repo::refuse_if_a_window_stands_on_it` refuses the delete of a row a
    // window stands on. Unlike the rows above, this one is not staged out of: the
    // window table refuses `DELETE`, so a row a window has ever stood on can never
    // be deleted and no act of this fixture makes the owner's call succeed. Paying
    // it means a *second* price row with no window, which is a different seed from
    // the one every property in this file is driven against — so it is paid where a
    // seed already leaves one, in
    // `rest_prices::a_foreign_tenant_cannot_delete_this_tenants_price_row`.
    ("DELETE", PLAN_PRICE),
];

/// **The SQL tenant predicate on a write, over the whole census.**
///
/// The sibling `a_foreign_tenants_object_reads_like_an_absent_one_on_every_by_id_read`
/// makes this claim for reads. Nothing made it for writes: [`Harness::denied`]
/// drives a PDP that refuses everything and [`Harness::scope_mismatch`] exercises
/// `access_scope`'s write-target membership assertion, and **neither passes a
/// caller-supplied id of another tenant's row to the repository at all**. A
/// mutating handler that resolved its path object without narrowing on the tenant
/// would satisfy both of those loops and this file's whole gate census.
///
/// Three claims per route, because each alone is satisfied by a broken other:
///
///   * the refusal is **indistinguishable** from an absent id, in status and in
///     the problem `type` — a `409` for another tenant's price id and a `404` for
///     a random one is a probe for which uuids name real rows;
///   * **no plane moved**, over [`mutable_planes`]'s whole roster and over both of
///     the harness's tenants, so a handler that wrote and then failed to find its
///     own row is not read as a refusal. Both tenants because the caller is the
///     *other* one: a handler writing before it resolves its object writes under
///     the tenant the request named, which a roster read for the row's owner alone
///     never sees;
///   * the **owner's identical call is accepted** — without which a route refusing
///     every caller for an unrelated reason (a stale precondition, a malformed
///     body) reads as tenant isolation. That control runs on a harness of its own,
///     because it succeeds and a success writes.
#[tokio::test]
async fn a_foreign_tenants_object_is_refused_like_an_absent_one_on_every_by_id_write() {
    for route in census()
        .into_iter()
        .filter(|route| route.mutating && route.path.contains('{'))
    {
        let exempt = BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE.contains(&(route.method, route.path));
        if !VARIED_SEGMENTS
            .iter()
            .any(|segment| route.path.contains(segment))
        {
            assert!(
                exempt,
                "{} {} carries no segment `absent_ids` varies, so its `foreign` and `absent` \
                 requests are byte-identical and this loop would compare a request with itself. \
                 Vary the segment, or list the route in \
                 BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE with the reason",
                route.method, route.path
            );
            continue;
        }

        // The control first and on its own world: it is the call that succeeds, and
        // a success writes.
        let control = Harness::new().await;
        let controlled = seed(&control).await;
        let owner = control
            .allowed()
            .send(drive(&route, &controlled, "\"0-3\"", "owner-key"))
            .await;
        if !owner.status().is_success() {
            let status = owner.status();
            assert!(
                exempt,
                "{} {}: the owner's own call answered {status}, so a refusal of the foreign \
                 caller below proves nothing about tenancy. Make the fixture drive this route \
                 to a success, or list it in BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE with the \
                 reason",
                route.method, route.path
            );
            continue;
        }
        assert!(
            !exempt,
            "{} {} is listed in BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE and is now coverable: \
             the owner writes it and `absent_ids` varies its id. Strike the row",
            route.method, route.path
        );

        let harness = Harness::new().await;
        let seeded = seed(&harness).await;
        let stores_before = mutable_planes(&harness).await;

        let foreign = harness
            .other_tenant()
            .send(drive(&route, &seeded, "\"0-3\"", "foreign-key"))
            .await;
        let absent = harness
            .other_tenant()
            .send(drive(&route, &absent_ids(&seeded), "\"0-3\"", "absent-key"))
            .await;

        let foreign_status = foreign.status();
        let absent_status = absent.status();
        // **The answer is named, not merely compared.** `!is_success()` is satisfied
        // by a handler that 500s on a foreign id, and by one that 500s on both - and
        // a pair of 500s is indistinguishable from itself while telling the caller
        // the row is there and something broke reaching it. The scope predicate's
        // own refusal is a `404`: the row is not in this caller's world.
        assert_eq!(
            foreign_status,
            StatusCode::NOT_FOUND,
            "{} {}: a foreign tenant's object must answer exactly as an id naming nothing \
             does, and that answer is 404 - got {foreign_status}",
            route.method,
            route.path
        );
        assert_eq!(
            foreign_status, absent_status,
            "{} {}: a foreign tenant's object answers {foreign_status} and an absent one \
             {absent_status}; the difference is a probe for which ids name real rows",
            route.method, route.path
        );
        let foreign_type = body_json(foreign).await.get("type").cloned();
        let absent_type = body_json(absent).await.get("type").cloned();
        assert!(
            foreign_type.is_some(),
            "{} {}: the refusal must be an RFC-9457 problem carrying a `type`; without this \
             the comparison below is two `None`s, which agree however the two handlers \
             answered",
            route.method,
            route.path
        );
        assert_eq!(
            foreign_type, absent_type,
            "{} {}: the two refusals must be indistinguishable in the problem document too, \
             or the code distinguishes what the status does not",
            route.method, route.path
        );

        let stores_after = mutable_planes(&harness).await;
        for (plane, before) in &stores_before {
            assert_eq!(
                stores_after.get(plane),
                Some(before),
                "{} {} refused a foreign caller and still wrote the {plane} plane",
                route.method,
                route.path
            );
        }
    }
}

#[test]
fn every_by_id_write_exemption_names_a_route_the_census_has() {
    let by_id: std::collections::BTreeSet<(&str, &str)> = census()
        .iter()
        .filter(|route| route.mutating && route.path.contains('{'))
        .map(|route| (route.method, route.path))
        .collect();

    let orphans: Vec<(&str, &str)> = BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE
        .iter()
        .copied()
        .filter(|row| !by_id.contains(row))
        .collect();
    assert!(
        orphans.is_empty(),
        "these rows exempt a by-id write the census does not have, so the exemption stands for \
         nothing: {orphans:?}"
    );
}

#[test]
fn every_by_id_read_exemption_names_a_route_the_census_has() {
    // `every_further_question_belongs_to_a_route_that_asks_one`'s shape: a row for a
    // route that no longer exists is compared against nothing and reads as an
    // acknowledged gap to anyone who greps for it.
    let by_id: std::collections::BTreeSet<(&str, &str)> = census()
        .iter()
        .filter(|route| !route.mutating && route.path.contains('{'))
        .map(|route| (route.method, route.path))
        .collect();

    let orphans: Vec<(&str, &str)> = BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE
        .iter()
        .copied()
        .filter(|row| !by_id.contains(row))
        .collect();
    assert!(
        orphans.is_empty(),
        "these rows exempt a by-id read the census does not have, so the exemption stands for \
         nothing: {orphans:?}"
    );
}

#[tokio::test]
async fn a_write_whose_target_tenant_is_outside_the_scope_is_denied_on_every_write() {
    // `access_scope`'s membership assertion. The degraded flat-`In` PDP decision
    // does not re-check `owner_tenant_id`, so a target outside the compiled
    // scope is refused there or nowhere.
    for route in census().into_iter().filter(|route| route.mutating) {
        let harness = Harness::new().await;
        let seeded = seed(&harness).await;
        let stores_before = mutable_planes(&harness).await;

        let response = harness
            .scope_mismatch()
            .send(drive(&route, &seeded, "\"0-3\"", "mismatch-key"))
            .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{} {} accepted a write anchored outside the caller's scope",
            route.method,
            route.path
        );

        // **The message's claim, measured.** "Accepted a write" is a statement
        // about the store, and the status alone cannot make it: a handler that
        // wrote and then refused answers 403 too. The roster is
        // `entity::MODULES`, so a table added to the schema owes a plane before
        // this can pass, and the two sibling properties in this file already read
        // it the same way.
        let stores_after = mutable_planes(&harness).await;
        assert_eq!(
            stores_after, stores_before,
            "{} {} refused the out-of-scope write after moving a plane",
            route.method, route.path
        );
    }
}

// ---------------------------------------------------------------------------
// Every declared authz label has a route that enforces it.
//
// `every_route_asks_the_catalogued_pair` compares each route against this
// file's `census()`, and the census records the pair each route **does** ask
// for. Both sides are therefore descriptions of the same implementation, and a
// route gated on the wrong label satisfies them both: the code asks for `plan`,
// the census says `plan`, the check is green.
//
// `labels::ALL` is the other kind of artifact. It is the catalog of what an
// operator can be **granted**, authored alongside the permission instances in
// `gts::permissions` -- "Read the catalog audit trail", "Import governed
// backdated reference prices" -- and it owes nothing to any handler. So a label
// in it that no route ever asks about is a permission that grants nothing:
// granting it withholds nothing, and withholding it protects nothing.
//
// That is not a hypothetical. `audit_read` and `audit_export` were declared
// while `/history` -- the catalog audit trail itself -- gated on `plan x read`,
// so anyone holding catalog read could read who changed what and when.
//
// `historical_import` was the same finding's second half and it was the wrong
// repair. The permission pair was grantable and no route asked for it, so this
// guard was satisfied by moving `/bulk-imports` onto the label -- which is the
// other way to make a set-membership check green, and the one that changes the
// system rather than describing it. The endpoint map had those routes on `plan`
// throughout. A label with no route is a question about which of the two the
// design set names; this guard cannot tell, and the answer here was the label.
// It is struck with its flow now (D-330), so it is neither grantable nor asked
// for and there is nothing left to reconcile.
// ---------------------------------------------------------------------------

/// Labels declared ahead of the surface that will enforce them.
///
/// An exemption is a **debt**, stated so it stays visible. `customer_group` sat
/// here while the membership half was not yet built in this strand
/// (`overlay_repo::taxonomy_declares` refused that class outright); it left the
/// day `api::rest::customer_groups` became that label's first route consumer.
/// A label whose surface exists and does not ask for it does not belong here
/// -- it belongs in the handler.
const LABELS_WITHOUT_A_SURFACE_YET: &[&str] = &[];

#[test]
fn every_grantable_label_is_enforced_by_some_route() {
    let asked: std::collections::BTreeSet<&str> =
        census().iter().map(|r| r.resource_type).collect();

    let unenforced: Vec<&str> = labels::ALL
        .iter()
        .copied()
        .filter(|label| !asked.contains(label))
        .filter(|label| !LABELS_WITHOUT_A_SURFACE_YET.contains(label))
        .collect();

    assert!(
        unenforced.is_empty(),
        "these labels can be granted and are asked for by no route, so granting \
         them confers nothing and withholding them protects nothing: {unenforced:?}"
    );
}

// ---------------------------------------------------------------------------
// And the same property one level down: every catalogued **pair** (Z13-13).
//
// `every_grantable_label_is_enforced_by_some_route` above collects
// `r.resource_type` and nothing else, so it is satisfied by *any* action on a
// label. `audit x read` gaining a route (`/history`, and then `/audit`) therefore
// silenced the guard for `audit x export` as well — and `actions::EXPORT` is
// asked for by no route in the gear, so the permission grants nothing and
// withholding it protects nothing. That is the label guard's own argument,
// applied to the axis it cannot see.
//
// The catalogued set is read off the **GTS inventory** rather than re-listed
// here: `gts::permissions` is where a pair becomes grantable, and a second copy
// of its 20 rows in a test file is the F-12 shape that produced this finding.
// ---------------------------------------------------------------------------

/// The `(resource_type, action)` pairs `gts::permissions` declares.
fn catalogued_pairs() -> std::collections::BTreeSet<(String, String)> {
    const PERMISSION_TYPE_ID: &str = "gts.cf.toolkit.authz.permission.v1~";
    toolkit_gts::inventory::iter::<toolkit_gts::InventoryInstance>
        .into_iter()
        .filter(|entry| {
            entry.instance_id.starts_with(PERMISSION_TYPE_ID)
                && entry.instance_id[PERMISSION_TYPE_ID.len()..].starts_with("cf.bss.pricing.")
        })
        .map(|entry| {
            let payload = (entry.payload_fn)();
            let field = |name: &str| {
                payload[name]
                    .as_str()
                    .unwrap_or_else(|| {
                        panic!("AuthzPermissionV1 {} carries a {name}", entry.instance_id)
                    })
                    .to_owned()
            };
            (field("resource_type"), field("action"))
        })
        .collect()
}

/// The floor under [`catalogued_pairs`], and it is load-bearing rather than
/// defensive.
///
/// Every guard below ranges over that set, so an inventory read that returned
/// **nothing** — a changed `gts_id!` prefix, a linkage change, a renamed instance
/// suffix — would make all three trivially green while measuring an empty set. That
/// is this codebase's most common defect shape, and a set-valued guard read out of a
/// registry is exactly where it hides.
///
/// **It drops with the catalog rather than tracking it.** D-330's strike took
/// `historical_import`'s two pairs out, leaving 20; a floor left at 20 would be an
/// exact-count assertion wearing a floor's name — green today, red on the next strike,
/// and saying nothing either time about whether the inventory read works.
const AT_LEAST_THIS_MANY_PAIRS: usize = 18;

/// Catalogued pairs declared ahead of the surface that will ask for them.
///
/// An exemption is a **debt**, stated so it stays visible —
/// [`LABELS_WITHOUT_A_SURFACE_YET`]'s rule, per pair.
///
/// **Empty, and `audit x export` is why it is.** That pair was this list's one
/// entry and the gear's one catalogued-but-unasked permission: `api::rest::audit`'s
/// registration said "it is not the export - `audit` x `export` is a second
/// permission for a chunked shape, and **neither is built**", of which the read
/// half was built by Z13-8 and the export half was owed. §5's
/// `POST /bss-pricing/v1/history/export` is that second half
/// (`inst-he-export`), and it asks for the pair — so the exemption is deleted
/// rather than kept describing a debt that has been paid, which is exactly what
/// `no_pair_exemption_outlives_the_route_that_arrived` refuses one plane down.
const PAIRS_WITHOUT_A_ROUTE_YET: &[(&str, &str)] = &[];

#[test]
fn every_catalogued_permission_pair_is_asked_for_by_some_route() {
    let asked: std::collections::BTreeSet<(String, String)> = census()
        .iter()
        .map(|route| (route.resource_type.to_owned(), route.action.to_owned()))
        .collect();

    let catalogued = catalogued_pairs();
    assert!(
        catalogued.len() >= AT_LEAST_THIS_MANY_PAIRS,
        "the inventory read found {} pairs, which is fewer than this catalog has declared since \
         Slice 5 - the read is broken, not the catalog: {catalogued:?}",
        catalogued.len()
    );

    let unasked: Vec<(String, String)> = catalogued
        .into_iter()
        .filter(|pair| !asked.contains(pair))
        .filter(|(label, action)| {
            !PAIRS_WITHOUT_A_ROUTE_YET
                .iter()
                .any(|(l, a)| l == label && a == action)
        })
        .collect();

    assert!(
        unasked.is_empty(),
        "these pairs are grantable and are asked for by no route, so granting them confers \
         nothing and withholding them protects nothing: {unasked:?}"
    );
}

#[test]
fn no_pair_exemption_outlives_the_route_that_arrived() {
    // `no_exemption_outlives_the_surface_it_was_waiting_for`'s direction, per pair:
    // six of this finding's seven original pairs were gated during the week it was
    // filed, so an exemption list that is not checked from this side is one that
    // describes a debt already paid.
    let asked: std::collections::BTreeSet<(String, String)> = census()
        .iter()
        .map(|route| (route.resource_type.to_owned(), route.action.to_owned()))
        .collect();

    let stale: Vec<&(&str, &str)> = PAIRS_WITHOUT_A_ROUTE_YET
        .iter()
        .filter(|(label, action)| asked.contains(&((*label).to_owned(), (*action).to_owned())))
        .collect();

    assert!(
        stale.is_empty(),
        "these pairs are asked for now and no longer need an exemption: {stale:?}"
    );
}

#[test]
fn every_exempted_pair_is_one_the_catalog_actually_declares() {
    // The third direction, which the label guard has no equivalent of and needs
    // one less: an exemption for a pair nobody catalogues is a debt against
    // nothing, and it would keep reading as a live obligation after the permission
    // instance was deleted.
    let catalogued = catalogued_pairs();

    assert!(
        catalogued.len() >= AT_LEAST_THIS_MANY_PAIRS,
        "the inventory read found {} pairs - the read is broken, not the catalog",
        catalogued.len()
    );

    let phantom: Vec<&(&str, &str)> = PAIRS_WITHOUT_A_ROUTE_YET
        .iter()
        .filter(|(label, action)| {
            !catalogued.contains(&((*label).to_owned(), (*action).to_owned()))
        })
        .collect();

    assert!(
        phantom.is_empty(),
        "these exemptions name pairs `gts::permissions` does not declare: {phantom:?}"
    );
}

#[test]
fn no_exemption_outlives_the_surface_it_was_waiting_for() {
    // The other direction, and the one an exemption list rots without: a label
    // excused for having no surface, whose surface has since arrived and asks
    // for it, must leave the list rather than sit there describing nothing.
    let asked: std::collections::BTreeSet<&str> =
        census().iter().map(|r| r.resource_type).collect();

    let stale: Vec<&str> = LABELS_WITHOUT_A_SURFACE_YET
        .iter()
        .copied()
        .filter(|label| asked.contains(label))
        .collect();

    assert!(
        stale.is_empty(),
        "these labels are enforced now and no longer need an exemption: {stale:?}"
    );
}
