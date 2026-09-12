//! `PATCH /bss-pricing/v1/prices/{priceId}/grandfather-until` — the horizon door
//! (`inst-gs-bound`, `inst-gs-tighten`, S7 §5).
//!
//! The authoring surface for the grandfathered-row eligibility machine's two
//! stored transitions. Until it landed, S7 §4's machine had **no producer at all**:
//! the route was declared and unmounted, and no repository wrote
//! `grandfather_until` on a published row — a cutover *inserts* the generation
//! carrying whatever horizon it was composed with, `update_draft` swaps on
//! `lifecycle_state = 'draft'`, and `insert_successor_draft_on` refuses the
//! grandfathered class outright. So `active_indefinite → active_bounded` and
//! `active_bounded → active_bounded` were states a row could be *born* in and
//! never move between.
//!
//! `infra::window::mutate_in`'s shape, step for step, because a horizon tightening
//! is the same kind of act as a window mutation — an operator move on published
//! content, always material, one publish unit, one transaction. What differs is
//! listed here and nowhere else.
//!
//! # It moves the operand D-04 protects, so it asks D-04's own question
//!
//! `refuse_horizon_uncovered` asks `[max(cohort, now), grandfatherUntil + W6's
//! margin)` of every act `mutate_in` performs (D-316 clause (1)). This door writes
//! the **right-hand end of that span**, which no window act does — so asking the
//! rule against the stored horizon would judge the state this act is about to
//! discard. It is asked against the **proposed** horizon instead, over the key's
//! unmoved interval set:
//! [`refuse_horizon_span_uncovered`](crate::infra::window::refuse_horizon_span_uncovered).
//!
//! **Most of what this door does could not break the bound, and one thing can.**
//! Tightening a finite horizon moves the far end of the span *inwards*: the floor
//! `horizon + margin` falls, the walk has less to cover, and coverage that
//! satisfied the bound still satisfies it. The exception is `inst-gs-bound`.
//! A null horizon is judged by an arm that never consults the margin at all — it
//! asks only for an unbroken run into an open interval — while every finite horizon
//! is judged by an arm that **requires** the margin to have a value. So a
//! generation on a market where the plan sells recurring and authors no frequency
//! passes indefinitely and is refused the moment a bound is set. That is the one
//! shape where this act can newly break D-04, it is the reason the guard is called
//! rather than argued away, and `a_generation_whose_plan_has_no_margin_may_not_be_bounded`
//! is armed against exactly it.
//!
//! Calling it rather than reasoning about it is also the standing lesson of D-316's
//! own residual, which records four write paths that reach the window plane around
//! `refuse_horizon_uncovered` and are held shut *"by an argument rather than by a
//! structure"* — and names the day one of those arguments stops holding as the day
//! the defect arrives. A fifth path is not something to add to that list.
//!
//! # It records a before-image, which is D-327's lesson taken rather than repeated
//!
//! D-327 recorded that D-05's cutover unwind could not restore a shortened
//! `effectiveTo` because **no before-image was recorded** anywhere: the cutover
//! reaches `window_repo::adjust_effective_to` directly, that is an in-place `UPDATE`
//! of a table with no before-image column, and the operand the unwind needs
//! therefore did not exist. A horizon tightening is an in-place `UPDATE` of the
//! same kind, so it would have been the next instance of that defect.
//!
//! **The cutover's own instance is repaired**, by this section's
//! remedy applied where D-327 found the defect: `infra::cutover::record_cutover` and
//! `infra::supersession`'s step 11 now carry the shortened window's interval as a
//! value, and `audit_repo::for_subject` reads it back. So the account below is a
//! description of a shared arrangement rather than of an exception this module
//! makes.
//!
//! It is not, because the audit record carries `before_state` and `after_state` —
//! `infra::window::mutate_in`'s arrangement, which pays the same debt for the
//! window plane through `pricing_audit_log` rather than through a column no
//! decision names. The prior horizon is therefore recoverable from the trail by
//! anything that ever needs to reverse one of these acts. What this crate does
//! **not** do is offer a reversal: `active_bounded` has no edge back to
//! `active_indefinite` and a later horizon is `GRANDFATHER_LOOSEN_FORBIDDEN`, so
//! the before-image is evidence rather than an undo. That is still a deliberate
//! difference from the cutover's case, where the unwind is a *decided* clause —
//! it now has its operand and waits on `unwound`, D-333's approval state, whose
//! store column is not built.
//!
//! # No event, and no code minted
//!
//! S7 §4's transition list declares no event for either edge, and `inst-el-expiry`
//! says in as many words that the expiry side needs *"no job, no new event (§7
//! holds)"*. The one event that superficially fits — `PriceUpdated` — is the
//! **supersession's**, deduplicated on the successor's id alone under the argument
//! that *"a row is superseded-onto exactly once"*, so a second tightening of one
//! row would collapse into the first. Minting a token for this act would be this
//! crate declaring a wire fact the design set does not, which D-204 clause (2)
//! refuses. Consumers therefore see the new horizon exactly as D-06 has them see an
//! overlay or a membership: at `CatalogVersionPublished` plus warm, through the
//! publish unit this act records — and `domain::projection` does carry
//! `grandfatherUntil`, which is what makes the publish unit necessary rather than
//! ceremonial.
//!
//! The two refusals of this door's own — `GRANDFATHER_LOOSEN_FORBIDDEN` and
//! `GRANDFATHER_UNTIL_FORBIDDEN` — are both **declared** by S7 §5. The third code
//! §5 declares for this surface, `GRANDFATHERED_ROW_IMMUTABLE`, has no producer
//! here and is reported rather than co-opted: §5 glosses none of the three, and
//! Foundation §4.3's sentence that a grandfathered generation *"is immutable in
//! price and may not be superseded; only its grandfatherUntil may be tightened"*
//! puts that code on the acts this door refuses to be — which is
//! `price_repo::refuse_unsupersedable_class`'s ground, and that refusal already
//! carries `LIFECYCLE_FORBIDDEN`.

