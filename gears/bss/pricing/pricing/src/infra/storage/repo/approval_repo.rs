//! Reads and writes of `pricing_approval` behind the tenant gate
//! (`design/05-governance.md` §6).
//!
//! Three free functions taking a **runner** rather than a provider, for the
//! reason [`audit_repo`](super::audit_repo) and
//! [`catalog_version_ref_repo`](super::catalog_version_ref_repo) take one: an
//! approval record that could commit separately from the act it authorizes is
//! evidence of something that may not have happened. `open` runs inside the
//! transaction that evaluated materiality; `decide` runs inside the transaction
//! that will also write the decision's audit record; and a decision that
//! committed while its trail rolled back would leave `pricing_audit_log`
//! answering "who approved this" with nothing.
//!
//! # `decide` takes an outcome, not a state
//!
//! The parameter is an [`ApprovalDecision`] — approve, reject or withdraw — and
//! not an [`ApprovalState`]. That is a correction. While it took a state,
//! `decide(…, Submitted, …)` was a call anybody could write, and on a *pending*
//! record it answered `APPROVAL_NOT_PENDING` (409) reading "approval X is
//! submitted; only a submitted record is decidable" — a refusal contradicting
//! itself, produced by folding every [`TransitionRefusal`] through one
//! constructor. The domain already carried the type that makes the self-edge
//! unrepresentable and argued from it that
//! [`TransitionRefusal::code`](crate::domain::approval::TransitionRefusal::code)
//! may answer `None` for [`TransitionRefusal::NotAnOutcome`]; the argument was
//! sound and the type had no production caller, so the store was the one place
//! the unreachable refusal was in fact reachable. Now it is not.
//!
//! What that buys the fold below: [`ApprovalState::decide`] can only fail
//! `NotPending`, and `NotPending`'s state is the record's own, so
//! `map_err(|_| not_pending(…, current.state))` states a true sentence rather
//! than a plausible one. `approval_tests::no_decision_is_ever_refused_as_not_an_outcome`
//! is what keeps that true over the whole state x decision product.
//!
//! # `decide` is a compare-and-swap, and the state machine is consulted twice
//!
//! [`ApprovalState::decide`] refuses on the row this call read, so the caller
//! gets the reason rather than a driver error. Then the `UPDATE` carries
//! its **own** `state = 'submitted'` predicate, and matching no row is the same
//! refusal: between the read and the write another decision may have landed, and
//! a repository that trusted its read would let two reviewers both believe they
//! decided one record. `pin_frontier_repo::advance` enforces its forward-only
//! rule the same way and for the same reason.
//!
//! The store holds the rule a third time (`trg_pricing_approval_append_only`).
//! That is not redundancy: the trigger is what an ad-hoc `UPDATE` meets, and
//! this is what a caller meets.
//!
//! # What this module deliberately does not enforce, and what that costs
//!
//! **The two-person rule's surface refusal, and the reject's mandatory reason.**
//! `chk_pricing_approval_distinct_principals` and `chk_pricing_approval_reason`
//! make both unstorable, and that is all that happens *here*. The refusals
//! `inst-tp-distinct` and `inst-as-reject` specify —
//! `SELF_APPROVAL_FORBIDDEN` (403) with its audited attempted-violation record,
//! and `REASON_REQUIRED` — are enforced one layer up, in
//! [`crate::infra::approval::ApprovalService::decide`], over the pure judgement
//! [`authorize_decision`](crate::domain::approval::authorize_decision).
//!
//! That split is deliberate rather than residual. A refusal raised here could
//! not write the `deny` record `inst-tp-selfaudit` binds it to: the record must
//! survive the rolled-back decision transaction, and this function *is* inside
//! it. So the store keeps the physical impossibility and the service keeps the
//! answer, and the two are not free to disagree — the store's CHECK is what a
//! caller that bypassed the service meets.
//!
//! # The four trailing arguments were **not** folded into a typed outcome, and
//! that is a decision
//!
//! The shape considered was `Approved { approver }` / `Rejected { approver,
//! reason }` / `Voided { reason }`, which would make
//! `chk_pricing_approval_reason` and `chk_pricing_approval_approver`
//! *unrepresentable* rather than merely refused. G2 deferred it so one rule
//! would keep one owner; this group owns the rule and did not take it.
//!
//! The reason is not budget. Folding them makes a reason-less reject
//! **unconstructible**, and the only executable evidence that
//! `chk_pricing_approval_reason` still exists is a test that constructs one:
//! `tests/sqlite_approval_repo.rs::a_reject_without_a_reason_is_still_refused_by_the_check_beneath_the_code`.
//! Deleting that test to buy a type-level guarantee would trade a guard that is
//! *proved* for one that is *argued* — and the CHECK would still be there,
//! unexercised, free to be dropped by a later migration with the whole crate
//! green. That is the exact defect class this phase's Postgres track exists to
//! close.
//!
//! What replaced the type-level answer is a surface-level one:
//! [`crate::infra::approval::ApprovalService::decide`] refuses a reason-less
//! reject with `REASON_REQUIRED` before this function is called, so nothing on a
//! wire path reaches the CHECK — and the CHECK is what a caller that went round
//! the service meets. Two lines, both executed.
//!
//! **That argument covered `reason` and it did not cover `approver`, which was
//! the hole.** "Nothing on a wire path reaches the CHECK" was true of
//! `chk_pricing_approval_reason` and false of `chk_pricing_approval_approver`:
//! an approve naming no approver passed every check in
//! [`authorize_decision`](crate::domain::approval::authorize_decision) — the
//! distinctness rule read `None` as *not the submitter* — and landed here, where
//! the CHECK answered with a 500. The two are not symmetric, and the fix is not
//! in this signature: a reason-less reject is a **request a code refuses**, so
//! it must stay constructible, while an approver-less approve is a request §5
//! has no code for, so it must not be constructible at all. The pairing is typed
//! one layer up, on
//! [`DecisionBy`](crate::domain::approval::DecisionBy); the four arguments here
//! stay exactly as they were, and `a_reject_without_a_reason_is_still_refused_by_the_check_beneath_the_code`
//! still constructs one.
//!
//! # The [`AuditStamp`] is threaded now, and the trail is written **here**
//!
//! [`open`] and [`decide`] each take a stamp and each append the record **inside
//! the caller's transaction**, which is what the module's opening paragraph
//! requires of the store — *"a decision that committed while its trail rolled back
//! would leave `pricing_audit_log` answering 'who approved this' with nothing"*.
//! The stamp is worth only the tokens behind it: without `AuditAction`'s `submit`,
//! `approve`, `reject` and `withdraw`, threading one names a record these functions
//! cannot write — a parameter that reads as a guarantee and produces nothing.
//!
//! The record's segment is the **plan's** (D-135, [`subject_plan`]), so an
//! approval's whole life — opened, decided, refused — extends the same chain as
//! the mutations it was about, and "who changed this plan and who signed it off"
//! is one walk.
//!
//! **One thing stays outside this rail.** The refused attempt's `deny` record is
//! written by `crate::infra::approval::record_denial` — not by this module, because
//! the subject of that record is an attempt rather than a unit — but it is written
//! **inside the judgement transaction**, like everything else here, and not in a
//! second one: the judgement returns the refusal as `Ok` and the transaction
//! commits. What stays outside is the machine-driven voids —
//! [`void_pending_for_plan`] and [`void_pending_for_subject`] — write no record
//! at all: no principal acted, the mutation that triggered them has already
//! written its own record on the same segment at the same instant, and
//! `actor_principal_id` is a non-null column precisely so it is never a
//! synthetic. Neither takes a stamp, and neither is a decision.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::instant::rfc3339;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QuerySelect};
use serde_json::{Value as JsonValue, json};
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::paginate_odata;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use bss_pricing_sdk::odata::ApprovalFilterField;

use crate::domain::approval::content_pin::membership_content_hash;
use crate::domain::approval::{ApprovalDecision, ApprovalState};
use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::membership_change::{MembershipMoveProposal, MembershipMoveSet};
use crate::domain::overlay::ScopeValue;
use crate::domain::scope_key::PlanId;
use crate::domain::taxonomy::{
    TaxCategoryPatch, TaxonomyClass, TaxonomyState, TaxonomyValuePatch, TaxonomyValueProposal,
};
use crate::infra::storage::entity::{approval, approval_key};
use crate::infra::storage::odata_mapping::{
    ApprovalODataMapper, LIST_LIMIT_CFG, OdataPageError, domain_page, map_odata_err,
    query_with_default_order,
};
use crate::infra::storage::repo::{NewAuditEntry, audit_repo};
use crate::infra::storage::{RepoError, contention_or_db, policy_guard_or_contention};

/// The subjects this gear can open an approval over — **every member of
/// `AuditSubjectKind::ALL`**.
///
/// At least one writer per kind, and each is named here so a reader can check the
/// claim rather than take it:
///
/// * `plan_revision` — `ApprovalService::submit`, plus the retirement and
///   supersession units that carry the same kind.
/// * `price_unit` — **three**, and the act rides the subject string because S5 §6
///   declares a token for none of them: `ApprovalService::submit_supersession_on`
///   (D-88), `ApprovalService::submit_cutover_on` (D-28) and
///   `ApprovalService::submit_horizon_tightening_on` (D-184). All three mint a
///   plan-prefixed ref; [`subject_aggregate`]'s `price_unit` bullet spells the
///   three shapes.
/// * `window` — `ApprovalService::submit_window_mutation`, the D-62/D-99 window
///   unit `inst-co-single-pending` names in its own enumeration of the units
///   that hold a key.
/// * `policy` — [`crate::infra::approval::open_policy_unit`] on behalf of
///   `infra::threshold::ThresholdService::propose`, D-10's always-material unit
///   over a proposed threshold-policy version.
/// * `overlay` — `ApprovalService::submit_overlay_on` (D-225).
/// * `bulk_operation` — `crate::api::rest::repricing_runs::advance_on_verdict`,
///   the material edge out of `validating` that opens `inst-bs-approval`'s unit.
/// * `membership` — `ApprovalService::submit_membership_move_on`, reached from
///   `crate::api::rest::customer_groups`' move route on the material edge
///   (`inst-mm-immediate` / `inst-mm-bulk`).
///
/// It is stated as a constant rather than left implicit so a later slice
/// widening the store finds the sentence rather than the assumption.
///
/// # This roster is the *approval* plane's, and the distinction is load-bearing
///
/// `AuditSubjectKind` spells two columns — `pricing_audit_log.subject_kind` and
/// `pricing_approval.subject_kind` — and D-158 requires the two stores to
/// declare the same enumeration. A kind reaches the audit plane first every
/// time: the mutation is audited on the day it is built, while the unit that
/// asks two people to approve it is a separate task. So "storable here" (D-158)
/// and "written here" (this roster) are different properties, and this constant
/// is only ever the second.
///
/// # It has now gone stale three times, and this is what stopped it
///
/// `price_unit` (D-88), then `overlay` (D-225, caught by review rather than by any
/// gate), then `membership` — whose writer landed
/// with Task 7 of the customer-group plane while this doc went on
/// asserting that the plane had no writer and that
/// `infra::approval::re_derive` and [`subject_aggregate`] "both refuse this kind
/// outright". Both had been un-refused in the same wave: `subject_aggregate`
/// resolves a membership move into [`SubjectAggregate::Payer`] or
/// [`SubjectAggregate::BulkOperation`], and `re_derive` decodes the pinned
/// payload back out of the `subject_ref`.
///
/// Each time, the test guarding the roster compared it against a hard-coded copy
/// of itself, so both operands moved together and the drift was invisible to it.
/// `approval_repo_tests::the_roster_is_exactly_the_kinds_production_opens_a_unit_of`
/// no longer does that: it reads this crate's own sources for every production
/// construction of [`NewApproval`] and asserts the roster is exactly the set of
/// kinds they name. That operand moves when a **writer** moves, which is the
/// only thing this constant claims to describe.
pub const SUBJECT_KINDS_WITH_A_WRITER: &[AuditSubjectKind] = &[
    AuditSubjectKind::PlanRevision,
    AuditSubjectKind::PriceUnit,
    AuditSubjectKind::Window,
    AuditSubjectKind::Policy,
    AuditSubjectKind::Overlay,
    AuditSubjectKind::BulkOperation,
    AuditSubjectKind::Membership,
    // D-353: `ApprovalService::submit_taxonomy_value_on`.
    AuditSubjectKind::TaxonomyValue,
];

