//! Cloning a plan into a new draft (`design/12-operator-efficiency.md` §3
//! `algo-clone`, `inst-cl-copy`, `inst-cl-resets`, `inst-cl-discount`,
//! `inst-cl-draft`, `inst-cl-windows`; D-19, D-264, D-268, D-269).
//!
//! The clone copies **configuration** and nothing else. What separates the two
//! is not a list to memorize: configuration is what an author wrote, and
//! everything left behind is *lifecycle state* — where the plan got to, not what
//! it is. Windows, grandfathered generations and superseded history are all the
//! second kind, and `inst-cl-resets` says so in the same breath for all three.
//!
//! # Both cutover-made eligibility classes are lifecycle state (D-268)
//!
//! `inst-cl-resets` excludes `existing_grandfathered` rows *because* copying
//! them under its own reset would collapse two rows onto one canonical scope
//! key. **The identical collapse happens for `new_subscriptions_only`**, and the
//! clause named only the first class. Both are made by a cutover rather than
//! authored, and on a clone the second is meaningless twice over: every
//! subscription on a brand-new plan is new. So both are excluded, and each is
//! reported on the receipt.
//!
//! The consequence is that the eligibility reset itself now has **no operand**:
//! every row that reaches [`reset_key`] already carries `all_subscriptions`,
//! because the only two classes that could carry anything else are excluded
//! first. It is kept as a structural fence beside `Cohort::None` and
//! `grandfather_until` (D-266), not as behaviour a test can prove.
//!
//! # A clone of a bundle is a bundle (D-269)
//!
//! §3's copy set predates Slice 8 and names none of the bundle tables, while
//! `plan_repo::open_revision` copies the composition as one of the plan's child
//! tables — *a bundle rides its plan's revisions*. The two paths that reproduce
//! a plan disagreed, and this one produced a plan holding a bundle's price rows
//! and none of its composition. It now copies: a **new `bundle_id`**
//! (`pricing_bundle.plan_id` is unique per plan), and the components and
//! rev-share groups under it. `bundle_component.component_plan_id` names
//! **other** plans and is carried unchanged — those are different plans, not the
//! clone's phases, and the phase remap has nothing to do with them.
//!
//! # The phase remap is the whole difficulty, and it has three sites
//!
//! Phase rows are copied under **new** `phase_id`s (D-19), so every reference to
//! the old ids has to move with them. There are exactly three:
//!
//! 1. the phase rows' own `converts_to_phase_id` chain;
//! 2. the `phase` axis of every copied price row's canonical scope key;
//! 3. **the keys of the D-41 `entitlement_grants.perPhase` map.**
//!
//! The third is the one a review found missing (C-7): the map is
//! keyed *by* `phase_id`, so an unremapped clone published a grant set pointing
//! at phases that existed only in the source, and its first publish failed
//! `GRANT_SET_PHASE_UNKNOWN` on dangling keys. `AddonRule` carries no phase and
//! the descriptor set carries no phase, so those two are copies rather than
//! remaps — checked rather than assumed.
//!
//! # Three clauses of §3 have no operand in this gear, and are named rather than
//! written
//!
//! Writing a guard for any of them would be a rule that can never fire, which
//! this program has spent a day removing:
//!
//! - **`pricing_plan_grant` is not copied because it does not exist.** The copy
//!   set names it, and it is *Slice 10's* credit-grant table (D-52) — a different
//!   object from Slice 6's `entitlement_grants` column, carrying `category`,
//!   `applicability`, `drawdownPriority` and the `source ∈ {authored,
//!   compiled_allowance}` lineage. It is unbuilt, and it sits inside the
//!   `inst-ac-*` chunk D-177 blocks. So **D-130's rule — a `compiled_allowance`
//!   grant is recompiled at the clone's own publish rather than copied — has
//!   nothing to range over**, and the arm that would implement it is not here.
//! - **Contract locks are not copied because this gear stores none.** They live
//!   in the Contracts registry, which D-251 records as absent; nothing on a plan
//!   or a price row is a lock for this code to skip.
//! - **`discountRef` copies unconditionally.** `inst-cl-discount` drops it unless
//!   it "still resolves to a registered instrument", and `pricing_plan_addon_rule`
//!   already records `inst-dr-referential` as **not buildable**: there is no
//!   instrument registry in this workspace to resolve against. A
//!   drop-if-unresolved arm could never fire, so the ref rides along with the
//!   rest of the row's content.
//!
//! # The clone is an ordinary draft
//!
//! `inst-cl-draft`: no rule reads `cloned_from`, the content pin does not frame
//! it, and the clone's first publish takes the full pipeline and an approval like
//! any other first publish (G1). This module therefore performs **no validation
//! of its own** — it authors a draft, and the publish path judges it. In
//! particular the clone is expected to be *unpublishable* on arrival, because
//! `inst-cl-windows` leaves its billable rows without coverage; that is reported
//! rather than prevented.

use toolkit_db::secure::{AccessScope, DBRunner, DbTx};
use uuid::Uuid;

