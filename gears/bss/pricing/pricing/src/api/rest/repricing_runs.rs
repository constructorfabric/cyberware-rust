//! The mass-repricing run's two surfaces
//! (`design/12-operator-efficiency.md` §5, §3 `inst-mr-api`, `inst-mr-journal`,
//! `inst-mr-return`, `inst-mp-journal`, `inst-mp-grandfathered`; D-88, D-134,
//! D-261, D-293, D-307).
//!
//! # What a run built here does, and — as plainly as possible — what it does not
//!
//! `POST /repricing-runs` validates the changeover instant, expands the selector
//! over the tenant's **published** rows, refuses `RUN_SELECTOR_EMPTY` if that
//! expansion is empty, opens a [`BulkKind::Repricing`] run, freezes the row set
//! into `pricing_repricing_journal`, evaluates the run's materiality **once**
//! over every plan it touches (`inst-mr-coalesce`), and writes the edge the
//! verdict names: `validating -> awaiting_approval` under an opened approval
//! unit when material, `validating -> committing` otherwise.
//!
//! **And then it hands the apply to a lane and answers.** On the non-material path
//! `open_run` has already taken the run to `committing`; this handler enqueues that
//! run's apply on [`crate::infra::repricing::RunApplyLane`] and answers `202` with
//! the run standing there, so the `202` means *accepted and being applied* rather
//! than *applied*. `POST /repricing-runs/{runId}/abort` is what makes handing back
//! `committing` safe: it ends a run whose apply never ran. A **material** run is
//! left where `open_run` put it and its apply belongs to `api::rest::approvals`,
//! enqueued on the same lane when a second principal approves the batch unit.
//!
//! **The apply is therefore not synchronous with any request, and
//! `GET /repricing-runs/{runId}` is where its outcome is read** (`inst-mr-return`
//! makes that read the progress endpoint for exactly this reason). A `pending`
//! journal row in that read still means the row was not attempted rather than
//! attempted and unchanged (D-261) — what changed is that an operator may see the
//! whole journal `pending` for a moment after a `202`, which is a run the applier
//! has not reached yet and not a run that stalled.
//!
//! **A module's own doc is a claim; the call sites are the measurement.** "What it
//! still does not do is apply a price… a `committing` run advances no further than
//! that state, with every journal row still `pending`" was false of this file while
//! an `apply_run_in` call sat in it — and it was read at face value and repeated
//! into the operator-facing UI, which then told an author that opening a run moves
//! no price.
//!
//! The debts that do remain are named rather than left to be discovered:
//!
//! * **The redrive.** The surface is a create, a read and an abort
//!   ([`REPRICING_RUN_ABORT`], `infra::repricing::abandon_committing_run`), so a
//!   run stalled `committing` has a door out — it *ends*, with its unreached rows
//!   marked `failed`. What is still owed is the door that would drive such a run
//!   *forward* instead: `apply_run_in` is written to be re-entered and
//!   `bulk_repo::take_locks` is idempotent for its own run, so the capability is a
//!   route away, and it needs no check of its own for `inst-co-single-pending`:
//!   `infra::repricing::refuse_targets_on_a_held_key` re-asks it on every arrival
//!   to the apply, which is what the lane made necessary and what a redrive would
//!   be one more of.
//! * **The apply's own limits** (`inst-mr-apply`, `inst-mr-validate-scope`) —
//!   successor rows, their outbox records, the journal's `pending -> applied`
//!   flips in one commit per plan, the re-run of the row-local and
//!   plan-aggregate rule sets, and the bulk lock over the run's
//!   own rows — live in [`crate::infra::repricing::apply_run_in`]; see that
//!   module's own doc for what the lock does and, as plainly, does not close.
//!   **Materiality does not wait on any of that.** The
//!   `ChangeSet` [`run_materiality`] evaluates carries each selected row with
//!   the run's own adjustment already applied to it ([`project_row`]) — real
//!   arithmetic over the row's published amount, not the apply's durable
//!   successor and not a zero-delta stand-in — compared against the same rows
//!   as currently published. The per-currency comparison
//!   (`inst-mat-percurrency`, `inst-mr-coalesce`'s "any row over its
//!   own-currency threshold trips the run") is therefore real: a run's own
//!   adjustment size decides materiality together with the configured policy,
//!   not the fail-safe alone.
//! * **`project_row`'s one open corner**, named rather than papered over, and
//!   **restated**: it is a property of a **rate-priced** row, not of
//!   currency coverage, and it belongs to all three rate kinds rather than two.
//!   An `amount` markup/discount or a `fixed` line on a `per_unit` row's
//!   `unit_rate` or on a `graduated`/`volume` row's bands has nowhere honest to
//!   record "not computable": `project_rate` refuses both magnitudes outright —
//!   whether or not the adjustment's `AmountSet` covers the row's currency,
//!   since a rate has no minor-unit floor to receive a currency delta against —
//!   and [`project_row`] reads that `None` as "leave the rate published". The
//!   escape `flat`/`package` have is not available: `TierBand::unit_price_rate`
//!   is a bare `RateMinor` with no absent state at all, and clearing a
//!   `per_unit` row's `unit_rate` — which *is* an `Option` — would author a row
//!   `rules::model_kind::check_amount_placement` refuses, since D-311 makes
//!   that field the one place such a row's money lives. So [`run_materiality`]
//!   measures a **zero** delta over such a row where `flat`/`package` would
//!   have reported `RowDelta::NotComputable` and been material regardless of
//!   threshold.
//!
//!   **What bounds it** is `infra::repricing::apply_rows_in`'s D-311 refusal,
//!   which covers all three rate kinds: a run whose
//!   adjustment cannot move a rate fails that row's whole plan
//!   (`inst-mr-apply`, D-134) and publishes nothing, so an understated verdict
//!   never becomes an unapproved change to a consumer-visible price — it is a
//!   run that reaches `completed_with_conflicts` having applied nothing, which
//!   is the outcome `rest_repricing_runs` pins. What is still unbuilt is a
//!   [`crate::domain::materiality::delta::RowDelta`] arm that says so, or a
//!   submit-time `ADJUSTMENT_CURRENCY_NOT_COVERED`-style refusal (the overlay
//!   plane's own check, unwired here) that would refuse the run at its own
//!   door instead of at the apply's — both decisions beyond this task's brief.
//! * **D-158's audit alignment:** opening a run appends a
//!   `create` record on the run's own chain
//!   (`audit_repo::bulk_operation_chain`), `subject_ref` its `operation_id`
//!   (`audit_repo::bulk_operation_ref`). The bulk import's open owes the
//!   identical record still; this change gave the token a writer on the
//!   repricing side only.
//! * **the two edges:** `chk_pricing_approval_subject_kind`
//!   admitted `bulk_operation` as of `pricing_migration`; `open_run_in` is now
//!   the writer that spends it, opening the approval unit with the identical
//!   `subject_kind`/`subject_ref` pair the `create` record above already
//!   carries (D-158).
//! * **Paid (`inst-mp-pending`, D-35's first half):** a selected row whose
//!   canonical key already holds a pending **interactive** unit is failed *per
//!   row* at selector time, naming that unit, and its key is withheld from what
//!   the run's own batch approval pins — see [`refuse_rows_on_a_held_key`]. This
//!   paragraph previously recorded the opposite as owed, and the cost was larger
//!   than "the per-row split does not exist yet" suggests: the run's own unit
//!   pinned every selected key, so one held key collided on
//!   `uq_pricing_approval_key_pending` and refused the **whole** `POST` — no run
//!   opened, and every other row of the batch was refused with it. D-35 decided
//!   both halves on one day and only the pinning half had been built, which is
//!   the shape where half a decision is worse than neither.
//! * **`inst-mp-grandfathered` clause 2** owes the per-row refusal of an
//!   explicitly-selected grandfathered row. The *selector* half is built (see
//!   [`RunSelector::admits_grandfathered`]): a selector that does not name the
//!   eligibility axis excludes the class. A selector that names it outright still
//!   expands over those rows and freezes them into the journal — because dropping
//!   them would be the silent skip the clause forbids, and a journal row cannot be
//!   *born* `failed` (D-261: the insert trigger admits only `pending`). The apply
//!   is therefore the only place that refusal can be written.
//!
//! # `run_id` is the idempotency column, and it is also the address
//!
//! §5's Idempotency cell for `POST /repricing-runs` is `run_id` — not a client
//! key header, which is why this surface declares **no** `Idempotency-Key` and no
//! `If-Match`. The run's `client_key` column holds it, so the store's
//! `(tenant_id, kind, client_key)` unique index is what makes a second `POST`
//! under one `run_id` answer the run it opened, exactly as
//! [`crate::api::rest::bulk_imports`] replays its own key.
//!
//! `GET /repricing-runs/{runId}` addresses the run by that same `run_id` rather
//! than by the server-minted `operation_id`, and that is the substantive choice
//! here. `inst-mr-return` makes the `GET` the progress endpoint; the caller who
//! needs it most is the one whose `POST` timed out and who therefore never saw a
//! response — and a progress endpoint keyed on an id that only the response
//! carries is unreachable for exactly that caller. Both ids are on the view, so
//! nothing is hidden.
//!
//! **The two spellings of "run id" are not the same value, and the collision is
//! the schema's rather than this module's.**
//! `pricing_repricing_journal.run_id` is a foreign key to
//! `pricing_bulk_operation.operation_id` — the *minted* id — while the API's
//! `run_id` is the caller's token in `client_key`. Nothing here can rename a
//! column; what it can do is never pass one where the other belongs, which is why
//! every journal call below takes `run.operation_id` explicitly.
//!
//! # The instant is checked at [`ChangeoverMoment::Submit`] and nowhere else here
//!
//! `inst-mr-api` gives the run's changeover the same two floors `inst-su-instant`
//! gives a supersession: strictly future at submit, and a whole
//! [`MAX_BATCHING_DELAY`](crate::domain::supersession::MAX_BATCHING_DELAY) ahead
//! at the **approval commit**. Only the first is this arm's — the second belongs
//! to a commit that does not exist — and the floor is
//! [`check_changeover_instant`]'s rather than a second copy of the bound, because
//! a hand-maintained second floor is how two mechanisms come to disagree about one
//! SLO. D-144's quantum is checked first, in `plan_supersession`'s order and for a
//! sharper version of its reason: the run freezes **one** instant for every
//! selected row, so a microsecond of precision that survived to the apply would
//! not fail one supersession, it would fail all of them.
//!
//! # `Bytes`, not `Json<T>`
//!
//! The body is parsed **after** the authz gate, this directory's standing
//! discipline: `Json<T>` runs as an extractor before the handler, so a caller
//! outside the scope with a malformed body would learn their body was malformed
//! instead of learning they were denied.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::StatusCode};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::approvals::MaterialityView;
use crate::api::rest::auth_context::require_authenticated;
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::overlays::AmountRequest;
use crate::api::rest::preconditions;
use crate::api::rest::prices::optional_token;
use crate::api::rest::state::AuthoringState;
use crate::domain::approval::content_pin::repricing_run_content_hash;
use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::bulk::{BulkKind, BulkState};
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::instant::rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::{self, ChangeSet, MaterialityVerdict, PublishedPriceBaseline};
use crate::domain::money::CurrencyCode;
use crate::domain::overlay::Adjustment;
use crate::domain::repricing::RunSelector;
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region,
};
use crate::domain::supersession::{ChangeoverMoment, check_changeover_instant};
use crate::infra::storage::RepoError;
use crate::infra::storage::repo::repricing_journal_repo::NewJournalRow;
use crate::infra::storage::repo::{
    BulkOperationRecord, NewAuditEntry, NewBulkOperation, approval_repo, audit_repo, bulk_repo,
    price_repo, repricing_journal_repo,
};
use crate::infra::storage::repo_failure;
use crate::infra::threshold::effective_policy_at;
use time::OffsetDateTime;