/// A record to open — the pending half of `pricing_approval`.
///
/// Carries no `state` and no `decided_at`: a record is opened `submitted` or it
/// is not opened, and `chk_pricing_approval_decided_at` makes those two the same
/// fact. Letting a caller name the state here would let a decided record be
/// written with no decision ever having been made.
///
/// **It carries no `submitter_principal` and no `submitted_at` either, and that
/// is the [`AuditStamp`] arriving rather than two fields being dropped.** The
/// submitter *is* the actor of the open and the submission instant *is* the
/// record's instant, so [`open`] takes them from the stamp it now has to take
/// anyway. Two spellings of one fact are two chances for the approval row and its
/// audit record to disagree about who submitted a change unit — on the pair of
/// stores whose whole job is to agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewApproval {
    /// The record's durable name, minted by the caller so the audit record of
    /// the same act can reference it.
    pub approval_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// The pinned subject — `<plan_id>/<revision>` for a plan revision, as
    /// [`audit_repo::plan_revision_ref`](super::audit_repo::plan_revision_ref)
    /// renders it.
    pub subject_ref: String,
    /// What kind of thing the subject is. Typed against the **audit** store's
    /// enumeration, which D-158 requires this store to spell identically.
    pub subject_kind: AuditSubjectKind,
    /// The digest of the content pinned at submission (`inst-ap-pin`).
    pub content_hash: Vec<u8>,
    /// The materiality evaluator's output.
    pub materiality: JsonValue,
    /// The canonical scope keys this unit **holds** while it is `submitted`
    /// (`inst-co-single-pending`).
    ///
    /// A **set**, so that a plan carrying two rows on one key holds it once; a
    /// `Vec` would make the register's primary key the deduplicator and turn an
    /// ordinary shape into a spurious conflict with the unit's own insert.
    ///
    /// **The canonical rendering, not `ScopeKey`** — ten axes since D-196, which is
    /// what `ScopeKey`'s `Display` writes. That is the register's
    /// own column, the string a publish refusal names a key by, and the only
    /// ordering over keys this gear has ever meant — `ScopeKey` deliberately derives
    /// no `Ord`, its axis order being a *rendering* order rather than a comparison
    /// one, and inventing a comparison on a validated domain type to hold a set the
    /// store keeps as text would put a second canonical form in the crate.
    ///
    /// It may legitimately be **empty** — a plan revision with no price row on it
    /// touches no key — and an empty set holds nothing rather than everything. That
    /// is why the per-subject pendingness check in [`find_pending_for_subject`]
    /// stays: the key register cannot answer "two reviewers are deciding two
    /// records over one revision" on a plan whose key set is empty, and that is a
    /// different invariant with the same code.
    pub held_keys: BTreeSet<String>,
}

/// One approval record, read back into the vocabulary the domain uses.
///
/// `state` and `subject_kind` arrive as their typed forms, so a token the store
/// admits and this crate does not is a [`RepoError::CorruptRow`] at the boundary
/// rather than a string carried into a handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRecord {
    /// The record's durable name.
    pub approval_id: Uuid,
    /// RLS scope.
    pub tenant_id: Uuid,
    /// The pinned subject.
    pub subject_ref: String,
    /// What kind of thing the subject is.
    pub subject_kind: AuditSubjectKind,
    /// The digest pinned at submission.
    pub content_hash: Vec<u8>,
    /// Where the record stands in §4's machine.
    pub state: ApprovalState,
    /// Who opened it.
    pub submitter_principal: Uuid,
    /// Who decided it; `None` while pending, and `None` on a void.
    pub approver_principal: Option<Uuid>,
    /// Mandatory on a reject.
    pub reason: Option<String>,
    /// The materiality evaluator's output.
    pub materiality: JsonValue,
    /// When it was opened, UTC.
    pub submitted_at: OffsetDateTime,
    /// When it was decided, UTC; `None` exactly while pending.
    pub decided_at: Option<OffsetDateTime>,
}

