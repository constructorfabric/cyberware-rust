//! `BundleService` — `inst-ba-validate` and the publish half of
//! `inst-ba-material` / `inst-ba-return` (`design/08-bundles.md` §2).
//!
//! Three things the domain cannot do for itself, and nothing else:
//!
//! **Assemble.** [`crate::domain::bundle_rules`] is pure over a snapshot it is
//! handed, deliberately — §4.2 requires it, because the same rule set runs at
//! submit and again inside the publish commit where the world has moved. Somebody
//! has to do the reads, and this is that somebody. [`BundleService::assemble`]
//! resolves, per referenced component: has its plan published, is that plan
//! itself a bundle, does it carry a phase schedule beyond the D-19 implicit
//! terminal phase, and which coverage-eligible rows does it have.
//!
//! **Declare the act.** A bundle composition change is an
//! [`Trigger::BundleComposition`] and a rev-share change is a
//! [`Trigger::RevenueShareChange`] (D-104), and whether a call *is* one of those
//! is knowable only at the surface performing it —
//! `domain::materiality::triggers`' module doc makes exactly that argument.
//! [`composition_change_set`] and [`rev_share_change_set`] are where this slice
//! builds the declaration, and `evaluate` answers `alwaysMaterialTrigger` from it
//! **whatever a threshold policy says**, which is the whole of what D-104 buys.
//!
//! **A constructor is not a declaration until something calls it**, and for two
//! waves neither of the two was called: `publish_bundle` evaluated no verdict at
//! all (D-232), then evaluated [`composition_change_set`]'s unconditionally, so a rev-share
//! re-split and a component swap opened units that were byte-identical. [`declared_act`] is
//! what calls both — it diffs the composition being published against the last one that was
//! live and answers which of D-104's two acts the publish is — and
//! `BundleService::declared_publish_act` is the route's one question.
//!
//! **Naming the act is worth nothing unless the record can carry it**, which is why
//! D-321 refused to wire the second constructor: with `MaterialityVerdict` carrying
//! a reason and no trigger, both declarations render the same bytes. The verdict
//! carries the trigger, so this file's choice between the two is the difference
//! between an approval record that says *"what a vendor is paid moved"* and one that
//! says *"the composition moved"*.
//!
//! **Normalise at publish.** D-07's residual lands on the group's absorber and
//! the effective shares are written back summing to exactly 10000 bp. That is a
//! write, so it is not the reconciler's; the reconciler is the arithmetic and
//! [`BundleService::publish_composition`] is the transaction.
//!
//! # The coverage narrowing happens here, once
//!
//! `inst-bc-coverage` ranges over `priceEligibility = all_subscriptions`
//! (`cohort = none`) **published** rows only: grandfathered generations are never
//! coverage candidates (ADR-0002) and `new_subscriptions_only` rows are not
//! either — bundle composition demands the durable base, and a new-only promo row
//! expires with its intent. The filter is a `WHERE` clause in [`component_rows`]
//! and nowhere else, which is what keeps it from drifting away from the rules
//! written over it.
//!
//! # A phase schedule is "more than one phase row"
//!
//! D-19 auto-creates an **implicit terminal phase** (kind `evergreen`) on every
//! plan at creation, so "carries no authored schedule" is *exactly one* phase row
//! rather than zero. `COMPONENT_PHASED` fires on two or more. Reading it as
//! `> 0` would refuse every component in the catalogue.

use std::collections::BTreeSet;
use std::sync::Arc;