use crate::domain::audit::AuditStamp;
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{EntitlementGrants, GrantSet};
use crate::domain::error::DomainError;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::plan::PlanShapePatch;
use crate::domain::plan_shape::{PhaseKind, PlanPhase};
use crate::domain::price_record::PriceRecord;
use crate::domain::scope_key::{PhaseId, PlanId, PriceEligibility, ScopeKey};
use crate::infra::storage::repo::{
    NewBundle, NewPlanDraft, NewPriceDraft, bundle_repo, plan_repo, plan_shape_repo, price_repo,
};
use crate::infra::storage::repo_failure;
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;

/// The states a source row is copied from.
///
/// `published` only. A `draft` row of the source belongs to an edit its author
/// has not finished and is not configuration the plan *has*; `superseded` and
/// `retired` rows are history, which `inst-cl-resets` leaves behind for the same
/// reason it leaves windows behind.
const COPIED_ROW_STATES: &[LifecycleState] = &[LifecycleState::Published];

/// What the clone left behind, so the operator learns it from the response
/// rather than from a refused publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneNotice {
    /// `inst-cl-windows`: `PriceWindow` schedules are Slice 7-owned runtime
    /// state (D-03) and are never cloned, so the clone's billable rows have no
    /// coverage and its publish is blocked until the operator schedules some.
    /// Expected, and the reason this is a notice rather than a failure.
    NoCoverageScheduled { rows: usize },
    /// `inst-cl-resets`: `existing_grandfathered` rows are lifecycle state, not
    /// configuration. Copying them under a reset eligibility would collapse two
    /// rows onto one canonical scope key and guarantee a duplicate-scope failure
    /// at the clone's first publish.
    GrandfatheredRowsNotCopied { rows: usize },
    /// `inst-cl-resets` under D-268: `new_subscriptions_only` rows are lifecycle
    /// state for the same two reasons, and the clause named only the first
    /// class. They are made by a cutover rather than authored, they collapse
    /// onto the `all_subscriptions` row's canonical key under the reset exactly
    /// as a grandfathered generation does, and on a clone the class means
    /// nothing anyway: every subscription on a new plan is new.
    NewSubscriptionsOnlyRowsNotCopied { rows: usize },
    /// **D-341: the clone's terminal phase was seeded rather than copied**, because
    /// the source held none — and which of the two things happened to its id, since
    /// the consequences differ.
    ///
    /// The surface said nothing about this. `phases_copied` is `0` (correctly: a
    /// count that folded the seed in would tell an operator a phase came across from
    /// a plan that never had one), so a seeded clone was reported as
    /// `prices_copied: N` plus `NoCoverageScheduled` — which reads as routine
    /// follow-up, while under [`SeededPhaseOrigin::Minted`] it is an unpublishable
    /// draft holding a phase nobody authored. D-341 calls the seeded phase an
    /// operator-visible consequence; this is where the operator sees it.
    TerminalPhaseSeeded {
        /// Where the seeded `phase_id` came from.
        origin: SeededPhaseOrigin,
        /// The copied rows the seed speaks for: **attached** under `Adopted`,
        /// **stranded** under `Minted`. Zero when the source carried no rows at all,
        /// which is a seed with nothing to be about but still a phase the operator
        /// did not author.
        rows: usize,
    },
}

/// Where a seeded clone's terminal `phase_id` came from (**D-341**).
///
/// Two variants rather than a `bool`, and not folded into two [`CloneNotice`]
/// variants either: it is one act — the seed — with two outcomes, and the outcomes
/// are what an operator has to tell apart. `max_struct_bools` is the lint that would
/// have argued the first point eventually; the reason it is right here is that
/// `adopted: false` names the case by what it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeededPhaseOrigin {
    /// The copied rows named exactly one unattached id and the clone **holds** it,
    /// which D-340 makes legal: a phase id belongs to a plan, so the source may keep
    /// it too. Every copied row is attached, and the clone can publish.
    Adopted,
    /// The copied rows named **two or more** distinct ids, so no winner could be
    /// picked without stranding the loser's rows under an id the clone had just
    /// legitimized. A fresh `Uuid::now_v7()`, the rows left exactly as copied, and
    /// every one of them refused by `PHASE_ROW_ORPHANED` until a human resolves which
    /// id the plan is meant to hold. Also the case of a source with no rows at all,
    /// where there is nothing to adopt.
    Minted,
}

/// What a clone produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneReceipt {
    /// The new plan, in `draft` at revision 0.
    pub plan_id: PlanId,
    /// The plan it was copied from, as recorded in `cloned_from`.
    pub cloned_from: PlanId,
    pub phases_copied: usize,
    pub prices_copied: usize,
    pub composites_copied: usize,
    /// What was deliberately left behind; see [`CloneNotice`].
    pub notices: Vec<CloneNotice>,
}