/// Open a pending approval record, **and its `submit` record**.
///
/// Both writes are the caller's transaction's, which is what the module doc's
/// opening paragraph asks of every function here: an approval record that could
/// commit separately from the trail of its own opening is evidence of something
/// that may not have happened.
///
/// The submitter and the submission instant come off `stamp`, not off a second
/// pair of fields; [`NewApproval`] says why.
///
/// # Errors
/// [`RepoError::ConcurrentMutation`] when the id is already taken — a loser on a
/// caller-minted primary key is told to retry rather than told the store failed
/// (D-159); and when the audit append loses a same-segment race, which rolls this
/// insert back with it. [`RepoError::PendingPolicyUnitHeld`] when a **policy** unit
/// finds the tenant's one open-proposal slot taken (D-192 clause (2)) — the primary
/// key is no longer this table's only serialization point, which is why the two are
/// told apart here rather than folded into one conflict.
/// [`RepoError::PendingKeyHeld`] when one of `held_keys` is held by another
/// `submitted` unit. [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] on a `subject_ref` no writer in this crate could have
/// produced.
pub async fn open(
    runner: &impl DBRunner,
    scope: &AccessScope,
    new: NewApproval,
    stamp: AuditStamp,
) -> Result<ApprovalRecord, RepoError> {
    let am = approval::ActiveModel {
        approval_id: Set(new.approval_id),
        tenant_id: Set(new.tenant_id),
        subject_ref: Set(new.subject_ref.clone()),
        subject_kind: Set(new.subject_kind.as_str().to_owned()),
        content_hash: Set(new.content_hash.clone()),
        state: Set(ApprovalState::Submitted.as_str().to_owned()),
        submitter_principal: Set(stamp.actor_principal_id),
        approver_principal: Set(None),
        reason: Set(None),
        materiality: Set(new.materiality.clone()),
        submitted_at: Set(stamp.recorded_at),
        decided_at: Set(None),
    };
    approval::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)
        .map_err(|e| RepoError::Db(format!("pricing_approval scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| {
            // **Two unique indexes stand on this table, and they want different
            // answers.** The primary key is the caller-minted `approval_id` and its
            // loser is told to retry; `uq_pricing_approval_policy_pending` is D-192's
            // mint guard and its loser is told to decide or withdraw the proposal the
            // tenant already has. `policy_guard_or_contention` is the only place that
            // can tell them apart, and it explains what it costs to do so.
            //
            // The subject-kind conjunct is here rather than inside it because it is a
            // fact about *this* write and not about the driver's message: the guard's
            // predicate is `subject_kind = 'policy'`, so no other kind can have
            // violated it, and a plan-revision or window unit reaching that branch
            // would be a misreading rather than a refusal.
            if new.subject_kind == AuditSubjectKind::Policy {
                policy_guard_or_contention(
                    &e,
                    new.tenant_id,
                    &format!("approval {}", new.approval_id),
                    "insert pricing_approval",
                )
            } else {
                contention_or_db(
                    &e,
                    &format!("approval {}", new.approval_id),
                    "insert pricing_approval",
                )
            }
        })?;

    // The keys this unit holds, in the same transaction as the unit. The order is
    // parent-then-register and not the reverse: a register row naming an approval
    // that does not exist would be a held key with no unit to decide, which is the
    // one state that cannot be got out of.
    for key in &new.held_keys {
        let row = approval_key::ActiveModel {
            approval_id: Set(new.approval_id),
            scope_key: Set(key.clone()),
            tenant_id: Set(new.tenant_id),
            state: Set(ApprovalState::Submitted.as_str().to_owned()),
        };
        approval_key::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .map_err(|e| RepoError::Db(format!("pricing_approval_key scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| {
                // Any unique violation reaching here **is**
                // `uq_pricing_approval_key_pending`. The composite primary key
                // cannot produce one: `held_keys` is a set, so this loop offers each
                // key once, and the parent insert above already refused a second
                // unit under one `approval_id`.
                if e.is_unique_violation() {
                    RepoError::PendingKeyHeld { key: key.clone() }
                } else {
                    RepoError::Db(format!(
                        "hold scope key {key} for approval {}: {e}",
                        new.approval_id
                    ))
                }
            })?;
    }

    let record = ApprovalRecord {
        approval_id: new.approval_id,
        tenant_id: new.tenant_id,
        subject_ref: new.subject_ref,
        subject_kind: new.subject_kind,
        content_hash: new.content_hash,
        state: ApprovalState::Submitted,
        submitter_principal: stamp.actor_principal_id,
        approver_principal: None,
        reason: None,
        materiality: new.materiality,
        submitted_at: stamp.recorded_at,
        decided_at: None,
    };
    append_trail(
        runner,
        scope,
        &record,
        AuditAction::Submit,
        // Nothing stood here: no unit held this subject before this insert, and
        // an absence is the only honest rendering of that — the same reading
        // `create` has one plane over.
        None,
        stamp,
    )
    .await?;
    Ok(record)
}

/// Read one record, scoped.
///
/// `None` means the record does not exist **or** lies outside the caller's
/// scope, deliberately the same answer either way: a distinguishable refusal
/// would confirm the existence of another tenant's approval unit, and what a
/// pending unit tells an observer is that a price change is in flight.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a stored token is outside the enumeration its CHECK admits.
pub async fn read(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    approval_id: Uuid,
) -> Result<Option<ApprovalRecord>, RepoError> {
    let row = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::ApprovalId.eq(approval_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_approval: {e}")))?;
    row.map(to_domain).transpose()
}

/// One page of the tenant's approval records, in `approval_id` order.
///
/// The reviewer's queue (`GET /bss-pricing/v1/approvals`, §5). Ordered by the
/// primary key and resumed strictly after `after`, which is D-125's keyset walk
/// and not an offset — `pricing_approval` is append-only over a ≥ 7-year
/// retention, so `OFFSET n` names a different row every time a unit opens ahead
/// of it (`crate::api::rest::cursor`'s module doc argues it once for every list
/// surface).
///
/// `states` narrows the walk. An **empty** slice is the whole set rather than
/// nothing: a caller that named no filter asked for everything, and answering an
/// empty page to that would be a filter nobody applied.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a stored token is outside the enumeration its CHECK admits.
pub async fn list_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    states: &[ApprovalState],
    after: Option<Uuid>,
    limit: u64,
) -> Result<Vec<ApprovalRecord>, RepoError> {
    let mut filter = Condition::all().add(approval::Column::TenantId.eq(tenant_id));
    if !states.is_empty() {
        let tokens: Vec<&str> = states.iter().map(|state| state.as_str()).collect();
        filter = filter.add(approval::Column::State.is_in(tokens));
    }
    if let Some(after) = after {
        filter = filter.add(approval::Column::ApprovalId.gt(after));
    }
    approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(approval::Column::ApprovalId, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pricing_approval: {e}")))?
        .into_iter()
        .map(to_domain)
        .collect()
}

/// State totals for the whole authorized approval set, independent of pagination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApprovalCounts {
    /// Sum of all four states.
    pub total: u64,
    /// Pending review.
    pub submitted: u64,
    /// Approved units.
    pub approved: u64,
    /// Rejected units.
    pub rejected: u64,
    /// Withdrawn or automatically voided units.
    pub voided: u64,
}

/// Count by state in one database aggregate under the original approval scope.
///
/// # Errors
/// Storage/scope failures and corrupt persisted states or counts fail the read.
pub async fn counts(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<ApprovalCounts, RepoError> {
    #[derive(sea_orm::FromQueryResult)]
    struct Row {
        state: String,
        count: i64,
    }
    let rows = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(approval::Column::TenantId.eq(tenant_id)))
        .project_all(runner, |q| {
            q.select_only()
                .column(approval::Column::State)
                .column_as(approval::Column::ApprovalId.count(), "count")
                .group_by(approval::Column::State)
                .into_model::<Row>()
        })
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_approval: {e}")))?;
    let mut counts = ApprovalCounts::default();
    for row in rows {
        let count = u64::try_from(row.count)
            .map_err(|_| RepoError::CorruptRow("negative approval count".to_owned()))?;
        match super::plan_repo::read_token(
            "pricing_approval.state",
            &row.state,
            ApprovalState::ALL,
            ApprovalState::as_str,
        )? {
            ApprovalState::Submitted => counts.submitted = count,
            ApprovalState::Approved => counts.approved = count,
            ApprovalState::Rejected => counts.rejected = count,
            ApprovalState::Voided => counts.voided = count,
        }
        counts.total += count;
    }
    Ok(counts)
}

/// One `OData` page of the tenant's approval records. Default order is
/// `approval_id asc`.
///
/// # Errors
/// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
/// malformed `$filter` / `$orderby` / cursor.
pub async fn list_odata(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<ApprovalRecord>, OdataPageError> {
    let base_select = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(approval::Column::TenantId.eq(tenant_id)));
    let query = query_with_default_order(query, &[ApprovalFilterField::ApprovalId]);
    let page = paginate_odata::<
        ApprovalFilterField,
        ApprovalODataMapper,
        approval::Entity,
        approval::Model,
        _,
        _,
    >(
        base_select,
        runner,
        &query,
        ("approval_id", SortDir::Asc),
        LIST_LIMIT_CFG,
        |m| m,
    )
    .await
    .map_err(map_odata_err)?;
    domain_page(page, to_domain)
}

/// Pending units belonging directly to the authorized page of plans.
///
/// One read of submitted plan/revision, window and price-unit subjects; indirect
/// tenant policy and overlay effects are not plan-owned units. Resolve ownership
/// with [`subject_aggregate`], the same parser used by the mutation/audit path.
/// Empty pages do not query. Each plan's units are ordered oldest first, then by
/// approval id, so equal timestamps do not reorder badges between reads.
///
/// # Errors
/// [`RepoError::Db`] on storage failure; [`RepoError::CorruptRow`] on invalid
/// persisted subjects. A failure must not appear as an empty pending list.
pub async fn pending_for_plans(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_ids: &[PlanId],
) -> Result<BTreeMap<PlanId, Vec<ApprovalRecord>>, RepoError> {
    let requested: BTreeSet<_> = plan_ids.iter().copied().collect();
    let mut pending = BTreeMap::<PlanId, Vec<ApprovalRecord>>::new();
    if requested.is_empty() {
        return Ok(pending);
    }
    for (plan_id, units) in pending_by_plan(runner, scope, tenant_id, &requested).await? {
        // The predicate is a `subject_ref` prefix, so a stored ref whose head is
        // not one of the requested ids cannot match — but the parse still decides
        // ownership, and a `LIKE` is not the parser. Kept as the authority.
        if requested.contains(&plan_id) {
            pending.insert(plan_id, units);
        }
    }
    Ok(pending)
}

/// Every plan the tenant holds an open unit over, bounded by the open units.
///
/// The `has_pending_approvals` filter's operand. It is deliberately **not**
/// authorized here — the caller intersects it with a scoped plan read, which is
/// what [`pending_by_plan`]'s own note means by "never expose this tenant
/// projection directly". Reading it first is the point: the filter used to start
/// from the tenant's whole catalogue of draft, published and retired plan ids so
/// that it could ask about all of them, which is a catalogue-sized read to answer
/// a question whose answer is bounded by how many reviews are open.
///
/// # Errors
/// [`RepoError::Db`] on storage failure; [`RepoError::CorruptRow`] on a stored
/// subject this crate cannot have written.
pub async fn plan_ids_with_open_units(
    runner: &impl DBRunner,
    tenant_id: Uuid,
) -> Result<Vec<PlanId>, RepoError> {
    let rows = approval::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectKind.is_in([
                    AuditSubjectKind::PlanRevision.as_str(),
                    AuditSubjectKind::Window.as_str(),
                    AuditSubjectKind::PriceUnit.as_str(),
                ])),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read open plan units: {e}")))?;
    let mut ids = BTreeSet::new();
    for row in rows {
        let unit = to_domain(row)?;
        if let SubjectAggregate::Plan(plan_id) = subject_aggregate(&unit)? {
            ids.insert(plan_id);
        }
    }
    Ok(ids.into_iter().collect())
}

/// Internal pending join index. Both plan badges and the computed plan filter
/// use this parser, so they cannot disagree about which subject belongs to a plan.
/// Never expose this tenant projection directly: intersect with scoped plan reads.
///
/// # Errors
/// Scope/storage failures or malformed persisted subjects fail the read.
async fn pending_by_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    requested: &BTreeSet<PlanId>,
) -> Result<BTreeMap<PlanId, Vec<ApprovalRecord>>, RepoError> {
    let mut pending = BTreeMap::<PlanId, Vec<ApprovalRecord>>::new();
    // **The requested plans are a predicate, not a post-filter.**
    //
    // All three kinds this reads spell their subject `<plan_id>/<rest>` —
    // `subject_plan` is the parser and it takes the head before the first `/` for
    // every one of them — so the plan the caller asked about is expressible in
    // SQL. Without it the read was the tenant's whole open-review set on every
    // call, and `GET /plans/{planId}` asking about one plan paid for all of it;
    // the ceiling was how many reviews the tenant happens to have open, which is
    // not a bound the request can see.
    let mut owned = Condition::any();
    for plan_id in requested {
        owned = owned.add(approval::Column::SubjectRef.starts_with(format!("{}/", plan_id.get())));
    }
    let rows = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectKind.is_in([
                    AuditSubjectKind::PlanRevision.as_str(),
                    AuditSubjectKind::Window.as_str(),
                    AuditSubjectKind::PriceUnit.as_str(),
                ]))
                .add(owned),
        )
        .order_by(approval::Column::SubmittedAt, Order::Asc)
        .order_by(approval::Column::ApprovalId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pending plan units: {e}")))?;
    for row in rows {
        let unit = to_domain(row)?;
        if let SubjectAggregate::Plan(plan_id) = subject_aggregate(&unit)? {
            pending.entry(plan_id).or_default().push(unit);
        }
    }
    Ok(pending)
}

/// A pending taxonomy unit and its parsed proposal, from the same stored row.
#[derive(Debug, Clone)]
pub struct PendingTaxonomyUnit {
    /// Internal approval data; a config reader may see only pending metadata.
    pub record: ApprovalRecord,
    /// The existing subject payload, not a second stored draft.
    pub proposal: TaxonomyValueProposal,
}

/// Submitted proposals for exactly the authorized taxonomy values being read.
///
/// One query, ordered oldest first then by id. The caller first reads the
/// values under `config × read`, then passes that scope's tenant projection:
/// taxonomy resource ids are tenant ids, not approval ids. This metadata join
/// does NOT authorize exposing proposal content; use [`visible_ids`] under the
/// original `approval × read` scope before doing so. Empty value sets do not query.
///
/// # Errors
/// Storage failures and malformed persisted proposals fail the read, never
/// masquerade as an empty pending array. Corruption diagnostics omit content.
pub async fn pending_for_taxonomy_values(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    values: &[String],
) -> Result<BTreeMap<String, Vec<PendingTaxonomyUnit>>, RepoError> {
    let requested: BTreeSet<_> = values.iter().map(String::as_str).collect();
    let mut pending = BTreeMap::<String, Vec<PendingTaxonomyUnit>>::new();
    if requested.is_empty() {
        return Ok(pending);
    }
    let rows = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectKind.eq(AuditSubjectKind::TaxonomyValue.as_str())),
        )
        .order_by(approval::Column::SubmittedAt, Order::Asc)
        .order_by(approval::Column::ApprovalId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pending taxonomy units: {e}")))?;
    for row in rows {
        let record = to_domain(row)?;
        // **The re-wording is a redaction, not lost context.** The inner
        // `CorruptRow` prints the `subject_ref`, and that ref *is* the stored
        // proposal — class, value and patch — so propagating it puts the content
        // of a pending change into an error a config reader receives. This path
        // exists to hand back pending *metadata* and not content, which is what
        // `sqlite_approval_repo::a_malformed_pending_taxonomy_subject_fails_without_exposing_content`
        // pins. The diagnosis lives in the log line below instead.
        let proposal = subject_taxonomy_value(&record).map_err(|e| {
            tracing::warn!(
                approval_id = %record.approval_id,
                error = %e,
                "bss-pricing: stored taxonomy proposal does not decode"
            );
            RepoError::CorruptRow(format!(
                "approval {} has an invalid taxonomy proposal",
                record.approval_id
            ))
        })?;
        if proposal.class == class && requested.contains(proposal.value.as_str()) {
            pending
                .entry(proposal.value.as_str().to_owned())
                .or_default()
                .push(PendingTaxonomyUnit { record, proposal });
        }
    }
    Ok(pending)
}