use std::sync::Arc;

use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::concurrency::RowVersion;
use crate::domain::coverage;
use crate::domain::error::DomainError;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::{self, ChangeSet, PublishedPriceBaseline};
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::price_record::PriceRecord;
use crate::domain::read_model::SubjectRef;
use crate::domain::scope_key::{PlanId, PriceEligibility, ScopeKey};
use crate::domain::window::{KeyWindows, WindowInterval};
use crate::infra::publish::assemble_from;
use crate::infra::registry_deadline::request_version_now;
use crate::infra::storage::repo::audit_repo::NewAuditEntry;
use crate::infra::storage::repo::catalog_version_ref_repo::PendingVersionRow;
use crate::infra::storage::repo::{
    audit_repo, catalog_version_ref_repo, plan_repo, price_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

use crate::domain::instant::format_rfc3339;
/// The renderer the surface hands in — `infra::window`'s type, imported rather
/// than re-declared, so a unit opened on this path and one opened on the window
/// path cannot store two different documents.
pub use crate::infra::window::VerdictJson;

/// What the door answers.
///
/// Two arms and `202` on both, exactly as the cutover and the window mutations
/// answer: the committed arm has requested a `CatalogVersion` that has not been
/// published yet, and the controlled arm has changed nothing at all.
#[derive(Debug, Clone)]
pub enum HorizonOutcome {
    /// The tightening committed under an approval this call resolved.
    Committed(Box<HorizonReceipt>),
    /// A unit was opened and **nothing was written** beyond it.
    SubmittedForApproval(Box<HorizonPending>),
}

/// The committed act, as the caller is told about it.
#[derive(Debug, Clone)]
pub struct HorizonReceipt {
    /// The plan whose subject re-projects.
    pub plan_id: PlanId,
    /// The plan revision this act freezes — the plan's **current** one, which a
    /// content change on a published row never advances.
    pub revision: u64,
    /// The generation whose horizon moved.
    pub price_id: Uuid,
    /// That generation's canonical scope key, rendered.
    pub scope_key: ScopeKey,
    /// The horizon the store held before this act. `None` is D-147's indefinite.
    pub prior_grandfather_until: Option<OffsetDateTime>,
    /// The horizon it holds now.
    pub grandfather_until: OffsetDateTime,
    /// The registry's pending handle.
    pub pending_version_ref: String,
    /// The row's entity tag — **unmoved**, and deliberately echoed so a caller can
    /// see that it is. D-141 freezes `row_version` with a published row's content.
    pub row_version: RowVersion,
}

/// The unit a second principal has to decide, and the state the caller will still
/// find.
#[derive(Debug, Clone)]
pub struct HorizonPending {
    /// The plan whose subject would re-project.
    pub plan_id: PlanId,
    /// Its current revision.
    pub revision: u64,
    /// The generation named.
    pub price_id: Uuid,
    /// Its canonical scope key.
    pub scope_key: ScopeKey,
    /// The **stored** horizon, because nothing moved.
    pub prior_grandfather_until: Option<OffsetDateTime>,
    /// What was asked for.
    pub proposed_grandfather_until: OffsetDateTime,
    /// The row's entity tag, unmoved.
    pub row_version: RowVersion,
    /// The opened unit.
    pub approval: crate::infra::storage::repo::approval_repo::ApprovalRecord,
}

/// The subject an approval of this act is opened under (D-184).
///
/// `<plan>/<priceId>/grandfather-until/<prior>/<new>` — the window act's shape with
/// the row in the window's place. Both ends are in it, which is what makes an
/// approval answer for *one* transition: a unit taken when the horizon read `T`
/// cannot complete after somebody else has moved it to `T'`, because the retry
/// renders a different subject and `authorizing_unit` finds nothing.
///
/// `none` for an absent prior rather than an empty segment, so the string never has
/// two adjacent separators and a reviewer reading it can tell `inst-gs-bound` from
/// `inst-gs-tighten` at a glance.
#[must_use]
pub fn horizon_unit_ref(
    plan_id: PlanId,
    price_id: Uuid,
    prior: Option<OffsetDateTime>,
    proposed: OffsetDateTime,
) -> String {
    format!(
        "{plan_id}/{price_id}/grandfather-until/{}/{}",
        prior.map_or_else(|| "none".to_owned(), format_rfc3339),
        format_rfc3339(proposed)
    )
}

/// The registry's idempotency handle for one horizon tightening.
///
/// `infra::window::unit_request_id`'s lesson applied rather than its spelling
/// reused: the handle has to name the **act**, not the row. A plan's revision does
/// not advance on a content change to a published row, so `(kind, tenant, plan,
/// revision, price)` would render one string for every tightening of one row —
/// the registry is contractually idempotent on `request_id`, the second act would
/// be handed the first's pending ref, and `record_pending`'s plain `INSERT` would
/// meet `PRIMARY KEY (tenant_id, pending_ref)`. That is the defect D-190 found on
/// the window plane, reached here through the same door.
///
/// The first segment is `grandfather-until/`, which is what keeps this requester's
/// handles apart from the six others sharing the one registry `Arc`. It does not
/// come from `PublishUnitKind::request_token` for `infra::supersession`'s stated
/// reason: S5 §6 declares no subject kind for it and this gear mints none.
fn unit_request_id(
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    price_id: Uuid,
    prior: Option<OffsetDateTime>,
    proposed: OffsetDateTime,
) -> String {
    format!(
        "grandfather-until/{tenant_id}/{plan_id}/{revision}/{price_id}/{}/{}",
        prior.map_or_else(|| "none".to_owned(), format_rfc3339),
        format_rfc3339(proposed)
    )
}

/// Every fact the validation needs, read inside the writing transaction (D-176).
struct HorizonContext {
    plan_id: PlanId,
    revision: u64,
    lifecycle_state: LifecycleState,
    generation: PriceRecord,
    key: ScopeKey,
    windows: KeyWindows,
    margin: Option<time::Duration>,
    published: Vec<PriceRecord>,
    shape: crate::domain::plan_shape::PlanShape,
}

/// The horizon door's workflow — one act, one transaction.
#[derive(Clone)]
pub struct GrandfatherService {
    db: DBProvider<DbError>,
    registry: Arc<dyn CatalogVersionRegistryV1>,
}

impl GrandfatherService {
    /// Build the workflow over one provider and the resolved registry.
    ///
    /// The **same** registry `Arc` every other requester holds; what keeps their
    /// handles apart is [`unit_request_id`]'s distinct first segment.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>, registry: Arc<dyn CatalogVersionRegistryV1>) -> Self {
        Self { db, registry }
    }

    /// Tighten one published generation's `grandfatherUntil`, or open the unit that
    /// would let it.
    ///
    /// # Errors
    /// [`tighten_in`]'s, exactly.
    #[allow(
        clippy::too_many_arguments,
        reason = "[`tighten_in`]'s list minus the runner and the registry it supplies itself: the \
                  caller's context and compiled scope, the tenant, the row and the instant that \
                  together name the act, the presented tag, the verdict renderer and the stamp. \
                  `WindowService::adjust_effective_to` carries nine for the same reason"
    )]
    pub async fn tighten(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_id: Uuid,
        proposed: OffsetDateTime,
        expected: RowVersion,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<HorizonOutcome, DomainError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        let registry = Arc::clone(&self.registry);
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<HorizonOutcome, DomainError, _>(move |txn| {
                Box::pin(async move {
                    tighten_in(
                        txn,
                        &registry,
                        &ctx,
                        &scope,
                        tenant_id,
                        price_id,
                        proposed,
                        expected,
                        verdict_json,
                        stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: horizon transaction: {infra}"))
            })
        })
    }
}

