//! The supersession unit's commit, across both planes (`inst-su-commit`, D-88).
//!
//! [`crate::domain::supersession`] decides — the changeover's two floors, the two
//! window operations, and whether the successor's content may land on a continued
//! counter. This module *writes* what that decided, and it exists for one property
//! neither the row repository nor the window repository can hold on its own:
//! **four writes on two planes, in one transaction, in one order**.
//!
//! # The order is the whole of it, and it is two constraints rather than one
//!
//! `inst-su-commit` says "or everything rolls back" and D-195 makes the *row* order
//! normative. Building it turned up a second ordering constraint of the same shape on
//! the window plane, enforced by a different mechanism:
//!
//! 1. **The predecessor's window is shortened before the successor's is scheduled.**
//!    `window_repo::schedule` refuses an interval intersecting one already on the key,
//!    so an open-ended successor scheduled while the predecessor's window is still
//!    open-ended is `WINDOW_OVERLAP` — the refusal `inst-su-compose` promises a
//!    committed unit can never produce, arriving from the store instead.
//! 2. **The predecessor's row leaves the published plane before the successor's
//!    arrives** ([`price_repo::commit_supersession_rows`], D-195): §3.7 admits one
//!    published row per key, and the wrong order is a raw driver error rather than a
//!    refusal.
//!
//! Two constraints, two mechanisms, one function — so no caller is in a position to
//! satisfy one and miss the other. That is the same argument
//! `commit_supersession_rows` makes about its own pair, applied one level up, and it
//! is why this composition is not a convenience.
//!
//! # Rows before windows, and a probe is what decided it
//!
//! The order **between** the two pairs is free for correctness: `window_repo` resolves a
//! window's canonical scope key through `pricing_price` without regard to the row's
//! `lifecycle_state` (verified against every predicate it applies, not from its docs), so
//! a `superseded` predecessor changes nothing either window write sees. What the order
//! decides is **what the loser of a race is told**, and that is measurable.
//!
//! Writing the windows first — on the ground that their preconditions are the volatile
//! ones while the row's is "a frozen version that only this unit's own siblings move" —
//! rests on a false premise: the presented version is the *successor draft's*, and
//! `PriceRepo::update_draft` bumps it on every mounted `PATCH …/prices/{priceId}`.
//! D-141 freezes *published* rows.
//!
//! With the reason gone, `tests/postgres_supersession_race.rs` was written to measure the
//! difference, and it is not cosmetic. Two commits of one unit — a retry after a timeout,
//! a double approval — serialize on whichever plane is written first:
//!
//! - **windows first**: the loser blocks on the predecessor window's row, finds
//!   `mutation_seq` moved, and is answered [`RepoError::StaleRowVersion`] — which sends an
//!   operator to re-read a **window entity tag** that is not what changed.
//! - **rows first**: the loser blocks on the predecessor *price row*, finds it
//!   `superseded`, and is answered [`RepoError::NotSupersedable`] — whose message names the
//!   actionable remedy, *recompose against the key's new current row*.
//!
//! The probe is the evidence: reordering the pairs flips the refusal, with the same
//! choreography and the same winner. So rows go first, and the argument recorded here is
//! **diagnosis** rather than correctness — which is also why it can be stated plainly
//! instead of defended.
//!
//! The orchestrator does not make this moot, and an earlier note claiming it would was
//! wrong: re-running `plan_supersession` at commit checks the instant, the plane and the
//! content, and **not** whether the predecessor is still its key's current row —
//! `plan_supersession` takes two `PriceRow`s and a window plane, neither of which carries
//! a lifecycle state.
//!
//! It is also a **lock order**, and on Postgres a lock order must be consistent across
//! writers or two transactions deadlock. Nothing violates it: `infra::window`'s mutation
//! unit only *reads* `pricing_price` and writes only `pricing_price_window`, and
//! `publish_rows` touches only price rows, so no writer takes the two in the opposite
//! order. "Free" is about correctness, never a licence to reverse this elsewhere.
//!
//! # What [`commit_supersession`] does **not** do
//!
//! It requests no `CatalogVersion`, enqueues no event, writes no approval record and
//! no audit entry. `inst-su-commit` names all of those, and they belong to the layer
//! that owns the unit — [`SupersessionService`], below — the same division
//! `price_repo::publish_rows` keeps against `PublishService::commit`, whose step 6
//! writes one record for the whole act rather than one per row moved. What
//! [`commit_supersession`] is, is the part that has to be one transaction whatever the
//! layer above decides, and it stays a separate function for that reason rather than
//! being inlined into the orchestrator now that both live here.
//!
//! # The orchestrator is in this file, and the whole act is one transaction
//!
//! [`SupersessionService::supersede`] is `inst-su-compose`'s composer and
//! `inst-su-commit`'s owner at once. Its order is the load-bearing part and it is
//! documented at [`supersede_in`]; the short version is that the successor draft is part
//! of the plan shape the unit's content pin covers, so it has to be staged **before** the
//! unit is opened or the pin is taken over a world without the row under review.

use std::sync::Arc;

use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DBRunner, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::concurrency::RowVersion;
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::{self, ChangeSet, MaterialityVerdict, PublishedPriceBaseline};
use crate::domain::plan_shape::PlanShape;
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::price_record::{PriceContent, PriceRecord};
use crate::domain::read_model::SubjectRef;
use crate::domain::scope_key::{PlanId, ScopeKey};
use crate::domain::supersession::{
    ChangeoverMoment, ComposedWindows, NamedWindow, SupersessionPlan, WindowShorten,
    plan_supersession,
};
use crate::domain::window::WindowInterval;
use crate::infra::publish::assemble_from;
use crate::infra::registry_deadline::request_version_now;
use crate::infra::storage::RepoError;
use crate::infra::storage::repo::approval_repo::ApprovalRecord;
use crate::infra::storage::repo::audit_repo::NewAuditEntry;
use crate::infra::storage::repo::outbox_repo::{
    NewOutboxEvent, PriceUpdatedPayload, PriceWindowTransitionPayload,
};
use crate::infra::storage::repo::{
    NewPriceDraft, NewWindow, PendingVersionRow, WindowMutationEvent, WindowRecord, approval_repo,
    audit_repo, catalog_version_ref_repo, outbox_repo, plan_repo, price_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use crate::infra::window::VerdictJson;
use time::OffsetDateTime;

/// Everything the supersession unit's commit writes, built **from** the composed
/// plan rather than beside it.
///
/// # The window instants are not two fields, and that is the whole point
///
/// An earlier shape of this struct carried `shorten.effective_to` and
/// `successor_from` as two independently-settable public fields, with a paragraph
/// here defending it: deriving both from one instant at write time would re-decide
/// what [`compose_windows`](crate::domain::supersession::compose_windows) already
/// decided. **That argument was against re-deriving and it does not license
/// declining to assert** — and the gap it permitted commits *silently*, which is
/// worse than every failure this module was built to order.
///
/// Concretely: a shorten to `T1` with a schedule from `T2 > T1` leaves `[T1, T2)`
/// uncovered. Nothing in the transaction notices, because window collision is an
/// **intersection** test (`window_repo`'s `intersects`) and a gap is not an
/// intersection. The result is exactly the `WINDOW_TRAILING_VOID` that
/// `inst-su-compose` promises cannot arise from a committed unit, and exactly the
/// *"no interim state exists in which the key is shortened without its scheduled
/// successor"* that `inst-su-commit` promises — both violated by a committed unit,
/// with no refusal anywhere.
///
/// So the fields are **private** and [`ComposedWindows`] — the value that has been
/// proved adjacent — is the only way to supply them. The disagreement is not
/// refused; it is unspellable. A module whose stated reason to exist is that "no
/// caller is in a position to satisfy one constraint and miss the other" cannot then
/// hand the caller a struct that drops the unit's defining relation.
///
/// # What is still checked at run time, and why it cannot be structural
///
/// Two relations are about rows the store holds rather than values the plan carries,
/// so no constructor can enforce them: that the predecessor and the successor stand
/// on the **same canonical scope key**, and that the successor's
/// `supersedes_price_id` names this predecessor. Both are checked inside
/// [`price_repo::commit_supersession_rows`], where the rows are in hand — see its
/// doc for why the key equality is the load-bearing half of the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupersessionCommit {
    /// The plan whose rows are moving — [`price_repo::publish_rows`]' own filter.
    plan_id: PlanId,
    /// The row leaving the published plane.
    predecessor: Uuid,
    /// Both window operations, as `compose_windows` proved them adjacent.
    windows: ComposedWindows,
    /// The window's **act counter** as the unit was composed against it (D-190/D-191).
    ///
    /// Presented rather than re-read inside the commit, and that is the precondition
    /// doing its job: a commit that read the counter itself would apply the shorten
    /// over an adjustment somebody made between compose and approval, which is the
    /// exact window D-191's precondition exists to close.
    shorten_expected_seq: u64,
    /// The successor row, at the version the rule set judged it at.
    successor: (Uuid, RowVersion),
    /// The successor window's durable name, minted by the surface.
    successor_window_id: Uuid,
    /// The operator-supplied reason the **scheduled** window is recorded under.
    ///
    /// The shorten carries no reason and cannot be given one: `adjust_effective_to`
    /// takes no `reason_code`, and the column is frozen by the append-only trigger on
    /// both backends. That is a **design-set gap rather than a code choice** — D-99
    /// makes the shorten a publish unit in its own right and §6 calls `reason_code`
    /// "the operator-supplied change reason", so the shorten's reason has nowhere in
    /// this schema to live.
    ///
    /// The only place it could go is the unit's own approval or audit record. This
    /// paragraph used to end *"which this module deliberately does not write"*, and
    /// that has been false since the record at [`supersede_in`] step 11 landed —
    /// which is a stronger statement of the gap, not a weaker one: the record exists,
    /// [`unit_state`] now carries the interval the shorten moved, and the **reason**
    /// is still not on it. It is owed as a field, not as a surface.
    reason_code: String,
}