/// Which candidate approval ids the caller's full approval-read scope exposes.
///
/// A batch intersection, including resource-id predicates, not a tenant-only
/// permission check. Empty input does not query. No content is returned here.
///
/// # Errors
/// [`RepoError::Db`] on storage failure.
pub async fn visible_ids(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    approval_ids: &[Uuid],
) -> Result<BTreeSet<Uuid>, RepoError> {
    if approval_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let rows = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::ApprovalId.is_in(approval_ids.iter().copied())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read visible approval ids: {e}")))?;
    Ok(rows.into_iter().map(|row| row.approval_id).collect())
}

/// The **pending** unit pinned to a revision of `plan_id`, if the plan holds one.
///
/// One half of what `PENDING_CHANGE_UNIT_EXISTS` reads
/// ([`07-pricewindow-linkage.md`](../../../../../docs/design/07-pricewindow-linkage.md)
/// `inst-co-single-pending`), and **no longer the whole of it**: the rule is about
/// the canonical scope keys a unit holds, which [`find_pending_key_holder`]
/// answers off the register. This one answers the narrower question the register
/// cannot — *is somebody already reviewing a revision of this plan* — and it is
/// kept for two reasons rather than as residue.
///
/// **A plan revision with no price row on it holds no key at all.** Its register
/// contribution is empty, so two units over one revision would both open and two
/// reviewers would decide two records whose order of arrival then decides what
/// publishes. That is the invariant `ApprovalService::submit`'s doc states, it is
/// not the per-key one, and it is invisible to a key register by construction.
///
/// **And it is what lets the widening be a superset.** Every submit this prefix
/// refused before the register existed is still refused, so nothing was traded
/// away for the per-key rule — which is the property to check first when a check is
/// replaced by a different check with the same code.
///
/// Matched by the `<plan_id>/` prefix, for [`void_pending_for_plan`]'s reason
/// and with its trailing slash: D-146 gives a plan one open draft revision, and a
/// pending unit is pinned to whichever revision that is. It is therefore scoped to
/// `plan_revision` subjects and does **not** see a window unit of the same plan —
/// correctly, since a window unit reviews no revision; the key register is what
/// sees that one.
///
/// The `submitted` filter is the whole predicate — a decided or voided unit
/// holds nothing, which is exactly what makes the withdraw of `inst-as-void` an
/// escape from the pin rather than a second way to spell it.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on
/// a token outside its enumeration.
pub async fn find_pending_for_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<ApprovalRecord>, RepoError> {
    let row = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectKind.eq(AuditSubjectKind::PlanRevision.as_str()))
                .add(approval::Column::SubjectRef.starts_with(format!("{plan_id}/"))),
        )
        .order_by(approval::Column::SubmittedAt, Order::Asc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the pending unit of plan {plan_id}: {e}")))?;
    row.map(to_domain).transpose()
}

/// Every unit of `plan_id` in one of `states` whose subject is a **cutover**.
///
/// A prefix read like [`find_pending_for_plan`]'s, and deliberately not that
/// function with a widened state filter: this one answers about a subject kind
/// that is not `plan_revision`, so it cannot share that prefix, and its callers
/// need units that are **decided** — `find_pending_for_plan`'s `submitted`-only
/// predicate is exactly what they must not have. A cutover an approver already
/// approved is the one whose schedules stand.
///
/// The prefix is [`crate::infra::cutover::cutover_unit_ref_prefix`]'s rather than
/// a literal here, so the scan and the renderer move together.
///
/// **The kind filter is `price_unit` and is not redundant with the prefix.** S5 §6
/// declares no `cutover` member, so the act rides the subject string
/// ([`subject_aggregate`]); the filter is what stops a later kind minting a
/// `<plan>/cutover/…` ref of its own meaning from being read as one of these.
///
/// Ordered by `submitted_at` so a refusal naming "the unit" names the same one on
/// every run: a message whose content depends on the storage engine's row order is
/// one an operator cannot reproduce.
///
/// An empty `states` reads every state, [`list_page`]'s convention.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on
/// a token outside its enumeration.
pub async fn cutover_units_of_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    states: &[ApprovalState],
) -> Result<Vec<ApprovalRecord>, RepoError> {
    let mut filter = Condition::all()
        .add(approval::Column::TenantId.eq(tenant_id))
        .add(approval::Column::SubjectKind.eq(AuditSubjectKind::PriceUnit.as_str()))
        .add(
            approval::Column::SubjectRef
                .starts_with(crate::infra::cutover::cutover_unit_ref_prefix(plan_id)),
        );
    if !states.is_empty() {
        let tokens: Vec<&str> = states.iter().map(|state| state.as_str()).collect();
        filter = filter.add(approval::Column::State.is_in(tokens));
    }
    approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(approval::Column::SubmittedAt, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the cutover units of plan {plan_id}: {e}")))?
        .into_iter()
        .map(to_domain)
        .collect()
}

/// The **pending** unit pinned to exactly `subject_ref`, if there is one.
///
/// The per-subject half of `inst-co-single-pending` for a subject that is not a
/// plan revision: a second window unit over one window is a second reviewer
/// deciding one change, whatever key it holds. `find_pending_for_plan` cannot answer
/// it — a window ref carries no plan prefix.
///
/// **What it buys is the message and not the refusal, and the doc used to overstate
/// that.** It said the key register "cannot answer it either", which is false for the
/// only subject kind this has a caller for: a window unit holds exactly **one** key —
/// its own — so a second unit over the same window contends on that key and
/// [`find_pending_key_holder`] refuses it too. What this check adds is a 409 that names
/// the *window*, where the register's names a scope key: an operator told "another unit
/// holds `<plan>|EUR|eu|…`" about their own second submit over one window has to work
/// out that the two are the same review. The overstatement would become true for a
/// subject kind that holds no key — and D-353's taxonomy-value unit is one: a taxonomy
/// value is not a scope key, so [`find_pending_key_holder`] cannot contend over it, and
/// this check is that kind's refusal outright rather than a better message for one the
/// register already made.
///
/// # Why the operand is a tenant scope and not the caller's
///
/// `pricing_approval` declares `resource_col = "approval_id"`, which no caller's plan
/// or config grant can pin — so a resource-constrained scope matches nothing here and
/// the check answers `None`, admitting the second unit it exists to refuse. For the
/// taxonomy-value kind, which has no register behind it, that is the whole control.
/// Pendingness is a fact about the tenant's records rather than about who is asking, so
/// the operand is derived from `tenant_id`; `tenant_only()` would not do, being
/// deny-all on an unconstrained scope and reading `None` the same way.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on
/// a token outside its enumeration.
pub async fn find_pending_for_subject(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    subject_ref: &str,
) -> Result<Option<ApprovalRecord>, RepoError> {
    let scope = &AccessScope::for_tenant(tenant_id);
    let row = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectRef.eq(subject_ref)),
        )
        .order_by(approval::Column::SubmittedAt, Order::Asc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the pending unit over {subject_ref}: {e}")))?;
    row.map(to_domain).transpose()
}

/// The pending unit holding any of `keys`, and **which** key it holds.
///
/// `inst-co-single-pending` proper: *"at most one pending approval unit of any
/// kind may hold a canonical scope key"*, which §5 glosses on the wire as *"a
/// pending unit already holds one of the touched keys"*. Both sentences are about
/// a set, and this is the read that answers them.
///
/// **Of any kind.** There is no `subject_kind` filter and there must not be one:
/// the rule's whole content is that a window unit and a plan-revision unit
/// touching one key contend, and a filter here is exactly the hole the C-3 fix
/// note records — two always-material units over one key both approvable, the final
/// state decided by commit order.
///
/// Ordered by `scope_key` so that a unit touching several held keys names the same
/// one on every run: a refusal whose message depends on the storage engine's row
/// order is a refusal an operator cannot reproduce.
///
/// An empty `keys` answers `None` **without a query**, which is the honest reading
/// rather than an optimisation: a unit that touches no key can conflict with
/// nothing, and an `IN ()` predicate is a dialect question.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on
/// a token outside its enumeration.
pub async fn find_pending_key_holder(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    keys: &BTreeSet<String>,
) -> Result<Option<(ApprovalRecord, String)>, RepoError> {
    if keys.is_empty() {
        return Ok(None);
    }
    Ok(find_pending_key_holders(runner, scope, tenant_id, keys)
        .await?
        .into_iter()
        .next())
}

/// **Every** pending holder among `keys`, in the same canonical order.
///
/// [`find_pending_key_holder`]'s plural, and the one implementation both use —
/// that function is now its first element. The singular answers a *submit*, which
/// stops at the first conflict because one is enough to refuse; the plural answers
/// the bulk import's Phase 1, which owes a verdict on **every** row and cannot
/// stop at the first (`inst-bk-phase1`: one invalid row blocks the batch, and the
/// report enumerates every violation).
///
/// One query for the register and one `read` per **distinct** unit, not per key: a
/// unit holding forty of the batch's keys is one read.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on a
/// register row whose unit has vanished — impossible, `DELETE` being refused on
/// both tables, which is why it is a corrupt store rather than an empty answer.
pub async fn find_pending_key_holders(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    keys: &BTreeSet<String>,
) -> Result<Vec<(ApprovalRecord, String)>, RepoError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rendered: Vec<String> = keys.iter().cloned().collect();
    let held = approval_key::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval_key::Column::TenantId.eq(tenant_id))
                .add(approval_key::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval_key::Column::ScopeKey.is_in(rendered)),
        )
        .order_by(approval_key::Column::ScopeKey, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the pending key register: {e}")))?;

    let mut units: BTreeMap<Uuid, ApprovalRecord> = BTreeMap::new();
    let mut holders = Vec::with_capacity(held.len());
    for row in held {
        let record = match units.entry(row.approval_id) {
            std::collections::btree_map::Entry::Occupied(found) => found.get().clone(),
            std::collections::btree_map::Entry::Vacant(slot) => {
                let Some(record) = read(runner, scope, tenant_id, row.approval_id).await? else {
                    return Err(RepoError::CorruptRow(format!(
                        "pricing_approval_key: scope key {} is held by approval {}, which does \
                         not exist",
                        row.scope_key, row.approval_id
                    )));
                };
                slot.insert(record).clone()
            }
        };
        holders.push((record, row.scope_key));
    }
    Ok(holders)
}