use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::bundle::{
    FULL_ALLOCATION_BP, PriceBasis, ReconciledGroup, RevShareGroup, reconcile,
};
use crate::domain::bundle_rules::{
    BundleComposition, ComponentDefect, ComponentSnapshot, CoverageRow, validate,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::ChangeSet;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::Frequency;
use crate::domain::publish::rules::ReferencingMarket;
use crate::domain::scope_key::{Cohort, PlanId, PriceEligibility, Region};
use crate::domain::validation::ValidationReport;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{
    bundle, bundle_component, bundle_revshare, plan, plan_phase, price,
};
use crate::infra::storage::repo::bundle_repo::{self, CompositionDraft};
use crate::infra::storage::repo::outbox_repo::BundleUpdatedPayload;
use crate::infra::storage::repo::plan_repo::{self, read_frequency};
use crate::infra::storage::repo::{
    BundleRepo, NewAuditEntry, NewOutboxEvent, audit_repo, outbox_repo,
};

/// The eligibility class a component contributes coverage from: the durable base.
///
/// Rendered by the enum that owns the axis rather than spelled again. Bare
/// literals — `"all_subscriptions"`, `"none"` — in a module that imports
/// [`LifecycleState`] and compares *that* axis through `as_str` three lines away
/// are the shape that goes stale: the reader on the same column goes through the
/// enum (`price_repo::read_eligibility` / `read_cohort`) and so does every other
/// filter on it, so a change to either rendering moves the writer and the readers
/// and leaves this query behind.
const COVERAGE_ELIGIBILITY: &str = PriceEligibility::AllSubscriptions.as_str();

/// The change set that says *"this call is a bundle composition change"*
/// (D-104, `inst-ba-material`).
///
/// No rows: a `sum_of_parts` recomposition carries **no price-row delta at all**,
/// which is precisely why D-104 had to register it — with a threshold configured
/// the `MaterialityEvaluator` saw nothing to trip on and a component swap reached
/// consumers with no approver, while a $1 price-row change above threshold took
/// two people.
#[must_use]
pub fn composition_change_set() -> ChangeSet {
    ChangeSet::of_act(Trigger::BundleComposition, [])
}

/// The change set for a rev-share, `price_basis` or `invoiceItemization` change
/// (D-104).
///
/// Separate from [`composition_change_set`] because D-104 registers two triggers
/// and a rev-share re-split *is* vendor payout, which an operator reading the
/// approval record should not have to infer from a trigger called "composition".
///
/// Its caller is [`declared_act`]. It had none from Slice 8 until
/// then, and the reason it was not simply given one is the point rather than the
/// history: until `MaterialityVerdict` could carry a trigger, this constructor and
/// [`composition_change_set`] produced a byte-identical response and a byte-identical
/// stored `materiality` document, so a call would have been observable to the trigger
/// census and to nothing else (D-232, D-321).
#[must_use]
pub fn rev_share_change_set() -> ChangeSet {
    ChangeSet::of_act(Trigger::RevenueShareChange, [])
}

/// Which of D-104's two acts a composition publish **is** — the diff that decision
/// registered two triggers in order to express.
///
/// `published` is the composition on the last revision of this plan that was ever
/// live, and `None` is *"none was"*. `candidate` is the composition the publish
/// would freeze.
///
/// # The rule, and why the narrow claim is the one made positively
///
/// [`Trigger::RevenueShareChange`] is answered **only** when the component set is
/// unchanged and the rev-share set moved. That is a claim an operator can act on —
/// *"what a vendor is paid moved, and what the customer receives did not"* — and it
/// is exactly the sentence D-232 said the record could not make. Everything else is
/// [`Trigger::BundleComposition`], which covers three worlds and states them here so
/// no reader has to infer which one a stored token came from:
///
/// - **no live predecessor** — the composition has never been anybody's, which is
///   D-104's own first clause, *"bundle creation"*;
/// - **the component set moved**, with or without the shares — a swap that also
///   re-splits is a swap, and reporting it as a re-split would hide the change to
///   what the customer receives behind the change to who is paid for it;
/// - **nothing moved at all** — a re-publish of an unchanged composition, which is a
///   legal act here (a revision may publish more than once) and is still material.
///
/// The precedence between the first two is not a coin toss. Both acts are always
/// material, so the choice moves no verdict and only a token; and of the two
/// mis-namings available, calling a component swap a re-split is the one that
/// misleads — it asserts the customer's side did not move. So the sharper claim is
/// made only when it is provably true, which is `compare`'s own rule in
/// `domain::materiality` (*"reported in the direction that names something the
/// operator can look at"*) applied to a naming rather than to a comparison.
///
/// # "Was live" is the plan revision's state, and that is the only signal there is
///
/// The composition is revision-scoped and carries no lifecycle of its own —
/// `pricing_bundle` holds no state column, *"a bundle's state is its plan
/// revision's"*, which the list `$filter=plan_id` contract states as a decision. So
/// [`plan_repo::load_live_predecessor`] reads the plan chain, and the baseline is
/// the composition riding the last revision of it that ever published.
///
/// The limit that follows is named rather than left to be discovered: a revision
/// can publish without [`BundleService::publish_composition`] having run over it,
/// so this baseline is *"the composition the last live revision carried"* and not
/// *"the composition a bundle publish last committed"*. There is no operand for the
/// second — that call records no `PendingVersionRow` and advances no
/// `CatalogVersion` (`inst-ba-return`'s read-model half is unbuilt, and
/// `publish_bundle` says so where it returns its 202), so nothing anywhere marks
/// one composition as the published one. Naming the revision is the honest
/// available answer, and it is what a consumer resolving the plan would have seen.
///
/// # What this diff does **not** see, and it is already owned
///
/// D-104 puts `price_basis` and `invoiceItemization` under the rev-share trigger,
/// and neither is here: both live on `pricing_bundle`, which is keyed by
/// `bundle_id` alone and carries no revision, so there is no per-revision value to
/// compare. **D-206 is the entry that owns that consequence** and has already
/// decided the fix (split identity from content, the content onto a revision-scoped
/// carrier), and it also records the fact that makes this a non-issue today rather
/// than a hole: *"the only writer of either column is `BundleRepo::create`, and no
/// mounted route mutates them"*. So the two fields cannot move at all, and a diff
/// that cannot see them is not missing anything that can change. The day D-206's
/// carrier lands they join this comparison, and
/// `bundle_tests::the_declared_act_reads_only_what_a_revision_can_carry` is where a
/// reader meets that.
///
/// # Order, not set semantics
///
/// [`CompositionDraft`] is compared with `==`, which compares two `Vec`s
/// element-wise. That is honest here and would not be in general:
/// `bundle_repo::read_composition` orders components by `component_plan_id`, groups
/// by `vendor_sku_id` and parties by `(vendor_sku_id, party)` on **both** sides of
/// this comparison, so two readings of one composition are one value. It is
/// deliberately not re-sorted here — a normalisation in this function would let the
/// repository's ordering drift without anything noticing, and the ordering is
/// load-bearing for other readers too.
///
/// The **authored** shares are what is compared, never the effective ones:
/// `publish_composition` normalises onto `effective_share_bp`, a different column
/// that `read_composition` does not read. Were it otherwise, publishing a
/// composition would move the very value the next publish diffs against, and every
/// second publish would report a rev-share change that no operator authored.
#[must_use]
pub fn declared_act(
    published: Option<&CompositionDraft>,
    candidate: &CompositionDraft,
) -> ChangeSet {
    let Some(published) = published else {
        return composition_change_set();
    };
    if published.components == candidate.components
        && published.rev_share_groups != candidate.rev_share_groups
    {
        return rev_share_change_set();
    }
    composition_change_set()
}

/// The composition service.
#[derive(Clone)]
pub struct BundleService {
    db: DBProvider<DbError>,
    bundles: BundleRepo,
    metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
}

impl BundleService {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self {
            db: db.clone(),
            bundles: BundleRepo::new(db),
            // The safe default, for `PublishService::new`'s reason: a service
            // built without one reports nothing rather than failing to build.
            metrics: Arc::new(crate::domain::ports::metrics::NoopPricingMetrics),
        }
    }

    /// The change set a publish of `revision` declares — [`declared_act`]'s operand
    /// resolution, and the only thing the route has to ask for.
    ///
    /// Two reads: the composition being published, and the composition on the last
    /// revision of this plan that was ever live
    /// ([`plan_repo::load_live_predecessor`]). They go through **one connection**
    /// so a reader of the two answers is reading one moment; they are deliberately
    /// **not** in a transaction, because this is a classification made before the
    /// publish's own mutating work and D-176's rule applies in the other direction
    /// here — a composition cannot move under it, `replace_composition_on` refusing
    /// every revision that is not a `draft` and the predecessor being, by
    /// construction, not one.
    ///
    /// **It is a read of two revisions and not a before-image**, which is what makes
    /// it constructible at all: the composition is revision-scoped (D-92, D-105), so
    /// "what was live before" is a row set that still exists rather than a value
    /// somebody had to have recorded. That is the difference between this and the
    /// operand D-327 declared unrecordable for the cutover's shorten, and it is why
    /// D-232's second half was buildable and that one was not.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
    /// for a stored absorber, party or revision value no `CHECK` should have
    /// admitted.
    pub async fn declared_publish_act(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<ChangeSet, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("bundle act conn: {e}")))?;
        let candidate =
            bundle_repo::load_composition_on(&conn, scope, tenant_id, plan_id, revision).await?;
        let published =
            match plan_repo::load_live_predecessor(&conn, scope, tenant_id, plan_id, revision)
                .await?
            {
                None => None,
                Some(predecessor) => Some(
                    bundle_repo::load_composition_on(
                        &conn,
                        scope,
                        tenant_id,
                        plan_id,
                        predecessor.revision,
                    )
                    .await?,
                ),
            };
        Ok(declared_act(published.as_ref(), &candidate))
    }

    /// Assemble one revision's composition into the snapshot the rules read.
    ///
    /// `markets` is the set of `(currency, region)` the bundle sells in, which
    /// for a `sum_of_parts` bundle cannot be read off its own rows — it has none
    /// (`inst-bb-rowless`) — so the caller supplies it. For `own_price` the
    /// bundle's own rows are read here and are in the tax-basis set.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when the plan carries no bundle;
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] for a stored token no `CHECK` should have
    /// admitted.
    pub async fn assemble(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        markets: Vec<(CurrencyCode, Region)>,
    ) -> Result<BundleComposition, RepoError> {
        let Some(record) = self.bundles.find_by_plan(scope, tenant_id, plan_id).await? else {
            return Err(RepoError::NotFound {
                subject: "bundle".to_owned(),
                id: plan_id.get().to_string(),
            });
        };
        let draft = self
            .bundles
            .load_composition(scope, tenant_id, plan_id, revision)
            .await?;
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("bundle service conn: {e}")))?;

        let mut components = Vec::with_capacity(draft.components.len());
        for component in &draft.components {
            let component_plan = PlanId::new(component.component_plan_id);
            components.push(ComponentSnapshot {
                component_plan_id: component.component_plan_id,
                included_sku_id: component.included_sku_id,
                defects: component_defects(&conn, scope, tenant_id, component_plan).await?,
                frequency: component_frequency(&conn, scope, tenant_id, component_plan).await?,
                rows: component_rows(&conn, scope, tenant_id, component_plan).await?,
            });
        }

        // `sum_of_parts` carries no own rows at all; reading them anyway would
        // put an empty set where the rule expects one and cost a query per
        // publish for a value that cannot exist.
        let own_rows = if record.price_basis == PriceBasis::OwnPrice {
            component_rows(&conn, scope, tenant_id, plan_id).await?
        } else {
            Vec::new()
        };

        Ok(BundleComposition {
            bundle_id: record.bundle_id,
            basis: record.price_basis,
            markets,
            components,
            own_rows,
            rev_share_groups: draft.rev_share_groups,
        })
    }

    /// `inst-ba-validate`: run the whole rule set over an assembled composition.
    ///
    /// A thin forward, and deliberately so — the rules are the domain's and this
    /// is only where they are reached from. Kept as a method rather than left to
    /// callers because the pair *assemble then validate* is the contract, and a
    /// caller free to validate something it assembled differently is a caller
    /// free to publish an unvalidated composition.
    ///
    /// # Errors
    /// Whatever [`Self::assemble`] refuses with.
    pub async fn validate_publish(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        markets: Vec<(CurrencyCode, Region)>,
    ) -> Result<ValidationReport, RepoError> {
        let composition = self
            .assemble(scope, tenant_id, plan_id, revision, markets)
            .await?;
        let report = validate(&composition);
        // §10's counter, cases (ii) and (iii) (`T-17`). **One run, not two**, and
        // that is a real difference from the plan plane: `report_market_metrics`
        // is called from a pre-check *and* a commit because the publish route's
        // approved arm reaches the commit without pre-checking. The bundle
        // publish has no such arm — `bundles::publish_bundle` calls this and then
        // `publish_composition`, in that order, always — so counting here counts
        // every block exactly once.
        crate::infra::metrics::report_bundle_coverage_metrics(
            &*self.metrics,
            composition.basis,
            &report,
        );
        Ok(report)
    }

    /// Attach the metrics port (`T-17`).
    ///
    /// A **second call** rather than a parameter on [`Self::new`], for
    /// `PublishService::with_metrics`' reason and to the same effect: every
    /// existing caller has nothing to say to it, and the no-op [`Self::new`]
    /// installs is exactly what a test harness means.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    /// Normalise the revision's rev-share onto its absorbers and emit
    /// `BundleUpdated`, in one transaction (`inst-rs-residual`,
    /// `inst-ba-return`).
    ///
    /// The two are one transaction because the event says the composition
    /// changed and the normalisation is what changed it: an event that could
    /// commit separately from the write it describes is evidence of something
    /// that may not have happened — the outbox's own contract.
    ///
    /// # `inst-ba-return`'s read-model half is **not** built here, and nothing
    /// else builds it either
    ///
    /// That instruction's third clause is *"composition frozen into the read
    /// model / snapshot"*. This method records **no**
    /// [`PendingVersionRow`](crate::infra::storage::repo::PendingVersionRow), so
    /// no `CatalogVersion` advances and no subject re-projects — and there would
    /// be nothing to re-project into, because
    /// [`PlanSubjectDelta`](crate::domain::projection::PlanSubjectDelta) carries
    /// no bundle member. Every other publish unit in this gear records one
    /// (`publish`, `window`, `supersession`, `cutover`, `overlay_publish`,
    /// `retirement`, `membership_publish`); this act is the only one that does
    /// not, and `bundle_sellability_tests`'
    /// `a_composition_publish_advances_no_catalog_version` is what holds the
    /// absence rather than leaving it to be rediscovered.
    ///
    /// **It is reported here because of what it costs one surface over.**
    /// `inst-sg-bundle` and `dod-sellability` both require the sellability
    /// surface to expose the *frozen* component key set and walk it; with no
    /// version carrying a composition there is nothing to freeze it into, which
    /// is why [`crate::domain::bundle_sellability`] is built, tested and
    /// uncalled.
    ///
    /// Callers reach this only with a publishable report;
    /// [`Self::validate_publish`] is what produces one, and a group that would
    /// refuse here has already refused there. The refusal is re-derived rather
    /// than trusted, because §4.2 has the rule set run twice for exactly this
    /// reason.
    ///
    /// # One revision may publish **more than once**
    ///
    /// Unlike a plan publish, and it is the premise the announcement's dedup key
    /// used to contradict: the writes are idempotent — the same composition
    /// re-normalises to the same effective shares — and a composition edit moves
    /// the plan revision's `row_version`, which voids the content pin and sends
    /// the operator back through a second approval **over the same revision**. So
    /// the second call is a legal act, and it is answered rather than refused;
    /// see the dedup key below for what keys the announcement instead.
    ///
    /// That is also why `expected` is **compared and not bumped**. Advancing the
    /// version here would move the pin under the act's own approved unit, so the
    /// legal second call would be refused by the surface that just authorised the
    /// first — the property this section exists to state.
    ///
    /// # `expected` is the version the reviewer's pin was taken over
    ///
    /// "Nothing here is a compare-and-swap" is not harmless on the ground that the
    /// writes are idempotent. They are idempotent **for one composition**, and the
    /// composition is read on a third connection after the surface has already
    /// matched the approved unit. The sequence that matters is not a lost update, it
    /// is an approval bypass:
    ///
    /// 1. the surface reads the revision at `row_version` *v* and computes
    ///    `bundle_content_hash(shape, v)`;
    /// 2. `approvals::approved_unit` matches a second principal's decision over
    ///    that digest;
    /// 3. a `PATCH …/bundles/{id}` replaces the composition, taking the revision
    ///    to *v + 1*;
    /// 4. this function reads the **new** composition, reconciles it, and writes
    ///    its effective shares — money split between vendor parties — while
    ///    `BundleUpdated` announces a composition nobody reviewed.
    ///
    /// Step 3 is an ordinary authorised edit, not a race an operator has to
    /// contrive. `expected` closes it: the revision is re-read **inside the
    /// writing transaction** and a version that has moved is
    /// [`RepoError::StaleRowVersion`], which is the contention refusal the act
    /// previously reported as `CorruptRow` when the divergence happened to strand
    /// a party row and reported not at all when it did not.
    ///
    /// **What it does not close, stated rather than implied.** The compare is a
    /// read and the store's isolation is what serialises it against
    /// `replace_composition_on`'s own swap; this is D-176's posture — read inside
    /// the transaction that writes — and not a row lock. What it converts is a
    /// silent wrong write into a refusal on every interleaving the store does
    /// serialise, which is the same guarantee the other seven publish units have.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when the plan carries no bundle or no such
    /// revision, and — Z9-6 — when a reconciled share addresses no stored party
    /// row, which rolls the whole transaction back so no `BundleUpdated`
    /// announces a normalisation that did not land;
    /// [`RepoError::StaleRowVersion`] when the composition moved after the pin was
    /// taken; [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a group refuses reconciliation at commit
    /// time — which is a state the pre-check should have caught, so it is
    /// reported as a corrupt read rather than as a caller error.
    #[allow(
        clippy::too_many_arguments,
        reason = "the plan coordinate the act addresses, the version its pin was taken over, the \
                  scope and tenant every read is bounded by, and the stamp the audit record and \
                  the announcement are both written from. `expected` is the one argument that \
                  cannot be derived here: it is the surface's read, the digest the reviewer \
                  decided over was computed from it, and re-reading it inside would be a second \
                  answer to the question the compare exists to ask"
    )]
    pub async fn publish_composition(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        stamp: AuditStamp,
    ) -> Result<(), RepoError> {
        let scope = scope.clone();
        let correlation_id = stamp.correlation_id;
        let at = stamp.recorded_at;
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<(), RepoError, _>(move |txn| {
                Box::pin(async move {
                    // **Every read this act judges from, inside the transaction that
                    // writes** (D-176). Three of them will run on the service's own
                    // connection before it opens unless they are held here, where
                    // "a comparison made before it opened is a hint and not a
                    // precondition". The seven sibling publish units all read here
                    // and cite the same decision.
                    let Some(record) =
                        bundle_repo::find_by_plan_on(txn, &scope, tenant_id, plan_id).await?
                    else {
                        return Err(RepoError::NotFound {
                            subject: "bundle".to_owned(),
                            id: plan_id.get().to_string(),
                        });
                    };
                    let bundle_id = record.bundle_id;

                    // The version the reviewer's digest was taken over, asked of the
                    // row **before** the composition is read, so a composition that
                    // moved is named as contention rather than reconciled and paid
                    // out. See this function's own doc for the four-step sequence.
                    let current =
                        plan_repo::load_revision(txn, &scope, tenant_id, plan_id, revision)
                            .await?
                            .ok_or_else(|| RepoError::NotFound {
                                subject: "plan revision".to_owned(),
                                id: format!("{plan_id}/{revision}"),
                            })?;
                    if current.row_version != expected {
                        return Err(RepoError::StaleRowVersion {
                            subject: "bundle composition".to_owned(),
                            id: format!("{plan_id}/{revision}"),
                            current: current.row_version.get(),
                            submitted: expected.get(),
                        });
                    }

                    let draft =
                        bundle_repo::load_composition_on(txn, &scope, tenant_id, plan_id, revision)
                            .await?;
                    let mut reconciled_groups = Vec::new();
                    for group in &draft.rev_share_groups {
                        let reconciled = reconcile(group).map_err(|refusal| {
                            RepoError::CorruptRow(format!(
                                "bundle {} vendor {} does not reconcile at commit: {}",
                                bundle_id,
                                group.vendor_sku_id,
                                refusal.code()
                            ))
                        })?;
                        // **D-07's promise, asked rather than trusted, at the write**.
                        // `ReconciledGroup::sums_to` exists for exactly this — its own doc says
                        // "a caller writing the values back should be able to check it without
                        // re-deriving the sum" — and the only code that asked was three unit
                        // tests. It is unreachable by construction today: `reconcile` returns
                        // `authored + residual`, which is `FULL_ALLOCATION_BP` by definition.
                        // That is the point. A future edit to `reconcile` breaking it would be
                        // caught by the unit tests and not by the transaction that persists the
                        // wrong split, which is the opposite of what the helper was built for.
                        //
                        // A hard refusal and not a `debug_assert!`: production is a
                        // release build and this is the last read of these numbers before
                        // they become the stored allocation.
                        let sum = reconciled.sums_to();
                        if sum != FULL_ALLOCATION_BP {
                            return Err(RepoError::CorruptRow(format!(
                                "bundle {} vendor {} reconciles to {sum} bp, not \
                                 {FULL_ALLOCATION_BP}",
                                bundle_id, group.vendor_sku_id,
                            )));
                        }
                        reconciled_groups.push(reconciled);
                    }

                    let Ok(number) = i64::try_from(revision) else {
                        return Err(RepoError::CorruptRow(format!(
                            "bundle {bundle_id} revision {revision} exceeds the storable range"
                        )));
                    };
                    for group in &reconciled_groups {
                        for (party, effective) in &group.effective_shares {
                            write_effective_share(
                                txn,
                                &scope,
                                bundle_id,
                                tenant_id,
                                number,
                                group.vendor_sku_id,
                                party.get(),
                                *effective,
                            )
                            .await?;
                        }
                    }

                    // **Inside the transaction that wrote the shares**, and this is
                    // the act D-135's ordering matters most for: appended afterwards,
                    // a crash between the two leaves money split between third
                    // parties with no record of who split it. The seven sibling
                    // publish units append here for the same reason.
                    //
                    // Both sides of the split are carried, because what an auditor
                    // has to be able to answer is *what D-07 moved*: the authored
                    // shares, and the effective ones the residual was absorbed into.
                    // A record carrying one side cannot say.
                    audit_repo::append(
                        txn,
                        &scope,
                        NewAuditEntry {
                            tenant_id,
                            chain_id: audit_repo::plan_chain(plan_id),
                            recorded_at: stamp.recorded_at,
                            actor_principal_id: stamp.actor_principal_id,
                            action: AuditAction::Publish,
                            subject_kind: AuditSubjectKind::PlanRevision,
                            subject_ref: audit_repo::bundle_ref(bundle_id),
                            before_state: Some(authored_split(&draft.rev_share_groups)),
                            after_state: Some(effective_split(&reconciled_groups)),
                            approval_ref: None,
                            correlation_id: stamp.correlation_id,
                        },
                    )
                    .await?;
                    // The name, the aggregate and the key are the module's, never
                    // this call site's — Z8-4. What that buys is
                    // `bundle_updated_dedup_key`, whose doc carries the whole
                    // argument: the key is **the act's**, because a revision may
                    // publish more than once and a revision-scoped key denies it.
                    outbox_repo::enqueue(
                        txn,
                        &scope,
                        NewOutboxEvent::bundle_updated(
                            tenant_id,
                            &BundleUpdatedPayload {
                                bundle_id,
                                plan_id,
                                plan_revision: revision,
                                price_basis: record.price_basis,
                                invoice_itemization: record.invoice_itemization,
                                correlation_id,
                            },
                            at,
                        ),
                    )
                    .await?;
                    Ok(())
                })
            })
            .await;
        outcome.map_err(|e| {
            e.into_domain(|infra| RepoError::Db(format!("bundle publish transaction: {infra}")))
        })
    }
}

