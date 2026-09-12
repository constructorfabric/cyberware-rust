//! The grandfathering cutover's commit, across both planes (`inst-gc-commit`, D-100).
//!
//! `infra::supersession`'s shape with one more window and one more row, and the two
//! ordering constraints it records hold here for the same reasons and through the
//! same two mechanisms. What this unit adds is a **second arrival**: the
//! grandfathered copy, on a new generation of the predecessor's key, born in the
//! same transaction as the successor that replaces it.
//!
//! # What this door owes the rows it publishes (D-344)
//!
//! It copied `supersede_in`'s *shape* and not its *rule sets*, and for as long as
//! that stood a cutover's entirely client-authored successor reached `published`
//! judged by two window checks and two store-level content checks — no
//! `price_row_rules`, no `supersession_rules`, no plan aggregate, no fixture gate.
//! D-344 settled which rules a door owes by the **act** it performs rather than by
//! which door it is, and all three tiers land here:
//!
//! * **Tier A**, at steps 2 and 7 — [`plan_supersession`] over the predecessor and
//!   the successor as the store will hold it, exactly where `supersede_in` spends
//!   it. A cutover supersedes on the predecessor's own canonical scope key
//!   (`inst-co-supersede`), so the tier counter continues across the act and
//!   `SupersessionUnitGuard` binds it for the same reason it binds a supersession.
//! * **Tier B**, at step 8b — [`run_publish_rules`] over the plan as this act's two
//!   drafts leave it. A cutover mints a **generation**, and `priceEligibility` is a
//!   scope-key axis, so this act changes which keys are current; a same-key
//!   supersession does not, which is why `supersede_in` does not run it.
//! * **The fixture gate**, beside Tier B, because a cutover's content is authored
//!   rather than derived.

use std::sync::Arc;

use aws_lc_rs::digest::{SHA256, digest as sha256};

use toolkit_db::secure::{AccessScope, DBRunner, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind, hex_bytes};
use crate::domain::concurrency::RowVersion;
use crate::domain::cutover::{
    ComposedCutover, check_cutover_instant, compose_cutover_windows, grandfathered_copy_key,
};
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::triggers::Trigger;
use crate::domain::materiality::{self, ChangeSet, MaterialityVerdict};
use crate::domain::plan_shape::PlanShape;
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::price_record::{PriceContent, PriceRecord};
use crate::domain::publish::rules::run_publish_rules;
use crate::domain::read_model::SubjectRef;
use crate::domain::scope_key::PlanId;
use crate::domain::scope_key::{Cohort, PriceEligibility, ScopeKey};
use crate::domain::supersession::{ChangeoverMoment, NamedWindow, plan_supersession};
use crate::domain::window::WindowInterval;
use crate::infra::fixture_gate::FixtureGate;
use crate::infra::publish::{assemble_from, check_fixtures, rule_params};
use crate::infra::registry_deadline::request_version_now;
use crate::infra::storage::RepoError;
use crate::infra::storage::repo::PolicyObjectRepo;
use crate::infra::storage::repo::approval_repo::{self, ApprovalRecord};
use crate::infra::storage::repo::audit_repo::{self, NewAuditEntry};
use crate::infra::storage::repo::window_repo::{NewWindow, WindowRecord};
use crate::infra::storage::repo::{
    NewOutboxEvent, NewPriceDraft, PendingVersionRow, PriceUpdatedPayload,
    PriceWindowTransitionPayload, WindowMutationEvent, catalog_version_ref_repo, outbox_repo,
    plan_repo, price_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use crate::infra::window::VerdictJson;
use time::OffsetDateTime;

/// Everything the cutover's commit writes, named by the orchestrator and **not
/// separable**.
///
/// The fields are private and the three instants come out of one
/// [`ComposedCutover`], which is D-88's review lesson applied before it can cost
/// anything: there, two independent public instants let a caller compose a shorten to
/// `T1` and a schedule from `T2 > T1`, leaving `[T1, T2)` uncovered and committing
/// cleanly, because collision is an intersection test and a gap is not an
/// intersection. This unit schedules **two** windows, so the same hazard would be
/// here twice.
///
/// The identities are separate arguments because none of them is the domain's to
/// know: it reasons about intervals and content, while the row ids, the act counter
/// and the minted window ids are the orchestrator's.
#[derive(Clone, Debug)]
pub struct CutoverCommit {
    plan_id: PlanId,
    predecessor: Uuid,
    windows: ComposedCutover,
    shorten_expected_seq: u64,
    successor: (Uuid, RowVersion),
    successor_window_id: Uuid,
    copy: (Uuid, RowVersion),
    copy_window_id: Uuid,
    reason_code: String,
}

impl CutoverCommit {
    /// Build the commit from a composition the domain has already proven adjacent.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the composition plus four identities the domain cannot know: two row ids with \
                  their versions, two minted window ids, the act counter and the reason code. \
                  Collapsing them into a struct would move the argument list rather than shorten \
                  it, and the composition is already the one thing that must not be assembled \
                  field by field"
    )]
    pub const fn of_composition(
        windows: ComposedCutover,
        plan_id: PlanId,
        predecessor: Uuid,
        shorten_expected_seq: u64,
        successor: (Uuid, RowVersion),
        successor_window_id: Uuid,
        copy: (Uuid, RowVersion),
        copy_window_id: Uuid,
        reason_code: String,
    ) -> Self {
        Self {
            plan_id,
            predecessor,
            windows,
            shorten_expected_seq,
            successor,
            successor_window_id,
            copy,
            copy_window_id,
            reason_code,
        }
    }

    /// The successor's interval, as the composition proved it.
    #[must_use]
    pub const fn successor_window(&self) -> WindowInterval {
        self.windows.successor()
    }

    /// The copy's interval, on the new generation's key.
    #[must_use]
    pub const fn copy_window(&self) -> WindowInterval {
        self.windows.copy()
    }

    /// The instant all three operations pivot on.
    #[must_use]
    pub const fn cutover_at(&self) -> OffsetDateTime {
        self.windows.shorten().effective_to
    }
}

/// What the commit wrote, for the caller to announce.
#[derive(Clone, Debug)]
pub struct CutoverWritten {
    /// The predecessor's window, now ending at the cutover.
    pub shortened: WindowRecord,
    /// The successor's window.
    pub successor_window: WindowRecord,
    /// The grandfathered copy's window.
    pub copy_window: WindowRecord,
}

/// Commit the cutover: five writes, one transaction (`inst-gc-commit`, D-03).
///
/// **The rows go first**, for `commit_supersession_rows`' diagnosis reason: a
/// replayed commit then blocks on the predecessor *row* and is refused by name —
/// recompose against the key's new current row — instead of blocking on the
/// predecessor *window* and being told an entity tag is stale, which is not what
/// changed.
///
/// **The shorten precedes both schedules**, and that is correctness rather than
/// preference: `window_repo::schedule` refuses an interval intersecting one already
/// on the key, and the successor's open-ended `[cutover, …)` sits inside the
/// predecessor's still-open interval until it is shortened. The copy is on a
/// different key and could go in any order; it follows the successor so that the two
/// arrivals stay adjacent in one reading.
///
/// **`shorten_expected_seq` is presented, never re-read.** A commit that read the
/// window's act counter itself would apply the shorten over an adjustment made
/// between compose and approval — exactly the gap D-191's precondition closes.
///
/// # Errors
///
/// Whatever the row plane refuses (`price_repo::commit_cutover_rows`), and whatever
/// the window plane refuses on the shorten or either schedule.
pub async fn commit_cutover(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan: CutoverCommit,
    stamp: AuditStamp,
) -> Result<CutoverWritten, RepoError> {
    // D-154's readiness, resolved in **this** transaction so the successor's
    // frozen category is the one the world held when it published — the same
    // reason `infra::publish::rule_params` resolves it there rather than letting
    // the projector re-derive it later (`T-13`).
    let readiness = crate::domain::tax_display::RegionTaxReadiness::new(
        crate::infra::storage::repo::taxonomy_repo::region_readiness_map(txn, scope, tenant_id)
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
    );
    // D-154's rule applied to the second frozen resolution: read in **this**
    // transaction so the successor, the copy and their rounding policy cannot
    // disagree.
    let default_rounding_policy =
        crate::infra::storage::repo::policy_repo::default_rounding_policy_on(txn, scope, tenant_id)
            .await?;
    price_repo::commit_cutover_rows(
        txn,
        scope,
        tenant_id,
        plan.plan_id,
        plan.predecessor,
        plan.successor,
        plan.copy,
        plan.cutover_at(),
        &readiness,
        default_rounding_policy.as_deref(),
    )
    .await?;

    let shortened = window_repo::adjust_effective_to(
        txn,
        scope,
        tenant_id,
        plan.windows.shorten().window_id,
        Some(plan.windows.shorten().effective_to),
        plan.shorten_expected_seq,
        stamp,
    )
    .await?;

    let successor_window = window_repo::schedule(
        txn,
        scope,
        NewWindow {
            window_id: plan.successor_window_id,
            tenant_id,
            price_id: plan.successor.0,
            effective_from: plan.successor_window().effective_from,
            effective_to: plan.successor_window().effective_to,
            reason_code: plan.reason_code.clone(),
        },
        stamp,
    )
    .await?;

    let copy_window = window_repo::schedule(
        txn,
        scope,
        NewWindow {
            window_id: plan.copy_window_id,
            tenant_id,
            price_id: plan.copy.0,
            effective_from: plan.copy_window().effective_from,
            effective_to: plan.copy_window().effective_to,
            reason_code: plan.reason_code.clone(),
        },
        stamp,
    )
    .await?;

    Ok(CutoverWritten {
        shortened,
        successor_window,
        copy_window,
    })
}