const TAG: &str = "BSS Pricing Mass Repricing";

/// `POST` — open a mass-repricing run.
pub const REPRICING_RUNS: &str = "/bss-pricing/v1/repricing-runs";

/// `GET` — the run and its journal (`inst-mr-return`'s progress endpoint).
pub const REPRICING_RUN: &str = "/bss-pricing/v1/repricing-runs/{runId}";

/// `POST` — stop a run stalled mid-apply, releasing its bulk lock (D-37).
///
/// Addressed by the caller's `run_id` like [`REPRICING_RUN`] and never by the
/// minted `operation_id`, for that constant's stated reason: the caller who most
/// needs this door is the one whose `POST` timed out and who therefore never saw
/// the minted id.
pub const REPRICING_RUN_ABORT: &str = "/bss-pricing/v1/repricing-runs/{runId}/abort";

/// Which published rows the run acts on: any subset of the canonical key's axes.
///
/// **Spelled as [`ScopeKeyRequest`] spells them**, field for field, with two
/// differences that are both the difference between a key and a filter: every
/// member is optional, because an absent axis is one the run does not constrain;
/// and the usage pair `(meter, dimensionKey)` is here, because D-196 authors that
/// pair on a row's *content* while a selector is choosing among rows already
/// filed under it.
///
/// `plan_id` is a member rather than a path segment for
/// [`BulkImportRowRequest`](crate::api::rest::bulk_imports::BulkImportRowRequest)'s
/// reason: a run may span plans, and `pricing_bulk_operation` carries a tenant and
/// no plan.
///
/// [`ScopeKeyRequest`]: crate::api::rest::prices::ScopeKeyRequest
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RepricingSelectorRequest {
    /// Axis 1. Absent selects across plans; named, it is also the axis D-134
    /// groups the apply's transactions by.
    pub plan_id: Option<Uuid>,
    /// Axis 2, ISO 4217 — §2's *"a currency segment"* is this member alone.
    pub currency: Option<String>,
    /// Axis 3.
    pub region: Option<String>,
    /// Axis 5. Axis 4, `priceOverlay`, is absent by construction: every row this
    /// gear authors is on the `base` plane.
    pub phase: Option<Uuid>,
    /// Axis 6 — `all_subscriptions | new_subscriptions_only |
    /// existing_grandfathered`.
    ///
    /// **The one axis whose absence narrows the run rather than widening it.**
    /// Leaving it out excludes `existing_grandfathered`
    /// (`inst-mp-grandfathered`); naming it is how an operator asks for that
    /// class, and what they then get is the per-row refusal the apply owes.
    pub price_eligibility: Option<String>,
    /// Axis 7 — `recurring | usage | one_time | one_time_setup`.
    pub charge_kind: Option<String>,
    /// Axis 8: the grandfathering generation's cutover instant.
    ///
    /// `null` is **unconstrained** here, where on
    /// [`ScopeKeyRequest`](crate::api::rest::prices::ScopeKeyRequest) it is *a row
    /// that retains nobody*. The second meaning is not lost: `cohort != none` if
    /// and only if `priceEligibility == existing_grandfathered`, so the rows that
    /// retain nobody are selected through `price_eligibility` instead.
    #[serde(default, with = "rfc3339::option")]
    pub cohort: Option<OffsetDateTime>,
    /// Axis 9 (D-196). A published metering unit.
    pub meter: Option<String>,
    /// Axis 10 (D-196). The empty string selects the **undimensioned** rows,
    /// whose column holds the empty-tuple sentinel rather than a `NULL`.
    ///
    /// **Absent and `""` are different runs, and no read surface emits the
    /// second.** Absent leaves the axis unconstrained — every dimension of the
    /// meter — while `""` constrains it to the rows carrying no dimension at all.
    /// A price read renders an undimensioned line as `null` on both members that
    /// carry it, so an operator composing a selector from a row they just read
    /// gets the *unconstrained* run unless they author the empty string here
    /// deliberately. The overloading is this field's, not the read's: one member
    /// cannot both name a value and mean "do not filter" without a spelling that
    /// no absence renders as.
    pub dimension_key: Option<String>,
}

