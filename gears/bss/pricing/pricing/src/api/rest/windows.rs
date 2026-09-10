//! Slice 7's window surface: the two reads and the three mutations §5 declares.
//!
//! - `GET /plans/{planId}/coverage` — the coverage/gap report an operator
//!   remediates from (`dod-window-coverage`).
//! - `GET /plans/{planId}/sellability` — the gate's surface, four of six
//!   predicates over one pinned delta (`dod-sellability`; see
//!   [`crate::domain::sellability`] for the split and
//!   [`get_plan_sellability`] for which version it answers from).
//! - `POST /prices/{priceId}/windows`, `PATCH`/`DELETE /price-windows/{windowId}` —
//!   each a publish unit (D-99): validation → pending `CatalogVersion` ref →
//!   plan-subject re-projection, answering **202**.
//!   `infra::window::WindowService` is what they call.
//!
//! **The two reads answer different questions about the same plane, and neither is
//! the other's summary.** Coverage is the *truth* side an operator acts on — every
//! key the plan touches, gaps included, over the rows a publish would produce.
//! Sellability is the *pinned* side a storefront reads — the keys a purchase binds
//! on one market, from the frozen version a consumer may pin, which is a different
//! row set at a different instant. A plan can be fully covered on the truth side
//! and unsellable on the pinned one, for the length of one batching delay.
//!
//! # The three mutations answer 202, and none of them answers 200
//!
//! A publish unit is not consumer-visible until `CatalogVersionPublished` + warm. A
//! 200 would tell the caller the coverage changed *for readers*, which it has not:
//! what changed is the truth side, and the read model still answers the previous
//! pin until the projector catches up. The body carries the **pending** handle for
//! the same reason — there is no committed `CatalogVersion` yet and there will not
//! be until the registry batches (D-47).
//!
//! # Two of the three mutations answer 202 **twice**, and D-62 is why
//!
//! The `DELETE` and a **shortening** `PATCH` are on `inst-mat-registered`'s
//! always-material trigger list, so neither commits on one principal. The first call
//! runs every check, opens a pinned approval unit and changes nothing —
//! `outcome = submitted_for_approval`, the stored interval echoed unmoved, no version
//! handle; the call after an independent approve commits — `outcome = mutated`. The
//! `POST` and a lengthening `PATCH` answer `mutated` on the first call, because they
//! are not on the trigger list and what would make *them* material is a threshold
//! policy G6 lands. [`crate::infra::window`]'s module doc carries the split and the
//! obligation.
//!
//! # The state is `GovernanceState`, coverage GET included
//!
//! The mutations need the window service, which holds the catalog-version registry
//! handle, and that handle is the criterion `api::rest::state` splits the two states
//! on. So the whole module moved rather than growing a second `pub fn router`: the
//! route census keys on `pub fn router` under `src/api/rest/**`, and a router named
//! anything else is precisely the hole `every_mounted_router_is_merged_into_both_censuses`
//! exists to close — G3 found a router that was registered, declared, catalogued for
//! authz and answering 404 on every request because the harness never mounted it.
//! One module, one router, one census entry.
//!
//! # `DELETE` takes no idempotency header, and that is §5's call
//!
//! §5's Idempotency column gives the `POST` a **client idempotency key** and the
//! `PATCH` an **`ETag`**, and leaves the `DELETE` cell empty — so this surface asks
//! for neither there, and the omission is the design set's rather than an oversight
//! to improve on. Reported as a divergence rather than fixed by adding a header:
//! a cancel is not naturally idempotent the way a publish is (a revision has one
//! edge out of `draft`; a window has one edge out of `scheduled`, which is
//! `window_repo::transition`'s narrowed `idempotent_arrival` refusing a second
//! cancellation outright — so a retried `DELETE` is answered `WINDOW_NOT_CANCELLABLE`
//! rather than replaying). That refusal is safe and legible, which is why the
//! divergence is reported and not patched.
//!
//! # Why this route exists when `precheck` already reports coverage
//!
//! `POST …/plans/{planId}/publish` runs the aggregate rule set and its refusal
//! already enumerates every uncovered key. This surface answers a different
//! question, and §5 files it under "operator remediation" for that reason:
//!
//! - **It needs no open draft revision.** `infra::publish::assemble` answers
//!   `NotFound` for a plan with nothing to publish, so a published plan whose
//!   coverage an operator wants to inspect — the ordinary case — cannot be
//!   pre-checked at all.
//! - **It reports state, not violations.** Every key, its intervals, its derived
//!   coverage end and its interior gaps, including the keys that are fine. A
//!   refusal names only what is wrong, which is the wrong shape for "what does
//!   this plan's time axis look like".
//!
//! What it deliberately does **not** report is `AVAILABILITY_OUTSIDE_COVERAGE`.
//! That rule judges the plan's `availableFrom`/`availableTo` against per-key
//! coverage, so it is a finding about the *plan* rather than a property of a key,
//! and the publish report is where it belongs. Naming it here as well would give
//! one violation two surfaces to disagree on.
//!
//! **DIVERGENCE (reported, not fixed): a cancel can move a published
//! plan's dates outside its own coverage, and the publish report is not always a
//! surface.** `AvailabilityInsideCoverage` is registered in the **publish** pipeline
//! only, and a window mutation runs the two rules `inst-fg-when` names — so cancelling
//! a plan's *first* window, which is legal whenever a later open-ended one keeps the key
//! covered, moves the coverage **start** past `availableFrom` with nothing re-checking.
//! The textual exclusion is defensible (`inst-fg-when` is step 2 of §3's Future-Gap
//! Detection, written about that section's two rules) and running the publish-time set
//! inside a mutation is not available either: `inst-wc-required` would refuse the very
//! mutation that fixes it.
//!
//! The sentence above — *"the publish report is its surface"* — is **not** the whole
//! answer, and D-128 is why: a retired plan can never publish again and a published plan
//! nobody revises gets no report, so for those the finding has no reader at all. What
//! finishes the argument is the **gate**: predicate (1) evaluates each bound key's
//! coverage at the caller's instant, so over the uncovered interval the plan-market is
//! `not_sellable` whatever its dates say. So the residue is an operator-visible
//! incoherence and not a sales hazard — nothing can be sold into the hole.
//! `sqlite_window_service.rs::a_cancel_can_move_a_published_plan_outside_its_own_coverage`
//! executes both halves.
//!
//! # The row set is the publish check's own
//!
//! [`CANDIDATE_ROW_STATES`] — the plan's `published` rows plus the `draft` rows a
//! publish would produce. Taking the published plane alone would make this
//! surface answer a different question from the one whose failure sent the
//! operator here, and the whole value of a remediation surface is that acting on
//! it makes the publish pass.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::http::HeaderMap;
use axum::http::header::ETAG;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, body::Bytes, http::StatusCode};
use bss_pricing_sdk::CatalogVersion;
use bss_pricing_sdk::odata::WindowFilterField;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::{ApprovalView, MaterialityView};
use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::plans::{idempotency_key_param, if_match_param};
use crate::api::rest::preconditions;
use crate::api::rest::state::GovernanceState;
use crate::domain::concurrency::RowVersion;
use crate::domain::coverage::{self, CoverageReport, KeyCoverage};
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::money::CurrencyCode;
use crate::domain::read_model::SubjectRef;
use crate::domain::scope_key::{PlanId, Region};
use crate::domain::sellability::{
    PredicateAnswer, PredicateOutcome, SellabilityFacts, SellabilitySurface,
};
use crate::domain::window::{CoverageEnd, WindowInterval};
use crate::infra::idempotent::{self, Guarded, GuardedRequest};
use crate::infra::publish::CANDIDATE_ROW_STATES;
use crate::infra::storage::repo::window_repo::WindowRecord;
use crate::infra::storage::repo::{pin_frontier_repo, price_repo, read_model_repo, window_repo};
use crate::infra::storage::repo_failure;
use crate::infra::window::{PendingApproval, WindowMutationOutcome, WindowMutationReceipt};
use time::OffsetDateTime;

/// `OpenAPI` tag applied to the window operations (DE0205 requires a tag and a
/// summary on every registered operation).
const TAG: &str = "BSS Pricing Price Windows";