/// Domain separation for the key-set hash, so a digest that names an **act** can
/// never be confused with one that pins **content**.
///
/// The crate's other two digests carry their own
/// ([`CONTENT_PIN_DOMAIN_SEP`](crate::domain::approval::content_pin::CONTENT_PIN_DOMAIN_SEP),
/// `THRESHOLD_PIN_DOMAIN_SEP`), and this one is read by a different comparison
/// than either: the subject register matches it for equality, while a pin is
/// compared against a re-derivation at approve.
const CUTOVER_KEY_SET_SEP: &[u8] = b"VHP-BSS-PRICING-CUTOVER-KEYSET-v1\x1f";

/// The hash of the **selected** keys, as D-28 names it.
///
/// **A set, so it is sorted and deduplicated first.** The selector is a set in the
/// payload and nothing downstream makes it a list, so two orderings of one
/// selection are one act and a key named twice is one member. Sorting is over the
/// canonical [`ScopeKey`] rendering, which is total and stable.
///
/// **Length-framed, so two selections cannot be re-split into each other.** The
/// axis values are operator-supplied strings and nothing forbids a separator
/// character inside one, so a plain join would let two distinct selections share a
/// preimage — the hazard `content_pin`'s framing exists for, met here at a
/// different layer.
///
/// It hashes [`ScopeKey`]'s own rendering rather than an encoding written out
/// here, and that is the point rather than a shortcut: `Display` is fixed at ten
/// segments by its own doc, so this hash discriminates on every axis the key has
/// and gains an eleventh without being edited. A hand-listed encoding is what left
/// `content_pin::put_scope_key` on eight.
fn key_set_hash(selected: &[ScopeKey]) -> String {
    let mut rendered: Vec<String> = selected.iter().map(ScopeKey::to_string).collect();
    rendered.sort_unstable();
    rendered.dedup();

    let mut preimage = CUTOVER_KEY_SET_SEP.to_vec();
    preimage.extend_from_slice(
        &u64::try_from(rendered.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for key in &rendered {
        preimage.extend_from_slice(&u64::try_from(key.len()).unwrap_or(u64::MAX).to_be_bytes());
        preimage.extend_from_slice(key.as_bytes());
    }
    hex_bytes(sha256(&SHA256, &preimage).as_ref())
}

/// The name of one cutover **act**, as its approval unit and any retry of the
/// request look it up by: `(planId, key-set hash, cutover instant)`.
///
/// **D-28 and S7 §5's API row both spell it that way**, and dropping the middle
/// term on the authority of `inst-gc-api` — which states no idempotency rule at
/// all — rests on the argument that the selection is content, so rendering it into
/// the subject would make a narrowed retry a different act. It is a different act:
/// the selection is *what is being cut over*, so a retry that drops a key is a
/// second request, and answering it out of the unit standing for the wider
/// selection is the failure rather than the protection. The caller is told
/// `submitted`, believes their narrower set is under review, and an approver
/// authorizes the key they removed.
///
/// **Nothing is lost by naming the selection, because a second unit over
/// overlapping keys does not open.** `inst-co-single-pending` pends every touched
/// key, so an overlapping second selection is refused at submit by name; a
/// *disjoint* second selection is two genuinely different acts on different keys,
/// and both may proceed. The subject was never what kept the two apart.
///
/// The difference from
/// [`supersession_unit_ref`](crate::infra::supersession::supersession_unit_ref) is
/// therefore only in arity: that act names its one key, this one names its set,
/// and both name what they change.
///
/// The instant is at the millisecond quantum for `supersession_unit_ref`'s reason:
/// it is matched for equality by a retry, and two renderings of one instant at
/// different resolutions are two subjects.
#[must_use]
pub fn cutover_unit_ref(
    plan_id: PlanId,
    selected: &[ScopeKey],
    cutover_at: OffsetDateTime,
) -> String {
    format!(
        "{}/cutover/{}/{}",
        plan_id.get(),
        key_set_hash(selected),
        format_rfc3339(cutover_at)
    )
}

/// The prefix every one of a plan's cutover unit refs starts with.
///
/// Beside [`cutover_unit_ref`] because a prefix scan and the renderer it scans for
/// are one shape: a reader that spelled `"{plan}/cutover/"` for itself would keep
/// matching after the renderer moved a segment, and the mismatch is a query that
/// silently answers nothing.
#[must_use]
pub fn cutover_unit_ref_prefix(plan_id: PlanId) -> String {
    format!("{}/cutover/", plan_id.get())
}

/// The cutover instant [`cutover_unit_ref`] rendered into `subject_ref`.
///
/// The decoder sits against the encoder so the pair cannot drift — the segment
/// count and the millisecond quantum are one fact stated once.
///
/// **Two answers, because the two failures are not alike.** `Ok(None)` is a ref
/// whose head is not `{plan}/cutover/` at all: not this function's business, and
/// not anybody's failure to report. A ref that *is* under that head and still
/// will not decode is [`RepoError::CorruptRow`] — this crate is its only writer,
/// so it means the approval plane was written around the gear.
///
/// The distinction is load-bearing at the one call site.
/// [`retirement::ensure_no_live_cutover`](crate::infra::retirement) reads it to
/// decide whether a plan carries a live cutover, over rows
/// `approval_repo::cutover_units_of_plan` has already narrowed to that head —
/// so an unreadable ref folded to `None` would answer "no live cutover" and let
/// the retirement strand a shortened predecessor window against a successor
/// standing on a retired plan, which is the state that guard exists to refuse.
///
/// The alternative shape — carrying the instant in a column of its own — costs a
/// migration and a second writer for a value the subject already names uniquely,
/// and leaves the two able to disagree about which instant a unit is for.
///
/// # Errors
/// [`RepoError::CorruptRow`] for a ref under this head that does not decode.
pub fn cutover_instant_of_unit_ref(subject_ref: &str) -> Result<Option<OffsetDateTime>, RepoError> {
    let corrupt = || {
        RepoError::CorruptRow(format!(
            "pricing_approval subject_ref `{subject_ref}` is under a cutover head and does not \
             decode to one"
        ))
    };
    let mut segments = subject_ref.split('/');
    let (Some(_plan), Some(kind)) = (segments.next(), segments.next()) else {
        return Ok(None);
    };
    if kind != "cutover" {
        return Ok(None);
    }
    let (Some(_key_set_hash), Some(instant)) = (segments.next(), segments.next()) else {
        return Err(corrupt());
    };
    if segments.next().is_some() {
        return Err(corrupt());
    }
    Ok(Some(
        crate::domain::instant::parse_rfc3339(instant).map_err(|_| corrupt())?,
    ))
}

/// The canonical scope keys one cutover entry holds while its unit is pending
/// (`inst-co-single-pending`).
///
/// **Two, and the second is the cutover's own.** The supersession pends the one key
/// it reprices; a cutover also mints a generation, so it pends *"the
/// `all_subscriptions` key and the **new generation's** key — prior generations are
/// not pended"*. Both are needed and neither is spare: without the first, a window
/// mutation could be approved against the key mid-cutover; without the second, two
/// cutovers of one plan at the same instant would each believe the generation free,
/// and the loser would discover it as a partial-`UNIQUE` violation inside its commit
/// rather than as a refusal at submit.
///
/// Prior generations are excluded **by construction rather than by a filter**: this
/// function is handed the two keys the act touches, and a prior generation is not one
/// of them. Stated because "prior generations are not pended" reads like something a
/// filter enforces, and here there is nothing to filter.
///
/// # Errors
///
/// Whatever [`grandfathered_copy_key`] refuses — a generation that already carries
/// the instant, or an axis the constructor will not take.
pub fn cutover_held_keys(
    predecessor: &ScopeKey,
    cutover_at: OffsetDateTime,
    existing_generations: &[Cohort],
) -> Result<[ScopeKey; 2], DomainError> {
    Ok([
        predecessor.clone(),
        grandfathered_copy_key(predecessor, cutover_at, existing_generations)?,
    ])
}

// ---------------------------------------------------------------------------
// The unit: `inst-gc-compose`'s composer and `inst-gc-commit`'s owner.
// ---------------------------------------------------------------------------

/// One cutover request, as the surface hands it over (`inst-gc-api`).
///
/// `infra::supersession::SupersessionRequest`'s shape with the cutover's second
/// arrival: two more minted ids, for the grandfathered copy's row and its window.
/// The same note applies to all four — they are the **surface's**, minted per
/// request, and the ones the act really uses are the staged rows' when an earlier
/// attempt staged them. That is why the act's subject
/// ([`cutover_unit_ref`]) names none of them.
///
/// **One key, and D-28's selector is wider.** The register decided a cutover may
/// select several keys at one instant; this request carries one, and
/// [`ApprovalService::submit_cutover_on`](crate::infra::approval::ApprovalService::submit_cutover_on)
/// already takes the set. The multi-key payload is the surface's to build, and the
/// unit below composes per key when it arrives — recorded here rather than left as
/// a silent narrowing.
#[derive(Clone, Debug)]
pub struct CutoverRequest {
    /// The key being cut over: the `all_subscriptions` row's own.
    pub predecessor_key: ScopeKey,
    /// The instant all three window operations pivot on.
    pub cutover_at: OffsetDateTime,
    /// The successor's authored content, landing on the predecessor's own key.
    pub successor: PriceContent,
    /// The id the successor draft is authored under, if this call stages it.
    pub successor_price_id: Uuid,
    /// The id the successor's window is scheduled under, if this call commits.
    pub successor_window_id: Uuid,
    /// The id the grandfathered copy is authored under, if this call stages it.
    pub copy_price_id: Uuid,
    /// The id the copy's window is scheduled under, if this call commits.
    pub copy_window_id: Uuid,
    /// The operator-supplied change reason both scheduled windows are recorded under.
    pub reason_code: String,
}

/// What a cutover did: it committed, or it opened a unit and stood *almost* still.
///
/// [`crate::infra::supersession::SupersessionOutcome`]'s two arms and the same
/// reason for them: a materially-refused cutover has written its **two drafts**,
/// because they are the reviewer's subject and there is nothing to review until
/// they exist. What has not happened is the flip, both publishes and all three
/// window operations.
#[derive(Clone, Debug)]
pub enum CutoverOutcome {
    /// The unit committed: five writes, one version handle, four events owed.
    Committed(Box<CutoverReceipt>),
    /// A second principal is required. Both drafts stand; nothing else moved.
    SubmittedForApproval(Box<CutoverPending>),
}

/// What a committed cutover leaves behind.
#[derive(Clone, Debug)]
pub struct CutoverReceipt {
    /// The plan whose subject re-projects.
    pub plan_id: PlanId,
    /// The revision that subject stands at.
    pub revision: u64,
    /// The row that left the key.
    pub predecessor_price_id: Uuid,
    /// The row that took it.
    pub successor_price_id: Uuid,
    /// The retained copy, on its own generation.
    pub copy_price_id: Uuid,
    /// The generation the copy was minted on.
    pub copy_key: ScopeKey,
    /// The instant coverage handed over.
    pub cutover_at: OffsetDateTime,
    /// The predecessor's window, now ending at the cutover.
    pub shortened_window_id: Uuid,
    /// The registry's **pending** handle: a 202 in a value.
    pub pending_version_ref: String,
    /// The approval unit that authorized this commit, as `SupersessionReceipt`
    /// carries its own.
    ///
    /// `None` only on the fail-open arm the gate itself decides; a committed
    /// cutover under governance names its unit. It is here because `approval_ref`
    /// is the sole join between the approved unit and the act it authorized: written
    /// in neither place — no audit record and no receipt field — a caller holding a
    /// cutover cannot name the second principal who signed it.
    pub authorization: Option<Uuid>,
}

/// What an opened cutover unit leaves behind: two drafts and a record.
#[derive(Clone, Debug)]
pub struct CutoverPending {
    /// The plan the unit's subject belongs to.
    pub plan_id: PlanId,
    /// The revision the content pin covers.
    pub revision: u64,
    /// The staged successor's id — the **stored** one when an earlier attempt
    /// staged it, never the id this call minted.
    pub successor_price_id: Uuid,
    /// The staged copy's id, on the same rule.
    pub copy_price_id: Uuid,
    /// The generation the copy stands on.
    pub copy_key: ScopeKey,
    /// The unit a second principal has to decide.
    pub approval: ApprovalRecord,
}

/// The plan-scoped facts one cutover is judged against, read once inside the
/// writing transaction (D-176).
struct CutoverContext {
    plan_id: PlanId,
    revision: u64,
    lifecycle_state: LifecycleState,
    shape: PlanShape,
    predecessor: PriceRecord,
    staged_successor: Option<PriceRecord>,
    staged_copy: Option<PriceRecord>,
    plane: Vec<NamedWindow>,
    stored_plane: Vec<WindowRecord>,
    /// Every generation the plan already carries on this market — `inst-co-copy`'s
    /// duplicate refusal needs them, and it is a read rather than a guess.
    existing_generations: Vec<Cohort>,
}

impl CutoverContext {
    /// The stored window the compose decided to shorten, with its act counter.
    fn shorten_target(&self, window_id: Uuid) -> Result<&WindowRecord, DomainError> {
        self.stored_plane
            .iter()
            .find(|record| record.window_id == window_id)
            .ok_or_else(|| DomainError::NotFound {
                subject: "price window".to_owned(),
                id: window_id.to_string(),
            })
    }
}

/// The cutover workflow (`inst-gc-api`).
#[derive(Clone)]
pub struct CutoverService {
    db: DBProvider<DbError>,
    /// D-344 Tier B's parameters. Owned rather than shared, for
    /// [`crate::infra::migration::MigrationService`]'s reason: the repo is a
    /// reader bound to the ratified deployment defaults, so a second instance is
    /// a second reader of the same rows and never a second source of truth.
    policies: PolicyObjectRepo,
    /// The joint-conformance gate, asked of a cutover's rows because their
    /// content is **authored** (D-344). One gate, loaded once at gear init, so
    /// two doors cannot answer from two corpora.
    fixture_gate: FixtureGate,
    registry: Arc<dyn CatalogVersionRegistryV1>,
}

impl CutoverService {
    /// Build the workflow over one provider, the deployment's limits, the joint
    /// fixture gate and the resolved registry.
    ///
    /// The **same** registry `Arc` every other requester holds; what keeps their
    /// handles apart is `unit_request_id`'s distinct first segment. The gate is
    /// likewise the engine's own instance rather than a second `load` — the
    /// corpus decides what publishes, and two readings of it are two answers.
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
        }
    }

    /// Compose and, if it may, commit one grandfathering cutover.
    ///
    /// The whole act in **one** transaction of this service's own. See
    /// [`cutover_in`] for the order and why every step is where it is.
    ///
    /// # Errors
    /// [`cutover_in`]'s, exactly.
    pub async fn cut_over(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        request: CutoverRequest,
        verdict_json: VerdictJson,
        stamp: AuditStamp,
    ) -> Result<CutoverOutcome, DomainError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        let registry = Arc::clone(&self.registry);
        let policies = self.policies.clone();
        let gate = self.fixture_gate.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<CutoverOutcome, DomainError, _>(move |txn| {
                Box::pin(async move {
                    // Boxed a second time on purpose: the orchestrator's own future
                    // is large — eleven steps, a whole `PlanShape` and two staged
                    // records live across its awaits — and `clippy::large_futures`
                    // is what says so. The cost is one allocation per act; the
                    // alternative is that size on every task's stack.
                    Box::pin(cutover_in(
                        txn,
                        &policies,
                        &gate,
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
                DomainError::Internal(format!("bss-pricing: cutover transaction: {infra}"))
            })
        })
    }
}

/// One cutover, composed and committed or composed and submitted, in the caller's
/// transaction.
///
/// **`infra::supersession::supersede_in`'s eleven steps, side by side, with three
/// differences** — and they are written here in that order so the two can be read
/// against each other:
///
/// 1. **Two keys through `inst-co-single-pending` instead of one.** The
///    `all_subscriptions` key and the generation this act mints.
/// 2. **A second staged row.** The grandfathered copy, on the generation's key,
///    beside the successor on the predecessor's own.
/// 3. **The verdict is fixed, not asked.** `inst-mat-registered` registers a
///    grandfathering cutover, so `evaluate` answers `alwaysMaterialTrigger` from
///    the **act** half and no threshold policy is read. This is also the function
///    that makes `Trigger::GrandfatheringCutover`'s subject exist in this crate:
///    it is the surface that declares the act, which is what
///    `domain::materiality::triggers`' module doc requires before that predicate
///    may answer `true`.
///
/// # Errors
///
/// Whatever the compose refuses (`CUTOVER_INSTANT_PASSED`, `CUTOVER_GAP`,
/// `WINDOW_OVERLAP`, a duplicate generation), whatever the approval plane refuses
/// (`PENDING_CHANGE_UNIT_EXISTS`), whatever the row and window doors refuse, and
/// [`DomainError::Internal`] on a storage or registry failure.
#[allow(
    clippy::too_many_arguments,
    reason = "`supersede_in`'s argument list, plus D-344's two collaborators: the transaction, the \
              tenant policy reader and the joint fixture gate the aggregate tier needs, the \
              registry, the security context, the scope, the tenant, the request, the verdict \
              renderer and the stamp. Every one is a different authority and none is derivable \
              from the others"
)]
pub async fn cutover_in(
    txn: &DbTx<'_>,
    policies: &PolicyObjectRepo,
    gate: &FixtureGate,
    registry: &Arc<dyn CatalogVersionRegistryV1>,
    ctx: &SecurityContext,
    scope: &AccessScope,
    tenant_id: Uuid,
    request: &CutoverRequest,
    verdict_json: VerdictJson,
    stamp: AuditStamp,
) -> Result<CutoverOutcome, DomainError> {
    let now = stamp.recorded_at;

    // 0. The refusal that needs no world at all, answered from the key alone and
    //    ahead of the registry request because it is **permanent** (D-156). A
    //    grandfathered generation may not be superseded (Foundation §4.3), and a
    //    cutover's successor supersedes the key it lands on — so a generation is not
    //    cuttable-over either, for the same reason and through the same spelling.
    price_repo::refuse_unsupersedable_class(&request.predecessor_key)
        .map_err(|e| repo_failure(&e))?;

    // 1. Every fact the judgement needs, read inside the transaction that writes.
    let context = read_cutover_context(
        txn,
        scope,
        tenant_id,
        &request.predecessor_key,
        request.cutover_at,
        now,
    )
    .await?;

    // 1a. A plan whose current revision can never be superseded takes no successor
    //     row either — `PublishService::commit`'s hoisted refusal and
    //     `supersede_in`'s step 1a, and it was missing here. `plan_repo::retire_revision`
    //     flips only the plan row, so a retired plan's price rows still read
    //     `published`, step 1's `on_key(key, Published)` still finds the predecessor,
    //     and nothing further down reads the plan's lifecycle: `commit_supersession_rows`
    //     runs `refuse_mispaired` / `supersede_row` / `publish_rows` and none of the
    //     three asks. So a cutover onto a retired plan staged two drafts, took a
    //     registry handle and published both rows. Hoisted because the refusal is
    //     **permanent** (D-156): past the handle it stands pending forever and trips
    //     `pricing.catalogversion.commit_overdue` for a publish that can never happen.
    crate::infra::publish::refuse_unpublishable_predecessor(txn, scope, tenant_id, context.plan_id)
        .await?;

    // 2 and 2a. `inst-gc-compose` plus D-344 Tier A, at the floor **every** call must
    //    clear and on both arms, because this is the last point the two share. The
    //    commit floor is stricter and is applied at step 7; this run is what refuses a
    //    request nobody should be asked to review.
    let (composed, copy_key) = compose_and_judge(&context, request, now, ChangeoverMoment::Submit)?;

    let selected = [request.predecessor_key.clone()];
    let subject_ref = cutover_unit_ref(context.plan_id, &selected, request.cutover_at);

    // 3. **An approved unit decides before the evaluator is asked**, exactly as the
    //    supersession does and for the reason its step 3 records: consulting the
    //    evaluator first let a threshold raised between the two calls make an act
    //    auto-publishable at the second one, committing on a single principal while
    //    the unit opened over it stayed `submitted` forever.
    let authorization = crate::infra::approval::authorizing_unit(
        txn,
        scope,
        tenant_id,
        &context.shape,
        &subject_ref,
    )
    .await?;

    // 4. The verdict, and it is **fixed**. `ChangeSet::of_act` is the declaration
    //    `domain::materiality::triggers` requires of a surface, and `evaluate`
    //    answers the act half before it reads anything else — so no threshold policy
    //    is fetched and none could change the answer. The rows go in so the stored
    //    verdict carries what moved, not so the delta decides anything.
    let verdict = materiality::evaluate(
        &ChangeSet::of_act(
            Trigger::GrandfatheringCutover,
            [context.predecessor.clone()],
        ),
        None,
        None,
    );

    if authorization.is_none() {
        // 3b. The content this act is really about, before the retry is answered out
        // of the unit standing for it. `infra::supersession` asks this ahead of its
        // own pending lookup for the same reason: a caller who edited the successor
        // and re-`POST`ed was answered *for a commit of the content they no longer
        // asked for*, and a replay is the one answer that cannot say so.
        refuse_divergent_staged(&context, request, &copy_key)?;

        // 4. This act's **own** pending unit — `inst-gc-api`'s idempotency, answered
        //    before anything is staged, because the answer is that nothing more
        //    should be. `authorizing_unit` looks for an *approved* record; a
        //    `submitted` one means the act is under review, and a retry of it must
        //    find that unit rather than try to open a second one over the same
        //    subject. Without this arm the retry reaches `submit_cutover_on` and is
        //    refused `PENDING_CHANGE_UNIT_EXISTS` by its own act — measured, by the
        //    replay case, which failed exactly that way.
        if let Some(held) = approval_repo::find_pending_for_subject(txn, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            // The **stored** rows, never the ids this call minted: the drafts belong
            // to the act, and `SupersessionRequest`'s doc makes the same point about
            // the id a second call brings.
            let successor = context
                .staged_successor
                .as_ref()
                .map_or(request.successor_price_id, |row| row.price_id);
            let copy = context
                .staged_copy
                .as_ref()
                .map_or(request.copy_price_id, |row| row.price_id);
            return Ok(CutoverOutcome::SubmittedForApproval(Box::new(
                CutoverPending {
                    plan_id: context.plan_id,
                    revision: context.revision,
                    successor_price_id: successor,
                    copy_price_id: copy,
                    copy_key,
                    approval: held,
                },
            )));
        }

        // 5. and 6. The controlled arm: stage both rows, open the unit over both
        //    keys, write nothing else.
        //     Boxed for `cut_over`'s reason one level down: this arm now assembles a
        //     whole `PlanShape` for the fixture gate, and `clippy::large_futures` is
        //     what says the caller's stack should not carry it.
        return Box::pin(submitted_cutover(
            txn,
            gate,
            scope,
            tenant_id,
            &context,
            request,
            &copy_key,
            &selected,
            verdict,
            verdict_json,
            stamp,
        ))
        .await;
    }

    // 6. `inst-co-single-pending` on the **committing** arm too, which is the gap the
    //    a review found on the supersession: asking only inside the submit
    //    means an act that arrives already approved walks over a key another unit is
    //    reviewing. This unit's own approved record does not hold its keys — the
    //    register row follows its parent out of `submitted` through a trigger.
    refuse_held_keys(txn, scope, tenant_id, &request.predecessor_key, &copy_key).await?;

    // 7. `inst-gc-compose` re-run at the **commit** floor, which is the stricter one,
    //    and the divergence guard beside it — `supersede_in`'s step 7 spends
    //    both here too. This is the arm that spends money: `authorizing_unit` answers
    //    on a subject naming the plan, the key set and the instant and **no content
    //    at all**, so without this the approved unit authorizes any body a later call
    //    brings, and what commits is the staged one the reviewer saw.
    check_cutover_instant(request.cutover_at, now, ChangeoverMoment::Commit)?;
    refuse_divergent_staged(&context, request, &copy_key)?;
    // **D-344 Tier A at the commit floor**, `supersede_in`'s step 7 verbatim: the
    // guard above has just proved the request is the stored successor, and this is
    // the arm that spends money. Re-run rather than trusted from step 2 for
    // `inst-su-compose`'s reason — the world moved between the two calls, and a pair
    // that composed legally at submit is refusable at commit with nothing else
    // having changed.
    judge_successor(&context, request, now, ChangeoverMoment::Commit)?;
    let shorten = context.shorten_target(composed.shorten().window_id)?;
    let shorten_seq = shorten.mutation_seq;

    // 8. Both drafts, staged now if no earlier attempt did — and **before** the
    //    registry request, for `supersede_in` step 8's reason: this insert is the
    //    compose's own write, and keeping content validation on the far side of the
    //    handle strands a refusal no retry can pass.
    //    8a is the same call's other half: **D-344's fixture gate**, over the two
    //    drafts this act would publish and the rows already beside them, after the
    //    staging because that is what puts them in the shape it ranges over.
    let (successor, copy) = stage_and_gate(
        txn, gate, scope, tenant_id, &context, request, &copy_key, now, stamp,
    )
    .await?;

    // 9. The five writes, in one order, in this transaction.
    let written = commit_cutover(
        txn,
        scope,
        tenant_id,
        CutoverCommit::of_composition(
            composed,
            context.plan_id,
            context.predecessor.price_id,
            shorten_seq,
            (successor.price_id, successor.row_version),
            request.successor_window_id,
            (copy.price_id, copy.row_version),
            request.copy_window_id,
            request.reason_code.clone(),
        ),
        stamp,
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // 9a. **D-344 Tier B**, over the plan as those writes just left it, and this is
    //     `infra::repricing::commit_plan_aggregate_in`'s placement rather than a new
    //     one: that door runs the identical pass after its own row writes, for the
    //     identical reason. See [`judge_plan_aggregate`] for why the aggregate
    //     cannot be asked before them.
    judge_plan_aggregate(txn, policies, scope, tenant_id, context.plan_id, now).await?;

    // 10. Addressability, fail closed — **after every refusal**, which is the half of
    //     D-156 that is load-bearing, and which Tier B's placement moved this line to
    //     honour. D-156's other half puts the request "before the writes"; step 9's
    //     writes are inside this transaction and a refusal at 9a rolls every one of
    //     them back, so nothing durable precedes the handle. Requesting it ahead of
    //     9a would put a **permanent** refusal — a rule violation is not something a
    //     retry passes — on the far side of the handle, and a handle stranded there
    //     stands pending forever and trips `pricing.catalogversion.commit_overdue`
    //     for a publish that can never happen. That is the failure D-156 exists to
    //     prevent, and it is why the ordering is stated as the reason rather than as
    //     the rule.
    let pending = request_version_now(
        registry.as_ref(),
        ctx,
        &cutover_request_id(tenant_id, &subject_ref),
    )
    .await?;

    // 11. The plan subject the projector re-freezes — one subject, a second reason to
    //     re-project it, exactly as the supersession's step 11 argues.
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

    // 12. The announcement, and it is **two** events rather than `inst-gc-commit`'s
    //     four. That clause lists `PriceCreated` x2 as well, on the ground that this
    //     unit's two rows are *"born published and pass no authoring door"* — which
    //     is false, and unavoidably so: both are staged as **drafts** at compose,
    //     because clause (a) makes them the reviewer's subject and a content pin over
    //     a shape that does not contain them is a unit no approve can satisfy. So
    //     both pass the authoring door, both announce there, and a second emission
    //     here would be a duplicate its own `PriceCreated/<price_id>` dedup key
    //     refuses. D-203's "second producer" conclusion goes with the premise.
    //
    //     Both schedules go through `price_window_mutation`, whose **act segment** is
    //     what keeps them out of a collision with any other `PriceWindowScheduled` on
    //     the same window: the subject is the act, and two cutovers of one key at two
    //     instants are two acts.
    // `PriceUpdated` for the successor, which this act announced to nobody until
    // D-218. The transition being announced is the **same** transition
    // `infra::supersession` announces — a row landing on an occupied published
    // canonical scope key and flipping its predecessor `published → superseded` —
    // and D-127 already binds both producers of it to one guard, on the ground that
    // the guard follows the key rather than the mechanism. A consumer that learned
    // "stop resolving the row this replaces" from a supersession learned nothing
    // from a cutover, so its correctness depended on which act moved the row. That
    // is the coupling D-100 removed on the truth side and this removes on the wire.
    //
    // The predecessor is `context.predecessor.price_id` rather than a column read:
    // a cutover's successor arrives as a **client-authored row ref**, so this
    // transaction is the only place that holds both sides of the link without a
    // lookup. `pending.pending_ref` is borrowed here and moved into the receipt
    // below, which is why this sits before the `Ok`.
    outbox_repo::enqueue(
        txn,
        scope,
        NewOutboxEvent::price_updated(
            tenant_id,
            &PriceUpdatedPayload {
                plan_id: context.plan_id,
                price_id: successor.price_id,
                scope_key: request.predecessor_key.to_string(),
                supersedes_price_id: context.predecessor.price_id,
                changeover: request.cutover_at,
                pending_version_ref: pending.pending_ref.clone(),
                correlation_id: stamp.correlation_id,
            },
            now,
        ),
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    let act = format!("cutover/{}", format_rfc3339(request.cutover_at));
    for (window, price_id) in [
        (&written.successor_window, successor.price_id),
        (&written.copy_window, copy.price_id),
    ] {
        outbox_repo::enqueue(
            txn,
            scope,
            NewOutboxEvent::price_window_mutation(
                tenant_id,
                WindowMutationEvent::Scheduled,
                &PriceWindowTransitionPayload {
                    window_id: window.window_id,
                    plan_id: context.plan_id,
                    price_id,
                    // **As written, not as asked for** — `commit_cutover` returns the
                    // rows for exactly this reason.
                    effective_from: window.effective_from,
                    effective_to: window.effective_to,
                    correlation_id: stamp.correlation_id,
                },
                now,
                &act,
            ),
        )
        .await
        .map_err(|e| repo_failure(&e))?;
    }

    // 13. The record of who did it, at the **act's** level rather than one per row
    //     moved — `supersession.rs:995`'s step verbatim, and this is the step the
    //     cutover's copy of that eleven-step skeleton stopped one short of.
    //
    //     **Nothing beneath this compensates for its absence.** Neither
    //     `publish_rows` nor `supersede_row` appends a record, and the submit half
    //     being audited (`submit_cutover_on` → `approval_repo::open`), along with the
    //     staging of the two drafts, leaves precisely the act that moves money
    //     unrecorded: the predecessor's supersession and the two publishes.
    //
    //     `approval_ref` is the point. It is the only join between the approved unit
    //     and the act it authorized, and on this path it was written nowhere — not
    //     on a record (there was none) and not on the receipt (no field) — so an
    //     auditor holding a cutover could not establish that a second principal had
    //     approved it, and `inst-tp-record` was unsatisfied for this act.
    //     §8's `dod-audit` is "every mutation MUST record".
    record_cutover(
        txn,
        scope,
        CutoverAudit {
            tenant_id,
            plan_id: context.plan_id,
            subject_ref: &subject_ref,
            predecessor: context.predecessor.price_id,
            successor: &successor,
            copy: &copy,
            shortened: ShortenedWindow::of(shorten, &written.shortened),
            approval_ref: authorization.as_ref().map(|record| record.approval_id),
            stamp,
            now,
        },
    )
    .await?;

    Ok(CutoverOutcome::Committed(Box::new(CutoverReceipt {
        plan_id: context.plan_id,
        revision: context.revision,
        predecessor_price_id: context.predecessor.price_id,
        successor_price_id: successor.price_id,
        copy_price_id: copy.price_id,
        copy_key,
        cutover_at: request.cutover_at,
        shortened_window_id: shorten.window_id,
        pending_version_ref: pending.pending_ref,
        authorization: authorization.as_ref().map(|record| record.approval_id),
    })))
}

/// The cutover's before/after, `supersession::unit_state`'s shape with the third
/// row this act moves.
///
/// A cutover publishes **two** rows — the successor and the grandfathered copy —
/// where a supersession publishes one, so a state naming only the successor would
/// leave the copy's transition unrecorded on the act that created it.
/// Everything the cutover's audit record names, gathered so the append is one
/// call — `clippy::too_many_lines` bounds `cutover_in` at 200 and the inline
/// spelling put it at 213, which is `error_mapping`'s `precondition` situation and
/// takes its remedy.
/// The shortened window on both sides of the act.
///
/// A pair rather than two fields on [`CutoverAudit`] for that struct's own reason:
/// `clippy::too_many_lines` bounds [`cutover_in`] at 200 and this assembly is what
/// pushed it over, which is the pressure that produced `CutoverAudit` to begin with.
struct ShortenedWindow<'a> {
    /// **As the transaction read it before writing** — D-05's *"recorded pre-cutover
    /// value"*, and the operand D-327 found missing.
    ///
    /// The stored row out of [`CutoverContext::stored_plane`], never
    /// [`ComposedCutover::shorten`]: the composition carries the end the act moves the
    /// window *to*, which is the cutover instant and is already recorded three other
    /// places. What is destroyed is the end it moved *from*, and only the stored row
    /// has it — so a re-read here would record the value the act produced and call it
    /// the value it destroyed.
    before: &'a WindowRecord,
    /// The same window as [`commit_cutover`] left it, returned by the write rather
    /// than re-read — `enqueue`'s "as written, not as asked for" one step down.
    after: &'a WindowRecord,
}

impl<'a> ShortenedWindow<'a> {
    /// Name the pair.
    ///
    /// A constructor rather than a struct literal at the call site, and the reason is
    /// this struct's own: the literal spans four formatted lines and puts [`cutover_in`]
    /// back over `clippy::too_many_lines`. Naming the two parameters here is also what
    /// keeps the order checked in one place instead of at the call site — the two are
    /// the same type, so a swap would compile and would record the act's own product as
    /// the value it destroyed.
    const fn of(before: &'a WindowRecord, after: &'a WindowRecord) -> Self {
        Self { before, after }
    }
}

struct CutoverAudit<'a> {
    tenant_id: Uuid,
    plan_id: PlanId,
    subject_ref: &'a str,
    predecessor: Uuid,
    successor: &'a PriceRecord,
    copy: &'a PriceRecord,
    /// The window this act shortens, on both sides of the write.
    shortened: ShortenedWindow<'a>,
    approval_ref: Option<Uuid>,
    stamp: AuditStamp,
    now: OffsetDateTime,
}

