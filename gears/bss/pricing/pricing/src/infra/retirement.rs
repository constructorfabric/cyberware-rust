//! What refers to a plan, read for the retirement guard (`inst-re-references`).
//!
//! `domain::retirement::ReferenceReport` decides whether a retirement may
//! proceed; this module is where the report's contents come from. The split is
//! the usual one — the judgement is a pure function with cases of its own, and
//! the reads are here because they are queries against a world the domain layer
//! may not know about.
//!
//! # Two blocking classes, and the narrowing that makes them true
//!
//! **Bundle components** ride `idx_pricing_bundle_component_plan`
//! (`(tenant_id, component_plan_id)`), the index that exists for exactly this shape
//! of question — `infra::bundle::referencing_markets` asks its
//! forward half. Only a bundle's **current published revision** counts, for
//! D-212's reason restated one direction over: `pricing_bundle_component` is
//! revision-scoped, so one plan may appear in several revisions of one bundle,
//! and a draft revision's component set is nobody's truth yet. Blocking a
//! retirement on a composition no consumer can resolve would refuse an operator
//! an act nothing depends on.
//!
//! **Add-on price-override targets** are the `pricing_plan_addon_rule` rows whose
//! `price_override_ref` names a **price row of the retiring plan**. The column
//! holds a price id rather than a plan id, so the question is asked in two steps:
//! the retiring plan's price ids, then the rules pointing at any of them. Same
//! revision narrowing, same reason.
//!
//! # What this module deliberately cannot see, and it is not an omission
//!
//! **`allowedChangeTargets` (D-24) has no store in this gear.** Nothing persists
//! it — no column, no table, no projection — so the warning class
//! `WarningReferenceKind::AllowedChangeTarget` exists in the domain vocabulary
//! and has no producer here. That is reported rather than smoothed over: the
//! alternative is a dry-run that silently claims no plan lists the retiree as a
//! change target when the truth is that nobody ever wrote it down. The class
//! stays in the domain type because the refusal it belongs to is decided, and a
//! producer landing later needs no change to the judgement.
//!
//! **Overlay targets (D-31) are not read here either.** `pricing_price_overlay`
//! carries them inside a `jsonb` `target_ref` of shape `{"plans": [...]}`, which
//! is answerable only by scanning the tenant's published overlays and matching in
//! Rust — a different cost profile from the two indexed probes above, and one
//! that belongs with the surface that decides how much of it to pay. Also
//! reported.
//!
//! Both absences are **warnings**, never blocks, so nothing this module cannot
//! see can make a retirement wrongly succeed: the two classes it does read are
//! exactly the two that refuse *as references*.
//!
//! Two further things refuse and neither is a reference:
//! [`ensure_no_live_migration_targets`] reads the migration plane and raises
//! `RETIRE_TARGET_OF_MIGRATION`, and `ensure_no_live_cutover` reads the approval
//! plane and raises `RETIRE_OVER_LIVE_CUTOVER` (see that function, and the constant,
//! for why that code is this gear's and not §5's). Both are kept out of
//! [`ReferenceReport`] because their remedies
//! are different acts on different objects, which is what the ladder types apart.
//!
//! The consequence worth stating for both: the **dry-run does not report them**, so
//! a preview can call a plan retirable that the commit then refuses. Closing that
//! means deciding whether a live schedule and a live cutover are further
//! `BlockingReferenceKind`s — which would fold them into `RETIRE_PLAN_REFERENCED`,
//! the thing the ladder deliberately avoids — or whether the preview grows a channel
//! of its own. Not invented here.

use std::collections::BTreeSet;
use std::sync::Arc;