/// The per-key coverage report (§5, `dod-window-coverage`).
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a **literal** argument and silently passes a `const` one, so the
/// route-shape rule only binds where the literal is; the two spellings are pinned
/// together by `tests/module_test.rs`'s route census.
pub const PLAN_COVERAGE: &str = "/bss-pricing/v1/plans/{planId}/coverage";

/// The scheduling collection of one price row (§5, `POST`).
///
/// The literal is repeated in the `OperationBuilder` call for [`PLAN_COVERAGE`]'s
/// reason: DE0801 validates a literal and silently passes a `const`.
pub const PRICE_WINDOWS: &str = "/bss-pricing/v1/prices/{priceId}/windows";

/// One window, addressed by its own id (§5, `PATCH` and `DELETE`).
///
/// **Not nested under the price row**, which is §5's shape and not a shortcut: a
/// window id is unique on its own, so nesting would put a second identifier in the
/// path that the server would have to check agrees with the first — the arm
/// `require_same_revision` exists for on the price routes, bought for nothing here.
pub const PRICE_WINDOW: &str = "/bss-pricing/v1/price-windows/{windowId}";

/// The window collection, cursor-paginated (D-125).
///
/// The **collection** of [`PRICE_WINDOW`] and deliberately not nested under a
/// price row: `POST …/prices/{priceId}/windows` is the scheduling collection of
/// one row, so a `GET` there would answer about one row only, and the timeline a
/// caller wants is over the tenant's plane with `price_id` as a *filter* rather
/// than as a path segment it must always know.
///
/// The literal is repeated in the `OperationBuilder` call for [`PLAN_COVERAGE`]'s
/// reason: DE0801 validates a literal and silently passes a `const`.
pub const PRICE_WINDOWS_LIST: &str = "/bss-pricing/v1/price-windows";

/// The sellability surface the joint gate reads (§5, `dod-sellability`).
///
/// The literal is repeated in the `OperationBuilder` call for [`PLAN_COVERAGE`]'s
/// reason: DE0801 validates a literal and silently passes a `const`.
pub const PLAN_SELLABILITY: &str = "/bss-pricing/v1/plans/{planId}/sellability";

/// The three query parameters §5 names, each optional at the type and **required
/// in the handler**.
///
/// Optional here so that an absent or unparseable value is refused through the
/// canonical ladder naming the parameter, rather than by axum's own extractor
/// rejection — which answers a bare `400` with no problem document at all, the
/// shape `a_malformed_plan_id_never_reaches_the_handler` pins for a path segment
/// and which is right there (this gear never answered) and wrong here (it did).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SellabilityQuery {
    /// The instant to evaluate at, RFC 3339.
    pub at: Option<String>,
    /// The currency half of the bound market, ISO 4217.
    pub currency: Option<String>,
    /// The region half of the bound market.
    pub region: Option<String>,
}

/// One canonical scope key's coverage, as the wire renders it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct KeyCoverageView {
    /// The key's canonical rendering — the ten axes, pipe-separated, exactly as
    /// the violation subjects of a publish refusal name it.
    ///
    /// The **same string** on both surfaces on purpose: an operator who was told
    /// `WINDOW_COVERAGE_MISSING` on a key has to be able to find that key here
    /// without transcribing ten axes.
    pub scope_key: String,
    /// Does a billable row of this plan sit on the key? `false` on a key only
    /// history occupies, which is reported and never refused.
    pub required: bool,
    /// Does the key hold an **active or scheduled** window — `inst-wc-required`'s
    /// own predicate?
    ///
    /// Deliberately not derivable from [`KeyCoverageView::coverage_end`]: a key
    /// whose only window expired has a coverage end and no live window, and it is
    /// the second of those that decides whether a publish passes.
    pub covered: bool,
    /// How far the key's coverage runs.
    pub coverage_end: CoverageEndView,
    /// Every window of the key, ordered, **every state included** — a cancelled
    /// one among them, because "why did this key lose its successor" is the
    /// question this surface exists to answer.
    pub intervals: Vec<WindowIntervalView>,
    /// The uncovered intervals **between** two of the key's windows.
    ///
    /// A void with no successor is never here: `inst-fg-detect` is an interior
    /// check and cannot see one, which is `inst-fg-trailing`'s whole reason for
    /// existing.
    pub interior_gaps: Vec<CoverageGapView>,
}

/// Where a key's coverage stops, as the three-armed answer the domain carries.
///
/// `kind` and a nullable instant rather than a bare nullable instant, because
/// `null` would have to serve two **opposite** answers: a key covered forever
/// passes every horizon predicate and a key covered not at all fails all of them.
/// The read-model payload declares the same shape for the same reason.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CoverageEndView {
    /// `uncovered` | `ends` | `open_ended`.
    pub kind: String,
    /// The exclusive instant coverage ends at, non-null only on `ends`.
    #[serde(default, with = "rfc3339::option")]
    pub at: Option<OffsetDateTime>,
}

impl From<CoverageEnd> for CoverageEndView {
    fn from(end: CoverageEnd) -> Self {
        Self {
            kind: end.as_str().to_owned(),
            at: end.at(),
        }
    }
}

/// One window interval of a key.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct WindowIntervalView {
    /// Inclusive start, UTC.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `null` is open-ended.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
    /// `scheduled` | `active` | `expired` | `cancelled`.
    pub state: String,
}

impl From<&WindowInterval> for WindowIntervalView {
    fn from(interval: &WindowInterval) -> Self {
        Self {
            effective_from: interval.effective_from,
            effective_to: interval.effective_to,
            state: interval.state.as_str().to_owned(),
        }
    }
}

/// One window on a page: the interval, where it stands, and the precondition the
/// next act on it needs.
///
/// It is **not** [`WindowMutationView`], which is a mutation's receipt: that one
/// carries a `pendingVersionRef` and the revision a re-projection froze, neither
/// of which a stored window has. Nor is it [`WindowIntervalView`], which is a
/// key's interval inside a coverage report and carries no id at all — a page of
/// those would be a list nothing could be addressed from.
///
/// `scopeKey` is the **same string** [`KeyCoverageView`] renders, for that field's
/// own reason: an operator told `WINDOW_COVERAGE_MISSING` on a key has to be able
/// to find the key here without transcribing ten axes.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct WindowSummaryView {
    /// The window's own id — what `PATCH` and `DELETE …/price-windows/{windowId}`
    /// address.
    pub window_id: Uuid,
    /// The price row the interval is bound to.
    pub price_id: Uuid,
    /// The row's canonical scope key, the ten axes pipe-separated.
    pub scope_key: String,
    /// Inclusive start, UTC.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `null` is **open-ended** and not "unset".
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
    /// `scheduled` | `active` | `expired` | `cancelled`.
    pub state: String,
    /// The operator-supplied change reason carried for the audit trail.
    pub reason_code: String,
    /// When the window was scheduled, UTC.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it became active, UTC; `null` while it has not.
    #[serde(default, with = "rfc3339::option")]
    pub activated_at: Option<OffsetDateTime>,
    /// When it expired, UTC; `null` while it has not.
    #[serde(default, with = "rfc3339::option")]
    pub expired_at: Option<OffsetDateTime>,
    /// When it was cancelled, UTC; `null` if it never was.
    #[serde(default, with = "rfc3339::option")]
    pub cancelled_at: Option<OffsetDateTime>,
    /// The act sequence a `PATCH` or `DELETE` must assert as its `If-Match`.
    ///
    /// On the page for the reason `PlanSummaryView::row_version` is: a caller that
    /// lists and then adjusts needs the precondition from the page, and a client
    /// that cannot read response headers can still submit an `If-Match`.
    pub mutation_seq: u64,
}

impl From<&WindowRecord> for WindowSummaryView {
    fn from(record: &WindowRecord) -> Self {
        Self {
            window_id: record.window_id,
            price_id: record.price_id,
            scope_key: record.scope_key.to_string(),
            effective_from: record.effective_from,
            effective_to: record.effective_to,
            state: record.state.as_str().to_owned(),
            reason_code: record.reason_code.clone(),
            created_at: record.created_at,
            activated_at: record.activated_at,
            expired_at: record.expired_at,
            cancelled_at: record.cancelled_at,
            mutation_seq: record.mutation_seq,
        }
    }
}

