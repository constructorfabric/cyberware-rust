//! The publish engine's service half: assembling the subject the rules judge,
//! §4.2 step 2 — the pre-check — and §4.2 step 4, the commit.
//!
//! The two share an assembler and a rule set on purpose. §4.2 runs the set
//! **twice**, and the second run is not a formality: approval approves
//! *content*, the commit re-validates *state*, and a commit-time failure voids
//! the approval and returns the subject to draft with the report. Two
//! assemblers or two rule sets would make the two runs incomparable, which is
//! the one thing that clause cannot survive.
//!
//! It lives in `infra` rather than in `domain` because it holds an
//! `AccessScope` and repositories, which dylint DE0301 forbids inside
//! `src/domain/**`. The sibling ledger puts `infra::posting::service` here for
//! the same reason. The publish **vocabulary** and the rules stay pure, in
//! [`crate::domain::publish`].
//!
//! # Assembling the subject: the one predicate that matters
//!
//! [`PlanShape`] is what the rules judge, and building it from five tables is
//! the half of §4.2 step 2 nobody had written. The consequential line is
//! **which rows are in it**.
//!
//! `inst-ph-coverage`, `inst-cs-hybrid`, `inst-cs-usage` and
//! `USAGE_MARKET_INCOMPLETE` all range over the plan's *published* rows —
//! meaning the shape **as it will be after this commit**. So the candidate set
//! is the plan's currently-`published` rows **plus** the `draft` rows this unit
//! publishes. Neither half alone is right:
//!
//! - **draft rows alone** would fail every completeness rule on a revision that
//!   changes only a descriptor or a phase duration, since such a revision
//!   authors no price rows at all and the plan would read as having none;
//! - **published rows alone** would let a newly authored market go unjudged,
//!   which is exactly the "sold but unrateable" state D-15/D-17 declare
//!   impossible.
//!
//! `superseded` and `abandoned` rows are **out**. A superseded row has had its
//! key taken by a successor and is history; an `abandoned` state does not occur
//! on a price row at all (it is a plan-revision state — §4.3,
//! `inst-ps-nodelete`), and naming it here would be a second reading of the
//! price row's state set.
//!
//! # The baseline, and why `None` is a correct answer
//!
//! [`PublishedBaseline`] exists for the three rules that are *comparisons*
//! against what is already published — `TerminalPhaseStable` (D-64) and
//! `AvailableFromNotBackdated`. On a **first** publish there is no current
//! revision, the baseline is `None`, and those rules decline to judge. That is
//! the `inst-cs-declared` posture (D-149): a rule with no referent has only two
//! behaviours available and both are wrong — pass vacuously, which is the guard
//! silently absent, or treat every value as changed, which blocks every first
//! publish outright.
//!
//! # Two assembly decisions the design set leaves implicit
//!
//! Both are decided here, argued here, and **reported** as divergences:
//!
//! 1. The candidate row set above. No section states it; four instructions
//!    presuppose it.
//! 2. `phase_ids_in_use` on the baseline is taken from the plan's currently
//!    **published** price rows, not from the candidate set. `PHASE_IN_USE`
//!    (D-64) refuses a revision that drops a phase "still referenced by a
//!    current published price row", so the referent is the published plane
//!    alone — reading it off the candidate set would let a draft row that this
//!    same revision authored keep a phase alive that nothing published uses.
//!
//! # The pre-check mutates nothing
//!
//! That is what makes it a pre-check, and it is what `POST …/plans/{planId}/publish`
//! calls before routing to approval — the route is declared at
//! [`crate::api::rest::publish::PLAN_PUBLISH`] and merged into the router, and it
//! reaches both this function and [`PublishService::commit`]. A non-empty report
//! comes back as a **report**; turning it into `DomainError::ValidationFailed` is
//! the surface's choice, because the submit path wants to *show* a report while
//! the commit path wants to *fail* on one.

use std::sync::Arc;

