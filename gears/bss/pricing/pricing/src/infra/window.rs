//! `WindowService` — the three window mutations, each as one publish unit (D-99).
//!
//! `POST /prices/{priceId}/windows`, `PATCH` and `DELETE
//! /price-windows/{windowId}` are the surfaces; this is what they call. Each is
//! one transaction in `PublishService::commit`'s own sequence: re-validate,
//! request the version (D-156 — after re-validation, before the writes), write
//! the row change, record the pending ref that carries the plan subject, enqueue
//! the event, write the audit record. **No interim state**: a mutation that
//! cannot get a version leaves the store exactly as it found it.
//!
//! # Why a window mutation is a publish and not a store write
//!
//! `inst-ws-publishunit` (D-99): the read model carries window **intervals**, and
//! predicate (1) of the sellability gate plus the D-80 coverage horizon are
//! required to be evaluable from the **pinned** model (`inst-sg-pinned`). A
//! cancellation that only moved the truth side would leave the last-warmed delta
//! advertising coverage the store had removed — selling into exactly the trailing
//! void D-62 → D-80 → D-94 closed. So the mutation re-projects the plan subject
//! and is consumer-visible only at `CatalogVersionPublished` + warm, which is why
//! all three surfaces answer **202** and not 200.
//!
//! The unit is [`PlanPublishUnit::window_mutation`] — G1's discriminator, not a
//! second unit type, and the `revision` it names is the plan's **current** one: a
//! window mutation authors no plan content and opens no revision.
//!
//! # What is re-validated here, and what deliberately is not
//!
//! `inst-fg-when` says the coverage check runs at publish **and** inside every
//! window-mutating operation. It says that in §3's *Future-Gap Detection*
//! section, about that section's two rules — `inst-fg-detect`'s interior gap and
//! `inst-fg-trailing`'s trailing void. It does **not** reach §3's
//! *Publish-Time* Window Coverage subsection, and the difference is not a
//! technicality:
//!
//! * `inst-wc-required` refuses a billable row whose key holds no live window.
//!   Run here it would refuse the very mutation that fixes it — an operator
//!   scheduling the first window of an uncovered key would be told the key is
//!   uncovered. A rule whose enforcement blocks its own remedy is a deadlock,
//!   and publish is where that rule belongs because publish is the freezing act.
//! * `inst-wc-availability` judges the **plan's** purchasability dates against
//!   per-key coverage; it is a finding about the plan, and the publish report is
//!   its surface (`api::rest::windows` says the same thing about the coverage
//!   GET).
//!
//! And the two rules that *do* run here are scoped to **the key being mutated**.
//! A mutation can only move that key's interval set, so a pre-existing gap on a
//! sibling key is a fault this call neither caused nor can fix; refusing on it
//! would make every key of a plan hostage to the worst one.
//!
//! # The row set is the validation set, not the projected set
//!
//! This path is on the **validation** side of the crate's one deliberate
//! asymmetry, and it inherits that side rather than choosing it. The plan's rows
//! reach it through `publish::assemble_from`, which loads `CANDIDATE_ROW_STATES` —
//! `published` plus the `draft` rows a publish would produce; the window plane
//! reaches it through `window_repo::list_for_plan`, which is every state over every
//! row; and the overlap check inside `window_repo` runs over
//! `price_repo::load_scope_keys_for_plan`, which is draft-inclusive for its own
//! stated reason. All three are the readers that ask "*if* this were current, would
//! the interval set be sound?".
//!
//! The two readers that assert fact **to a consumer** — `read_model::project_windows`
//! and the activation sweep — restrict to `PROJECTED_ROW_STATES` instead. That
//! asymmetry is the point and not an inconsistency to tidy: a mutation refused for a
//! draft row's key is refused conservatively, while a *projection* that included one
//! would advertise coverage nothing published.
//!
//! # The plan shape comes from the **current** revision, and it has to
//!
//! `infra::publish::assemble` resolves the plan's **open draft** and answers
//! `NotFound` when there is none. A window mutation's ordinary subject is a
//! *published* plan with no draft at all, so assembling that way would make the
//! whole surface answer 404 for exactly the case it exists to serve. This path
//! resolves `plan_repo::load_current` and calls `publish::assemble_from` with it,
//! which is not a convenience but G1's decision:
//! [`PlanPublishUnit::window_mutation`]'s own doc fixes the unit's revision as the
//! plan's current one, "and not an open draft's".
//!
//! Two facts come off that single read and both matter. The revision **number**
//! is what the pending ref pins, so the projector freezes the revision this call
//! validated rather than whichever is current when the sweep arrives (D-165). The
//! revision's **`lifecycle_state`** is what the ref carries as the subject state,
//! and taking it off the same record is what keeps this path from asking twice
//! which revision is current and getting two answers.
//!
//! A plan that has **never published** therefore has no window surface at all —
//! `NotFound`, because there is no current revision to project and a window on an
//! unpublished plan's draft row has nothing to be consumer-visible *in*.
//!
//! # And that refusal is a **rule** (D-314), not this function's choice of reader
//!
//! Argued from `load_current`'s state list alone — which is all the paragraph above
//! does — the 404 reads like an accident of reusing one repository function, and an
//! accident is something a later author deletes. It is not one, and the two grounds
//! were measured by lifting the restriction rather than reasoned about.
//!
//! **The store has no `draft` to pin.** Step 6 records
//! `(subject_revision, subject_lifecycle_state)`, and
//! `chk_pricing_catalog_version_ref_subject_lifecycle` admits
//! `NULL | 'published' | 'retired'` — read off the constraint as it now stands after
//! `pricing_catalog_version_ref`'s rebuild, not off the migration that first wrote it. Relaxing
//! the domain check for the **schedule** verb alone takes three edits, one per
//! `load_current` in the path (`read_plan_context` below,
//! `approval::current_revision_shape`, `approval::submit_window_mutation_on`), and the
//! act then runs the whole way — coverage passes, the evaluator answers
//! `Material { reason: FirstPublish }`, a second principal approves, the registry
//! answers a handle — before aborting at the write with a `CHECK` failure surfaced as
//! `DomainError::Internal`. A partial relaxation converts a truthful 404 into a 500.
//!
//! **And the projector's licence does not extend to a draft.** `read_model` pins the
//! revision *number* and the lifecycle *state* on the ref row and deliberately pins no
//! content, on the stated premise that "a published revision row and its
//! revision-scoped children are physically immutable". A draft's are not:
//! `plan_shape_repo::set_descriptor_set` rewrites a draft revision's descriptor set
//! after the ref naming it exists, and `plan_repo::load_revision` — the projector's
//! content read — carries no state filter and hands the draft back. The sweep arrives
//! up to D-47's five-minute batching maximum later and writes an INSERT-only row on the
//! seven-year horizon, so a draft-pinned delta would freeze whatever the draft said at
//! sweep time: content no rule judged and that keeps moving afterwards.
//!
//! # A billable row cannot ride a plan's **first** publish — it takes two, and
//! both are reachable
//!
//! The two sections above state a fact and why it is a rule; this states the
//! consequence it has, and the consequence is narrower than it looks. Put together:
//!
//! * `plan_repo::load_current` resolves `published | retired`
//!   ([`LifecycleState::is_current_revision`](crate::domain::lifecycle::LifecycleState::is_current_revision)),
//!   so all three window mutations answer **404 `current plan revision not found`**
//!   on a plan whose only revision is a `draft`. Measured over HTTP, not inferred.
//! * `inst-wc-required` refuses a publish whose billable key holds **no** live
//!   window, and that rule is at publish for the reason the section above gives.
//!
//! So a billable row's key needs a window before the publish that freezes the row,
//! and the window needs a revision the publish has already frozen. **The order is
//! therefore forced: an empty first publish, then the row and its window, then the
//! publish that freezes them.** Nothing is unreachable.
//!
//! **A plan's first publish is reachable through the mounted surfaces**, and what
//! makes it so is that `coverage::check` ranges over the **billable** set: a plan
//! with no price row presents no key to find uncovered, and `run_publish_rules`
//! carries no minimum-row rule. A draft plan whose *shape* is sound publishes with an
//! empty row set, which leaves a current revision for a window mutation to freeze.
//! `rest_windows.rs::a_plans_first_window_is_authorable_through_the_routes_after_an_empty_publish`
//! executes the whole sequence through the mounted surfaces — submit, an independent
//! approve, the commit, `POST …/plans/{planId}/prices`, the threshold-policy `PUT` and
//! *its* independent approve, `POST …/prices/{priceId}/windows` — and ends on a
//! **mutating** 202.
//!
//! The policy step joined that list when the schedule began consulting the evaluator,
//! and it is not a detour: an unconfigured tenant has every act material, so a schedule
//! would answer with a unit rather than a window. It is the same two-person shape as the
//! first publish, so what is left is still an ordering constraint and not a deadlock.
//!
//! **What the sequence costs (D-314).** The ordering needs no design decision; what
//! it leaves behind does. The empty publish is a real publish:
//! it commits a version and freezes a delta carrying `lifecycleState: "published"`,
//! `prices: []` and `windows: []`, INSERT-only on the seven-year horizon. Of
//! `inst-sg-pinned`'s six predicates the three a version can answer today — committed
//! addressability, the purchasability dates and the plan lifecycle state — all answer
//! affirmatively, and the rest have no key to be evaluated on, because
//! `inst-sg-conjunction` ranges over the plan's keys on a bound `(currency, region)`
//! and this revision publishes none. Whether a storefront can bind a market on a plan
//! that publishes no key is Subscriptions' half of the joint gate; what is recorded is
//! the artifact, not a verdict about it, and D-314 states why closing it is a change to
//! the sellability gate rather than a change here. A plan that *already* carries rows
//! pays the same order the hard way — the rows come out, the plan publishes empty, the
//! rows go back — because no other surface authors a window either (`infra::import` has
//! none by its own design). The in-crate fixtures schedule their coverage window
//! through `window_repo` directly, for isolation and speed.
//!
//! # The two-person control runs here, on **all four** acts, and two of them for a
//! # different reason than the other two
//!
//! `inst-mat-registered` puts *"window cancellation and `effectiveTo` shortening"* on
//! the **always-material** trigger list, and D-62's Problem paragraph is the act it
//! exists to stop: one `plan × write` holder cancels the two-person-approved
//! scheduled successor, the active window expires at its natural end, and every
//! arrears charge on the key fails closed. So the `DELETE` and a **shortening**
//! `PATCH` do not commit on one principal. Each one:
//!
//! 1. runs every check below — the state machine's, the interior gap, the trailing
//!    void — because a mutation that cannot commit must not be put in front of a
//!    reviewer (`api::rest::publish` makes the same argument about its own submit
//!    arm);
//! 2. looks for an **approved** unit over this window *and this content*;
//! 3. finding none, writes **nothing at all** and answers
//!    [`WindowMutationOutcome::SubmittedForApproval`] — the route opens the pinned
//!    unit through `ApprovalService::submit_window_mutation` and answers 202;
//! 4. finding one, commits under it, and the audit record carries its
//!    `approval_ref`.
//!
//! **All four acts run the same four steps.** For a `POST` (a schedule) and a
//! **lengthening** `PATCH` what puts them on step 3 is not the trigger list but
//! [`crate::domain::materiality::evaluate`]'s answer over the tenant's configured
//! policy, read inside this transaction through
//! [`crate::infra::threshold::effective_policy_at`] — the `_at` form, at the instant
//! the act's own stamp carries, never the `OffsetDateTime::now_utc()` wrapper. Under D-188 a version
//! is not the policy before its `effective_from`, so a mutation that read the wall
//! clock while stamping some other instant could decide a verdict against a policy
//! the record it wrote says was not yet in force. `api::rest::windows` stamps from
//! `OffsetDateTime::now_utc()`, so no route reaches that; an in-process caller holding a fixed stamp
//! does, and every service-level suite is one. `infra::threshold`'s own doc states
//! the rule this obeys, and `api::rest::publish` obeys it with the submit's instant.
//!
//! What that means concretely, because it is narrower than "a schedule may now need an
//! approval": a window mutation moves an interval and no money, so **every row of its
//! change set has a delta of zero** and no bar can be reached. The only rule left that
//! can answer is `inst-mat-percurrency`'s fail-safe half — *"a row whose currency has
//! no threshold entry is material"* — and its consequence is exactly the design set's
//! own bootstrap: a tenant that has configured nothing has every act material,
//! including these two.
//!
//! **The verdict is minted in one place and it is not here.** `Op::registered_trigger`
//! hands over *which act this is* (D-176: decided inside the transaction, against the
//! stored row) and the evaluator decides *what that means*. The constant this module
//! used to write — `MaterialityReason::AlwaysMaterialTrigger` on the controlled arm —
//! was true of D-62's two and would have been a lie about the other two, which reach
//! that arm because a threshold rule answered.
//!
//! **What a refused act cannot yet do, reported rather than papered over.** A
//! materially-refused `PATCH` or `DELETE` completes: the window id is in the path, so
//! the call made after an independent approve finds its own unit. A materially-refused
//! **`POST`** does not: the window id is minted per request, so the second `POST` names
//! a different subject and opens a second unit rather than finding the first approved.
//! The refusal direction is safe — nothing is written either way — and the remedy is a
//! decision about what a *schedule* unit's subject is (a caller-supplied window id, or
//! the `price_unit` kind that has no writer), which is not a code group's to take. The
//! reachable operator sequence is the design set's own: configure a threshold policy
//! first, and a schedule commits on one principal.
//!
//! # The 503 from the unconfigured registry is the expected answer here
//!
//! The module boots with `UnconfiguredCatalogVersionRegistryV1` unless
//! `CatalogVersionSource::LocalDevInventedVersions` is configured, in which case
//! `LocalDevCatalogVersionRegistryV1` answers instead. Under the default every
//! mutation rolls back at step 2 exactly as a plan publish does, and the tests
//! assert the arm was **reached**. The local implementation stays opt-in because
//! a gear inventing versions is a second incrementer, which `pricing-sdk`'s
//! registry contract refuses outright.