/// One uncovered interval, half-open `[gapStart, gapEnd)`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CoverageGapView {
    /// Inclusive start of the hole — the instant the preceding window stopped
    /// covering.
    #[serde(with = "rfc3339")]
    pub gap_start: OffsetDateTime,
    /// Exclusive end — the instant the next window starts.
    #[serde(with = "rfc3339")]
    pub gap_end: OffsetDateTime,
}

/// The body of `POST /prices/{priceId}/windows`.
///
/// `effectiveTo` is `Option` and its absence means **open-ended**, which is the
/// store's own reading: a window with no end covers every instant from its start.
/// It is deliberately not a required field with a sentinel — an open-ended window
/// is the ordinary case for a plan's first window, and a sentinel instant would be
/// an instant somebody eventually compares against.
// `(request, response)` and not `(request)` alone: the guarded `POST` digests the
// **parsed** request (`preconditions::request_digest`, so member order and whitespace
// cannot make an honest retry look like a different intent), and that needs the type to
// serialize. Every guarded create in this gear is declared the same way.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct ScheduleWindowRequest {
    /// Inclusive start, UTC, strictly in the future (`inst-ws-future-start`,
    /// D-63).
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; absent is open-ended.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
    /// Why the window was scheduled — free text the store keeps with the row.
    pub reason_code: String,
}

/// The body of `PATCH /price-windows/{windowId}`.
///
/// One field, and the `Option` carries the same meaning it does on the schedule:
/// absent means **remove the bound**, making the window open-ended. That is an
/// extension rather than a shortening, so it passes the trailing-void floor by
/// construction — a key covered forever satisfies every floor there is.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct AdjustWindowRequest {
    /// The new exclusive end, UTC; absent makes the window open-ended.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
}

/// What a window mutation answers with — the pending handle and the row as stored.
///
/// It carries no `catalogVersion`, and that absence is the 202: the committed
/// number does not exist yet. A field for it would be a field every success left
/// null, which is the shape D-161 refuses one artifact over.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct WindowMutationView {
    /// The window the mutation created or moved.
    pub window_id: Uuid,
    /// The plan whose subject re-projects.
    pub plan_id: Uuid,
    /// The price row the window is bound to.
    pub price_id: Uuid,
    /// The plan revision the re-projection freezes — the current one (D-99).
    pub revision: u64,
    /// The registry handle the projector resolves into a `CatalogVersion`.
    ///
    /// **`null` on the controlled arm**, and that is the one thing about it worth
    /// stating: a D-62 mutation that opened a unit requested no version, because it
    /// wrote nothing that could need addressing. A handle there would name an
    /// assignment nothing will ever commit — the orphan D-156 moves the request
    /// *inside* the transaction to avoid. `PublishReceiptView` types it the same way
    /// for the same reason.
    pub pending_version_ref: Option<String>,
    /// Inclusive start, UTC.
    #[serde(with = "rfc3339")]
    pub effective_from: OffsetDateTime,
    /// Exclusive end, UTC; `null` is open-ended.
    #[serde(default, with = "rfc3339::option")]
    pub effective_to: Option<OffsetDateTime>,
    /// `scheduled` | `active` | `expired` | `cancelled`.
    pub state: String,
}

impl From<WindowMutationReceipt> for WindowMutationView {
    fn from(receipt: WindowMutationReceipt) -> Self {
        Self {
            window_id: receipt.window_id,
            plan_id: receipt.plan_id.get(),
            price_id: receipt.price_id,
            revision: receipt.revision,
            pending_version_ref: Some(receipt.pending_version_ref),
            effective_from: receipt.effective_from,
            effective_to: receipt.effective_to,
            state: receipt.state.as_str().to_owned(),
        }
    }
}

impl From<&PendingApproval> for WindowMutationView {
    /// The window as the controlled arm leaves it — **unmoved**, and with no handle.
    fn from(pending: &PendingApproval) -> Self {
        Self {
            window_id: pending.window_id,
            plan_id: pending.plan_id.get(),
            price_id: pending.price_id,
            revision: pending.revision,
            pending_version_ref: None,
            effective_from: pending.effective_from,
            effective_to: pending.effective_to,
            state: pending.state.as_str().to_owned(),
        }
    }
}

/// The wire token for the arm that mutated.
///
/// Its sibling is `api::rest::publish`'s `OUTCOME_SUBMITTED`, imported rather than
/// re-spelled: one word for one act across both surfaces.
const OUTCOME_MUTATED: &str = "mutated";

/// What a window mutation did: it ran, or it opened a unit and stood still.
///
/// One response type across the two arms, for the reason `PublishOutcomeView` gives —
/// a generated client has one thing to deserialize and reads `outcome` to know which
/// arm it got. Here both arms are **202**, which is D-99 rather than an oversight: a
/// mutation that ran is not consumer-visible until its version commits and warms, and
/// a mutation that opened a unit has not run, so neither can honestly answer 200.
/// `outcome` is therefore the only discriminator, which is exactly why it is a field
/// and not an inference from which halves are null.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct WindowMutationOutcomeView {
    /// `mutated` | `submitted_for_approval`.
    pub outcome: String,
    /// The window as it stands **after** the call — on both arms, and on the
    /// controlled one it is the row unchanged. A caller therefore reads the truth of
    /// the interval either way rather than having to infer it from the outcome.
    pub window: WindowMutationView,
    /// Why a second principal was required, on the controlled arm. `null` on the arm
    /// that mutated: nothing about a schedule or an extension is material yet, and a
    /// verdict there would be a verdict nobody evaluated.
    pub materiality: Option<MaterialityView>,
    /// The pinned unit this call opened, on the controlled arm.
    pub approval: Option<ApprovalView>,
}

/// The plan's whole time axis, one entry per canonical scope key.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PlanCoverageView {
    /// The plan the report is about.
    pub plan_id: Uuid,
    /// Every key: the ones a billable row sits on, and the ones only windows
    /// mention. Ordered by the key's canonical rendering, so two reads of one
    /// state read identically.
    pub keys: Vec<KeyCoverageView>,
}

/// One predicate's answer, as the wire renders it.
///
/// **`answer` is a token and there is no boolean anywhere in this document**, which
/// is D-99 reaching the wire: what a consumer is given is intervals, states, a
/// derived coverage end and per-predicate tokens, so no field of it is an answer to
/// a question about the reader's clock.
///
/// `detail` and `owed_to` are both nullable and never both set: the first belongs to
/// `failed` and the second to `not_evaluable`. Two fields rather than one, because
/// the whole of D-167 clause (3) is that a consumer can tell *"this predicate is
/// false"* from *"this version cannot evaluate this predicate"* — and one merged
/// message field would have made the difference a matter of reading English.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PredicateAnswerView {
    /// The design set's own number for the predicate, 1 to 6.
    pub ordinal: u8,
    /// The predicate's name.
    pub predicate: String,
    /// `satisfied` | `failed` | `not_evaluable`.
    pub answer: String,
    /// Why the predicate is false, on `failed` and nowhere else.
    pub detail: Option<String>,
    /// What owes the fact this predicate reads, on `not_evaluable` and nowhere
    /// else — the slice, or the version.
    pub owed_to: Option<String>,
}

impl From<&PredicateOutcome> for PredicateAnswerView {
    fn from(outcome: &PredicateOutcome) -> Self {
        let (detail, owed_to) = match &outcome.answer {
            PredicateAnswer::Satisfied => (None, None),
            PredicateAnswer::Failed { detail } => (Some(detail.clone()), None),
            PredicateAnswer::NotEvaluable { owed_to } => (None, Some((*owed_to).to_owned())),
        };
        Self {
            ordinal: outcome.predicate.ordinal(),
            predicate: outcome.predicate.as_str().to_owned(),
            answer: outcome.answer.as_str().to_owned(),
            detail,
            owed_to,
        }
    }
}