/// The record of who performed the cutover — `supersession.rs:995`'s step, and the
/// one the cutover's copy of that eleven-step skeleton stopped short of.
///
/// **Nothing beneath this compensates for its absence.** Neither `publish_rows`
/// nor `supersede_row` appends a record, and the submit half being audited
/// (`submit_cutover_on` → `approval_repo::open`), along with the staging of the
/// two drafts, leaves precisely the act that moves money unrecorded: the
/// predecessor's supersession and the two publishes.
///
/// `approval_ref` is the point. It is the only join between the approved unit and
/// the act it authorized, and on this path it was written nowhere — not on a record
/// (there was none) and not on the receipt (no field) — so an auditor holding a
/// cutover could not establish that a second principal had approved it, and
/// `inst-tp-record` was unsatisfied for this act. §8's `dod-audit` is "every
/// mutation MUST record".
///
/// # It names three rows **and the window**, and that fourth fact is D-05's operand
///
/// [`cutover_state`] carries the predecessor, the successor and the copy with
/// their lifecycle states, and — — the interval of the window this
/// act **shortens**, on both sides of the pair.
///
/// **It named no window until then, and nothing else the act writes named one
/// either.** The three outbox events are the succession and the two schedules, and
/// [`commit_cutover`] reaches `window_repo::adjust_effective_to`, an in-place
/// `UPDATE` of a table with no before-image column — so the end the act moved *from*
/// existed nowhere the moment the transaction committed. `infra::window`'s own adjust
/// path always recorded it (`before_state: window_state_value(current)`), which made
/// the same shorten auditable through `PATCH …/windows/{id}` and unauditable here;
/// the asymmetry was the defect, not the schema.
///
/// That was not only an audit gap. D-05 requires a retirement to restore the
/// predecessor's `effectiveTo` *"to its recorded pre-cutover value"*, and this is the
/// act that has to record it: the value is **not derivable afterwards**, because
/// [`compose_cutover_windows`] accepts both an open-ended predecessor and one ending
/// strictly after the cutover, and after the shorten those two are the same row.
/// D-327 concluded the unwind was unbuildable on exactly that ground.
///
/// # Why the record is now addressable, which is what made writing it worth doing
///
/// The reason this was *not* repaired earlier was sound and had to be answered
/// rather than dropped: `audit_repo` offered [`append`](audit_repo::append) and a
/// tenant-wide keyset `page` with no query by subject, so a field added here would
/// have been written and unreadable — the appearance of the operand rather than the
/// operand. [`audit_repo::for_subject`] is that answer. It is a ~12-line read over
/// `idx_pricing_audit_log_subject`, an index that had existed since
/// `pricing_audit_log` with no reader at all, and `audit.subject_ref` is already
/// act-unique and reconstructible from the plan, the key set and the instant
/// ([`cutover_unit_ref`]) — so a caller holding a cutover can name the record
/// without having stored anything.
///
/// The unwind itself stays unbuilt, and deliberately: D-05's fourth clause closes
/// the unit as `unwound`, a state D-333 has only just declared and whose store
/// column does not exist. What lands here is the operand and its reader.
/// `tests/sqlite_cutover_unwind.rs` holds both, and holds the two clauses that are
/// still absent.
async fn record_cutover(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    audit: CutoverAudit<'_>,
) -> Result<(), DomainError> {
    audit_repo::append(
        txn,
        scope,
        NewAuditEntry {
            tenant_id: audit.tenant_id,
            chain_id: audit_repo::plan_chain(audit.plan_id),
            recorded_at: audit.now,
            actor_principal_id: audit.stamp.actor_principal_id,
            action: AuditAction::Publish,
            subject_kind: AuditSubjectKind::PriceUnit,
            // The act, not either row: `price_unit_ref` names a *row*, which cannot
            // tell a cutover from a `PATCH` on the same id, and the unit's approval
            // record already stands under this exact string.
            subject_ref: audit.subject_ref.to_owned(),
            before_state: Some(cutover_state(
                audit.predecessor,
                LifecycleState::Published,
                audit.successor,
                audit.copy,
                LifecycleState::Draft,
                &crate::infra::window::shortened_window_state_value(audit.shortened.before),
            )),
            after_state: Some(cutover_state(
                audit.predecessor,
                LifecycleState::Superseded,
                audit.successor,
                audit.copy,
                LifecycleState::Published,
                &crate::infra::window::shortened_window_state_value(audit.shortened.after),
            )),
            approval_ref: audit.approval_ref,
            correlation_id: audit.stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| repo_failure(&e))
}

fn cutover_state(
    predecessor: Uuid,
    predecessor_state: LifecycleState,
    successor: &PriceRecord,
    copy: &PriceRecord,
    published_state: LifecycleState,
    shortened_window: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "predecessorPriceId": predecessor,
        "predecessorState": predecessor_state.as_str(),
        "successorPriceId": successor.price_id,
        "successorState": published_state.as_str(),
        "successorScopeKey": successor.scope_key.to_string(),
        "copyPriceId": copy.price_id,
        "copyState": published_state.as_str(),
        "copyScopeKey": copy.scope_key.to_string(),
        // The fourth thing this act moves, and the only one whose prior value the
        // store overwrites in place. Rendered by
        // `infra::window::shortened_window_state_value` rather than spelled here,
        // because `infra::supersession` writes the identical fact for the identical
        // reason and a second spelling is how two records of one shape come to
        // disagree about a field name a reader has to guess.
        "shortenedWindow": shortened_window,
    })
}