use sea_orm::{ColumnTrait, Condition, EntityTrait};
use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DBRunner, DbTx, SecureEntityExt};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::approval::ApprovalState;
use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::coverage;
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{self, ChangeSet};
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::read_model::SubjectRef;
use crate::domain::retirement::{
    BlockingReferenceKind, GenerationCoverage, PlanReference, PresenceMap, ReferenceReport,
    ScheduledWindow, WindowVerdict, dispose_windows, strand_free_disposition,
};
use crate::domain::scope_key::{PlanId, PriceEligibility, ScopeKey};
use crate::domain::window::{WindowInterval, WindowState};
use crate::infra::approval::retirement_unit_ref;
use crate::infra::publish::assemble_from;
use crate::infra::registry_deadline::request_version_now;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{bundle, bundle_component, plan_addon_rule};
use crate::infra::storage::repo::approval_repo::{self, ApprovalRecord};
use crate::infra::storage::repo::audit_repo::NewAuditEntry;
use crate::infra::storage::repo::outbox_repo::PriceWindowTransitionPayload;
use crate::infra::storage::repo::{
    NewOutboxEvent, PendingVersionRow, PlanRetiredPayload, WindowMutationEvent, audit_repo,
    catalog_version_ref_repo, migration_repo, outbox_repo, plan_repo, price_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use crate::infra::window::VerdictJson;
use time::OffsetDateTime;

/// Everything that refers to `plan_id`, in the two weights
/// `inst-re-references` gives them.
///
/// The report is built even when it is empty — a retirement of a plan nothing
/// refers to costs two index probes returning no rows, which is the common case
/// by a wide margin, and the dry-run needs the empty report to say so.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a revision number has no column representation.
pub async fn references(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<ReferenceReport, RepoError> {
    let mut blocking = Vec::new();
    for reference in referencing_bundles(runner, scope, tenant_id, plan_id).await? {
        blocking.push((BlockingReferenceKind::BundleComponent, reference));
    }
    for reference in referencing_addon_overrides(runner, scope, tenant_id, plan_id).await? {
        blocking.push((BlockingReferenceKind::AddOnPriceOverrideTarget, reference));
    }
    Ok(ReferenceReport {
        blocking,
        // See the module doc: neither warning class has a producer in this gear.
        warnings: Vec::new(),
    })
}

/// Refuse the retirement when a **live** migration is aimed at this plan.
///
/// The sibling of [`ReferenceReport::ensure_retirable`] and deliberately not part
/// of it: a blocking *reference* is remedied by re-composing or retiring the
/// referrer, and this one by cancelling the schedule or waiting for it, so the
/// error ladder types them apart (`RETIRE_PLAN_REFERENCED` against
/// `RETIRE_TARGET_OF_MIGRATION`) and the refusal has to as well.
///
/// "Live" is [`migration_repo::live_targeting`]'s narrowing and not this
/// function's: a completed or cancelled run moves nobody, so it must not refuse.
///
/// # Errors
/// [`DomainError::RetireTargetOfMigration`] naming every live schedule;
/// [`DomainError::Internal`] on a storage failure.
async fn ensure_no_live_migration_targets(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<(), DomainError> {
    let live = migration_repo::live_targeting(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    if live.is_empty() {
        return Ok(());
    }
    let schedules = live
        .iter()
        .map(|record| {
            format!(
                "{} (effective {})",
                record.migration_id,
                format_rfc3339(record.effective_at)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(DomainError::RetireTargetOfMigration(format!(
        "plan {plan_id} cannot be retired while it is the target of a live migration: \
         {schedules}; cancel the migration or wait for it to complete first"
    )))
}

/// Refuse the retirement while a **live cutover** stands over the plan
/// (`inst-co-retirement-unwind`, D-05).
///
/// [`ensure_no_live_migration_targets`]'s shape and its placement: a read of one
/// neighbouring plane, raised before anything is staged, so no writer of this
/// transaction has run when the refusal lands.
///
/// # This refuses where D-05 asks for an unwind, and that is the deviation
///
/// `inst-co-retirement-unwind` requires the retirement to *unwind* the unit inside
/// its own transaction — restore the predecessor's bound, cancel the copy and the
/// successor, close the unit as `unwound`. [`retire_in`]'s own doc holds the account
/// of why that act is one slice and not a paragraph, and one of its four clauses is
/// not constructible here at all: `unwound` is a state neither this machine nor
/// `05-governance.md` §4 has, and minting it is what D-204 clause (2) refuses.
///
/// What this function decides is the *unbuilt* case's outcome. Committing instead
/// flips the revision and leaves the cutover's shortened predecessor ending at the
/// cutover instant with the schedules that were to take over from it still standing
/// on a retired plan — D-05's trailing void, reached by doing nothing. Refusing
/// costs an operator an act and buys the property D-05 exists for; the unwind, when
/// it lands, is what makes the act possible again.
///
/// # It is a 409 carrying `RETIRE_OVER_LIVE_CUTOVER`
///
/// The ladder types this apart from `RETIRE_PLAN_REFERENCED` and
/// `RETIRE_TARGET_OF_MIGRATION` for exactly the reason those two are typed apart
/// from each other: the remedy is a different act on a different object — wait for
/// the instant, or withdraw the unit while it is still `submitted`. And it is a
/// **conflict**, not a precondition failure: the request was well formed and the
/// authority sufficient, and re-reading the approval plane is what resolves it,
/// which is what a 409 asks for. `infra::storage`'s ladder argues the general case
/// on `ApprovalNotPending` — told `LIFECYCLE_FORBIDDEN` (400) instead, a caller
/// reads it as their own request being malformed and stops.
///
/// **The design set declares the unwind, not this refusal**
/// (`inst-co-retirement-unwind`, `inst-rt-cancel`, D-05), so the code is minted here
/// and its §5 entry is **owed** — a deviation from D-204 clause (2), *"a wire code is
/// the set's to declare"*, recorded rather than smoothed over.
/// [`crate::domain::retirement::RETIRE_OVER_LIVE_CUTOVER`] states it where the name
/// is declared and states the cost. It is not the move the paragraph above refuses:
/// `unwound` would be a **state this machine has to hold** and has no producer, where
/// this is a discriminator on a refusal already being made and already reaching a
/// caller under a code that misdescribes it.
///
/// # "Live" is the instant and the state together
///
/// A cutover unit is `approved` for the rest of time once it is decided, and the
/// instant is the only thing that says whether its schedules are still ahead. A
/// state filter alone would refuse every plan that had ever been cut over; an
/// instant filter alone would refuse one whose unit was rejected. A `submitted` unit
/// counts too — D-05 voids it, and a retirement that left it standing would leave a
/// reviewer able to approve a cutover of a retired plan.
///
/// # Errors
/// [`DomainError::RetireOverLiveCutover`] naming the first live unit;
/// [`DomainError::Internal`] on a storage failure.
async fn ensure_no_live_cutover(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let units = approval_repo::cutover_units_of_plan(
        runner,
        scope,
        tenant_id,
        plan_id,
        &[ApprovalState::Submitted, ApprovalState::Approved],
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    // A loop rather than `find_map`, because the decoder can now refuse: a ref
    // under the cutover head that will not decode is a corrupt row and must not
    // read here as "this plan carries no live cutover".
    let mut live = None;
    for unit in &units {
        let at = crate::infra::cutover::cutover_instant_of_unit_ref(&unit.subject_ref)
            .map_err(|e| repo_failure(&e))?;
        if let Some(at) = at.filter(|at| *at > now) {
            live = Some((unit, at));
            break;
        }
    }
    let Some((unit, at)) = live else {
        return Ok(());
    };
    Err(DomainError::RetireOverLiveCutover(format!(
        "plan {plan_id} carries a live cutover at {}: approval {} is {}. Retiring the plan now \
         would leave the cutover's shortened predecessor window ending at that instant with the \
         successor that was to take over from it standing on a retired plan, which is the \
         trailing void no gap check can see. Retire once the cutover has taken effect, or \
         withdraw the unit while it is still submitted",
        format_rfc3339(at),
        unit.approval_id,
        unit.state.as_str()
    )))
}

/// The bundles whose **current published revision** lists this plan as a
/// component.
async fn referencing_bundles(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    component_plan_id: PlanId,
) -> Result<Vec<PlanReference>, RepoError> {
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
        .map_err(|e| RepoError::Db(format!("read bundles referencing a retiring plan: {e}")))?;

    let bundle_ids: BTreeSet<Uuid> = memberships.iter().map(|row| row.bundle_id).collect();
    let mut found = Vec::new();
    for bundle_id in bundle_ids {
        let Some(record) = bundle_row(runner, scope, tenant_id, bundle_id).await? else {
            continue;
        };
        let Some(revision) = current_revision(runner, scope, tenant_id, record.plan_id).await?
        else {
            continue;
        };
        if !memberships
            .iter()
            .any(|row| row.bundle_id == bundle_id && row.plan_revision == revision)
        {
            // A component of some *other* revision of that bundle, not of the
            // one consumers resolve against.
            continue;
        }
        found.push(PlanReference {
            referrer_id: bundle_id,
            // The bundle's own plan, because that is the thing an operator goes
            // and edits: `pricing_bundle` carries no display name, and a bare
            // bundle id names nothing they can open.
            referrer_label: format!("bundle on plan {}", record.plan_id),
        });
    }
    Ok(found)
}

/// The plans whose current published revision carries an add-on rule overriding
/// a **price row of the retiring plan**.
///
/// # The per-plan collapse runs *after* the revision test, and it is a fix
///
/// [`referencing_bundles`]' shape, and it is spelled the same way here because
/// the alternative was a live defect. A referring plan
/// holds **one rule row per revision it has ever published** — `copy_addon_rules`
/// carries every row forward with `price_override_ref` unchanged (D-83) and only
/// an abandoned draft is ever deleted — while the query below filters on neither
/// revision nor order. With the de-duplication inside the loop's opening
/// `continue`, whichever row the scan returned first consumed the referring plan's
/// one slot; if it sat on an older revision the three-line-later
/// `plan_revision != revision` dropped it and every remaining row of that plan was
/// then skipped as already-seen. Both engines tend to answer an unordered scan in
/// insertion order — the **oldest** revision first — so a blocking reference was
/// lost systematically rather than occasionally, and a plan whose price row another
/// plan's current revision overrides retired anyway.
///
/// So the referrers are collected first and each is asked about its own current
/// revision once, which is also why the answer is ordered rather than
/// storage-dependent.
async fn referencing_addon_overrides(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Vec<PlanReference>, RepoError> {
    let price_ids: Vec<Uuid> =
        price_repo::load_scope_keys_for_plan(runner, scope, tenant_id, plan_id)
            .await?
            .into_iter()
            .map(|(price_id, _key)| price_id)
            .collect();
    if price_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rules = plan_addon_rule::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_addon_rule::Column::TenantId.eq(tenant_id))
                .add(plan_addon_rule::Column::PriceOverrideRef.is_in(price_ids)),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read add-on overrides of a retiring plan: {e}")))?;

    // The rule's **own** plan is not a referrer. A rule on the retiring plan
    // itself is not a reference to anything: retiring a plan whose add-on
    // overrides one of its own rows refuses nothing.
    let referrers: BTreeSet<Uuid> = rules
        .iter()
        .map(|rule| rule.plan_id)
        .filter(|referrer| *referrer != plan_id.get())
        .collect();

    let mut found = Vec::new();
    for referrer in referrers {
        let Some(revision) = current_revision(runner, scope, tenant_id, referrer).await? else {
            continue;
        };
        if !rules
            .iter()
            .any(|rule| rule.plan_id == referrer && rule.plan_revision == revision)
        {
            // An override carried by some *other* revision of that plan, not by
            // the one consumers resolve against.
            continue;
        }
        found.push(PlanReference {
            referrer_id: referrer,
            referrer_label: format!("plan {referrer}"),
        });
    }
    Ok(found)
}

/// The referring plan's current revision, as the child tables store it.
async fn current_revision(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: Uuid,
) -> Result<Option<i64>, RepoError> {
    let Some(current) =
        plan_repo::load_current(runner, scope, tenant_id, PlanId::new(plan_id)).await?
    else {
        return Ok(None);
    };
    i64::try_from(current.revision).map(Some).map_err(|_| {
        RepoError::CorruptRow(format!(
            "plan {plan_id} stands at revision {}, which no column can address",
            current.revision
        ))
    })
}

/// Read one bundle header.
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
        .map_err(|e| RepoError::Db(format!("read a referencing bundle: {e}")))
}

// ---------------------------------------------------------------------------
// The orchestrator (`inst-rt-api`, `inst-rt-cancel`, `inst-rt-event`,
// `inst-rt-return`, `inst-re-block`, `inst-re-warn`, `inst-re-governed`).
// ---------------------------------------------------------------------------

/// What a dry-run answers, and what the confirm screen renders
/// (`inst-rt-api`, `inst-re-warn`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetirementPreview {
    /// The plan this would retire.
    pub plan_id: PlanId,
    /// The revision the flip targets — the plan's current published one.
    pub revision: u64,
    /// Every not-yet-active window, cancelled or kept **and why**. Kept windows
    /// are labelled distinctly because `inst-re-cancelflow` requires it: an
    /// operator shown a bare count cannot tell a plan whose coverage is being
    /// preserved from one whose coverage is being torn down.
    pub windows: Vec<WindowVerdict>,
    /// What refers to the plan, in both weights.
    pub references: ReferenceReport,
    /// Whether the presence lane answered at all. `true` is D-182's case and
    /// therefore this system's only one, and it is on the preview so the screen
    /// can say "kept because nobody could be asked" rather than implying a
    /// subscriber was found on every key.
    pub presence_unresolved: bool,
}

/// What a confirmed retirement answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetirementReceipt {
    /// The plan that retired.
    pub plan_id: PlanId,
    /// The revision that flipped.
    pub revision: u64,
    /// The registry's pending handle (D-128: retirement is a publish unit).
    pub pending_version_ref: String,
    /// The windows the cancellation flow was invoked on.
    pub cancelled_window_ids: Vec<Uuid>,
    /// The audit sequence the record landed at.
    pub audit_seq: u64,
}

/// A retirement waiting on an independent second principal (D-109).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetirementPending {
    /// What the reviewer is deciding about.
    pub preview: RetirementPreview,
    /// The unit itself.
    pub approval: ApprovalRecord,
}

/// Committed, or opened for review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetirementOutcome {
    /// The flip committed.
    Retired(Box<RetirementReceipt>),
    /// A second principal has to decide first — always, for a retirement.
    SubmittedForApproval(Box<RetirementPending>),
}

/// The `RetirementOrchestrator` of §1.7.
///
/// `Clone` because [`crate::api::rest::state::GovernanceState`] is, and both of
/// its fields are handles rather than state: a `DBProvider` and the one registry
/// `Arc`. Cloning the service is two refcount bumps and gives a second reader of
/// the same provider — never a second incrementer of `CatalogVersion`, which is
/// the invariant `infra::publish` states and which holds here because the `Arc`
/// is shared rather than rebuilt.
#[derive(Clone)]
pub struct RetirementService {
    db: DBProvider<DbError>,
    registry: Arc<dyn CatalogVersionRegistryV1>,
}

impl RetirementService {
    /// Build the workflow over one provider and the resolved registry.
    ///
    /// The **same** registry `Arc` every other requester holds; what keeps their
    /// handles apart is the request id's distinct first segment.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>, registry: Arc<dyn CatalogVersionRegistryV1>) -> Self {
        Self { db, registry }
    }

    /// The dry-run (`inst-rt-api` clause 1, `inst-re-warn`).
    ///
    /// Reads and decides; writes nothing, requests no version, opens no unit.
    /// It is the screen an approver reads **before** deciding, which D-61's
    /// reviewability invariant requires and which `plan × read` already covers.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the plan has no current revision;
    /// [`DomainError::LifecycleForbidden`] when that revision is not one a
    /// retirement may leave; [`DomainError::Internal`] on a storage failure.
    pub async fn preview(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
    ) -> Result<RetirementPreview, DomainError> {
        let conn = self.db.conn().map_err(|e| {
            DomainError::Internal(format!("bss-pricing: retirement preview connection: {e}"))
        })?;
        // The dry-run's own clock. The commit stamps its instant and passes it
        // down; a read that writes nothing has no stamp to take one from, and
        // D-04's anchor is `max(cohort, now)` — so the screen answers about the
        // instant the operator is looking at it.
        compose_preview(&conn, scope, tenant_id, plan_id, OffsetDateTime::now_utc()).await
    }

    /// The confirm (`inst-rt-cancel`, `inst-rt-event`, `inst-rt-return`).
    ///
    /// The whole act in **one** transaction of this service's own. See
    /// [`retire_in`] for the order and why each step is where it is.
    ///
    /// # Errors
    /// [`retire_in`]'s, exactly.
    pub async fn retire(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<RetirementOutcome, DomainError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        let registry = Arc::clone(&self.registry);
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<RetirementOutcome, DomainError, _>(move |txn| {
                Box::pin(async move {
                    // Boxed a second time for `infra::cutover::cut_over`'s reason:
                    // the orchestrator's future carries a whole `PlanShape` and a
                    // preview across its awaits, and `clippy::large_futures` is
                    // what says so.
                    Box::pin(retire_in(
                        txn,
                        &registry,
                        &ctx,
                        &scope,
                        tenant_id,
                        plan_id,
                        verdict_json,
                        stamp,
                    ))
                    .await
                })
            })
            .await;
        outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: retirement transaction: {infra}"))
            })
        })
    }
}