/// One bound key's sellability: its frozen window facts and its per-key
/// predicates.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct KeySellabilityView {
    /// The key's canonical rendering — the **same string** the coverage report and
    /// a publish refusal use, so an operator matches keys across the three
    /// surfaces without transcribing ten axes.
    pub scope_key: String,
    /// How far this key's coverage runs, as the three-armed answer.
    pub coverage_end: CoverageEndView,
    /// The key's frozen intervals and their states, exactly as the version froze
    /// them — what lets a consumer re-derive the predicate at another instant
    /// without a second call.
    pub intervals: Vec<WindowIntervalView>,
    /// One answer per per-key predicate.
    pub predicates: Vec<PredicateAnswerView>,
}

/// The plan's sellability on one market at one instant.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PlanSellabilityView {
    /// The plan asked about.
    pub plan_id: Uuid,
    /// The instant the answer is about — the caller's own, echoed so a stored
    /// response cannot be mistaken for one about a different moment.
    #[serde(with = "rfc3339")]
    pub at: OffsetDateTime,
    /// The currency half of the bound market.
    pub currency: String,
    /// The region half of the bound market.
    pub region: String,
    /// The pin-eligible version the answer was read from, `null` when none carries
    /// this plan.
    pub catalog_version: Option<u64>,
    /// The D-94 conjunction: `sellable` | `not_sellable` | `not_evaluable`.
    ///
    /// **`not_evaluable` is not a yes.** With two of the six predicates unanswerable
    /// by any version this gear can project, it is what a fully-covered, in-dates,
    /// published plan answers today: the gate saying it is not yet a gate.
    pub verdict: String,
    /// The plan-level predicates.
    pub predicates: Vec<PredicateAnswerView>,
    /// Every key the purchase binds on this market, eligibility-resolved.
    /// **Empty means not sellable**, never vacuously sellable.
    pub keys: Vec<KeySellabilityView>,
}

impl From<&SellabilitySurface> for PlanSellabilityView {
    fn from(surface: &SellabilitySurface) -> Self {
        Self {
            plan_id: surface.plan_id.get(),
            at: surface.at,
            currency: surface.currency.as_str().to_owned(),
            region: surface.region.as_str().to_owned(),
            catalog_version: surface.catalog_version.map(CatalogVersion::get),
            verdict: surface.plan_market_verdict().as_str().to_owned(),
            predicates: surface
                .plan_answers
                .iter()
                .map(PredicateAnswerView::from)
                .collect(),
            keys: surface
                .keys
                .iter()
                .map(|key| KeySellabilityView {
                    scope_key: key.scope_key.to_string(),
                    coverage_end: CoverageEndView::from(key.coverage_end),
                    intervals: key.intervals.iter().map(WindowIntervalView::from).collect(),
                    predicates: key.answers.iter().map(PredicateAnswerView::from).collect(),
                })
                .collect(),
        }
    }
}

impl From<&KeyCoverage> for KeyCoverageView {
    fn from(entry: &KeyCoverage) -> Self {
        Self {
            scope_key: entry.scope_key().to_string(),
            required: entry.required,
            covered: entry.has_live_window(),
            coverage_end: CoverageEndView::from(entry.coverage_end()),
            intervals: entry
                .windows
                .intervals
                .iter()
                .map(WindowIntervalView::from)
                .collect(),
            interior_gaps: entry
                .interior_gaps()
                .into_iter()
                .map(|(gap_start, gap_end)| CoverageGapView { gap_start, gap_end })
                .collect(),
        }
    }
}