/// The controlled arm: stage both rows, open the unit over both keys, write nothing
/// else.
#[allow(
    clippy::too_many_arguments,
    reason = "the context, the request, the minted copy key, the selection, the verdict and its \
              renderer, plus the runner, D-344's fixture gate, the scope, the tenant and the \
              stamp. Splitting it would hand the halves to two functions that would both need all \
              of it"
)]
async fn submitted_cutover(
    runner: &DbTx<'_>,
    gate: &FixtureGate,
    scope: &AccessScope,
    tenant_id: Uuid,
    context: &CutoverContext,
    request: &CutoverRequest,
    copy_key: &ScopeKey,
    selected: &[ScopeKey],
    verdict: MaterialityVerdict,
    verdict_json: VerdictJson,
    stamp: AuditStamp,
) -> Result<CutoverOutcome, DomainError> {
    let (successor, copy) = stage_and_gate(
        runner,
        gate,
        scope,
        tenant_id,
        context,
        request,
        copy_key,
        stamp.recorded_at,
        stamp,
    )
    .await?;
    // **D-344's fixture gate on this arm too**, and for step 2's reason rather
    // than because this arm publishes anything: it publishes nothing.
    // `check_cutover_instant` runs its Submit floor here because "this run is what
    // refuses a request nobody should be asked to review", and an act whose rows
    // the corpus will refuse is exactly such a request — without it a second
    // principal reviews and approves a cutover that is then refused permanently on
    // the call that would have committed it. `PublishService::precheck` asks the
    // gate for the same reason, on a surface that also writes nothing.
    //
    // **Tier B is not asked here and cannot be.** See [`judge_plan_aggregate`]: the
    // aggregate is a statement about the plan *after* the act, and until the commit
    // flips the predecessor `superseded` this plan carries two rows on one key —
    // which `inst-cmp-injective` correctly reports as a duplicate metered line. The
    // arm that can answer it truthfully is the one that has written.
    let record = crate::infra::approval::ApprovalService::submit_cutover_on(
        runner,
        scope,
        tenant_id,
        selected,
        request.cutover_at,
        Uuid::now_v7(),
        verdict_json(&verdict)?,
        stamp,
    )
    .await?;
    Ok(CutoverOutcome::SubmittedForApproval(Box::new(
        CutoverPending {
            plan_id: context.plan_id,
            revision: context.revision,
            successor_price_id: successor.price_id,
            copy_price_id: copy.price_id,
            copy_key: copy_key.clone(),
            approval: record,
        },
    )))
}