impl SupersessionCommit {
    /// Build the commit from a plan `plan_supersession` produced.
    ///
    /// Taking [`SupersessionPlan`] rather than its parts is what makes the adjacency
    /// structural: both window instants come out of the one `changeover` that
    /// [`compose_windows`](crate::domain::supersession::compose_windows) validated the
    /// plane against.
    ///
    /// The identities are separate arguments because none of them is the domain's to
    /// know: the plan reasons about intervals and content, while the row ids, the act
    /// counter and the minted window id are the orchestrator's.
    #[must_use]
    pub const fn of_plan(
        plan: &SupersessionPlan,
        plan_id: PlanId,
        predecessor: Uuid,
        shorten_expected_seq: u64,
        successor: (Uuid, RowVersion),
        successor_window_id: Uuid,
        reason_code: String,
    ) -> Self {
        Self {
            plan_id,
            predecessor,
            windows: plan.windows,
            shorten_expected_seq,
            successor,
            successor_window_id,
            reason_code,
        }
    }

    /// The predecessor window's operation.
    #[must_use]
    pub const fn shorten(&self) -> WindowShorten {
        self.windows.shorten
    }

    /// Where the successor's coverage opens — the same changeover the shorten ends at.
    ///
    /// There is no matching end: `inst-su-compose` schedules the successor
    /// open-ended, and an accessor for one would invite a caller to close a key's
    /// coverage as a side effect of repricing it.
    #[must_use]
    pub const fn successor_from(&self) -> OffsetDateTime {
        self.windows.successor.effective_from
    }
}

/// The two window rows as they stand after the commit.
///
/// Returned so the layer above can build `inst-su-return`'s events from what was
/// **written** rather than from what was asked for — the distinction
/// `infra::window`'s `pending_approval` makes for the same reason one plane over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupersessionWritten {
    /// The predecessor's window, shortened.
    pub shortened: WindowRecord,
    /// The successor's window, scheduled.
    pub scheduled: WindowRecord,
}