/// The whole act, inside one transaction the caller supplies.
///
/// The order is `infra::window::mutate_in`'s and each step is where it is for that
/// function's stated reason; only the differences are commented here.
#[allow(
    clippy::too_many_arguments,
    reason = "`infra::window::mutate_in`'s list and its reason: the runner, the registry, the \
              caller's context and compiled scope, the tenant, the row and the instant that \
              together name the act, the presented tag, the verdict renderer and the stamp. \
              Grouping them into a request type would move the list rather than shorten it"
)]
async fn tighten_in(
    runner: &impl DBRunner,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    proposed: OffsetDateTime,
    expected: RowVersion,
    verdict_json: VerdictJson,
    stamp: AuditStamp,
) -> Result<HorizonOutcome, DomainError> {
    let now = stamp.recorded_at;
    // 1. Every fact the validation needs, read inside the transaction that writes.
    let context = read_horizon_context(runner, scope, tenant_id, price_id, now).await?;
    let prior = context.generation.grandfather_until;

    // 2. The row has to be a **published generation**, and the two halves of that
    // are two refusals with two remedies.
    //
    // The **state first**, and the order is a decision rather than an accident of
    // writing. A draft row's `grandfatherUntil` is ordinary authored content and
    // moves through `PATCH.../plans/{planId}/prices/{priceId}` under the row's own
    // tag; this door exists for the published plane, where that path's swap guard
    // (`lifecycle_state = 'draft'`) cannot reach. So a caller who arrived at the
    // wrong door has to be told *that*, and the class question is one only a row
    // this door could act on has an answer to: asked first, it told the author of a
    // draft `all_subscriptions` row to clear a field they may not carry, which is a
    // remedy that does not lead anywhere — measured, on this probe's first run.
    // `LIFECYCLE_FORBIDDEN` because Foundation §3.3 is where an unsanctioned
    // mutation of a row's content lands.
    if context.generation.lifecycle_state != LifecycleState::Published {
        return Err(DomainError::LifecycleForbidden(format!(
            "price row {price_id} is {}; the grandfathering horizon door addresses the published \
             generation, and a draft row's grandfatherUntil is authored through PATCH \
             /bss-pricing/v1/plans/{{planId}}/prices/{{priceId}} with the rest of its content",
            context.generation.lifecycle_state.as_str()
        )));
    }
    // Then the class (D-147): a horizon on any other class names a moment nothing
    // observes, and S7 §4's entry condition is what admits a row to this machine at
    // all — which is why neither `inst-gs-bound` nor `inst-gs-tighten` states the
    // test, and why it is asked here rather than inside
    // `domain::grandfather::check_tightening`.
    if context.key.price_eligibility() != PriceEligibility::ExistingGrandfathered {
        return Err(DomainError::GrandfatherUntilForbidden(format!(
            "price row {price_id} is on eligibility class {}, and only an existing_grandfathered \
             generation carries a grandfathering horizon; there is nothing on this key for a \
             horizon to expire",
            context.key.price_eligibility()
        )));
    }
    // 3. D-141's precondition, compared here rather than in the store, because the
    // store's swap is on the horizon and not on this column — see
    // `price_repo::tighten_grandfather_until`. The tag is frozen with a published
    // row's content, so what this refuses is a caller addressing a version the row
    // never had; it cannot refuse a caller whose horizon is stale, and the swap is
    // what does that.
    if context.generation.row_version != expected {
        return Err(DomainError::StaleVersion(format!(
            "price row {price_id}: the presented If-Match is {}, and this row stands at {}",
            expected.get(),
            context.generation.row_version.get()
        )));
    }
    // 4. The machine's own edge (`inst-gs-bound`, `inst-gs-tighten`).
    crate::domain::grandfather::check_tightening(prior, proposed)?;

    // 5. `inst-co-single-pending` over the one key this act moves, before anything
    // is touched — `infra::window::refuse_pending_key_holder`'s rule and its one
    // spelling.
    let held = std::collections::BTreeSet::from([context.key.to_string()]);
    crate::infra::approval::refuse_held_key(runner, scope, tenant_id, &held).await?;

    // 6. D-04's span, asked of the horizon this act would leave behind. The
    // interval set is unmoved — this act schedules, shortens and cancels nothing —
    // so what changes between the stored question and this one is the far end
    // alone. The module doc says which direction that can break in.
    crate::infra::window::refuse_horizon_span_uncovered(
        &context.key,
        Some(proposed),
        context.margin,
        &context.windows,
        now,
    )?;

    // 7. The two-person control, after every check above and before anything is
    // written. Always material: the act is `inst-mat-registered`'s first clause, so
    // the evaluator is consulted for the **record** rather than for the answer —
    // and it is consulted rather than short-circuited so the verdict a reviewer
    // reads years later was produced by the one evaluator.
    let verdict = materiality::evaluate(
        &ChangeSet::of_horizon_tightening(context.published.iter().cloned()),
        crate::infra::threshold::effective_policy_at(runner, scope, tenant_id, now)
            .await?
            .as_ref(),
        context
            .shape
            .baseline
            .is_some()
            .then(|| PublishedPriceBaseline::of_records(context.published.iter().cloned()))
            .as_ref(),
    );
    let subject_ref = horizon_unit_ref(context.plan_id, price_id, prior, proposed);
    let authorized = crate::infra::approval::authorizing_unit(
        runner,
        scope,
        tenant_id,
        &context.shape,
        &subject_ref,
    )
    .await?;
    let Some(authorization) = authorized else {
        let record = crate::infra::approval::ApprovalService::submit_horizon_tightening_on(
            runner,
            scope,
            tenant_id,
            price_id,
            Uuid::now_v7(),
            verdict_json(&verdict)?,
            stamp,
            &subject_ref,
        )
        .await?;
        return Ok(HorizonOutcome::SubmittedForApproval(Box::new(
            HorizonPending {
                plan_id: context.plan_id,
                revision: context.revision,
                price_id,
                scope_key: context.key,
                prior_grandfather_until: prior,
                proposed_grandfather_until: proposed,
                row_version: context.generation.row_version,
                approval: record,
            },
        )));
    };

    // 8. Addressability, fail closed, after re-validation and before the writes
    // (D-156).
    let pending = request_version_now(
        registry.as_ref(),
        ctx,
        &unit_request_id(
            tenant_id,
            context.plan_id,
            context.revision,
            price_id,
            prior,
            proposed,
        ),
    )
    .await?;

    // 9. The column. Its compare-and-swap is on the horizon this transaction read.
    let written =
        price_repo::tighten_grandfather_until(runner, scope, tenant_id, price_id, prior, proposed)
            .await
            .map_err(|e| repo_failure(&e))?;

    // 10. The plan subject the projector re-freezes. `domain::projection` renders
    // `grandfatherUntil` into the plan's delta, so without this the last-warmed
    // delta would keep advertising a horizon the truth side has moved — D-99's
    // argument for the window plane, on the one field a consumer derives
    // `inst-el-expiry`'s signal from.
    catalog_version_ref_repo::record_pending(
        runner,
        scope,
        PendingVersionRow::for_subject(
            tenant_id,
            pending.pending_ref.clone(),
            &SubjectRef::Plan(context.plan_id.get()),
            Some(context.revision),
            Some(context.lifecycle_state),
            now,
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // 11. The record of who moved it, and **what it was** — the module doc's
    // D-327 paragraph. No outbox event: the module doc says why there is none to
    // enqueue.
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::plan_chain(context.plan_id),
            recorded_at: now,
            actor_principal_id: stamp.actor_principal_id,
            // `Update`, and not the window plane's `Publish`: this is a row's
            // content changing under a caller, which is the sentence
            // `price_repo::record_price_mutation` already writes for the draft
            // plane. The action token therefore tells a reader which plane the act
            // was on without consulting the states.
            action: AuditAction::Update,
            subject_kind: AuditSubjectKind::PriceUnit,
            subject_ref: audit_repo::price_unit_ref(price_id),
            before_state: Some(horizon_state_value(&context.generation)),
            after_state: Some(horizon_state_value(&written)),
            approval_ref: Some(authorization.approval_id),
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Ok(HorizonOutcome::Committed(Box::new(HorizonReceipt {
        plan_id: context.plan_id,
        revision: context.revision,
        price_id,
        scope_key: context.key,
        prior_grandfather_until: prior,
        grandfather_until: proposed,
        pending_version_ref: pending.pending_ref,
        row_version: written.row_version,
    })))
}

/// One generation's audited state — the horizon, and enough of the row to say
/// whose it is.
///
/// The **content**, not a digest, for `infra::window::window_state_value`'s stated
/// reason: a trail that carried a hash could not answer what the horizon
/// was, which is the whole operand D-327 found missing on the window plane.
///
/// `lifecycleState` and `rowVersion` are in it although neither moves. They are
/// what makes the pair of records self-describing: a reader who finds
/// `before_state` and `after_state` differing in exactly one field, with the tag
/// equal on both sides, can see that this act is the one in-place content mutation
/// D-141 freezes the tag through — rather than having to know it.
fn horizon_state_value(record: &PriceRecord) -> serde_json::Value {
    serde_json::json!({
        "priceId": record.price_id,
        "scopeKey": record.scope_key.to_string(),
        "lifecycleState": record.lifecycle_state.as_str(),
        "grandfatherUntil": record.grandfather_until.map(format_rfc3339),
        "rowVersion": record.row_version.get(),
    })
}

/// Read the generation, its plan, its key's windows and W6's margin.
///
/// `infra::window::read_plan_context`'s reads over a **price** id rather than a
/// window id, and the same list for the same reason: a comparison made before the
/// writing transaction opened is a hint and not a precondition (D-176).
async fn read_horizon_context(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    now: OffsetDateTime,
) -> Result<HorizonContext, DomainError> {
    let generation = price_repo::find_on(runner, scope, tenant_id, price_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "price row".to_owned(),
            id: price_id.to_string(),
        })?;
    let key = generation.scope_key.clone();
    let plan_id = key.plan_id();
    let revision = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "current plan revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    let lifecycle_state = revision.lifecycle_state;
    let shape = assemble_from(runner, scope, tenant_id, plan_id, revision, now).await?;
    // Ordered, because the walk `refuse_horizon_span_uncovered` runs steps from one
    // interval's end to the next one's start. `infra::window::project` sorts for the
    // same reason and this is the same set read a different way.
    let mut intervals: Vec<WindowInterval> =
        window_repo::list_for_plan(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .into_iter()
            .filter(|w| w.scope_key == key)
            .map(|w| WindowInterval::new(w.effective_from, w.effective_to, w.state))
            .collect();
    intervals.sort_by_key(|i| (i.effective_from, i.effective_to));
    let windows = KeyWindows {
        scope_key: key.clone(),
        intervals,
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
    Ok(HorizonContext {
        plan_id,
        revision: shape.revision,
        lifecycle_state,
        generation,
        key,
        windows,
        margin,
        published,
        shape,
    })
}

#[cfg(test)]
#[path = "grandfather_tests.rs"]
mod tests;