use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditSubjectKind, subject_state};
use crate::domain::concurrency::RowVersion;
use crate::domain::error::DomainError;
use crate::domain::instant::from_unix_millis;
use crate::domain::instant::timestamp_millis;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::plan::PlanRevision;
use crate::domain::plan_shape::{PhaseGraph, PlanShape, PublishedBaseline};
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::price_record::PriceRecord;
use crate::domain::publish::rules::{PublishRuleParams, SoftSizeCaps, run_publish_rules};
use crate::domain::publish::{PlanPublishUnit, PublishAuthorization, PublishReceipt};
use crate::domain::read_model::SubjectRef;
use crate::domain::scope_key::{PhaseId, PlanId};
use crate::domain::tax_display::{RegionReadiness, RegionTaxReadiness};
use crate::domain::validation::ValidationReport;
use crate::domain::window::{self, KeyWindows, WindowInterval};
use crate::infra::fixture_gate::{FixtureGate, Reservation};
use crate::infra::registry_deadline::request_version_now;
use crate::infra::storage::repo::{
    NewAuditEntry, NewOutboxEvent, PendingVersionRow, PlanPublishedPayload, PolicyObjectRepo,
    approval_repo, audit_repo, catalog_version_ref_repo, outbox_repo, plan_repo, plan_shape_repo,
    price_repo, taxonomy_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// The lifecycle states a publish subject's candidate row set is drawn from.
///
/// Written down once, here, because it is the single most consequential line in
/// the assembler — see the module doc. `superseded` and `abandoned` are absent
/// deliberately and their absence is the rule.
///
/// `pub(crate)` for one reader: `api::rest::windows`' coverage report ranges over
/// the same set, because a remediation surface that answered about a different row
/// set from the check whose failure sent the operator there would tell them to fix
/// something else.
pub(crate) const CANDIDATE_ROW_STATES: &[LifecycleState] =
    &[LifecycleState::Published, LifecycleState::Draft];

/// The publish engine, as the surfaces and the commit path see it.
///
/// Holds the repositories and the joint-conformance gate, built from what
/// [`PricingRuntime`](crate::module) already carries. The `DBProvider` is here
/// because the pre-check reads through a plain connection while the commit
/// reads through its own transaction, and both go through the same assembler.
#[derive(Clone)]
pub struct PublishService {
    db: DBProvider<DbError>,
    policies: PolicyObjectRepo,
    fixture_gate: FixtureGate,
    /// The sole incrementer of `CatalogVersion`, resolved at init with the
    /// fail-closed default.
    ///
    /// The engine is the only place a version is **requested** from, and that
    /// is the invariant this field carries: two requesters would mean two
    /// publishes believing they own one assignment. It is deliberately narrower
    /// than "one holder": the read-model warm sweep holds the same `Arc` and calls
    /// only `committed_version`, asking what a handle **this** engine already
    /// obtained resolved to. One requester, two readers.
    registry: Arc<dyn CatalogVersionRegistryV1>,
    /// What this path reports about itself (`T-17`); a no-op unless the
    /// gear lifecycle attached one.
    metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
}

impl PublishService {
    /// Build the engine over one database provider and the loaded gate.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        limits: &crate::config::LimitsConfig,
        gate: FixtureGate,
        registry: Arc<dyn CatalogVersionRegistryV1>,
    ) -> Self {
        let policies = PolicyObjectRepo::new(db.clone(), limits);
        Self {
            db,
            policies,
            fixture_gate: gate,
            registry,
            // The safe default: a service built without one reports nothing
            // rather than failing to build. `with_metrics` is what production
            // hands it.
            metrics: Arc::new(crate::domain::ports::metrics::NoopPricingMetrics),
        }
    }

    /// The tenant policy reader, for [`rule_params`]'s second caller.
    ///
    /// `pub(crate)` for [`crate::infra::repricing::run_apply_lane`], which builds
    /// the worker that calls [`crate::infra::repricing::apply_run_in`]: the
    /// mass-repricing apply's aggregate pass is `rule_params` plus
    /// `run_publish_rules`, run over a shape it assembles itself rather than
    /// one this service built — but the policy reader those parameters resolve
    /// against is the tenant's one authoring policy, and reusing the instance
    /// this engine already holds is what keeps that one policy read from a
    /// second, independently-constructed reader answering it a different way
    /// after an operator raises a cap between the two calls.
    #[must_use]
    pub(crate) const fn policies(&self) -> &PolicyObjectRepo {
        &self.policies
    }

    /// The registry handle, for a second requester that does not go through
    /// [`Self::commit`].
    ///
    /// `pub(crate)` for [`crate::infra::repricing::run_apply_lane`]: D-134's
    /// per-plan `CatalogVersion` request is the apply's own, made directly
    /// against the registry rather than through this service's `commit` (which
    /// assembles an open **draft** revision a repricing run does not have) —
    /// see `api::rest::state`'s module doc for why sharing the one `Arc` rather
    /// than minting a second is what keeps two requesters one incrementer.
    #[must_use]
    pub(crate) fn registry(&self) -> &Arc<dyn CatalogVersionRegistryV1> {
        &self.registry
    }

    /// Attach the metrics port (`T-17`).
    ///
    /// A **second call** rather than a fifth parameter on [`Self::new`], for
    /// `PublishRuleParams::with_referencing_markets`' reason: every existing
    /// caller has nothing to say to it, and the no-op [`Self::new`] already
    /// installs is exactly what they mean.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    /// §4.2 step 2 — validate a plan's open draft revision without touching it.
    ///
    /// Resolves the tenant's authoring policy, assembles the publish subject,
    /// runs the aggregate rule set, then asks the joint-conformance gate. It
    /// **mutates nothing**.
    ///
    /// The gate is asked **after** the rules and its refusal supersedes the
    /// report, which is a decision worth stating: a `FIXTURE_MISSING` is not an
    /// item on the author's remediation list at all — no edit to the plan
    /// changes the gate's answer, because the gate is about what this
    /// deployment's joint corpus has agreed to evaluate. So when both hold, the
    /// caller is told the one it can act on, and acting on it means waiting for
    /// a fixture rather than editing a row.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the plan has no open draft revision —
    /// there is nothing to pre-check, and answering with an empty report would
    /// say a plan that cannot publish is publishable.
    /// [`DomainError::FixtureMissing`] when a candidate row's shape has no green
    /// joint conformance fixture.
    /// [`DomainError::Internal`] on a storage failure, or when the plan's
    /// current revision holds no unique terminal phase.
    pub async fn precheck(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        now: OffsetDateTime,
    ) -> Result<ValidationReport, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("bss-pricing: publish precheck: {e}")))?;
        // The shape is assembled **first**, because `inst-cb-addon`'s domain is
        // the add-on set this revision composes — the parameters are now a
        // function of the subject, not only of the tenant.
        let shape = assemble(&conn, scope, tenant_id, plan_id, now).await?;
        // D-332: the pre-check must judge the plan the way the commit will, or a
        // plan the commit would publish is refused before a unit ever opens.
        let params = rule_params(&self.policies, &conn, scope, tenant_id, &shape)
            .await?
            .opening_initial_coverage();
        let report = run_publish_rules(&shape, &params);
        crate::infra::metrics::report_market_metrics(&*self.metrics, &shape, &params);
        check_fixtures(&self.fixture_gate, &shape)?;
        Ok(report)
    }

    /// §4.2 step 4 — the publish commit. One transaction, or none of it.
    ///
    /// The signature is the contract: no publish without a unit, a version to
    /// swap on, an authorization, and an actor for the trail.
    ///
    /// # The sequence, and why each move sits where it does
    ///
    /// 1. **Re-assemble the subject and re-run the rules and the gate, inside
    ///    the transaction.** This is the clause the whole design turns on.
    ///    Approval approved *content*; the commit re-validates *state*, because
    ///    the world moved between the two — a referenced phase retired, a
    ///    market's rows changed, a cap lowered. A non-empty report returns
    ///    [`DomainError::ValidationFailed`] and the transaction rolls back, so
    ///    the subject is exactly where it was: still `draft`, no ref, no event,
    ///    no audit row. §4.2's "voids the approval and returns the subject to
    ///    draft" has two halves, and only one of them is here: the return to
    ///    draft **is** the rollback, since the subject never left `draft`. The
    ///    other half — voiding the approval **record** — is step 7's, through
    ///    `approval_repo::void_pending_for_subject`, and runs inside the same
    ///    transaction as the writes.
    /// 2. **Request the `CatalogVersion`, fail closed.** After the
    ///    re-validation passes and before the writes. Earlier, a commit-time
    ///    validation failure would leave a pending ref at the registry that
    ///    nothing ever commits, tripping
    ///    `pricing.catalogversion.commit_overdue` for a publish that never
    ///    happened; later, the outbox payload would have nothing to carry.
    ///    The cost is real and named: a network round-trip inside an open
    ///    database transaction. It is bounded by the registry's own idempotency
    ///    — `request_id` is documented as exactly that — so
    ///    [`publish_request_id`] derives it from `(tenant, plan, revision)` and
    ///    a rolled-back-then-retried commit re-requests the **same** handle
    ///    instead of orphaning one.
    /// 3. **Flip the row set** — the revision, then its price rows.
    /// 4. **Record the pending ref**, with the subject, the revision **and the
    ///    lifecycle state** this commit judged — the three facts the projector
    ///    needs and cannot re-derive later without reading a world that moved.
    /// 5. **Enqueue `PlanPublished`**, carrying the pending handle.
    /// 6. **Extend the audit chain.** Last among the writes, because the record
    ///    describes what the transaction did and every fact it hashes is
    ///    settled by then; and inside the transaction because D-14 says a record
    ///    that could commit separately is a record that can be lost on a crash.
    ///
    /// # CONTRACT — isolation, and the two invariants that are not the same
    ///
    /// This transaction is opened with the **engine default**, which on Postgres
    /// is READ COMMITTED. Two different invariants stand behind that, and an
    /// earlier version of this paragraph conflated them; the conflation is what
    /// hid a live defect, so both are spelled out.
    ///
    /// **The counter invariants need no SSI, and that argument is sound.** The
    /// audit chain's primary key `(tenant_id, chain_id, seq)` makes a fork
    /// unrepresentable at any isolation level; `uq_pricing_outbox_sequence` does
    /// the same for the event counter and `uq_pricing_plan_current` for the
    /// current revision. Two writers that read one head cannot both insert after
    /// it — the loser takes a unique violation and its whole transaction rolls
    /// back. The sibling ledger's posting path opens `SERIALIZABLE` because its
    /// chain tip lives in a separate row read locklessly, a premise this design
    /// does not have.
    ///
    /// **The predicate invariant is a different thing, and no key covers it.**
    /// The commit also depends on "every row that publishes was judged by the
    /// rule set", which ranges over rows a concurrent writer can *insert* and
    /// *mutate*. A key can only refuse a second row at a position it already
    /// knows about; it says nothing about a row that did not exist when the
    /// subject was assembled. Under READ COMMITTED every statement takes a fresh
    /// snapshot, so the window between the assembly at step 1 and the flips at
    /// step 3 — which contains the registry's network round-trip — is wide, and
    /// a fork argument cannot close it. **A conclusion about predicates does not
    /// follow from a premise about keys.**
    ///
    /// **It is closed by the row-version guard, not by isolation.** Step 3
    /// publishes exactly the `(price_id, row_version)` set the rule set judged,
    /// so a row whose content moved is refused naming the row and a row that did
    /// not exist is simply not in the set. That is what D-141 gave every price
    /// row its own entity tag for.
    ///
    /// **The rest of the subject, input by input, with the mechanism that holds
    /// each.** The enumeration is written out rather than closed with "those are all
    /// the inputs": an unchecked "all" is the same shape of claim that hid the defect
    /// above.
    ///
    /// | Input | Read by | Held by |
    /// |---|---|---|
    /// | The **draft** revision row | [`assemble`] | its own compare-and-swap in `publish_revision` |
    /// | Its Slice-2 children — phases, add-on rules, descriptor set | [`assemble`] | that same revision tag; `PlanShapeRepo` bumps it on every child edit |
    /// | The plan's **draft** price rows | [`assemble`] | the per-row `row_version` guard above (D-141) |
    /// | The plan's **published** price rows — *content* | [`assemble`] | `pricing_price`'s append-only trigger: a published row's price, scope, model and tag columns are immutable |
    /// | The **current published** revision row (`available_from`/`available_to`) | [`published_baseline`] | `pricing_plan`'s frozen-column whitelist, which fires on any UPDATE of a non-draft row |
    /// | That revision's **phase set** (the terminal phase id) | [`terminal_phase`] | `pricing_plan_phase`'s append-only trigger, which refuses DML under a parent revision that is not `draft` |
    /// | The tenant's policy row | [`rule_params`] | nothing — see the isolation paragraph below |
    ///
    /// **One premise in that table is not a mechanism, and it is stated as a
    /// premise.** The append-only trigger on `pricing_price` guarantees a
    /// published row's *content*; it does **not** guarantee the *membership* of
    /// the published set, because the same trigger sanctions
    /// `published -> superseded`. Four completeness instructions range over that
    /// set (`inst-ph-coverage`, `inst-cs-hybrid`, `inst-cs-usage`,
    /// `USAGE_MARKET_INCOMPLETE`) and `PublishedBaseline::phase_ids_in_use` is
    /// derived from it, so a row leaving it between assembly and commit would
    /// change what the rule set judged. What holds it today is that **this gear
    /// has no producer for that flip at all**: its two sanctioned producers are
    /// the D-88 supersession unit and the D-100 grandfathering cutover, both
    /// Slice 7's, and neither exists.
    ///
    /// **The premise has narrowed twice, and it is now one step from gone.**
    /// This paragraph first closed with "the `PriceWindow` store is unbuilt",
    /// offered as why the two units could not exist; the store and the window state
    /// machine landed before either unit was built on it.
    ///
    /// **The flip has a producer in this crate**:
    /// [`price_repo::commit_supersession_rows`] flips a predecessor
    /// `published -> superseded` and publishes the successor beside it (D-88's row
    /// half, ordered per D-195). What holds the premise today is therefore no longer
    /// "there is no producer" but the strictly weaker **"no surface reaches it"** —
    /// the unit around those two functions does not exist, so nothing a client can
    /// call flips a published row out of this candidate set. The premise is stated at
    /// its real strength rather than at the comfortable one.
    ///
    /// # What the next group owes, decided rather than left open (D-195)
    ///
    /// The route that reaches the supersession unit deletes the premise, and the
    /// published half of the candidate set then needs its own pin. **The decision is
    /// taken: extend the judged set.** Published rows carry `row_version` frozen with
    /// their content (D-141), so the same `(price_id, row_version)` shape reaches
    /// them, and the commit verifies before the flips that every published row the
    /// rule set judged is *still* `published` at *that* version. It is a membership
    /// check wearing a version check's clothes, which is exactly what a frozen tag
    /// makes it.
    ///
    /// **Departures are the hazard and arrivals are covered — by an argument, so it
    /// is written down.** A pin over the judged rows catches a published row
    /// *leaving* the set. It cannot catch one *arriving*, and an arrival matters:
    /// the market-uniformity rules (`TAX_BASIS_MIXED_MARKET`,
    /// `PRORATION_CONTRACT_MIXED_MARKET`) read content across the whole set, and the
    /// D-82 unit guard preserves the counter fields while leaving `tax_inclusive`
    /// and the proration contract free to move. Arrivals are nevertheless covered:
    ///
    /// - A **supersession** arrival is paired with a departure on the same key, by
    ///   `inst-su-commit`'s ordering — the flip precedes the publish, in one
    ///   transaction — so the pin fires on the departure.
    /// - A **cutover** (D-100) arrival on the `all_subscriptions` key is likewise
    ///   paired with that key's flip (`inst-co-supersede`).
    /// - The cutover's **grandfathered copy** is the one unpaired arrival: it lands
    ///   on a new generation key with nothing departing. It is covered by D-132 —
    ///   the market-uniformity rules exclude immutable grandfathered generations —
    ///   so the rules that could care do not read it.
    ///
    /// If a third producer of a published row is ever added that is *not* paired
    /// with a departure and *not* a grandfathered generation, this argument is what
    /// it has to be checked against.
    ///
    /// **`SERIALIZABLE` is still not the answer**, for the reason below: it would
    /// hold predicate locks across the registry round-trip, which is the one cost
    /// step 2's ordering exists to bound.
    ///
    /// **`SERIALIZABLE` is therefore not raised, and that is a judgement rather
    /// than an impossibility.** `Db::transaction_ref_mapped_with_config` takes no
    /// `extract_db_err` and is the platform's building block for exactly this
    /// case — a service entrypoint returning domain errors that needs non-default
    /// isolation — so the option is available and is declined on its merits: SSI
    /// would hold predicate locks across the registry's network round-trip, which
    /// is the one cost the ordering decision at step 2 was chosen to bound, and
    /// it would buy nothing the tags and the two triggers above do not already
    /// buy. The one input covered by neither is the tenant's policy row, read at
    /// the start of the transaction; a policy edit committing afterwards means
    /// the publish was judged against the snapshot it read rather than the newest
    /// one, which is staleness and not a bypass — the next publish enforces the
    /// new cap — and this gear has no writer for that row at all.
    ///
    /// **Contention is the remainder, and it is a retry question.**
    /// `transaction_with_retry` is the platform's mechanism for that and is
    /// **not usable here**: it classifies retryability by extracting a
    /// `sea_orm::DbErr` out of the error type, and [`DomainError`] is a
    /// `Clone + Eq` value type carried into responses that cannot hold one, so
    /// every failure would classify as non-retryable and the loop would be
    /// decoration. The retry decision therefore sits with the caller — the
    /// publish endpoint and its client.
    ///
    /// **That debt is discharged, both halves** (it read
    /// "half discharged" until then, and a review pass re-derived the missing
    /// half from this paragraph before measuring it).
    ///
    /// The wire code arrived — `CONCURRENT_MUTATION` (D-159, 409, naming the
    /// aggregate) — and so did the recognition. `plan_repo::publish_revision`
    /// maps its driver error through `contention_or_db`, which answers
    /// `RepoError::ConcurrentMutation` on `is_unique_violation()`; that folds to
    /// `DomainError::ConcurrentMutation` and renders **409**, not the 500 this
    /// paragraph used to claim. `sqlite_publish_commit.rs` asserts the pair by
    /// equality on this very path.
    ///
    /// The recognition sits in the repo layer rather than here for the reason
    /// below, which still holds: it has to read a backend-specific constraint
    /// violation, which is the coupling `RepoError`'s own doc refuses and which
    /// only a layer that knows the backend can do.
    ///
    /// **One narrower race is still a 500** and must not be swept up by a
    /// careless correction of this paragraph: a scope-key collision inside
    /// `price_repo::publish_rows` is not recognized and does flatten to
    /// `RepoError::Db`. What was owed and is now paid is the D-159 class — the
    /// tell.
    ///
    /// **One refusal after the registry request is permanent, and it is moved
    /// ahead of it.** Every other post-request refusal self-heals, because the
    /// `request_id` is deterministic and a retry of the same unit is handed the
    /// same pending handle. A retired plan's is not: no retry of that unit can
    /// ever succeed, so the handle would stand pending forever and trip
    /// `pricing.catalogversion.commit_overdue` for a publish that never happened.
    /// [`refuse_unpublishable_predecessor`] asks that question **before** step 2.
    /// What remains orphanable is the race in which a concurrent commit publishes
    /// this very revision between the two runs; §3.6's remediation — "a registry
    /// re-request, never a silent re-emit" — is what covers it, and the handle is
    /// identifiable to its unit because the id is derived rather than random.
    ///
    /// **What this crate's tests can and cannot see.** `sqlite::memory:`
    /// serializes writers, so no test here distinguishes one isolation level from
    /// another. The row-version guard, by contrast, **is** provable here and is
    /// proved — it is a predicate on a statement, not a property of a schedule.
    ///
    /// # Idempotency is deliberately not wired here
    ///
    /// `pricing_idempotency_dedup` and the gate over it exist and take a
    /// transaction handle, so wiring them in would be easy — and wrong for this
    /// path. S2 §5 scopes this endpoint's idempotency "per plan revision", and
    /// the revision already publishes at most once: `LifecycleState` gives
    /// `draft` one edge out and
    /// [`publish_revision`](crate::infra::storage::repo::plan_repo::publish_revision)'s
    /// compare-and-swap refuses the second attempt. The client-key gate answers
    /// a different question — "is this the same HTTP request" — and it needs a
    /// header only a surface receives. The endpoint exists and reads no such
    /// header, so the gate is unwired on a route that is mounted: the machinery
    /// it would use is `infra::idempotent`, and what is owed is the wiring.
    ///
    /// # Errors
    /// [`DomainError::ValidationFailed`] carrying the whole report when the
    /// commit-time run finds anything;
    /// [`DomainError::FixtureMissing`] when a row's shape has no green joint
    /// fixture; [`DomainError::CatalogVersionUnavailable`] when the registry
    /// cannot be reached or has none configured — publish stops rather than
    /// inventing a version; [`DomainError::NotFound`] when the plan has no open
    /// draft revision; [`DomainError::StaleVersion`] when `expected` is not the
    /// revision's current row version;
    /// [`DomainError::PlanRetiredNoSuccessor`] when the plan's current revision
    /// can never be superseded; [`DomainError::LifecycleForbidden`] when the
    /// named revision is frozen; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "every parameter is a distinct precondition of a publish - the unit, the \
                  version to swap on, the approval decision, the actor and the correlation \
                  id are what the audit record and the outbox row are made of, and bundling \
                  them into a request struct would hide which of them a caller may omit \
                  (none)"
    )]
    pub async fn commit(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        unit: PlanPublishUnit,
        expected: RowVersion,
        authorization: PublishAuthorization,
        actor_principal_id: Uuid,
        correlation_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<PublishReceipt, DomainError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        let policies = self.policies.clone();
        let gate = self.fixture_gate.clone();
        // Cloned beside the gate and for its reason: the transaction closure
        // outlives the borrow of `self`.
        let metrics = Arc::clone(&self.metrics);
        let registry = Arc::clone(&self.registry);
        let request_id = publish_request_id(tenant_id, unit);

        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PublishReceipt, DomainError, _>(move |txn| {
                Box::pin(async move {
                    // 1. The second run. Same assembler, same rule set, same
                    // gate - against the world as it now stands.
                    let shape = assemble(txn, &scope, tenant_id, unit.plan_id, now).await?;
                    let params = rule_params(&policies, txn, &scope, tenant_id, &shape)
                        .await?
                        .opening_initial_coverage();
                    if shape.revision != unit.revision {
                        return Err(DomainError::NotFound {
                            subject: "open plan draft revision".to_owned(),
                            id: format!("{}/{}", unit.plan_id, unit.revision),
                        });
                    }

                    // 1a. The approve→commit window, closed **here** and not at
                    // the surface. `inst-ap-pin` scopes the TOCTOU void to a
                    // `submitted` record, so an approved unit is not voided when
                    // its subject moves, and a check made before this
                    // transaction opened would be a check the world can move
                    // behind. The row-version swap below does not cover it: it
                    // swaps on the *revision's* version, and a price row edited
                    // after the approve moves the row's version and not the
                    // revision's.
                    //
                    // Ahead of the rules, because the answer is about the
                    // reviewer's decision rather than about the plan: telling an
                    // operator to fix a violation in content the second person
                    // never agreed to is the wrong next action.
                    if let Some(pinned) = authorization.pinned_content_hash()
                        && crate::domain::approval::content_hash(&shape) != pinned
                    {
                        return Err(DomainError::ApprovalContentMismatch(format!(
                            "plan {}/{}: the content approved under {} is not the content this \
                             commit would freeze; re-submit and have the change reviewed again",
                            unit.plan_id,
                            unit.revision,
                            authorization
                                .approval_ref()
                                .map_or_else(|| "-".to_owned(), |id| id.to_string()),
                        )));
                    }

                    let report = run_publish_rules(&shape, &params);
                    // **Reported here as well as at the pre-check, and the
                    // commit is the more important of the two.** The route's
                    // approved arm reaches this without a pre-check at all, and
                    // a plan that failed its pre-check is never approved — so a
                    // block raised *here* is one that appeared between the
                    // reviewer's decision and the commit, which is the only kind
                    // an operator cannot see coming. There is no double count:
                    // the two runs are two events, and a plan blocked at
                    // pre-check never reaches this one.
                    crate::infra::metrics::report_market_metrics(&*metrics, &shape, &params);
                    if !report.is_publishable() {
                        return Err(DomainError::ValidationFailed(report));
                    }
                    let validated = validated_draft_rows(&shape);
                    check_fixtures(&gate, &shape)?;

                    // The one **permanent** refusal that would otherwise land
                    // after the registry request, moved ahead of it: a retired
                    // plan takes no successor, so its handle would stand pending
                    // forever and trip `commit_overdue` for a publish that can
                    // never happen. Asked of the state machine rather than
                    // matched on `Retired`, and it is **not** the guard -
                    // `publish_revision` still is, and answers the same way.
                    refuse_unpublishable_predecessor(txn, &scope, tenant_id, unit.plan_id).await?;

                    // 2. Addressability, fail closed. No locally invented
                    // version, ever: two incrementers make `CatalogVersion`
                    // unordered.
                    let pending = request_version_now(registry.as_ref(), &ctx, &request_id).await?;

                    // 3. The row set.
                    let published_revision = plan_repo::publish_revision(
                        txn,
                        &scope,
                        tenant_id,
                        unit.plan_id,
                        unit.revision,
                        expected,
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;
                    // Exactly the rows the rule set just judged, at the
                    // versions it judged them at. See `publish_rows`: a
                    // re-derived set would publish rows validated by nothing.
                    // **D-332: the initial windows, written after the pin.**
                    //
                    // Not before the rules, which is where this sat first and
                    // where it was wrong: the submit computes the content pin
                    // over a shape with no publish-opened window, so writing one
                    // ahead of the rules made the commit's content differ from
                    // the reviewer's and the approval died with
                    // `APPROVAL_CONTENT_MISMATCH`. The pin was right — the
                    // placement was not.
                    //
                    // It does not need to be early. `inst-wc-required` already
                    // passes on a key this publish is freezing for the first
                    // time (`coverage::opened_by_this_publish`), so the rule and
                    // the write are independent: the rule states that coverage
                    // *will* exist, and this is where it comes to.
                    open_initial_windows(
                        txn,
                        &scope,
                        tenant_id,
                        &shape,
                        actor_principal_id,
                        now,
                        correlation_id,
                    )
                    .await?;

                    let price_ids = price_repo::publish_rows(
                        txn,
                        &scope,
                        tenant_id,
                        unit.plan_id,
                        &validated,
                        // **The readiness the rule set just judged these
                        // rows with**, not a fresh read: D-154 freezes the
                        // result of the check that passed, so resolving
                        // again — even here — would be a second answer to
                        // one question.
                        params.region_readiness(),
                        // The tenant default the rule set resolved against, for
                        // the same reason and from the same params: a second read
                        // here would be a second answer to one question.
                        params.default_rounding_policy(),
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    // 4. The ref, carrying the subject G6 will project.
                    catalog_version_ref_repo::record_pending(
                        txn,
                        &scope,
                        PendingVersionRow::for_subject(
                            tenant_id,
                            pending.pending_ref.clone(),
                            &SubjectRef::Plan(unit.plan_id.get()),
                            // The revision this commit's rule set judged. The
                            // projector reads it rather than whatever revision
                            // is current when the sweep arrives, which may be a
                            // later one: the registry batches at up to five
                            // minutes and a second publish inside that window
                            // would otherwise freeze content this publish never
                            // judged into this publish's version.
                            Some(unit.revision),
                            // And the state that flip produced. Read back into
                            // the delta rather than the row's state as it then
                            // stands: that keeps moving (`published ->
                            // superseded` at the next revision's commit), and
                            // `superseded` is a value D-128 does not
                            // contemplate for a projected subject.
                            Some(published_revision.lifecycle_state),
                            now,
                        ),
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    // 5. The event, carrying the pending handle.
                    let payload = PlanPublishedPayload {
                        plan_id: unit.plan_id,
                        revision: unit.revision,
                        pending_version_ref: pending.pending_ref.clone(),
                        price_ids: price_ids.clone(),
                        correlation_id,
                    };
                    outbox_repo::enqueue(
                        txn,
                        &scope,
                        NewOutboxEvent::plan_published(tenant_id, &payload, now),
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    // 6. The record of what the transaction did.
                    let seq = audit_repo::append(
                        txn,
                        &scope,
                        NewAuditEntry {
                            tenant_id,
                            chain_id: audit_repo::plan_chain(unit.plan_id),
                            recorded_at: now,
                            actor_principal_id,
                            action: AuditAction::Publish,
                            subject_kind: AuditSubjectKind::PlanRevision,
                            subject_ref: audit_repo::plan_revision_ref(unit.plan_id, unit.revision),
                            before_state: Some(subject_state(
                                LifecycleState::Draft,
                                expected.get(),
                                None,
                            )),
                            after_state: Some(subject_state(
                                published_revision.lifecycle_state,
                                published_revision.row_version.get(),
                                Some(&pending.pending_ref),
                            )),
                            approval_ref: authorization.approval_ref(),
                            correlation_id,
                        },
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    // 7. The units this publish did **not** consume.
                    //
                    // A unit still `submitted` over the revision this
                    // transaction just froze is an orphan: its subject is now
                    // immutable, so no approve of it can ever lead to a publish,
                    // and re-deriving it answers `APPROVAL_CONTENT_MISMATCH`
                    // forever. Left standing it sits in the reviewer's queue
                    // asking for a decision that cannot mean anything.
                    //
                    // Only `submitted` rows match, so this does **not** touch
                    // the `approved` record that authorized this very commit.
                    // A publish voiding here does not "undo its own
                    // authorization": the authorizing unit is not pending.
                    // And it matches the **exact** subject rather than
                    // the plan prefix the TOCTOU guard uses, so a later slice's
                    // unit over another revision, a window or an overlay of the
                    // same plan is not swept up by a publish that had nothing to
                    // do with it.
                    approval_repo::void_pending_for_subject(
                        txn,
                        &scope,
                        tenant_id,
                        &audit_repo::plan_revision_ref(unit.plan_id, unit.revision),
                        crate::infra::approval::ORPHANED_BY_PUBLISH_REASON,
                        now,
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    Ok(PublishReceipt::new(
                        unit,
                        pending.pending_ref,
                        price_ids,
                        seq,
                    ))
                })
            })
            .await;
        outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: publish transaction: {infra}"))
            })
        })
    }
}

/// The `(price_id, row_version)` set the rule set actually judged **and this publish
/// unit is entitled to move**.
///
/// The **draft** half of the candidate row set: the published half is already
/// published and is physically immutable, so nothing about it can move between
/// validation and the flip. What this returns is what
/// [`publish_rows`](crate::infra::storage::repo::price_repo::publish_rows)
/// publishes, at the versions it was validated at, and a row whose content moved
/// in between no longer matches. See that function's doc for the defect this
/// closes.
///
/// # A draft on an occupied key belongs to another unit, and is excluded
///
/// Price rows hang off `plan_id`, not off a revision, so this set is every draft row
/// of the plan. Since D-195 that includes one it must not touch: a **supersession
/// successor**, staged by `price_repo::insert_successor_draft_on` beside the published
/// row it will supersede.
///
/// Sweeping it up is not a near-miss. `publish_rows` would flip it to `published`
/// while its predecessor still reads `published`, the published-plane partial `UNIQUE`
/// would refuse the statement, and the refusal arrives as a raw driver error —
/// `RepoError::Db` → `DomainError::Internal` → **500**, taking an entirely unrelated
/// revision publish down with it for as long as a supersession is pending on any key
/// of the plan. No publish rule catches it first; there is no scope-key-duplication
/// rule in the set (found by review, after `publish_rows`' amended doc had
/// named the wrong caller for this collision).
///
/// So the rule is: **a draft row whose canonical scope key already carries a published
/// row is not this unit's to publish.** It publishes through the unit that staged it,
/// which is the one that also flips the predecessor and moves the windows. This is the
/// same reasoning `publish_rows` gives for a *newly inserted* row staying `draft` —
/// nothing validated it as part of this act — one step further: here something did
/// validate it, and it still is not this act's row.
///
/// The exclusion is **decided here rather than in the rule set** because it is about
/// entitlement rather than about validity: the successor's content is perfectly
/// publishable, and a rule reporting a violation would tell an operator to fix a row
/// that is not wrong. `shape.rows` keeps carrying it, so every completeness and
/// uniformity rule still sees the key covered.
///
/// **It is a no-op on every world that predates the second door** — nothing else can
/// put a draft on an occupied key (`inst-pr-return`/D-21 refuses it at save,
/// `IMPORT_TARGETS_PUBLISHED` per row on the bulk plane) — which is why no existing
/// case moves.
fn validated_draft_rows(shape: &PlanShape) -> Vec<(Uuid, RowVersion)> {
    unit_row_set(shape)
        .into_iter()
        .filter(|record| record.lifecycle_state == LifecycleState::Draft)
        .map(|record| (record.price_id, record.row_version))
        .collect()
}

/// The candidate rows **this** publish unit owns — `shape.rows` less the drafts
/// another unit staged.
///
/// [`validated_draft_rows`]' occupancy argument, hoisted so it has one owner and
/// two readers. The other reader is `api::rest::publish`'s materiality evaluation,
/// and the reason it must be the same set is the sentence
/// [`validated_draft_rows`] already makes about entitlement: a draft row whose
/// canonical scope key already carries a `published` row was staged by
/// `price_repo::insert_successor_draft_on` for the D-88 supersession unit, and a
/// row this act will not publish is not a row this act needs a second principal
/// for.
///
/// **Measured, not feared.** Handing `materiality::evaluate` `shape.rows` whole
/// puts a *moved* row — one a supersession staged and left awaiting its reviewer —
/// into a plan-revision change set, which is the one shape
/// `triggered_by_content`'s `moves_no_row` cannot see through. A plan revision
/// carrying a D-319 period floor and nothing else then evaluates
/// `AutoPublishable` and publishes a €500-per-period minimum with no second
/// principal, through the real router (`rest_publish::
/// a_period_bound_published_beside_an_orphaned_successor_is_still_judged`). The
/// clause that makes the arm look unreachable — *"repricing an occupied key needs
/// the D-88 unit"* — is true and stopped being sufficient when D-195 gave that
/// unit a second door and `inst-su-api` gave it a route.
///
/// The rows are returned **whole** rather than as ids, because the second reader
/// compares content: a delta needs the amount a row moved from.
///
/// # This contradicts D-200's normative sentence, and D-326 is the entry that
/// # reverses it
///
/// D-200 [H] decided **exactly this divergence** and decided to keep
/// it: *"a plan revision's materiality ranges over its whole candidate set — the
/// assembled shape, published plus draft — including rows the commit will not
/// flip"*, now normative in `design/05-governance.md` §3 `inst-mat-percurrency`.
/// It rests on one premise, stated three times — *"over-material and never
/// under-material"*, *"fails safe in the direction that matters"*, *"the accepted
/// cost is legibility, not control"* — and the premise is **false**, measured
/// through the router: the staged row is the only thing that can make this route
/// answer `AutoPublishable` at all, because it is the only moved row a plan
/// revision can hold and `moves_no_row` is what it defeats. The cost is control.
///
/// D-200 weighed exactly two options and this is **neither**. Its rejected (a) was
/// *"building the change set from `validated_draft_rows`"* — the commit's flip
/// list, drafts only — and it loses for a reason this function does not inherit:
/// it drops the **published** half, so `inst-mat-newrow` would stop seeing a newly
/// authored row's absent baseline on every publish. This set is the candidate set
/// less **another unit's** staged rows: the published plane is intact, a newly
/// authored row is on a free key and stays, and `inst-mat-newrow` and
/// `inst-mat-percurrency` range over what they ranged over before, minus one row
/// that was never this act's. It is never *less* material than D-200's reading in
/// any world — where that reading answered `thresholdReached` naming a foreign
/// row, this answers `alwaysMaterialTrigger` naming the revision — and it retires
/// the legibility cost D-200 accepted in the same move.
///
/// **That is a reversal of a normative statement an owner took directly, so it is
/// a decision and not a fix** — and the decision exists: **D-326 [H]**, decided
/// and confirmed by the product owner, "Amends D-200", implemented
/// by hoisting `unit_row_set` out of that filter, and propagated to S5 §3
/// `inst-mat-percurrency`, which no longer says what D-200 made it say.
///
///
/// It is a no-op on every world that predates that door — nothing else can put a
/// draft on an occupied key (`inst-pr-return`/D-21 at save,
/// `IMPORT_TARGETS_PUBLISHED` on the bulk plane) — which is why no existing case
/// moves.
pub(crate) fn unit_row_set(shape: &PlanShape) -> Vec<&PriceRecord> {
    // Compared by canonical **rendering**, the way `infra::window`'s pending-key check
    // does: `ScopeKey` is not `Ord`, and the rendering is the ten axes in a fixed
    // order — the same string a `DUPLICATE_SCOPE_KEY` refusal names.
    //
    // Two equal renderings are one key because the two free-form axes that can
    // collide refuse the two strings that would make them collide:
    // `scope_key::KEY_SEPARATOR`, which would move a segment boundary, and
    // `scope_key::ABSENT_AXIS_TOKEN`, which would make an authored value render as
    // the absent one. Without the second, a draft on meter `none` and a published
    // meterless row on the same eight axes are two keys with one rendering, and the
    // filter below drops the draft out of its own publish unit.
    let occupied: std::collections::BTreeSet<String> = shape
        .rows
        .iter()
        .filter(|record| record.lifecycle_state == LifecycleState::Published)
        .map(|record| record.scope_key.to_string())
        .collect();
    shape
        .rows
        .iter()
        .filter(|record| {
            record.lifecycle_state != LifecycleState::Draft
                || !occupied.contains(&record.scope_key.to_string())
        })
        .collect()
}

/// Refuse a publish onto a predecessor that can never be superseded, **before**
/// the registry is asked for a handle.
///
/// It is not a second owner of D-90's rule:
/// [`publish_revision`](crate::infra::storage::repo::plan_repo::publish_revision)
/// is still the authority and answers identically. What this buys is position.
/// Every other refusal reachable after the registry request self-heals — the
/// `request_id` is deterministic, so a retry of the same unit is handed the same
/// pending handle rather than a second one. A retired plan's refusal is
/// **permanent**: no retry of that unit can ever succeed, so the handle stands
/// pending forever and trips `pricing.catalogversion.commit_overdue` for a
/// publish that never happened — which is the outcome the request's position was
/// chosen to avoid in the first place.
///
/// **`pub(crate)` for a second caller**, `infra::supersession`: a plan whose current
/// revision can never be superseded takes no *successor row* either, and the refusal
/// is permanent, so that unit hoists it ahead of its own registry request for exactly
/// the reason this one does.
///
/// # Errors
/// [`DomainError::PlanRetiredNoSuccessor`] when the plan's current revision
/// cannot flip `superseded`; [`DomainError::Internal`] on a storage failure.
pub(crate) async fn refuse_unpublishable_predecessor(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<(), DomainError> {
    let Some(current) = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
    else {
        return Ok(());
    };
    if current
        .lifecycle_state
        .can_transition(LifecycleState::Superseded)
    {
        return Ok(());
    }
    Err(DomainError::PlanRetiredNoSuccessor(format!(
        "plan {plan_id} stands at a {} revision, which can never be superseded; \
         it takes no successor",
        current.lifecycle_state
    )))
}

/// Resolve the tenant's authoring policy into the rule set's parameters.
///
/// One read per run, never memoized: a cached cap keeps rejecting plans after an
/// operator has raised it, and the two runs of one publish would be free to
/// disagree about the limit they enforced. The commit re-reads it **inside its
/// transaction**, because the caps are part of the state §4.2's second run
/// re-validates.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure or a policy row this gear
/// cannot read.
///
/// `pub(crate)` for [`crate::infra::repricing`], the mass-repricing apply's own
/// module: its aggregate pass is this exact function's `run_publish_rules`
/// call, over a shape it assembles itself (`assemble_from` against the plan's
/// **current** revision rather than an open draft — a repricing run has none),
/// so the parameters that pass has to resolve are these same five and not a
/// second reading of the tenant's policy grown for one caller.
pub(crate) async fn rule_params(
    policies: &PolicyObjectRepo,
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    shape: &PlanShape,
) -> Result<PublishRuleParams, DomainError> {
    let plan_id = shape.plan_id;
    let policy = policies
        .authoring_policy_on(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    // D-212: the bundle markets this plan's rows are judged against
    // (`inst-bc-taxbasis`'s reverse half). Resolved **here**, in the same pass as
    // the caps, for the reason this function exists at all — the rule set runs
    // twice on one publish and a rule that reached for storage itself could
    // answer differently in the two runs for no authored reason. A plan that is a
    // component of nothing costs one index probe returning no rows.
    let referencing = crate::infra::bundle::referencing_markets(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    // `inst-tx-region`: the tenant's **active** region universe. Resolved here,
    // in the same pass as everything else, for the reason this function exists —
    // the rule set runs twice on one publish and a rule reaching for storage
    // itself could answer differently in the two runs for no authored reason.
    //
    // It is the `active` set, which is `overlay_repo::declares`' predicate one
    // plane over: a value that reached `retired` must not validate a new row
    // against itself. An empty answer is C2's fail-closed reading and not an
    // error — a tenant who has declared no region publishes no row.
    let declared_regions = taxonomy_repo::active_regions(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    // **Loaded, not defaulted.** D-334's rule reads this set and an empty one
    // means "unconstrained", so a `rule_params` that forgot the read would make
    // the vocabulary check pass on every plan while looking registered — the
    // D-254 defect class this file has already paid for once with the composite
    // definitions.
    let declared_rounding_policies =
        taxonomy_repo::active_rounding_policies(runner, scope, tenant_id)
            .await
            .map_err(|e| repo_failure(&e))?;
    // D-356's GL-code vocabulary, loaded for exactly the reason the line above
    // is: its rule reads the empty set as "unconstrained", so a forgotten read
    // is a check that passes every plan while looking registered.
    let declared_gl_codes = taxonomy_repo::active_gl_codes(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    // C4's `RegionTaxReadiness`, resolved in the same pass and for the same
    // reason. One statement over the tenant's declared regions rather than one
    // per market: a plan at C1's 20-currency floor spans as many, and twenty
    // round trips inside the commit transaction is twenty chances to hold it
    // open longer than it needs.
    let readiness = RegionTaxReadiness::new(
        taxonomy_repo::region_readiness_map(runner, scope, tenant_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .into_iter()
            .map(|(region, markers)| {
                (
                    region,
                    RegionReadiness {
                        tax_category: markers.tax_category,
                        tax_rate_present: markers.tax_rate_present,
                    },
                )
            })
            .collect(),
    );
    let tax_display_policy = policy.tax_display_policy().map_err(|e| repo_failure(&e))?;
    // `inst-cb-addon`'s domain: what each of this plan's add-ons covers. Resolved
    // here for the reason the markets above are — the rule is a property of other
    // plans' published rows and the set runs twice on one publish.
    let addon_coverage =
        crate::infra::currency_binding::addon_coverage(runner, scope, tenant_id, shape)
            .await
            .map_err(|e| repo_failure(&e))?;
    // Slice 6's change graph, resolved for the same two reasons the markets and
    // the add-on coverage above are: the rules read other plans' published
    // revisions, and the set runs twice on one publish with the same answer
    // required both times. Narrowed to the edges this contract names, so the
    // index's size follows the contract rather than the catalog.
    let change_targets = crate::infra::change_graph::resolve(
        runner,
        scope,
        tenant_id,
        plan_id,
        shape.change_contract.targets(),
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(PublishRuleParams::new(
        policy.interval_bounds(),
        policy.descriptor_rule(),
        policy.default_rounding_policy_ref().map(ToOwned::to_owned),
        // The two soft caps, which had no consumer at all until D-160 named the
        // advisory code. Read in the same pass as everything else, so the
        // commit's second run judges the caps the state then holds.
        SoftSizeCaps::new(
            policy.max_tier_bands_per_row(),
            policy.max_price_rows_per_plan(),
        ),
    )
    .with_referencing_markets(referencing)
    .with_declared_regions(declared_regions)
    .with_declared_rounding_policies(declared_rounding_policies)
    .with_declared_gl_codes(declared_gl_codes)
    .with_tax_display(tax_display_policy, readiness)
    .with_addon_coverage(addon_coverage)
    .with_change_targets(change_targets))
}

/// The registry request id of one publish unit.
///
/// **Deterministic, and that is the whole point.** `request_id` is documented as
/// the catalog's own idempotency handle — "a retry after a crash returns the
/// same pending ref rather than queueing a second assignment" — so a commit that
/// rolled back and is retried must present the id its first attempt did. Derived
/// from `(tenant, plan, revision)` because that is what identifies the publish:
/// a revision publishes exactly once, so the id names one assignment for the
/// life of the plan. A random id per attempt would orphan a pending ref at the
/// registry on every rollback, and each orphan trips
/// `pricing.catalogversion.commit_overdue` for a publish that never happened.
///
/// The tenant is in it, unlike the outbox dedup key: that key is unique **per
/// tenant** by its index, while the registry is a cross-tenant service and has
/// no such scope to lean on.
///
/// **The unit's kind is in the preimage (D-99).** A revision publishes exactly
/// once, but a revision is not the whole of what identifies a unit any more: a
/// window mutation is a publish unit over the plan's *current* revision, so a
/// content publish and a window mutation of one revision are two units that would
/// otherwise present the same id. The registry is idempotent on that id, so the
/// second would be answered with the first's pending ref — and the window mutation
/// would ride a version that never re-projected it, which is precisely the
/// last-warmed-delta-disagrees-with-the-truth-side exposure D-99 exists to close.
/// Two window mutations of one revision are likewise two units, which is why the
/// window's own id is in the token rather than a bare `window-mutation` marker.
///
/// The plan-content token is unchanged from before there was a kind, for
/// [`PublishUnitKind::request_token`](crate::domain::publish::PublishUnitKind::request_token)'s reason: re-spelling it would orphan every
/// pending ref a retry is meant to find.
#[must_use]
fn publish_request_id(tenant_id: Uuid, unit: PlanPublishUnit) -> String {
    format!(
        "{}/{tenant_id}/{}/{}",
        unit.kind.request_token(),
        unit.plan_id,
        unit.revision
    )
}

/// Build the publish subject from storage, through whichever runner the
/// caller holds.
///
/// Generic over the runner precisely so the pre-check and the commit's
/// re-validation are the **same** assembly: two assemblers would be two
/// answers to "what is being published", and §4.2's whole point is that the
/// second run judges the same subject against a world that has moved.
///
/// # Errors
/// [`DomainError::NotFound`] when the plan has no open draft revision;
/// [`DomainError::Internal`] on a storage failure or a current revision with
/// no unique terminal phase.
pub(crate) async fn assemble(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<PlanShape, DomainError> {
    let draft = plan_repo::load_open_draft(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "open plan draft revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    assemble_from(runner, scope, tenant_id, plan_id, draft, now).await
}

/// The assembly itself, over a revision the caller has already resolved.
///
/// **One body, two entry points**, which is the reason [`assemble`]'s own doc
/// gives for being generic over the runner, applied one level up: two assemblies
/// would be two answers to "what is this plan", and the window path validates
/// against the same subject the publish path judges.
///
/// [`assemble`] resolves the plan's **open draft**; `infra::window` resolves its
/// **current** revision and calls this directly, because
/// [`PlanPublishUnit::window_mutation`] names the current revision by decision —
/// a window mutation authors no plan content and opens none. Passing the record in
/// rather than resolving it here is what lets that caller take the revision's
/// `lifecycle_state` off the same read it assembles from, instead of reading the
/// plan row twice and risking two answers about which revision is current.
pub(crate) async fn assemble_from(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    draft: crate::domain::plan::PlanRevision,
    now: OffsetDateTime,
) -> Result<PlanShape, DomainError> {
    // **Destructured, so a field added to `PlanRevision` and forgotten here is a
    // compile error** (D-259). This assembly was written field by field and was
    // wrong three times in one day — D-257's `composites`, and D-258's
    // `change_contract` and `entitlement_grants`, one of which refused
    // legitimate publishes outright. A missing line was not a compile error and
    // no test that hand-builds a `PlanShape` could see it, which is why all
    // three passed every gate. `content_pin::put_plan_shape` has always
    // destructured for exactly this reason; the defect was that it destructures
    // the *shape*, not the draft the shape is assembled from.
    //
    // The five ignored fields are ignored **by name**: `plan_id` and `revision`
    // are the constructor's own arguments, and `lifecycle_state`, `created_by`,
    // `created_at_utc` and `row_version` are the row's provenance rather than
    // its authored content — the rules judge what an author wrote, and the pin
    // frames the same set.
    let crate::domain::plan::PlanRevision {
        // `plan_id` is the caller's argument; `revision` is bound because the
        // child-table loads below key on it.
        plan_id: _,
        revision,
        sku_id,
        plan_tier,
        plan_name,
        billing_cycle,
        frequency,
        plan_tier_override,
        purchase_min_qty,
        purchase_max_qty,
        invoice_grouping_key,
        available_from,
        available_to,
        entitlement_grants,
        change_contract,
        lifecycle_state: _,
        created_by: _,
        created_at_utc: _,
        row_version: _,
        // Provenance, like the four above: a clone is an **ordinary** draft
        // (`inst-cl-draft`) taking the full pipeline and an approval on its first
        // publish, so no rule judges where it came from and the content pin does
        // not frame it. Ignored by name because D-259's destructure is what makes
        // that a decision rather than an omission.
        cloned_from: _,
    } = draft;

    let mut shape = PlanShape::new(plan_id, revision, now);
    shape.sku_id = sku_id;
    shape.billing_cycle = billing_cycle;
    shape.frequency = frequency;
    shape.plan_tier = plan_tier;
    shape.plan_name = plan_name;
    shape.plan_tier_override = plan_tier_override;
    shape.available_from = available_from;
    shape.available_to = available_to;
    shape.purchase_min_qty = purchase_min_qty;
    shape.purchase_max_qty = purchase_max_qty;
    shape.invoice_grouping_key = invoice_grouping_key;
    shape.change_contract = change_contract;
    shape.entitlement_grants = entitlement_grants;
    shape.phases = PhaseGraph::new(
        plan_shape_repo::load_phase_set(runner, scope, tenant_id, plan_id, revision)
            .await
            .map_err(|e| repo_failure(&e))?,
    );
    shape.addon_rules =
        plan_shape_repo::load_addon_rule_set(runner, scope, tenant_id, plan_id, revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    shape.descriptor_set =
        plan_shape_repo::load_descriptor(runner, scope, tenant_id, plan_id, revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    // Slice 10's composite definitions. **Without this line the two composite
    // rules iterate an empty vec and the content pin frames a count of zero**,
    // so a one-constituent or `vm -> pod -> vm` revision publishes unrejected and
    // a formula edit moves no digest -- which is precisely the hole D-256 and
    // `CONTENT_PIN_DOMAIN_SEP`'s v11 note both claim to close. Found by review
    // (D-257), and it is D-254's defect class exactly: a rule with an operand
    // nobody loads is a rule that always passes.
    // **The same omission, twice more** (D-258, found by running D-257's own lens
    // one field further). Both are authored on the draft revision, both are
    // framed by the content pin, and neither reached the shape:
    //
    // * `change_contract` left `ChangeGraphAuthorable` judging the default, so
    //   `CHANGE_TARGET_UNPUBLISHED` and `COMPARABILITY_RANK_REQUIRED` could never
    //   fire -- and `COMPARABILITY_RANK_REVOKED` fired **spuriously**, because
    //   inbound edges are read off the stored column independently of the shape,
    //   so any plan another published plan points at was refused publication even
    //   carrying a rank. That one is a live wrong answer, not only a silent gap.
    // * `entitlement_grants` left `GrantSetPhasesKnown` returning early on every
    //   publish and the pin framing the default, so the "approved a trial capped
    //   at 20, published 20 000, equal digest" hole `content_pin`'s own doc
    //   claims to close was open.
    shape.composites =
        plan_shape_repo::load_composite_set(runner, scope, tenant_id, plan_id, revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    // D-319's period floor/cap set, loaded for the same reason and stated here
    // rather than left implicit: both of its rules range over this vec, so an
    // unloaded set makes `PERIOD_FLOOR_CAP_MARKET_UNSOLD` and
    // `PERIOD_FLOOR_CAP_AMOUNT_INVALID` pass on every plan, and the content pin
    // frames an empty set — a reviewer approving a plan with no minimum and a
    // commit publishing one, at an equal digest. That is D-254's class and it
    // has landed four times in this function.
    shape.period_floor_caps =
        plan_shape_repo::load_period_floor_cap_set(runner, scope, tenant_id, plan_id, revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    // The candidate set: the shape as it will be after the commit.
    shape.rows = price_repo::load_for_plan(runner, scope, tenant_id, plan_id, CANDIDATE_ROW_STATES)
        .await
        .map_err(|e| repo_failure(&e))?;
    shape.windows = window_plane(runner, scope, tenant_id, plan_id, &shape.rows).await?;
    shape.baseline = published_baseline(runner, scope, tenant_id, plan_id).await?;
    Ok(shape)
}

/// Truncate to the millisecond quantum every authored instant is held to (D-144).
fn quantized(at: OffsetDateTime) -> OffsetDateTime {
    from_unix_millis(timestamp_millis(at)).unwrap_or(at)
}

/// Open a window on every billable key of this publish that has none (D-332).
///
/// # Why this is not a relaxation of the future-only rule
///
/// `inst-ws-future-start` exists against one attack: a `plan x write` holder
/// `POST`ing a window that starts sixty days ago, having the activation job pick
/// it up, and repricing open arrears periods — bypassing the two-person
/// `BackdateGrant` that S2 calls the only sanctioned backdating. A window opened
/// **here** starts at this commit's own instant, on a key that has no coverage
/// at all: there is no earlier interval to overwrite and no arrears period the
/// instant precedes. The route stays future-only; nothing a client can POST
/// changes.
///
/// # It writes `scheduled`, not `active`
///
/// The state machine is untouched: the activation sweep flips it, exactly as it
/// does for a window an operator scheduled, and `inst-wc-required` is satisfied
/// by "active **or** scheduled" either way. Writing `active` here would make
/// this the second thing that decides activation.
///
/// The uncovered set is `coverage::check_shape`'s own, not a second reading of
/// it: a private notion of "covered" here and the rule's there would drift, and
/// the one that publishes would be the one nobody tested.
async fn open_initial_windows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    shape: &PlanShape,
    actor_principal_id: Uuid,
    now: OffsetDateTime,
    correlation_id: Uuid,
) -> Result<(), DomainError> {
    let report = crate::domain::coverage::check_shape(shape);
    let uncovered: Vec<_> = report
        .required()
        .filter(|entry| !entry.has_live_window())
        // The same predicate the rule skips on, so the two cannot disagree
        // about which keys this publish is answering for.
        .filter(|entry| crate::domain::coverage::opened_by_this_publish(shape, entry.scope_key()))
        .map(|entry| entry.scope_key().clone())
        .collect();

    for key in uncovered {
        // The key came out of the shape's own billable set, so a row for it
        // exists; a `continue` rather than an error keeps this from being a
        // second place that decides what a publishable shape is.
        let Some(row) = shape.rows.iter().find(|record| record.scope_key == key) else {
            continue;
        };
        window_repo::schedule(
            runner,
            scope,
            window_repo::NewWindow {
                window_id: Uuid::now_v7(),
                tenant_id,
                price_id: row.price_id,
                // **Quantised to the millisecond** (D-144). `now` is the
                // commit's own instant and carries microseconds; an authored
                // instant may not, and `check_authored_instant` refuses one —
                // which surfaced as the publish failing on
                // `TIMESTAMP_PRECISION_EXCEEDED` after every rule had passed.
                effective_from: quantized(now),
                effective_to: None,
                // Named for what it is, so an operator reading the window plane
                // can tell the ones they scheduled from the one the publish
                // opened for them.
                reason_code: "initial coverage opened by publish (D-332)".to_owned(),
            },
            crate::domain::audit::AuditStamp {
                actor_principal_id,
                recorded_at: now,
                correlation_id,
            },
        )
        .await
        .map_err(|e| repo_failure(&e))?;
    }
    Ok(())
}

/// The plan's window plane, grouped per canonical scope key, seeded with the
/// candidate rows' keys.
///
/// **Read here rather than only at `GET …/coverage`** so the coverage report
/// appears in [`PublishService::precheck`] too. The pre-check is the operator's
/// remediation path: a plan that cannot publish for want of a window has to say
/// so before the two-person unit opens, or the first news of it is a refused
/// commit.
///
/// # Two filters this deliberately does not apply
///
/// `window_repo::list_for_plan` is taken whole — every window state, over every
/// price row of the plan whatever its lifecycle state — because the coverage
/// rules are **validation over a hypothetical**: "if this row were current on
/// this key, would the interval set be sound?", asked of a change set made of
/// `draft` rows about to publish. That is the same posture
/// `price_repo::load_scope_keys_for_plan` states for itself and the same posture
/// `window_repo::refuse_overlap` runs under.
///
/// The two readers that *assert facts to a consumer* — `read_model::project_windows`
/// and the activation sweep — restrict to
/// [`PROJECTED_ROW_STATES`](crate::domain::projection::PROJECTED_ROW_STATES)
/// instead, and that asymmetry is the point rather than an inconsistency to
/// resolve. A later group narrowing this read to match theirs would move the
/// publish check across that line, and a `draft` row's own window would stop
/// counting as the coverage of the key it is about to publish onto.
///
/// Which of the intervals then *contribute* coverage is each reader's own
/// statement: [`KeyWindows::coverage_end`] drops `cancelled`, and
/// [`crate::domain::coverage`] names the state set `inst-wc-required` and
/// `inst-fg-detect` read.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, or when a window names a price
/// row that is not on the plan.
async fn window_plane(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    candidates: &[PriceRecord],
) -> Result<Vec<KeyWindows>, DomainError> {
    let records = window_repo::list_for_plan(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    Ok(window::group_by_key_seeded(
        candidates.iter().map(|record| record.scope_key.clone()),
        records.into_iter().map(|w| {
            (
                w.scope_key,
                WindowInterval::new(w.effective_from, w.effective_to, w.state),
            )
        }),
    ))
}

/// What the plan's current revision holds, for the rules that compare
/// against it. `None` on a first publish.
async fn published_baseline(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<PublishedBaseline>, DomainError> {
    let Some(current) = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
    else {
        return Ok(None);
    };
    let terminal_phase_id = terminal_phase(runner, scope, tenant_id, plan_id, &current).await?;
    // The **published** plane alone. `PHASE_IN_USE` refuses dropping a phase
    // a current published row still references; taken off the candidate set
    // instead, a draft row this very revision authored could keep a phase
    // alive that nothing published uses.
    let published = price_repo::load_for_plan(
        runner,
        scope,
        tenant_id,
        plan_id,
        &[LifecycleState::Published],
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(Some(PublishedBaseline {
        terminal_phase_id,
        phase_ids_in_use: published
            .iter()
            .map(|record| record.scope_key.phase())
            .collect(),
        available_from: current.available_from,
        available_to: current.available_to,
    }))
}

/// The current revision's terminal phase.
///
/// A published revision has **exactly one**: `PHASE_GRAPH_INVALID` refused
/// anything else at its own publish, and `uq_pricing_plan_phase_terminal`
/// refuses a second one physically. So zero or two here is an invariant breach
/// rather than a caller mistake, and it surfaces as an internal fault instead of
/// silently yielding no baseline — a baseline quietly withheld is D-64's guard
/// switched off, which is precisely the "pass vacuously" behaviour
/// [`PublishedBaseline`] exists to prevent.
async fn terminal_phase(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    current: &PlanRevision,
) -> Result<PhaseId, DomainError> {
    let phases = PhaseGraph::new(
        plan_shape_repo::load_phase_set(runner, scope, tenant_id, plan_id, current.revision)
            .await
            .map_err(|e| repo_failure(&e))?,
    );
    let terminals = phases.terminals();
    match terminals.as_slice() {
        [only] => Ok(only.phase_id),
        _ => Err(DomainError::Internal(format!(
            "plan {plan_id} revision {} is current and holds {} terminal phases; \
             PHASE_GRAPH_INVALID permits exactly one",
            current.revision,
            terminals.len()
        ))),
    }
}

/// Ask the joint-conformance gate about **every** row of the publish unit.
///
/// The gate is a fail-closed **step** beside the report rather than a rule
/// inside it, and the doc on [`PublishService::precheck`] carries why. Its
/// refusal is the whole publish's refusal: one row whose shape nobody has agreed
/// how to evaluate makes the plan unpublishable, because the plan is what
/// becomes consumer-visible.
///
/// [`Reservation::of_slice3_row`] is asked at the call site rather than assumed,
/// which is the named hole that type exists to keep visible: a Slice-3 row
/// cannot say whether it reserves, and when Slice 10 lands that constructor
/// starts reading the row and this call site does not move.
///
/// `pub(crate)` for [`crate::infra::cutover`] (D-344). The gate guards acts whose
/// content is **authored** rather than derived, and a cutover's successor is
/// entirely client-authored — so it is the gate's second subject, not a second
/// gate. The repricing apply is deliberately not a third: its successor is
/// computed by `project_row` from a row whose own publish already passed here,
/// so it can present no shape this gate has not already seen.
///
/// # Errors
/// [`DomainError::FixtureMissing`] naming the kind and the variant that has no
/// green fixture.
pub(crate) fn check_fixtures(gate: &FixtureGate, shape: &PlanShape) -> Result<(), DomainError> {
    for record in &shape.rows {
        gate.check(&record.row, Reservation::of_slice3_row(&record.row))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod publish_tests;