/// What the run does to every selected row's amount.
///
/// The four members are [`OverlayLineRequest`]'s, spelled identically and parsed
/// by [`crate::domain::overlay::adjustment_of`] — the same function, not a copy. `inst-mr-api` names the
/// adjustment without defining it, and a second vocabulary for "what to do to a
/// price" is the hazard the overlay plane's own line parser already exists to
/// prevent.
///
/// [`OverlayLineRequest`]: crate::api::rest::overlays::OverlayLineRequest
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RepricingAdjustmentRequest {
    /// `markup | discount | fixed`.
    pub adjustment_kind: String,
    /// `percent_bp | amount`. **Declared, never inferred** (D-08).
    pub magnitude_kind: String,
    /// The basis-points magnitude, on a `percent_bp` adjustment.
    pub adjustment_value: Option<i64>,
    /// The per-currency values, on an `amount` adjustment.
    #[serde(default)]
    pub amounts: Vec<AmountRequest>,
}

/// `inst-mr-api`'s four fields, and nothing else.
#[derive(Debug, Clone)]
// `(request, response)` and not `(request)` alone, for `ScheduleWindowRequest`'s
// reason: the run records the digest of the **parsed** request on its row, and
// that needs the type to serialize.
#[toolkit_macros::api_dto(request, response)]
pub struct RepricingRunRequest {
    /// The caller's name for this run, and §5's idempotency column for the
    /// surface. It becomes the run's `client_key`, so a second `POST` under it
    /// answers the run it opened rather than opening a second one.
    pub run_id: Uuid,
    /// Which published rows to act on.
    pub selector: RepricingSelectorRequest,
    /// What to do to their amounts.
    pub adjustment: RepricingAdjustmentRequest,
    /// **One** instant for every row of the run (D-88). Strictly future at
    /// submit; the approval commit holds it to a whole batching delay.
    #[serde(with = "rfc3339")]
    pub changeover: OffsetDateTime,
}

/// One selected row and how far it got (`inst-mr-journal`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RepricingJournalRowView {
    /// The published row this run selected.
    pub price_id: Uuid,
    /// `pending | applied | failed`.
    ///
    /// **`not-attempted` is not among them.** D-261 makes that a *rendering* of a
    /// `pending` row on a report, never a stored value, which is what lets a
    /// re-drive tell "never reached" from "decided" — for a run left
    /// `committing` after an ordinary apply failure, where a row reading
    /// `pending` here genuinely means "not yet reached, retake the lock and
    /// carry on". A run a panic or a dropped future ended instead reaches a
    /// terminal state with every such row marked `failed`
    /// (`infra::repricing::RunLockGuard`'s `Drop` fallback, which has no code
    /// left running to preserve the distinction any other way) — there,
    /// "never reached" survives only in `failure_reason`'s free text, not in
    /// this field.
    pub state: String,
    /// Why it did not apply. Present exactly on `failed`.
    pub failure_reason: Option<String>,
    /// The successor the apply created. Present exactly on `applied`.
    pub applied_price_id: Option<Uuid>,
    /// When that commit landed.
    pub applied_at: Option<String>,
}

/// A run and its journal — `inst-mr-return`'s progress endpoint, whole.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RepricingRunView {
    /// The caller's `run_id`, which is what the `GET` is addressed by.
    pub run_id: String,
    /// The run's durable row name, and what the journal's own `run_id` column
    /// holds. Both are on the view because they are two different values.
    pub operation_id: Uuid,
    /// One of §4's seven states. A run this arm opened has already left
    /// `validating`, for `awaiting_approval` or `committing` on its materiality
    /// verdict, and is at that state when the `POST` answers — the apply runs after
    /// the response, so the terminal state is the `GET`'s to report.
    pub state: String,
    /// The frozen run parameters — selector, adjustment, changeover, and how many
    /// rows the expansion found.
    pub report: serde_json::Value,
    /// When the run was submitted.
    pub submitted_at: String,
    /// When it ended; absent while it is still going.
    pub completed_at: Option<String>,
    /// The frozen row set, in `price_id` order.
    pub journal: Vec<RepricingJournalRowView>,
}

/// What this surface needs.
pub struct ApiState {
    /// The authoring plane's state. No field of its own, for
    /// [`crate::api::rest::bulk_imports::ApiState`]'s reason: the expansion is a
    /// free function over a runner and the run's own statements are too, so what
    /// is wanted here is the provider and the scope every read goes through.
    pub authoring: Arc<AuthoringState>,
    /// The lane an accepted run's apply is handed to
    /// ([`crate::infra::repricing::RunApplyLane`]).
    ///
    /// Not optional, and that is the point: a state built without a lane would be a
    /// surface that accepts runs and applies none, silently. What a process with no
    /// `bss-pricing` lifecycle gets instead is a lane whose applier nothing spawned —
    /// the enqueue says so at `error`, and a harness drives
    /// [`crate::infra::repricing::RunApplyWorker::drain_pending`] itself.
    ///
    /// **This state carries no registry handle and no policy reader**, which is what
    /// keeps it inside `AuthoringState`'s own rule — "the authoring routes must not
    /// be able to reach [a registry handle]" (`api::rest::state`'s module doc). The
    /// apply is what needs both, and the apply is the lane's; a copy of either here
    /// would be a handle on a request path that never requests a `CatalogVersion`.
    pub apply_lane: crate::infra::repricing::RunApplyLane,
}