/// The authored split, as the audit record's `before_state` holds it.
///
/// **Not `domain::audit::subject_state`**, which renders a row's lifecycle state
/// and entity tag: what changed here is a division of money between third
/// parties, and neither of those columns moves. Rendering it through the shared
/// shape would file a record whose two states are identical.
///
/// Wire keys `camelCase`, as every other audited state and every outbox payload.
fn authored_split(groups: &[RevShareGroup]) -> serde_json::Value {
    serde_json::json!({
        "revShareGroups": groups
            .iter()
            .map(|group| serde_json::json!({
                "vendorSkuId": group.vendor_sku_id.to_string(),
                "platformCutBp": group.platform_cut_bp,
                "parties": group
                    .parties
                    .iter()
                    .map(|share| serde_json::json!({
                        "party": share.party.get(),
                        "shareBp": share.share_bp,
                    }))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// The reconciled split, as the audit record's `after_state` holds it.
///
/// `adjustmentBp` and `effectivePlatformCutBp` are carried beside the party
/// shares because D-07 requires the absorbed residual to be **auditable** rather
/// than merely applied, and the party shares alone do not say which absorber took
/// it or how much.
fn effective_split(groups: &[ReconciledGroup]) -> serde_json::Value {
    serde_json::json!({
        "revShareGroups": groups
            .iter()
            .map(|group| serde_json::json!({
                "vendorSkuId": group.vendor_sku_id.to_string(),
                "adjustmentBp": group.adjustment_bp,
                "effectivePlatformCutBp": group.effective_platform_cut_bp,
                "parties": group
                    .effective_shares
                    .iter()
                    .map(|(party, effective)| serde_json::json!({
                        "party": party.get(),
                        "effectiveShareBp": effective,
                    }))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// The reads the rules cannot make for themselves.
// ---------------------------------------------------------------------------

/// Everything disqualifying about one component plan.
///
/// One function rather than three predicates, so the set the rules receive is
/// built in one place and a fourth disqualifier has one home.
async fn component_defects(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    component_plan_id: PlanId,
) -> Result<std::collections::BTreeSet<ComponentDefect>, RepoError> {
    let mut defects = std::collections::BTreeSet::new();

    // Published means a revision of this plan is `published`. `retired` is not
    // published for composition purposes — a retired component is one Slice 11
    // has already taken out of sale.
    let published = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(component_plan_id.get()))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read component plan state: {e}")))?;
    if published == 0 {
        defects.insert(ComponentDefect::Unpublished);
    }

    let is_bundle = bundle::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle::Column::TenantId.eq(tenant_id))
                .add(bundle::Column::PlanId.eq(component_plan_id.get())),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read component bundle flag: {e}")))?;
    if is_bundle > 0 {
        defects.insert(ComponentDefect::IsBundlePlan);
    }

    // See the module doc: D-19's implicit terminal phase means "no authored
    // schedule" is exactly one row.
    let phases = plan_phase::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_phase::Column::TenantId.eq(tenant_id))
                .add(plan_phase::Column::PlanId.eq(component_plan_id.get())),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read component phases: {e}")))?;
    if phases > 1 {
        defects.insert(ComponentDefect::Phased);
    }

    Ok(defects)
}

/// A component's recurring frequency, or `None` when it is usage-only.
///
/// Read off the plan's `frequency` column. A plan with none is usage-only for
/// `inst-bc-frequency`'s purposes (L-8), which is the same thing the column
/// says: a frequency is what a recurring cycle has.
async fn component_frequency(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    component_plan_id: PlanId,
) -> Result<Option<Frequency>, RepoError> {
    let row = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(component_plan_id.get()))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read component frequency: {e}")))?;
    let Some(row) = row else {
        return Ok(None);
    };
    // `plan_repo`'s reader, not a second one: it refuses the half-set interval
    // pairings the CHECK cannot see, and a component's frequency has to mean
    // exactly what its own plan's does — `inst-bc-frequency` compares the two.
    read_frequency(&row)
}

/// One plan's coverage-eligible published rows.
///
/// The narrowing is here and nowhere else; see the module doc.
async fn component_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Vec<CoverageRow>, RepoError> {
    let rows = price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PlanId.eq(plan_id.get()))
                .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str()))
                .add(price::Column::PriceEligibility.eq(COVERAGE_ELIGIBILITY))
                // The non-grandfathered generation (ADR-0002), rendered by
                // `Cohort` and not spelled: `Display` is the writer's
                // rendering (`price_repo`'s `cohort.to_string()`), and it is
                // not `const`, so it lives at the call site rather than in a
                // constant above.
                .add(price::Column::Cohort.eq(Cohort::None.to_string())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read component coverage rows: {e}")))?;

    let mut coverage = Vec::with_capacity(rows.len());
    for row in rows {
        let currency = CurrencyCode::new(&row.currency).map_err(|e| {
            RepoError::CorruptRow(format!(
                "price {} carries an unusable currency: {e}",
                row.price_id
            ))
        })?;
        let region = Region::new(&row.region).map_err(|e| {
            RepoError::CorruptRow(format!(
                "price {} carries an unusable region: {e}",
                row.price_id
            ))
        })?;
        coverage.push(CoverageRow {
            currency,
            region,
            tax_inclusive: row.tax_inclusive,
        });
    }
    Ok(coverage)
}

/// The bundle markets a **component plan's** publish is judged against
/// (`inst-bc-taxbasis`'s reverse half; D-119, homed by D-212).
///
/// The read the pure publish walk cannot do and must not do. It answers *"which
/// bundles sell this plan, and on what tax display basis does each of their
/// markets already stand"*, so that
/// `domain::publish::rules::BundleMarketBasisUnmixed` can compare this publish's
/// candidate rows against it. The index it rides is
/// `idx_pricing_bundle_component_plan` on `(tenant_id, component_plan_id)`, added
/// by `pricing_bundle_component` for exactly this shape of question — S11's
/// `inst-re-references` asks the mirror one for retirement.
///
/// # Three narrowings, each of them a decision rather than an optimisation
///
/// **Only the referencing bundle's *current published* revision counts** (D-212).
/// `pricing_bundle_component` is revision-scoped, so the same plan may appear in
/// several revisions of one bundle; a draft revision's component set is not yet
/// anybody's truth, and guarding against it would fail a publish over a
/// composition no consumer can resolve.
///
/// **The basis is the *other* members'**, never the publishing plan's own rows —
/// those are the thing being judged, and folding them in would make the
/// comparison a tautology on every market where this plan is the only priced
/// member.
///
/// **Where the other members already disagree among themselves, the first is
/// taken and nothing more is said.** That bundle fails its own publish on the
/// forward half; reporting it again here would send an operator to repair a
/// market whose fault is not the one they are publishing.
///
/// A plan that is a component of nothing costs **one index probe returning no
/// rows**, which is the common case by a wide margin.
///
/// # Errors
///
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] for
/// a stored token no `CHECK` should have admitted.
pub async fn referencing_markets(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    component_plan_id: PlanId,
) -> Result<Vec<ReferencingMarket>, RepoError> {
    let memberships = bundle_component::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle_component::Column::TenantId.eq(tenant_id))
                .add(bundle_component::Column::ComponentPlanId.eq(component_plan_id.get())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read bundles referencing a component: {e}")))?;

    let mut bundle_ids: BTreeSet<Uuid> = BTreeSet::new();
    for row in &memberships {
        bundle_ids.insert(row.bundle_id);
    }

    let mut markets = Vec::new();
    for bundle_id in bundle_ids {
        let Some(record) = bundle_row(runner, scope, tenant_id, bundle_id).await? else {
            // The row went while we read; the composition it named is not
            // resolvable, so there is no market to be judged against.
            continue;
        };
        let bundle_plan = PlanId::new(record.plan_id);
        let Some(current) = plan_repo::load_current(runner, scope, tenant_id, bundle_plan).await?
        else {
            continue;
        };
        // Refused rather than saturated, as `retirement::current_revision` refuses
        // the identical conversion on the mirror question. `i64::MAX` is a value the
        // membership test three lines down can never match, so the saturation read as
        // "this plan is a component of some other revision" and stopped judging the
        // bundle's markets — a guard disabled by an arithmetic fallback.
        let revision = i64::try_from(current.revision).map_err(|_| {
            RepoError::CorruptRow(format!(
                "pricing_plan row for plan {bundle_plan} holds revision {} \
                 outside the storable range",
                current.revision
            ))
        })?;
        if !memberships
            .iter()
            .any(|row| row.bundle_id == bundle_id && row.plan_revision == revision)
        {
            // This plan is a component of some *other* revision of that bundle,
            // not of the one consumers resolve against.
            continue;
        }

        let basis = PriceBasis::parse(&record.price_basis).ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_bundle {bundle_id} carries an unknown price_basis `{}`",
                record.price_basis
            ))
        })?;
        let mut rows: Vec<CoverageRow> = Vec::new();
        if basis == PriceBasis::OwnPrice {
            rows.extend(component_rows(runner, scope, tenant_id, bundle_plan).await?);
        }
        // Every component of that revision *except* the one publishing.
        for other in siblings_of(runner, scope, tenant_id, bundle_id, revision).await? {
            if other == component_plan_id {
                continue;
            }
            rows.extend(component_rows(runner, scope, tenant_id, other).await?);
        }

        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        for row in rows {
            let market = (
                row.currency.as_str().to_owned(),
                row.region.as_str().to_owned(),
            );
            if seen.insert(market) {
                markets.push(ReferencingMarket::new(
                    bundle_id,
                    row.currency,
                    row.region,
                    row.tax_inclusive,
                ));
            }
        }
    }
    Ok(markets)
}

/// One bundle row by id.
async fn bundle_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    bundle_id: Uuid,
) -> Result<Option<bundle::Model>, RepoError> {
    bundle::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle::Column::TenantId.eq(tenant_id))
                .add(bundle::Column::BundleId.eq(bundle_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_bundle by id: {e}")))
}

/// Every component plan of one bundle revision, **in a fixed order**.
///
/// Sorted by plan id, and that is correctness rather than tidiness: the caller
/// takes the *first* basis it sees per market, so an unsorted read would make the
/// resolved basis depend on the order the store happened to return rows in — two
/// engines, or one engine after a vacuum, could answer differently about the same
/// composition. A probe found this: excluding the publishing plan from the set
/// reddened nothing, because the sibling happened to be read first, and the guard
/// would have compared this plan against **itself** whenever the order flipped.
async fn siblings_of(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    bundle_id: Uuid,
    revision: i64,
) -> Result<Vec<PlanId>, RepoError> {
    let rows = bundle_component::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle_component::Column::TenantId.eq(tenant_id))
                .add(bundle_component::Column::BundleId.eq(bundle_id))
                .add(bundle_component::Column::PlanRevision.eq(revision)),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read a bundle revision's components: {e}")))?;
    let mut plans: Vec<PlanId> = rows
        .into_iter()
        .map(|row| PlanId::new(row.component_plan_id))
        .collect();
    plans.sort_unstable_by_key(|plan| plan.get());
    Ok(plans)
}