/// Refuse a call whose rows are not the rows this act already staged.
///
/// **`infra::supersession`'s guard, spent on both of this act's rows**, and the
/// step a copy of that eleven-step skeleton most easily drops. Without it a caller
/// who edits the successor and re-`POST`s under a pending unit is answered `202`
/// and a replay, and one who does it after the approve is answered `200` for a
/// commit of the **staged** body — neither answer saying which content was
/// committed. The supersession answers this 409, and a sibling written from the
/// same skeleton has to as well or it silently does something other than what the
/// submitted body said.
///
/// **Both sides are the row as the store holds it**, which is
/// [`crate::infra::supersession::refuse_divergent_successor`]'s own rule and the half
/// that is easy to get wrong: the staged side is a record read back, so the request's
/// side has to go through `price_repo::authored_content` — the one spelling of the two
/// rewrites the authoring door performs, `charge_kind` from the key and the band order.
/// Comparing the wire's row instead is what made every non-recurring key
/// unsupersedable on the supersession, and it would do the same here.
///
/// **The copy's authored side is the predecessor's content, not the request's.**
/// [`stage_both`] writes the row being closed onto a generation of its own key, so a
/// staged copy that does not carry the predecessor's content is not this act's copy —
/// the case `read_cutover_context`'s D-300 filter narrows by key and this narrows by
/// content.
///
/// # Errors
/// [`DomainError::DuplicateScopeKey`] naming whichever staged row diverged.
fn refuse_divergent_staged(
    context: &CutoverContext,
    request: &CutoverRequest,
    copy_key: &ScopeKey,
) -> Result<(), DomainError> {
    if let Some(staged) = context.staged_successor.as_ref() {
        crate::infra::supersession::refuse_divergent_successor(
            staged,
            &authored_successor(context, request),
        )?;
    }
    if let Some(staged) = context.staged_copy.as_ref() {
        crate::infra::supersession::refuse_divergent_successor(
            staged,
            &price_repo::authored_content(copy_key, context.predecessor.content()),
        )?;
    }
    Ok(())
}