/// C4's region readiness, as the taxonomy stands in **this** transaction.
///
/// One spelling, because two callers now need it and they must not be able to
/// judge a successor against one reading of the taxonomy and freeze it against
/// another: [`commit_supersession`] resolves the category it writes, and
/// [`refuse_unresolved_tax_category`] refuses the successor that would resolve
/// none. Both read here.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
async fn region_readiness(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<crate::domain::tax_display::RegionTaxReadiness, RepoError> {
    Ok(crate::domain::tax_display::RegionTaxReadiness::new(
        crate::infra::storage::repo::taxonomy_repo::region_readiness_map(runner, scope, tenant_id)
            .await?
            .into_iter()
            .map(|(region, markers)| {
                (
                    region,
                    crate::domain::tax_display::RegionReadiness {
                        tax_category: markers.tax_category,
                        tax_rate_present: markers.tax_rate_present,
                    },
                )
            })
            .collect(),
    ))
}

/// `inst-td-policy` arm 1 (D-154), restated at the **one** door that never
/// reaches the rule which judges it.
///
/// # Why this exists at all
///
/// `TaxBasisComplete` is registered in the publish foundation set, so it runs
/// under `run_publish_rules` — and of `price_repo::publish_rows`' four doors, the
/// plain publish runs that set before the flip while the cutover and mass
/// repricing re-run it over the post-commit shape inside the same transaction.
/// This one does neither: `commit_supersession` reads the readiness and the
/// rounding default itself and goes straight into `commit_supersession_rows`, and
/// `domain::supersession::plan_supersession` runs `price_row_rules()` and
/// `supersession_rules()`, neither of which mentions `tax_category_ref`.
/// `infra::cutover` states the gap in as many words — *"a same-key supersession
/// does not change that set and does not run this, which is why `supersede_in`
/// has no such call"* — which is true of the descriptor-set rules it was written
/// about and was carrying this one with it.
///
/// So a successor authored with no `taxCategoryRef` on a region declaring no
/// default published with `resolved_tax_category` **NULL** — a state
/// `pricing_price`'s migration header and `price_repo::FrozenResolution`'s doc both call
/// impossible for a published row — and `pricing_price`'s append-only guard
/// froze both tax columns, so there was no repair afterwards. It is worse than a
/// bad version: `CANDIDATE_ROW_STATES` includes `published`, so the plan's next
/// ordinary publish assembles that successor, `TaxBasisComplete` refuses it, and
/// the plan is wedged out of publishing until somebody declares a default on the
/// region.
///
/// `RepoError::RoundingPolicyUnresolved`'s argument, one column over: the same
/// fault reaching the operator through a different door has to be the same
/// refusal, so this carries `TAX_BASIS_INCOMPLETE` and the remedy the rule's own
/// message names — set the row's category, or declare a default on the region.
///
/// # Where it runs, and why not lower
///
/// Before the registry request and before the evaluator, so it is answered where
/// D-156 puts every **permanent** refusal: no retry of this request can pass it,
/// and a reviewer must not be asked to approve an act that can never commit. The
/// symmetric refusal for the *rounding* half lives one layer down, in
/// `publish_rows` itself, and holds all four doors; this one cannot go there
/// without a `RepoError` variant to carry it.
///
/// # Errors
/// [`DomainError::ValidationFailed`] carrying one `TAX_BASIS_INCOMPLETE`
/// violation against the successor; [`DomainError::Internal`] when the taxonomy
/// cannot be read.
async fn refuse_unresolved_tax_category(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    successor: &PriceRecord,
) -> Result<(), DomainError> {
    let readiness = region_readiness(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    if crate::domain::tax_display::effective_category(successor, &readiness).is_some() {
        return Ok(());
    }
    let region = successor.scope_key.region();
    let mut report = crate::domain::validation::ValidationReport::default();
    report.violate(
        crate::domain::tax_display::TAX_BASIS_INCOMPLETE,
        successor.price_id.to_string(),
        format!(
            "the successor on {} resolves no tax category: it states none and region `{region}` \
             declares no default, so `coalesce(row.tax_category_ref, readiness.taxCategory)` is \
             empty. `taxCategory` is a pinned D-48 v1 descriptor element and a published row \
             carries one, so this supersession stops rather than freezing nothing onto a row \
             nothing can then repair (D-154) — set the successor's category, or declare a \
             default on the region",
            successor.scope_key
        ),
    );
    Err(DomainError::ValidationFailed(report))
}

/// Apply the supersession unit's four writes, in the caller's transaction.
///
/// See the module doc for the two ordering constraints and why they are one function.
///
/// # Errors
/// [`RepoError::StaleRowVersion`] from either precondition — the window's act counter
/// or the successor's row version; [`RepoError::WindowOverlap`] or
/// [`RepoError::WindowHistorical`] from the window writes (`WindowHistoricalImmutable`
/// is the `DomainError` it maps to, not a variant a caller can match here);
/// [`RepoError::NotSupersedable`] when the predecessor is no longer the key's current
/// row; [`RepoError::NotFound`] when anything named is invisible to `scope`;
/// [`RepoError::Db`] on a storage failure. Every one of them rolls the whole unit
/// back, which is the point.
pub async fn commit_supersession(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan: SupersessionCommit,
    stamp: AuditStamp,
) -> Result<SupersessionWritten, RepoError> {
    // 1. and 2. The row pair, in the one order that works (D-195). Their own ordering
    //    lives inside that function so it cannot be got wrong here either.
    //
    //    **First, so that a replayed commit is refused where the refusal is
    //    actionable** — see the module doc. This was the other way round until a probe
    //    measured what each order tells the loser of a race.
    // D-154's readiness, resolved in **this** transaction so the successor's
    // frozen category is the one the world held when it published — the same
    // reason `infra::publish::rule_params` resolves it there rather than letting
    // the projector re-derive it later (`T-13`).
    let readiness = region_readiness(txn, scope, tenant_id).await?;
    // D-154's rule applied to the second frozen resolution: read in **this**
    // transaction so the successor and its rounding policy cannot disagree.
    let default_rounding_policy =
        crate::infra::storage::repo::policy_repo::default_rounding_policy_on(txn, scope, tenant_id)
            .await?;
    price_repo::commit_supersession_rows(
        txn,
        scope,
        tenant_id,
        plan.plan_id,
        plan.predecessor,
        plan.successor,
        &readiness,
        default_rounding_policy.as_deref(),
    )
    .await?;

    // 3. The predecessor's coverage ends at the changeover. Before the schedule,
    //    because the successor's interval is inside this one until it does — the one
    //    ordering here that is about correctness rather than about diagnosis.
    let shortened = window_repo::adjust_effective_to(
        txn,
        scope,
        tenant_id,
        plan.shorten().window_id,
        Some(plan.shorten().effective_to),
        plan.shorten_expected_seq,
        stamp,
    )
    .await?;

    // 4. The successor's coverage opens there. Open-ended by decision, not by
    //    omission — see `successor_from`.
    let scheduled = window_repo::schedule(
        txn,
        scope,
        NewWindow {
            window_id: plan.successor_window_id,
            tenant_id,
            price_id: plan.successor.0,
            effective_from: plan.successor_from(),
            effective_to: None,
            reason_code: plan.reason_code.clone(),
        },
        stamp,
    )
    .await?;

    Ok(SupersessionWritten {
        shortened,
        scheduled,
    })
}

/// The name of one supersession **act**, as its approval unit and any retry of the
/// request look it up by.
///
/// `inst-su-api` makes the act idempotent per **`(planId, scope key, changeover
/// instant)`**, so those three are exactly what this renders — no more, because a
/// component the caller does not control would make a genuine retry a different act, and
/// no fewer, because each of the three is one an operator actually varies:
///
/// - drop the **changeover** and two reprices of one key on two dates share a subject, so
///   an approval of one authorizes the other. That is the defect
///   `infra::window`'s `unit_subject_ref` records for a schedule, one plane over.
/// - drop the **key** and a mass reprice — which names *one* changeover for every row it
///   touches (`inst-su-instant`'s bulk clause) — becomes one act with one approval.
/// - drop the **plan** and nothing else in the string is plan-scoped.
///
/// It carries no successor `price_id` and no window id: both are minted per request, so a
/// retry could not reproduce them, which is the same reason a window schedule's subject
/// names its interval rather than its id.
///
/// **The instant is rendered to the millisecond**, D-144's quantum. Anything coarser would
/// map two distinct legal changeovers onto one act; anything finer names a value
/// `plan_supersession` refuses.
///
/// The unit's `subject_kind` is `price_unit` — the token S5 §6 declares, since its
/// enumeration has no `supersession` member and the gear does not mint tokens the design
/// set has not declared. `supersession` therefore appears in the *subject* instead, which
/// is what tells this act's unit from a `PATCH`'s on the same row.
#[must_use]
pub fn supersession_unit_ref(
    plan_id: PlanId,
    key: &crate::domain::scope_key::ScopeKey,
    changeover: OffsetDateTime,
) -> String {
    format!(
        "{}/supersession/{key}/{}",
        plan_id.get(),
        format_rfc3339(changeover)
    )
}

// ---------------------------------------------------------------------------
// The unit: `inst-su-compose`'s composer and `inst-su-commit`'s owner.
// ---------------------------------------------------------------------------

/// One supersession request, as the surface hands it over (`inst-su-api`).
///
/// A struct rather than six more arguments, and the exception to the note every other
/// service in this crate carries about not naming a request type in `infra`: those
/// notes are about `api::rest` **DTOs**, which carry wire spellings and a
/// `Deserialize`. This is domain values only — a canonical scope key, an instant, a
/// [`PriceContent`] — so the route's DTO still exists and converts into it, and what is
/// avoided is a ten-argument function whose two `Uuid`s an implementer can transpose.
///
/// # Both minted ids are the **surface's**, and one of them may be ignored
///
/// `successor_price_id` and `successor_window_id` are minted per request, exactly as a
/// window `POST`'s window id is. The consequence D-184 clause (2) records applies here
/// too and is handled rather than avoided: the call made *after* an independent approve
/// mints a **fresh** `successor_price_id`, while the successor draft it must publish was
/// staged by the first call under the first id. So the staged row's id wins, and the id
/// in the request is used only when nothing is staged yet. That is why the act's subject
/// ([`supersession_unit_ref`]) names neither.
#[derive(Clone, Debug)]
pub struct SupersessionRequest {
    /// The canonical scope key being repriced.
    pub key: ScopeKey,
    /// When coverage hands over from the predecessor to the successor.
    pub changeover: OffsetDateTime,
    /// The successor row's authored content.
    pub successor: PriceContent,
    /// The id the successor draft is authored under, if this call is what stages it.
    pub successor_price_id: Uuid,
    /// The id the successor's window is scheduled under, if this call commits.
    pub successor_window_id: Uuid,
    /// The operator-supplied change reason the scheduled window is recorded under.
    pub reason_code: String,
}

/// What a supersession did: it committed, or it opened a unit and stood *almost* still.
///
/// Two arms for [`crate::infra::window::WindowMutationOutcome`]'s reason. The
/// difference from that enumeration is worth stating because it is easy to read as an
/// inconsistency: a materially-refused **window** mutation writes nothing at all, while
/// a materially-refused supersession has written its successor **draft** — the draft is
/// the reviewer's subject, so there is nothing to review until it exists
/// (`inst-su-compose` clause (a)). What has not happened is the publish, the flip, and
/// both window operations.
#[derive(Clone, Debug)]
pub enum SupersessionOutcome {
    /// The unit committed: four writes, one version handle, two events.
    Committed(Box<SupersessionReceipt>),
    /// A second principal is required. The successor draft stands; nothing else moved.
    SubmittedForApproval(Box<SupersessionPending>),
}

/// What a committed supersession leaves behind.
///
/// It carries the **pending** handle and no `CatalogVersion`, which is the 202 in a
/// type — [`crate::infra::window::WindowMutationReceipt`]'s reason exactly.
#[derive(Clone, Debug)]
pub struct SupersessionReceipt {
    /// The plan whose subject re-projects.
    pub plan_id: PlanId,
    /// The revision this act's re-projection freezes — the plan's **current** one.
    pub revision: u64,
    /// The row that left the published plane.
    pub predecessor_price_id: Uuid,
    /// The row that arrived on it — the **staged** draft's id, never a minted one.
    pub successor_price_id: Uuid,
    /// The instant coverage handed over at.
    pub changeover: OffsetDateTime,
    /// The predecessor's window, whose end now sits at the changeover.
    pub shortened_window_id: Uuid,
    /// The successor's window, open-ended from the changeover.
    pub successor_window_id: Uuid,
    /// The registry handle the projector resolves.
    pub pending_version_ref: String,
    /// The act sequence the shortened window stands at **after** this act (D-190).
    ///
    /// On the receipt for [`crate::infra::window::WindowMutationReceipt::mutation_seq`]'s
    /// reason: there is no `GET` on a window, so a caller's next act on the
    /// predecessor's window has no other producer of the tag it must assert.
    pub shortened_mutation_seq: u64,
    /// The act sequence the **successor's** window stands at, which nothing else in the
    /// gear can answer: this act created that window, and there is no `GET` on one.
    pub successor_mutation_seq: u64,
    /// Why the act was material, or that it was not — carried on the committed arm as
    /// well, which is `PublishReceipt`'s decision one surface over.
    pub verdict: Option<MaterialityVerdict>,
    /// The unit this commit ran under, `None` exactly when no second principal was
    /// required. It is what tells an auto-publishable commit from an approved one.
    pub authorization: Option<ApprovalRecord>,
}

/// The facts a caller needs when a second principal is required.
///
/// The successor's id is the **staged** one, so the caller can read the draft it will
/// be asked to re-submit; the intervals are the ones the composition *would* apply,
/// which is the opposite choice from
/// [`crate::infra::window::PendingApproval`]'s and deliberate. There the answer
/// describes a world the caller will still find, because nothing was written. Here the
/// composition is what the reviewer is being asked to agree to, and the whole content of
/// the review is *what would move*.
#[derive(Clone, Debug)]
pub struct SupersessionPending {
    /// The plan the act is on.
    pub plan_id: PlanId,
    /// The plan's current revision — what the unit's pin covers.
    pub revision: u64,
    /// The row that would leave the published plane.
    pub predecessor_price_id: Uuid,
    /// The successor draft standing on the key.
    pub successor_price_id: Uuid,
    /// The instant coverage would hand over at.
    pub changeover: OffsetDateTime,
    /// The window whose end would move to the changeover.
    pub shortened_window_id: Uuid,
    /// Why a second principal is required — the evaluator's own answer, carried rather
    /// than re-minted.
    ///
    /// **`None` on the idempotent replay**, and that is the fix rather than an omission:
    /// a replay opens no unit and evaluates nothing, so the authority is the verdict the
    /// unit already **stored**, which rides `approval`. Carrying a freshly computed one
    /// there let a single 202 assert two materiality reasons for one act — this call's
    /// at the top level and the record's inside it — which is exactly the disagreement
    /// this field's own doc claimed it prevented.
    pub verdict: Option<MaterialityVerdict>,
    /// The unit now reviewing the act, opened inside the same transaction that
    /// declined to commit it.
    pub approval: ApprovalRecord,
}

/// The supersession unit's workflow over one database provider.
#[derive(Clone)]
pub struct SupersessionService {
    db: DBProvider<DbError>,
    registry: Arc<dyn CatalogVersionRegistryV1>,
}

impl SupersessionService {
    /// Build the workflow over one provider and the resolved registry.
    ///
    /// The **same** registry `Arc` the publish engine and the window service hold —
    /// `api::rest::state`'s rule, and this is the gear's third requester. Three
    /// requesters of one registry is still one incrementer; what keeps their handles
    /// apart is [`unit_request_id`]'s distinct first segment.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>, registry: Arc<dyn CatalogVersionRegistryV1>) -> Self {
        Self { db, registry }
    }

    /// Compose and, if it may, commit one supersession
    /// (`POST /plans/{planId}/supersessions`).
    ///
    /// The whole act in **one** transaction of this service's own. See [`supersede_in`]
    /// for the order and why every step is where it is.
    ///
    /// # Errors
    /// [`supersede_in`]'s, exactly.
    pub async fn supersede(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        request: SupersessionRequest,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<SupersessionOutcome, DomainError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        let registry = Arc::clone(&self.registry);
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<SupersessionOutcome, DomainError, _>(move |txn| {
                Box::pin(async move {
                    // Boxed a second time on purpose, and it became necessary at a
                    // **merge** rather than in either branch that caused it:
                    // `clippy::large_futures` was clean on Slice 6's tree and on
                    // this one, and fired at 16648 bytes once they were combined.
                    // Slice 6's four proration columns widened `PriceRow`, and this
                    // future holds one across every await — so the size is the sum
                    // of two changes neither of which was over the line alone.
                    // `infra::cutover` reached the same shape one plane over, for
                    // the same reason and with the same cost: one allocation per
                    // act, against that size on every task's stack.
                    Box::pin(supersede_in(
                        txn,
                        &registry,
                        &ctx,
                        &scope,
                        tenant_id,
                        &request,
                        verdict_json,
                        stamp,
                    ))
                    .await
                })
            })
            .await;
        outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: supersession transaction: {infra}"))
            })
        })
    }
}