/// Build the router for the run's three surfaces.
pub fn router(state: Arc<ApiState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post(REPRICING_RUNS)
        .operation_id("bss_pricing.open_repricing_run")
        .summary("Open a mass-repricing run")
        .description(
            "Expands the selector over the tenant's published price rows, freezes that row set \
             into the run's journal, and answers `202` with the run ref. The selector is any \
             subset of the canonical scope key's axes; rows in the `existing_grandfathered` class \
             are excluded unless the selector names that class, since they are immutable in \
             price. A selector matching no published row is refused `400` \
             `RUN_SELECTOR_EMPTY` and opens nothing, so the `run_id` stays available for a \
             corrected selector. The changeover instant must be strictly in the future. \
             Idempotency is the `run_id`: a second call under one answers the run it opened. \
             Progress is at `GET /bss-pricing/v1/repricing-runs/{runId}`. Materiality is \
             evaluated once for the whole run against the tenant's configured threshold policy; \
             a material run moves to `awaiting_approval` under an opened approval unit, and an \
             auto-publishable one moves to `committing`. The apply of a `committing` run runs \
             **after** this response, not inside it: this call answers as soon as the run and \
             its journal are durable, and the progress read is where the rows move off \
             `pending` and the run reaches `completed` or `completed_with_conflicts`. A \
             `pending` entry in that read means the row was not attempted rather than attempted \
             and unchanged (D-261). A run that is not applied -- stalled mid-apply, or never \
             reached -- is ended with `POST /bss-pricing/v1/repricing-runs/{runId}/abort`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RepricingRunRequest>(openapi, "The run to open.")
        .handler(open_repricing_run)
        .json_response_with_schema::<RepricingRunView>(
            openapi,
            StatusCode::ACCEPTED,
            "The run, with its frozen journal, accepted for applying. An auto-publishable run \
             is `committing` here and its rows are still `pending`; poll \
             `GET /bss-pricing/v1/repricing-runs/{runId}` for the terminal state.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(REPRICING_RUN)
        .operation_id("bss_pricing.read_repricing_run")
        .summary("Read a mass-repricing run's progress")
        .description(
            "The run and its per-row journal, addressed by the `run_id` the caller opened it \
             under. Gates on `plan` x `read`: a run's journal is price data, and an operator who \
             may read prices may read what a run did to them.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("runId", "The run_id the run was opened under.")
        .handler(read_repricing_run)
        .json_response_with_schema::<RepricingRunView>(
            openapi,
            StatusCode::OK,
            "The run and its journal.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(REPRICING_RUN_ABORT)
        .operation_id("bss_pricing.abort_repricing_run")
        .summary("Abort a run stalled mid-apply")
        .description(
            "Moves a `committing` run to a terminal state, clearing the bulk locks its apply \
             took so interactive authoring on those rows is not frozen by a run that stopped \
             (D-37). Rows the apply had already repriced stand; rows it never reached are marked \
             `failed` in the journal with the abort as their reason, since a `pending` row under \
             a terminal run is a row nothing can reach. A run that is not `committing` is \
             refused, except when this operation is the one that ended it: a replayed abort is \
             answered the run it already aborted. Gates on `plan` x `write`, the same authority \
             the open takes -- the abort is un-authoring at scale, not a new authority.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("runId", "The run_id the run was opened under.")
        .handler(abort_repricing_run)
        .json_response_with_schema::<RepricingRunView>(
            openapi,
            StatusCode::OK,
            "The aborted run, with its locks released and its journal decided.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-178's edge. The `POST` writes — a run row and a journal row per selected
    // price — so it travels with the routes rather than with whoever merges them.
    router
        .layer(Extension(state))
        .layer(axum::middleware::from_fn(
            crate::api::rest::correlation::establish,
        ))
}

/// `POST /repricing-runs`.
async fn open_repricing_run(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // **For a value, not only for its refusal.** `open_run_in` writes the audit
    // record itself, on this same request, and D-178 clause (2) requires every record
    // and outbox row one operator call produces to carry **one** correlation id — so
    // the value taken here is the one that record carries. On an arm that wrote no
    // record the ask would be D-178's edge alone: a router mounted without it must
    // answer 403 rather than 500 at the first record a later arm writes.
    let correlation_id = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let scope = write_scope(&enforcer, &ctx).await?;
    // Parsed after the gate; see the module doc for why `Json<T>` is not used.
    let body: RepricingRunRequest = preconditions::parse_body(&body)?;
    let client_key = body.run_id.to_string();

    let conn =
        state.authoring.db.conn().map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!("repricing run: {e}")))
        })?;

    // **The replay comes before every judgement below**, and that order is the
    // contract rather than an optimisation: a replay answers what the first call
    // answered, so a run accepted yesterday is still answered today even though its
    // changeover has since fallen behind the submit floor and its selector may now
    // match different rows. Re-judging either would turn a retry into a refusal of
    // work that was already accepted -- which is the one conclusion idempotency
    // exists to prevent, for exactly the client that retried on a timeout.
    if let Some(existing) =
        bulk_repo::find_by_client_key(&conn, &scope, tenant, BulkKind::Repricing, &client_key)
            .await
            .map_err(|e| CanonicalError::from(repo_failure(&e)))?
    {
        let view = run_view(&conn, &scope, tenant, &existing).await?;
        return Ok((StatusCode::ACCEPTED, Json(view)).into_response());
    }

    let selector = selector_of(&body.selector)?;
    let adjustment = crate::domain::overlay::adjustment_of(
        &body.adjustment.adjustment_kind,
        &body.adjustment.magnitude_kind,
        body.adjustment.adjustment_value,
        &body
            .adjustment
            .amounts
            .iter()
            .map(|a| (a.currency.clone(), a.value_minor))
            .collect::<Vec<_>>(),
    )?;
    // D-144's quantum before the distance, `plan_supersession`'s order: a malformed
    // instant is not an instant whose distance is worth measuring.
    crate::domain::instant::check_quantum("changeover", body.changeover)?;
    let now = OffsetDateTime::now_utc();
    check_changeover_instant(body.changeover, now, ChangeoverMoment::Submit)?;

    let selected = price_repo::load_published_for_selector(&conn, &scope, tenant, &selector)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    if selected.is_empty() {
        // **Refused before anything is opened, so the `run_id` is not spent.** That
        // is the one place this surface diverges from `bulk_imports`, whose Phase-1
        // refusal deliberately keeps its key: there the run has to exist because it
        // holds the per-row report the refusal points at, and `inst-bs-reject` rests
        // on a spent key being auditable. Here there is no report to hold -- nothing
        // was selected, so there is nothing to record per row -- and the remedy is
        // to correct one field of the request. Charging an operator a fresh `run_id`
        // for a mistyped region would be a cost with nothing bought by it. The
        // design set does not settle this; it is stated here because it is a choice.
        return Err(CanonicalError::from(DomainError::RunSelectorEmpty(
            empty_selector_detail(&selector),
        )));
    }

    let report = frozen_report(&selector, &adjustment, body.changeover, selected.len());
    let run = open_run(
        &state.authoring.db,
        &scope,
        tenant,
        NewBulkOperation {
            operation_id: Uuid::now_v7(),
            tenant_id: tenant,
            kind: BulkKind::Repricing,
            client_key,
            // **Recorded here, and this surface's replay does not compare it**. The run's key
            // is spent on one request either way, so the digest is part of what a run *is* and
            // every writer of the table sets it — but the two flows differ in what a second
            // `POST` is owed. This one's idempotency column is the `run_id` **inside** the body
            // — a posture the design set does not yet carry an instruction id for — and the arm
            // above answers the frozen run precisely so a retry
            // is not re-judged against a selector that now matches different rows. Refusing a
            // changed body here would be a second decision on a second surface, not this fix;
            // it is stated rather than left to be inferred from an uncompared column.
            request_hash: preconditions::request_digest(&body)?,
            report,
            submitted_by: ctx.subject_id(),
            submitted_at: now,
        },
        selected,
        &adjustment,
        correlation_id,
    )
    .await?;

    // **The non-material path.** `open_run` has already taken the run to
    // `committing` in the same transaction that froze its journal, on a verdict of
    // "no second principal required" — `advance_on_verdict`'s `else` arm. A material
    // run is left exactly where `open_run` put it (`awaiting_approval`); its apply is
    // `api::rest::approvals`' arm, spent when a second principal approves the batch
    // unit.
    //
    // **The apply leaves on the lane; this response does not wait for it.** A mass
    // repricing is a per-plan commit over every row the run selected, so applying it
    // here held an HTTP connection for the run's whole duration and a gateway timeout
    // left the caller with no answer for work that did happen — over a surface whose
    // whole idempotency exists for the caller that retried on a timeout.
    //
    // **What makes handing back `committing` safe is that the state has an owner.**
    // `POST /bss-pricing/v1/repricing-runs/{runId}/abort` reaches it
    // (`infra::repricing::abandon_committing_run`): the run's bulk locks go and its
    // unreached rows are decided, so a run whose apply never runs — a full lane, a
    // replica that lost its applier, a shutdown mid-apply — is ended by an operator
    // rather than stranded. Without that door this response would be handing the
    // caller a state nothing could spend, which is the trap D-37 named.
    //
    // The scope and the context travel with the request rather than being resolved on
    // the lane: the PDP has already answered for this principal above, and the apply
    // replays that decision instead of making a new one where no principal is present.
    if run.state == BulkState::Committing {
        state
            .apply_lane
            .enqueue(crate::infra::repricing::RunApplyRequest {
                ctx: ctx.clone(),
                scope: scope.clone(),
                tenant_id: tenant,
                operation_id: run.operation_id,
                stamp: AuditStamp {
                    actor_principal_id: ctx.subject_id(),
                    recorded_at: OffsetDateTime::now_utc(),
                    correlation_id,
                },
            });
    }

    let view = run_view(&conn, &scope, tenant, &run).await?;
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

/// `GET /repricing-runs/{runId}`.
async fn read_repricing_run(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<RepricingRunView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let tenant = ctx.subject_tenant_id();
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

    let conn =
        state.authoring.db.conn().map_err(|e| {
            CanonicalError::from(DomainError::Internal(format!("repricing run: {e}")))
        })?;
    let run = bulk_repo::find_by_client_key(
        &conn,
        &scope,
        tenant,
        BulkKind::Repricing,
        &run_id.to_string(),
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?
    .ok_or_else(|| {
        CanonicalError::from(DomainError::NotFound {
            subject: "repricing run".to_owned(),
            id: run_id.to_string(),
        })
    })?;
    Ok(Json(run_view(&conn, &scope, tenant, &run).await?))
}

/// `POST /repricing-runs/{runId}/abort`.
///
/// `committing`'s one owner. `apply_run_in`'s ordinary-`Err` exit and
/// `RunLockGuard`'s `Drop` both leave a run there on purpose — that is what keeps a
/// redrive possible and what stops a storage hiccup freezing every unreached plan
/// `failed` — and this route is what spends what they preserve.
///
/// **It is also what makes the open's `202` answerable at all.** That response hands
/// back a `committing` run whose apply is still on
/// [`crate::infra::repricing::RunApplyLane`], so every way an apply can fail to
/// arrive — a full lane, a replica whose applier died, a shutdown between the two —
/// lands a run here with nothing else to reach it. This route is the ordinary door
/// out of that, not only the stalled-apply one.
///
/// **A run this operation already ended is answered, not refused.** The sweep lands
/// the run in a terminal state, so the state alone cannot tell "this operation ran"
/// from "this run finished on its own"; the note the sweep writes can, and nothing
/// else writes it. Refusing the replay would tell a client whose abort succeeded
/// that it did not, over a surface whose whole idempotency is a retry finding what
/// the first call did.
///
/// **No `Idempotency-Key` and no `If-Match`**, this plane's own posture: §5's
/// idempotency column for the repricing surface is the `run_id`, which this route
/// carries in its path. Its bulk sibling one plane over does take a key, and reading
/// the two as one family is exactly the mistake
/// `the_repricing_run_declares_no_precondition_header` was written against — a
/// header every generated client would send and this server would never read. The
/// state guard plus the note is what makes the retry safe, and it needs no header to
/// be so.
async fn abort_repricing_run(
    Extension(state): Extension<Arc<ApiState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<RepricingRunView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Required, not optional, for D-178's reason: the audit record this route
    // writes would otherwise carry no correlation id.
    let correlation_id = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let scope = write_scope(&enforcer, &ctx).await?;

    let conn = state.authoring.db.conn().map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!("repricing run abort: {e}")))
    })?;
    let run = bulk_repo::find_by_client_key(
        &conn,
        &scope,
        tenant,
        BulkKind::Repricing,
        &run_id.to_string(),
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?
    .ok_or_else(|| {
        CanonicalError::from(DomainError::NotFound {
            subject: "repricing run".to_owned(),
            id: run_id.to_string(),
        })
    })?;

    // The replay arm, ahead of the sweep's own state guard so the guard never
    // answers about a run this operation ended.
    if run.state != BulkState::Committing
        && run.report.get(crate::infra::bulk::ABORTED_MEMBER).is_some()
    {
        return Ok(Json(run_view(&conn, &scope, tenant, &run).await?));
    }

    let aborted = crate::infra::repricing::abandon_committing_run(
        &state.authoring.db,
        &scope,
        tenant,
        run.operation_id,
        crate::infra::repricing::ABORT_NOTE,
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(CanonicalError::from)?;

    audit_the_abort(&conn, &scope, tenant, &ctx, correlation_id, &aborted).await;
    Ok(Json(run_view(&conn, &scope, tenant, &aborted).await?))
}

/// Record who stopped a run, and from which state.
///
/// `AuditAction::Abandon` and not `Delete`, for that variant's stated reason: the
/// row survives and the run keeps its identity. On the run's own chain, under the
/// same `subject_kind`/`subject_ref` pair the open's `create` record carries
/// (D-158), so `GET /audit` answers "who ended run X" beside "who opened it".
///
/// Best-effort, the bulk plane's `audit_the_abort` posture and its reason: the sweep
/// has already released the locks and landed the run, so refusing here would leave
/// an operator believing the abort failed when it is the one thing that certainly
/// succeeded.
async fn audit_the_abort(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant: Uuid,
    ctx: &SecurityContext,
    correlation_id: Uuid,
    aborted: &BulkOperationRecord,
) {
    if let Err(e) = audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id: tenant,
            chain_id: audit_repo::bulk_operation_chain(aborted.operation_id),
            recorded_at: OffsetDateTime::now_utc(),
            actor_principal_id: ctx.subject_id(),
            action: AuditAction::Abandon,
            subject_kind: AuditSubjectKind::BulkOperation,
            subject_ref: audit_repo::bulk_operation_ref(aborted.operation_id),
            before_state: None,
            after_state: Some(serde_json::json!({
                "kind": aborted.kind.as_str(),
                "state": aborted.state.as_str(),
            })),
            approval_ref: None,
            correlation_id,
        },
    )
    .await
    {
        tracing::error!(
            error = %e,
            operation_id = %aborted.operation_id,
            "bss-pricing: a repricing run was aborted without its audit record"
        );
    }
}