/// The successor **as the door would write it**.
///
/// `infra::supersession::requested_content`'s three rewrites, and all three matter:
/// `price_repo::authored_content` applies the key's `charge_kind` and the band order —
/// the two the store performs and the wire cannot express — and the supersession link
/// is stamped from the predecessor this transaction read, overwriting whatever the
/// caller sent, exactly as `insert_successor_draft_on` does.
fn authored_successor(context: &CutoverContext, request: &CutoverRequest) -> PriceContent {
    PriceContent {
        supersedes_price_id: Some(context.predecessor.price_id),
        ..price_repo::authored_content(&request.predecessor_key, request.successor.clone())
    }
}

/// **D-344 Tier A on this door**: the per-row rules and the supersession rules over
/// the successor this act would publish.
///
/// [`plan_supersession`] is the pair, and it is called here rather than restated for
/// the reason D-127 gives about the guard itself: a rule set that can differ by which
/// mechanism asked already differs. `supersede_in` spends this at its steps 2 and 7
/// and this door spends it at the same two, so the two acts are judged by one
/// statement of what a successor may be.
///
/// **The successor as the store will hold it**, never the request's row — the
/// [`authored_successor`] rewrites, whose absence made `is_usage()` answer `false` on
/// every usage key and cost the supersession three Criticals.
///
/// # Why it sits after `check_cutover_instant` and `compose_cutover_windows`
///
/// [`plan_supersession`] asks three questions and this act has its own spelling of
/// the first two. Its instant floor is [`changeover_floor`](crate::domain::supersession::changeover_floor)
/// — the identical bound `check_cutover_instant` applies — and its window composition
/// is `compose_windows`, which asks `compose_cutover_windows`' first two questions on
/// the same plane. Both are pure functions of arguments this call does not change, so
/// once the cutover's own two have passed neither can fire, and the operator hears
/// `CUTOVER_INSTANT_PASSED` and `CUTOVER_GAP` rather than the supersession's codes for
/// the same facts. Ordered rather than trimmed to the rule sets alone, because
/// `plan_supersession` is the one spelling of the pair and a caller picking two of its
/// three parts is how the next divergence starts.
///
/// The composed plan it returns is discarded: this act's three window operations come
/// from [`compose_cutover_windows`], which is the composer that knows about the copy.
///
/// # Errors
/// [`DomainError::ValidationFailed`] carrying the row and supersession report —
/// `SUPERSESSION_UNIT_MISMATCH` among them, which is what a cutover moving `meter`,
/// `dimensionKey`, `model_kind`, `reservationFlavor` or a `carry` allowance under a
/// continued tier counter now hears.
fn judge_successor(
    context: &CutoverContext,
    request: &CutoverRequest,
    now: OffsetDateTime,
    moment: ChangeoverMoment,
) -> Result<(), DomainError> {
    plan_supersession(
        &context.predecessor.row,
        &authored_successor(context, request).row,
        &context.plane,
        request.cutover_at,
        now,
        moment,
    )?;
    Ok(())
}