/// What [`copy_rows_on`] did with the source's published price rows.
///
/// Three counts rather than a tuple, because two of the three are exclusions and
/// a caller reading `(usize, usize, usize)` positionally is one transposition
/// away from telling the operator that the wrong class stayed behind.
struct CopiedRows {
    /// Rows written onto the clone.
    copied: usize,
    /// `existing_grandfathered` rows left behind (`inst-cl-resets`).
    grandfathered: usize,
    /// `new_subscriptions_only` rows left behind (D-268).
    new_subscriptions_only: usize,
}

impl CopiedRows {
    /// What the operator is told: one notice per class that had rows, and none
    /// for a class that had none.
    ///
    /// Here rather than inline in [`clone_plan_on`] because the third
    /// arm took that method to a cognitive complexity of 21 against a cap of 20
    /// — and because "what the receipt says" is a different question from "what
    /// the copy did", which is the whole reason the receipt carries notices.
    ///
    /// `seeded` is the phase write's own notice (**D-341**), passed in rather than
    /// computed here because the seed is not something the row copy can see. It goes
    /// **first**: under [`SeededPhaseOrigin::Minted`] it is the difference between a
    /// draft that needs windows scheduled and a draft that cannot publish at all, and
    /// a consumer rendering the head of the list must not lead with the milder fact.
    fn notices(&self, seeded: Option<CloneNotice>) -> Vec<CloneNotice> {
        let mut notices = Vec::from_iter(seeded);
        if self.copied > 0 {
            notices.push(CloneNotice::NoCoverageScheduled { rows: self.copied });
        }
        if self.grandfathered > 0 {
            notices.push(CloneNotice::GrandfatheredRowsNotCopied {
                rows: self.grandfathered,
            });
        }
        if self.new_subscriptions_only > 0 {
            notices.push(CloneNotice::NewSubscriptionsOnlyRowsNotCopied {
                rows: self.new_subscriptions_only,
            });
        }
        notices
    }
}