use std::sync::Arc;

use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::coverage::{self, WINDOW_TRAILING_VOID};
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{self, ChangeSet, MaterialityVerdict, PublishedPriceBaseline};
use crate::domain::plan_shape::PlanShape;
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::price_record::PriceRecord;
use crate::domain::publish::PlanPublishUnit;
use crate::domain::read_model::SubjectRef;
use crate::domain::scope_key::{PlanId, PriceEligibility, ScopeKey};
use crate::domain::validation::ValidationReport;
use crate::domain::window::{self, CoverageEnd, KeyWindows, WindowInterval, WindowState};
use crate::infra::publish::assemble_from;
use crate::infra::registry_deadline::request_version_now;
use crate::infra::storage::repo::approval_repo::ApprovalRecord;
use crate::infra::storage::repo::audit_repo::NewAuditEntry;
use crate::infra::storage::repo::outbox_repo::{NewOutboxEvent, PriceWindowTransitionPayload};
use crate::infra::storage::repo::window_repo::{NewWindow, WindowRecord};
use crate::infra::storage::repo::{
    PendingVersionRow, audit_repo, catalog_version_ref_repo, outbox_repo, plan_repo, price_repo,
    window_repo,
};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// What a window mutation answers with — the pending handle and the row as it
/// now stands.
///
/// It carries the **pending** ref and no `CatalogVersion`, which is the 202 in a
/// type: a committed number does not exist yet and will not until the registry
/// batches (D-47). A field for it would be a field every success left empty.
#[derive(Clone, Debug)]
pub struct WindowMutationReceipt {
    /// The window the mutation created or moved.
    pub window_id: Uuid,
    /// The plan whose subject re-projects.
    pub plan_id: PlanId,
    /// The price row the window is bound to.
    pub price_id: Uuid,
    /// The plan revision this mutation's re-projection will freeze — the current
    /// one.
    pub revision: u64,
    /// The registry handle the projector resolves.
    pub pending_version_ref: String,
    /// The interval as stored after the mutation.
    pub effective_from: OffsetDateTime,
    /// The end as stored after the mutation; `None` is open-ended.
    pub effective_to: Option<OffsetDateTime>,
    /// The state as stored after the mutation.
    pub state: WindowState,
    /// The act sequence the window stands at **after** this act — the entity tag the
    /// caller's next act must assert (D-190, D-191).
    ///
    /// On the receipt because there is no `GET` on a window: the mutating verbs are
    /// the only producers of the tag, so a surface that did not carry it here would
    /// require a precondition its own caller could not obtain.
    pub mutation_seq: u64,
}

/// What a window mutation did: it ran, or it opened a unit and stood still.
///
/// Two arms rather than a receipt with an empty half, for the reason
/// [`WindowMutationReceipt`] carries no `CatalogVersion`: on the controlled arm there
/// is no pending ref, no moved interval and no new state, so a receipt would be a
/// receipt every field of which is a lie. The route reads the arm to know which
/// status it is answering — the shape `api::rest::publish`'s `outcome` token already
/// gives the publish surface.
#[derive(Clone, Debug)]
pub enum WindowMutationOutcome {
    /// The mutation committed. The store moved and the plan subject re-projects.
    Committed(Box<WindowMutationReceipt>),
    /// D-62: the act is always-material and no approved unit covers it, so
    /// **nothing was written**. The caller opens the unit and re-issues the mutation
    /// once a second principal has approved it.
    SubmittedForApproval(Box<PendingApproval>),
}

/// The facts a caller needs to open the unit a controlled act requires.
///
/// The window's own coordinates plus the verdict, so the route opens the unit without
/// re-reading a world the refusing transaction already read. It carries no
/// `content_hash`: the pin is taken inside `submit_window_mutation`'s own
/// transaction, which is what makes it a pin rather than a value passed between two.
#[derive(Clone, Debug)]
pub struct PendingApproval {
    /// The window the act is about.
    pub window_id: Uuid,
    /// The plan whose subject the act would re-project.
    pub plan_id: PlanId,
    /// The price row the window is bound to.
    pub price_id: Uuid,
    /// The plan revision the act would freeze — the current one.
    pub revision: u64,
    /// The interval as it **still** stands, unmoved.
    pub effective_from: OffsetDateTime,
    /// The end as it **still** stands, unmoved.
    pub effective_to: Option<OffsetDateTime>,
    /// The state as it **still** stands, unmoved.
    pub state: WindowState,
    /// Why a second principal is required — the evaluator's own reason, carried as
    /// a value so the unit's stored verdict and this answer cannot disagree.
    ///
    /// **Not always [`MaterialityReason::AlwaysMaterialTrigger`]**, which this doc
    /// asserted while its producer 1100 lines down recorded the opposite: a cancel
    /// and a shortening are always-material by `inst-mat-registered` and need no
    /// threshold, and the other two acts reach this arm because a *threshold* rule
    /// answered — including the currency that has no threshold entry at all, which
    /// is the one reason an operator can act on.
    pub verdict: MaterialityVerdict,
    /// **The unit that is now reviewing the act**, opened inside the same transaction
    /// that refused it.
    ///
    /// The record travels here, not the *subject* with the route opening the unit in a
    /// second transaction. The `POST`'s idempotency gate records the response body it
    /// is handed and that body names the approval: a unit opened after the guarded
    /// transaction committed would be missing from the recorded answer, and a replay
    /// would hand the second caller something the first never received. See
    /// [`mutate_in`].
    pub approval: ApprovalRecord,
}

/// How a window unit's stored `materiality` payload is rendered.
///
/// A function travelling in rather than a type this module names, for the reason
/// `infra::idempotent`'s renderer closure gives: DE0202 forbids `infra` from importing a
/// `*View` type, and the stored jsonb is the **same document** the approvals surface
/// renders back out. So the one producer of that shape stays in `api::rest` and arrives
/// here as a function pointer — which is also what keeps a unit opened on this path and
/// one opened on the publish path from storing two different documents.
pub type VerdictJson = fn(&MaterialityVerdict) -> Result<JsonValue, DomainError>;

/// The window mutation workflow over one database provider.
#[derive(Clone)]
pub struct WindowService {
    db: DBProvider<DbError>,
    registry: Arc<dyn CatalogVersionRegistryV1>,
}