/// The plan's candidate row set, read through whichever runner the caller holds.
///
/// One assembly for both of D-344's plan-scoped arms, so a shape one of them judged
/// and the other did not cannot exist. `assemble_from` against the plan's **current**
/// revision — a cutover opens no draft revision — which is `infra::repricing`'s entry
/// point too.
///
/// # Errors
/// [`DomainError::NotFound`] when the plan has no current revision;
/// [`DomainError::Internal`] on a storage failure.
async fn cutover_subject(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<PlanShape, DomainError> {
    let current = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "current plan revision".to_owned(),
            id: plan_id.to_string(),
        })?;
    assemble_from(runner, scope, tenant_id, plan_id, current, now).await
}

/// **D-344's fixture gate on this door**: is this deployment's joint corpus green for
/// every shape the plan carries?
///
/// The gate guards acts whose content is **authored** rather than derived, and a
/// cutover's successor is entirely client-authored, so it is the gate's second
/// subject. The repricing apply is deliberately not a third: `project_row` derives its
/// successor from a row whose own publish already passed here, so it can present no
/// shape this gate has not already seen.
///
/// **Asked on both arms**, unlike Tier B, because it is a question about each row
/// on its own and the answer does not depend on which rows are current. So it can be
/// put where `check_cutover_instant`'s Submit floor is put, and for that floor's
/// reason: a request the corpus will refuse is a request nobody should be asked to
/// review.
///
/// # Errors
/// [`DomainError::FixtureMissing`] naming the kind and the variant that has no green
/// fixture; whatever [`cutover_subject`] refuses.
async fn judge_authored_shapes(
    runner: &impl DBRunner,
    gate: &FixtureGate,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let shape = cutover_subject(runner, scope, tenant_id, plan_id, now).await?;
    check_fixtures(gate, &shape)
}

/// `inst-gc-compose` and D-344 Tier A together, at one moment.
///
/// The three refusals are ordered and the order is the reason [`judge_successor`]
/// goes last: the cutover's own instant floor and its own window composition are what
/// give an operator `CUTOVER_INSTANT_PASSED` and `CUTOVER_GAP` rather than the
/// supersession's codes for the same two facts, and Tier A's copy of both is
/// unreachable once they have passed.
///
/// [`grandfathered_copy_key`] sits between them because it is the act's own third
/// refusal — a generation already carrying this instant — and because Tier A needs no
/// part of it.
///
/// # Errors
/// [`DomainError::CutoverInstantPassed`], [`DomainError::CutoverGap`],
/// [`DomainError::WindowOverlap`], [`DomainError::DuplicateScopeKey`], and whatever
/// [`judge_successor`] refuses.
fn compose_and_judge(
    context: &CutoverContext,
    request: &CutoverRequest,
    now: OffsetDateTime,
    moment: ChangeoverMoment,
) -> Result<(ComposedCutover, ScopeKey), DomainError> {
    check_cutover_instant(request.cutover_at, now, moment)?;
    let composed = compose_cutover_windows(&context.plane, request.cutover_at)?;
    let copy_key = grandfathered_copy_key(
        &request.predecessor_key,
        request.cutover_at,
        &context.existing_generations,
    )?;
    judge_successor(context, request, now, moment)?;
    Ok((composed, copy_key))
}

/// **D-344 Tier B**: the plan aggregate, over the plan as this act's writes left it.
///
/// A cutover mints a **generation**, and `priceEligibility` is a scope-key axis, so
/// this act changes which keys are current — which is precisely the condition D-344
/// binds the aggregate tier to. A same-key supersession does not change that set and
/// does not run this, which is why `supersede_in` has no such call and its absence
/// there is a stated consequence rather than an omission.
///
/// # Why it runs after the writes and not before them
///
/// **Measured, not preferred.** `run_publish_rules` asserts things about *which rows
/// are current on which key*, and between `stage_both` and `commit_cutover` this plan
/// carries two rows on one key: the published predecessor and the draft successor
/// that will replace it. Asked there, `inst-cmp-injective` reports the pair as a
/// duplicate `(meter, dimensionKey)` line — correctly, on the shape it was handed —
/// and every cutover of a usage key is refused for a duplication the act itself is
/// halfway through removing. `inst-wc-required` is the same shape from the other side:
/// the grandfathered copy stands on a generation key nothing has covered until
/// `commit_cutover` schedules its window.
///
/// After the writes both dissolve without a flag or an exception:
/// [`CANDIDATE_ROW_STATES`](crate::infra::publish::CANDIDATE_ROW_STATES) excludes
/// `superseded`, so the predecessor has left the row set, and both new keys carry the
/// windows this act just wrote. That is the same placement and the same reason as
/// `infra::repricing::commit_plan_aggregate_in`, which runs the identical pass over
/// the plan "as this transaction just left it".
///
/// The transaction is what makes this safe rather than optimistic: a refusal here
/// rolls back all five writes, so the act's failure mode is unchanged and only the
/// registry request had to move (see step 10).
///
/// # Errors
/// [`DomainError::ValidationFailed`] carrying the aggregate report; whatever
/// [`cutover_subject`] refuses.
async fn judge_plan_aggregate(
    runner: &impl DBRunner,
    policies: &PolicyObjectRepo,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let shape = cutover_subject(runner, scope, tenant_id, plan_id, now).await?;
    let params = rule_params(policies, runner, scope, tenant_id, &shape).await?;
    let report = run_publish_rules(&shape, &params);
    if !report.is_publishable() {
        return Err(DomainError::ValidationFailed(report));
    }
    Ok(())
}

/// Neither key this act writes may be held by another live unit.
///
/// The pair named as a pair, rather than a `BTreeSet` assembled at the call site:
/// a cutover writes exactly two keys — the predecessor's own and the generation it
/// mints — and which two they are is the thing a reader of `cutover_in` wants,
/// where the set-building was four lines of plumbing around it.
///
/// # Errors
/// Whatever [`crate::infra::approval::refuse_held_key`] refuses.
async fn refuse_held_keys(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    predecessor: &ScopeKey,
    copy: &ScopeKey,
) -> Result<(), DomainError> {
    let held = std::collections::BTreeSet::from([predecessor.to_string(), copy.to_string()]);
    crate::infra::approval::refuse_held_key(runner, scope, tenant_id, &held).await
}

/// Stage both drafts, then ask D-344's fixture gate over the plan they leave.
///
/// **One function because the two are one act, and the two arms proved they are.**
/// The commit arm and [`submitted_cutover`] each did the pair in sequence, in the
/// same order, for the same reason — which made "a staging is always followed by
/// the gate" a coincidence of two call sites rather than a property. D-344 says
/// the gate is asked on *both* arms; this is where that stops depending on
/// remembering it at the second one.
///
/// `now` is a parameter rather than `stamp.recorded_at` because the two arms pass
/// different instants and always did: the commit arm judges against the call's own
/// clock and the submit arm against the stamp it is recording under. Folding them
/// would have been a behaviour change smuggled in as a refactor.
///
/// # Errors
/// Whatever [`stage_both`] or [`judge_authored_shapes`] refuses.
#[allow(
    clippy::too_many_arguments,
    reason = "`stage_both`'s list plus the gate and the instant the gate reads the plan at. \
              Every one is a different authority and none is derivable from the others - this \
              module's own standing reason, and the two it adds are exactly what makes the pair \
              one act"
)]
async fn stage_and_gate(
    runner: &DbTx<'_>,
    gate: &FixtureGate,
    scope: &AccessScope,
    tenant_id: Uuid,
    context: &CutoverContext,
    request: &CutoverRequest,
    copy_key: &ScopeKey,
    now: OffsetDateTime,
    stamp: AuditStamp,
) -> Result<(PriceRecord, PriceRecord), DomainError> {
    let staged = Box::pin(stage_both(
        runner, scope, tenant_id, context, request, copy_key, stamp,
    ))
    .await?;
    judge_authored_shapes(runner, gate, scope, tenant_id, context.plan_id, now).await?;
    Ok(staged)
}