/// The plan-scoped facts one supersession is judged against, read once inside the
/// writing transaction (D-176).
struct UnitContext {
    plan_id: PlanId,
    /// The plan's **current** revision, resolved exactly as `infra::window` does and
    /// for `PlanPublishUnit::window_mutation`'s reason: a supersession opens no plan
    /// revision — price rows hang off `plan_id`, not off a revision — so its subject is
    /// the revision that is current, and `ApprovalService::submit_supersession_on`
    /// already resolves it that way.
    revision: u64,
    lifecycle_state: LifecycleState,
    /// The plan as this transaction assembled it — the preimage of the content pin.
    shape: PlanShape,
    /// The key's current row: the one being superseded.
    predecessor: PriceRecord,
    /// A successor draft already standing on the key, staged by an earlier attempt.
    staged: Option<PriceRecord>,
    /// The key's window plane, named, as `compose_windows` needs it.
    plane: Vec<NamedWindow>,
    /// The same windows as stored, so the shorten can present the act counter it was
    /// composed against without a second read (D-190/D-191).
    stored_plane: Vec<WindowRecord>,
}

impl UnitContext {
    /// The plan's published rows — `inst-mat-newrow`'s baseline referent.
    ///
    /// Filtered off [`PlanShape::rows`] rather than read again: that field *is*
    /// `price_repo::load_for_plan` over `CANDIDATE_ROW_STATES`, the published plane plus
    /// the draft plane, so a second query keyed on `[Published]` would be the same rows
    /// at a second instant. `infra::window` does take that second read and the
    /// difference is not a disagreement — it needs the records for a change set as well.
    fn published(&self) -> Vec<PriceRecord> {
        self.shape
            .rows
            .iter()
            .filter(|row| row.lifecycle_state == LifecycleState::Published)
            .cloned()
            .collect()
    }

    /// The baseline, `None` when the plan has never published.
    ///
    /// Spelled off `shape.baseline` rather than off the row list, which is the reading
    /// `infra::window` states: "has this plan published" has one answer, and
    /// `published_baseline` is what gives it.
    fn baseline(&self) -> Option<PublishedPriceBaseline> {
        self.shape
            .baseline
            .is_some()
            .then(|| PublishedPriceBaseline::of_records(self.published()))
    }

    /// The stored window a composed shorten names, and its act counter.
    fn shorten_target(&self, window_id: Uuid) -> Result<&WindowRecord, DomainError> {
        self.stored_plane
            .iter()
            .find(|record| record.window_id == window_id)
            .ok_or_else(|| {
                // Unreachable by construction: `compose_windows` picks the id out of
                // the plane this list built. Reported rather than unwrapped, because
                // the alternative is a panic in an operator's transaction.
                DomainError::Internal(format!(
                    "bss-pricing: composed shorten names window {window_id}, which is not on the \
                     plane it was composed from"
                ))
            })
    }
}

/// Steps 0 and 0a of [`supersede_in`]: the two refusals that need no world at all.
///
/// Both are **permanent** and both read only `request`, so they precede every read and
/// the registry request whichever arm the call takes (D-156). They are one function
/// because they are one argument: step 0 guarantees the key is not
/// `existing_grandfathered`, which is the premise step 0a's reasoning rests on.
///
/// # Errors
///
/// - the key names a class that may never be superseded, through
///   `price_repo::refuse_unsupersedable_class` — the one spelling this door and the
///   staging door both use;
/// - [`DomainError::GrandfatherUntilForbidden`] when the successor authors a
///   grandfathering horizon. It is **always** wrong on this route, and it used to reach
///   the store's own check inside the staging door — i.e. after the registry request, a
///   fourth permanent refusal the step list did not know it had.
///   `check_grandfather_horizon` refuses a horizon on every class step 0 leaves, so no
///   value of this field can ever publish through a supersession. A horizon is tightened
///   through its own surface.
fn refuse_from_the_request_alone(request: &SupersessionRequest) -> Result<(), DomainError> {
    price_repo::refuse_unsupersedable_class(&request.key).map_err(|e| repo_failure(&e))?;

    if request.successor.grandfather_until.is_some() {
        return Err(DomainError::GrandfatherUntilForbidden(format!(
            "{}: a supersession cannot author a grandfathering horizon. Only an \
             existing_grandfathered generation may carry one, and such a generation may not \
             be superseded at all; tighten the horizon through its own surface instead",
            request.key
        )));
    }

    Ok(())
}