/// Every canonical scope key `approval_id` holds, in canonical order.
///
/// The register's own reading surface. It exists because the register is otherwise
/// only ever consulted by a refusal, and a test that asserts the refusal alone
/// cannot tell a unit that held the right keys from one that held every key of the
/// tenant — the observability twin of `find_pending_key_holder`.
///
/// Rendered keys and not `ScopeKey`s: this reads the register's own column, and
/// re-parsing ten axes out of it to hand back a type the caller renders again
/// would put a second parser on the one string the store keeps.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn held_keys_of(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    approval_id: Uuid,
) -> Result<Vec<String>, RepoError> {
    Ok(approval_key::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval_key::Column::TenantId.eq(tenant_id))
                .add(approval_key::Column::ApprovalId.eq(approval_id)),
        )
        .order_by(approval_key::Column::ScopeKey, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the keys approval {approval_id} holds: {e}")))?
        .into_iter()
        .map(|row| row.scope_key)
        .collect())
}

/// Which of `approval_id`'s register rows are still **holding** their key.
///
/// The register follows its unit out of `submitted` through
/// `trg_pricing_approval_key_follow_state`, so this is how a test asserts that a
/// decided unit **freed** what it held rather than merely that the parent flipped.
/// A sync that silently stopped happening would leave every key held forever, and
/// nothing about the parent row would look wrong.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn held_keys_still_pending(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    approval_id: Uuid,
) -> Result<Vec<String>, RepoError> {
    Ok(approval_key::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval_key::Column::TenantId.eq(tenant_id))
                .add(approval_key::Column::ApprovalId.eq(approval_id))
                .add(approval_key::Column::State.eq(ApprovalState::Submitted.as_str())),
        )
        .order_by(approval_key::Column::ScopeKey, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the keys approval {approval_id} holds: {e}")))?
        .into_iter()
        .map(|row| row.scope_key)
        .collect())
}

/// The **approved** unit over exactly `subject_ref` **and exactly**
/// `content_hash`, if there is one.
///
/// The publish route's question, and both halves of the predicate are
/// load-bearing.
///
/// **The subject is matched exactly**, never by the plan prefix: an approval
/// names `<plan_id>/<revision>`, and a decision taken over revision *N*
/// authorizes the publish of revision *N* and of nothing else. A prefix match
/// would let a stale approval of a superseded revision authorize the freeze of a
/// revision no second person ever read.
///
/// **The content is matched too**, and that is what makes an approval a decision
/// about a change set rather than a standing permission over a revision. Without
/// it the route finds an approval whose subject moved after the decision —
/// `inst-ap-pin` voids only `submitted` records, so an approved one survives the
/// mutation — tries to commit, and is refused by the pin **forever**: the plan
/// could never publish again, because a price row's `row_version` is inside the
/// digest, so even restoring the old values does not restore the old pin. With
/// it, an approval that no longer covers the content simply does not answer and
/// the route opens a fresh unit for a fresh decision. That is the same security
/// property — nothing freezes that a second person did not see — reached through
/// a state an operator can act on.
///
/// Ordered by `decided_at` so that where a plan somehow holds two approved units
/// over one revision **and** one content, the first answers deterministically
/// rather than whichever the storage engine returns.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on
/// a token outside its enumeration.
pub async fn find_approved_for_content(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    subject_ref: &str,
    content_hash: &[u8],
) -> Result<Option<ApprovalRecord>, RepoError> {
    let row = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Approved.as_str()))
                .add(approval::Column::SubjectRef.eq(subject_ref))
                .add(approval::Column::ContentHash.eq(content_hash.to_vec())),
        )
        .order_by(approval::Column::DecidedAt, Order::Asc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the approved unit of {subject_ref}: {e}")))?;
    row.map(to_domain).transpose()
}

/// Void every **pending** unit pinned to exactly `subject_ref`, carrying
/// `reason`.
///
/// The narrow sibling of [`void_pending_for_plan`], and the narrowness is the
/// point. That one answers "the plan's content moved", which is a fact about
/// every revision of the plan. This one answers "**this revision** was frozen
/// under another decision", which is a fact about one subject — so a later
/// slice's unit over a different revision, or over a window or an overlay of the
/// same plan, is not swept up by a publish that had nothing to do with it.
///
/// Everything else is [`void_pending_for_plan`]'s: one `UPDATE` with nothing to
/// judge, `approver_principal` left NULL because a machine-driven void has no
/// human decider, and the three columns the append-only trigger's whitelist
/// admits.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure. It runs inside its caller's
/// transaction, so a failure rolls that caller back.
pub async fn void_pending_for_subject(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    subject_ref: &str,
    reason: &str,
    voided_at: OffsetDateTime,
) -> Result<u64, RepoError> {
    let result = approval::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            approval::Column::State,
            Expr::value(ApprovalState::Voided.as_str()),
        )
        .col_expr(approval::Column::DecidedAt, Expr::value(voided_at))
        .col_expr(approval::Column::Reason, Expr::value(reason))
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectRef.eq(subject_ref)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("void pending approvals of {subject_ref}: {e}")))?;
    Ok(result.rows_affected)
}

/// Decide a pending record — the one flip §4 sanctions — **and record it**.
///
/// The caller supplies the decision and its columns; §4's machine decides
/// whether the record may take it, and the `UPDATE`'s own `state = 'submitted'`
/// predicate decides whether it still may by the time the statement runs.
///
/// `decided_at` is `stamp.recorded_at`. It is not a separate argument, for
/// [`NewApproval`]'s reason: the decision's instant and the record's instant are
/// one fact, and a call that could hand them two values could put a decision in
/// the approval store at a time the trail says nothing happened.
///
/// # The withdrawer's identity lives **here**, and only here
///
/// `approver_principal` is `None` on every void — `chk_pricing_approval_approver`
/// exempts a `voided` row precisely because a machine-driven void has no human
/// decider, and `chk_pricing_approval_distinct_principals` would refuse the
/// submitter's own withdraw outright if it were written there. So the column
/// cannot hold the withdrawer, and until this change nothing else did: a
/// submitter who withdrew their own unit left no trace of having done so. The
/// stamp's actor is what the `withdraw` record carries, which is the whole of
/// `inst-as-void`'s "audited".
///
/// # Errors
/// [`RepoError::NotFound`] when there is no such record in scope;
/// [`RepoError::ApprovalNotPending`] when it has already been decided or voided
/// — including the case where a concurrent decision landed between the read and
/// the write (`APPROVAL_NOT_PENDING`, 409). [`RepoError::Db`] on a scope or
/// storage failure, which is where a self-approval and a reasonless reject land
/// today; see the module doc. [`RepoError::CorruptRow`] on a `subject_ref` no
/// writer in this crate could have produced.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the runner and scope, the \
              (tenant, approval) the compare-and-swap addresses, the three decision columns the \
              store lets move, and the stamp the record is written under. Folding the decision \
              columns into the `ApprovalDecision` would make `chk_pricing_approval_reason` and \
              `chk_pricing_approval_approver` unrepresentable rather than merely refused, and is \
              the right shape - it is deferred to the group that owns the decision-refusal \
              vocabulary, so that one rule keeps one owner"
)]
pub async fn decide(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    approval_id: Uuid,
    decision: ApprovalDecision,
    approver_principal: Option<Uuid>,
    reason: Option<String>,
    stamp: AuditStamp,
) -> Result<ApprovalRecord, RepoError> {
    let current = read(runner, scope, tenant_id, approval_id)
        .await?
        .ok_or_else(|| RepoError::NotFound {
            subject: "approval".to_owned(),
            id: approval_id.to_string(),
        })?;

    // Sound because `ApprovalDecision` names no state the machine treats as a
    // self-edge, so the only refusal this can produce is `NotPending` and its
    // `from` is `current.state`; see the module doc.
    let state = current
        .state
        .decide(decision)
        .map_err(|_| not_pending(approval_id, current.state))?;

    swap(
        runner,
        scope,
        tenant_id,
        approval_id,
        state,
        approver_principal,
        reason.clone(),
        stamp.recorded_at,
    )
    .await?;

    let decided = ApprovalRecord {
        state,
        approver_principal,
        reason,
        decided_at: Some(stamp.recorded_at),
        ..current.clone()
    };
    append_trail(
        runner,
        scope,
        &decided,
        decision_action(decision),
        // The record **as it stood** — read in this transaction, before the swap
        // above moved it. An auditor asking what a decision was taken against
        // gets the answer from the record rather than from the writer's memory
        // of it.
        Some(&current),
        stamp,
    )
    .await?;
    Ok(decided)
}

/// The `action` a decision is recorded under.
///
/// A total match rather than an `if`, so a fourth decision cannot fall through
/// to whichever token happened to be the `else`. `Void` is `withdraw` because the
/// only voids that reach [`decide`] are human ones — the guard's own void goes
/// through [`void_pending_for_plan`], which writes nothing.
const fn decision_action(decision: ApprovalDecision) -> AuditAction {
    match decision {
        ApprovalDecision::Approve => AuditAction::Approve,
        ApprovalDecision::Reject => AuditAction::Reject,
        ApprovalDecision::Void => AuditAction::Withdraw,
    }
}

/// Append one approval-plane record, inside the caller's transaction.
///
/// **The before/after pair is the approval *record's*, not the subject's**, and
/// that is the one thing about this plane a reader has to know. The subject did
/// not change: a decision is a judgement *about* content that stood still, so
/// rendering the plan's lifecycle state twice would be two identical halves
/// telling an auditor nothing. What moved is the unit — from `submitted` to a
/// decision — and the pair says exactly that. The refused attempt's `deny` record
/// reads the same way and is written by `crate::infra::approval` for the
/// transaction reason its own doc gives.
///
/// `approval_ref` carries the unit's id, so a record joins to it without parsing
/// the subject; `subject_ref` and `subject_kind` are the unit's own, so the
/// approval plane and the authoring plane name one subject one way.
async fn append_trail(
    runner: &impl DBRunner,
    scope: &AccessScope,
    record: &ApprovalRecord,
    action: AuditAction,
    before: Option<&ApprovalRecord>,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    let aggregate = subject_aggregate(record)?;
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id: record.tenant_id,
            chain_id: aggregate.chain_id(),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action,
            subject_kind: record.subject_kind,
            subject_ref: record.subject_ref.clone(),
            before_state: before.map(unit_state),
            after_state: Some(unit_state(record)),
            approval_ref: Some(record.approval_id),
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
}