/// Copy `source`'s **current** revision into `target` as a fresh draft, on a
/// transaction the caller owns.
///
/// **This is the whole clone, and it is the only door.** Steps that were
/// repository *methods*, each opening its own transaction, would leave a failure
/// at the composition copy behind a committed draft plan carrying committed
/// phases, add-on rules, a descriptor set and composites — not prices, which are
/// copied last and are the one child class such a failure could not reach (D-275).
/// Every write below is a runner-taking form on the caller's single transaction,
/// and the reads are on it too, so what the copy reads is what the copy is
/// protected against changing. D-274 built the runner-taking forms this composes;
/// composing them is what makes the clone atomic.
///
/// # One scope, and the reason is a property of this gear's schema
///
/// D-278 split this into a source scope and a target scope, on the ground that a
/// compiled [`AccessScope`] is both the authorization answer and the `SecureORM`
/// row filter and that this route reads plan A while writing plan B. The
/// diagnosis was right and **the remedy was wrong**, which D-279 records: the
/// child tables bind `RESOURCE_ID` to their **own** id — `plan_phase` to
/// `phase_id`, `pricing_price` to `price_id`, `composite_meter` to
/// `composite_id`, `pricing_bundle` to `bundle_id` — so a scope naming a *plan*
/// filters four of the seven tables to **zero rows** rather than to that plan's.
/// A separate source scope could not read the source's own children, and the
/// split turned a loud denial into a silent, empty clone.
///
/// One scope, so an id-shaped answer this gear cannot honour is refused at the
/// first write instead of quietly producing a plan with nothing in it. That is
/// the same behaviour every other plan route has, and the inability to express
/// "this plan's subtree" as a `RESOURCE_ID` constraint is theirs too.
///
/// # The runner is a transaction, and the type is the contract
///
/// `&DbTx<'_>` rather than `&impl DBRunner`, as `retirement::retire_in`,
/// `migration::schedule_in`, `cancel_in` and `synthesis::synthesize_in` already
/// take: a bare connection satisfies `DBRunner`, so stating the requirement in
/// prose left it to whoever read the prose. This function writes a whole plan
/// subtree, and half of one committed by autocommit is the failure the paragraph
/// below is about.
///
/// **And there is deliberately no wrapper that opens one.** A method holding a
/// `DBProvider` and opening its own transaction is *silently nested* when called
/// from inside another one: `Db::in_transaction` does not consult the task-local
/// `IN_TX`, so the inner transaction commits on its own and the outer rollback
/// cannot reach it. Such a wrapper is safe only by accident — if its first
/// statement happens to be `DBProvider::conn()`, which **is** guarded and answers
/// `ConnRequestedInsideTx` — and an accident is not a guard. The alternative,
/// keeping one behind a warning, puts the warning where nobody reads it: at the
/// definition rather than at the call site.
///
/// The target id is the caller's, for `NewPlanDraft`'s stated reason: an
/// authoring surface has to be able to name what it created before the row is
/// durable, and a store that minted the id would make an idempotent retry create
/// a second plan. The *child* ids — phases, prices, composites — are minted here,
/// because no caller can name objects it has not seen.
///
/// # Errors
/// [`DomainError::CloneSourceNotFound`] when `source` has no current revision —
/// §5's `CLONE_SOURCE_NOT_FOUND`, minted by D-278 once the route that returns it
/// existed. Until then it was deliberately absent, D-146's posture: a gear may
/// mint its own error variants, but a **wire code** is the design set's to
/// declare, and minting one ahead of its route is how a code ends up in two
/// spellings. Otherwise whatever the repository forms refuse with.
pub async fn clone_plan_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    source: PlanId,
    target: PlanId,
    now: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<CloneReceipt, DomainError> {
    let current = plan_repo::load_current(runner, scope, tenant_id, source)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| {
            DomainError::CloneSourceNotFound(format!(
                "plan {source} holds no published revision to clone"
            ))
        })?;

    let source_revision = current.revision;
    // **Destructured, so a field added to `PlanRevision` and forgotten here
    // is a compile error** — D-259's remedy, applied to the second place in
    // this crate that rebuilds a revision from another one. It is here
    // because this path was written field by field and **lost
    // `change_contract` on its first draft**: the clone dropped the plan's
    // `allowedChangeTargets`, its `comparabilityRank` and D-113's carry flag,
    // and nothing refused it — with no edges K4 asks for no rank, so the
    // clone published clean and wrong. `open_revision` had already written
    // the comment explaining why an edge list that resets itself is a silent
    // drop; this path did not read it.
    //
    // The five ignored fields are ignored **by name**: the source's own
    // `plan_id` and `revision` (the clone is a different plan starting at 0),
    // its `cloned_from` (lineage is to the source, not through it), and the
    // three provenance fields the create stamps itself.
    let crate::domain::plan::PlanRevision {
        plan_id: _,
        revision: _,
        cloned_from: _,
        sku_id,
        plan_tier,
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
        // **Not copied, and this is an exception `inst-cl-copy` now names**
        // (D-318). A name is an identity label rather than configuration: a
        // clone carrying its source's name puts two identically-named plans in
        // every list, which is the state the column was added to remove. The
        // clone's operator names it with an ordinary draft `PATCH`, and until
        // then it displays by tier exactly as every plan did before the column
        // existed. Its sibling exceptions are `effective_share_bp` and the
        // compiled-allowance grant, both left behind for the same kind of
        // reason: the value belongs to the act, not to the shape.
        plan_name: _,
        lifecycle_state: _,
        created_by: _,
        created_at_utc: _,
        row_version: _,
    } = current;

    let source_phases =
        plan_shape_repo::load_phase_set(runner, scope, tenant_id, source, source_revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    let remap = phase_remap(&source_phases);

    // **Read here rather than inside [`copy_rows_on`], and split here rather than
    // in its loop** (D-341). The seed below adopts the id *the copied rows* name,
    // so the adoption and the copy have to be looking at the same set — one read,
    // one exclusion, one answer. The two cutover-made classes stay behind, so a
    // row this partition leaves out must not vote on the id either: a grandfathered
    // row naming a second phase would make the set look ambiguous and the clone
    // would mint, stranding the rows it did carry.
    let (travelling, copied) = partition_copied(
        price_repo::load_for_plan(runner, scope, tenant_id, source, COPIED_ROW_STATES)
            .await
            .map_err(|e| repo_failure(&e))?,
    );

    let created = plan_repo::create_draft_on(
        runner,
        scope,
        NewPlanDraft {
            plan_id: target,
            tenant_id,
            created_by: stamp.actor_principal_id,
            created_at_utc: now,
            sku_id,
            plan_tier,
            plan_name: None,
            billing_cycle,
            frequency,
            plan_tier_override,
            purchase_min_qty,
            purchase_max_qty,
            invoice_grouping_key,
            available_from,
            available_to,
            cloned_from: Some(source),
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    let revision = created.revision;

    let phase_write = write_phases_on(
        runner,
        scope,
        tenant_id,
        &created,
        SourceShape {
            phases: &source_phases,
            remap: &remap,
            travelling: &travelling,
        },
        created.row_version,
        stamp,
    )
    .await?;
    let mut version = phase_write.version;

    let rules =
        plan_shape_repo::load_addon_rule_set(runner, scope, tenant_id, source, source_revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    if !rules.is_empty() {
        version = plan_shape_repo::replace_addon_rules_on(
            runner, scope, tenant_id, target, revision, version, rules, stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?
        .row_version;
    }

    if let Some(descriptors) =
        plan_shape_repo::load_descriptor(runner, scope, tenant_id, source, source_revision)
            .await
            .map_err(|e| repo_failure(&e))?
    {
        version = plan_shape_repo::set_descriptor_set_on(
            runner,
            scope,
            tenant_id,
            target,
            revision,
            version,
            descriptors,
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?
        .row_version;
    }

    let source_composites =
        plan_shape_repo::load_composite_set(runner, scope, tenant_id, source, source_revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    let composites_copied = source_composites.len();
    if composites_copied > 0 {
        let composites = source_composites
            .into_iter()
            .map(|mut composite| {
                // A new id: the definition is the same, the row is not, and
                // `composite_id` is stable across *revisions of one plan*
                // (D-106) rather than across plans.
                composite.composite_id = Uuid::new_v4();
                composite
            })
            .collect();
        version = plan_shape_repo::replace_composites_on(
            runner, scope, tenant_id, target, revision, version, composites, stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?
        .row_version;
    }

    // D-319's period floor/cap set. Extracted rather than written inline like
    // its four siblings above, and the reason is measured: the sixth copy took
    // this function past clippy's cognitive-complexity bar (21/20). Whichever
    // child set is the seventh should follow it out.
    version = copy_period_bounds_on(
        runner,
        scope,
        tenant_id,
        (source, source_revision),
        (target, revision),
        version,
        stamp,
    )
    .await?;

    // **A bundle rides its plan's revisions, so a clone of a bundle is a
    // bundle** (D-269). Ahead of the patch below because the composition
    // write is a compare-and-swap on the same revision tag, and it returns
    // the version the patch then has to hold.
    version = copy_bundle_on(
        runner,
        scope,
        tenant_id,
        (source, source_revision),
        (target, revision),
        version,
        stamp,
    )
    .await?;

    // **The two authored facts `NewPlanDraft` cannot express**, patched onto
    // the created draft: the grant set with the per-phase map's keys remapped
    // (C-7), and the plan-change contract, which the create path drops
    // because its struct has no field for it.
    plan_repo::update_draft_on(
        runner,
        scope,
        tenant_id,
        target,
        revision,
        version,
        PlanShapePatch {
            entitlement_grants: Some(remapped_grants(&entitlement_grants, &remap)),
            change_contract: Some(change_contract),
            ..PlanShapePatch::default()
        },
        stamp,
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Box::pin(copy_rows_on(
        runner,
        scope,
        tenant_id,
        target,
        &travelling,
        &remap,
        now,
        stamp,
    ))
    .await?;

    Ok(CloneReceipt {
        plan_id: target,
        cloned_from: source,
        // The **copies**, so a source that held none reports zero even though the
        // clone now holds a phase: D-341's row is seeded, not copied, and a count
        // that folded the two together would tell an operator a phase came across
        // from a plan that never had one. The seed is reported as a **notice**
        // instead, which is where a fact with two different consequences belongs.
        phases_copied: source_phases.len(),
        prices_copied: copied.copied,
        composites_copied,
        notices: copied.notices(phase_write.notice),
    })
}

/// What [`write_phases_on`] did: the row version the next write has to hold, and the
/// notice the seed owes the operator.
///
/// Named for `SourceShape`'s reason one paragraph up — a `(RowVersion,
/// Option<CloneNotice>)` return is two unrelated values a caller destructures
/// positionally — and because `notice` is `None` on the ordinary copy path, which is
/// a fact worth having a field name say.
struct PhaseWrite {
    /// The revision's row version after the write. **Unchanged** on the seed path:
    /// `seed_terminal_phase_on` writes the child row and nothing else.
    version: RowVersion,
    /// D-341's notice, or `None` when the source's phases were copied.
    notice: Option<CloneNotice>,
}

/// What the phase write reads off the source revision.
///
/// Named rather than a tuple of three references. The tuple form was reached for to
/// keep `write_phases_on`'s argument count down and bought `clippy::type_complexity`
/// instead — and two of the three members are slices, which is the arrangement a
/// caller silently transposes.
struct SourceShape<'a> {
    /// The source's own phase set. Empty is the case D-341 exists for.
    phases: &'a [PlanPhase],
    /// Source `phase_id` → the id the copy files it under (D-19's remap).
    remap: &'a BTreeMap<Uuid, PhaseId>,
    /// The rows that will travel, which is where an adopted id comes from.
    travelling: &'a [PriceRecord],
}

/// The clone's phase set: the source's rows under new ids, or **D-341's seed** when
/// the source holds none.
///
/// Returns the row version the next write has to hold.
///
/// # Two creation paths, one seeding call
///
/// `inst-ph-default` is a **creation-time** act — every plan gets a terminal phase
/// row when it is created — and a clone creates a plan. The copy wrote phase rows
/// only when the source had some, so a clone of a phase-less source was born in
/// exactly the state the act abolishes, reached through the one creation path that
/// is not `POST /plans`. That is D-269's shape a second time in this function's copy
/// set: two paths that create a plan disagreeing about what a plan is.
///
/// The seed therefore goes through [`plan_shape_repo::seed_terminal_phase_on`] —
/// the function `POST /plans` calls, on the runner this clone already holds — and
/// not through `replace_phases_on`. It is the *same* call rather than an equivalent
/// one on purpose: the drift, not the missing row, is what D-341 is written against.
/// It also leaves the revision's `row_version` alone, writing the child row and
/// nothing else, so the next write still holds the version the create answered.
///
/// A phase-less source is not hypothetical. Every plan authored before the seed
/// existed is one, and `inst-cl-source` makes a plan clonable exactly when it holds
/// a **current** revision — so such a source is *published* while the clone it used
/// to produce could not publish at all, its copied rows refused row by row
/// (`PHASE_ROW_ORPHANED`) with nothing to attach and no remedy but deletion, a scope
/// key being the row's identity.
///
/// # Errors
/// Whatever the shape repository refuses with.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the runner, the scope, the tenant, \
              the revision the create answered, the source's own phase set with the remap and the \
              rows that will travel, and the version and stamp the compare-and-swap takes. \
              `copy_rows_on` below carries the same allow for the same reason"
)]
async fn write_phases_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    created: &crate::domain::plan::PlanRevision,
    source: SourceShape<'_>,
    version: RowVersion,
    stamp: AuditStamp,
) -> Result<PhaseWrite, DomainError> {
    let SourceShape {
        phases: source_phases,
        remap,
        travelling,
    } = source;
    if source_phases.is_empty() {
        let (phase, origin) = seeded_terminal_phase(travelling);
        plan_shape_repo::seed_terminal_phase_on(runner, scope, tenant_id, created, &phase)
            .await
            .map_err(|e| repo_failure(&e))?;
        return Ok(PhaseWrite {
            version,
            // The count is the travelling rows' own, which is what makes the notice
            // true under either origin: an adopted id attaches every one of them, and
            // a minted one strands every one of them, the ids they name all being
            // absent from a source that held no phase row.
            notice: Some(CloneNotice::TerminalPhaseSeeded {
                origin,
                rows: travelling.len(),
            }),
        });
    }
    let phases = source_phases
        .iter()
        .map(|phase| remapped_phase(phase, remap))
        .collect();
    Ok(PhaseWrite {
        version: plan_shape_repo::replace_phases_on(
            runner,
            scope,
            tenant_id,
            created.plan_id,
            created.revision,
            version,
            phases,
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?
        .row_version,
        notice: None,
    })
}

/// D-341's terminal phase for a clone whose source held none: the id its copied
/// rows name when they name exactly one, a fresh id otherwise.
///
/// **Adoption is what D-340 makes legal.** A phase id belongs to a plan now
/// (`(tenant_id, plan_id, plan_revision, phase_id)`), so the clone may hold the id
/// its source's rows name while the source keeps it too. Before that widening
/// adoption was not an option to weigh — it *was* the collision, answered `500`.
/// Nothing checks that the id is unattached because nothing has to: this is called
/// only where the source holds no phase row at all, so every id the rows name is
/// unattached by construction.
///
/// **The condition is on the distinct set, not on the first row read.** Two or more
/// ids is a state a human must resolve, and a clone that silently picked a winner
/// would strand the loser's rows under an id it had just legitimized — a worse
/// artefact than the ambiguity, because the refusal would then name rows on a phase
/// the plan visibly attaches. So it mints and leaves the rows as copied, where
/// `PHASE_ROW_ORPHANED` names each of them and the operator learns there were two.
///
/// The `kind`, `ordinal` and the two absent day counts are `POST /plans`' seed
/// verbatim: an implicit terminal row is `evergreen`, first, and converts to
/// nothing, and a duration on it would date a conversion that never happens.
///
/// The [`SeededPhaseOrigin`] rides back out because which branch was taken is a fact
/// about the artefact and not about this function: under `Minted` the clone holds a
/// phase nobody authored **and** rows nothing attaches, which is what the receipt's
/// notice tells the operator.
fn seeded_terminal_phase(travelling: &[PriceRecord]) -> (PlanPhase, SeededPhaseOrigin) {
    let named: Vec<Uuid> = travelling
        .iter()
        .map(|row| row.scope_key.phase().get())
        .collect::<BTreeSet<Uuid>>()
        .into_iter()
        .collect();
    let (phase_id, origin) = match named.as_slice() {
        [single] => (PhaseId::new(*single), SeededPhaseOrigin::Adopted),
        // None — a source with no rows at all — and two or more both mint.
        _ => (PhaseId::new(Uuid::now_v7()), SeededPhaseOrigin::Minted),
    };
    (
        PlanPhase {
            phase_id,
            kind: PhaseKind::Evergreen,
            display_name: None,
            ordinal: 0,
            converts_to_phase_id: None,
            phase_duration_days: None,
            display_trial_days: None,
        },
        origin,
    )
}

/// The source's published rows split into the ones that travel with the clone and
/// the count of each cutover-made class that stays behind (D-268).
///
/// **One spelling of the exclusion**, which is why this is a function rather than
/// two matches: D-341's seed adopts the id the *copied* rows name, so the adoption
/// and the copy must agree about which rows those are. The match is exhaustive
/// rather than two `if`s so that a fourth class added to [`PriceEligibility`] has to
/// be classified here instead of defaulting into the copy.
///
/// `copied` is the travelling rows' own count, so the receipt cannot disagree with
/// what was written: the copy below writes every row it is handed or fails.
fn partition_copied(source_rows: Vec<PriceRecord>) -> (Vec<PriceRecord>, CopiedRows) {
    let mut travelling = Vec::new();
    let mut counts = CopiedRows {
        copied: 0,
        grandfathered: 0,
        new_subscriptions_only: 0,
    };
    for row in source_rows {
        match row.scope_key.price_eligibility() {
            PriceEligibility::ExistingGrandfathered => counts.grandfathered += 1,
            PriceEligibility::NewSubscriptionsOnly => counts.new_subscriptions_only += 1,
            PriceEligibility::AllSubscriptions => {
                counts.copied += 1;
                travelling.push(row);
            }
        }
    }
    (travelling, counts)
}

/// Copy the source revision's period floor/cap set onto the clone (**D-319**).
///
/// Verbatim: nothing is re-minted, because the key is the market pair and a
/// market means the same thing on the clone as on the source — unlike a
/// `composite_id`, which is stable across revisions of one plan rather than
/// across plans (D-106). Whether the clone actually sells those markets is
/// `PERIOD_FLOOR_CAP_MARKET_UNSOLD`'s question at its first publish, and the
/// clone copies the source's price rows, so the answer is normally the source's.
///
/// Returns the row version the next write has to hold — the caller's, unchanged,
/// when the source authored no bound.
async fn copy_period_bounds_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    from: (PlanId, u64),
    to: (PlanId, u64),
    version: RowVersion,
    stamp: AuditStamp,
) -> Result<RowVersion, DomainError> {
    let (source, source_revision) = from;
    let (target, revision) = to;
    let bounds = plan_shape_repo::load_period_floor_cap_set(
        runner,
        scope,
        tenant_id,
        source,
        source_revision,
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    if bounds.is_empty() {
        return Ok(version);
    }
    Ok(plan_shape_repo::replace_period_floor_caps_on(
        runner, scope, tenant_id, target, revision, version, bounds, stamp,
    )
    .await
    .map_err(|e| repo_failure(&e))?
    .row_version)
}

/// Copy the source bundle's identity and composition onto the clone (D-269).
///
/// Returns the clone revision's row version — **unchanged** when the source is
/// not a bundle, which is the ordinary answer: the overwhelming majority of plans
/// are not bundles, and `BundleRepo`'s three reads take the same posture,
/// answering `None` or an empty composition rather than refusing. Its *writer*
/// does not — `replace_composition_on` answers `NotFound` for a plan carrying no
/// bundle — which is why the absence is decided here, by the read, before any
/// write is attempted.
///
/// The `bundle_id` is **new**. `pricing_bundle.plan_id` is unique per plan, so the
/// identity cannot be shared; and it is the bundle's own identity rather than a
/// revision-scoped row, exactly as `composite_id` is (D-106). What is *not*
/// re-minted is `component_plan_id`: those name **other** plans, which the clone
/// did not copy and must not repoint.
///
/// Its own function rather than a branch inside [`clone_plan_on`], which is
/// already at the cognitive-complexity cap, and because the two plans arrive here
/// as a pair with the revision each is read at.
///
/// # Errors
/// Whatever the bundle repository refuses with.
///
/// Not "including `StaleRowVersion` when a concurrent writer moved the clone's own
/// draft": the composition makes that unreachable. The clone's draft is inserted
/// inside this same uncommitted transaction, so no other session can see the row,
/// let alone bump its version.
/// The compare-and-swap still runs, and still has to: it is the one guard that
/// would answer if `runner` were ever a bare connection rather than a
/// transaction, which is the misuse [`clone_plan_on`] documents.
async fn copy_bundle_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    from: (PlanId, u64),
    onto: (PlanId, u64),
    version: RowVersion,
    stamp: AuditStamp,
) -> Result<RowVersion, DomainError> {
    let (source, source_revision) = from;
    let (target, revision) = onto;
    let Some(bundle) = bundle_repo::find_by_plan_on(runner, scope, tenant_id, source)
        .await
        .map_err(|e| repo_failure(&e))?
    else {
        return Ok(version);
    };
    let composition =
        bundle_repo::load_composition_on(runner, scope, tenant_id, source, source_revision)
            .await
            .map_err(|e| repo_failure(&e))?;
    bundle_repo::create_on(
        runner,
        scope,
        NewBundle {
            bundle_id: Uuid::new_v4(),
            tenant_id,
            plan_id: target,
            price_basis: bundle.price_basis,
            invoice_itemization: bundle.invoice_itemization,
        },
        stamp,
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(bundle_repo::replace_composition_on(
        runner,
        scope,
        tenant_id,
        target,
        revision,
        version,
        composition,
        stamp,
    )
    .await
    .map_err(|e| repo_failure(&e))?
    .row_version)
}

/// Write the travelling rows onto the clone, resetting each.
///
/// Separate from [`clone_plan_on`] because it is the *rows* rather than the shape.
/// It used to load and classify them too; both moved out to
/// [`partition_copied`] so that D-341's seed and this copy
/// read one set — the exclusion is a decision about which rows the clone carries,
/// and two callers of that decision must not each make it.
///
/// # Errors
/// Whatever the price repository refuses with, and
/// [`DomainError::ValidationFailed`] if a reset key is not constructible.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the runner, the scope, the tenant, \
              the target plan, the rows that travel, the phase remap the shape copy already built, \
              the clone instant and the D-135 audit stamp. `plan_repo::update_draft_on` carries the \
              same allow for the same reason"
)]
async fn copy_rows_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    target: PlanId,
    travelling: &[PriceRecord],
    remap: &BTreeMap<Uuid, PhaseId>,
    now: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<(), DomainError> {
    for row in travelling {
        Box::pin(price_repo::create_draft_on(
            runner,
            scope,
            tenant_id,
            NewPriceDraft {
                price_id: Uuid::new_v4(),
                scope_key: reset_key(&row.scope_key, target, remap)?,
                content: reset_content(row),
                created_by: stamp.actor_principal_id,
                created_at_utc: now,
                correlation_id: stamp.correlation_id,
            },
        ))
        .await
        .map_err(|e| repo_failure(&e))?;
    }
    Ok(())
}

/// Old `phase_id` -> new `phase_id`, one entry per source phase.
fn phase_remap(phases: &[PlanPhase]) -> BTreeMap<Uuid, PhaseId> {
    phases
        .iter()
        .map(|phase| (phase.phase_id.get(), PhaseId::new(Uuid::new_v4())))
        .collect()
}

/// A phase under its new id, with its conversion target remapped too.
///
/// A `converts_to_phase_id` the map does not know is left as it stands rather
/// than dropped: the chain is `PhaseGraph`'s to judge, and a cloner that silently
/// repaired a broken source chain would hide the fault instead of copying it.
fn remapped_phase(phase: &PlanPhase, remap: &BTreeMap<Uuid, PhaseId>) -> PlanPhase {
    let mut copy = phase.clone();
    if let Some(new_id) = remap.get(&phase.phase_id.get()) {
        copy.phase_id = *new_id;
    }
    copy.converts_to_phase_id = phase
        .converts_to_phase_id
        .map(|target| remap.get(&target.get()).copied().unwrap_or(target));
    copy
}

/// The grant set with its per-phase keys moved onto the clone's phases (C-7).
///
/// A key the map does not know is **kept as it stands**, for `remapped_phase`'s
/// reason: `GrantSetPhasesKnown` is what judges a dangling key, and dropping it
/// here would silence the very refusal the clone's first publish owes the author.
fn remapped_grants(
    grants: &EntitlementGrants,
    remap: &BTreeMap<Uuid, PhaseId>,
) -> EntitlementGrants {
    EntitlementGrants {
        plan_tier_ref: grants.plan_tier_ref.clone(),
        plan_level: grants.plan_level.clone(),
        per_phase: grants
            .per_phase
            .iter()
            .map(|(phase_id, set)| {
                let key = remap.get(phase_id).map_or(*phase_id, |id| id.get());
                (key, set.clone())
            })
            .collect::<BTreeMap<Uuid, GrantSet>>(),
    }
}

/// The copied row's key: the clone's plan, the clone's phase, eligibility reset.
///
/// `inst-cl-resets`: `priceEligibility` goes to `all_subscriptions` because
/// eligibility must be re-decided, and the cohort follows it to `none` — the two
/// are one fact, and `ScopeKey::new` refuses the pair that disagrees.
///
/// **Both resets are structural fences and neither has an operand** since D-268:
/// the only two classes that could carry another value are excluded before a row
/// reaches here, so every key this sees already reads `all_subscriptions` /
/// `none`. Kept because a later change admitting either class would need them —
/// the same posture D-266 took for the cohort and `grandfather_until` — and not
/// claimed as behaviour a test proves.
fn reset_key(
    key: &ScopeKey,
    target: PlanId,
    remap: &BTreeMap<Uuid, PhaseId>,
) -> Result<ScopeKey, DomainError> {
    let phase = remap
        .get(&key.phase().get())
        .copied()
        .unwrap_or(key.phase());
    let reset = ScopeKey::new(
        target,
        key.currency().clone(),
        key.region().clone(),
        phase,
        PriceEligibility::AllSubscriptions,
        key.charge_kind(),
        crate::domain::scope_key::Cohort::None,
    )?;
    reset.with_usage_line(key.meter().cloned(), key.dimension_key().clone())
}

/// The copied row's content, with the two lifecycle fields cleared.
///
/// `grandfather_until` is a cutover's tombstone on the source row and says
/// nothing about the clone; `supersedes_price_id` names a row in the *source's*
/// chain, and a clone's first row supersedes nothing.
fn reset_content(row: &PriceRecord) -> crate::domain::price_record::PriceContent {
    let mut content = row.content();
    content.grandfather_until = None;
    content.supersedes_price_id = None;
    content
}