/// One supersession, **inside a transaction somebody else owns**.
///
/// Split out of [`SupersessionService::supersede`] for `infra::window`'s `mutate_in`
/// reason: the route's gate may want to own the transaction, and a service that insists
/// on opening one cannot be composed with a gate that does the same.
///
/// # The order, which is the whole of this function
///
/// Every step below is where it is because of a specific failure, and three of the
/// orderings are not free:
///
/// 1. **Read, inside the writing transaction** (D-176). A comparison made before the
///    transaction opened is a hint.
/// 2. **Compose at the submit floor** (`inst-su-compose`, `ChangeoverMoment::Submit`).
///    The instant, the plane, then the content, in the order D-195 settled.
///    2a. **The successor's tax category resolves at all** (D-154). Here rather than
///    at the freeze because it is a **permanent** refusal, so step 5's whole argument
///    applies to it: no retry can pass it, and nobody should be asked to review an act
///    that can never commit. See [`refuse_unresolved_tax_category`] for why this path
///    is the one door that has to ask.
/// 3. **Materiality**, over the tenant's policy read through the same transaction and
///    at the act's own stamped instant (D-188).
/// 4. **The two-person control**, and its three arms. The first is an approved unit;
///    the second is *this act's own pending unit*, which is `inst-su-api`'s idempotency;
///    the third stages the successor and opens the unit, **in that order**, because the
///    pin covers the shape the staged row is in. See
///    [`submitted_for_approval`] — the order is measured rather than argued.
/// 5. **Every remaining refusal**, before the registry request: the staged draft that
///    disagrees with the request, and the commit floor. Each is **permanent** for this
///    act — no retry of the same request can pass it — and a permanent refusal after
///    the request strands a pending handle forever, which is the class
///    `PublishService::commit` hoists `refuse_unpublishable_predecessor` for. The
///    other permanent refusals are steps **0**, **0a** and **2a**: the unsupersedable
///    class and the grandfathering horizon need no world at all, so they are answered
///    before the read, and the tax resolution needs only the taxonomy.
/// 6. **The registry request**, then the writes (D-156: after re-validation, before the
///    writes).
/// 7. **The four writes**, in one order, inside [`commit_supersession`].
/// 8. **The pending ref, the two events, the audit record** — the half
///    [`commit_supersession`]'s own doc withholds.
///
/// # What is deliberately not composed from `WindowService`
///
/// `WindowService::schedule` and `adjust_effective_to` each open their own publish unit
/// with their own materiality evaluation, so building step 7 from them would produce the
/// **two** approvals D-88 exists to collapse into one. That is why this function reaches
/// [`commit_supersession`] and the repositories directly.
///
/// # Errors
/// [`DomainError::NotFound`] when the plan has no current revision or the key no current
/// row; [`DomainError::SupersessionInstantPassed`],
/// [`DomainError::SupersessionKeyDormant`],
/// [`DomainError::SupersessionWouldEmptyWindow`], [`DomainError::WindowOverlap`] or
/// [`DomainError::ValidationFailed`] from the compose-time judgement, or carrying
/// `TAX_BASIS_INCOMPLETE` when the successor resolves no tax category
/// ([`refuse_unresolved_tax_category`]);
/// [`DomainError::DuplicateScopeKey`] when a staged successor disagrees with the request;
/// [`DomainError::PendingChangeUnitExists`] when a unit holds this key for something
/// else; [`DomainError::SelfApprovalForbidden`] when the authorizing record names one
/// principal twice; [`DomainError::CatalogVersionUnavailable`] when the registry cannot
/// assign; [`DomainError::StaleVersion`] when the successor's row version or the
/// window's act counter moved; [`DomainError::Internal`] on a storage failure.
#[allow(
    clippy::too_many_arguments,
    reason = "`infra::window::mutate_in`'s reason and the same facts: the transaction and the \
              registry handle the caller owns, the security context the registry request needs, \
              the compiled scope and tenant, the request itself, the verdict renderer DE0202 keeps \
              in `api::rest`, and the stamp the edge established. The request's own nine values are \
              already bundled - see `SupersessionRequest` for why that one is a struct and these \
              are not"
)]
pub async fn supersede_in(
    txn: &DbTx<'_>,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    request: &SupersessionRequest,
    verdict_json: VerdictJson,
    stamp: AuditStamp,
) -> Result<SupersessionOutcome, DomainError> {
    let now = stamp.recorded_at;

    // 0 and 0a, both answerable from the request alone.
    refuse_from_the_request_alone(request)?;

    // 1. Every fact the judgement needs, read inside the transaction that writes.
    let context = read_unit_context(txn, scope, tenant_id, &request.key, now).await?;

    // 1a. A plan whose current revision can never be superseded takes no successor row
    //     either — `PublishService::commit`'s own hoisted refusal, for the same reason
    //     it is hoisted there: it is **permanent**, so a handle requested for it would
    //     stand pending forever.
    crate::infra::publish::refuse_unpublishable_predecessor(txn, scope, tenant_id, context.plan_id)
        .await?;

    // The successor **as the store will hold it**, and this is not a convenience.
    // `api::rest::prices::content_of` cannot fill `charge_kind` — `PriceContentView` has
    // no such field — so the wire's row carries a `Recurring` placeholder, and the door
    // rewrites it from the key along with the band order. Judging or comparing the wire's
    // row therefore judged a row that does not exist: on a **usage** key the unit guard
    // saw `Recurring`, answered `is_usage() == false`, and returned without evaluating
    // (Critical). `price_repo::authored_content` is the one spelling
    // of those two rewrites and the door applies it too.
    let successor_content = requested_content(request, &context);

    // The id the act is really about: the **staged** draft's when one stands, and the
    // one the surface minted only when this call is what stages it. See
    // `SupersessionRequest` for why the minted id may be discarded.
    let successor_price_id = context
        .staged
        .as_ref()
        .map_or(request.successor_price_id, |row| row.price_id);

    // 2. `inst-su-compose` at the floor **every** call must clear, over the successor as
    //    authored. The commit floor is stricter and is applied at step 6; this run is
    //    what refuses a request nobody should be asked to review.
    let composed = plan_supersession(
        &context.predecessor.row,
        &successor_content.row,
        &context.plane,
        request.changeover,
        now,
        ChangeoverMoment::Submit,
    )?;

    // 2a. D-154's category half — a **permanent** refusal this step list did not
    //     know it had. `commit_supersession` freezes the resolution and no
    //     rule on this path judges it, so a successor resolving no category
    //     published `resolved_tax_category` NULL and `trg_pricing_price_append_only` makes that
    //     immutable. Here rather than at the freeze for D-156's reason — it is
    //     permanent, so it belongs ahead of the registry request and ahead of the
    //     review — and against the successor **as the door would write it**, which
    //     is what `requested_content` above produces.
    Box::pin(refuse_unresolved_tax_category(
        txn,
        scope,
        tenant_id,
        &successor_candidate(&successor_content, request, successor_price_id, stamp),
    ))
    .await?;

    let subject_ref = supersession_unit_ref(context.plan_id, &request.key, request.changeover);

    // 3. **An approved unit decides before the evaluator is asked**, and that order is
    //    required rather than preferred. Evaluating materiality first and looking the
    //    unit up only on the material branch lets a threshold policy raised between the
    //    submit and the commit make the act auto-publishable at the second call, so it
    //    **commits on one principal** and leaves the unit opened over it `submitted`
    //    forever. `evaluate`'s own contract says materiality is decided *once, at
    //    submit*; consulting the unit first is how a second call obeys that.
    let authorization = crate::infra::approval::authorizing_unit(
        txn,
        scope,
        tenant_id,
        &context.shape,
        &subject_ref,
    )
    .await?;

    // The evaluator's answer, on the one arm that asks it. `None` on the approved arm,
    // where the authority is the verdict the **unit** stored and the record carries it —
    // the same division `ApprovalView` makes on the publish mount.
    let verdict = if authorization.is_none() {
        // 4. This act's **own** pending unit — `inst-su-api`'s idempotency, and the
        //    second half of the fix above: a unit under review holds the act whatever a
        //    later policy says about it. Answered before anything is staged, because the
        //    answer is that nothing more should be.
        // **The lookup stays inside the `staged` arm**, and the reason is on
        // `submit_supersession_on`: with the successor draft gone the unit's pin
        // covers a world without the row under review, so every decision on it
        // answers `APPROVAL_CONTENT_MISMATCH`. Answering such a retry "still
        // pending" would name a unit that can never be decided. The 409 the submit
        // raises instead is actionable — it says a unit holds the key and must be
        // withdrawn — and the residue is carried in the register rather than
        // papered over here. `refuse_divergent_successor` is inside for its own
        // reason: a caller who edited the successor must be told, and a replay is
        // the one answer that cannot say so.
        if let Some(staged) = context.staged.as_ref() {
            refuse_divergent_successor(staged, &successor_content)?;
            if let Some(held) =
                approval_repo::find_pending_for_subject(txn, tenant_id, &subject_ref)
                    .await
                    .map_err(|e| repo_failure(&e))?
            {
                return Ok(pending_answer(&context, request, &composed, None, held));
            }
        }

        // 5. The evaluator, and only now: no unit of this act exists, so its verdict is
        //    the thing that decides. Against the policy in force **at the act's own
        //    instant** (D-188), read through the transaction that is about to write.
        let verdict = materiality::evaluate(
            &ChangeSet::of_records([successor_candidate(
                &successor_content,
                request,
                successor_price_id,
                stamp,
            )]),
            crate::infra::threshold::effective_policy_at(txn, scope, tenant_id, now)
                .await?
                .as_ref(),
            context.baseline().as_ref(),
        );
        if verdict.is_material() {
            return Box::pin(submitted_for_approval(
                txn,
                scope,
                tenant_id,
                &context,
                request,
                &successor_content,
                &composed,
                verdict,
                verdict_json,
                stamp,
            ))
            .await;
        }
        Some(verdict)
    } else {
        None
    };

    // 6. `inst-co-single-pending` on the committing arm, which it did not have. The
    //    window plane asks this **unconditionally and before anything is touched**
    //    (`infra::window`'s step 2) and this path asked it only inside
    //    `submit_supersession_on`, i.e. on the controlled arm — so an *immaterial*
    //    supersession committed straight through a key another unit was reviewing,
    //    moving the very interval set that reviewer had been shown.
    //    This unit's own approved record does not hold the key: the register row follows
    //    its parent out of `submitted` through a trigger, so `approved` frees it.
    crate::infra::approval::refuse_held_key(
        txn,
        scope,
        tenant_id,
        &std::collections::BTreeSet::from([request.key.to_string()]),
    )
    .await?;

    // 7. The remaining permanent refusals, both ahead of the registry request.
    if let Some(staged) = context.staged.as_ref() {
        refuse_divergent_successor(staged, &successor_content)?;
    }
    // `inst-su-compose` re-validated at commit, at the batching-delay floor, over the
    // successor as authored — which the guard above has just proved is the **stored**
    // successor when one is staged.
    let plan = plan_supersession(
        &context.predecessor.row,
        &successor_content.row,
        &context.plane,
        request.changeover,
        now,
        ChangeoverMoment::Commit,
    )?;
    let shorten = context.shorten_target(plan.windows.shorten.window_id)?;
    let shorten_seq = shorten.mutation_seq;
    let shortened_window_id = shorten.window_id;

    // 8. The successor draft, staged now if no earlier attempt did it — and **before**
    //    the registry request rather than after it. D-156 puts the version request
    //    before "the writes", and the writes it means are the publish's: this insert is
    //    `inst-su-compose` clause (a), the compose's own write, which the controlled arm
    //    performs with no version requested at all. Keeping it after the request put
    //    `prepare_draft`'s content validation — a value out of range, an unquantized
    //    instant — on the far side of the handle, where a refusal no retry can pass
    //    strands it forever.
    let successor = match context.staged.as_ref() {
        Some(staged) => staged.clone(),
        None => Box::pin(stage_successor(txn, scope, tenant_id, request, stamp)).await?,
    };

    // 9. Addressability, fail closed, after every refusal and before the publish's own
    //    writes (D-156).
    let pending = request_version_now(
        registry.as_ref(),
        ctx,
        &unit_request_id(tenant_id, &subject_ref),
    )
    .await?;

    // 10. The four writes, in one order, in this transaction.
    let written = commit_supersession(
        txn,
        scope,
        tenant_id,
        SupersessionCommit::of_plan(
            &plan,
            context.plan_id,
            context.predecessor.price_id,
            shorten_seq,
            (successor.price_id, successor.row_version),
            request.successor_window_id,
            request.reason_code.clone(),
        ),
        stamp,
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // 8. The plan subject the projector re-freezes. `SubjectRef::Plan` and not a
    //    per-row subject, for `infra::window`'s reason: D-99 makes windows plan facts
    //    and D-128 projects plan subjects, so this act is a different *reason* to
    //    re-project one subject rather than a second subject to project.
    catalog_version_ref_repo::record_pending(
        txn,
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

    enqueue_events(
        txn,
        scope,
        tenant_id,
        &context,
        &successor,
        &written,
        &pending.pending_ref,
        request.changeover,
        stamp,
    )
    .await?;

    // The record of who did it, at the **act's** level rather than one per row moved —
    // `PublishService::commit` step 6's division. The subject is the act, not either
    // row: `audit_repo::price_unit_ref` names a *row*, which cannot tell a supersession
    // from a `PATCH` on the same id, and the unit's approval record already stands under
    // this exact string, so `approval_ref` joins the two.
    audit_repo::append(
        txn,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::plan_chain(context.plan_id),
            recorded_at: now,
            actor_principal_id: stamp.actor_principal_id,
            action: AuditAction::Publish,
            subject_kind: AuditSubjectKind::PriceUnit,
            subject_ref: subject_ref.clone(),
            before_state: Some(unit_state(
                context.predecessor.price_id,
                LifecycleState::Published,
                &successor,
                LifecycleState::Draft,
                // The pre-image is the row **step 7 read**, not a re-read:
                // `adjust_effective_to` has overwritten the column by now, so a second
                // read would record the value the act produced and call it the value it
                // destroyed. `infra::cutover::record_cutover` says the same at its own
                // site, and this is the second half of the one fix — the defect was
                // identical on both paths and was found on this one by inspection
                // rather than by a failing case.
                &crate::infra::window::shortened_window_state_value(shorten),
            )),
            after_state: Some(unit_state(
                context.predecessor.price_id,
                LifecycleState::Superseded,
                &successor,
                LifecycleState::Published,
                &crate::infra::window::shortened_window_state_value(&written.shortened),
            )),
            approval_ref: authorization.as_ref().map(|record| record.approval_id),
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Ok(SupersessionOutcome::Committed(Box::new(
        SupersessionReceipt {
            plan_id: context.plan_id,
            revision: context.revision,
            predecessor_price_id: context.predecessor.price_id,
            successor_price_id: successor.price_id,
            changeover: request.changeover,
            shortened_window_id,
            successor_window_id: request.successor_window_id,
            pending_version_ref: pending.pending_ref,
            shortened_mutation_seq: written.shortened.mutation_seq,
            successor_mutation_seq: written.scheduled.mutation_seq,
            verdict,
            authorization,
        },
    )))
}

/// One `SubmittedForApproval` answer, built in one place.
///
/// Two arms produce one — the replay and the freshly-opened unit — and they differ in
/// exactly one field, which is why they share a builder rather than a struct literal
/// each: **the verdict is `None` on the replay**. The *freshly computed* one there
/// lets a 202 carry two materiality documents that disagree — this call's
/// evaluation at the top level and the unit's stored one inside `approval` — over
/// one act. On the replay the stored verdict is the only authority there is, and
/// it already rides the record.
fn pending_answer(
    context: &UnitContext,
    request: &SupersessionRequest,
    composed: &SupersessionPlan,
    verdict: Option<MaterialityVerdict>,
    approval: ApprovalRecord,
) -> SupersessionOutcome {
    SupersessionOutcome::SubmittedForApproval(Box::new(SupersessionPending {
        plan_id: context.plan_id,
        revision: context.revision,
        predecessor_price_id: context.predecessor.price_id,
        successor_price_id: context
            .staged
            .as_ref()
            .map_or(request.successor_price_id, |row| row.price_id),
        changeover: request.changeover,
        shortened_window_id: composed.windows.shorten.window_id,
        verdict,
        approval,
    }))
}

/// The controlled arm: stage the successor if nothing has, open the unit, write nothing
/// else.
///
/// # The successor is staged **before** the unit is opened, and a probe is what settled
/// why
///
/// The reason recorded first was wrong and is worth keeping as the wrong one:
/// `price_repo::insert_successor_draft_on` reaches `record_price_mutation`, which calls
/// `void_pending_units_of`, so opening the unit first looked like it would have the door
/// void the very unit being composed. **Measured false**: reversing the two
/// statements left the retry case green on its own assertion that the unit is *still
/// submitted*, because `approval_repo::void_pending_for_plan` filters
/// `subject_kind = 'plan_revision'` and this unit is a `price_unit`. What the reversal
/// actually reddened is the approve: `submit_supersession_on` pins
/// `content_hash(&shape)`, the shape carries the plan's `draft` rows, and a pin taken
/// before the successor exists is a pin over a world without the row under review — so
/// **every** attempt to approve such a unit answers `APPROVAL_CONTENT_MISMATCH`, a unit
/// that can be opened and never decided. That is the order's real ground, and
/// `the_units_pin_covers_the_successor_it_stages` is the case that holds it.
///
/// # What this arm does **not** do, and the caller has already done
///
/// The idempotent replay (`inst-su-api`) is decided by [`supersede_in`] before this is
/// reached, so arriving here means no unit of this act exists. Looking for one here
/// instead does **not** repair a pending unit whose successor draft was deleted:
/// `submit_supersession_on` performs its own subject lookup and refuses
/// `PENDING_CHANGE_UNIT_EXISTS`, rolling the staging back. Such a unit is
/// undecidable and has to be withdrawn by hand, which is the residue the register
/// carries.
///
/// # Errors
/// [`DomainError::PendingChangeUnitExists`] when a unit holds this key — including this
/// act's own, whose draft has been deleted; [`DomainError::Internal`] on a storage
/// failure.
#[allow(
    clippy::too_many_arguments,
    reason = "every value is one [`supersede_in`] already resolved and this arm must not resolve \
              any of them a second time - the context, the request, the authored successor, the \
              composition, the verdict, its renderer and the stamp. Extracted for readability of \
              the ordering argument in its own doc, so the parameter list is that function's"
)]
async fn submitted_for_approval(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    context: &UnitContext,
    request: &SupersessionRequest,
    successor_content: &PriceContent,
    composed: &SupersessionPlan,
    verdict: MaterialityVerdict,
    verdict_json: VerdictJson,
    stamp: AuditStamp,
) -> Result<SupersessionOutcome, DomainError> {
    if let Some(staged) = context.staged.as_ref() {
        refuse_divergent_successor(staged, successor_content)?;
    }
    let successor = match context.staged.as_ref() {
        Some(staged) => staged.clone(),
        None => Box::pin(stage_successor(runner, scope, tenant_id, request, stamp)).await?,
    };
    let record = crate::infra::approval::ApprovalService::submit_supersession_on(
        runner,
        scope,
        tenant_id,
        &request.key,
        request.changeover,
        Uuid::now_v7(),
        verdict_json(&verdict)?,
        stamp,
    )
    .await?;
    let mut answer = pending_answer(context, request, composed, Some(verdict), record);
    if let SupersessionOutcome::SubmittedForApproval(pending) = &mut answer {
        // The row this call staged, which `pending_answer` could not see: it reads
        // `context.staged`, and the context was assembled before the insert.
        pending.successor_price_id = successor.price_id;
    }
    Ok(answer)
}

/// Stage the successor draft through D-195's door.
///
/// The authoring metadata is the **stamp's**, not the request's: `created_by` is the
/// stamp's actor and `created_at_utc` its instant, for the reason `NewApproval` gives
/// about a submitter — a caller able to name either could author a row under somebody
/// else's identity, and the two facts belong to the edge that authenticated the call.
async fn stage_successor(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    request: &SupersessionRequest,
    stamp: AuditStamp,
) -> Result<PriceRecord, DomainError> {
    let (record, _predecessor) = Box::pin(price_repo::insert_successor_draft_on(
        runner,
        scope,
        tenant_id,
        NewPriceDraft {
            price_id: request.successor_price_id,
            scope_key: request.key.clone(),
            content: request.successor.clone(),
            created_by: stamp.actor_principal_id,
            created_at_utc: stamp.recorded_at,
            correlation_id: stamp.correlation_id,
        },
    ))
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(record)
}

/// Refuse a request whose content is not the content already staged for this act.
///
/// # Why this is `DUPLICATE_SCOPE_KEY` and not a code of its own
///
/// The store's own answer for a draft standing on the key is `DUPLICATE_SCOPE_KEY`
/// (`price_repo`'s `duplicate_key`), and this refusal *is* that condition — a draft
/// occupies the key and it is not the one the caller is describing. Minting a wire code
/// for the finer distinction would design a contract §5 has not declared, which is the
/// rule the two undeclared supersession refusals already record. What is added is the
/// **sentence**, because the store's cannot say what an operator has to do about it.
///
/// # Both sides are the row **as the store holds it**, and getting that wrong made every
/// non-recurring key unsupersedable
///
/// The comparison is against `staged.content()` — a record read back — so the request's
/// side has to be the row the door *would have written*, not the row the wire carried.
/// It used to normalize `supersedes_price_id` alone, and the door rewrites two more
/// things: `charge_kind`, from the key, and the band order. So on a **usage**,
/// `one_time` or `one_time_setup` key the two sides differed in `charge_kind` on every
/// call — the wire cannot spell that axis — and a legitimately approved unit was refused
/// here, permanently, with a remedy sentence ("re-send that content") no caller could
/// act on. A body listing bands out of order failed the same way on any key. Both sides
/// now go through `price_repo::authored_content`.
///
/// # Two acts, one guard
///
/// `pub(crate)` because [`crate::infra::cutover`] is the second caller. That act was
/// written from this file's eleven-step skeleton and inherited every step except this
/// one, so the two sibling paths disagreed about the same hazard — one refused a
/// divergent successor, the other committed the staged body and answered the caller as
/// though it had committed theirs. The refusal is content-generic and takes a staged
/// record against an authored content, so the cutover spends it three times: on its
/// successor, on its grandfathered copy, and on both of its arms.
///
/// # Errors
/// [`DomainError::DuplicateScopeKey`] naming the staged row.
pub(crate) fn refuse_divergent_successor(
    staged: &PriceRecord,
    successor_content: &PriceContent,
) -> Result<(), DomainError> {
    if staged.content() == *successor_content {
        return Ok(());
    }
    Err(DomainError::DuplicateScopeKey(format!(
        "the successor draft {} already staged on {} carries different content from this \
         request; a composed unit's content is what a reviewer agreed to, so re-send that \
         content, or withdraw the unit to free the key and compose again",
        staged.price_id, staged.scope_key
    )))
}

/// The request's successor **as the door would write it**.
///
/// Three rewrites, and only one of them was here before: `price_repo::authored_content`
/// applies the key's `charge_kind` and the band order — the two the store performs and
/// the wire cannot express — and the link is stamped from the read that validated the
/// key, overwriting whatever the caller sent. Comparing or judging without all three is
/// what made the unit guard unreachable and the divergence guard always-true; see
/// `refuse_divergent_successor`.
fn requested_content(request: &SupersessionRequest, context: &UnitContext) -> PriceContent {
    PriceContent {
        supersedes_price_id: Some(context.predecessor.price_id),
        ..price_repo::authored_content(&request.key, request.successor.clone())
    }
}

/// The successor as the materiality evaluator must see it.
///
/// A hypothetical [`PriceRecord`] rather than the staged row, so that the verdict is a
/// function of the **request** and cannot differ between the call that opens the unit and
/// the call that commits it. The content is the authored one, for
/// [`requested_content`]'s reason. The one field that has to be real is `price_id`:
/// `MaterialityVerdict::tripped_row` names it, and a stored verdict naming a row that
/// never published would be unreadable years later — hence `successor_price_id`, which is
/// the staged row's whenever one stands.
fn successor_candidate(
    successor_content: &PriceContent,
    request: &SupersessionRequest,
    successor_price_id: Uuid,
    stamp: AuditStamp,
) -> PriceRecord {
    let authored = successor_content.clone();
    PriceRecord {
        price_id: successor_price_id,
        scope_key: request.key.clone(),
        row: authored.row,
        tax_inclusive: authored.tax_inclusive,
        tax_category_ref: authored.tax_category_ref.clone(),
        billing_timing: authored.billing_timing,
        proration_contract: authored.proration_contract,
        rounding_policy_ref: authored.rounding_policy_ref,
        grandfather_until: authored.grandfather_until,
        supersedes_price_id: authored.supersedes_price_id,
        // The state it is being judged *for*, which is what the evaluator's change set
        // means: `ChangeSet::of_records` is "the rows this act would publish".
        lifecycle_state: LifecycleState::Draft,
        created_by: stamp.actor_principal_id,
        created_at_utc: stamp.recorded_at,
        row_version: RowVersion::new(0),
    }
}

/// `inst-su-return`'s two events, in the transaction that wrote what they describe.
///
/// The third event the instruction names — the predecessor's `PriceWindowExpired`
/// **firing at the changeover** — is deliberately not here and needs no producer of its
/// own: `infra::jobs::window_activation` emits it when the sweep crosses the end this
/// commit just moved. Emitting it now would announce an expiry that has not happened, and
/// `inst-ws-publishunit`'s negative half is explicit that the time-driven flips are not
/// publish units.
#[allow(
    clippy::too_many_arguments,
    reason = "the enqueue needs what the two payloads are made of and nothing else: the runner and \
              scope, the tenant, the plan facts, the row that published, the windows as written, \
              the pending handle and the stamp. Bundling them would name a type whose only reader \
              is this function"
)]
async fn enqueue_events(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    context: &UnitContext,
    successor: &PriceRecord,
    written: &SupersessionWritten,
    pending_ref: &str,
    changeover: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<(), DomainError> {
    outbox_repo::enqueue(
        runner,
        scope,
        NewOutboxEvent::price_updated(
            tenant_id,
            &PriceUpdatedPayload {
                plan_id: context.plan_id,
                price_id: successor.price_id,
                scope_key: successor.scope_key.to_string(),
                supersedes_price_id: context.predecessor.price_id,
                changeover,
                pending_version_ref: pending_ref.to_owned(),
                correlation_id: stamp.correlation_id,
            },
            stamp.recorded_at,
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // `PriceWindowScheduled` for the successor's window, **as written** rather than as
    // asked for — `commit_supersession` returns the rows for exactly this reason.
    //
    // Through `price_window_mutation`, whose act segment is what keeps this event out of
    // a collision with any other `PriceWindowScheduled` on the same window: the subject
    // is the act, and two supersessions of one key at two changeovers are two acts.
    outbox_repo::enqueue(
        runner,
        scope,
        NewOutboxEvent::price_window_mutation(
            tenant_id,
            WindowMutationEvent::Scheduled,
            &PriceWindowTransitionPayload {
                window_id: written.scheduled.window_id,
                plan_id: context.plan_id,
                price_id: successor.price_id,
                effective_from: written.scheduled.effective_from,
                effective_to: written.scheduled.effective_to,
                correlation_id: stamp.correlation_id,
            },
            stamp.recorded_at,
            &format!("supersede/{}", format_rfc3339(changeover)),
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(())
}

/// The registry's idempotency handle for one supersession unit.
///
/// Built from [`supersession_unit_ref`] rather than from a minted
/// `PublishUnitKind` variant, and the two properties the registry contract needs are
/// exactly the two that subject already has: **stable across a retry**, and **distinct
/// across two acts** so that two changeovers on one key do not share a `CatalogVersion`.
///
/// "Stable across a retry" is about the **commit** attempt specifically, and an earlier
/// wording overstated it: the controlled arm returns before this is ever called, so a
/// first attempt that opens a unit requests no handle at all. What the stability buys is
/// that a commit refused after the request — a lost window race, a storage fault — is
/// handed the *same* handle when it is retried, rather than stranding the first.
///
/// The `supersession/` prefix is what keeps it clear of `plan-publish/…` and
/// `window-mutation/…`, which is the role `PublishUnitKind::request_token` plays for the
/// other two requesters. A variant here would be a wire-adjacent enumeration widened for
/// a token no `subject_kind` column declares.
///
/// Re-spelling this string strands any pending ref the old spelling produced.
fn unit_request_id(tenant_id: Uuid, subject_ref: &str) -> String {
    format!("supersession/{tenant_id}/{subject_ref}")
}

/// The two rows' states **and the shortened window's interval**, as the audit
/// record's before/after.
///
/// The **content**, not a digest of it, for `infra::window`'s `window_state_value`
/// reason: the standing heuristic is that a test rebuilding its expectation from the data it
/// checks cannot see that data replaced.
///
/// # The window was missing here for the same reason it was missing on the cutover
///
/// This record named two price ids, their lifecycle states and the key, and no window
/// and no instant — while [`commit_supersession`] step 3 reaches
/// `window_repo::adjust_effective_to` and overwrites the predecessor's `effectiveTo`
/// in place. D-327 found that defect on the cutover and declared the unwind
/// unbuildable because of it; this path had it identically, and nothing had found it,
/// because a supersession has no unwind clause pointing at the missing operand. It is
/// fixed on both paths at once, through one renderer, so the two records cannot drift:
/// a reader who learns the shape from a supersession can read a cutover.
///
/// **A supersession is not merely the cutover's rehearsal here.** Its shorten is
/// exactly as destructive — `compose_windows` picks the covering window by
/// `WindowInterval::covers`, satisfied by an absent end and by any end strictly after
/// the changeover alike — so "what did this key's coverage end at before the
/// reprice" had no answer on this path either, for any purpose, not only for D-05's.
fn unit_state(
    predecessor: Uuid,
    predecessor_state: LifecycleState,
    successor: &PriceRecord,
    successor_state: LifecycleState,
    shortened_window: &JsonValue,
) -> JsonValue {
    serde_json::json!({
        "predecessorPriceId": predecessor,
        "predecessorState": predecessor_state.as_str(),
        "successorPriceId": successor.price_id,
        "successorState": successor_state.as_str(),
        "scopeKey": successor.scope_key.to_string(),
        "shortenedWindow": shortened_window,
    })
}

/// Read every fact the judgement needs, inside the writing transaction.
async fn read_unit_context(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    key: &ScopeKey,
    now: OffsetDateTime,
) -> Result<UnitContext, DomainError> {
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

    let on_key = |state: LifecycleState| {
        shape
            .rows
            .iter()
            .find(|row| row.scope_key == *key && row.lifecycle_state == state)
            .cloned()
    };
    // The **published** occupant is the precondition, and its absence is `NotFound` on
    // the key rather than a conflict — `insert_successor_draft_on`'s reading of the same
    // two rows, and the remedy the design names for a key with no current row is a plain
    // publish plus a window schedule rather than a retry of this act.
    let predecessor = on_key(LifecycleState::Published).ok_or_else(|| DomainError::NotFound {
        subject: "current price row on canonical scope key".to_owned(),
        id: key.to_string(),
    })?;
    let staged = on_key(LifecycleState::Draft);

    let stored_plane: Vec<WindowRecord> =
        window_repo::list_for_plan(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .into_iter()
            .filter(|record| record.scope_key == *key)
            .collect();
    let plane = stored_plane
        .iter()
        .map(|record| NamedWindow {
            window_id: record.window_id,
            interval: WindowInterval::new(record.effective_from, record.effective_to, record.state),
        })
        .collect();

    Ok(UnitContext {
        plan_id,
        revision: shape.revision,
        lifecycle_state,
        shape,
        predecessor,
        staged,
        plane,
        stored_plane,
    })
}

#[cfg(test)]
#[path = "supersession_tests.rs"]
mod supersession_tests;