/// One approval unit as a record's `jsonb` half holds it.
///
/// The four facts a reviewer of the trail needs and the store's own columns
/// carry: where the unit stands, who opened it, who decided it, and on what
/// grounds. `contentHash` is in it because it is the pin the whole decision is
/// about (D-61) — a trail that recorded an approval without recording *what* was
/// approved is the hash-blind failure one store over.
///
/// Wire keys `camelCase`, as [`crate::domain::audit::subject_state`]'s are.
fn unit_state(record: &ApprovalRecord) -> JsonValue {
    json!({
        "approvalState": record.state.as_str(),
        "submitterPrincipal": record.submitter_principal,
        "approverPrincipal": record.approver_principal,
        "reason": record.reason,
        "contentHash": crate::domain::audit::hex_bytes(&record.content_hash),
    })
}

/// The plan a **plan-prefixed** unit's `subject_ref` names.
///
/// Two kinds carry one, and both are minted with the plan first: a plan revision's
/// `<plan_id>/<revision>` ([`audit_repo::plan_revision_ref`]) and a window
/// mutation's `<plan_id>/<window_id>` ([`audit_repo::window_ref`]). Parsed rather
/// than carried on the record, and parsed **once**: [`crate::infra::approval`]
/// re-derives the pinned subject through this same function, so which plan a unit is
/// about has one answer.
///
/// **The window arm is not a store read**, and a `window` unit's ref is not a bare
/// `window_id` that carries no plan. A store read cannot answer for a window that
/// has not been written, which is exactly the state D-62's refusal arm leaves
/// behind. [`audit_repo::window_ref`] has the argument.
///
/// A `price_unit`'s ref would be a bare `price_id` and would **not** parse here.
/// That kind has no writer, so no format is decided for it; [`subject_aggregate`]
/// records the divergence and the slice that gives it a writer owes the decision.
///
/// # Errors
/// [`RepoError::CorruptRow`] on a ref this crate cannot have written — the CHECKs
/// admit any text, so an unparseable one means the table was written around, and
/// that is an invariant breach rather than a caller's mistake.
pub fn subject_plan(record: &ApprovalRecord) -> Result<PlanId, RepoError> {
    record
        .subject_ref
        .split_once('/')
        .and_then(|(head, _)| Uuid::parse_str(head).ok())
        .map(PlanId::new)
        .ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "approval {} names the subject {:?}, which is not <plan_id>/<subject>",
                record.approval_id, record.subject_ref
            ))
        })
}

/// The overlay a unit's `subject_ref` names.
///
/// [`subject_plan`]'s counterpart, and it parses rather than reads for the same
/// reason: the aggregate must be answerable even when the subject itself is gone,
/// because that is exactly the state a reviewer deciding a stale unit is in.
///
/// # Errors
/// [`RepoError::CorruptRow`] on a ref this crate cannot have written.
pub fn subject_overlay(record: &ApprovalRecord) -> Result<Uuid, RepoError> {
    record
        .subject_ref
        .split_once('/')
        .and_then(|(head, _)| Uuid::parse_str(head).ok())
        .ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "approval {} names the subject {:?}, which is not <price_overlay_id>/<revision>",
                record.approval_id, record.subject_ref
            ))
        })
}

/// The `operation_id` half of a [`AuditSubjectKind::BulkOperation`] unit's
/// subject.
///
/// [`subject_overlay`]'s counterpart for a bulk operation, and simpler by one
/// step: `audit_repo::bulk_operation_ref` renders the whole ref as the bare
/// `operation_id`, with no second component to split off, because a run carries
/// no revision to disambiguate — `pricing_bulk_operation` never reopens under
/// one id the way a plan reopens a revision.
///
/// # Errors
/// [`RepoError::CorruptRow`] on a ref this crate cannot have written.
pub fn subject_bulk_operation(record: &ApprovalRecord) -> Result<Uuid, RepoError> {
    Uuid::parse_str(&record.subject_ref).map_err(|_| {
        RepoError::CorruptRow(format!(
            "approval {} names the subject {:?}, which is not an operation_id",
            record.approval_id, record.subject_ref
        ))
    })
}

/// The aggregate whose audit segment a unit of one subject kind belongs to.
///
/// D-135 keys a chain on the audited subject's *aggregate*, and S5 §6's aggregate
/// list — plan, overlay, payer, policy, bulk operation — has more than one member.
/// It **used to** be a `PlanId`, because every subject this gear could open a unit
/// over had a plan; G6's policy unit is the first that does not, and returning a
/// synthetic plan id for it would have put a threshold proposal on some plan's
/// chain, where it verifies perfectly and tells an auditor the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectAggregate {
    /// A plan — every subject kind but one.
    Plan(PlanId),
    /// The tenant's approval-threshold policy: one aggregate per tenant, with no
    /// id of its own because there is only ever one of it.
    Policy,
    /// One price overlay, named by its `price_overlay_id`.
    ///
    /// The third member, and the second that is not a plan. S5 §6's aggregate list
    /// names an overlay in its own right, and it has to: an overlay **is not a plan
    /// and has no plan**, so borrowing [`Self::Plan`] would put two aggregates on one
    /// hash chain — which then verifies perfectly while telling an auditor something
    /// false.
    Overlay(Uuid),
    /// One bulk operation — a mass-repricing run or an import — named by its
    /// `operation_id`.
    ///
    /// The fourth member and the third that is not a plan, for [`Self::Overlay`]'s
    /// exact reason: a run's rows may span several plans (`inst-mr-api`), so it
    /// has no one plan to borrow [`Self::Plan`] from, and folding it onto the
    /// first touched plan's chain would put a run's approval trail on a chain an
    /// auditor reading that plan alone would never reach the rest of.
    BulkOperation(Uuid),
    /// One payer's membership history, named by `payer_tenant_id` — S5 §6's
    /// fifth aggregate (`audit_repo::payer_chain`'s own doc: "makes a payer
    /// one in its own right").
    ///
    /// The fifth member and the fourth that is not a plan, for [`Self::Overlay`]'s
    /// exact reason: a membership row has no plan (D-09's non-overlap is per
    /// `(tenant, payer)`, not per plan), so putting `inst-mm-immediate`'s unit
    /// on [`Self::Plan`] would put a payer's group history on a chain that
    /// happens to share a plan with them, which is not what the row is about.
    Payer(Uuid),
}

impl SubjectAggregate {
    /// The `chain_id` half of this aggregate's audit segment key.
    #[must_use]
    pub const fn chain_id(self) -> Uuid {
        match self {
            Self::Plan(plan_id) => audit_repo::plan_chain(plan_id),
            Self::Policy => audit_repo::policy_chain(),
            Self::Overlay(price_overlay_id) => audit_repo::overlay_chain(price_overlay_id),
            Self::BulkOperation(operation_id) => audit_repo::bulk_operation_chain(operation_id),
            Self::Payer(payer_tenant_id) => audit_repo::payer_chain(payer_tenant_id),
        }
    }

    /// The plan, when the aggregate is one.
    ///
    /// `Option` rather than a panicking accessor: the caller that needs a plan is
    /// re-deriving a plan-bearing subject, so it has already matched on the kind
    /// and its `None` arm is a store written around rather than a shape it must
    /// handle. See `infra::approval::plan_of`, which is where that reading is
    /// turned into an error message.
    #[must_use]
    pub const fn plan(self) -> Option<PlanId> {
        match self {
            Self::Plan(plan_id) => Some(plan_id),
            // Four aggregates that are not a plan, and the overlay, the bulk
            // operation and the payer are the ones where saying so matters: all
            // three are **subjects with a plan-shaped id**, so an accessor that
            // answered `Some` here would hand a caller a `PlanId` built from an
            // overlay's, a run's or a payer's uuid and every read under it would
            // miss quietly.
            Self::Policy | Self::Overlay(_) | Self::BulkOperation(_) | Self::Payer(_) => None,
        }
    }
}