/// Open the run, freeze its row set and record it, **in one transaction**.
///
/// The atomicity is required rather than tidy, and
/// [`repricing_journal_repo::open_rows`] says so from its own side: it opens no
/// transaction by design (D-134 makes the run's transaction unit the plan) and
/// obliges the expansion to hold one across the whole set. Without it a partial
/// freeze leaves a run whose completion predicate — *no `pending` rows remain* —
/// is satisfiable by rows that were never selected, and a run open with no journal
/// at all is a run that reports success having touched nothing. The audit record
/// belongs in the same transaction for D-14's reason every writer in this crate
/// gives: a crash between the insert and the record must not produce one without
/// the other.
async fn open_run(
    db: &toolkit_db::DBProvider<toolkit_db::DbError>,
    scope: &AccessScope,
    tenant_id: Uuid,
    new: NewBulkOperation,
    selected: Vec<Uuid>,
    adjustment: &Adjustment,
    correlation_id: Uuid,
) -> Result<BulkOperationRecord, CanonicalError> {
    let scope = scope.clone();
    let adjustment = adjustment.clone();
    let (_, outcome) = db
        .db()
        .in_transaction::<BulkOperationRecord, RepoError, _>(move |txn| {
            Box::pin(async move {
                open_run_in(
                    txn,
                    &scope,
                    tenant_id,
                    new,
                    &selected,
                    &adjustment,
                    correlation_id,
                )
                .await
            })
        })
        .await;
    outcome
        .map_err(|err| {
            err.into_domain(|infra| {
                RepoError::Db(format!("bss-pricing: repricing run transaction: {infra}"))
            })
        })
        .map_err(|e| CanonicalError::from(repo_failure(&e)))
}