impl WindowService {
    /// Build the workflow over one provider and the resolved registry.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>, registry: Arc<dyn CatalogVersionRegistryV1>) -> Self {
        Self { db, registry }
    }

    /// Schedule a window on a price row (`POST /prices/{priceId}/windows`).
    ///
    /// **It commits on one principal only when a configured threshold says so**, and
    /// that is the correction G6 owed here. A schedule is not on
    /// `inst-mat-registered`'s trigger list, so nothing about it is *always*
    /// material — but "not always material" is not "never material", and while the
    /// threshold policy had no store this method returned a bare receipt on the
    /// argument that its controlled arm was unreachable by construction. The policy
    /// has a store, a surface and an approved-version reader now, so the arm is
    /// reachable and the return type says so: a tenant with no configured entry for
    /// the plan's currency gets `inst-mat-failsafe`, a unit and **no window at all**.
    ///
    /// # What a schedule's threshold question actually is
    ///
    /// A schedule moves an interval and no money, so every row of its change set has
    /// a delta of zero and no bar can be reached. What decides it is therefore the
    /// per-currency fail-safe: *"a row whose currency has no threshold entry is
    /// material"*. That is the whole of it, and it is evaluated by
    /// [`crate::domain::materiality::evaluate`] rather than restated here.
    ///
    /// # Errors
    /// [`DomainError::WindowStartInPast`] when `effective_from` is not strictly
    /// future (D-63, checked **before** the store for the reason
    /// [`Self::adjust_effective_to`] states); [`DomainError::WindowOverlap`] on a
    /// sibling interval; [`DomainError::WindowTrailingVoid`] never — a schedule
    /// only adds coverage; [`DomainError::ValidationFailed`] when the schedule
    /// opens an interior gap on the key; [`DomainError::NotFound`] for an unknown
    /// price row or a plan with no revision at all;
    /// [`DomainError::PendingChangeUnitExists`] when a unit already holds the key;
    /// [`DomainError::CatalogVersionUnavailable`] when the registry cannot be
    /// reached or has none configured; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a fact only the caller holds - the compiled scope and tenant, \
                  the row the path named, the interval the body carried, the id the surface minted \
                  and the stamp the edge established. `ApprovalService::submit` carries the same \
                  note for the same reason: a request struct here would be an `api::rest` DTO \
                  named in `infra`, which DE0301 and the layer discipline both refuse"
    )]
    pub async fn schedule(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_id: Uuid,
        window_id: Uuid,
        effective_from: OffsetDateTime,
        effective_to: Option<OffsetDateTime>,
        reason_code: String,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<WindowMutationOutcome, DomainError> {
        // `inst-ws-future-start` ahead of the store, which is the whole of what
        // debt 2 was: `check_creation` and `check_cancellation` had no non-test
        // caller, so `WINDOW_START_IN_PAST` and `WINDOW_NOT_CANCELLABLE` were
        // unreachable over HTTP. It is **not** a check the store lacks - the
        // trigger refuses a past start too - it is the difference between a 400
        // naming the field and a 500 out of a trigger.
        window::check_creation(effective_from, effective_to, stamp.recorded_at)?;

        self.mutate(
            ctx,
            scope,
            tenant_id,
            window_id,
            Subject::NewOn { price_id },
            stamp,
            verdict_json,
            schedule_plan(price_id, effective_from, effective_to, reason_code),
        )
        .await
    }

    /// [`Self::schedule`] **inside a transaction the caller owns** — the `POST`'s
    /// guarded form (D-191).
    ///
    /// The same domain pre-check and the same plan closure; the only difference is who
    /// opened the transaction. `infra::idempotent::guarded` owns it on this path, so the
    /// claim that makes the act at-most-once and the act itself commit together or not
    /// at all — which is the property `idempotency_repo`'s doc says the guarantee
    /// inverts without: "the claim commits while the mutation does not, and every retry
    /// of a request that never happened is answered 'already done', forever."
    ///
    /// # Errors
    /// [`Self::schedule`]'s, exactly.
    #[allow(
        clippy::too_many_arguments,
        reason = "`Self::schedule`'s reason and its arguments, plus the transaction the \
                  gate owns. The alternative is a request struct named in `infra` that \
                  no surface has, which DE0301 refuses"
    )]
    pub async fn schedule_in(
        &self,
        runner: &impl DBRunner,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_id: Uuid,
        window_id: Uuid,
        effective_from: OffsetDateTime,
        effective_to: Option<OffsetDateTime>,
        reason_code: String,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<WindowMutationOutcome, DomainError> {
        window::check_creation(effective_from, effective_to, stamp.recorded_at)?;
        mutate_in(
            runner,
            &self.registry,
            ctx,
            scope,
            tenant_id,
            window_id,
            Subject::NewOn { price_id },
            stamp,
            verdict_json,
            schedule_plan(price_id, effective_from, effective_to, reason_code),
        )
        .await
    }

    /// Move a window's future `effectiveTo` (`PATCH /price-windows/{windowId}`).
    ///
    /// # A **shortening** move is always-material and takes two principals
    ///
    /// D-62, and it is decided inside the transaction against the stored end
    /// ([`window::shortens`](crate::domain::window::shortens)): a move that removes
    /// coverage answers [`WindowMutationOutcome::SubmittedForApproval`] and writes
    /// **nothing** until an approved unit covers this window and this content. An
    /// **extension** commits directly — it is not on the trigger list, and its
    /// materiality is the threshold policy's question (see the module doc's split and
    /// G6's obligation).
    ///
    /// # The domain check runs before the store and is **not** more permissive
    ///
    /// `window_repo::adjust_effective_to`'s doc forbids a pre-check more
    /// permissive than the floor beneath it, because that only relocates where
    /// the 500 comes from. Both sides consult
    /// [`window::frozen_end`](crate::domain::window::frozen_end) — the single
    /// statement of `inst-ws-immutable` the G1 fix wave established — so the two
    /// answers cannot diverge by construction, and no third spelling is added
    /// here. What the pre-check buys is that the refusal is decided before a
    /// registry round-trip is spent inside an open transaction.
    ///
    /// # Errors
    /// [`DomainError::WindowHistoricalImmutable`] on each of `frozen_end`'s three
    /// grounds; [`DomainError::WindowTrailingVoid`] when the move would leave the
    /// key uncovered through its floor; [`DomainError::WindowOverlap`] when an
    /// extension collides with a sibling; [`DomainError::ValidationFailed`] on an
    /// interior gap; [`DomainError::NotFound`] for an unknown window;
    /// [`DomainError::CatalogVersionUnavailable`] when the registry cannot be reached
    /// or has none configured;
    /// [`DomainError::StaleVersion`] when the window has been acted on since
    /// `expected_seq` was read; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "`Self::schedule`'s reason, plus the one this act has that the other \
                  two do not: the act sequence the caller read the window at (D-191's \
                  `If-Match`). It is a fact only the caller holds - the whole point of \
                  a precondition is that the store did not choose it - so it cannot be \
                  derived here, and bundling the six into a struct would name a request \
                  type in `infra` that no surface has"
    )]
    pub async fn adjust_effective_to(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        window_id: Uuid,
        effective_to: Option<OffsetDateTime>,
        expected_seq: u64,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<WindowMutationOutcome, DomainError> {
        let now = stamp.recorded_at;
        self.mutate(
            ctx,
            scope,
            tenant_id,
            window_id,
            Subject::Existing,
            stamp,
            verdict_json,
            move |plan| {
                let current = plan.current.as_ref().ok_or_else(|| DomainError::NotFound {
                    subject: "price window".to_owned(),
                    id: window_id.to_string(),
                })?;
                window::check_effective_to_adjustment(
                    &WindowInterval::new(
                        current.effective_from,
                        current.effective_to,
                        current.state,
                    ),
                    effective_to,
                    now,
                )?;
                Ok(Planned {
                    price_id: current.price_id,
                    effective_from: current.effective_from,
                    effective_to,
                    reason_code: current.reason_code.clone(),
                    op: Op::Adjust { expected_seq },
                    plan,
                })
            },
        )
        .await
    }

    /// Cancel a not-yet-active window (`DELETE /price-windows/{windowId}`).
    ///
    /// A **state flip**, never a deletion: the store refuses `DELETE`
    /// unconditionally, because a cancelled schedule is a fact an auditor is
    /// entitled to see.
    ///
    /// # Always-material, unconditionally (D-62)
    ///
    /// The act D-62's Problem paragraph exists to stop — *"one operator could delete
    /// the two-person-approved scheduled successor, let the active window expire at
    /// its natural end, and leave every arrears charge and renewal on the key failing
    /// closed"*. So the first call opens a pinned unit and **the window stays
    /// `scheduled`**; the flip happens on the call that follows an independent
    /// approve. No threshold policy is consulted, because D-62 does not depend on one.
    ///
    /// # Errors
    /// [`DomainError::WindowNotCancellable`] for any state but `scheduled`
    /// (checked before the store — the other half of debt 2);
    /// [`DomainError::WindowTrailingVoid`] when the cancel would leave the key
    /// uncovered through its floor; [`DomainError::NotFound`] for an unknown
    /// window; [`DomainError::CatalogVersionUnavailable`] when the registry cannot be
    /// reached or has none configured; [`DomainError::Internal`] on a storage failure.
    pub async fn cancel(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        window_id: Uuid,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<WindowMutationOutcome, DomainError> {
        self.mutate(
            ctx,
            scope,
            tenant_id,
            window_id,
            Subject::Existing,
            stamp,
            verdict_json,
            move |plan| {
                let current = plan.current.as_ref().ok_or_else(|| DomainError::NotFound {
                    subject: "price window".to_owned(),
                    id: window_id.to_string(),
                })?;
                window::check_cancellation(current.state)?;
                Ok(Planned {
                    price_id: current.price_id,
                    effective_from: current.effective_from,
                    effective_to: current.effective_to,
                    reason_code: current.reason_code.clone(),
                    op: Op::Cancel,
                    plan,
                })
            },
        )
        .await
    }
}

/// Which of the three mutations is running.
///
/// A closed enumeration rather than three copies of the transaction body: the
/// sequence — re-validate, request, write, record the ref, enqueue, audit — is one
/// sequence, and three copies of it would be three places for step 3 to drift out
/// of the order D-156 fixed it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Schedule,
    /// A `PATCH`, carrying the **act sequence the caller read the window at** —
    /// D-191's `If-Match`, travelling with the act rather than beside it.
    ///
    /// On the variant and not on `mutate`'s parameter list because it belongs to
    /// exactly one of the three acts: the `POST` has no window to have read, and §5
    /// declares no precondition for the `DELETE` (D-191 clause (3) — an empty cell
    /// left empty, rather than a pattern completed by analogy). A parameter every
    /// caller had to pass would make two of them pass a value that means nothing.
    Adjust {
        expected_seq: u64,
    },
    Cancel,
}