/// Read every fact the retirement judgement needs, and decide.
///
/// Shared by the dry-run and the confirm **deliberately**: a preview computed by
/// different code from the commit is a preview an operator can approve and then
/// watch do something else.
async fn compose_preview(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<RetirementPreview, DomainError> {
    // **One call, a presence map** (D-131) — and in this system the call has no
    // callee. D-182 makes the absent D-79 lane D-131's fail-closed case, so every
    // key reads occupied and every window is kept. This is the whole of the
    // lane's integration: when a client lands it produces a
    // `PresenceMap::resolved` here, and what it hands to [`compose_preview_with`]
    // is the only thing that changes.
    compose_preview_with(
        runner,
        scope,
        tenant_id,
        plan_id,
        PresenceMap::fail_closed(),
        now,
    )
    .await
}

/// [`compose_preview`] over a presence map the caller supplies.
///
/// # It is `pub`, and it is [`cancel_windows_in`]'s posture rather than a
/// testability leak
///
/// The D-79 lane has no client, so [`compose_preview`] can hand this exactly one
/// value — [`PresenceMap::fail_closed`] — under which every window is kept and
/// the condemned list is empty. Every rule that decides what happens to a
/// **cancelled** window is therefore unreachable from the orchestrator, and a
/// rule no suite can arm is how the missing `PriceWindowCancelled` event stayed
/// green for a slice and how D-04's bound went unenforced on this surface for a
/// day. So the judgement is addressable on its own, `sqlite_retirement_unit` arms
/// it against a resolved map, and the day the lane lands its client passes the
/// map it read instead of a constant.
///
/// # Errors
/// [`compose_preview`]'s, exactly.
pub async fn compose_preview_with(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    presence: PresenceMap,
    now: OffsetDateTime,
) -> Result<RetirementPreview, DomainError> {
    let current = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "current plan revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    // Asked of the state machine rather than matched on `Published`, so the one
    // refused edge that has its own words keeps them: a plan already retired is
    // told so, and never that "something is in the way".
    if !current
        .lifecycle_state
        .can_transition(LifecycleState::Retired)
    {
        return Err(DomainError::LifecycleForbidden(format!(
            "plan {plan_id} stands at a {} revision; only a published revision retires",
            current.lifecycle_state
        )));
    }

    let references = references(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;

    // The plane is read **once** and read whole: the disposition needs the
    // not-yet-active subset, and D-04's bound needs every window of a
    // grandfathered key whatever state it stands in — a generation's coverage is
    // carried by its active windows as much as by its scheduled ones, and a rule
    // that saw only the candidates would read a covered key as uncovered and keep
    // windows nothing was going to strand.
    let plane = window_repo::list_for_plan(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;

    // Only **not-yet-active** windows are candidates. An active window runs to
    // its natural end for the in-flight subscribers this slice preserves
    // coverage for, so it is not a member of the input set at all.
    let scheduled: Vec<ScheduledWindow> = plane
        .iter()
        .filter(|w| w.state == WindowState::Scheduled)
        .map(|w| ScheduledWindow {
            window_id: w.window_id,
            price_id: w.price_id,
        })
        .collect();

    let mut windows = dispose_windows(&scheduled, &presence);

    // D-04's `inst-co-bounds`, asked of what the cancellations would **leave**
    // (D-316's residual). It runs after the presence judgement and only ever
    // moves a verdict back to `kept`, so nothing the lane decides to keep is
    // disturbed and nothing it decides to cancel escapes the bound.
    let generations = generation_coverage(
        runner,
        scope,
        tenant_id,
        plan_id,
        current.clone(),
        &plane,
        now,
    )
    .await?;
    strand_free_disposition(&mut windows, &generations, now);

    Ok(RetirementPreview {
        plan_id,
        revision: current.revision,
        windows,
        references,
        presence_unresolved: presence.is_fail_closed(),
    })
}

/// Every grandfathered generation of the plan, with the coverage and the two
/// bound terms D-04 judges it by.
///
/// # The operands were here all along, and D-316 recorded otherwise
///
/// That entry's residual says the guard *"needs the plan shape and the act's key
/// set, neither of which the retirement path carries"*, and `window_repo`'s
/// roster says the same one layer down. The roster's version is **true about
/// `window_repo`** — that function holds one window and its key — and the
/// conclusion drawn from it does not survive one layer up: every operand
/// `refuse_horizon_uncovered` reads is already assembled on this path.
/// [`window_repo::list_for_plan`] resolves each window's `ScopeKey` off
/// `pricing_price` on every read, which **is** the act's key set;
/// [`assemble_from`] is called by [`retire_in`] eleven lines below the composition
/// for the approval pin, which **is** the plan shape; and W6's margin is
/// [`coverage::longest_cycle_sold`] over that shape.
///
/// **The two calls are two calls.**
/// `retire_in` reaches this function through `compose_preview` and then assembles
/// again for the pin, so a plan holding any `existing_grandfathered` key builds the
/// whole `PlanShape` twice inside one transaction — and resolves `load_current`
/// twice with it. The early return above is what keeps that off the common path:
/// a plan with no generation reaches neither read. Threading one shape down from
/// `retire_in` is owed; what is stated here is the call graph as it stands.
async fn generation_coverage(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    current: crate::domain::plan::PlanRevision,
    plane: &[window_repo::WindowRecord],
    now: OffsetDateTime,
) -> Result<Vec<GenerationCoverage>, DomainError> {
    // The **grandfathered** keys of the plane, in a stable order. Every other key
    // carries no horizon and nothing here to judge.
    let mut keys: Vec<ScopeKey> = Vec::new();
    for record in plane {
        if record.scope_key.price_eligibility() == PriceEligibility::ExistingGrandfathered
            && !keys.contains(&record.scope_key)
        {
            keys.push(record.scope_key.clone());
        }
    }
    if keys.is_empty() {
        // The common case by a wide margin, and it costs neither of the two reads
        // below. A plan with no generation cannot strand one.
        return Ok(Vec::new());
    }

    let shape = assemble_from(runner, scope, tenant_id, plan_id, current, now).await?;
    // The **published** rows, because the horizon is a published fact: a draft row
    // carries no generation any subscriber is bound to, which is the same carve-out
    // `refuse_horizon_uncovered` makes when it reads `planned.plan.published`.
    let published = price_repo::load_for_plan(
        runner,
        scope,
        tenant_id,
        plan_id,
        &[LifecycleState::Published],
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Ok(keys
        .into_iter()
        .filter_map(|key| {
            // **A key with no published row is skipped, and the `find` is not a
            // lookup that may fail** — it is the carve-out itself, and collapsing
            // it into `and_then` would be a bug with a money consequence: a
            // grandfathered key carrying only a draft row would render
            // `grandfather_until: None`, which the domain reads as D-147's
            // **indefinite** generation and answers with "coverage must never
            // end". That is the strictest arm of the bound applied to a key that
            // has no bound at all. `refuse_horizon_uncovered` makes the same
            // carve-out one surface over, by returning `Ok(())` when its `find`
            // misses.
            let row = published.iter().find(|row| row.scope_key == key)?;
            let windows = plane
                .iter()
                .filter(|w| w.scope_key == key)
                .map(|w| {
                    (
                        w.window_id,
                        WindowInterval::new(w.effective_from, w.effective_to, w.state),
                    )
                })
                .collect();
            let margin = coverage::longest_cycle_sold(&shape, key.currency(), key.region());
            Some(GenerationCoverage {
                scope_key: key,
                windows,
                grandfather_until: row.grandfather_until,
                margin,
            })
        })
        .collect())
}

/// One retirement, composed and committed or composed and submitted, in the
/// caller's transaction.
///
/// # The order, and why each step is where it is
///
/// 1. **Compose** — the same read-and-decide the dry-run runs, inside the
///    transaction that writes, so nothing an operator approved can have moved
///    behind the decision.
/// 2. **Refuse on a blocking reference** (`inst-re-references`) before anything
///    else. It is permanent for this world state and names an act the operator
///    must perform first, so asking a reviewer to decide a retirement that
///    cannot commit would waste the one thing D-109 is spending: a second
///    principal's attention.
/// 3. **An approved unit decides before the evaluator is asked**, the
///    supersession's rule and for its reason.
/// 4. **The verdict is fixed** — `inst-mat-registered` registers
///    [`Trigger::PlanRetirement`] (D-109), so no threshold policy is read and
///    none could change the answer. Retirement is material because of what it
///    **is**: irreversible, sales-stopping, and cancelling on every unoccupied
///    key at once — which is exactly the act D-62 made two-person for a *single*
///    window.
/// 5. **The registry request**, fail closed, after every refusal (D-156).
/// 6. **The flip**, then the windows, then the ref, the event and the record.
///
/// # What this function does **not** do, and it is not an oversight
///
/// **It does not abandon the plan's open draft revision.** `inst-rt-cancel`
/// requires that (D-145) and it is owed. `PlanRepo::abandon_draft` opens a
/// transaction of its own, so calling it here would nest one; the honest fix is
/// to extract its body into an `_on` form the way this file's neighbours are
/// factored, which is a refactor of a shared repository better made deliberately
/// than as a side effect. The gap is **inert rather than unsound**: a retired
/// plan can never publish, and `publish_revision`'s
/// `refuse_unpublishable_predecessor` refuses the draft's publish with
/// `PLAN_RETIRED_NO_SUCCESSOR` — so what survives is a draft nobody can ship,
/// holding a revision number, rather than a path to publishing on a retired
/// plan. Reported in the hand-back.
///
/// **It does not unwind a live cutover unit** (`inst-co-retirement-unwind`,
/// D-05) — it **refuses** while one stands. `inst-rt-cancel` requires the unwind
/// *in the same transaction* as the flip: the predecessor window's `effectiveTo`
/// restored to its recorded pre-cutover value, the scheduled copy and successor
/// cancelled, the unit closed as `unwound`, a merely `submitted` unit voided.
/// Nothing below performs any of it — `cancel_windows_in` acts on
/// `preview.windows`, which is the plan's own window plane judged by
/// `dispose_windows`, and knows nothing about a cutover unit. So
/// [`ensure_no_live_cutover`] refuses the whole act instead, and that function
/// carries why refusing is the arm taken and what it costs.
///
/// **One of the four is not merely unbuilt — it is not constructible here**, which
/// is the part an account of this gap most easily leaves out:
///
/// * *"its recorded pre-cutover value"* named an operand **nothing recorded**, and
///   now names one that is recorded and addressable. The account of the gap was
///   exact — `infra::cutover::commit_cutover` shortens the predecessor through
///   `window_repo::adjust_effective_to`, an in-place `UPDATE` of a table with no
///   before-image column, the act's three events are none of them about the shorten,
///   and the value is not derivable afterwards because `compose_cutover_windows`
///   picks the window `WindowInterval::covers` answers `true` for, which admits an
///   absent end *and* any end strictly after the cutover. What changed is the fourth
///   store: `record_cutover`'s state now carries the shortened window's interval on
///   both sides of the pair, keyed `(scopeKey, effectiveFrom)`, and
///   `audit_repo::for_subject` reads it back under the act's own subject. So an
///   unwind no longer has to guess `None` and hand a plan being retired open-ended
///   coverage it never had.
/// * *"closed as `unwound`"* names a state **this machine** does not have, and the
///   store is the whole of what is missing: `05-governance.md` §4 declares the
///   fifth token, §6's `state` column carries it and `inst-as-unwind` declares the
///   `approved → unwound` transition — all three since **D-333**. What
///   does not exist is the persisted half: `chk_pricing_approval_state` still admits
///   only `submitted | approved | rejected | voided`, and `ApprovalState` has no
///   variant. That is ordinary build work in this crate, on a decision that is
///   already taken.
///
///   The design set **does** declare the token and D-204 clause (2) does not refuse
///   minting it. Reading it the other way is the error worth guarding against: a
///   mirroring obligation with no target reads to the next author as a blocked
///   external dependency, which is D-333's own account of why it corrected the
///   same sentence in the register. `infra::cutover::record_cutover` and
///   `infra::grandfather` describe this world correctly and are the wording to
///   follow.
///
/// The other three — finding the live unit, cancelling its two scheduled windows,
/// restoring the bound off the trail — **are** constructible today, and building
/// them alone would be worse than building nothing: a predecessor still ending at
/// the cutover with the schedules that were to take over from it gone is precisely
/// the trailing void D-05 rejected its option (a) to avoid, arriving as the result of
/// the fix. So the unwind is one act or none.
///
/// # Two things the unwind must do that reading D-05 does not tell you
///
/// Recorded here, at the site that will build it, rather than left to be rediscovered:
///
/// 1. **Cancel the successor window before restoring the bound, never after.**
///    `window_repo::adjust_effective_to` runs `refuse_overlap` over
///    `OCCUPYING_STATES`, and the successor's `[cutover, …)` sits on the
///    predecessor's own key — so lengthening the predecessor back past the cutover
///    while that window is still `scheduled` is refused `WINDOW_OVERLAP`. The copy
///    does not participate: `inst-co-copy` moves it to a **new generation's** key, so
///    it is on a different plane. The ordering is therefore forced by a rule, not
///    chosen — and it is the reverse of the order D-05's own sentence lists the
///    clauses in.
/// 2. **Re-check `cutover_at > now` inside the unwind's own transaction.** This is
///    the single reachable path to the append-only trigger's refusal
///    (`pricing_price_window` arm 5, `pricing_price_window`'s Postgres twin), and
///    `tests/sqlite_cutover_unwind.rs` pins why every *other* path is unreachable:
///    inside D-05's scope the cutover is approved and not yet effective, so `OLD` is
///    the cutover instant `T > now` and `NEW` is the original bound the cutover
///    shortened, hence `> T > now`; a NULL original bound skips the clause outright.
///    What that argument depends on is `T > now`, and the cutover instant can pass
///    between an operator's decision to retire and this transaction's write. A
///    half-applied unwind — the two schedules cancelled by clause (1) above, the
///    bound not restored because the trigger refused it — is exactly the coverage
///    void D-05 exists to prevent, reached by the fix. So the guard belongs in the
///    same transaction as the writes, and a retirement that finds `T <= now` must
///    refuse the whole act rather than perform part of it.
///
/// What D-05 asks for and **is** already paid is its materiality half: retirement
/// is always material under `Trigger::PlanRetirement` (D-109, unconditional),
/// `MaterialityVerdict` carries no trigger identity, and so a verdict declared
/// under `Trigger::RetirementUnwindingACutover` would be byte-identical. That
/// trigger's zero producers therefore cost nothing observable — which is why the
/// `false` beside it in `domain::materiality::triggers` is honest and why the
/// gap had no symptom for a census to find.
///
/// # Errors
/// [`DomainError::RetirePlanReferenced`] on a blocking reference;
/// [`DomainError::RetireTargetOfMigration`] when a live migration aims at the plan;
/// [`DomainError::LifecycleForbidden`] when the plan is not at a published
/// revision; [`DomainError::RetireOverLiveCutover`] when a live cutover stands over
/// it ([`ensure_no_live_cutover`]); [`DomainError::PendingChangeUnitExists`] when a
/// unit is already open; [`DomainError::CatalogVersionUnavailable`] when the registry cannot be
/// reached; [`DomainError::Internal`] on a storage failure.
#[allow(
    clippy::too_many_arguments,
    reason = "`cutover_in`'s argument list, minus the request: the transaction, the registry, the \
              security context, the scope, the tenant, the plan, the verdict renderer and the \
              stamp. Every one is a different authority and none is derivable from the others"
)]
pub async fn retire_in(
    txn: &DbTx<'_>,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    verdict_json: VerdictJson,
    stamp: AuditStamp,
) -> Result<RetirementOutcome, DomainError> {
    let now = stamp.recorded_at;

    // 1. and 2.
    let preview = compose_preview(txn, scope, tenant_id, plan_id, now).await?;
    preview.references.ensure_retirable(plan_id.get())?;
    ensure_no_live_migration_targets(txn, scope, tenant_id, plan_id).await?;
    ensure_no_live_cutover(txn, scope, tenant_id, plan_id, now).await?;
    let revision = preview.revision;

    // 3. The approved unit, resolved against the same subject a submit opens.
    let current = plan_repo::load_current(txn, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "current plan revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    let shape = assemble_from(txn, scope, tenant_id, plan_id, current, now).await?;
    let subject_ref = retirement_unit_ref(plan_id, revision);
    let authorization =
        crate::infra::approval::authorizing_unit(txn, scope, tenant_id, &shape, &subject_ref)
            .await?;

    // 4. Fixed, and read from the act.
    let verdict = materiality::evaluate(
        &ChangeSet::of_act(Trigger::PlanRetirement, std::iter::empty()),
        None,
        None,
    );

    if authorization.is_none() {
        // This act's own pending unit — answered before anything is staged,
        // because the answer is that nothing more should be. A retry of a
        // retirement under review must find that unit rather than open a second.
        if let Some(held) = approval_repo::find_pending_for_subject(txn, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Ok(RetirementOutcome::SubmittedForApproval(Box::new(
                RetirementPending {
                    preview,
                    approval: held,
                },
            )));
        }
        let record = crate::infra::approval::ApprovalService::submit_retirement_on(
            txn,
            scope,
            tenant_id,
            plan_id,
            Uuid::now_v7(),
            verdict_json(&verdict)?,
            stamp,
        )
        .await?;
        return Ok(RetirementOutcome::SubmittedForApproval(Box::new(
            RetirementPending {
                preview,
                approval: record,
            },
        )));
    }

    // 5. Addressability, fail closed, after every refusal and before the writes.
    let pending = request_version_now(
        registry.as_ref(),
        ctx,
        &retirement_request_id(tenant_id, &subject_ref),
    )
    .await?;

    // 6. The flip.
    let retired = plan_repo::retire_revision(txn, scope, tenant_id, plan_id, revision)
        .await
        .map_err(|e| repo_failure(&e))?;

    // The windows Slice 7's cancellation flow is **invoked** on, and the invocation
    // is what makes D-182's posture a measured fact rather than a missing feature.
    //
    // **The list is not empty; the *cancelled* subset of it is.** `compose_preview`
    // hands `dispose_windows` a fail-closed presence map, so every one of the plan's
    // not-yet-active windows comes back `Kept(PresenceUnresolved)` and this call
    // walks them all and cancels none. That is D-182's normative behaviour — an
    // absent D-79 lane reads every key as occupied, and retirement keeps coverage —
    // not an inert call. Deleting the walk would delete the built D-51 flow and the
    // measurement with it, and the day the lane lands a resolved map is the only
    // thing that changes here.
    let cancelled_window_ids = cancel_windows_in(
        txn,
        scope,
        tenant_id,
        &preview.windows,
        &subject_ref,
        now,
        stamp,
    )
    .await?;

    // The subject the projector re-freezes, carrying the state the flip
    // produced: D-128's whole point is that `lifecycle_state` must be evaluable
    // from the **pinned** read model, and predicate (4) reads it there.
    catalog_version_ref_repo::record_pending(
        txn,
        scope,
        PendingVersionRow::for_subject(
            tenant_id,
            pending.pending_ref.clone(),
            &SubjectRef::Plan(plan_id.get()),
            Some(revision),
            Some(retired.lifecycle_state),
            now,
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    outbox_repo::enqueue(
        txn,
        scope,
        NewOutboxEvent::plan_retired(
            tenant_id,
            &PlanRetiredPayload {
                plan_id,
                revision,
                pending_version_ref: pending.pending_ref.clone(),
                cancelled_window_ids: cancelled_window_ids.clone(),
                correlation_id: stamp.correlation_id,
            },
            now,
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    let seq = audit_repo::append(
        txn,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::plan_chain(plan_id),
            recorded_at: now,
            actor_principal_id: stamp.actor_principal_id,
            action: AuditAction::Retire,
            subject_kind: AuditSubjectKind::PlanRevision,
            subject_ref: audit_repo::plan_revision_ref(plan_id, revision),
            before_state: Some(retirement_state(LifecycleState::Published, None)),
            after_state: Some(retirement_state(
                retired.lifecycle_state,
                Some(&pending.pending_ref),
            )),
            approval_ref: authorization.as_ref().map(|record| record.approval_id),
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Ok(RetirementOutcome::Retired(Box::new(RetirementReceipt {
        plan_id,
        revision,
        pending_version_ref: pending.pending_ref,
        cancelled_window_ids,
        audit_seq: seq,
    })))
}

/// Invoke Slice 7's cancellation flow on every window the preview condemned, and
/// answer the ids in the order it decided them.
///
/// # It is a function because the *event* is half of what "invoked" means
///
/// `inst-re-cancelflow` is one sentence with two clauses: the flow is **invoked**
/// on each window, *"each cancellation emits `PriceWindowCancelled` and drives its
/// cache-eviction path"* — and *"marking-invalid without the event is
/// forbidden (consumers would keep warm caches)"*. Inline in [`retire_in`] this
/// was a bare [`window_repo::transition`] loop with a comment asserting the
/// second clause and no code performing it. That was inert only because D-182's
/// fail-closed presence map keeps the list empty; the day the D-79 lane lands,
/// every window it condemns would be marked invalid with no event — the exact
/// shape the instruction forbids, arriving green.
///
/// # The enqueue is the caller's, and that is this crate's settled arrangement
///
/// [`window_repo::transition`] enqueues nothing, deliberately: it is also the
/// activation sweep's edge (`jobs::window_activation`), and the sweep's two
/// events are `PriceWindowActivated`/`PriceWindowExpired` — a store that emitted
/// per edge would have to know which of §5's surfaces called it, which is
/// [`NewOutboxEvent::price_window_mutation`]'s own argument for taking the event
/// as an argument. So every window-mutating **service** pairs the flip with its
/// own enqueue — `infra::window`, `infra::cutover`, `infra::supersession`,
/// `infra::repricing` — and retirement was the one that did not. Putting it in
/// the repo instead would have double-emitted in all four.
///
/// # The act segment, and why the dedup key needs one
///
/// `price_window_mutation` keys on the **act** rather than on the transition,
/// because `Op::event` maps a schedule *and* an adjustment to one name; the same
/// reasoning applies here in the other direction. A window cancelled by a
/// retirement and the same window cancelled through `DELETE` are two acts, and
/// `{plan}/retirement/{revision}` is the act this one is — the same coordinate
/// [`retirement_unit_ref`] names the approval unit by, passed in from
/// [`retire_in`]'s `subject_ref` rather than re-spelled here.
///
/// **The plan segment leads**, and transposing it is not a typo with no
/// consequence: [`retirement_unit_ref`]'s own doc records that
/// `approval_repo::subject_plan` parses the aggregate off the **head** of every
/// ref, that the column's `CHECK`s admit any text, and that putting the act first
/// reads back as a `CorruptRow` from inside the approval plane. A reader who takes
/// this rendering as the shape and builds the next act's ref to match it lands
/// exactly there.
///
/// # It is `pub`, and that is the D-182 posture rather than a testability leak
///
/// [`retire_in`] cannot reach this loop at all today: `compose_preview` hard-wires
/// [`PresenceMap::fail_closed`], so every window is kept and the condemned list is
/// empty. A rule that runs only once a lane **nobody has built** starts answering
/// is a rule no suite driving the orchestrator can arm — which is precisely how
/// the missing event stayed green. So the flow is addressable on its own, and
/// `sqlite_retirement_unit` arms it against hand-built verdicts.
///
/// # Errors
/// [`DomainError::Internal`] when a transition or an enqueue fails; a window that
/// has moved since the preview read it surfaces as its own repository refusal.
pub async fn cancel_windows_in(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    verdicts: &[WindowVerdict],
    act: &str,
    now: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<Vec<Uuid>, DomainError> {
    let mut cancelled = Vec::new();
    for verdict in verdicts.iter().filter(|v| v.is_cancelled()) {
        let record = window_repo::transition(
            txn,
            scope,
            tenant_id,
            verdict.window_id,
            WindowState::Cancelled,
            now,
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        // **As written, not as the verdict described it** — the sibling services'
        // rule: the payload's interval is the row's, read back off the transition,
        // so a consumer evicting a cache is told which interval stopped existing.
        outbox_repo::enqueue(
            txn,
            scope,
            NewOutboxEvent::price_window_mutation(
                tenant_id,
                WindowMutationEvent::Cancelled,
                &PriceWindowTransitionPayload {
                    window_id: record.window_id,
                    plan_id: record.scope_key.plan_id(),
                    price_id: record.price_id,
                    effective_from: record.effective_from,
                    effective_to: record.effective_to,
                    correlation_id: stamp.correlation_id,
                },
                now,
                act,
            ),
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        cancelled.push(record.window_id);
    }
    Ok(cancelled)
}

/// The registry request id of one retirement.
///
/// Derived rather than random, so a retry of the same retirement is handed the
/// same pending handle instead of stranding a second one.
///
/// **It leads with the act, and it used to lead with the tenant**. The
/// registry is a cross-tenant service whose idempotency is keyed on exactly this
/// string, and this was the one id of the five whose uniqueness rested on a
/// segment of a string built somewhere else: `{tenant}/{subject_ref}`, where the
/// `/retirement/` that told it apart from a publish of the same revision is
/// `approval::retirement_unit_ref`'s to spell and to change. The four siblings all
/// carry a discriminator of their own — `publish.rs`' `{kind_token}/…`,
/// `supersession.rs`' `supersession/…`, `cutover.rs`' `cutover/…`, `window.rs`'
/// `…/{act}` — so this one now does too, and no collision depends on a subject
/// helper keeping a segment it never promised.
///
/// **Re-spelling strands an outstanding pending ref**, which is the same re-freeze
/// question `PublishUnitKind::request_token` carries: a retirement that requested a
/// version under the old id and has not committed will request a *new* handle on
/// its retry, leaving the first assigned and unreferenced. It is stranded, not
/// double-spent — the flip is guarded by `plan_repo::retire_revision`'s own
/// compare-and-swap, not by the handle — and the window is one uncommitted
/// retirement per plan.
fn retirement_request_id(tenant_id: Uuid, subject_ref: &str) -> String {
    format!("retirement/{tenant_id}/{subject_ref}")
}

/// The before/after state an audit record carries for a retirement.
fn retirement_state(state: LifecycleState, pending_ref: Option<&str>) -> JsonValue {
    match pending_ref {
        Some(pending) => serde_json::json!({
            "lifecycleState": state.as_str(),
            "pendingVersionRef": pending,
        }),
        None => serde_json::json!({ "lifecycleState": state.as_str() }),
    }
}

#[cfg(test)]
#[path = "retirement_tests.rs"]
mod retirement_tests;