/// [`open_run`]'s body, on the transaction's runner.
async fn open_run_in(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    new: NewBulkOperation,
    selected: &[Uuid],
    adjustment: &Adjustment,
    correlation_id: Uuid,
) -> Result<BulkOperationRecord, RepoError> {
    let actor_principal_id = new.submitted_by;
    let recorded_at = new.submitted_at;
    let kind = new.kind;
    let row_count = selected.len();
    let run = bulk_repo::open(runner, scope, new).await?;
    // **`tenant_id` is written here and never taken from the request.** The
    // journal's only foreign key covers `run_id`, so the key is not what stops a
    // journal row carrying a foreign tenant — the table's trigger is, on both
    // engines, looking the run up and refusing an insert whose `tenant_id` differs
    // from the run's. `NewJournalRow` states both halves from the other end.
    //
    // The rule is written here anyway, because the trigger's refusal arrives as a
    // storage failure: a caller that needed the schema to tell it whose rows these
    // are has already lost the scope it was handed.
    let rows: Vec<NewJournalRow> = selected
        .iter()
        .map(|price_id| NewJournalRow {
            run_id: run.operation_id,
            price_id: *price_id,
            tenant_id,
        })
        .collect();
    repricing_journal_repo::open_rows(runner, scope, &rows).await?;

    // `inst-mp-pending` (D-35), and it is the **selector-time** refusal the
    // instruction names — D-124 reads it that way in as many words ("rejects rows
    // at selector time only, blocking nothing submitted later"), and this is the
    // only moment that reading is available: the run's own approval unit does not
    // exist yet, so a key found held here is held by somebody else.
    let refused_keys =
        refuse_rows_on_a_held_key(runner, scope, tenant_id, run.operation_id, selected).await?;

    // The debt `repricing_runs`' own module doc named: until `AuditSubjectKind`
    // carried a `BulkOperation` member, opening a run wrote no audit record at
    // all. `subject_ref` is `audit_repo::bulk_operation_ref(run.operation_id)`,
    // so the audit record and the batch approval `inst-bs-approval` will one day
    // open name this run identically — D-158's alignment, paid in advance of the
    // approval writer that does not exist yet.
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::bulk_operation_chain(run.operation_id),
            recorded_at,
            actor_principal_id,
            action: AuditAction::Create,
            subject_kind: AuditSubjectKind::BulkOperation,
            subject_ref: audit_repo::bulk_operation_ref(run.operation_id),
            before_state: None,
            after_state: Some(serde_json::json!({
                "kind": kind.as_str(),
                "state": run.state.as_str(),
                "rowCount": row_count,
            })),
            approval_ref: None,
            correlation_id,
        },
    )
    .await?;

    // **The two edges out of `validating`, in the same transaction as the freeze
    // above.** `api::rest::repricing_runs`' own module doc named this the second
    // half of the debt `AuditSubjectKind::BulkOperation` unblocked: a run opened
    // and frozen but left with no materiality verdict and no edge written would
    // rest in `validating` forever on a crash between the two, exactly the
    // failure D-14 exists to close for the audit record one step up. Materiality
    // is evaluated **once for the whole run** (`inst-mr-coalesce`) — never once
    // per plan and folded — so there is exactly one `evaluate` call here however
    // many plans the selector spans.
    let (verdict, held_keys) =
        run_materiality(runner, scope, tenant_id, selected, adjustment, recorded_at).await?;
    // **The run pins what it will act on, and a key another unit holds is not
    // that.** Subtracted here rather than inside `run_materiality`, which is
    // deliberately still evaluated over the **whole** selected set: a row this
    // run will not apply still counts toward the verdict, which is
    // `inst-mp-grandfathered`'s established treatment of exactly the same shape
    // (an explicitly-selected retained row is journalled, counted, and failed) and
    // is the fail-safe direction — it can only ask for a second principal more
    // often, never less.
    let held_keys: BTreeSet<String> = held_keys.difference(&refused_keys).cloned().collect();
    let stamp = AuditStamp {
        actor_principal_id,
        recorded_at,
        correlation_id,
    };
    advance_on_verdict(runner, scope, tenant_id, &run, &verdict, held_keys, stamp).await
}

/// `inst-mp-pending` (D-35): fail, **per row**, every selected row whose
/// canonical scope key already holds a pending interactive unit — and answer with
/// the keys so refused.
///
/// # Why the refusal is here and not in the apply
///
/// Its sibling clause `inst-mp-grandfathered` is refused in
/// [`crate::infra::repricing::apply_rows_in`], and copying that placement here
/// would be wrong for a reason that is about *which* rule it is. A grandfathered
/// row is refused for a property of the row itself, which no later act changes;
/// a held key is a fact about a **second unit**, and D-35's other half has the
/// run's own batch approval pin every key it contains — so the collision this
/// arm exists to answer happens at
/// [`approval_repo::open`](crate::infra::storage::repo::approval_repo::open),
/// three statements below, before any apply ever runs. Leaving it to the apply
/// leaves the whole `POST` failing on `uq_pricing_approval_key_pending`, which
/// is what it did: one held key refused the batch, and `inst-mp-pending`'s "fails
/// per-row" had nothing to refer to on this surface.
///
/// # The register read is the import's, not a second one
///
/// [`approval_repo::find_pending_key_holders`](crate::infra::storage::repo::approval_repo::find_pending_key_holders)
/// is the plural read `infra::import::key_holds_a_pending_unit` already answers
/// D-35's import half with. One decision, one read, two surfaces — D-282's line,
/// and the alternative is two register queries free to disagree about what
/// "held" means.
///
/// # The row is born `pending` and moved, never born `failed`
///
/// D-261: the journal's insert trigger admits only `pending`, so this runs
/// **after** [`repricing_journal_repo::open_rows`] and moves the row. That is not
/// a workaround — it is what keeps the frozen row set the set the selector
/// matched, so an operator reading the run sees the row that was refused rather
/// than a run that quietly selected one fewer row than it says it did.
///
/// # Errors
/// Whatever the price repository, the approval register or the journal refuses
/// with.
async fn refuse_rows_on_a_held_key(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
    selected: &[Uuid],
) -> Result<BTreeSet<String>, RepoError> {
    let keys = price_repo::load_scope_keys_for_ids(runner, scope, tenant_id, selected).await?;
    let rendered: BTreeSet<String> = keys.iter().map(|(_, key)| key.to_string()).collect();
    let holders: HashMap<String, _> =
        approval_repo::find_pending_key_holders(runner, scope, tenant_id, &rendered)
            .await?
            .into_iter()
            .map(|(record, key)| (key, record))
            .collect();
    if holders.is_empty() {
        return Ok(BTreeSet::new());
    }

    let mut refused = BTreeSet::new();
    for (price_id, key) in keys {
        let rendered = key.to_string();
        let Some(unit) = holders.get(&rendered) else {
            continue;
        };
        repricing_journal_repo::mark_failed(
            runner,
            scope,
            tenant_id,
            operation_id,
            price_id,
            // Names the unit, which is the whole of what D-35 asks the refusal to
            // carry: the operator's remedy is to decide or withdraw that unit and
            // re-run, and a refusal that did not name it would send them looking.
            &format!(
                "inst-mp-pending (D-35): this row's canonical scope key is held by pending \
                 approval unit {}, and the one-pending-unit rule reserves the key for the \
                 change already under way; decide or withdraw it and re-run. The run's other \
                 rows are unaffected",
                unit.approval_id
            ),
        )
        .await?;
        refused.insert(rendered);
    }
    Ok(refused)
}