impl Op {
    /// This operation's name inside a registry request id.
    ///
    /// Short and stable: it is one field of [`unit_request_id`]'s handle, so
    /// re-spelling it strands every outstanding pending ref the old spelling
    /// produced. See that function for what the whole handle has to identify.
    const fn token(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Adjust { .. } => "adjust",
            Self::Cancel => "cancel",
        }
    }

    /// Does this operation **remove** coverage from the key?
    ///
    /// `inst-fg-trailing` names exactly two acts — "any cancel/shorten" — and this
    /// is that predicate. A schedule only adds intervals, so it cannot cross the
    /// floor, and evaluating the floor there would refuse the operator's own remedy
    /// on a key that is already short of it.
    ///
    /// An **extension** is a `PATCH` too and is deliberately included, which keeps
    /// this predicate coarser than D-62's. Running the floor check on an extension
    /// is sound rather than merely harmless: the floor is evaluated against the
    /// interval set the mutation *produces*, and an extension can only move the
    /// coverage end later, so any state that satisfied the floor still does — while
    /// a coarse predicate on a fail-closed floor errs toward checking, which is the
    /// direction it may err in.
    ///
    /// The exact shorten/extend question **is** asked, one guard along, by
    /// [`Self::is_always_material`] — and it is asked through
    /// [`window::shortens`](crate::domain::window::shortens), the single definition
    /// of "shorten" this crate has, rather than through a second spelling here.
    const fn may_remove_coverage(self) -> bool {
        matches!(self, Self::Adjust { .. } | Self::Cancel)
    }

    /// Which registered always-material trigger this act **is** (§3 step 4, D-62),
    /// if it is one.
    ///
    /// The two acts D-62 names, and no others:
    ///
    /// * a **cancel** — [`Trigger::WindowCancellation`], whatever the interval set
    ///   looks like afterwards;
    /// * a **shortening** `PATCH` — [`Trigger::WindowShortening`], decided by
    ///   [`window::shortens`](crate::domain::window::shortens) against the stored
    ///   end.
    ///
    /// A **schedule** and a **lengthening** `PATCH` answer `None`, and that is not
    /// "not material": it is *not on the trigger list*, which leaves their
    /// materiality to be decided by the threshold comparison the caller now performs.
    /// The predicate hands over the **fact** (which act this is) rather than a
    /// **verdict**, which is why it is not a bool:
    /// [`crate::domain::materiality::evaluate`] mints every verdict on
    /// this path, from one trigger list, and a second copy of §3 step 4 per surface is
    /// how a rule acquires as many owners as it has callers.
    ///
    /// It is answered **inside the mutating transaction**, off the row that
    /// transaction read (D-176). A classification made at the surface would be a
    /// hint: a concurrent extension committed between the two reads would send a
    /// shortening down the direct path, which is the one direction this must not
    /// fail in.
    ///
    /// [`Trigger::WindowCancellation`]: crate::domain::materiality::triggers::Trigger::WindowCancellation
    /// [`Trigger::WindowShortening`]: crate::domain::materiality::triggers::Trigger::WindowShortening
    fn registered_trigger(self, planned: &Planned) -> Option<Trigger> {
        match self {
            Self::Schedule => None,
            Self::Cancel => Some(Trigger::WindowCancellation),
            Self::Adjust { .. } => planned
                .plan
                .current
                .as_ref()
                .filter(|current| {
                    window::shortens(
                        &WindowInterval::new(
                            current.effective_from,
                            current.effective_to,
                            current.state,
                        ),
                        planned.effective_to,
                    )
                })
                .map(|_| Trigger::WindowShortening),
        }
    }

    /// The event this mutation emits — two of §7's four frozen names.
    const fn event(self) -> crate::infra::storage::repo::WindowMutationEvent {
        use crate::infra::storage::repo::WindowMutationEvent;
        match self {
            // An adjustment emits `PriceWindowScheduled`, and that is a reading
            // rather than a convenience: §7 names four events and none of them is
            // `PriceWindowAdjusted`, so the choice is between an existing name and
            // a minted one. What an adjustment publishes is a window's interval,
            // which is what `PriceWindowScheduled` carries — the payload holds
            // `effectiveFrom`/`effectiveTo`, so a consumer reads the new interval
            // off the same field either way. Minting a fifth name would add a
            // token to `chk_pricing_outbox_event_name` that no document declares.
            Self::Schedule | Self::Adjust { .. } => WindowMutationEvent::Scheduled,
            Self::Cancel => WindowMutationEvent::Cancelled,
        }
    }

    /// The audit verb, and there is exactly one for all three — which is why it is
    /// a `const` on the type rather than a function of the operation: a function
    /// would invite a second verb, and the argument below is that there must not be
    /// one.
    ///
    /// **`publish`, an already-declared token — nothing is minted here.** The
    /// warrant is `inst-ws-publishunit`'s own words: a window mutation "runs the
    /// Foundation engine path — validation → pending `CatalogVersion` ref → warm
    /// (Foundation §4.2)". S5 §6 declares `publish` against Foundation §4.2, so the
    /// act this records is the act the token names.
    ///
    /// The `create` / `update` / `delete` trio is **not** available for it: S5 §6
    /// declares those as "the draft-authoring mutations" and **D-175 clause (1)** is
    /// where their writers are enumerated — the plan facet, the draft revision and the
    /// draft price row, and nothing else. Stretching one onto the published plane
    /// therefore contradicts that enumeration rather than the closure rule, which is
    /// what this comment used to cite: clause **(2)** is *"no writer without a token"*
    /// and says the roster is audited against the surfaces the set specifies rather
    /// than appended to as records happen to land. The two point the same way here —
    /// clause (1) says these three verbs are not this act's, clause (2) says a code
    /// group may not add a fourth to the roster itself.
    ///
    /// **What tells the three apart is the state pair, not the verb**, and it tells
    /// them apart completely: a schedule has no `before_state`, a cancel moves
    /// `scheduled → cancelled`, and an adjustment moves an `effectiveTo` with the
    /// state unchanged. So one verb costs an auditor nothing — D-146's line, and
    /// D-179 clause (1)'s: a token names the act, not the field.
    const AUDIT_ACTION: AuditAction = AuditAction::Publish;
}

/// How a mutation names the row it is about.
///
/// The two arms exist because the three surfaces address different things: `POST`
/// names a **price row** and mints the window id, while `PATCH` and `DELETE` name
/// an existing **window**. Resolving both through one lookup would mean answering
/// "which window is this about" on a path where the answer is "none yet".
#[derive(Clone, Copy, Debug)]
enum Subject {
    /// A new window on a price row the caller named.
    NewOn { price_id: Uuid },
    /// A window that already exists.
    Existing,
}

/// The plan-scoped facts a mutation is validated against, read once inside the
/// writing transaction.
struct PlanContext {
    plan_id: PlanId,
    revision: u64,
    lifecycle_state: LifecycleState,
    /// The window as stored — `None` only on the schedule path.
    current: Option<WindowRecord>,
    /// The key the mutation's window sits on.
    key: ScopeKey,
    /// The key's interval set as stored, **before** the mutation.
    before: KeyWindows,
    /// W6's margin on the key's market: `Some(zero)` when the plan sells no
    /// recurring row there, `None` when the term has no value at all.
    margin: Option<time::Duration>,
    /// Every window of the plan with its id, as stored.
    plane: Vec<(Uuid, ScopeKey, WindowInterval)>,
    /// The plan's **published** price rows — the row set the mutation's
    /// re-projection freezes, and the baseline the same rows are compared against.
    ///
    /// The published plane alone, deliberately: a window mutation authors no plan
    /// content and publishes no draft row, so a change set built off the candidate
    /// set would put a row this act does not freeze in front of the evaluator - and
    /// `inst-mat-newrow` would then make every window mutation of a plan carrying an
    /// unpublished draft row material, for a row the act never touches.
    published: Vec<PriceRecord>,
    /// The plan as this transaction assembled it — the preimage of the content pin
    /// a D-62 unit is measured against.
    ///
    /// Kept rather than re-derived at the gate, and that is the pin's own rule: an
    /// approved unit authorizes *this content*, so the digest compared against it has
    /// to be the digest of the world the mutation is about to change. A second
    /// assembly would be a second world.
    shape: PlanShape,
}

/// What a mutation resolved to, once its own domain check has passed.
struct Planned {
    price_id: Uuid,
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
    reason_code: String,
    op: Op,
    plan: PlanContext,
}

/// The act an approval unit's subject names, read back out of it.
///
/// The inverse of [`Planned::unit_subject_ref`], and it lives beside it so the two
/// cannot drift: a subject built one way and parsed another is a reviewer shown the
/// wrong act, which is worse than showing none.
///
/// **The act is authenticated by the record, not by the pin.** It rides
/// `subject_ref` on an append-only store and the resolve matches on it, so nothing
/// can move it after an approver signs. Folding it into the content digest would
/// re-freeze a pin for a fact already immutable, and would cost the pin the question
/// it exists to answer — whether the plan's *content* has moved since the reviewer
/// looked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposedAct {
    /// `schedule`, `adjust` or `cancel`.
    pub operation: String,
    /// The window the act is on. `None` for a schedule, whose window does not exist
    /// yet — its id is an outcome of the commit (D-184 clause (2)).
    pub window_id: Option<Uuid>,
    /// The price row a schedule would put a window on. `None` for the other two,
    /// which name a window instead.
    pub price_id: Option<Uuid>,
    /// The start a schedule proposes. `None` for the other two, which move no start.
    pub effective_from: Option<String>,
    /// The end the act proposes; `None` means open-ended.
    pub effective_to: Option<String>,
    /// The end as it stands **before** the act, so a reviewer can see what moves.
    /// `None` for a schedule, and for a window that is already open-ended.
    pub current_effective_to: Option<String>,
}

/// Read a window unit's subject back into the act it names.
///
/// `None` when the subject is not one this builder produced — a corrupt or
/// foreign-shaped ref must leave the reviewer's read answering rather than failing.
#[must_use]
pub fn parse_unit_subject(subject_ref: &str) -> Option<ProposedAct> {
    fn end(token: &str) -> Option<String> {
        (token != "open").then(|| token.to_owned())
    }
    let parts: Vec<&str> = subject_ref.split('/').collect();
    match parts.as_slice() {
        // A schedule names the act's own content, since its window has no id yet
        // (D-184 clause (2)) and no act sequence either.
        [_plan, "schedule", price, from, to] => Some(ProposedAct {
            operation: "schedule".to_owned(),
            window_id: None,
            price_id: Uuid::parse_str(price).ok(),
            effective_from: end(from),
            effective_to: end(to),
            current_effective_to: None,
        }),
        // The other two carry the act sequence they were read at (D-190) between the
        // operation and the ends. It is **skipped** rather than rendered: it is how
        // two occurrences of one transition are told apart, and a reviewer is being
        // shown what moves rather than how many acts preceded it.
        [_plan, window, op, _seq, prior, new] => Some(ProposedAct {
            operation: (*op).to_owned(),
            window_id: Uuid::parse_str(window).ok(),
            price_id: None,
            effective_from: None,
            effective_to: end(new),
            current_effective_to: end(prior),
        }),
        _ => None,
    }
}

/// One end of an interval, in a spelling that round-trips and sorts.
///
/// Shared by the act token and the subject builder so the two cannot spell an
/// instant two ways — a subject spelled differently at open and at resolve is a
/// unit the retry never finds. The spelling is chrono's `to_rfc3339()` (`AutoSi`,
/// `+00:00`), because that is what every window-act subject and request id
/// already in the register was written with; the millisecond `Z` form would
/// spell the same act differently after this migration than before it — the
/// exact miss this comment warns about, across a deploy instead of a retry.
fn interval_end(at: Option<OffsetDateTime>) -> String {
    at.map_or_else(|| "open".to_owned(), format_rfc3339)
}

impl Planned {
    /// This mutation as one token: the operation, and the **transition** it makes
    /// to the end it moves.
    ///
    /// One builder for the two places that have to tell one act on a window from
    /// the next one — the registry handle ([`unit_request_id`]) and the outbox
    /// dedup key. Both used to name the *window* and not the act, and both failed
    /// the same way: the second mutation of a window collided with its own first.
    /// A single token keeps the two from drifting apart, since a fix to one that
    /// missed the other would leave the sequence broken at a different step.
    ///
    /// Stable across a genuine retry of one act — which is what both callers want,
    /// a retry being the thing they exist to make idempotent — and distinct across
    /// two different acts. The residue is stated at [`unit_request_id`].
    /// The subject the approval unit for this act is opened and resolved under.
    ///
    /// **A schedule's subject carries no window id, and that is D-184 clause (2)**
    /// as the owner decided it. The id of a window a `POST` would
    /// create is minted per request, so a subject naming it could not be reproduced
    /// by the call made *after* the approve: the retry named a different subject,
    /// found no unit and opened a second one, which is an approval loop with no
    /// exit. What identifies a schedule is the **act** — the price row it is on and
    /// the interval it proposes — so that is the subject, and the window id becomes
    /// an outcome of the commit rather than an input to the request.
    ///
    /// The other two acts keep the window id, and the asymmetry is the point rather
    /// than an inconsistency: a `PATCH` and a `DELETE` name an existing window in
    /// their path, so their subject is reproducible by construction and naming the
    /// window is what keeps two acts on *different* windows apart.
    fn unit_subject_ref(&self, window_id: Uuid) -> String {
        match self.op {
            // **Both ends, and not `act_token`'s transition.** A window that does not
            // exist yet has no prior end, so the transition would render
            // `schedule/open/open` for every schedule on the row and two proposals
            // with different starts would share one subject — an approval of one
            // authorizing the other, which is the defect this clause exists to close,
            // reintroduced one level down.
            Op::Schedule => format!(
                "{}/schedule/{}/{}/{}",
                self.plan.plan_id,
                self.price_id,
                interval_end(Some(self.effective_from)),
                interval_end(self.effective_to),
            ),
            Op::Adjust { .. } | Op::Cancel => {
                window_unit_ref(self.plan.plan_id, window_id, &self.act_token())
            }
        }
    }