/// Build the Axum router for the window surface and register its operations.
///
/// The declared error responses are the ones this path can produce: 401, 403, 503
/// (the PDP unreachable or the store unavailable, both fail closed) and 500. **No
/// 400** — the only input is a path `Uuid` axum itself rejects before the handler,
/// and there is no body and no query. **No 404**: a plan with no rows and no
/// windows is answered `200` with an empty key list, which is a state and not an
/// absent resource, and a plan outside the caller's scope reads the same way
/// rather than leaking whose plans exist. **No 422** — the design set's 422s are
/// architectural and reach the wire as 400s carrying their code
/// (`infra::error_mapping`).
pub fn router(state: Arc<GovernanceState>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-pricing/v1/price-windows")
        .operation_id("bss_pricing.list_price_windows")
        .summary("List the tenant's price windows (cursor-paginated)")
        .description(
            "One page of the tenant's price windows in `window_id` order, with an opaque `cursor` \
             and a `limit` whose server default is 100 and whose hard cap is 1,000 (D-125). \
             Narrow with `$filter=price_id eq <uuid>`. There is no `plan_id` filter: a window is \
             bound to a **row** and carries no plan reference, so that the row and its canonical \
             scope key cannot disagree - a plan's whole time axis is \
             `GET /bss-pricing/v1/plans/{planId}/coverage`, which answers it per key. Every \
             state is on the page, cancelled and expired included, because \"why did this key \
             lose its successor\" is a question about a plane rather than about a live row. Each \
             entry carries `mutationSeq`, which is the `If-Match` the next `PATCH` or `DELETE` \
             on that window must assert. Gates on `plan` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Windows per page (default 100, hard cap 1,000)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_price_windows)
        .with_odata_filter::<WindowFilterField>()
        .with_odata_orderby::<WindowFilterField>()
        .json_response_with_schema::<Page<WindowSummaryView>>(
            openapi,
            StatusCode::OK,
            "One page of the tenant's price windows.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(Router::new(), openapi);

    let router = OperationBuilder::get("/bss-pricing/v1/plans/{planId}/coverage")
        .operation_id("bss_pricing.get_plan_coverage")
        .summary("Read a plan's per-scope-key window coverage and gap report")
        .description(
            "The plan's time axis, one entry per canonical scope key: the key's window intervals \
             and states, its derived coverage end, whether it holds an active or scheduled window \
             at all, and every uncovered interval between two of its windows. The row set is the \
             publish check's own - the plan's `published` rows plus the `draft` rows a publish \
             would produce - so a key this report calls uncovered is exactly a key \
             `WINDOW_COVERAGE_MISSING` would name, under the same canonical rendering. A void \
             with no successor is never reported here: gap detection is an interior check and \
             cannot see one by construction. A plan with no rows and no windows is answered `200` \
             with an empty list; a plan outside the caller's scope reads the same way. Gates on \
             `plan` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan whose coverage is reported.")
        .handler(get_plan_coverage)
        .json_response_with_schema::<PlanCoverageView>(
            openapi,
            StatusCode::OK,
            "The plan's per-key coverage and gap report.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post("/bss-pricing/v1/prices/{priceId}/windows")
        .operation_id("bss_pricing.schedule_price_window")
        .summary("Schedule a window on a price row")
        .description(
            "Schedules a `[effectiveFrom, effectiveTo)` window on the row's canonical scope key \
             and answers `202` with the pending `CatalogVersion` handle. It is a **publish unit** \
             (D-99): the mutation re-projects the plan subject and is consumer-visible only at \
             `CatalogVersionPublished` + warm, so a `200` would claim the coverage changed for \
             readers when it has not. `effectiveFrom` must be strictly in the future \
             (`WINDOW_START_IN_PAST`), the interval must not intersect a scheduled or active \
             sibling on the same key (`WINDOW_OVERLAP`, 409 - siblings include every price row of \
             the plan on that key, not just this one), and the resulting interval set must open no \
             hole between two of the key's windows (`WINDOW_GAP`). Adjacency is legal: \
             `effectiveTo = next.effectiveFrom` is not a gap, the intervals being half-open. \
             \
             **An `Idempotency-Key` header is required and is honoured** (D-171, and S5's own \
             idempotency column for this surface): the same key with the same body returns the \
             first answer verbatim and mints no second window, and the same key with a different \
             body is `IDEMPOTENCY_PAYLOAD_MISMATCH` (409). What a key identifies is an **attempt**, \
             not an act - so retrying a request you never got an answer to is safe, and \
             **completing an act a reviewer has since approved is a new attempt and needs a new \
             key**. Re-issuing the refused request under its original key replays the refusal, \
             which is what a retry is for; the approved act commits when the same request arrives \
             under a fresh one. \
             \
             **A schedule is not on `inst-mat-registered`'s trigger list, so what decides whether \
             it needs a second principal is the tenant's approval-threshold policy.** It moves an \
             interval and no money, so its per-row delta is zero and reaches no bar - which leaves \
             `inst-mat-percurrency`'s fail-safe half as the only rule that can answer: a tenant \
             with no configured entry for the row's currency gets `outcome = \
             submitted_for_approval`, a pinned unit and **no window at all**. With an entry, \
             `outcome = mutated` and the body names the pending version handle. Gates on `plan` x \
             `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("priceId", "The price row the window applies to.")
        .param(idempotency_key_param())
        .json_request::<ScheduleWindowRequest>(openapi, "The interval and its reason code.")
        .handler(schedule_window)
        .json_response_with_schema::<WindowMutationOutcomeView>(
            openapi,
            StatusCode::ACCEPTED,
            "The window is scheduled, or a unit was opened and nothing was written; `outcome` says \
             which.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = mounted_window_mutations(router, openapi);

    let router = OperationBuilder::get("/bss-pricing/v1/plans/{planId}/sellability")
        .operation_id("bss_pricing.get_plan_sellability")
        .summary("Read a plan's sellability on one market at one instant")
        .description(
            "The sellability gate's surface, answered from the tenant's pin-eligible frontier. It \
             exposes **intervals, states and a derived coverage end per bound key, and never a \
             point-in-time boolean**: `at` is the caller's instant, so a consumer re-derives the \
             answer at another instant from the same frozen version and the time-driven window \
             transitions need no re-projection. Four of the six predicates are answered here - the \
             active-window-plus-coverage-horizon predicate, committed-version addressability, the \
             availability dates and the plan lifecycle state; the GA-gate flags and the registry \
             `sellable` flag answer `not_evaluable` and name the slice that owes each, so a \
             consumer can tell a false predicate from an unanswerable one. The verdict is the \
             conjunction over **every** key the purchase binds on the market, eligibility-resolved \
             and with grandfathered generations excluded: one failing key makes the plan-market \
             not sellable, never partially sellable, and a market the plan binds no key on is not \
             sellable either. `not_evaluable` is **not** a yes. All three query parameters are \
             required. A plan no pin-eligible version carries is answered `200` with the \
             committed-version predicate failed, which is also what a plan outside the caller's \
             scope reads. Gates on `plan` x `read`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan whose sellability is answered.")
        .query_param(
            "at",
            true,
            "The instant to evaluate at, RFC 3339 (required). Percent-encode a numeric offset: `+` \
             is a space under form-urlencoding, so `...+00:00` written literally arrives mangled \
             and is refused. `...Z` needs no encoding.",
        )
        .query_param(
            "currency",
            true,
            "The bound market's currency, ISO 4217 (required)",
        )
        .query_param("region", true, "The bound market's region (required)")
        .handler(get_plan_sellability)
        .json_response_with_schema::<PlanSellabilityView>(
            openapi,
            StatusCode::OK,
            "The plan's per-key sellability and the plan-market verdict.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-178's edge, applied where the routes are rather than where the routers
    // merge, so it travels with them: a mutation reachable without it cannot build
    // an `AuditStamp`, and `require_correlation` answers 500 rather than minting a
    // second value per record. The two GETs write nothing and need none, but
    // the layer is per-router and costs them only a header read.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// Register the two window mutations D-62 controls — the `PATCH` and the `DELETE`.
///
/// Split out of [`router`] because the two descriptions are long enough to put that
/// function over the 200-line lint, and split **here** rather than anywhere else
/// because these are the two operations that share a shape: both answer 202 twice,
/// both carry `inst-mat-registered`'s two-person control, and both hand back
/// [`WindowMutationOutcomeView`]. The five literals stay literals — DE0801 validates a
/// literal argument and silently passes a `const` one.
fn mounted_window_mutations(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::patch("/bss-pricing/v1/price-windows/{windowId}")
        .operation_id("bss_pricing.adjust_price_window")
        .summary("Move a window's future effectiveTo")
        .description(
            "Moves a **future** `effectiveTo` - shorten, extend, or drop the bound entirely by \
             omitting the field - under the `If-Match` precondition, and answers `202`. A \
             **publish unit** (D-99); a shorten is additionally always-material (D-62). Three \
             things are frozen and each answers `WINDOW_HISTORICAL_IMMUTABLE` (409): an `expired` \
             or `cancelled` window, a stored end that has already passed, and a target end that is \
             not in the future - the last two because rewriting an end that has passed rewrites \
             what was chargeable. A move that would leave the key uncovered with no successor is \
             `WINDOW_TRAILING_VOID`: the key must stay covered through the later of its current \
             coverage end and now plus the longest billing cycle sold on it. That check is \
             **fail-closed with no exemption** (D-182) - the D-79 Subscriptions lane that would \
             decide the exemption has no client, no contract type and no counterpart gear here, and \
             D-131 makes a lane that cannot answer the subscribers-present case. \
             \
             **A shortening move takes two principals** (D-62, `inst-mat-registered`): the first \
             call opens a pinned approval unit, answers `202` with \
             `outcome = submitted_for_approval` and **changes nothing** - the stored end is echoed \
             unmoved and no version handle is issued - and the move commits on the call made after \
             an independent principal approves that unit. An **extension** removes no coverage and \
             is not on the trigger list, so it is the threshold policy that decides it: with an \
             entry for the row's currency it commits on the first call (`outcome = mutated`), and \
             with none `inst-mat-failsafe` answers and it opens a unit like a shorten. The \
             sentence here used to say an extension commits on the first call full stop, which was \
             true only while no policy could exist. \
             \
             **The `If-Match` is compared** (D-191). It used to be required and discarded, which is \
             worse than not requiring it - a caller who sent it believed they were protected. Every \
             committed act answers with the `ETag` its successor must assert, so the tag a caller \
             needs is always the one they were last handed. Gates on `plan` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("windowId", "The window whose end is moved.")
        .param(if_match_param(
            "On a window route the tag is the window row's **own** version and nothing else, \
             because the path addresses one window by id - the same rule D-141 states for a price \
             row, and never the plan's tag. Here that version is the window's **act sequence** \
             (D-190): the number of operator acts it has been the subject of, which the activation \
             and expiry sweeps deliberately do not move. Copy it verbatim out of the `ETag` of the \
             answer to the act that scheduled or last moved this window - there is no `GET` on a \
             window, so those responses are where the tag comes from. A tag naming a sequence some \
             other act has consumed is `STALE_VERSION` (409), naming both.",
        ))
        .json_request::<AdjustWindowRequest>(openapi, "The new end, or nothing for open-ended.")
        .handler(adjust_window)
        .json_response_with_schema::<WindowMutationOutcomeView>(
            openapi,
            StatusCode::ACCEPTED,
            "The end moved, or a unit was opened and nothing moved; `outcome` says which.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/bss-pricing/v1/price-windows/{windowId}")
        .operation_id("bss_pricing.cancel_price_window")
        .summary("Cancel a not-yet-active window")
        .description(
            "Cancels a `scheduled` window and answers `202`, emitting `PriceWindowCancelled`. A \
             **publish unit** (D-99) and always-material (D-62). It is a **state flip, never a \
             deletion**: the store refuses `DELETE` on the row unconditionally, because a \
             cancelled schedule is a fact an auditor is entitled to see - which is also why a \
             second cancellation of one window is refused `WINDOW_NOT_CANCELLABLE` (409) rather \
             than answered `Ok`. Any state but `scheduled` gets that same refusal, and an active \
             window is shortened through its `effectiveTo` instead, which is the operation the \
             state machine leaves. A cancel that would leave the key uncovered with no successor is \
             `WINDOW_TRAILING_VOID`, fail-closed with no exemption (D-182). It takes **no** \
             `Idempotency-Key` and **no** `If-Match`: S5's idempotency column for this surface is \
             empty, and the refusal of a second cancellation is what stands in for one. \
             \
             **It takes two principals** (D-62, `inst-mat-registered`): the first call runs every \
             check, opens a pinned approval unit and answers `202` with \
             `outcome = submitted_for_approval` while the window **stays `scheduled`** - this is \
             the act D-62 exists to stop one operator performing alone, a cancelled successor \
             silently reverting an approved price change. The flip happens on the call made after \
             an independent principal approves the unit, and its audit record names that unit. \
             Gates on `plan` x `write`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("windowId", "The window to cancel.")
        .handler(cancel_window)
        .json_response_with_schema::<WindowMutationOutcomeView>(
            openapi,
            StatusCode::ACCEPTED,
            "The window is cancelled, or a unit was opened and nothing moved; `outcome` says which.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

/// The `plan x write` gate, spelled once for the three mutating routes.
///
/// `resource_id` is the **plan**, not the price row or the window, because that is
/// the object the catalog's endpoint map puts the action on: `plan x write` is a
/// grant over a plan, and asking the PDP about a window id it has no label for
/// would fail closed for every caller. Resolving the plan is therefore part of the
/// gate rather than part of the mutation, which is why both window handlers below
/// read their subject's key before they ask.
async fn window_write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    plan_id: PlanId,
    tenant: Uuid,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ Some(crate::authz::ResourceRef(plan_id.get())),
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// Resolve the plan a subject belongs to, under a scope the caller already holds.
///
/// # Why the mutating routes ask the PDP twice, and why both asks are `write`
///
/// `PATCH` and `DELETE` address `/price-windows/{windowId}` — §5's shape, and the
/// right one, a window id being unique on its own. But the `plan x write` gate
/// needs a **plan** id as its `resource_id`, because that is the object the authz
/// catalog's endpoint map puts the action on; asking the PDP about a window id it
/// carries no label for would fail closed for every caller. So something has to
/// turn one id into the other before the per-plan gate can be asked.
///
/// That lookup is a read of the tenant's catalog, so it must run under a compiled
/// scope: unscoped, a principal could tell from the difference between `404` and
/// `403` which window ids exist in tenants they cannot see. The scope it runs under
/// is a **tenant-level `plan x write`** — `resource_id: None` — and *not* a
/// `plan x read`, for two reasons that point the same way. It is the honest
/// question ("may this principal write plans here at all?"), and it keeps the
/// route's **first** PDP ask equal to the pair the catalog names for it, which is
/// the property `every_route_asks_the_catalogued_pair` exists to hold. A route
/// whose first ask were `read` would behave identically under a fixture granting
/// both and one granting neither, and only what it *asked* would separate them.
///
/// The second ask then narrows to the resolved plan. Two asks of one pair is one
/// gate evaluated twice, never a gate widened.
async fn resolve_plan(
    state: &GovernanceState,
    scope: &AccessScope,
    tenant: Uuid,
    subject: Lookup,
) -> Result<PlanId, CanonicalError> {
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: window lookup: {e}")).create()
    })?;
    let key = match subject {
        Lookup::Price(price_id) => price_repo::load_scope_key(&conn, scope, tenant, price_id)
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?
            .ok_or_else(|| {
                CanonicalError::from(crate::domain::error::DomainError::NotFound {
                    subject: "price row".to_owned(),
                    id: price_id.to_string(),
                })
            })?,
        Lookup::Window(window_id) => {
            window_repo::find(&conn, scope, tenant, window_id)
                .await
                .map_err(|e| CanonicalError::from(repo_failure(&e)))?
                .ok_or_else(|| {
                    CanonicalError::from(crate::domain::error::DomainError::NotFound {
                        subject: "price window".to_owned(),
                        id: window_id.to_string(),
                    })
                })?
                .scope_key
        }
    };
    Ok(key.plan_id())
}

/// The tenant-level `plan x write` scope the subject lookup runs under.
async fn tenant_write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    tenant: Uuid,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// Which identifier the path gave us.
#[derive(Clone, Copy, Debug)]
enum Lookup {
    Price(Uuid),
    Window(Uuid),
}

/// `POST /prices/{priceId}/windows`: schedule a window, answering 202.
async fn schedule_window(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    correlation: Option<Extension<CorrelationId>>,
    Path(price_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(correlation)?;
    let tenant = ctx.subject_tenant_id();
    let coarse = tenant_write_scope(&enforcer, &ctx, tenant).await?;
    let plan_id = resolve_plan(&state, &coarse, tenant, Lookup::Price(price_id)).await?;
    let scope = window_write_scope(&enforcer, &ctx, plan_id, tenant).await?;
    // **After the gate**, `api/rest.rs`'s stated discipline. Reading the key and
    // parsing the body first — on the argument that a caller who omitted the key
    // should be told that rather than that a price id does not exist — is sound
    // about the *lookup* and wrong about the *gate*: a caller with no authority on
    // the window plane is answered 400 where the two rules written elsewhere in this
    // layer both say 403, and the PDP is asked only on the well-formed path, so the
    // census's recorded question sequence for the route is a claim about a subset of
    // requests.
    let key = preconditions::idempotency_key(&headers)?;
    let request: ScheduleWindowRequest = preconditions::parse_body(&body)?;
    let digest = preconditions::request_digest(&request)?;

    let now = OffsetDateTime::now_utc();
    let stamp = crate::api::rest::auth_context::audit_stamp(&ctx, now, correlation);
    let windows = state.windows.clone();
    let mutation_ctx = ctx.clone();
    let mutation_scope = scope.clone();
    let effective_from = request.effective_from;
    let effective_to = request.effective_to;
    // The third writer of `pricing_price_window.reason_code`, and guarded for the
    // reason `cutovers` and `supersessions` are: the column is frozen by the table's
    // append-only trigger, so a blank or unbounded value is what an auditor reads
    // forever. A door on two of three writers is a door that reads as closed.
    crate::api::rest::require_reason_code(&request.reason_code)?;
    let reason_code = request.reason_code.clone();

    let guarded = idempotent::guarded(
        &state.db,
        &state.idempotency,
        &scope,
        GuardedRequest {
            operation: SCHEDULE_WINDOW_OPERATION,
            client_key: key,
            request_hash: digest,
            tenant_id: tenant,
            status: StatusCode::ACCEPTED.as_u16().into(),
            now,
        },
        move |txn| {
            Box::pin(async move {
                // Minted **inside** the guarded body, which is `plans.rs`'s and
                // `prices.rs`'s stated rule and not a preference: an id minted outside
                // would be a second one nobody is ever told about, since a replay does
                // not reach this closure at all. Server-minted either way, like a price
                // row's id and for the same reason — the surface has to name the window
                // in its answer before the row is durable.
                let window_id = Uuid::now_v7();
                windows
                    .schedule_in(
                        txn,
                        &mutation_ctx,
                        &mutation_scope,
                        tenant,
                        price_id,
                        window_id,
                        effective_from,
                        effective_to,
                        reason_code,
                        verdict_json,
                        stamp,
                    )
                    .await
            })
        },
        |outcome| body_of(&view_of(outcome)),
    )
    .await?;

    match guarded {
        Guarded::Performed(outcome) => Ok(answer(outcome)),
        // The stored status and body, verbatim. **No `ETag`**, for
        // `api::rest::prices::replayed`'s reason: the dedup row holds a status and a
        // body and no headers, and the window's act sequence may have moved since — a
        // tag rebuilt from a stored body would be a precondition token that looks valid
        // and is not.
        Guarded::Replayed { status, body } => {
            Ok(replayed(SCHEDULE_WINDOW_OPERATION, status, &body)?)
        }
    }
}

/// `PATCH /price-windows/{windowId}`: move a future end, answering 202.
async fn adjust_window(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    correlation: Option<Extension<CorrelationId>>,
    Path(window_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(correlation)?;
    let tenant = ctx.subject_tenant_id();
    let coarse = tenant_write_scope(&enforcer, &ctx, tenant).await?;
    let plan_id = resolve_plan(&state, &coarse, tenant, Lookup::Window(window_id)).await?;
    let scope = window_write_scope(&enforcer, &ctx, plan_id, tenant).await?;
    // **After the gate**, with its thirteen sibling `If-Match` consumers rather
    // than against them; see `schedule_window` above for the argument that moved.
    // Required and parsed here; the comparison itself is the store's, inside the
    // writing transaction (D-176 — a tag compared out here is a hint), where it rides
    // the `UPDATE`'s own `WHERE` over the window's act sequence (D-191).
    let expected = preconditions::if_match(&headers)?;
    let request: AdjustWindowRequest = preconditions::parse_body(&body)?;

    let stamp =
        crate::api::rest::auth_context::audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    let outcome = state
        .windows
        .adjust_effective_to(
            &ctx,
            &scope,
            tenant,
            window_id,
            request.effective_to,
            expected.get(),
            verdict_json,
            stamp,
        )
        .await?;
    Ok(answer(outcome))
}

/// `DELETE /price-windows/{windowId}`: cancel a scheduled window, answering 202.
///
/// **No header preconditions at all**, which is §5's Idempotency column for this
/// surface — see the module doc for why that is reported as a divergence rather
/// than improved on.
async fn cancel_window(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    correlation: Option<Extension<CorrelationId>>,
    Path(window_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(correlation)?;
    let tenant = ctx.subject_tenant_id();
    let coarse = tenant_write_scope(&enforcer, &ctx, tenant).await?;
    let plan_id = resolve_plan(&state, &coarse, tenant, Lookup::Window(window_id)).await?;
    let scope = window_write_scope(&enforcer, &ctx, plan_id, tenant).await?;

    let stamp =
        crate::api::rest::auth_context::audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);
    let outcome = state
        .windows
        .cancel(&ctx, &scope, tenant, window_id, verdict_json, stamp)
        .await?;
    Ok(answer(outcome))
}

/// Render a window mutation's outcome, and on the committed arm emit its `ETag`.
///
/// **It opens nothing.** The unit a controlled act needs is opened inside the
/// mutating transaction, by `infra::window`'s `mutate_in`, whose own doc carries the
/// argument for that location: under D-191's idempotency gate the body the first
/// caller was sent has to be the body the gate recorded, and on the refused arm that
/// body names the approval — so a unit opened after the guarded transaction committed
/// would be missing from the replay.
///
/// **This function opens nothing**, and the signature is the proof: a synchronous
/// function over one owned outcome, holding no `SecurityContext`, no
/// [`AccessScope`] and no service, can open no approval unit. Claiming the opening
/// here puts two module docs on opposite locations for one act, and sends a reader
/// tracing D-62's two-person control to a pure renderer.
///
/// Both arms answer **202**; [`WindowMutationOutcomeView`] says why.
fn answer(outcome: WindowMutationOutcome) -> Response {
    let view = view_of(&outcome);
    match outcome {
        // **The tag, and this is its only producer.** There is no `GET` on a window
        // (D-191 clause (2)), so a `PATCH` that demanded a precondition no response ever
        // emitted would be requiring a header its own caller cannot obtain. Every
        // committed act therefore hands back the sequence the window now stands at,
        // which is the tag the next act asserts.
        WindowMutationOutcome::Committed(receipt) => (
            StatusCode::ACCEPTED,
            [(
                ETAG,
                preconditions::etag(RowVersion::new(receipt.mutation_seq)),
            )],
            Json(view),
        )
            .into_response(),
        // **No `ETag` on this arm, deliberately.** Nothing was written, so the window
        // stands at the sequence the caller already asserted; emitting it again would
        // suggest an act had advanced it. On a refused *schedule* there is no window at
        // all and so no sequence to name.
        WindowMutationOutcome::SubmittedForApproval(_) => {
            (StatusCode::ACCEPTED, Json(view)).into_response()
        }
    }
}

/// The operation this route's client keys are scoped to.
///
/// A per-route constant, so one client key used on two different verbs does not
/// collide — `pricing_idempotency_dedup`'s key is `(tenant, operation, client_key)`.
const SCHEDULE_WINDOW_OPERATION: &str = "bss_pricing.schedule_price_window";

/// The outcome as the wire renders it.
///
/// One producer, shared by the answer this attempt sends and the body the gate
/// **records**, because those two must be the same document: a replay hands back the
/// stored body verbatim, so anything the recorded body lacked the second caller would
/// never be told. `infra::idempotent`'s module doc makes that the reason the renderer
/// is the caller's at all.
fn view_of(outcome: &WindowMutationOutcome) -> WindowMutationOutcomeView {
    match outcome {
        WindowMutationOutcome::Committed(receipt) => WindowMutationOutcomeView {
            outcome: OUTCOME_MUTATED.to_owned(),
            window: WindowMutationView::from(receipt.as_ref().clone()),
            materiality: None,
            approval: None,
        },
        WindowMutationOutcome::SubmittedForApproval(pending) => WindowMutationOutcomeView {
            outcome: crate::api::rest::publish::OUTCOME_SUBMITTED.to_owned(),
            window: WindowMutationView::from(pending.as_ref()),
            materiality: Some(MaterialityView::from(&pending.verdict)),
            approval: Some(ApprovalView::from(&pending.approval)),
        },
    }
}

/// The recorded body: [`view_of`]'s document as JSON.
///
/// # Errors
/// [`DomainError::Internal`] when the view will not serialize — unreachable, and
/// reported rather than unwrapped, because it would otherwise abort a transaction that
/// has already written.
fn body_of(view: &WindowMutationOutcomeView) -> Result<serde_json::Value, DomainError> {
    serde_json::to_value(view)
        .map_err(|e| DomainError::Internal(format!("cannot render the window outcome: {e}")))
}

/// The answer a replay is handed back — the stored status and body, verbatim.
///
/// # Errors
/// [`DomainError::Internal`] when the stored status is not one — see
/// [`super::replayed_status`].
fn replayed(
    operation: &str,
    status: i32,
    body: &serde_json::Value,
) -> Result<Response, DomainError> {
    let status = super::replayed_status(operation, status)?;
    Ok((status, Json(body.clone())).into_response())
}

/// The stored `materiality` document a window unit carries.
///
/// Handed to `infra::window` as a function pointer rather than called from here,
/// because the verdict is minted **inside** the mutating transaction and the unit is
/// opened there too (D-191). DE0202 forbids `infra` from naming [`MaterialityView`],
/// so the rendering stays here — one producer of the document, whichever path opens
/// the unit.
///
/// **Public because the service is reachable without a route.** The suites that drive
/// `WindowService` directly need the same rendering the routes use; a second one written
/// in a test would let the stored document drift from the one a reviewer reads, which is
/// the drift this function exists to prevent.
///
/// # Errors
/// [`DomainError::Internal`] when the view will not serialize, which is unreachable
/// and reported rather than unwrapped.
pub fn verdict_json(
    verdict: &crate::domain::materiality::MaterialityVerdict,
) -> Result<serde_json::Value, DomainError> {
    serde_json::to_value(MaterialityView::from(verdict))
        .map_err(|e| DomainError::Internal(format!("cannot render the materiality verdict: {e}")))
}

/// `GET /price-windows`.
///
/// The collection read, gated on the same `plan` x `read` pair every other read
/// on this surface asks for — `resource_id` is `None` because there is no single
/// resource to name, so what the PDP compiles is the tenant filter the whole walk
/// runs under, and `require_constraints` is `true` so an unconstrained allow
/// fail-closes rather than paging through every tenant's window plane.
///
/// **No `plan_id` parameter**: the store has no plan reference on a window and
/// this handler invents no join to fabricate one. Narrow with
/// `$filter=price_id eq <uuid>`.
#[allow(clippy::implicit_hasher)]
async fn list_price_windows(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<WindowSummaryView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: price window listing: {e}")).create()
    })?;
    let page = window_repo::list_odata(&conn, &scope, ctx.subject_tenant_id(), &odata)
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.iter().map(WindowSummaryView::from).collect(),
        page_info: page.page_info,
    }))
}

/// `GET /plans/{planId}/coverage`: the per-key coverage report.
///
/// `owner_tenant_id` is `None` — this is a read, so the PDP derives the scope from
/// the subject and its roles rather than trusting a caller-supplied tenant, and
/// the compiled scope is the SQL filter. `resource_id` names the plan, which is
/// what makes the gate answerable per plan rather than per tenant, and
/// `require_constraints = true` so an unconstrained allow fail-closes instead of
/// exposing every tenant's catalog. The pair is `plan` x `read`, the same one
/// `list_plan_prices` asks for, and **no new authz vocabulary**: the catalog
/// already carries every label and action this needs.
async fn get_plan_coverage(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(plan_id): Path<Uuid>,
) -> Result<Json<PlanCoverageView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let plan_id = PlanId::new(plan_id);
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ Some(crate::authz::ResourceRef(plan_id.get())),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let tenant = ctx.subject_tenant_id();
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: plan coverage: {e}")).create()
    })?;

    let report = plan_coverage(&conn, &scope, tenant, plan_id).await?;

    Ok(Json(PlanCoverageView {
        plan_id: plan_id.get(),
        keys: report.keys.iter().map(KeyCoverageView::from).collect(),
    }))
}

/// The plan's coverage, read from the store.
///
/// **The billable key set and the window plane are read separately and neither
/// filters the other**, which is `coverage::check`'s own contract: a key a
/// billable row sits on is reported even with no window (uncovered), and a key
/// only a window mentions is reported too, because "why did this key lose its
/// successor" is a question about a plane rather than about a row.
///
/// The window read is `window_repo::list_for_plan` taken whole — every state, over
/// every price row of the plan whatever its lifecycle state — for the reason
/// `infra::publish::window_plane` states at length: coverage is validation over a
/// hypothetical, and the readers that restrict to `PROJECTED_ROW_STATES` are the
/// ones asserting facts to a consumer.
async fn plan_coverage(
    runner: &impl DBRunner,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) -> Result<CoverageReport, CanonicalError> {
    let rows = price_repo::load_for_plan(runner, scope, tenant, plan_id, CANDIDATE_ROW_STATES)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let billable: Vec<_> = rows
        .iter()
        .filter(|record| coverage::is_billable(record))
        .map(|record| record.scope_key.clone())
        .collect();

    let records = window_repo::list_for_plan(runner, scope, tenant, plan_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let windows = crate::domain::window::group_by_key_seeded(
        billable.iter().cloned(),
        records.into_iter().map(|w| {
            (
                w.scope_key,
                WindowInterval::new(w.effective_from, w.effective_to, w.state),
            )
        }),
    );

    Ok(coverage::check(&billable, &windows))
}

/// `GET /plans/{planId}/sellability`: the gate's surface on one market at one
/// instant.
///
/// The gate is `plan` x `read`, the same pair `get_plan_coverage` and
/// `list_plan_prices` ask for, and **no new authz vocabulary**: the catalog
/// already carries every label and action this needs.
///
/// # Which version it answers from, and why there is no `version=` parameter
///
/// §5's signature is `?at=&currency=&region=` and names **no version**, so the
/// surface reads the tenant's own **pin-eligible frontier** and resolves the plan's
/// subject at it (§4.4). Adding a parameter would be minting a surface the design
/// set does not declare, and it would let a caller ask about a version they are not
/// entitled to pin.
///
/// The consequence is a property that cannot be asserted through this route: *"a
/// consumer pinned to the pre-cancel version still reports the old coverage"* is
/// about two versions at once, and this surface only ever answers at one. It is
/// therefore asserted **at the store**, against two stored payloads, in
/// `tests/sqlite_sellability.rs` - said here so the missing parameter reads as the
/// design set's shape rather than as an oversight.
///
/// # The frontier and the delta are two reads, and that is not a race
///
/// If the frontier advances between them the answer is computed at the older
/// version, which is a version the caller could legitimately have pinned a moment
/// earlier - an older answer, never an incoherent one. A frozen version never
/// changes what it says, which is what makes the pair safe to read apart.
async fn get_plan_sellability(
    Extension(state): Extension<Arc<GovernanceState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(plan_id): Path<Uuid>,
    Query(query): Query<SellabilityQuery>,
) -> Result<Json<PlanSellabilityView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let plan_id = PlanId::new(plan_id);
    // **Parsed before the gate is asked, and this is the layer's one stated
    // exception to gate-before-parse** — `api::rest`'s "Gate before parse, and the
    // one stated exception", which names this route and rules on it. The argument
    // is about the *subject*: this gate is `plan × read` over a **market**, and a
    // caller who omitted `currency` has asked a question that names no subject for
    // a gate to be about. It is an exception because it is argued, not because it
    // is a read.
    //
    // Nothing here is exploitable, which is the half a reader has to be able to
    // check: `market_of` reads the query and nothing else, so the 400 is identical
    // for an authorized and an unauthorized caller and identical whether or not the
    // plan exists — there is no 400-vs-403 or 400-vs-404 oracle, and a caller
    // probing their own authority need only send the well-formed query, which is
    // free. `rest_authz.rs` pins both halves.
    //
    // Not "the shape `schedule_window` reads its idempotency key in":
    // `schedule_window` gates first (see its own note), so citing it here points at
    // a precedent reversed for the opposite reason — a caller with no authority on
    // the window plane being answered 400 where the layer's rules say 403. The
    // exception rests on the market argument alone.
    let (at, currency, region) = market_of(&query)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ Some(crate::authz::ResourceRef(plan_id.get())),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let tenant = ctx.subject_tenant_id();
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::internal(format!("bss-pricing: plan sellability: {e}")).create()
    })?;

    let facts = sellability_facts(&conn, &scope, tenant, plan_id).await?;
    let surface = SellabilitySurface::of_delta(&facts, at, &currency, &region);
    Ok(Json(PlanSellabilityView::from(&surface)))
}

/// The three query parameters, or the refusal that names the missing one.
///
/// **All three are required**, which §5 does not state either way and is decided
/// here. The market is what the answer is *about*, so neither half of it has a
/// defensible default; and `at` is required rather than defaulted to this server's
/// clock because the whole contract of this surface is that the instant is the
/// caller's - a defaulted one would put the server's clock inside an answer the
/// caller reads as being about their own moment. An absent or unparseable value is
/// a malformed request under the validation envelope: **no new code**, which is the
/// rule D-171 clause (5) already states for an absent precondition.
///
/// The D-144 millisecond quantum is deliberately **not** checked here. It governs
/// *authored* instants - the ones this gear stores and later matches for equality -
/// and `at` is neither stored nor matched; refusing a finer-grained read parameter
/// would spend a validation on a value that leaves no trace.
///
/// # A numeric offset has to be percent-encoded, and that is refused rather than
/// repaired
///
/// `+` is a **space** under form-urlencoding, so an `at` written `...+00:00`
/// literally in a query string arrives as `...00:00` with a space where the sign
/// was - a string that is not an instant in any format. It is refused: repairing it
/// would mean guessing which character the caller meant, and answering about a
/// moment they did not name. `...Z` needs no encoding and is what the parameter's
/// own description tells a client to send.
fn market_of(
    query: &SellabilityQuery,
) -> Result<(OffsetDateTime, CurrencyCode, Region), DomainError> {
    let required = |value: &Option<String>, name: &str| {
        value.clone().ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "the `{name}` query parameter is required on the sellability surface"
            ))
        })
    };
    let at = required(&query.at, "at")?;
    let at = OffsetDateTime::parse(&at, &::time::format_description::well_known::Rfc3339).map_err(
        |e| {
            DomainError::InvalidRequest(format!(
                "the `at` query parameter is not an RFC 3339 instant: {e}"
            ))
        },
    )?;
    Ok((
        at,
        CurrencyCode::new(&required(&query.currency, "currency")?)?,
        Region::new(&required(&query.region, "region")?)?,
    ))
}

/// The plan's facts at the tenant's pin-eligible frontier.
///
/// **Both absences answer the same way** - a tenant with no frontier at all, and a
/// frontier that carries no delta for this plan - because they are one fact to a
/// consumer: this pin does not carry this plan's content. The surface answers
/// predicate (2) failed for it, which is the fail-closed direction, rather than
/// synthesising a plan with no rows (a different and answerable statement) or a
/// 404 (which would tell an unauthorized caller which plan ids exist).
async fn sellability_facts(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) -> Result<SellabilityFacts, CanonicalError> {
    let Some(frontier) = pin_frontier_repo::read_at(runner, scope, tenant)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
    else {
        return Ok(SellabilityFacts::NotAddressable { plan_id });
    };
    let Some(delta) = read_model_repo::delta_at(
        runner,
        scope,
        tenant,
        &SubjectRef::Plan(plan_id.get()),
        frontier.catalog_version,
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?
    else {
        return Ok(SellabilityFacts::NotAddressable { plan_id });
    };
    read_model_repo::sellability_facts(&delta).map_err(|e| CanonicalError::from(repo_failure(&e)))
}