/// The run's materiality, evaluated once over every plan its rows sit on
/// (`inst-mr-coalesce`; the shape settled after two implementers left
/// it open).
///
/// The change set is [`ChangeSet::of_repricing_run`] over `selected`, each row
/// **projected under `adjustment`** ([`project_row`]) — the amount the operator
/// is being asked to approve, computed in-memory for this comparison alone and
/// never written. The baseline is [`PublishedPriceBaseline::of_records`] over
/// **every published row of every plan `selected` touches**, not only
/// `selected` itself: `inst-mat-newrow`'s coverage check
/// (`change.keys().any(|key| !baseline.covers(key))`) is a property of the
/// plan's whole key set, and scope keys carry `plan_id`, so records from
/// several plans coexist in the one baseline without collision — the reason a
/// per-plan fold was rejected in favour of building this once.
///
/// **Projecting rather than reading a real successor is not the same gap as
/// evaluating a zero delta.** `inst-mr-apply`'s successor row additionally
/// takes the bulk lock, writes an outbox record and lands inside a per-plan
/// transaction the aggregate pass re-runs against — none of which materiality
/// needs in order to know what a row's amount is *about* to become. The
/// arithmetic [`project_row`] performs is total over `flat`/`package` rows in
/// every currency-resolvable case, and over the rate-priced kinds —
/// `per_unit`'s `unit_rate` and `graduated`/`volume`'s bands — under a
/// `percent_bp` magnitude, which is currency-neutral and therefore always
/// resolvable. `per_unit` moved into that second group, with
/// [`project_row`]'s own arm (D-311): under an `amount` or `fixed` magnitude
/// `project_rate` computes nothing for any of the three, and this projection
/// measures a zero delta — see the module doc for that corner and for the
/// apply-side refusal that bounds it.
///
/// `selected` is never empty here: `open_repricing_run` refuses
/// `RUN_SELECTOR_EMPTY` before a run is ever opened, so every touched plan
/// contributes at least the rows `selected` names and the baseline is never
/// `None` — `inst-mat-first`'s `FirstPublish` structurally cannot fire for a
/// repricing run.
///
/// Returns the verdict alongside the canonical scope keys `selected` sits on,
/// so a material caller can hand them to the approval unit it opens
/// (`inst-bk-approval-subset`'s "pins its keys exactly like bulk import")
/// without a second walk of the same rows.
async fn run_materiality(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    selected: &[Uuid],
    adjustment: &Adjustment,
    now: OffsetDateTime,
) -> Result<(MaterialityVerdict, BTreeSet<String>), RepoError> {
    let touched_plans: BTreeSet<PlanId> =
        price_repo::load_plan_ids(runner, scope, selected.iter().copied())
            .await?
            .into_iter()
            .map(|(_, plan_id)| plan_id)
            .collect();

    // One read for the whole touched set. Per plan it was one query and one band
    // hydration each, inside the transaction that froze this run's journal, and
    // a selector naming no `plan_id` spans the tenant's whole catalogue — the
    // one path a repricing run cannot avoid. `load_for_plans` answers in the
    // order this loop produced, so nothing downstream sees a different sequence.
    let baseline_rows = price_repo::load_for_plans(
        runner,
        scope,
        tenant_id,
        touched_plans,
        &[LifecycleState::Published],
    )
    .await?;

    // The run's own rows, out of the baseline just read — never a second query
    // for the same content. Selected is exactly the subset of every touched
    // plan's published rows the selector matched, so every id in it is present
    // in `baseline_rows` by construction. `held_keys` is taken off the
    // **published** rows, before projection: the key a unit holds is the row's
    // own identity, unaffected by what its amount is about to become.
    let selected_ids: HashSet<Uuid> = selected.iter().copied().collect();
    let selected_rows: Vec<_> = baseline_rows
        .iter()
        .filter(|row| selected_ids.contains(&row.price_id))
        .cloned()
        .collect();
    let held_keys: BTreeSet<String> = selected_rows
        .iter()
        .map(|row| row.scope_key.to_string())
        .collect();

    let change_rows: Vec<_> = selected_rows
        .into_iter()
        .map(|row| crate::domain::repricing::project_row(row, adjustment))
        .collect();
    let change = ChangeSet::of_repricing_run(change_rows);
    let baseline = PublishedPriceBaseline::of_records(baseline_rows);
    let policy = effective_policy_at(runner, scope, tenant_id, now)
        .await
        .map_err(|e| RepoError::Db(format!("bss-pricing: repricing run threshold policy: {e}")))?;

    Ok((
        materiality::evaluate(&change, policy.as_ref(), Some(&baseline)),
        held_keys,
    ))
}

/// Write the verdict's edge: `validating -> awaiting_approval` under an opened
/// approval unit when material, `validating -> committing` otherwise
/// (`inst-bs-approval`, `inst-bs-commit`'s entry).
///
/// The unit's `subject_kind` is [`AuditSubjectKind::BulkOperation`] and its
/// `subject_ref` is `audit_repo::bulk_operation_ref(run.operation_id)` — the
/// same pair `open_run_in`'s own `create` record above already carries, so an
/// auditor reading either finds the other (D-158).
///
/// `report` travels unchanged: nothing about the run's frozen parameters moved
/// by evaluating its materiality, only its `state` did, and `bulk_repo::advance`
/// requires a report because a bulk import's does grow at this edge — a
/// repricing run's does not, yet.
async fn advance_on_verdict(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run: &BulkOperationRecord,
    verdict: &MaterialityVerdict,
    held_keys: BTreeSet<String>,
    stamp: AuditStamp,
) -> Result<BulkOperationRecord, RepoError> {
    if verdict.is_material() {
        let materiality = serde_json::to_value(MaterialityView::from(verdict)).map_err(|e| {
            RepoError::Db(format!("bss-pricing: render repricing materiality: {e}"))
        })?;
        approval_repo::open(
            runner,
            scope,
            approval_repo::NewApproval {
                approval_id: Uuid::now_v7(),
                tenant_id,
                subject_ref: audit_repo::bulk_operation_ref(run.operation_id),
                subject_kind: AuditSubjectKind::BulkOperation,
                content_hash: repricing_pin(&run.report, run.operation_id),
                materiality,
                held_keys,
            },
            stamp,
        )
        .await?;
        bulk_repo::advance(
            runner,
            scope,
            tenant_id,
            run.operation_id,
            // The state this verdict was evaluated over, carried into the
            // statement rather than trusted to still hold.
            run.state,
            BulkState::AwaitingApproval,
            run.report.clone(),
            stamp.recorded_at,
        )
        .await
    } else {
        bulk_repo::advance(
            runner,
            scope,
            tenant_id,
            run.operation_id,
            run.state,
            BulkState::Committing,
            run.report.clone(),
            stamp.recorded_at,
        )
        .await
    }
}

/// `SHA-256(domain separator || the run's frozen report || its own operation
/// id)` — the content pin of a repricing run's approval unit (`inst-ap-pin`).
///
/// **The bytes moved to
/// [`repricing_run_content_hash`](crate::domain::approval::content_pin::repricing_run_content_hash)
/// and this is the caller.** `infra::approval::re_derive` has to take the same
/// digest when the unit is decided, and an `infra` module may not name an `api`
/// item (`DE0202`, D-270's recorded encounter with it) — so the preimage lives
/// in the domain now, byte for byte, and both ends spell it once. Kept as a
/// named function here rather than inlined at the one call site because the
/// paragraphs below are about *this* unit's pin and not about the encoder.
///
/// **Not `domain::approval::content_pin`'s canonical per-field framing.** That
/// module hashes a caller-authored shape a submitter could otherwise steer, so
/// it frames every field by hand to stay collision-honest; a run's `report` is
/// this crate's own controlled rendering (`frozen_report`), already built once
/// from the parsed request rather than echoed from it, so hashing its
/// serialization is total and needs no second encoder. `operation_id` is folded
/// in rather than `run_id` (`client_key`): it is the value this same unit's
/// `subject_ref` already carries, so the pin and the subject name the run by
/// one id and not two.
///
/// **What this does not yet do** is `inst-bk-approval-subset`'s **per-row**
/// content hash — the pin that lets a committed subset shrink by row rather
/// than only by whole plan. Named rather than built: the apply's transaction unit
/// is the plan (`infra::repricing::apply_plan_in`, D-134), so a per-row pin would
/// name a granularity nothing commits at — one plan's rows land or fail together,
/// and a pin authorising half of them would describe a subset the writer cannot
/// produce. Per-row pinning is owed the same group as a per-row commit unit.
fn repricing_pin(report: &serde_json::Value, operation_id: Uuid) -> Vec<u8> {
    repricing_run_content_hash(report, operation_id).to_vec()
}