/// The aggregate a unit of **any** subject kind belongs to. What differs per kind
/// is how the ref names it.
///
/// * `plan_revision` — the ref *is* `<plan_id>/<revision>`, so [`subject_plan`]
///   parses it and no read happens.
/// * `window` — the ref is `<plan_id>/<window_id>`, so [`subject_plan`] parses that
///   one too. It **used to** be a bare `window_id` resolved through a one-row read of
///   the window's canonical scope key, and the argument for that was symmetry across
///   the two stores D-158 requires to agree. The read was the defect: a window unit
///   is opened by the arm that writes **nothing** (D-62), so the row it read did not
///   exist and this function answered `CorruptRow` — a 500 — for every read of a
///   pending cancellation's unit. Encoding the plan is what makes both stores spell
///   one act one way *and* makes the aggregate answerable without the subject
///   existing.
/// * `policy` — the ref is a bare version number and names **no** aggregate id,
///   because the aggregate is the tenant's single threshold policy. No read
///   happens and none can fail; the ref is not even parsed here, since which
///   version it is has no bearing on which segment the record extends.
/// * `price_unit` — **three writers, and all three mint a plan-prefixed ref**, so
///   it inherits [`subject_plan`]'s parse rather than getting an arm of its own and
///   the parse reads every one of them:
///   `<plan_id>/<price_id>/grandfather-until/<prior>/<new>` from
///   `infra::grandfather::horizon_unit_ref`, `<plan_id>/supersession/<key>/<changeover>`
///   from [`crate::infra::supersession::supersession_unit_ref`], and
///   `<plan_id>/cutover/<key set hash>/<instant>` from
///   [`crate::infra::cutover::cutover_unit_ref`]. All three carry the kind because S5
///   §6 declares no `supersession`, `cutover` or `grandfather` member and this gear
///   mints no token the design set has not (D-204 clause (2)) — the *act* rides the
///   subject string instead, which is what tells these three and a `PATCH`'s unit on
///   the same row apart.
///
///   **The writers exist and the shape is decided** — the roster at the top of this
///   file names `submit_supersession_on` among them, and what they write is the
///   plan-prefixed shape above. Giving this arm the bare-`price_id` parse
///   `entity::approval`'s column doc still describes would make [`subject_plan`]
///   fail on every one of the three, turning each submit into a `CorruptRow` 500 —
///   exactly what `audit_repo::window_ref`'s doc records having been paid for once
///   already, on the arm one line up. That column doc is owed the same correction.
///
/// **No read, and therefore no `Db` failure.** It took a runner and a scope while the
/// `window` arm resolved its plan out of the store; every kind resolves by parse now,
/// so both arguments are gone rather than carried unused.
///
/// # Errors
/// [`RepoError::CorruptRow`] on a ref this crate cannot have written, or on a
/// subject kind with no resolution.
pub fn subject_aggregate(record: &ApprovalRecord) -> Result<SubjectAggregate, RepoError> {
    match record.subject_kind {
        AuditSubjectKind::PlanRevision | AuditSubjectKind::PriceUnit | AuditSubjectKind::Window => {
            subject_plan(record).map(SubjectAggregate::Plan)
        }
        // A taxonomy value (D-353) is a row of the tenant's config object, and its
        // audit records already sit on `policy_chain()` (`taxonomy_repo::
        // record_value_mutation`): the unit that decides one of those edits belongs
        // on the same segment, or the decision and the edit it authorised sort apart.
        AuditSubjectKind::Policy | AuditSubjectKind::TaxonomyValue => Ok(SubjectAggregate::Policy),
        // **Its own aggregate, resolved by parse like every other kind.** This arm
        // refused outright while the unit was unwired — "storable and not resolvable",
        // which was true for exactly as long as nothing opened one. D-225's
        // `submit_overlay_on` is the writer, and the ref it writes is
        // `audit_repo::overlay_revision_ref`.
        AuditSubjectKind::Overlay => subject_overlay(record).map(SubjectAggregate::Overlay),
        // This arm refused outright while the unit was
        // unwired — `AuditSubjectKind::Overlay`'s own situation before D-225,
        // reproduced exactly: `bulk_operation` was storable
        // (`chk_pricing_approval_subject_kind`, `pricing_migration`, D-158's
        // extend-together rule) and not resolvable, because `inst-bs-approval`'s
        // batch approval was the unit that would open one and it was unwired.
        // `api::rest::repricing_runs::advance_on_verdict` is that writer now.
        AuditSubjectKind::BulkOperation => {
            subject_bulk_operation(record).map(SubjectAggregate::BulkOperation)
        }
        // (Task 7 of the customer-group plane). This arm
        // refused outright while the unit was unwired — `AuditSubjectKind::Overlay`'s
        // own situation before D-225, reproduced exactly: `membership` was
        // storable (`chk_pricing_approval_subject_kind`, `chk_pricing_approval_subject_kind`)
        // and not resolvable, because `ApprovalService::submit_membership_move_on`
        // was the writer that would open one and it did not exist yet.
        //
        // **Resolved by parse, like every other kind — and the parse is the
        // whole subject**, not a prefix of it. `inst-mm-pending` settles that a
        // material membership change carries its payload *inside* the
        // approval record, so `subject_membership_move` decodes the whole
        // proposed member set out of `subject_ref`. No store read, and
        // therefore no `RepoError::Db` on this arm either.
        //
        // **One payer names [`SubjectAggregate::Payer`]** — S5 §6's own
        // aggregate for exactly this shape (`inst-mm-immediate`). **Many
        // payers reuse [`SubjectAggregate::BulkOperation`]**, deliberately:
        // `inst-mm-bulk`'s unit spans several payers the same way a
        // repricing run spans several plans, so no one payer's chain is the
        // right home for its submit/approve/reject trail, and
        // `bulk_operation_chain` is a pure function of any `Uuid` rather than
        // a fact about `pricing_bulk_operation` specifically — nothing about
        // reusing it here claims a row in that table exists. The id fed to it
        // is the set's own content digest (truncated to 16 bytes), so two
        // requests naming the same payer set land on the same segment and a
        // segment is never shared between two different proposed sets.
        AuditSubjectKind::Membership => {
            let set = subject_membership_move(record)?;
            match set.proposals() {
                [one] => Ok(SubjectAggregate::Payer(one.payer_tenant_id)),
                _many => {
                    let digest = membership_content_hash(&set);
                    let mut bytes = [0u8; 16];
                    bytes.copy_from_slice(&digest[..16]);
                    Ok(SubjectAggregate::BulkOperation(Uuid::from_bytes(bytes)))
                }
            }
        }
    }
}

/// The `subject_ref` a membership-move unit is opened under — `inst-mm-pending`'s
/// whole payload, encoded as the subject rather than read from a second store.
///
/// A JSON array of the set's own canonical (payer-sorted) order — see
/// [`MembershipMoveSet`]'s doc — prefixed so a reader can tell this ref apart
/// from every other kind's at a glance, exactly as [`audit_repo::plan_revision_ref`]'s
/// `<plan_id>/<revision>` and [`audit_repo::overlay_revision_ref`]'s
/// `<overlay_id>/<revision>` are each their own kind's shape.
///
/// # Errors
/// [`RepoError::Db`] when the set will not serialize — unreachable for
/// [`MembershipMoveProposalWire`]'s three fields (a `Uuid`, a `String` and a
/// `OffsetDateTime`, none of which can fail to serialize), and *reported*
/// rather than unwrapped so a caller on a route answers 500 instead of
/// panicking a request thread.
pub fn membership_move_subject_ref(set: &MembershipMoveSet) -> Result<String, RepoError> {
    let wire: Vec<MembershipMoveProposalWire> = set
        .proposals()
        .iter()
        .map(MembershipMoveProposalWire::of)
        .collect();
    let json = serde_json::to_string(&wire)
        .map_err(|e| RepoError::Db(format!("cannot render a membership move set: {e}")))?;
    Ok(format!("membership-move/{json}"))
}

/// The prefix every taxonomy-value unit's `subject_ref` carries (D-353).
pub const TAXONOMY_VALUE_REF_PREFIX: &str = "taxonomy-value/";

/// The `subject_ref` a taxonomy-value unit is opened under — the whole proposal,
/// encoded as the subject, for [`membership_move_subject_ref`]'s reason: a
/// taxonomy value has no draft table, so the pending edit has nowhere else to
/// live. One pending unit per exact `(class, value, patch)`.
///
/// # Errors
/// [`RepoError::Db`] when the proposal will not serialize — unreachable for the
/// wire shape's plain fields, and reported rather than unwrapped so a route
/// answers 500 instead of panicking a request thread.
pub fn taxonomy_value_subject_ref(proposal: &TaxonomyValueProposal) -> Result<String, RepoError> {
    let json = serde_json::to_string(&TaxonomyValueProposalWire::of(proposal))
        .map_err(|e| RepoError::Db(format!("cannot render a taxonomy value proposal: {e}")))?;
    Ok(format!("{TAXONOMY_VALUE_REF_PREFIX}{json}"))
}

/// The proposal a taxonomy-value unit's `subject_ref` names —
/// [`subject_membership_move`]'s counterpart.
///
/// # Errors
/// [`RepoError::CorruptRow`] on a ref this crate cannot have written.
pub fn subject_taxonomy_value(record: &ApprovalRecord) -> Result<TaxonomyValueProposal, RepoError> {
    let corrupt = || {
        RepoError::CorruptRow(format!(
            "approval {} names the subject {:?}, which is not taxonomy-value/<json>",
            record.approval_id, record.subject_ref
        ))
    };
    let json = record
        .subject_ref
        .strip_prefix(TAXONOMY_VALUE_REF_PREFIX)
        .ok_or_else(corrupt)?;
    // Its own message, but still without the payload. `corrupt()` says the ref
    // "is not taxonomy-value/<json>", which is false once the prefix has matched —
    // the two failures are a missing prefix and a body that will not parse, and
    // they are diagnosed differently. What it must not do is quote the body: that
    // body is the stored proposal, and a config reader must not receive the
    // content of a pending change in an error. `serde_json`'s own message can
    // echo the input, so only its line and column are carried.
    let wire: TaxonomyValueProposalWire = serde_json::from_str(json).map_err(|e| {
        RepoError::CorruptRow(format!(
            "approval {} carries a taxonomy-value payload that does not decode at line {}, \
             column {}",
            record.approval_id,
            e.line(),
            e.column()
        ))
    })?;
    wire.into_domain().ok_or_else(corrupt)
}

/// The stored spelling of a [`TaxonomyValueProposal`].
///
/// [`TaxCategoryPatch`]'s three meanings — keep, set, clear — are two members
/// here, because a nullable member alone folds the clear into the keep on the
/// way back.
#[derive(serde::Serialize, serde::Deserialize)]
struct TaxonomyValueProposalWire {
    class: String,
    value: String,
    display_name: Option<String>,
    state: Option<String>,
    tax_category: Option<String>,
    clear_tax_category: bool,
    tax_rate_present: Option<bool>,
}

impl TaxonomyValueProposalWire {
    fn of(proposal: &TaxonomyValueProposal) -> Self {
        let TaxonomyValuePatch {
            display_name,
            state,
            tax_category,
            tax_rate_present,
        } = &proposal.patch;
        Self {
            class: proposal.class.path_segment().to_owned(),
            value: proposal.value.as_str().to_owned(),
            display_name: display_name.clone(),
            state: state.map(|s| s.as_str().to_owned()),
            tax_category: match tax_category {
                TaxCategoryPatch::Set(category) => Some(category.clone()),
                TaxCategoryPatch::Keep | TaxCategoryPatch::Clear => None,
            },
            clear_tax_category: matches!(tax_category, TaxCategoryPatch::Clear),
            tax_rate_present: *tax_rate_present,
        }
    }

    fn into_domain(self) -> Option<TaxonomyValueProposal> {
        let class = TaxonomyClass::parse_segment(&self.class)?;
        let value = ScopeValue::new(&self.value)?;
        let state = match self.state {
            None => None,
            Some(token) => Some(TaxonomyState::parse(&token)?),
        };
        let tax_category = if self.clear_tax_category {
            TaxCategoryPatch::Clear
        } else {
            self.tax_category
                .map_or(TaxCategoryPatch::Keep, TaxCategoryPatch::Set)
        };
        Some(TaxonomyValueProposal {
            class,
            value,
            patch: TaxonomyValuePatch {
                display_name: self.display_name,
                state,
                tax_category,
                tax_rate_present: self.tax_rate_present,
            },
        })
    }
}