/// Stage the successor and the grandfathered copy, reusing whatever an earlier
/// attempt staged.
///
/// **Two doors, not one, and that is `inst-co-copy` rather than a shortcut.** The
/// successor lands on a key a published row occupies, which is
/// `insert_successor_draft_on`'s precondition; the copy lands on a **new
/// generation**, a key nothing occupies at all, which is the authoring door's. A
/// single door could not admit both — each refuses the occupant the other requires
/// (D-195).
async fn stage_both(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    context: &CutoverContext,
    request: &CutoverRequest,
    copy_key: &ScopeKey,
    stamp: AuditStamp,
) -> Result<(PriceRecord, PriceRecord), DomainError> {
    let successor = if let Some(staged) = context.staged_successor.as_ref() {
        staged.clone()
    } else {
        let (record, _predecessor) = Box::pin(price_repo::insert_successor_draft_on(
            runner,
            scope,
            tenant_id,
            NewPriceDraft {
                price_id: request.successor_price_id,
                scope_key: request.predecessor_key.clone(),
                content: request.successor.clone(),
                created_by: stamp.actor_principal_id,
                created_at_utc: stamp.recorded_at,
                correlation_id: stamp.correlation_id,
            },
        ))
        .await
        .map_err(|e| repo_failure(&e))?;
        record
    };
    let copy = match context.staged_copy.as_ref() {
        Some(staged) => staged.clone(),
        None => Box::pin(price_repo::create_draft_on(
            runner,
            scope,
            tenant_id,
            NewPriceDraft {
                price_id: request.copy_price_id,
                scope_key: copy_key.clone(),
                // **The predecessor's content, not the successor's.** The copy is what
                // retained subscribers keep paying, so it is the row being closed,
                // carried onto a generation of its own key — a copy of the *new* price
                // would grandfather nobody.
                content: context.predecessor.content(),
                created_by: stamp.actor_principal_id,
                created_at_utc: stamp.recorded_at,
                correlation_id: stamp.correlation_id,
            },
        ))
        .await
        .map_err(|e| repo_failure(&e))?,
    };
    Ok((successor, copy))
}

/// The registry request id of one cutover unit.
///
/// Its own first segment, for `unit_request_id`'s reason: three requesters share one
/// registry and what keeps their handles apart is this prefix.
fn cutover_request_id(tenant_id: Uuid, subject_ref: &str) -> String {
    format!("cutover/{tenant_id}/{subject_ref}")
}

/// Read every fact the judgement needs, inside the writing transaction.
async fn read_cutover_context(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    key: &ScopeKey,
    cutover: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<CutoverContext, DomainError> {
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

    let on_key = |wanted: &ScopeKey, state: LifecycleState| {
        shape
            .rows
            .iter()
            .find(|row| row.scope_key == *wanted && row.lifecycle_state == state)
            .cloned()
    };
    let predecessor =
        on_key(key, LifecycleState::Published).ok_or_else(|| DomainError::NotFound {
            subject: "current price row on canonical scope key".to_owned(),
            id: key.to_string(),
        })?;
    let staged_successor = on_key(key, LifecycleState::Draft);

    // Every generation already minted on this market, whatever its state: a cutover
    // onto an instant one of them carries is refused at compose, and a *superseded*
    // generation still holds its instant — the key is immutable history, not a slot
    // that frees.
    //
    // **The whole key, and nothing weaker** (D-296, narrowed by D-300). This
    // predicate first spelled `currency` and `region` by hand — two of ten axes —
    // so on a plan carrying a second key in one market a cutover adopted the draft
    // staged for a *different* key and never minted its own. D-296 moved it to
    // `is_sibling_of`, which excludes `price_eligibility` and `cohort` by design;
    // with the class re-imposed beside it that still left **any generation** on the
    // market key matching, and a grandfathered draft is authorable directly through
    // `POST …/prices`. So a stranger's draft at the same instant would have been
    // adopted as this act's copy — retained subscribers billed from operator-typed
    // content instead of the predecessor's — and one at a different instant made
    // the act permanently un-committable behind `refuse_ungenerational`.
    //
    // What this site wants is the key this act *mints*, which is the predecessor's
    // with two axes moved. [`generation_key`] is that construction and the only
    // place that knows it.
    let minted = crate::domain::cutover::generation_key(key, cutover).ok();
    let staged_copy = shape
        .rows
        .iter()
        .find(|row| {
            row.lifecycle_state == LifecycleState::Draft
                && minted
                    .as_ref()
                    .is_some_and(|target| row.scope_key == *target)
        })
        .cloned();

    // **The copy this act already staged is not a generation standing in its way.**
    // `inst-co-copy` refuses a cutover onto an instant some generation already
    // carries, and after the first attempt the plan carries exactly one such
    // generation: the draft copy that attempt staged. Counting it would make the
    // act's own idempotent replay refuse itself — measured, by the replay case,
    // which failed `DUPLICATE_SCOPE_KEY` before this filter existed. Every *other*
    // generation counts whatever its state: a superseded one still holds its
    // instant, because the key is immutable history rather than a slot that frees.
    let staged_copy_id = staged_copy.as_ref().map(|row| row.price_id);
    let existing_generations: Vec<Cohort> = shape
        .rows
        .iter()
        .filter(|row| Some(row.price_id) != staged_copy_id)
        // Six of ten by hand until D-296: `meter` and `dimension_key` were absent,
        // so on D-103's confirmed plan — several usage lines, one `charge_kind`,
        // one market — the generations of *every* line counted as this line's. A
        // cutover onto an instant that only a neighbouring meter's generation held
        // was refused `DUPLICATE_SCOPE_KEY` on a key that was genuinely free: the
        // fail-closed twin of the site above, and the same class as D-283.
        .filter(|row| {
            key.is_sibling_of(&row.scope_key)
                && row.scope_key.price_eligibility() == PriceEligibility::ExistingGrandfathered
        })
        .map(|row| row.scope_key.cohort())
        .collect();
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

    Ok(CutoverContext {
        plan_id,
        revision: shape.revision,
        lifecycle_state,
        shape,
        predecessor,
        staged_successor,
        staged_copy,
        plane,
        stored_plane,
        existing_generations,
    })
}

#[cfg(test)]
mod cutover_ref_tests {
    use super::{cutover_instant_of_unit_ref, cutover_unit_ref};
    use crate::domain::instant::utc_ymd_hms;
    use crate::domain::scope_key::PlanId;
    use crate::infra::storage::RepoError;

    fn plan() -> PlanId {
        PlanId::new(uuid::Uuid::from_u128(0x9_1a4))
    }

    /// The pair round-trips at the quantum it renders.
    ///
    /// Driven through the encoder rather than a hand-written string: the two
    /// exist to be each other's inverse, and a literal here would be a third
    /// statement of the format free to agree with neither.
    #[test]
    fn what_the_encoder_writes_is_what_the_decoder_reads() {
        let at = utc_ymd_hms(2099, 8, 4, 9, 30, 0);

        let decoded = cutover_instant_of_unit_ref(&cutover_unit_ref(plan(), &[], at))
            .expect("this crate's own ref decodes");

        assert_eq!(decoded, Some(at));
    }

    /// A ref under another head is not this decoder's business.
    #[test]
    fn a_ref_that_is_not_a_cutovers_is_absent_rather_than_corrupt() {
        for other in [
            format!("{}/price_unit/eu", plan().get()),
            format!("{}/supersession/abc/2099-08-04T00:00:00.000Z", plan().get()),
            String::new(),
            "no-slashes-at-all".to_owned(),
        ] {
            assert_eq!(
                cutover_instant_of_unit_ref(&other).expect("another head is not a fault"),
                None,
                "{other}"
            );
        }
    }

    /// A ref **under this head** that will not decode is the table written around.
    ///
    /// `Ok(None)` here would answer `ensure_no_live_cutover` that the plan carries
    /// no live cutover, which is the retirement that guard exists to refuse.
    #[test]
    fn a_ref_under_this_head_that_does_not_decode_is_corrupt() {
        for broken in [
            format!("{}/cutover/", plan().get()),
            format!("{}/cutover/hash", plan().get()),
            format!("{}/cutover/hash/not-an-instant", plan().get()),
            format!(
                "{}/cutover/hash/2099-08-04T00:00:00.000Z/extra",
                plan().get()
            ),
        ] {
            let err = cutover_instant_of_unit_ref(&broken)
                .expect_err("a cutover ref that does not decode is not an absence");
            assert!(matches!(err, RepoError::CorruptRow(_)), "{broken}: {err:?}");
        }
    }
}