/// Why the expansion came back empty, in the operator's terms.
///
/// Two selectors are empty for a reason their own axes do not show, and for those
/// two a bare *"matched nothing"* sends an author hunting the catalog for rows
/// that are there:
///
/// * one naming **no axis at all** is the whole published catalog, so an empty
///   result means the tenant has none outside the retained class;
/// * one naming a **cohort** without naming `existing_grandfathered` is empty *by
///   construction*, whatever the tenant has published. A cohort exists only on the
///   retained class — `check_cohort_eligibility`'s biconditional — and that class
///   is excluded until the eligibility axis names it (`inst-mp-grandfathered`), so
///   the two conditions cannot both hold. Naming the class beside the cohort is
///   the request the author meant, and the message says so rather than leaving
///   them to derive it from two rules in different documents.
fn empty_selector_detail(selector: &RunSelector) -> String {
    let because = if selector.is_unconstrained() {
        " and named no axis at all, so this tenant has no published row outside the retained \
         grandfathered class"
    } else if selector.cohort.is_some_and(|cohort| !cohort.is_none())
        && !selector.admits_grandfathered()
    {
        ". A cohort names a retained generation, and the retained class is excluded until the \
         price_eligibility axis names existing_grandfathered, so this selector is empty whatever \
         is published; name the class beside the cohort"
    } else {
        ""
    };
    format!(
        "the selector matched no published price row{because}; a run over an empty set completes \
         the moment it opens, which would report a mass adjustment that never happened"
    )
}

/// The run's parameters, frozen onto its report.
///
/// `report` is the **only** column `pricing_bulk_operation` has for this: §6 gives
/// a run `kind`, `state`, `client_key`, `report`, `submitted_by` and timestamps,
/// and the apply needs the adjustment and the instant that were accepted. So they
/// go here, and they are rendered from the **parsed domain values** rather than
/// echoed from the request — an echo would store a currency the parse
/// uppercased and a region it trimmed, i.e. a record of what was sent instead of a
/// record of what was selected against.
///
/// `rows` is deliberately **not** in it. The row set is the journal's
/// (`inst-mr-journal`), and a second copy on the report would be a second answer
/// to "what did this run select" that no transition keeps in step.
fn frozen_report(
    selector: &RunSelector,
    adjustment: &Adjustment,
    changeover: OffsetDateTime,
    selected: usize,
) -> serde_json::Value {
    let amounts: serde_json::Value = adjustment.amounts().map_or_else(
        || serde_json::json!({}),
        |set| {
            set.iter()
                .map(|(currency, value)| (currency.as_str().to_owned(), serde_json::json!(value)))
                .collect()
        },
    );
    serde_json::json!({
        "selector": {
            "plan_id": selector.plan_id.map(|p| p.get().to_string()),
            "currency": selector.currency.as_ref().map(CurrencyCode::as_str),
            "region": selector.region.as_ref().map(Region::as_str),
            "phase": selector.phase.map(|p| p.get().to_string()),
            "price_eligibility": selector.price_eligibility.map(PriceEligibility::as_str),
            "charge_kind": selector.charge_kind.map(ChargeKind::as_str),
            "cohort": selector.cohort.map(|c| c.to_string()),
            "meter": selector.meter.as_ref().map(Meter::as_str),
            "dimension_key": selector.dimension_key.as_ref().map(DimensionKey::as_str),
        },
        "adjustment": {
            "adjustment_kind": adjustment.kind().as_str(),
            "magnitude_kind": adjustment.magnitude_kind().as_str(),
            "adjustment_value": adjustment.percent_bp(),
            "amounts": amounts,
        },
        // main rendered this with `to_rfc3339()` (`+00:00`), and the document is
        // what `repricing_run_content_hash` pins, so a unit opened before the type
        // change must still digest to its `content_hash` after it.
        "changeover": format_rfc3339(changeover),
        "selected": selected,
    })
}

/// The run and its journal, as one view.
///
/// The journal is read on **every** rendering, including the `POST`'s, because
/// `inst-mr-return` promises a run ref *and* a progress endpoint and a `202` whose
/// journal came from the caller's own request rather than from the store would be
/// a receipt for a freeze that may not have happened. That is the defect D-225
/// records for the overlay submit's `202`.
async fn run_view(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    run: &BulkOperationRecord,
) -> Result<RepricingRunView, CanonicalError> {
    let journal = repricing_journal_repo::list_for_run(runner, scope, tenant_id, run.operation_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    Ok(RepricingRunView {
        run_id: run.client_key.clone(),
        operation_id: run.operation_id,
        state: run.state.as_str().to_owned(),
        report: run.report.clone(),
        submitted_at: format_rfc3339(run.submitted_at),
        completed_at: run.completed_at.map(format_rfc3339),
        journal: journal
            .into_iter()
            .map(|row| RepricingJournalRowView {
                price_id: row.price_id,
                state: row.state.as_str().to_owned(),
                failure_reason: row.failure_reason,
                applied_price_id: row.applied_price_id,
                applied_at: row.applied_at.map(format_rfc3339),
            })
            .collect(),
    })
}

/// The submitted selector as the domain's.
///
/// Each axis is validated by the **same** constructor the authoring plane uses, so
/// a selector cannot name a value no row could have been filed under: a blank
/// region, a two-letter currency or a token outside the eligibility enumeration is
/// refused here rather than silently matching nothing and being reported as
/// `RUN_SELECTOR_EMPTY` — which would send an operator hunting for missing rows
/// when what they have is a typo.
fn selector_of(request: &RepricingSelectorRequest) -> Result<RunSelector, DomainError> {
    Ok(RunSelector {
        plan_id: request.plan_id.map(PlanId::new),
        currency: request
            .currency
            .as_deref()
            .map(CurrencyCode::new)
            .transpose()?,
        region: request.region.as_deref().map(Region::new).transpose()?,
        phase: request.phase.map(PhaseId::new),
        price_eligibility: optional_token(
            "selector.price_eligibility",
            request.price_eligibility.as_deref(),
            price_repo::PRICE_ELIGIBILITIES,
            PriceEligibility::as_str,
        )?,
        charge_kind: optional_token(
            "selector.charge_kind",
            request.charge_kind.as_deref(),
            price_repo::CHARGE_KINDS,
            ChargeKind::as_str,
        )?,
        cohort: request.cohort.map(Cohort::Generation),
        meter: request.meter.as_deref().map(Meter::new).transpose()?,
        dimension_key: request.dimension_key.as_deref().map(DimensionKey::new),
    })
}

/// The `plan x write` gate the `POST` takes.
///
/// `resource_id = None` for `bulk_imports`' reason, sharpened by D-281: a run
/// spans plans and its selector names a *set*, so there is no single resource to
/// name — and an id-shaped constraint could not filter these tables anyway.
async fn write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(ctx.subject_tenant_id())),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)
}