/// Write one party's normalised effective share.
///
/// **The zero-row outcome is a refusal, not a success**. `update_many`
/// answers `Ok` for a filter that matched nothing, and dropping its
/// `rows_affected` made that outcome indistinguishable from a write: the stored
/// `effective_share_bp` stayed at whatever it was, `publish_composition` returned
/// `Ok(())`, and the `BundleUpdated` in the same transaction announced a
/// composition whose reconciled shares were never stored. Downstream parties are
/// paid on this column, so a normalisation that addressed no row has to be told
/// rather than announced — the same reading `repricing_journal_repo::mark_applied`
/// and `mark_failed` take of their own zero-row case.
///
/// The read behind the write is `bundle_repo::load_composition`, on a **separate**
/// connection and before the transaction opens, so the state it described is not
/// the state this statement runs against: a concurrent composition replace or a
/// `plan_repo::abandon_draft` between the two leaves exactly this filter matching
/// nothing. `chk_pricing_bundle_revshare_party` trims on the same character set
/// `Party::new` does, so a party this statement cannot address is one whose padding
/// is non-ASCII whitespace — the residue stated on `pricing_region_taxonomy`'s
/// migration, and reachable only by a writer that goes around this repository.
///
/// # Errors
/// [`RepoError::NotFound`] naming the four-column key when no row answers to it;
/// [`RepoError::Db`] on a scope or storage failure.
#[allow(
    clippy::too_many_arguments,
    reason = "the row's whole four-column primary key plus the tenant, the scope \
              and the value; every one of them addresses the row and none is \
              derivable from the others"
)]
async fn write_effective_share(
    runner: &impl DBRunner,
    scope: &AccessScope,
    bundle_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
    vendor_sku_id: Uuid,
    party: &str,
    effective_share_bp: i32,
) -> Result<(), RepoError> {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;

    let affected = bundle_revshare::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            bundle_revshare::Column::EffectiveShareBp,
            Expr::value(effective_share_bp),
        )
        .filter(
            Condition::all()
                .add(bundle_revshare::Column::BundleId.eq(bundle_id))
                .add(bundle_revshare::Column::TenantId.eq(tenant_id))
                .add(bundle_revshare::Column::PlanRevision.eq(revision))
                .add(bundle_revshare::Column::VendorSkuId.eq(vendor_sku_id))
                .add(bundle_revshare::Column::Party.eq(party)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("write effective share: {e}")))?
        .rows_affected;
    if affected == 0 {
        return Err(RepoError::NotFound {
            subject: "bundle rev-share party row".to_owned(),
            // The whole filter, because any one of the four could be the axis
            // that missed and an operator cannot tell which from a bundle id.
            id: format!("{bundle_id}/{revision}/{vendor_sku_id}/{party}"),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "bundle_tests.rs"]
mod bundle_tests;