/// The membership-move payload a unit's `subject_ref` names.
///
/// [`subject_overlay`]'s counterpart for the one kind whose whole content —
/// not merely an id and a revision — lives in the ref. Re-validated through
/// [`MembershipMoveSet::new`] rather than trusted as constructed, so a row
/// written around the store (a migration, a restore) with an empty or
/// duplicate-payer payload is caught here rather than handed to a caller as
/// though it were reviewable.
///
/// # Errors
/// [`RepoError::CorruptRow`] on a ref this crate cannot have written.
pub fn subject_membership_move(record: &ApprovalRecord) -> Result<MembershipMoveSet, RepoError> {
    let corrupt = || {
        RepoError::CorruptRow(format!(
            "approval {} names the subject {:?}, which is not membership-move/<json>",
            record.approval_id, record.subject_ref
        ))
    };
    let json = record
        .subject_ref
        .strip_prefix("membership-move/")
        .ok_or_else(corrupt)?;
    let wire: Vec<MembershipMoveProposalWire> =
        serde_json::from_str(json).map_err(|_| corrupt())?;
    let proposals = wire
        .into_iter()
        .map(MembershipMoveProposalWire::into_domain)
        .collect();
    MembershipMoveSet::new(proposals).map_err(|_| corrupt())
}

/// The wire shape [`membership_move_subject_ref`] and [`subject_membership_move`]
/// serialize [`MembershipMoveProposal`] through.
///
/// A private mirror rather than `#[derive(Serialize, Deserialize)]` on the
/// domain type itself: the `subject_ref`'s JSON shape is this store's own
/// concern (`subject_plan`'s "one place to answer how a subject is named"),
/// not a wire contract the domain type should carry for every caller.
#[derive(serde::Serialize, serde::Deserialize)]
struct MembershipMoveProposalWire {
    payer_tenant_id: Uuid,
    group_value: String,
    #[serde(with = "rfc3339")]
    effective_from: OffsetDateTime,
}

impl MembershipMoveProposalWire {
    fn of(proposal: &MembershipMoveProposal) -> Self {
        Self {
            payer_tenant_id: proposal.payer_tenant_id,
            group_value: proposal.group_value.clone(),
            effective_from: proposal.effective_from,
        }
    }

    fn into_domain(self) -> MembershipMoveProposal {
        MembershipMoveProposal {
            payer_tenant_id: self.payer_tenant_id,
            group_value: self.group_value,
            effective_from: self.effective_from,
        }
    }
}

/// The tenant's pending threshold-policy proposal, if it has one.
///
/// `inst-co-single-pending` for the D-10 unit, and it cannot be
/// [`find_pending_for_subject`]: a policy unit's `subject_ref` is the **version
/// number** it proposes, so every proposal names a different subject and an
/// exact-ref read would find nothing. What the rule means on this plane is "one
/// pending proposal per tenant" — the aggregate is the tenant's single policy, and
/// two open proposals would leave two reviewers approving two versions whose order
/// of arrival then decides the tenant's thresholds.
///
/// Ordered by `submitted_at` so a tenant that somehow holds two names the older,
/// which is the one a reviewer is already looking at.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on
/// a token outside its enumeration.
pub async fn find_pending_policy_unit(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Option<ApprovalRecord>, RepoError> {
    let row = approval::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectKind.eq(AuditSubjectKind::Policy.as_str())),
        )
        .order_by(approval::Column::SubmittedAt, Order::Asc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the pending threshold policy unit: {e}")))?;
    row.map(to_domain).transpose()
}

/// Void every **pending** unit pinned to a revision of `plan_id` — the TOCTOU
/// guard's storage half (`inst-ap-pin`, `inst-as-void`).
///
/// One `UPDATE`, not a read-then-write. There is nothing to judge: the state
/// machine's `submitted -> voided` edge is legal from every pending record, the
/// predicate selects only pending ones, and a caller cannot be told anything
/// useful about how many it closed. A read first would also be a race with
/// itself — a decision landing between the read and the write would be
/// overwritten by the void, on the row that **is** the evidence of who agreed.
///
/// # Why the subject is matched by prefix
///
/// A pending unit's `subject_ref` is `<plan_id>/<revision>`, and the mutating
/// paths that call this know the plan but not always the revision — a price row
/// carries its plan on its scope key and no revision at all. Matching
/// `<plan_id>/` covers every revision of the plan, which is what is wanted
/// rather than a widening: D-146 gives a plan **one** open draft revision, a
/// pending unit pins that revision, and a mutation of the plan's shape or rows
/// is a mutation of that revision's candidate content. Deriving the revision at
/// each call site would be a second answer to "which unit is at risk", free to
/// disagree with this one.
///
/// The trailing `/` is load-bearing: without it the prefix of one uuid could
/// match another's, which uuids make unlikely and not impossible.
///
/// # What it writes, and why every CHECK is satisfied
///
/// `state = 'voided'`, `decided_at = voided_at`, `reason = ` [`TOCTOU_VOID_REASON`].
/// `approver_principal` is left NULL, which `chk_pricing_approval_approver`
/// exempts for `voided` precisely because a machine-driven void has no human
/// decider. The three columns are exactly the ones
/// `trg_pricing_approval_append_only`'s whitelist admits.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure. It runs inside the calling
/// mutation's transaction, so the failure rolls that mutation back — a mutation
/// that could not void the approval it invalidated must not commit, or a
/// reviewer approves content that moved.
pub async fn void_pending_for_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    voided_at: OffsetDateTime,
) -> Result<u64, RepoError> {
    let result = approval::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            approval::Column::State,
            Expr::value(ApprovalState::Voided.as_str()),
        )
        .col_expr(approval::Column::DecidedAt, Expr::value(voided_at))
        .col_expr(
            approval::Column::Reason,
            Expr::value(crate::infra::approval::TOCTOU_VOID_REASON),
        )
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str()))
                .add(approval::Column::SubjectKind.eq(AuditSubjectKind::PlanRevision.as_str()))
                .add(approval::Column::SubjectRef.starts_with(format!("{plan_id}/"))),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("void pending approvals of plan {plan_id}: {e}")))?;
    Ok(result.rows_affected)
}

/// The compare-and-swap itself: move a record that is **still** `submitted` onto
/// its decision, or refuse.
///
/// Two things here, and each is load-bearing on its own.
///
/// **`state = 'submitted'` in the predicate** is the physical half of
/// `inst-as-immutable`, and the race it addresses is real and unprevented:
/// nothing holds a pending record between [`decide`]'s read and this statement,
/// so two reviewers can both read one record, both pass §4's machine, and both
/// arrive here. Without the predicate the second `UPDATE` matches on the primary
/// key alone and overwrites the first reviewer's `approver_principal` and
/// `reason` — on the row that *is* the evidence of who agreed. The trigger would
/// still refuse it, but as a driver error, which reaches the caller as a **500**
/// telling an operator to page somebody about a race whose whole remedy is to
/// re-read. With the predicate, the loser matches nothing and is told
/// `APPROVAL_NOT_PENDING` (409), which is the answer §5 specifies.
/// `idempotency_repo::take_over` guards its own swap the same way and for the
/// same reason.
///
/// **`.scope_with(scope)` on the write** is the tenant gate on the mutating
/// path, and it is not made redundant by the read [`decide`] performs first.
/// That read is a separate statement under a separate gate; a `decide` that
/// reached this function by any other route, or a read whose gate was widened,
/// would otherwise write across a tenant boundary — and the object it would
/// write is another tenant's approval record.
///
/// **Matching no row is re-read before it is reported.** The state named in the
/// refusal is the row's own as of now, not the state the caller read: in the
/// race this function exists for, the caller read `submitted`, and reporting
/// that would answer a lost decision with "approval X is submitted; only a
/// submitted record is decidable".
///
/// # Errors
/// [`RepoError::ApprovalNotPending`] when the record has moved past `submitted`;
/// [`RepoError::NotFound`] when nothing in this caller's scope answers to the id
/// — which is what a foreign scope sees, deliberately indistinguishable from
/// absence for the reason [`read`] gives. [`RepoError::Db`] on a scope or
/// storage failure.
#[allow(
    clippy::too_many_arguments,
    reason = "the arguments of `decide` minus the decision the machine already resolved; \
              see that function's own note"
)]
async fn swap(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    approval_id: Uuid,
    state: ApprovalState,
    approver_principal: Option<Uuid>,
    reason: Option<String>,
    decided_at: OffsetDateTime,
) -> Result<(), RepoError> {
    let result = approval::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(approval::Column::State, Expr::value(state.as_str()))
        .col_expr(
            approval::Column::ApproverPrincipal,
            Expr::value(approver_principal),
        )
        .col_expr(approval::Column::Reason, Expr::value(reason))
        .col_expr(approval::Column::DecidedAt, Expr::value(decided_at))
        .filter(
            Condition::all()
                .add(approval::Column::TenantId.eq(tenant_id))
                .add(approval::Column::ApprovalId.eq(approval_id))
                .add(approval::Column::State.eq(ApprovalState::Submitted.as_str())),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("decide pricing_approval {approval_id}: {e}")))?;

    if result.rows_affected == 0 {
        return Err(match read(runner, scope, tenant_id, approval_id).await? {
            Some(fresh) => not_pending(approval_id, fresh.state),
            None => RepoError::NotFound {
                subject: "approval".to_owned(),
                id: approval_id.to_string(),
            },
        });
    }
    Ok(())
}

/// The pendingness refusal, built in one place so the two producers cannot spell
/// it differently.
fn not_pending(approval_id: Uuid, state: ApprovalState) -> RepoError {
    RepoError::ApprovalNotPending {
        approval_id: approval_id.to_string(),
        state: state.as_str().to_owned(),
    }
}

/// Read a stored row into the domain's vocabulary.
///
/// Both discriminators are read through their `ALL` slices rather than parsed:
/// the enumeration lives in one place per type, and a token the CHECK admits
/// while this crate does not is an invariant breach the boundary reports rather
/// than a string a handler renders.
fn to_domain(row: approval::Model) -> Result<ApprovalRecord, RepoError> {
    let state = super::plan_repo::read_token(
        "pricing_approval.state",
        &row.state,
        ApprovalState::ALL,
        ApprovalState::as_str,
    )?;
    let subject_kind = super::plan_repo::read_token(
        "pricing_approval.subject_kind",
        &row.subject_kind,
        AuditSubjectKind::ALL,
        AuditSubjectKind::as_str,
    )?;
    Ok(ApprovalRecord {
        approval_id: row.approval_id,
        tenant_id: row.tenant_id,
        subject_ref: row.subject_ref,
        subject_kind,
        content_hash: row.content_hash,
        state,
        submitter_principal: row.submitter_principal,
        approver_principal: row.approver_principal,
        reason: row.reason,
        materiality: row.materiality,
        submitted_at: row.submitted_at,
        decided_at: row.decided_at,
    })
}

#[cfg(test)]
#[path = "approval_repo_tests.rs"]
mod approval_repo_tests;