    /// The **act sequence this act was read at** — the window's `mutation_seq` before
    /// it moves, and `0` for a schedule, whose window does not exist yet.
    ///
    /// "Before it moves" is what makes the token stable across a genuine retry: a
    /// refused act wrote nothing, so the retry reads the same number and renders the
    /// same token, which is what lets it find the unit the first attempt opened.
    fn read_at_seq(&self) -> u64 {
        self.plan.current.as_ref().map_or(0, |w| w.mutation_seq)
    }

    fn act_token(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.op.token(),
            self.read_at_seq(),
            interval_end(self.plan.current.as_ref().and_then(|w| w.effective_to)),
            interval_end(self.effective_to),
        )
    }
}

impl WindowService {
    /// The one transaction all three mutations run.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument but the last is a fact only the caller holds - the security \
                  context the registry request needs, the compiled scope, the tenant, the window \
                  id, which of the two ways the path named its subject, and the stamp - and the \
                  last is the closure that is the *only* difference between the three mutations. \
                  The count is deliberately not stated: it read `seven of the eight` over a list \
                  of six. Bundling them would name a request type in `infra` that no surface has, \
                  which is the note `ApprovalService::submit` already carries"
    )]
    async fn mutate<F>(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        window_id: Uuid,
        subject: Subject,
        stamp: AuditStamp,
        verdict_json: VerdictJson,
        plan_mutation: F,
    ) -> Result<WindowMutationOutcome, DomainError>
    where
        F: FnOnce(PlanContext) -> Result<Planned, DomainError> + Send + 'static,
    {
        let ctx = ctx.clone();
        let scope = scope.clone();
        let registry = Arc::clone(&self.registry);
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<WindowMutationOutcome, DomainError, _>(move |txn| {
                Box::pin(async move {
                    mutate_in(
                        txn,
                        &registry,
                        &ctx,
                        &scope,
                        tenant_id,
                        window_id,
                        subject,
                        stamp,
                        verdict_json,
                        plan_mutation,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: window transaction: {infra}"))
            })
        })
    }
}

/// A schedule's plan closure, shared by [`WindowService::schedule`] and
/// [`WindowService::schedule_in`].
///
/// Extracted rather than written twice: the two differ only in who owns the
/// transaction, and two copies of the `Planned` it builds would be two places for a
/// schedule's shape to drift — which is the same argument `plan_repo::create_draft_on`
/// carries for being extracted from `create_draft` rather than copied out of it.
fn schedule_plan(
    price_id: Uuid,
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
    reason_code: String,
) -> impl FnOnce(PlanContext) -> Result<Planned, DomainError> + Send {
    move |plan| {
        Ok(Planned {
            price_id,
            effective_from,
            effective_to,
            reason_code,
            op: Op::Schedule,
            plan,
        })
    }
}

/// The mutation itself, **inside a transaction somebody else owns**.
///
/// Split out of [`WindowService::mutate`] for D-191's `POST` replay, and the split is
/// the whole of that item's work: `infra::idempotent::guarded` runs the caller's
/// mutation *inside the gate's* transaction and has no two-phase form, so the gate and
/// this service each used to insist on owning one. Now the service can be handed a
/// transaction, and the three surfaces wrap it — the `POST` through the gate, the other
/// two through [`WindowService::mutate`], which opens one of its own.
///
/// **The unit a controlled act needs is opened here**, not by the route in a second
/// transaction. That `submit_window_mutation` takes the pin inside its own
/// transaction is true and not the whole story: under the gate the *recorded
/// response body* has to be the body the first caller was actually sent, and on the
/// refused arm that body names the approval. A unit opened after the guarded
/// transaction committed would be absent from the recorded answer, so a replay
/// would hand the second caller something the first never received — the exact
/// failure `infra::idempotent`'s own doc names. It also strengthens the pin: the
/// digest is taken over the world the refusal was decided on rather than over a
/// second read that could have moved in between.
#[allow(
    clippy::too_many_arguments,
    reason = "`WindowService::mutate`'s reason, plus the two things it no longer owns - \
              the transaction and the registry handle. Every one is a fact only the \
              caller holds, and the last is the closure that is the *only* difference \
              between the three mutations"
)]
async fn mutate_in<F>(
    runner: &impl DBRunner,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    window_id: Uuid,
    subject: Subject,
    stamp: AuditStamp,
    verdict_json: VerdictJson,
    plan_mutation: F,
) -> Result<WindowMutationOutcome, DomainError>
where
    F: FnOnce(PlanContext) -> Result<Planned, DomainError> + Send,
{
    let now = stamp.recorded_at;
    // 1. Every fact the validation needs, read inside the
    // transaction that writes: a comparison made before it opened
    // is a hint and not a precondition (D-176).
    let context = read_plan_context(runner, scope, tenant_id, window_id, subject, now).await?;
    let planned = plan_mutation(context)?;
    let unit =
        PlanPublishUnit::window_mutation(planned.plan.plan_id, planned.plan.revision, window_id);

    // 2. `inst-co-single-pending`, **before** anything is touched.
    // A pending unit of any kind holding this key is a review in
    // progress over the interval set this mutation would move, and
    // committing under it would leave a reviewer approving content
    // that had already changed — with no void to catch it, since
    // `inst-ap-pin`'s rail hangs off the *authoring* audit writers
    // and a window mutation is not one of them.
    refuse_pending_key_holder(runner, scope, tenant_id, &planned.plan.key).await?;

    // 3. The two rules `inst-fg-when` puts inside every window
    // mutation, over the key this one touches.
    let after = project(&planned, window_id);
    refuse_interior_gap(&planned.plan.key, &after)?;
    if planned.op.may_remove_coverage() {
        refuse_trailing_void(&planned, &after, now)?;
    }
    // D-04's `inst-co-bounds`, the half §6 names `adjust_effective_to` for. It is
    // evaluated for **every** operation and not only the coverage-removing two:
    // see the function's own doc.
    refuse_horizon_uncovered(&planned, &after, now)?;

    // 3a. The two-person control, **after** every check above and
    // before anything at all is written. The order is the publish
    // route's own: a mutation that cannot commit must not be put in
    // front of a reviewer, who would then approve an act the commit
    // refuses.
    //
    // **All four acts consult the evaluator**, and only the fact each
    // one *is* comes from here (D-176). D-62's two arrive carrying
    // their trigger and are material whatever a threshold says; a
    // schedule and a lengthening arrive carrying none, and what
    // decides them is the per-currency comparison against the
    // tenant's configured policy — read through the **same**
    // transaction that is about to write, so the verdict and the unit
    // it opens are about one policy, and at the **same instant** the
    // act is stamped with, so the verdict and the record cannot land
    // on opposite sides of a policy's `effective_from` (D-188).
    let verdict = materiality::evaluate(
        &ChangeSet::of_window_mutation(
            window_id,
            planned.op.registered_trigger(&planned),
            planned.plan.published.iter().cloned(),
        ),
        crate::infra::threshold::effective_policy_at(runner, scope, tenant_id, now)
            .await?
            .as_ref(),
        // `Some` for every plan this path can reach — `load_current`
        // resolved above, which is the same condition
        // `publish::published_baseline` answers `Some` on. Spelled off
        // the shape rather than assumed, so the two readings of "has
        // this plan published" stay one reading.
        planned
            .plan
            .shape
            .baseline
            .is_some()
            .then(|| PublishedPriceBaseline::of_records(planned.plan.published.iter().cloned()))
            .as_ref(),
    );
    let authorization = if verdict.is_material() {
        let authorized = crate::infra::approval::authorizing_unit(
            runner,
            scope,
            tenant_id,
            &planned.plan.shape,
            &planned.unit_subject_ref(window_id),
        )
        .await?;
        // Nothing is written on the `None` branch, and the tests assert that on all four
        // things a mutation touches: the window plane, the pending-ref table, the outbox
        // and the audit log.
        let Some(record) = authorized else {
            // **The unit, opened here and not by the route.** `mutate_in`'s doc
            // carries the argument; the short form is that the gate records the
            // body it is handed, and on this arm that body names the approval.
            //
            // The id is minted here for the reason the route's comment already
            // gave — "the caller names the window, never the unit" — so it is an
            // outcome of the act rather than an input to the request, exactly as
            // a schedule's window id became one under D-184. Under the gate a
            // replay hands back the first attempt's id, so nothing observable
            // churns.
            let record = crate::infra::approval::ApprovalService::submit_window_mutation_on(
                runner,
                scope,
                tenant_id,
                window_id,
                // The **price row**: a schedule's window does not exist yet, so
                // the window id names nothing readable at this point.
                planned.price_id,
                Uuid::now_v7(),
                verdict_json(&verdict)?,
                stamp,
                &planned.unit_subject_ref(window_id),
            )
            .await?;
            return Ok(WindowMutationOutcome::SubmittedForApproval(Box::new(
                pending_approval(&planned, window_id, verdict, record),
            )));
        };
        Some(record)
    } else {
        None
    };

    // 4. Addressability, fail closed, after re-validation and
    // before the writes (D-156).
    let pending = request_version_now(
        registry.as_ref(),
        ctx,
        &unit_request_id(tenant_id, unit, &planned),
    )
    .await?;

    // 5. The row change. Its own guards run beneath the domain
    // checks above and are not replaced by them.
    let written = write(runner, scope, tenant_id, window_id, &planned, stamp).await?;

    // 6. The plan subject the projector re-freezes.
    // `SubjectRef::Plan` and not a window subject of its own: D-99
    // makes windows plan facts, so a window mutation is a different
    // *reason* to re-project one subject rather than a second
    // subject to project.
    catalog_version_ref_repo::record_pending(
        runner,
        scope,
        PendingVersionRow::for_subject(
            tenant_id,
            pending.pending_ref.clone(),
            &SubjectRef::Plan(planned.plan.plan_id.get()),
            Some(planned.plan.revision),
            Some(planned.plan.lifecycle_state),
            now,
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // 7. The event.
    let payload = PriceWindowTransitionPayload {
        window_id,
        plan_id: planned.plan.plan_id,
        price_id: planned.price_id,
        effective_from: written.effective_from,
        effective_to: written.effective_to,
        correlation_id: stamp.correlation_id,
    };
    outbox_repo::enqueue(
        runner,
        scope,
        NewOutboxEvent::price_window_mutation(
            tenant_id,
            planned.op.event(),
            &payload,
            now,
            &planned.act_token(),
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // 8. The record of who did it — the debt `window_repo`'s module
    // doc once filed as "the store holds who *scheduled* a window and
    // cannot answer who *shortened* it", and now records as paid
    // **here**, naming this site. It is answered by a record
    // rather than by an `updated_by` column, which is what makes it
    // payable without minting: a column no decision names would be
    // this group inventing storage, while `pricing_audit_log` is the
    // store D-135 already keys on the audited subject's aggregate.
    // The one act-discrimination it does not carry is the action
    // token — `Op::AUDIT_ACTION` is `Publish` for all three — so the
    // acts are told apart by the states below and by the unit's
    // subject, which is what `window_repo`'s doc now says too.
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            // The **plan's** segment. D-135 keys a chain on the
            // audited subject's *aggregate*, and a window's
            // aggregate is the plan its row prices — exactly what
            // `AuditSubjectKind::PriceUnit`'s doc says of a price
            // row. So the aggregate list naming
            // plan/overlay/payer/policy/bulk and **not** `window` is
            // correct rather than a gap: `window` is a subject kind,
            // not an aggregate. That list is **S5 §3's
            // `inst-au-tamper`** — the chain-segmentation rule — and
            // not §6's writer contract, which this comment used to
            // cite; §6 declares the record's columns and says nothing
            // about which aggregates segment a chain.
            chain_id: audit_repo::plan_chain(planned.plan.plan_id),
            recorded_at: now,
            actor_principal_id: stamp.actor_principal_id,
            action: Op::AUDIT_ACTION,
            subject_kind: AuditSubjectKind::Window,
            subject_ref: audit_repo::window_ref(planned.plan.plan_id, window_id),
            before_state: planned.plan.current.as_ref().map(window_state_value),
            after_state: Some(window_state_value(&written)),
            // The unit that authorized it, on the two acts D-62
            // controls, and `None` on the two it does not.
            //
            // Not always `None`. Whether a schedule or a lengthening
            // `PATCH` needs a unit is the threshold policy's answer,
            // but `inst-mat-registered` makes a cancel and a
            // shortening always-material independent of any
            // threshold, so their unit does not wait for a policy —
            // it is step 3a above, and this is where it is recorded.
            approval_ref: authorization.as_ref().map(|r| r.approval_id),
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Ok(WindowMutationOutcome::Committed(Box::new(
        WindowMutationReceipt {
            window_id,
            plan_id: planned.plan.plan_id,
            price_id: planned.price_id,
            revision: planned.plan.revision,
            pending_version_ref: pending.pending_ref,
            effective_from: written.effective_from,
            effective_to: written.effective_to,
            state: written.state,
            mutation_seq: written.mutation_seq,
        },
    )))
}

/// The key's interval set **as the mutation would leave it**.
///
/// Built from the stored plane with the subject window's stored form replaced by
/// the form the mutation produces — never by re-reading after the write. That
/// ordering is a rule and not a preference: a check made after the write has to
/// roll back a mutation the store already accepted, and on the cancel path the
/// append-only trigger would refuse the rollback's own `UPDATE`, turning a clean
/// 400 into a 500 about a constraint the caller never touched.
///
/// The subject is dropped **by window id**, which is the only identifier that is
/// unique here: two intervals of one key can share an `effective_from` when one of
/// them is `cancelled`, so matching on the interval would drop the wrong row.
fn project(planned: &Planned, window_id: Uuid) -> KeyWindows {
    let mut intervals: Vec<WindowInterval> = planned
        .plan
        .plane
        .iter()
        .filter(|(id, key, _)| *key == planned.plan.key && *id != window_id)
        .map(|(_, _, interval)| *interval)
        .collect();
    intervals.push(match planned.op {
        Op::Cancel => WindowInterval::new(
            planned.effective_from,
            planned.effective_to,
            WindowState::Cancelled,
        ),
        Op::Schedule | Op::Adjust { .. } => WindowInterval::new(
            planned.effective_from,
            planned.effective_to,
            planned
                .plan
                .current
                .as_ref()
                .map_or(WindowState::Scheduled, |c| c.state),
        ),
    });
    intervals.sort_by_key(|i| (i.effective_from, i.effective_to));
    KeyWindows {
        scope_key: planned.plan.key.clone(),
        intervals,
    }
}

/// `inst-co-single-pending` over the one key this mutation moves.
///
/// **One key and not the plan's set**, which is the same scoping every other rule
/// in this transaction takes: a mutation can only move this key's interval set, so
/// a review in progress over a *sibling* key of the same plan is not a reason to
/// refuse — refusing on it would make every key of a plan hostage to a review of
/// the worst one.
///
/// It is evaluated **before** the registry request and before any write, so the
/// refusal leaves the store exactly as it found it. That ordering is the whole of
/// what "before it touches anything" means here, and it is asserted rather than
/// argued: the refusal tests read the window plane, the outbox and the audit log
/// back afterwards.
///
/// The refusal itself is [`crate::infra::approval::refuse_held_key`]'s and **not a
/// second spelling of it**: this path used to build its own copy of the same sentence,
/// which is one refusal with two authors and one field — the holding unit — for them to
/// drift apart in. What is local here is only the set: one key, because a mutation can
/// move only one.
///
/// # Errors
/// [`DomainError::PendingChangeUnitExists`] naming the holding unit and the key.
async fn refuse_pending_key_holder(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    key: &ScopeKey,
) -> Result<(), DomainError> {
    let keys = std::collections::BTreeSet::from([key.to_string()]);
    crate::infra::approval::refuse_held_key(runner, scope, tenant_id, &keys).await
}

/// The facts the route needs to open the unit a controlled act requires.
///
/// The interval and the state are the **stored** ones, deliberately: this is the
/// answer to a call that changed nothing, so every field of it describes the world as
/// the caller will still find it. `planned.effective_to` — what the caller *asked*
/// for — is not carried, because a 202 that echoed the requested end would read as
/// though the end had moved.
fn pending_approval(
    planned: &Planned,
    window_id: Uuid,
    verdict: MaterialityVerdict,
    approval: ApprovalRecord,
) -> PendingApproval {
    let (effective_from, effective_to, state) = planned.plan.current.as_ref().map_or(
        (
            planned.effective_from,
            planned.effective_to,
            WindowState::Scheduled,
        ),
        |current| (current.effective_from, current.effective_to, current.state),
    );
    PendingApproval {
        window_id,
        plan_id: planned.plan.plan_id,
        price_id: planned.price_id,
        revision: planned.plan.revision,
        effective_from,
        effective_to,
        state,
        // **The evaluator's verdict, carried rather than re-minted.** A constant
        // `material(AlwaysMaterialTrigger)` would be right for a cancel and a
        // shortening, which D-62 needs no threshold for, and wrong for the other two
        // acts, which reach this arm because a *threshold* rule answered — it would
        // tell an operator "alwaysMaterialTrigger" about a currency with no entry,
        // which is the one thing they could have acted on.
        verdict,
        approval,
    }
}

/// The subject of the approval unit a controlled window mutation opens.
///
/// `window_ref` names the window; this names the **act on** it, and the difference
/// is D-62's whole content. A unit subject to `window_ref` alone and pinned to the
/// plan shape *before* the mutation is identical for every act on one window while
/// nothing has committed, so
/// [`crate::infra::approval::authorizing_unit`]'s lookup would answer an approval
/// taken for a different act: a reviewer signs a lengthening and the submitter
/// cancels the window under it, asking nobody.
///
/// Scoping the **subject** rather than widening the content pin, deliberately. The
/// pin is re-derived at read time by `ApprovalService::find` so a reviewer can be
/// shown what they are signing, and it re-derives the plan's shape; folding the act
/// into it would make `content_matches_pin` unanswerable without storing the act
/// beside the record. The subject is stored, is what
/// `find_approved_for_content` already filters on, and is the field that means
/// "the thing being decided".
///
/// This does **not** let two acts on one window pend at once:
/// `ApprovalService::submit_window_mutation` registers the canonical scope key as a
/// held key, and `refuse_held_key` refuses the second submit with
/// `PENDING_CHANGE_UNIT_EXISTS` whatever its subject.
///
/// What it still does not fix is the reviewer's **view**: `re_derive`'s `Window`
/// arm renders the plan as it already stands and names no operation, so a reviewer
/// reads the world the act would leave rather than the act. That is the owed half
/// and it needs a `PinnedSubject` variant.
pub(crate) fn window_unit_ref(plan_id: PlanId, window_id: Uuid, act: &str) -> String {
    format!("{}/{act}", audit_repo::window_ref(plan_id, window_id))
}

/// `inst-fg-detect` over the mutated key, and only over it.
///
/// It fans out through [`DomainError::ValidationFailed`] rather than a bespoke
/// variant because `WINDOW_GAP` is a report code — the same code the publish rule
/// raises, from the same constant, so an operator sees one sentence about one fault
/// whichever surface refused it.
fn refuse_interior_gap(key: &ScopeKey, after: &KeyWindows) -> Result<(), DomainError> {
    let report = coverage::check(std::slice::from_ref(key), std::slice::from_ref(after));
    let Some(entry) = report.find(key) else {
        return Ok(());
    };
    let gaps = entry.interior_gaps();
    if gaps.is_empty() {
        return Ok(());
    }
    let mut violations = ValidationReport::default();
    for (start, end) in gaps {
        violations.violate(
            coverage::WINDOW_GAP,
            key.to_string(),
            format!(
                "[{}, {}) is uncovered between two of the key's windows",
                format_rfc3339(start),
                format_rfc3339(end)
            ),
        );
    }
    Err(DomainError::ValidationFailed(violations))
}

/// `inst-fg-trailing`, fail-closed with **no exemption** (D-182).
///
/// The floor is `max(current coverage end, now + the longest billing cycle sold on
/// that key)` and both halves are load-bearing: the first is why an already
/// open-ended key cannot be bounded by this path at all, the second is why a key
/// whose coverage has nearly run out cannot simply be released.
///
/// **`None` from `longest_cycle_sold` refuses.** It means the market sells
/// recurring and the plan authored no frequency, so the term has *no value* —
/// distinct from `Some(zero)`, which is W6's "a plan with no recurring part needs
/// no forward coverage". Folding "unknown margin" into "no margin" is the direction
/// a fail-closed floor must never round in, and it is the same collapse
/// [`CoverageEnd`] refuses one type over.
///
/// **There is no exemption branch**, and that is D-182 rather than an omission: the
/// D-79 lane has no client, no contract type and no counterpart gear, D-131 makes a
/// lane that cannot answer the subscribers-present case, and a branch nothing can
/// reach is a branch the next group deletes as dead code without learning what it
/// was for.
fn refuse_trailing_void(
    planned: &Planned,
    after: &KeyWindows,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let Some(margin) = planned.plan.margin else {
        return Err(DomainError::WindowTrailingVoid(format!(
            "{}: the plan sells a recurring row on this market and authors no frequency, so \
             {WINDOW_TRAILING_VOID}'s floor - now plus the longest billing cycle sold on the key - \
             has no value, and the coverage this would remove cannot be shown to be free",
            planned.plan.key
        )));
    };
    let Some(horizon) = now.checked_add(margin) else {
        return Err(DomainError::WindowTrailingVoid(format!(
            "{}: now plus the longest billing cycle sold on the key is not a representable \
             instant, so {WINDOW_TRAILING_VOID}'s floor has no value, and the coverage this \
             would remove cannot be shown to be free",
            planned.plan.key
        )));
    };
    let after_end = after.coverage_end();
    match planned.plan.before.coverage_end() {
        // Covered forever today. Any bound at all opens a void with no successor,
        // and the first uncovered instant *is* the bound the caller asked for.
        CoverageEnd::OpenEnded => {
            if matches!(after_end, CoverageEnd::OpenEnded) {
                return Ok(());
            }
            Err(DomainError::WindowTrailingVoid(format!(
                "{}: the key's coverage is open-ended, so bounding it leaves every instant from {} \
                 uncovered with no successor; schedule the successor first",
                planned.plan.key,
                after_end
                    .at()
                    .map_or_else(|| "the key's start".to_owned(), format_rfc3339)
            )))
        }
        before => {
            let until = before.at().map_or(horizon, |at| at.max(horizon));
            let covered = match after_end {
                CoverageEnd::OpenEnded => true,
                CoverageEnd::Ends(at) => at >= until,
                CoverageEnd::Uncovered => false,
            };
            if covered {
                return Ok(());
            }
            Err(DomainError::WindowTrailingVoid(format!(
                "{}: the key must stay covered through {} - the later of its current coverage end \
                 and now plus the longest billing cycle sold on it - and this leaves {} the first \
                 uncovered instant",
                planned.plan.key,
                format_rfc3339(until),
                after_end
                    .at()
                    .map_or_else(|| "every instant".to_owned(), format_rfc3339)
            )))
        }
    }
}

/// `inst-co-bounds` (D-04) — a grandfathered generation's coverage must reach
/// through **`grandfatherUntil` + the longest billing cycle sold on the key**,
/// and be open-ended when the horizon is null.
///
/// The margin exists because re-bind happens only at the **next renewal** after
/// the horizon: a bound period that started before `grandfatherUntil` keeps
/// rating against the generation's key until that renewal, so a window ending at
/// the horizon strands subscribers for up to one full cycle. Nothing leaks —
/// new subscriptions never bind a grandfathered row.
///
/// # It was enforced nowhere, and both accounts of that were out of date
///
/// D-04 says the bound is enforced *"at cutover and on every `effectiveTo`
/// adjustment"*. The cutover half holds **by construction** (D-204 clause 4):
/// `compose_cutover_windows` schedules the copy's window open-ended, and an open
/// interval covers every instant the margin could name. The adjustment half was
/// unbuilt, and `window_repo`'s two doc comments recorded the reason as *"the
/// rule's second input has no producer anywhere in this crate … no function, no
/// column, no type"*. That was true when it was written and is not now:
/// [`coverage::longest_cycle_sold`] is W6's term, built for D-80's horizon and
/// `inst-fg-trailing`'s floor, and this transaction already reads it —
/// `PlanContext::margin`.
///
/// # Why every operation and not the two that remove coverage
///
/// [`refuse_trailing_void`] runs on a cancel or a `PATCH` because its floor is
/// `max(current coverage end, now + margin)`, which no addition can cross. This
/// floor is different: it is anchored on the **row's** horizon, not on the key's
/// current coverage, so a key can be *authored* below it. The reachable shape is
/// a **schedule** — the first bounded window on a grandfathered key whose row
/// carries a horizon — and an extension that moves the end without reaching the
/// floor is the second. Neither is a coverage removal, and both leave subscribers
/// uncovered inside their own bound period. So this is asked of the interval set
/// the mutation *produces*, whichever of [`mutate_in`]'s three acts produced it.
///
/// **Its reach is those three acts and not every window mutation in the crate.**
/// `infra::cutover`, `infra::supersession`, `infra::retirement` and the activation
/// sweep all write through `window_repo` without passing here;
/// [`window_repo`](crate::infra::storage::repo::window_repo)'s module roster names
/// each and says which are argued shut and which carries a bound of its own.
/// Retirement is the second kind: its cancellations are judged by
/// [`strand_free_disposition`](crate::domain::retirement::strand_free_disposition),
/// which asks this same span through the same walk and answers by **keeping** the
/// window rather than refusing the act — the two surfaces differ in what the act
/// *is*, not in what the bound says.
///
/// # The refusal reuses `WINDOW_TRAILING_VOID`, and that is a reading
///
/// §5 declares no code for `inst-co-bounds`; D-04's own step glosses the refusal
/// as *"`WINDOW_HISTORICAL_IMMUTABLE`/`CUTOVER_GAP` semantics"*, and neither fits
/// a schedule or an extension. `WINDOW_TRAILING_VOID` is declared as *"names the
/// key and the first uncovered instant"* — this refusal's exact shape — and D-80
/// records the three margins (its horizon, this bound, `inst-fg-trailing`'s
/// floor) as **one** margin under three anchors. Minting a fifth window code
/// would be this crate declaring a wire fact the design set does not, which
/// D-204 clause (2) explicitly refuses to do. The naming question is reported as
/// owed rather than answered here.
///
/// # It is a span and not a last instant, and that took a second reading
///
/// The bound was first built as one comparison — `coverage_end >= floor` — which
/// answers only where coverage **stops**. An interval satisfies it whatever
/// instant it *starts* at, so `[grandfatherUntil + 60d, floor + 365d)` passed:
/// coverage ran a year past the floor and the generation held no window at all
/// for the two months before it. Every neighbouring guard is silent on that
/// shape, each for a reason it states itself. [`refuse_interior_gap`] walks each
/// window's exclusive end to its successor's start, so a **leading** void has no
/// predecessor to open it from. [`refuse_trailing_void`] runs only on the two
/// coverage-removing acts, and a schedule is not one. And this rule read the far
/// end. The identical hole was in the null-horizon arm, where
/// [`CoverageEnd::OpenEnded`] is likewise a property of one interval's end.
///
/// So the question is asked of the whole span the generation is answerable for,
/// `[max(cohort, now), floor)`, and the refusal names the first instant of it
/// that nothing covers.
///
/// **The lower anchor is the cohort**, because that axis *is* the cutover instant
/// (ADR-0002) and a generation cannot strand anybody before it exists — the copy's
/// window is scheduled *at* the cutover instant, so a freshly composed cutover
/// whose instant is still a few minutes out is covered from its own first instant
/// and not from the operator's clock. **`now` raises it**, because a void strictly
/// in the past is not repairable by any window mutation — no interval can be
/// authored backwards over it — and a floor anchored on it alone would freeze
/// such a key against every further change, including the ones repairing its
/// future.
///
/// # What this does **not** cover
///
/// A grandfathered row that is still a **draft** carries no published generation
/// on the key, so there is nothing here to judge; the publish pipeline's coverage
/// rules own that world and do not check this bound either. Reported.
///
/// # Two callers, and the second one moves the operand this one reads
///
/// The span this rule asks about is `[max(cohort, now), grandfatherUntil + margin)`,
/// so its **right-hand end is the horizon** — the very column
/// [`crate::infra::grandfather`]'s door writes. That door therefore cannot ask this
/// question through this function, whose horizon is the *stored* one: it would
/// judge the state the act is about to leave behind. It calls
/// [`refuse_horizon_span_uncovered`] with the horizon it is **proposing**, over the
/// key's unmoved interval set, which is the same rule asked of the post-state.
/// Splitting the two here rather than there is what keeps one walk, one anchor and
/// one message for both.
fn refuse_horizon_uncovered(
    planned: &Planned,
    after: &KeyWindows,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    if planned.plan.key.price_eligibility() != PriceEligibility::ExistingGrandfathered {
        return Ok(());
    }
    // The generation on **this** key, not the plan's grandfathered rows at large:
    // the cohort axis makes every generation its own key (ADR-0002), and each
    // carries its own horizon.
    let Some(generation) = planned
        .plan
        .published
        .iter()
        .find(|row| row.scope_key == planned.plan.key)
    else {
        return Ok(());
    };
    refuse_horizon_span_uncovered(
        &planned.plan.key,
        generation.grandfather_until,
        planned.plan.margin,
        after,
        now,
    )
}

/// [`refuse_horizon_uncovered`]'s rule over an **explicit** horizon.
///
/// The whole of D-04's span check with the one operand the caller supplies rather
/// than reads: a window mutation passes the generation's stored
/// `grandfatherUntil`, and the horizon door passes the one it is about to write.
/// Everything else — the anchor, the null arm, the missing-margin arm, the walk,
/// the far-end comparison and the message — is one body, because two spellings of
/// a money bound is how the two ends of it came to disagree in the first place
/// (D-316 problem 2).
///
/// The caller is responsible for the class test: a key that is not
/// `existing_grandfathered` carries no horizon at all (D-147), and the two callers
/// answer that differently — a window mutation returns `Ok` on such a key, while
/// the horizon door refuses the request with `GRANDFATHER_UNTIL_FORBIDDEN`.
///
/// # Errors
/// [`DomainError::WindowTrailingVoid`] naming the key and the first uncovered
/// instant, per D-316 clause (4) — or, on the two arms where the floor has no
/// value at all (W6's term absent, or `grandfatherUntil + margin` not a
/// representable instant), naming the missing operand instead of an instant no
/// walk could reach.
pub(crate) fn refuse_horizon_span_uncovered(
    key: &ScopeKey,
    horizon: Option<OffsetDateTime>,
    margin: Option<time::Duration>,
    after: &KeyWindows,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let after_end = after.coverage_end();
    // The generation's own first instant, never earlier than the clock this
    // mutation is stamped with — see the doc's anchor paragraph for both halves.
    // `map_or` rather than an `expect`: the cohort/eligibility biconditional
    // (`check_cohort_eligibility`) makes the `None` unreachable on this arm, and
    // a key that reached it anyway is answered on `now` rather than on a panic.
    let anchor = key
        .cohort()
        .generation()
        .map_or(now, |cutover| cutover.max(now));

    let Some(horizon) = horizon else {
        // D-04's parenthesis — *"open-ended when null"*. D-147: a grandfathered
        // row with a null `grandfatherUntil` is **indefinite**, so no finite end
        // can satisfy a bound that never arrives. Distinct from the margin case
        // below and refused on its own words, because "no horizon" read as "a
        // horizon of now" is the direction a money bound must never round in.
        //
        // `None` for the far end is that "never arrives" spelled as the walk's
        // own argument: nothing short of an unbroken run to an open interval
        // answers it, which subsumes a bare `OpenEnded` test and additionally sees
        // the interval that opens late.
        let Some(uncovered_at) = after.first_uncovered_from(anchor, None) else {
            return Ok(());
        };
        return Err(DomainError::WindowTrailingVoid(format!(
            "{key}: the generation's grandfatherUntil is null, so its eligibility is indefinite \
             and its coverage must run unbroken from {} and never end; this leaves {} the first \
             uncovered instant",
            format_rfc3339(anchor),
            format_rfc3339(uncovered_at)
        )));
    };

    // `None` refuses, for `refuse_trailing_void`'s reason at full strength: it
    // means the market sells recurring and the plan authored no frequency, so
    // W6's term has *no value* — which is not `Some(zero)`, W6's "a plan with no
    // recurring part needs no forward coverage".
    //
    // **This arm is why the horizon door has to ask the question at all.** Under a
    // null horizon the margin is never consulted, so a generation whose plan
    // authors no frequency passes the null arm and would fail here the instant a
    // horizon is set: `inst-gs-bound` is the one act that can move a key from the
    // arm that needs no margin to the arm that requires one. Every other move the
    // door makes — a finite horizon tightened further — only shrinks the span.
    let Some(margin) = margin else {
        return Err(DomainError::WindowTrailingVoid(format!(
            "{key}: the plan sells a recurring row on this market and authors no frequency, so \
             D-04's bound - grandfatherUntil plus the longest billing cycle sold on the key - has \
             no value, and this generation's coverage cannot be shown to reach it"
        )));
    };

    // **The floor is an addition, and this one is on request input.** The horizon
    // door passes the instant off the `PUT.../grandfather-until` body with serde
    // and `domain::grandfather::check_tightening` between, and that edge admits
    // *every* proposed instant on an indefinite generation, since each of them is
    // earlier than never. The only range-adjacent check on the field —
    // `price_repo::tighten_grandfather_until`'s `check_authored_instant` — is
    // millisecond **precision** rather than range, and runs three steps later, on
    // the write this rule stands in front of. So `DateTime::<Utc>::MAX_UTC`, and
    // every millisecond-quantised instant near it, reaches here. chrono's
    // `impl Add<time::Duration> for DateTime<Tz>` is
    // `checked_add_signed(rhs).expect("… overflowed")`, which aborts a **release**
    // build too, so this was a panic on an authenticated request path.
    //
    // It refuses on the missing-margin arm's own words, because it is the same
    // fact: the floor has no value, so no interval set can be *shown* to reach it.
    // `domain::sellability::active_window_with_horizon` does the identical add
    // fallibly and refuses in the identical direction — even under open-ended
    // coverage, which would otherwise satisfy every representable floor, because
    // fail-closed is the direction a money bound rounds in.
    // `domain::retirement::GenerationCoverage::span` folds the same overflow into
    // `BoundSpan::Incomputable`, which its caller reads as *keep*: that is the two
    // surfaces' standing disagreement over an absent operand — this one refuses the
    // act, that one refuses to strand a generation for it — and not a third reading.
    let Some(floor) = horizon.checked_add(margin) else {
        return Err(DomainError::WindowTrailingVoid(format!(
            "{key}: this generation's grandfatherUntil {} plus the longest billing cycle sold on \
             the key is not a representable instant, so D-04's bound has no value and this \
             generation's coverage cannot be shown to reach it",
            format_rfc3339(horizon)
        )));
    };

    // Two questions, and the second does not subsume the first. The walk starts at
    // `anchor`, so on a key whose whole bound is already in the past it has nothing
    // to report and would accept coverage that never reached the floor - which is
    // the reading this rule landed with and must not lose. The walk is what sees a
    // void that opens at or after the anchor, wherever in the span it falls.
    let reaches_floor = match after_end {
        CoverageEnd::OpenEnded => true,
        CoverageEnd::Ends(at) => at >= floor,
        CoverageEnd::Uncovered => false,
    };
    let uncovered_at = after.first_uncovered_from(anchor, Some(floor));
    if reaches_floor && uncovered_at.is_none() {
        return Ok(());
    }
    Err(DomainError::WindowTrailingVoid(format!(
        "{key}: this generation is grandfathered until {} and its coverage must run unbroken from \
         {} through {} - that horizon plus the longest billing cycle sold on the key - so that \
         every bound period stays rateable until its renewal re-bind; this leaves {} the first \
         uncovered instant",
        format_rfc3339(horizon),
        format_rfc3339(anchor),
        format_rfc3339(floor),
        uncovered_at.map_or_else(
            || after_end
                .at()
                .map_or_else(|| "every instant".to_owned(), format_rfc3339),
            format_rfc3339
        )
    )))
}

/// Read every fact the validation needs, inside the writing transaction.
async fn read_plan_context(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    window_id: Uuid,
    subject: Subject,
    now: OffsetDateTime,
) -> Result<PlanContext, DomainError> {
    // The key first, because it carries the plan and everything else is
    // plan-scoped. Which read answers it depends on what the surface addressed.
    let (current, key) = match subject {
        Subject::NewOn { price_id } => {
            let key = price_repo::load_scope_key(runner, scope, tenant_id, price_id)
                .await
                .map_err(|e| repo_failure(&e))?
                .ok_or_else(|| DomainError::NotFound {
                    subject: "price row".to_owned(),
                    id: price_id.to_string(),
                })?;
            (None, key)
        }
        Subject::Existing => {
            let record = window_repo::find(runner, scope, tenant_id, window_id)
                .await
                .map_err(|e| repo_failure(&e))?
                .ok_or_else(|| DomainError::NotFound {
                    subject: "price window".to_owned(),
                    id: window_id.to_string(),
                })?;
            let key = record.scope_key.clone();
            (Some(record), key)
        }
    };
    let plan_id = key.plan_id();
    // The **current** revision, for `PlanPublishUnit::window_mutation`'s reason,
    // and its lifecycle state off the same record.
    let revision = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "current plan revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    let lifecycle_state = revision.lifecycle_state;
    let shape = assemble_from(runner, scope, tenant_id, plan_id, revision, now).await?;
    let plane: Vec<(Uuid, ScopeKey, WindowInterval)> =
        window_repo::list_for_plan(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .into_iter()
            .map(|w| {
                (
                    w.window_id,
                    w.scope_key,
                    WindowInterval::new(w.effective_from, w.effective_to, w.state),
                )
            })
            .collect();
    let before = KeyWindows {
        scope_key: key.clone(),
        intervals: plane
            .iter()
            .filter(|(_, k, _)| *k == key)
            .map(|(_, _, i)| *i)
            .collect(),
    };
    let margin = coverage::longest_cycle_sold(&shape, key.currency(), key.region());
    let published = price_repo::load_for_plan(
        runner,
        scope,
        tenant_id,
        plan_id,
        &[LifecycleState::Published],
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(PlanContext {
        plan_id,
        revision: shape.revision,
        lifecycle_state,
        current,
        key,
        before,
        margin,
        plane,
        published,
        shape,
    })
}

/// Perform the row change the operation names.
async fn write(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    window_id: Uuid,
    planned: &Planned,
    stamp: AuditStamp,
) -> Result<WindowRecord, DomainError> {
    match planned.op {
        Op::Schedule => window_repo::schedule(
            runner,
            scope,
            NewWindow {
                window_id,
                tenant_id,
                price_id: planned.price_id,
                effective_from: planned.effective_from,
                effective_to: planned.effective_to,
                reason_code: planned.reason_code.clone(),
            },
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e)),
        Op::Adjust { expected_seq } => window_repo::adjust_effective_to(
            runner,
            scope,
            tenant_id,
            window_id,
            planned.effective_to,
            expected_seq,
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e)),
        Op::Cancel => window_repo::transition(
            runner,
            scope,
            tenant_id,
            window_id,
            WindowState::Cancelled,
            stamp.recorded_at,
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e)),
    }
}

/// The registry's idempotency handle for a window mutation.
///
/// The same shape `publish_request_id` builds, and it has to be: the token is the
/// registry's own idempotency key, so a retried mutation re-requests the *same*
/// handle rather than stranding the first.
///
/// **It has to identify the act, and it used to identify only the window.** The
/// handle was `(kind, tenant, plan, revision)`, and `PublishUnitKind::request_token`
/// names the window id — but a window mutation deliberately freezes the plan's
/// *current* revision and never advances it, so a `POST` and every later
/// `PATCH`/`DELETE` on that window rendered one string. The registry is
/// contractually idempotent on `request_id`, so the second mutation was handed the
/// pending ref the first had already recorded, and `record_pending`'s plain
/// `INSERT` met `PRIMARY KEY (tenant_id, pending_ref)`: **a window this gear
/// scheduled through its own `POST` could never be adjusted or cancelled through
/// the routes**, and the D-62 path ended in a 500 on the approved retry.
///
/// Answering with the *same* handle would have been the worse repair. Two acts
/// sharing one pending ref share one `CatalogVersion`, so the second act's
/// re-projection would ride a version assigned to the first — a mutation that
/// commits locally and never becomes consumer-visible. So the act is named
/// instead: the operation, and the **transition** it makes to the end it moves.
///
/// The transition and not merely the produced end, so that an oscillation
/// (`A → B`, then `B → A`) renders two handles rather than one. What this still
/// does not distinguish is a *repeated* oscillation — `A → B` twice with a
/// `B → A` between — because the window row carries no version or mutation
/// counter to name. That residue is recorded rather than hidden: it needs a
/// schema column, which is out of this fix's remit.
///
/// Re-spelling this string strands any pending ref the old spelling produced. That
/// is the intent here — the old spelling was the defect — and it is the same
/// re-freeze question `PIN_DOMAIN_SEP` carries one plane over.
fn unit_request_id(tenant_id: Uuid, unit: PlanPublishUnit, planned: &Planned) -> String {
    format!(
        "{}/{tenant_id}/{}/{}/{}",
        unit.kind.request_token(),
        unit.plan_id,
        unit.revision,
        planned.act_token(),
    )
}

/// One window's audited state — the interval and where it stands in §4's machine.
///
/// The **content**, not a digest of it. The standing heuristic is that a test
/// which rebuilds its expectation from the data it checks cannot see that data
/// replaced, and the corollary for a writer is that the record has to carry
/// something an independent expectation can be written against.
fn window_state_value(record: &WindowRecord) -> JsonValue {
    serde_json::json!({
        "state": record.state.as_str(),
        "effectiveFrom": format_rfc3339(record.effective_from),
        "effectiveTo": record.effective_to.map(format_rfc3339),
        "priceId": record.price_id,
    })
}

/// One **shortened** window's audited interval, as an act whose subject is not the
/// window records it.
///
/// # Why this exists beside [`window_state_value`] rather than being it
///
/// The two answer to different readers. `window_state_value` belongs to a record
/// whose `subject_ref` is [`audit_repo::window_ref`] — plan and **window id** — so a
/// reader already holds the window's durable name and what the state adds is the row
/// it prices. The cutover's and the supersession's records name an **act**
/// (`cutover_unit_ref`, `supersession_unit_ref`): three price ids and an instant, and
/// no window id anywhere, because a window id is not a component of either act's
/// identity. A reader of one of those records who wanted the shortened window would
/// have nothing to look it up by.
///
/// So the identity carried here is `(scopeKey, effectiveFrom)`, and both halves are
/// chosen rather than convenient:
///
/// * **`scopeKey`** is what the frozen delta keys a window plane by
///   ([`crate::domain::window::WindowInterval`] deliberately carries no `window_id`,
///   and that doc argues why it must not start), so a record keyed this way is
///   comparable against `read_model_repo::delta_at` without either side learning the
///   other's identifier. `tests/sqlite_cutover_unwind.rs` asserts they agree.
/// * **`effectiveFrom`** can never move. The append-only trigger
///   (`pricing_price_window`, arm 4) refuses any `UPDATE` that changes it — *"only state,
///   `effective_to` and the flip timestamps may move"* — so it names one interval of
///   the key for as long as the row exists. `effectiveTo`, the operand this record
///   exists to preserve, is precisely the column that does move, which is why it
///   cannot also be part of the name.
///
/// # It is a value, and that is the whole point (D-327)
///
/// A digest here would satisfy the appearance of a before-image and none of its use.
/// D-05 requires a retirement to restore the predecessor's `effectiveTo` *"to its
/// recorded pre-cutover value"*, and `grandfather::horizon_state_value` states the
/// same reason one plane over: a trail carrying a hash *"could not answer what the
/// horizon was, which is the whole operand D-327 found missing on the window plane."*
/// This is that operand.
///
/// `state` is in it although a shorten never moves it. It is what makes the pair
/// self-describing — a reader finding `before_state` and `after_state` differing in
/// exactly one field can see that this act moved an end and nothing else, rather
/// than having to know it — which is `horizon_state_value`'s argument for carrying
/// `lifecycleState` verbatim.
pub(crate) fn shortened_window_state_value(record: &WindowRecord) -> JsonValue {
    serde_json::json!({
        "scopeKey": record.scope_key.to_string(),
        "effectiveFrom": format_rfc3339(record.effective_from),
        "effectiveTo": record.effective_to.map(format_rfc3339),
        "state": record.state.as_str(),
    })
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod window_tests;
