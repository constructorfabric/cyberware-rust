//! The approval workflow, as the surfaces will drive it: open a pinned unit,
//! decide it, and record the attempts that were refused.
//!
//! [`crate::domain::approval::decision`] holds the judgement and is pure;
//! [`crate::infra::storage::repo::approval_repo`] holds the store and judges
//! nothing. This is the layer in between, and it exists for the three things
//! neither of those can do: re-derive the pinned subject from the database,
//! commit the decision, and **write the attempted-violation record**
//! `inst-tp-selfaudit` requires.
//!
//! # `approval_repo` takes the `AuditStamp` now, and the trail is complete
//!
//! The phase's Global Constraints say every mutating repository call takes one.
//! `approval_repo::open` and `decide` did not, which G2 reported rather than
//! absorbed: `AuditAction` declared no `submit`, `approve`, `reject` or
//! `withdraw`, so a stamp threaded there would have named a record those
//! functions cannot write. The tokens and their writers landed together, so both
//! functions take a stamp and both append inside the transaction that mutates —
//! see `approval_repo`'s own module doc. What is left here is the `deny` record
//! of a refused attempt, and the two rulings this group owed on it.
//!
//! # The denial record is written in the judgement transaction, because that
//! transaction commits
//!
//! A second transaction for the record would be paying a crash window for a hazard
//! that does not exist. The argument for one runs: the decision happens inside one
//! transaction — read the record, re-assemble the subject, judge, swap — and a
//! refusal makes that transaction *roll back*, so a record written inside it would
//! be a record that never existed.
//!
//! **A refusal does not roll that transaction back.** [`judge`] returns
//! `Ok(`[`Outcome::Refused`]`)` — a refusal is the transaction's *answer*, not
//! its failure — and `in_transaction` commits on `Ok` and rolls back only on
//! `Err` (`toolkit_db::secure::SecureConn::in_transaction`). The refusal path's
//! transaction commits.
//!
//! So the record is written **inside** [`judge`], on the refusal branch, before
//! the outcome is returned. It needs no `SAVEPOINT` (there is nothing to undo: the
//! swap has not run and the transaction has done nothing but read), no second
//! connection and no outbox row, and there is no window between the two: the
//! judgement and its `deny` record commit or fail together.
//!
//! What the reversal does **not** change:
//!
//! - **A refused decision still leaves the unit exactly as it found it.** The
//!   swap is on the other branch; the refusal branch's only write is the record.
//! - **A record that cannot be written still fails the whole decision.** It
//!   fails it harder, and better: the append's error is an `Err` out of `judge`,
//!   so the transaction rolls back and `decide` answers 500 rather than a clean
//!   403. An attempt this gear could not record must not be reported as though
//!   the trail were complete.
//! - **`inst-tp-selfaudit` is still satisfied** — *"rejected **and** written to
//!   `pricing_audit_log`"* — and now by a single commit rather than by two that
//!   a crash could separate.
//!
//! The three previously-examined alternatives are moot. Writing the record
//! *before* judging is still wrong for its own reason (the refusal is not known
//! until after the re-derivation, and a record written for every decision would
//! have to be retracted on the append-only store whose triggers forbid exactly
//! that), and it is not what this does: the record is written after the
//! judgement, in the same transaction.
//!
//! # Ruling 2 — the `deny` record's before/after pair is **ratified**, and it is
//! now the plane's shape rather than one action's exception
//!
//! The pair is not an exception peculiar to `deny`: **no** record on the
//! approval plane is about a change to the
//! subject. An approve, a reject and a withdraw are all judgements *about* content
//! that stood still, so every one of them renders the **unit** rather than the
//! subject — `approval_repo::append_trail` writes the unit's state before and
//! after the flip. The `deny` pair is the same reading with the one difference the
//! act forces, and it is ratified as written:
//!
//! - `before_state` — **the record as it stood**: its state, its subject and its
//!   submitter. An auditor needs to know which unit was attacked.
//! - `after_state` — **the attempt**: the code it was refused with, the decision
//!   that was asked for, and the principal who asked.
//!
//! The asymmetry is load-bearing and is why the pair is not simply
//! `append_trail`'s. There is no "after" state of the unit to render — the swap
//! never ran and the row is byte-identical to `before_state` — so a pair rendered
//! the sibling way would be the same object twice, and a reader could not tell a
//! refused attempt from a decision that changed nothing. The refusal has to go
//! somewhere, and the `after` half is the only half free to hold it.
//!
//! `approval_ref` carries the approval id, so the record joins to the unit
//! without parsing the subject.
//!
//! # The TOCTOU void hangs off the two audit choke points, and off one more
//! call site
//!
//! [`void_pending_units_of`] is called from
//! [`record_revision_mutation`](crate::infra::storage::repo::plan_repo) and
//! `price_repo`'s `record_price_mutation`, which between them are the audit
//! writer of every authoring mutation of a plan's shape or rows — and from
//! `plan_repo::open_revision`, which is neither.
//!
//! **The void is not inherited, and a surface added later has to be checked.** The
//! inheritance argument runs: every mutating repository call takes an `AuditStamp`
//! and lands a record, therefore a surface added later cannot mutate a pinned
//! subject without passing through one of those two functions, therefore it
//! inherits the void. The middle step does not hold — landing a record and landing
//! it *through those two functions* are different obligations, and the crate
//! contains four writers that honour the first without the second: `create_draft`,
//! `create_draft_on`, the publish commit, and `open_revision`, which inserts a
//! revision row, copies three child tables and appends its own `Create` record.
//! Three of those are argued exceptions; the fourth is the one an argument of this
//! shape hides, by saying it cannot exist.
//!
//! So the placement is kept for what it is actually worth — nine surfaces
//! covered by two call sites instead of nine — and the claim it was justified
//! with is replaced by an obligation on the reader: a **direct** audit writer is
//! a void site to be decided about, and the enumeration in
//! [`void_pending_units_of`]'s doc lists every one of them with its decision.
//! A later slice inherits the void only where it goes through the rail.

use std::collections::BTreeSet;

use serde_json::{Value as JsonValue, json};
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::approval::content_pin::{
    bundle_content_hash, membership_content_hash, overlay_content_hash, repricing_run_content_hash,
    taxonomy_value_content_hash, threshold_content_hash,
};
use crate::domain::approval::{
    ApprovalState, DecisionBy, DecisionRefusal, DecisionRequest, WithdrawAuthority,
    authorize_decision, content_hash,
};
use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::concurrency::RowVersion;
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::materiality::{ThresholdEntry, ThresholdVersion};
use crate::domain::membership_change::MembershipMoveSet;
use crate::domain::money::CurrencyCode;
use crate::domain::overlay::{OverlayRevision, ScopeClass};
use crate::domain::plan_shape::PlanShape;
use crate::domain::ports::CatalogVersionRegistryV1;
use crate::domain::scope_key::{PlanId, Region};
use crate::domain::taxonomy::{TaxonomyClass, TaxonomyEntry, TaxonomyValueChange};
use crate::infra::storage::odata_mapping::OdataPageError;
use crate::infra::storage::repo::approval_repo::{ApprovalRecord, NewApproval};
use crate::infra::storage::repo::bundle_repo::{self, CompositionDraft};
use crate::infra::storage::repo::{
    NewAuditEntry, approval_repo, audit_repo, bulk_repo, plan_repo, price_repo,
    repricing_journal_repo, threshold_repo,
};
use crate::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

/// The `reason` a TOCTOU void writes on the record it closes.
///
/// A literal rather than an operator's words, because nobody typed it: the
/// subject moved and the guard fired. It is stored so that a reviewer who comes
/// back to a `voided` unit is told **why** it closed rather than being left to
/// infer it from a null.
pub const TOCTOU_VOID_REASON: &str =
    "voided by a post-submit mutation of the pinned subject (inst-ap-pin)";

/// The `reason` a publish writes on a unit it orphaned.
///
/// Its own literal rather than [`TOCTOU_VOID_REASON`], because it is a different
/// event and a reviewer coming back to the record has to be able to tell them
/// apart: nothing about the subject *moved* — it was frozen, under a decision
/// this unit was not the one. Saying "a post-submit mutation" here would send an
/// operator looking for an edit that never happened.
pub const ORPHANED_BY_PUBLISH_REASON: &str =
    "voided by the publish of the pinned revision under another approval (inst-as-void)";

/// The deciding principal's pricing-region grant, **or the fact that nothing
/// transports one**.
///
/// # Why this is a two-valued type and not a set
///
/// `inst-ap-scope` measures the approver's grant against the pricing regions the
/// pinned change set touches, and `inst-rb-preview-scope` says a grant *"carries
/// an explicit pricing-region set"* and stops there: **no document in the set
/// says how that set is transported** — which claim, which PDP constraint, which
/// resource property. So a surface today has nothing to read, and the only
/// honest thing it can hand over is the statement that it has nothing.
///
/// It used to hand over a *set* anyway — the change set's own reach, read at the
/// surface — and that shape had a defect no amount of documentation fixed. The
/// read happened **outside** the judgement transaction while
/// [`judge`] re-derives the change set **inside** it, so a mutation that added a
/// row in a new region between the two made `is_subset` fail: a 403 **and**, via
/// [`DecisionRefusal::is_an_audited_violation`], a `deny` record filed against a
/// reviewer who had exceeded no authority. It failed closed, which is why it was
/// not a hole; what it manufactured was a false attempted-violation record, and
/// those are read by humans.
///
/// [`RegionGrant::Untransported`] moves that synthesis inside `judge`, off the
/// same re-derivation the rule is judged against, so the two values cannot
/// disagree because there is now only one read.
///
/// [`DecisionRefusal::is_an_audited_violation`]: crate::domain::approval::DecisionRefusal::is_an_audited_violation
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionGrant {
    /// **Nothing transports a grant on this path.** The rule is then measured
    /// against the change set's own reach, taken from the subject
    /// [`judge`] re-derived, so `change_set_regions.is_subset(approver_regions)`
    /// holds by construction and `REGION_SCOPE_DENIED` is unreachable.
    ///
    /// The consequence is stated rather than buried: **`inst-ap-scope` is not
    /// enforced for a caller that passes this.** The alternative was
    /// fail-closed, and it was rejected on what it does rather than on how it
    /// reads — an empty grant refuses every change set carrying a single price
    /// row, so no unit could ever be approved and no plan could ever publish, in
    /// the name of a rule whose input nobody can supply.
    ///
    /// Whichever wave declares the transport reads it and passes
    /// [`RegionGrant::Explicit`] instead; this variant is what makes that day a
    /// visible change rather than a silent one.
    Untransported,
    /// An explicit grant, as a declared transport will supply it — and as the
    /// service-level suites supply it to drive **both** directions of
    /// `inst-ap-scope`, which is the coverage that would be lost if the field
    /// collapsed into the synthesis above.
    Explicit(BTreeSet<Region>),
}

/// What a decision was asked to do, as a surface will hand it over.
#[derive(Clone, Debug)]
pub struct DecideRequest {
    /// The record being decided.
    pub approval_id: Uuid,
    /// Approve, reject or withdraw — **and by whom**, as one value.
    ///
    /// The identity is not a field beside this one, and
    /// [`DecisionBy`]'s own doc says why: an approve that named nobody used to
    /// slip both authority rules and land on a CHECK. A surface that has no
    /// authenticated principal cannot construct an `Approve` here at all, which
    /// is the obligation landing where the principal actually is.
    pub decision: DecisionBy,
    /// The reason, mandatory on a reject.
    ///
    /// Deliberately **not** folded into [`DecisionBy::Reject`], for the reason
    /// `approval_repo`'s module doc gives for not folding it into the repo
    /// signature either: a reason-less reject has to stay constructible, or the
    /// only test that executes `chk_pricing_approval_reason` cannot be written.
    /// The approver is a different case — nothing tests a CHECK by posing an
    /// approve with no approver, because that request has no code to be
    /// refused with.
    pub reason: Option<String>,
    /// The **pricing** regions the deciding principal's grant covers, or the
    /// statement that the caller could not establish them.
    ///
    /// Never the authz-region claim; see
    /// [`crate::domain::approval::decision`]'s module doc, which also records
    /// that no document fixes how this grant is transported and that the
    /// plumbing is owed by whichever surface mounts the route.
    pub approver_regions: RegionGrant,
    /// Who is deciding, when, and under which request.
    ///
    /// The instant is the decision's — `decided_at` on the record is
    /// `stamp.recorded_at` and there is no second field for it, so the store and
    /// the trail cannot disagree about when a unit was decided.
    ///
    /// **The actor is the stamp's, not [`DecisionBy::decider`]'s**, and the two
    /// answer different questions: `decider` says whether the act exercises
    /// *approver* authority (and is `None` on the guard's own void), while the
    /// stamp says which principal the platform authenticated. Every surface that
    /// reaches here has one.
    pub stamp: AuditStamp,
    /// Whether this caller may close a unit they did not submit
    /// (`inst-as-void`).
    ///
    /// [`Self::approver_regions`]'s shape and its reason: an authority the domain
    /// must judge and cannot establish, transported by the surface that can. The
    /// two non-void arms never read it.
    pub withdraw_authority: WithdrawAuthority,
}

/// One record, with the content its pin covers.
///
/// See [`ApprovalService::find`] for why the content is a re-derivation plus a
/// flag rather than a stored document.
///
/// It carries **no** region accessor, and it used to. The surface read the
/// change set's reach off one of these and handed it to `decide` as the
/// approver's grant, which put the two halves of the scope rule on two reads of
/// two different worlds; the synthesis now happens inside [`judge`] and this
/// type is what a reviewer is *shown*, not an input to what they are allowed to
/// decide.
#[derive(Clone, Debug)]
pub struct ApprovalDetail {
    /// The record as the store holds it.
    pub record: ApprovalRecord,
    /// The pinned subject as it stands now, or `None` when it can no longer be
    /// re-derived at all — the plan's draft published, or was abandoned.
    pub subject: Option<PinnedSubject>,
    /// Whether `subject`'s digest still equals `record.content_hash`. `false`
    /// when there is no subject, which is the strongest form of "moved".
    pub content_matches_pin: bool,
}

/// What an approval unit is *about*, re-derived.
///
/// # Why this is a sum type and not a second field
///
/// It was `Option<PlanShape>` while every unit on the plane reviewed a plan. G6
/// opened the second kind — D-10's always-material threshold-policy unit, whose
/// subject is a proposed [`ThresholdVersion`] and has no plan anywhere in it — and
/// the two are genuinely disjoint rather than one being a special case of the
/// other: they are re-derived from different stores, hashed under different domain
/// separators, and rendered to a reviewer as different documents.
///
/// Two `Option` fields would have carried the same information and lost the one
/// property that matters: **exactly one of them is present**. A record with both,
/// or with neither and `content_matches_pin: true`, is a state the store cannot
/// produce and the type should not be able to express, because the reader that
/// gets it wrong is the reviewer's document (D-61) and the failure is silent.
#[derive(Clone, Debug)]
pub enum PinnedSubject {
    /// A plan revision, a price unit or a window mutation — every unit whose
    /// subject is a plan, whatever names it.
    ///
    /// Boxed because [`PlanShape`] is two orders of magnitude the other variant's
    /// size and clippy's `large_enum_variant` is right about it: an
    /// `Option<PinnedSubject>` is returned and moved on every approval read.
    Plan(Box<PlanShape>),
    /// One proposed version of the tenant's approval-threshold policy.
    ThresholdPolicy(ThresholdVersion),
    /// One overlay revision (D-225).
    ///
    /// Boxed for [`Self::Plan`]'s reason at a smaller scale: the line set is
    /// unbounded, and this enum is returned and moved on every approval read.
    Overlay(Box<OverlayRevision>),
    /// A **bundle composition** at a plan revision (D-104).
    ///
    /// `Plan` with the revision's `row_version` and the composition beside it.
    ///
    /// The `row_version` is what the **pin** folds: a composition edit moves no
    /// field of the shape — D-104 exists because a `sum_of_parts` recomposition
    /// carries no price-row delta at all — but it does advance the revision.
    /// Re-deriving this subject as a plain `Plan` let an approve carry to a
    /// different component set.
    ///
    /// The composition is what the **reviewer reads**. D-61 requires the `GET` to
    /// return the pinned content rather than the hash, and a plan shape carries no
    /// component set and no revenue split — so an approver of the one act whose
    /// whole subject is third-party money was shown a document that says nothing
    /// about it. It rides here rather than being fetched by the surface because
    /// this is the read the pin was taken over.
    BundleComposition(Box<PlanShape>, RowVersion, Box<CompositionDraft>),
    /// A **material membership move** (`inst-mm-immediate`, `inst-mm-bulk`,
    /// D-215's `inst-mm-pending`).
    ///
    /// Not boxed, unlike [`Self::Plan`] and [`Self::Overlay`]: a
    /// [`MembershipMoveSet`] carries no plan shape and no line set, and its
    /// size is bounded by the request that proposed it rather than by
    /// anything this crate authors without limit.
    ///
    /// # Why this subject re-derives with **no store read at all**
    ///
    /// Every other variant is re-read from a store that can move between
    /// submit and decide — a plan's draft, an overlay's revision, a threshold
    /// version's rows. A membership move has no such store: `inst-mm-pending`
    /// settles that its whole payload travels *inside* the approval record's
    /// own `subject_ref`, so re-deriving it is decoding text that cannot have
    /// changed since the transaction that wrote it — there is no world for it
    /// to have moved in. See [`re_derive`]'s `Membership` arm.
    Membership(MembershipMoveSet),
    /// A proposed edit of one taxonomy value, read against the value as it
    /// stands (D-353). Like a membership move the proposal rides the record's
    /// own `subject_ref`; unlike it the **held** half is re-read from the store,
    /// which is what lets `content_matches_pin` answer `false` when the value
    /// moves under a submitted unit.
    TaxonomyValue(Box<TaxonomyValueChange>),
    /// A **mass-repricing run's batch approval** (`inst-bs-approval`, D-267).
    ///
    /// The run's own frozen `report` — its selector, adjustment, changeover
    /// instant and selected-row count — beside the `operation_id` its
    /// `subject_ref` names, and the pricing regions its **selected rows** sit
    /// on.
    ///
    /// # Why the report and not a plan shape
    ///
    /// A run spans plans by construction (D-134 makes the *plan* the
    /// transaction unit precisely because a run has several), so no one
    /// `PlanShape` is the document a reviewer of this unit is being shown. What
    /// they are being asked to approve is the run's parameters, which is what
    /// `frozen_report` renders and what
    /// [`repricing_run_content_hash`](crate::domain::approval::content_pin::repricing_run_content_hash)
    /// pins.
    ///
    /// # Why the regions are carried rather than answered empty
    ///
    /// [`PinnedSubject::regions`]'s own rule is *"regions live on a row's
    /// canonical scope key"*, and a run has rows — so the honest set is the one
    /// its journal names, resolved through those rows' keys. Answering the
    /// empty set (the reading a policy version and a membership move take, both
    /// of which genuinely have no rows) would be covered by **every** grant,
    /// and a region-restricted `FinanceReviewer` would silently gain the
    /// authority to approve a tenant-wide reprice. That is the scope rule
    /// deleted rather than applied.
    ///
    /// Not boxed: a `serde_json::Value` is one pointer-sized handle and the
    /// region set is bounded by the run's own key set, so this variant is not
    /// the `large_enum_variant` hazard [`Self::Plan`] is.
    BulkRun {
        /// The run's frozen parameters, exactly as `pricing_bulk_operation.report`
        /// holds them.
        report: JsonValue,
        /// The run this unit is about — the id its `subject_ref` carries.
        operation_id: Uuid,
        /// The regions the run's selected rows sit on.
        regions: BTreeSet<Region>,
    },
}

impl PinnedSubject {
    /// The digest of this subject under **its own** frozen encoding.
    ///
    /// The dispatch is here and nowhere else, so that the three places that
    /// compare a re-derivation against `record.content_hash` — the reviewer's read,
    /// the decision, and the publish route's authorization lookup — cannot pick
    /// different preimages for one subject. `content_pin`'s two functions take two
    /// different types, so the compiler already refuses to cross them; what this
    /// prevents is a *caller* handling only the variant it was written for.
    #[must_use]
    pub fn content_hash(&self) -> [u8; 32] {
        match self {
            Self::Plan(shape) => content_hash(shape),
            Self::ThresholdPolicy(version) => threshold_content_hash(version),
            Self::Overlay(revision) => overlay_content_hash(revision),
            Self::BundleComposition(shape, version, _) => bundle_content_hash(shape, *version),
            Self::Membership(set) => membership_content_hash(set),
            Self::TaxonomyValue(change) => taxonomy_value_content_hash(change),
            Self::BulkRun {
                report,
                operation_id,
                ..
            } => repricing_run_content_hash(report, *operation_id),
        }
    }

    /// The pricing regions this subject reaches, for the scope rule.
    ///
    /// **A policy version reaches none, and that is the existing rule rather than a
    /// new exemption.** Regions live on a row's canonical scope key and a threshold
    /// policy has no rows, so the set is empty — exactly as it already is for a pure
    /// plan-shape revision, which is no less material for it (D-115's trial
    /// stretched 7 → 90 days is one). An empty set is covered by every grant, so a
    /// region-restricted `FinanceReviewer` may decide a policy unit; the reading
    /// that makes that right is that the policy is tenant-wide, so there is no
    /// region a restricted reviewer would be reaching outside of.
    /// # Errors
    ///
    /// [`DomainError::InvalidRequest`] when a stored region value does not parse
    /// as a [`Region`] — a value the declare door should have refused.
    pub fn regions(&self) -> Result<BTreeSet<Region>, DomainError> {
        match self {
            Self::Plan(shape) | Self::BundleComposition(shape, ..) => Ok(regions_of(shape)),
            // **Neither a policy version nor a membership move reaches a
            // region, and that is one reading applied twice rather than two.**
            // A threshold policy has no rows and a payer's group membership
            // has no region axis at all — the customer-group taxonomy is
            // tenant-wide — so both answer the empty set, which every grant
            // covers: there is no region a restricted reviewer would be
            // reaching outside of by deciding either.
            Self::ThresholdPolicy(_) | Self::Membership(_) => Ok(BTreeSet::new()),
            // **A region value reaches exactly its own region.** Retiring `EU` or
            // moving its tax markers is a decision about `EU`, so a reviewer whose
            // grant stops short of it must not be the second principal on it; the
            // other three universes have no region axis and answer the empty set,
            // for the policy arm's reason.
            //
            // The parse is propagated rather than discarded. It used to end in
            // `.ok()`, on the premise that `ScopeValue::new` having admitted the
            // value made a failure unreachable — and that premise was false:
            // `ScopeValue::new` refuses only a blank, while `Region::new` also
            // refuses `KEY_SEPARATOR`. The two disagreed, and where they disagreed
            // the discarded error became an **empty** reach set, which is a subset
            // of every grant — so the one arm whose whole purpose is to narrow who
            // may be the second principal widened it to everybody instead. The
            // declare door now runs `Region::new` too, which is what makes this
            // arm's premise true; propagating is what keeps it true if that ever
            // stops being so.
            Self::TaxonomyValue(change) => {
                if change.proposal.class == TaxonomyClass::Region {
                    Ok(BTreeSet::from([Region::new(
                        change.proposal.value.as_str(),
                    )?]))
                } else {
                    Ok(BTreeSet::new())
                }
            }
            // The run's rows' own regions, resolved when the subject was
            // re-derived — see the variant's doc for why this is the existing
            // rule applied rather than a new one.
            Self::BulkRun { regions, .. } => Ok(regions.clone()),
            // **An overlay reaches a region exactly when it is scoped to one.** A
            // brand- or partner-scoped overlay has no region axis at all and answers
            // the empty set, which every grant covers — the same reading the policy
            // arm above takes, and right for the same reason: there is no region a
            // restricted reviewer would be reaching outside of.
            //
            // A `region`-scoped overlay is the case where that reading would be
            // wrong, and the parse is propagated for the taxonomy arm's reason.
            // It used to end in `.into_iter().collect()`, which on a `Result` keeps
            // the `Ok` and drops the `Err` — so an unparseable stored region became
            // the **empty** set, a subset of every grant, widening who may decide
            // the unit at exactly the point the arm exists to narrow it. That it is
            // guarded upstream by `inst-plv-scope` is why it has no reachable
            // caller today, not why it would be safe if it did.
            Self::Overlay(revision) => match revision.scope.value() {
                Some(value) if revision.scope.class() == ScopeClass::Region => {
                    Ok(BTreeSet::from([Region::new(value.as_str())?]))
                }
                _ => Ok(BTreeSet::new()),
            },
        }
    }

    /// The plan shape, when this subject is one.
    #[must_use]
    pub const fn plan(&self) -> Option<&PlanShape> {
        match self {
            Self::Plan(shape) | Self::BundleComposition(shape, ..) => Some(shape),
            Self::ThresholdPolicy(_)
            | Self::Overlay(_)
            | Self::Membership(_)
            | Self::BulkRun { .. }
            | Self::TaxonomyValue(_) => None,
        }
    }

    /// The threshold-policy version, when this subject is one.
    #[must_use]
    pub const fn threshold_policy(&self) -> Option<&ThresholdVersion> {
        match self {
            Self::ThresholdPolicy(version) => Some(version),
            Self::Plan(_)
            | Self::Overlay(_)
            | Self::BundleComposition(..)
            | Self::Membership(_)
            | Self::BulkRun { .. }
            | Self::TaxonomyValue(_) => None,
        }
    }

    /// The taxonomy-value edit, on a `taxonomy_value` unit; `None` on every other kind.
    #[must_use]
    pub fn taxonomy_value(&self) -> Option<&TaxonomyValueChange> {
        match self {
            Self::TaxonomyValue(change) => Some(change),
            _ => None,
        }
    }

    /// The composition, when this subject is a bundle composition unit.
    ///
    /// What D-61 owes a reviewer of a `bundleComposition` act, and what a plan
    /// shape cannot answer.
    #[must_use]
    pub const fn composition(&self) -> Option<&CompositionDraft> {
        match self {
            Self::BundleComposition(_, _, composition) => Some(composition),
            Self::Plan(_)
            | Self::ThresholdPolicy(_)
            | Self::Overlay(_)
            | Self::Membership(_)
            | Self::BulkRun { .. }
            | Self::TaxonomyValue(_) => None,
        }
    }

    /// The run's frozen report, when this subject is a repricing run's batch
    /// unit — what D-61 owes that unit's reviewer, and what no other accessor
    /// here can answer (a run spans plans, so [`Self::plan`] is `None` on it).
    #[must_use]
    pub const fn repricing_run(&self) -> Option<&JsonValue> {
        match self {
            Self::BulkRun { report, .. } => Some(report),
            Self::Plan(_)
            | Self::ThresholdPolicy(_)
            | Self::Overlay(_)
            | Self::BundleComposition(..)
            | Self::Membership(_)
            | Self::TaxonomyValue(_) => None,
        }
    }

    /// The overlay revision, when this subject is one.
    #[must_use]
    pub const fn overlay(&self) -> Option<&OverlayRevision> {
        match self {
            Self::Overlay(revision) => Some(revision),
            Self::Plan(_)
            | Self::ThresholdPolicy(_)
            | Self::BundleComposition(..)
            | Self::Membership(_)
            | Self::BulkRun { .. }
            | Self::TaxonomyValue(_) => None,
        }
    }
}

/// What the judgement transaction resolved to.
///
/// A value rather than a `Result`, because a refusal is **not** a failure of the
/// transaction: it is its answer, and the transaction **commits** it — which is
/// what lets the `deny` record be written inside [`judge`] rather than after it.
/// See Ruling 1 in this module's doc for the reversal this encodes.
///
/// The refused arm carries the approval id and nothing else. It used to carry
/// the whole record, for a caller that had to re-render the attempt outside the
/// transaction; that caller is gone, and the only thing left to do with a
/// refusal out here is name the unit in the error.
enum Outcome {
    Decided(Box<ApprovalRecord>),
    Refused {
        refusal: DecisionRefusal,
        approval_id: Uuid,
    },
}

/// The approval workflow over one database provider.
#[derive(Clone)]
pub struct ApprovalService {
    db: DBProvider<DbError>,
}

impl ApprovalService {
    /// Build the workflow over one database provider.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Open a pending unit over a plan's open draft revision, **pinning its
    /// content** (`inst-ap-open`, `inst-ap-pin`).
    ///
    /// The pin is taken from the subject re-derived inside this transaction, not
    /// from anything the caller supplies: a submitter who could name the digest
    /// could name one over content nobody is about to publish.
    ///
    /// # One pending unit per subject, and the withdraw that is its only escape
    ///
    /// A second submit over a plan that already holds a `submitted` unit is
    /// refused [`DomainError::PendingChangeUnitExists`] (409), naming the unit
    /// that holds it — `inst-co-single-pending`, and the reading
    /// `inst-as-void` depends on. Without it §4's withdraw path frees nothing,
    /// because nothing was ever held: "a `submitted` unit pins its scope key
    /// indefinitely … without a withdraw path the only escape was mutating the
    /// subject's content" is a sentence about a **pin**, and a store that
    /// accepted a second unit would leave two reviewers deciding two records
    /// over one revision, whose order of arrival then decides what publishes.
    ///
    /// The check runs **inside the transaction the insert runs in**, so it is
    /// not a hint: two concurrent submits serialize on the same read, and the
    /// loser sees the winner's row.
    ///
    /// **The residual race is closed by a constraint, and not by the primary key.**
    /// The primary key is `approval_id`, which the *caller* mints
    /// (`api::rest::publish` calls `Uuid::now_v7()` per request), so two concurrent
    /// submits over one plan carry two different primary keys and collide on
    /// nothing. Under `READ COMMITTED` both read a store with no pending unit, both
    /// insert, and both commit. What the primary key decides is a **retry of one
    /// submit** — a real property, and a different one.
    ///
    /// What closes it is `uq_pricing_approval_key_pending`, the partial unique index
    /// on the key register this call now writes: one canonical scope key, one
    /// `submitted` holder, enforced by the server. The loser is answered
    /// [`DomainError::PendingChangeUnitExists`] — the same code the check produces,
    /// deliberately, so a caller does not read two different refusals for one
    /// condition depending on which transaction committed first.
    /// `tests/postgres_approval_race.rs` settles it against two real writers;
    /// `sqlite::memory:` serializes them and can neither confirm nor refute it.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the plan has no open draft revision;
    /// [`DomainError::PendingChangeUnitExists`] when the plan already holds a
    /// pending unit; [`DomainError::ConcurrentMutation`] when `approval_id` is
    /// taken; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a fact only the caller holds: the scope and the tenant the \
                  gate compiled, the plan and the id the surface minted, the submitter's identity, \
                  the materiality verdict and the caller's instant. Folding them into a struct \
                  would name a request type no surface has yet - G5 mounts the route, and inventing \
                  its DTO here would be the shape guessed rather than derived"
    )]
    pub async fn submit(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let scope = scope.clone();
        // The submitter is the stamp's actor and the submission instant is its
        // `recorded_at`; `NewApproval` says why neither is a second argument.
        let now = stamp.recorded_at;
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<ApprovalRecord, DomainError, _>(move |txn| {
                Box::pin(async move {
                    let shape =
                        crate::infra::publish::assemble(txn, &scope, tenant_id, plan_id, now)
                            .await?;
                    if let Some(held) =
                        approval_repo::find_pending_for_plan(txn, &scope, tenant_id, plan_id)
                            .await
                            .map_err(|e| repo_failure(&e))?
                    {
                        return Err(DomainError::PendingChangeUnitExists(format!(
                            "plan {plan_id}: approval {} is still submitted over {}; decide it, \
                             or withdraw it to free the subject",
                            held.approval_id, held.subject_ref
                        )));
                    }
                    // The keys this revision's change set touches — the per-key half
                    // of `inst-co-single-pending`, which the plan prefix above cannot
                    // see: a window unit of the same plan reviews no revision, so it
                    // carries no plan-revision subject and only the register finds it.
                    let held_keys = touched_keys(&shape);
                    refuse_held_key(txn, &scope, tenant_id, &held_keys).await?;
                    let new = NewApproval {
                        approval_id,
                        tenant_id,
                        subject_ref: audit_repo::plan_revision_ref(plan_id, shape.revision),
                        subject_kind: AuditSubjectKind::PlanRevision,
                        content_hash: content_hash(&shape).to_vec(),
                        materiality,
                        held_keys,
                    };
                    approval_repo::open(txn, &scope, new, stamp)
                        .await
                        .map_err(|e| repo_failure(&e))
                })
            })
            .await;
        outcome.map_err(into_domain)
    }

    /// Open a pending unit over a **window mutation** of a published plan
    /// (`inst-co-single-pending`'s D-62/D-99 window unit).
    ///
    /// # Why a second entry point rather than a parameter on [`Self::submit`]
    ///
    /// The two units differ in more than their `subject_kind`. A plan-revision unit
    /// reviews an **open draft**, so its shape comes from `publish::assemble`, which
    /// answers `NotFound` when there is none; a window unit's ordinary subject is a
    /// *published* plan with no draft at all, so it resolves the plan's **current**
    /// revision exactly as `infra::window` does and for
    /// [`PlanPublishUnit::window_mutation`](crate::domain::publish::PlanPublishUnit::window_mutation)'s
    /// stated reason. Folding them would put a branch on the shape resolution inside
    /// one function and leave the caller naming which branch by passing a different
    /// id — which is two functions with extra steps.
    ///
    /// # What it holds, and what that is not
    ///
    /// **One key**: the canonical scope key of the window's price row. A window
    /// mutation can move only that key's interval set, so holding the plan's whole
    /// key set would make every key of a plan hostage to a review of one of them —
    /// the same argument `infra::window`'s module doc makes about refusing on a
    /// sibling key's pre-existing gap.
    ///
    /// **This does not mutate the window.** It opens the unit that reviews the
    /// mutation; `WindowService`'s three operations are what commit one, and they
    /// refuse outright while a unit holds the key.
    ///
    /// **Which surfaces call it.** `api::rest::windows` mounts all three window
    /// surfaces. `DELETE` and a shortening `PATCH` route through this function
    /// because D-62 makes those two always-material whatever the threshold says;
    /// `POST` and a **lengthening** `PATCH` consult `materiality::evaluate` against
    /// [`crate::infra::threshold::effective_policy`] and take this arm when the
    /// verdict is material. All four acts reach it.
    ///
    /// # The subject is resolved through the **price row**, not the window
    ///
    /// Not `window_repo::find(window_id)`, which cannot answer for the act that needs
    /// it most: a schedule's controlled arm opens its unit before the window row
    /// exists at all — D-62's arm writes *nothing* — so that lookup answers
    /// `NotFound` and the surface returns **404** for a window the caller has just
    /// been handed the id of. The price row is what carries the canonical scope key
    /// (and therefore the plan), it exists on all four acts, and it is the same
    /// resolution `infra::window` performs on its own schedule path.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] for an unknown price row, or a plan with no current
    /// revision; [`DomainError::PendingChangeUnitExists`] when a pending unit
    /// already holds the window's key or the window itself;
    /// [`DomainError::ConcurrentMutation`] when `approval_id` is taken;
    /// [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "`Self::submit`'s reason, and the same facts: the scope and the tenant the gate \
                  compiled, the window and the price row the act is about, the approval id the \
                  surface minted, the evaluator's verdict and the caller's stamp. The window and \
                  the row are two ids because a schedule's window does not exist yet and its row \
                  does - which is the whole content of the paragraph above"
    )]
    pub async fn submit_window_mutation_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        window_id: Uuid,
        price_id: Uuid,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
        subject_ref: &str,
    ) -> Result<ApprovalRecord, DomainError> {
        let now = stamp.recorded_at;
        let key = price_repo::load_scope_key(runner, scope, tenant_id, price_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| DomainError::NotFound {
                subject: "price row".to_owned(),
                id: price_id.to_string(),
            })?;
        let plan_id = key.plan_id();
        let revision = plan_repo::load_current(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| DomainError::NotFound {
                subject: "current plan revision".to_owned(),
                id: plan_id.to_string(),
            })?;
        let shape =
            crate::infra::publish::assemble_from(runner, scope, tenant_id, plan_id, revision, now)
                .await?;
        // Built by `infra::window::Planned::unit_subject_ref`, which is also what
        // `authorizing_unit` resolves against, so the subject a unit opens under and
        // the subject a retry looks for cannot drift. It names the **act**: a subject
        // naming only the window let an approval taken for one act authorize any other
        // on it, and for a schedule it carries no window id at all, that id being
        // minted per request and therefore unreproducible by the retry.
        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "window {window_id}: approval {} is still submitted over it; decide it, or \
                 withdraw it to free the subject",
                held.approval_id
            )));
        }
        let held_keys = BTreeSet::from([key.to_string()]);
        refuse_held_key(runner, scope, tenant_id, &held_keys).await?;
        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref: subject_ref.to_owned(),
            subject_kind: AuditSubjectKind::Window,
            // The **plan shape**, exactly as a plan-revision unit pins it. D-99 makes
            // windows plan facts, so what a reviewer of a window mutation is shown is
            // the plan whose intervals move — and `content_pin`'s preimage already
            // frames the window plane, since `v2`.
            content_hash: content_hash(&shape).to_vec(),
            materiality,
            held_keys,
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Open the **grandfathering horizon**'s unit (`inst-gs-bound`,
    /// `inst-gs-tighten`, `inst-mat-registered`).
    ///
    /// [`Self::submit_window_mutation_on`]'s shape over a price row, and the two
    /// differences are the design set's.
    ///
    /// **The subject kind is `price_unit`, not `window`.** What moves is a
    /// published row's content, which is exactly what that kind names — the same
    /// reading [`Self::submit_supersession_on`] takes, and for the same reason S5
    /// §6 gives: this crate mints no subject token the design set has not declared
    /// (D-204 clause (2)).
    ///
    /// **The subject string names the act**, `infra::grandfather::horizon_unit_ref`
    /// building it as `<plan>/<priceId>/grandfather-until/<prior>/<new>` — D-184's
    /// shape, so an approval taken for a tightening to one instant cannot authorize
    /// a tightening to another, and a reviewer's detail view says which transition
    /// they are deciding. The prior end is part of it for the same reason it is on
    /// a window act: a unit approved when the horizon read `T` must not complete
    /// after somebody else has moved it.
    ///
    /// The pin is the plan shape, for the window unit's reason, taken over the
    /// shape **this transaction assembled**.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the row or the plan's current revision is
    /// absent; [`DomainError::PendingChangeUnitExists`] when a unit already holds
    /// this act or this key; [`DomainError::ConcurrentMutation`] when `approval_id`
    /// is taken; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "`Self::submit_window_mutation_on`'s reason and very nearly its list: the runner \
                  and the compiled scope, the tenant, the row the horizon sits on, the minted \
                  approval id, the evaluator's verdict, the caller's stamp and the act's subject. \
                  The subject is passed rather than rebuilt so the string a unit opens under and \
                  the string a retry resolves against have one author"
    )]
    pub async fn submit_horizon_tightening_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_id: Uuid,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
        subject_ref: &str,
    ) -> Result<ApprovalRecord, DomainError> {
        let now = stamp.recorded_at;
        let key = price_repo::load_scope_key(runner, scope, tenant_id, price_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| DomainError::NotFound {
                subject: "price row".to_owned(),
                id: price_id.to_string(),
            })?;
        let plan_id = key.plan_id();
        let revision = plan_repo::load_current(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| DomainError::NotFound {
                subject: "current plan revision".to_owned(),
                id: plan_id.to_string(),
            })?;
        let shape =
            crate::infra::publish::assemble_from(runner, scope, tenant_id, plan_id, revision, now)
                .await?;
        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "price row {price_id}: approval {} is still submitted over this horizon \
                 tightening; decide it, or withdraw it to free the subject",
                held.approval_id
            )));
        }
        let held_keys = BTreeSet::from([key.to_string()]);
        refuse_held_key(runner, scope, tenant_id, &held_keys).await?;
        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref: subject_ref.to_owned(),
            subject_kind: AuditSubjectKind::PriceUnit,
            content_hash: content_hash(&shape).to_vec(),
            materiality,
            held_keys,
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Open the overlay unit in a transaction of this service's own.
    ///
    /// [`Self::submit_overlay_on`]'s `&self` half, and the split is this module's
    /// existing one rather than a new shape: the `_on` form runs inside a caller's
    /// transaction, this form is for a caller that has none. The overlay submit is
    /// the second kind — no route in this gear opens a transaction, because a route
    /// that did would be a second place transaction boundaries are decided.
    ///
    /// It is a transaction rather than a bare call because `approval_repo::open`
    /// appends the unit's audit record inside it: a unit that committed while its
    /// trail rolled back would leave `pricing_audit_log` answering "who submitted
    /// this" with nothing.
    ///
    /// # Errors
    /// [`Self::submit_overlay_on`]'s, unchanged.
    pub async fn submit_overlay(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        revision: &OverlayRevision,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let scope = scope.clone();
        let revision = revision.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<ApprovalRecord, DomainError, _>(move |txn| {
                Box::pin(async move {
                    Self::submit_overlay_on(
                        txn,
                        &scope,
                        tenant_id,
                        &revision,
                        approval_id,
                        materiality,
                        stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(into_domain)
    }

    /// Open the **overlay** unit `inst-pl-commit` promises (D-50, D-225).
    ///
    /// The fifth submit on this service and the first whose subject is neither a plan
    /// nor a fact about one. What that changes is not the shape — subject, pin, open —
    /// but the two things below, and both are decisions rather than omissions.
    ///
    /// # It holds **no** keys, and that is `inst-co-single-pending` read literally
    ///
    /// That rule binds *"at most one pending approval unit **of any kind** … whose
    /// change set touches the key"*, and the key it means is a **canonical scope
    /// key** — the held-key register's vocabulary is scope keys, and
    /// [`refuse_held_key`]'s refusal says "scope key {key}" in as many words. An
    /// overlay writes no price row and touches no scope key, so the rule does not
    /// reach it and a held key minted for it would put a string in that register
    /// which the register's own diagnostic would then misname.
    ///
    /// What guards it instead is the **subject ref**: one pending unit per overlay
    /// *revision*. Two pending units over one revision would leave two reviewers
    /// approving two contents whose order of arrival decides the overlay.
    ///
    /// **What is deliberately not guarded here** is two *different* overlays pending
    /// on one `(scope class, precedence)` slot. Both may be approved and the second
    /// publish is refused by `uq_pricing_price_overlay_precedence` — the read-then-index
    /// arrangement D-148 states, and the same posture Slice 9 already built for the
    /// unapproved path. Making it a unit-level refusal would need a register keyed on
    /// something other than a scope key, which is a decision the design set has not
    /// made.
    ///
    /// # The pin is the overlay's own, not a plan shape's
    ///
    /// The window and supersession units pin the **plan shape**, because D-99 makes
    /// windows plan facts and a supersession's subject is a published plan's key. An
    /// overlay has no plan — its lines may target several, or none — so there is no
    /// plan shape to pin, and [`overlay_content_hash`] frames the revision itself.
    ///
    /// # Errors
    /// [`DomainError::PendingChangeUnitExists`] when a unit over this revision is
    /// already submitted; [`DomainError::Db`] on a storage failure.
    pub async fn submit_overlay_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        revision: &OverlayRevision,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let subject_ref =
            audit_repo::overlay_revision_ref(revision.price_overlay_id, revision.revision);
        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "overlay revision {subject_ref}: approval {} is still submitted over it; decide \
                 it, or withdraw it to submit again",
                held.approval_id
            )));
        }
        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref,
            subject_kind: AuditSubjectKind::Overlay,
            content_hash: overlay_content_hash(revision).to_vec(),
            materiality,
            held_keys: BTreeSet::new(),
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Open the **supersession** unit (`inst-su-commit`, D-88).
    ///
    /// The sibling of [`Self::submit_window_mutation_on`] and deliberately its shape:
    /// same held-key refusal, same plan-shape content pin, same subject-then-key order.
    /// What differs is what the subject names and which token the record carries.
    ///
    /// # `price_unit`, not a token of its own
    ///
    /// S5 §6's `subject_kind` enumeration is `plan_revision | price_unit | window |
    /// overlay | membership | bundle | retirement | policy | bulk_batch`
    /// (`historical_import` left it with D-330's strike) — there is **no
    /// `supersession` member**, and this gear does not mint
    /// tokens the design set has not declared. (`chk_pricing_approval_subject_kind` is the shape when one
    /// *is* declared: a widening migration, and only once a writer exists.) A
    /// supersession's change set is one price row on one canonical scope key, which is
    /// exactly what `price_unit` names, so the record carries that and the **subject
    /// string** carries `supersession` — see
    /// [`supersession_unit_ref`](crate::infra::supersession::supersession_unit_ref) for
    /// why it also carries the changeover.
    ///
    /// # The pin is the plan shape, for the window unit's reason
    ///
    /// A reviewer of a supersession is shown the plan whose row and intervals move, and
    /// the shape is what `authorizing_unit` re-derives at commit — so an edit to any of
    /// the plan's rows between the approve and the commit re-opens the review rather
    /// than riding the old signature. It is taken over the shape **this transaction
    /// assembled**, never a second read.
    ///
    /// # Both refusals, and why the subject one comes first
    ///
    /// A pending unit over *this very act* is answered before a pending unit over the
    /// *key*, because the first is the caller's own duplicate submit and names the record
    /// to decide or withdraw, while the second is somebody else's work on the key. Same
    /// order, same reason, as the window unit's.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the plan has no current revision;
    /// [`DomainError::PendingChangeUnitExists`] when a unit already holds this act or
    /// this key; [`DomainError::ConcurrentMutation`] when `approval_id` is taken;
    /// [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "`Self::submit_window_mutation_on`'s reason and the same facts: the compiled \
                  scope and tenant, the key and the changeover that together name the act, the \
                  approval id the surface minted, the evaluator's verdict and the caller's stamp. \
                  The key and the changeover are two arguments because the subject is built from \
                  both and neither is derivable from the other"
    )]
    pub async fn submit_supersession_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        key: &crate::domain::scope_key::ScopeKey,
        changeover: OffsetDateTime,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let now = stamp.recorded_at;
        let plan_id = key.plan_id();
        let revision = plan_repo::load_current(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| DomainError::NotFound {
                subject: "current plan revision".to_owned(),
                id: plan_id.to_string(),
            })?;
        let shape =
            crate::infra::publish::assemble_from(runner, scope, tenant_id, plan_id, revision, now)
                .await?;
        let subject_ref =
            crate::infra::supersession::supersession_unit_ref(plan_id, key, changeover);

        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "this supersession of {key} at {}: approval {} is still submitted over it; decide \
                 it, or withdraw it to free the subject",
                format_rfc3339(changeover),
                held.approval_id
            )));
        }
        let held_keys = BTreeSet::from([key.to_string()]);
        refuse_held_key(runner, scope, tenant_id, &held_keys).await?;

        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref,
            subject_kind: AuditSubjectKind::PriceUnit,
            content_hash: content_hash(&shape).to_vec(),
            materiality,
            held_keys,
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Open the **grandfathering cutover's** unit (`inst-gc-api`,
    /// `inst-co-single-pending`, D-100).
    ///
    /// [`Self::submit_supersession_on`]'s shape with two differences, and both are
    /// the design set's rather than this function's.
    ///
    /// **The act names its selection** (D-28): the subject is
    /// `(planId, key-set hash, cutover instant)`, so a retry that adds or drops a
    /// key is a *different* act and does not find this unit. See
    /// [`cutover_unit_ref`](crate::infra::cutover::cutover_unit_ref) for why
    /// naming the selection is what protects the caller rather than what
    /// endangers them.
    ///
    /// **It pends two keys per selected key, not one.** A supersession is a change
    /// on one key; a cutover also *mints a generation*, and
    /// `inst-co-single-pending` names both — *"the `all_subscriptions` key and the
    /// **new generation's** key — prior generations are not pended"*. Without the
    /// first, a window mutation could be approved against the key mid-cutover;
    /// without the second, two cutovers of one plan at one instant would each read
    /// the generation as free and the loser would meet a partial-`UNIQUE`
    /// violation inside its commit rather than a refusal at submit. Prior
    /// generations are excluded **by construction**: the set is built from the
    /// keys this act touches, and a prior generation is not one of them.
    ///
    /// The generation keys are minted against an **empty** existing-generation
    /// list on purpose. Refusing a cutover onto an instant some generation already
    /// carries is *compose's* answer, where the plan's real generations have been
    /// read; here the key is wanted only for its rendering, and duplicating that
    /// refusal against a list this function has not read would answer it from less
    /// information than the caller already has.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the plan has no current revision;
    /// [`DomainError::PendingChangeUnitExists`] when this act is already
    /// submitted, or when any key it would pend is held;
    /// [`DomainError::InvalidRequest`] when the selection is empty or names more
    /// than one plan; whatever
    /// [`grandfathered_copy_key`](crate::domain::cutover::grandfathered_copy_key)
    /// refuses.
    #[allow(
        clippy::too_many_arguments,
        reason = "the same eight `submit_supersession_on` takes, with the selection in place of \
                  its one key: the runner and the scope, the tenant, what is being cut over, when, \
                  the minted approval id, the verdict and the stamp. None is derivable from the \
                  others, and grouping them into a request type here would move the list rather \
                  than shorten it — the surface's DTO is where that type belongs"
    )]
    pub async fn submit_cutover_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        selected: &[crate::domain::scope_key::ScopeKey],
        cutover_at: OffsetDateTime,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let now = stamp.recorded_at;
        let Some(first) = selected.first() else {
            return Err(DomainError::InvalidRequest(
                "a cutover selects at least one canonical scope key; an empty selection names no \
                 act"
                .to_owned(),
            ));
        };
        let plan_id = first.plan_id();
        // D-28's selector is *"the plan's keys"*, and the subject's first segment is
        // the plan id — `subject_aggregate` refuses a subject that is not
        // `<plan_id>/<rest>`. A selection spanning two plans has no such segment to
        // be named by, so it is one refusal here rather than a corrupt subject the
        // register would reject three layers down.
        if let Some(stray) = selected.iter().find(|key| key.plan_id() != plan_id) {
            return Err(DomainError::InvalidRequest(format!(
                "this cutover selects keys of two plans, {plan_id} and {}; one act cuts over one \
                 plan, and the approval subject is named by it",
                stray.plan_id()
            )));
        }

        let revision = plan_repo::load_current(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| DomainError::NotFound {
                subject: "current plan revision".to_owned(),
                id: plan_id.to_string(),
            })?;
        let shape =
            crate::infra::publish::assemble_from(runner, scope, tenant_id, plan_id, revision, now)
                .await?;
        let subject_ref = crate::infra::cutover::cutover_unit_ref(plan_id, selected, cutover_at);

        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "this cutover of plan {plan_id} at {}: approval {} is still submitted over it; \
                 decide it, or withdraw it to free the subject",
                format_rfc3339(cutover_at),
                held.approval_id
            )));
        }

        let mut held_keys = BTreeSet::new();
        for key in selected {
            for touched in crate::infra::cutover::cutover_held_keys(key, cutover_at, &[])? {
                held_keys.insert(touched.to_string());
            }
        }
        refuse_held_key(runner, scope, tenant_id, &held_keys).await?;

        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref,
            // `price_unit`, for `submit_supersession_on`'s reason: S5 §6 declares no
            // `cutover` member and this gear mints no token the design set has not.
            // The act rides the subject string.
            subject_kind: AuditSubjectKind::PriceUnit,
            content_hash: content_hash(&shape).to_vec(),
            materiality,
            held_keys,
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Open the **retirement** unit inside the caller's transaction
    /// (`inst-re-governed`, D-109).
    ///
    /// [`Self::submit_window_mutation_on`]'s shape, with two differences worth
    /// stating rather than leaving to be inferred.
    ///
    /// **The subject is the act, not the revision.** A retirement pinned under
    /// `plan_revision_ref` would let an approve taken for a *publish* of that
    /// revision authorize the retirement of the plan, which is the strongest
    /// possible confusion of two units: one ships a change, the other ends the
    /// plan and cannot be undone. [`retirement_unit_ref`] names the act and the
    /// revision it stands at, so a retry of the retirement finds its own unit and
    /// nothing else does.
    ///
    /// **It holds no scope key.** `inst-co-single-pending`'s register is about
    /// units that stage a row on a key; retirement stages nothing and moves no
    /// price row, so there is no key to hold and holding one would refuse an
    /// unrelated window mutation for the duration of a review.
    ///
    /// The content pin is the plan shape's, exactly as every other unit over a
    /// plan takes it: what a reviewer of a retirement is shown is the plan they
    /// are ending.
    ///
    /// # Errors
    /// [`DomainError::PendingChangeUnitExists`] when a unit is already open over
    /// this retirement; whatever `assemble_from` refuses; [`DomainError::NotFound`]
    /// when the plan has no current revision; [`DomainError::Internal`] on a
    /// storage failure.
    pub async fn submit_retirement_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let now = stamp.recorded_at;
        let current = plan_repo::load_current(runner, scope, tenant_id, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| DomainError::NotFound {
                subject: "current plan revision".to_owned(),
                id: plan_id.to_string(),
            })?;
        // The revision is read here rather than taken as a parameter: the caller
        // composed against this very transaction, so a passed one could only ever
        // agree - and a parameter that can only agree is one that can be passed
        // wrong.
        let revision = current.revision;
        let shape =
            crate::infra::publish::assemble_from(runner, scope, tenant_id, plan_id, current, now)
                .await?;
        let subject_ref = retirement_unit_ref(plan_id, revision);
        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "plan {plan_id}: approval {} is still submitted over its retirement; decide it, \
                 or withdraw it to free the subject",
                held.approval_id
            )));
        }
        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref,
            subject_kind: AuditSubjectKind::PlanRevision,
            content_hash: content_hash(&shape).to_vec(),
            materiality,
            held_keys: BTreeSet::new(),
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Open the **bundle composition** unit D-104 registers (`inst-ba-material`).
    ///
    /// [`Self::submit_retirement_on`]'s shape — an act over a plan, pinned on the
    /// plan shape, holding no key — with the differences below.
    ///
    /// **The subject is the act at its revision, not the revision.** A composition
    /// publish pinned under `plan_revision_ref` would let an approve taken for a
    /// *publish* of that revision authorize a component swap and a vendor re-split,
    /// which is the confusion `submit_retirement_on` refuses for the same reason.
    /// [`bundle_composition_unit_ref`] names the bundle and the revision it stands
    /// at, so a retry of the publish finds its own unit and nothing else does.
    ///
    /// **It holds no keys.** `inst-co-single-pending`'s register is keyed on
    /// canonical scope keys; a composition stages no price row of its own — a
    /// `sum_of_parts` recomposition carries no price-row delta at all, which is the
    /// very fact that made D-104 necessary — so there is no key to hold.
    /// `submit_overlay_on` states this at length and it applies here unchanged.
    ///
    /// **The pin is the plan shape, and it arrives rather than being re-derived.**
    /// The composition normalizes onto its absorber inside the plan, so what a
    /// reviewer of a vendor re-split is shown is the plan that split changes — and
    /// the caller has already assembled that shape to look for an approval over it.
    /// Assembling a second one here would be two answers to "what is being
    /// published", which is the very thing `infra::publish::assemble`'s own doc is
    /// generic over its runner to prevent. It is also why this takes no `PlanId`
    /// revision of its own: the subject is the **draft** the caller resolved, not
    /// the plan's current revision. Resolving the current one here was this
    /// method's first shape, copied from [`Self::submit_retirement_on`], and it
    /// refused every composition publish on a never-published plan — which is all
    /// of them, since a composition is authored on a draft.
    ///
    /// # Errors
    /// [`DomainError::PendingChangeUnitExists`] when a unit is already open over this
    /// composition; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "[`Self::submit`]'s reason, and one more that is specific to this unit: the \
                  content hash arrives rather than being re-derived, precisely so this method and \
                  its caller cannot answer 'what is being published' differently. Folding the \
                  arguments into a struct would either drop that parameter or name a request type \
                  the bundle surface does not have"
    )]
    pub async fn submit_bundle_publish_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        plan_revision: u64,
        approval_id: Uuid,
        content_hash: Vec<u8>,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let subject_ref = bundle_composition_unit_ref(plan_id, plan_revision);
        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "plan {plan_id}: approval {} is still submitted over its composition; decide it, \
                 or withdraw it to free the subject",
                held.approval_id
            )));
        }
        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref,
            subject_kind: AuditSubjectKind::PlanRevision,
            content_hash,
            materiality,
            held_keys: BTreeSet::new(),
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// [`Self::submit_bundle_publish_on`] in a transaction of its own.
    ///
    /// The unit's audit record is appended in the same transaction that opens it —
    /// `approval_repo::open`'s requirement rather than this caller's preference, so a
    /// unit that committed while its trail rolled back cannot leave
    /// `pricing_audit_log` answering "who submitted this vendor re-split" with nothing.
    ///
    /// # Errors
    /// Whatever [`Self::submit_bundle_publish_on`] refuses.
    #[allow(
        clippy::too_many_arguments,
        reason = "it forwards [`Self::submit_bundle_publish_on`]'s parameters unchanged; a \
                  narrower signature here would mean the transaction wrapper and the body \
                  disagreeing about what an open needs"
    )]
    pub async fn submit_bundle_publish(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        plan_revision: u64,
        approval_id: Uuid,
        content_hash: Vec<u8>,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<ApprovalRecord, DomainError, _>(move |txn| {
                Box::pin(async move {
                    Self::submit_bundle_publish_on(
                        txn,
                        &scope,
                        tenant_id,
                        plan_id,
                        plan_revision,
                        approval_id,
                        content_hash,
                        materiality,
                        stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(into_domain)
    }

    /// Open the **material membership-move** unit (`inst-mm-immediate`,
    /// `inst-mm-bulk`, `inst-mm-pending`), inside the caller's transaction.
    ///
    /// [`Self::submit_overlay_on`]'s shape, adapted to a subject with no store
    /// of its own: the pin is [`membership_content_hash`] rather than a
    /// re-derivation of a stored row, and `subject_ref` is
    /// `approval_repo::membership_move_subject_ref(set)` — the whole payload,
    /// not a pointer to one, per [`PinnedSubject::Membership`]'s doc.
    ///
    /// # It holds no keys, for [`Self::submit_overlay_on`]'s reason
    ///
    /// A membership move writes no price row and touches no canonical scope
    /// key, so `inst-co-single-pending`'s register — keyed on scope keys —
    /// does not reach it. What guards it instead is the subject ref: one
    /// pending unit per **exact proposed set**. Two units proposing
    /// overlapping-but-different sets over one payer are **not** guarded
    /// here — the same residual gap `submit_overlay_on`'s doc records for two
    /// different overlays on one precedence slot, and for the same reason: a
    /// register keyed on something other than a scope key is a decision the
    /// design set has not made for this subject either.
    ///
    /// # Errors
    /// [`DomainError::PendingChangeUnitExists`] when a unit over this exact
    /// set is already submitted; [`DomainError::Internal`] on a storage
    /// failure.
    pub async fn submit_membership_move_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        set: &MembershipMoveSet,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let subject_ref =
            approval_repo::membership_move_subject_ref(set).map_err(|e| repo_failure(&e))?;
        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "this membership move: approval {} is still submitted over it; decide it, or \
                 withdraw it to submit again",
                held.approval_id
            )));
        }
        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref,
            subject_kind: AuditSubjectKind::Membership,
            content_hash: membership_content_hash(set).to_vec(),
            materiality,
            held_keys: BTreeSet::new(),
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Apply an **approved** membership-move unit, atomically, inside the
    /// caller's transaction — `inst-mm-pending`'s "the commit applies the
    /// whole set atomically".
    ///
    /// One [`crate::infra::membership_publish::move_payer_in`] per proposal, in
    /// the set's own canonical order, all inside `runner`'s transaction: each
    /// call is already the atomic end-then-enroll D-09 requires for one payer,
    /// and running the whole set inside one transaction is what makes *all of
    /// them* land together or none do, which is the atomicity `inst-mm-pending`
    /// promises for the set as a whole. Each call requests its own registry
    /// handle — D-06's own batching coalesces the several requests into one
    /// `CatalogVersion` (§3's own note on the audit-only path, unchanged here),
    /// so a bulk move is several publish units rather than a hand-rolled single
    /// one.
    ///
    /// **No re-verification of the content hash.** Unlike a plan-shape unit,
    /// whose subject can move between approve and this call, a membership
    /// move's whole content is [`ApprovalRecord::subject_ref`] itself — there
    /// is nothing else it could have drifted from. [`independent_approver`] is
    /// still checked: the two-person rule is a fact about *who decided*, not
    /// about *what*, and a record written around the store (a migration, a
    /// restore) could still name one principal twice.
    ///
    /// # The replay guard, and why `overlays::submit_overlay`'s does not transfer
    ///
    /// `ApprovalState::Approved` is terminal — nothing in this crate moves a
    /// decided record — so a caller of `POST …/move` who retries after the
    /// approve (a fresh `Idempotency-Key`, per that route's own doc: a retry
    /// after approval is a new attempt) finds the **same** approved record
    /// every time and would re-enter this function for a proposal already
    /// applied.
    ///
    /// `overlays::submit_overlay`'s D-234 shape — the precedent this module's
    /// doc borrows for "open on first arrival, commit once approved" — is
    /// replay-safe because its commit checks `lifecycle_state != Draft` on a
    /// **stored row that already existed before the unit opened**: an
    /// overlay revision is authored first and reviewed second, so "already
    /// published" is a fact the row itself can carry. A membership move has
    /// no such row: `inst-mm-pending`'s whole premise is that nothing is
    /// written before approval, so there is no pre-existing row whose state
    /// this call could consult. The row *this call would create* is the only
    /// candidate witness, which is why the guard below reads the **payer's
    /// current intervals** rather than a lifecycle flag.
    ///
    /// For each proposal: if the payer already holds an interval on exactly
    /// `(group_value, effective_from)`, an earlier call already applied this
    /// proposal (the failure mode this closes: without the guard, a second
    /// call would try to end the row it just created at the instant it
    /// starts, and `end_membership` correctly refuses that as
    /// `MembershipIntervalEmpty` — fails closed, but with a confusing
    /// refusal rather than an idempotent answer). The existing row is
    /// reported back as the receipt, and the registry is asked again under
    /// [`crate::infra::membership_publish::move_request_id`]'s **same**
    /// deterministic id — idempotent by that function's own contract, so the
    /// caller gets back the same `CatalogVersion` handle rather than a
    /// second, orphaned request.
    ///
    /// # Errors
    /// [`DomainError::SelfApprovalForbidden`] when `approved` names one
    /// principal as both submitter and approver; [`DomainError::Internal`]
    /// when `approved` is not `subject_kind = membership` or its payload does
    /// not decode; whatever [`crate::infra::membership_publish::move_payer_in`]
    /// refuses, per proposal not already applied; [`DomainError::CatalogVersionUnavailable`]
    /// when the registry cannot be reached on a replayed proposal.
    pub async fn commit_membership_move_in(
        runner: &impl DBRunner,
        registry: &dyn CatalogVersionRegistryV1,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant_id: Uuid,
        approved: &ApprovalRecord,
        stamp: AuditStamp,
    ) -> Result<Vec<crate::infra::membership_publish::MembershipMoveReceipt>, DomainError> {
        independent_approver(approved)?;
        if approved.subject_kind != AuditSubjectKind::Membership {
            return Err(DomainError::Internal(format!(
                "approval {} is not a membership unit ({}); refusing to apply it as one",
                approved.approval_id,
                approved.subject_kind.as_str()
            )));
        }
        let set = approval_repo::subject_membership_move(approved).map_err(|e| repo_failure(&e))?;
        let mut receipts = Vec::with_capacity(set.proposals().len());
        for proposal in set.proposals() {
            if let Some(existing) = already_applied(runner, scope, tenant_id, proposal).await? {
                let pending = crate::infra::registry_deadline::request_version_now(
                    registry,
                    ctx,
                    &crate::infra::membership_publish::move_request_id(
                        tenant_id,
                        existing.membership_id,
                    ),
                )
                .await?;
                receipts.push(crate::infra::membership_publish::MembershipMoveReceipt {
                    ended: None,
                    enrolled: existing,
                    pending_ref: pending.pending_ref,
                });
                continue;
            }
            let new_membership_id = Uuid::now_v7();
            let receipt = crate::infra::membership_publish::move_payer_in(
                runner,
                registry,
                ctx,
                scope,
                tenant_id,
                proposal.payer_tenant_id,
                new_membership_id,
                proposal.group_value.clone(),
                proposal.effective_from,
                stamp,
            )
            .await?;
            receipts.push(receipt);
        }
        Ok(receipts)
    }

    /// One page of the tenant's records, for the reviewer's queue.
    ///
    /// No subject is re-derived here, deliberately: a page of 100 would be 100
    /// plan assemblies, and the queue's job is to say *which* units want a
    /// decision. The pinned content rides [`ApprovalService::find`], which is
    /// the surface a reviewer opens before deciding — see its doc for D-61.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    /// Open the **taxonomy-value edit** unit (D-353), inside the caller's
    /// transaction — [`Self::submit_membership_move_on`]'s shape: the proposal is
    /// the subject (`approval_repo::taxonomy_value_subject_ref`), the pin is
    /// [`taxonomy_value_content_hash`] over the proposal **and the value as held**,
    /// and no scope key is held (a taxonomy value is not a price row).
    ///
    /// # Errors
    /// [`DomainError::PendingChangeUnitExists`] when a unit over this exact
    /// proposal is already submitted; [`DomainError::Internal`] on a storage
    /// failure.
    pub async fn submit_taxonomy_value_on(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        change: &TaxonomyValueChange,
        approval_id: Uuid,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<ApprovalRecord, DomainError> {
        let subject_ref = approval_repo::taxonomy_value_subject_ref(&change.proposal)
            .map_err(|e| repo_failure(&e))?;
        if let Some(held) = approval_repo::find_pending_for_subject(runner, tenant_id, &subject_ref)
            .await
            .map_err(|e| repo_failure(&e))?
        {
            return Err(DomainError::PendingChangeUnitExists(format!(
                "this edit of `{}` in the {} taxonomy: approval {} is still submitted over it; \
                 decide it, or withdraw it to submit again",
                change.proposal.value, change.proposal.class, held.approval_id
            )));
        }
        let new = NewApproval {
            approval_id,
            tenant_id,
            subject_ref,
            subject_kind: AuditSubjectKind::TaxonomyValue,
            content_hash: taxonomy_value_content_hash(change).to_vec(),
            materiality,
            held_keys: BTreeSet::new(),
        };
        approval_repo::open(runner, scope, new, stamp)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Apply an **approved** taxonomy-value edit, inside the caller's
    /// transaction — [`Self::commit_membership_move_in`]'s shape.
    ///
    /// The pin is re-derived here and compared to the record's: the value as it
    /// stands **now** must still be the one the reviewer saw, or the commit is
    /// refused `APPROVAL_CONTENT_MISMATCH` — the unit was decided over a document
    /// that no longer exists. The two guards run again, because a row may have
    /// been published against the value between approve and commit.
    ///
    /// # Errors
    /// [`DomainError::ApprovalContentMismatch`] when the held value moved or is
    /// gone; [`DomainError::TaxonomyValueInUse`] when a guard refuses now;
    /// [`DomainError::Internal`] when the record is not a taxonomy-value unit or
    /// on a storage failure.
    pub async fn commit_taxonomy_value_in(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        approved: &ApprovalRecord,
        stamp: AuditStamp,
    ) -> Result<TaxonomyEntry, DomainError> {
        independent_approver(approved)?;
        if approved.subject_kind != AuditSubjectKind::TaxonomyValue {
            return Err(DomainError::Internal(format!(
                "approval {} is not a taxonomy-value unit ({}); refusing to apply it as one",
                approved.approval_id,
                approved.subject_kind.as_str()
            )));
        }
        let proposal =
            approval_repo::subject_taxonomy_value(approved).map_err(|e| repo_failure(&e))?;
        let held = crate::infra::storage::repo::taxonomy_repo::find_value_on(
            runner,
            scope,
            tenant_id,
            proposal.class,
            &proposal.value,
        )
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| {
            DomainError::ApprovalContentMismatch(format!(
                "approval {} was decided over `{}` in the {} taxonomy, which is no longer \
                 declared",
                approved.approval_id, proposal.value, proposal.class
            ))
        })?;
        let change = TaxonomyValueChange { proposal, held };
        if taxonomy_value_content_hash(&change).as_slice() != approved.content_hash.as_slice() {
            return Err(DomainError::ApprovalContentMismatch(format!(
                "approval {} was decided over `{}` in the {} taxonomy as it then stood; the \
                 value has moved since, so the document the reviewer saw is not the one this \
                 commit would change",
                approved.approval_id, change.proposal.value, change.proposal.class
            )));
        }
        let next = change.next();
        let class = change.proposal.class;
        let report = crate::infra::storage::repo::taxonomy_repo::judge_value_patch(
            runner,
            tenant_id,
            class,
            &change.held,
            &next,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        if let Some(violation) = report.violations.first() {
            return Err(DomainError::TaxonomyValueInUse(violation.detail.clone()));
        }
        crate::infra::storage::repo::taxonomy_repo::write_value_patch(
            runner,
            scope,
            tenant_id,
            class,
            &change.held,
            &next,
            Some(approved.approval_id),
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        Ok(next)
    }

    /// D-355: commit an edit whose value **nothing published names**, with no
    /// second principal — the direct twin of [`Self::commit_taxonomy_value_in`].
    ///
    /// Re-judges the guards **inside this transaction** (a retirement that became
    /// referenced between the handler's read and here is refused
    /// `TAXONOMY_VALUE_IN_USE`), then writes the row and the audit record with
    /// **`approval_ref = None`**: the edit ran under no unit, exactly as a
    /// declaration does. The governance predicate itself is **not** re-checked
    /// here — that snapshot race is the one D-243 already declines to serialise
    /// for the guard's own counts.
    ///
    /// # Errors
    /// [`DomainError::TaxonomyValueInUse`] when a guard refuses the edit, and
    /// [`DomainError::Internal`] when the store fails.
    pub async fn commit_taxonomy_value_direct_in(
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        change: &TaxonomyValueChange,
        stamp: AuditStamp,
    ) -> Result<TaxonomyEntry, DomainError> {
        let next = change.next();
        let class = change.proposal.class;
        // **The `If-Match` premise, re-tested inside this transaction.**
        //
        // The handler compared `tag_of_value` against the header on a plain
        // connection, and `update_entry` below is an unconditional `UPDATE`
        // filtered on `(tenant_id, value)` over tables that carry no row
        // version — so the handler-side comparison alone is the shape
        // `taxonomy_repo::apply_replace` refuses in writing: two callers whose
        // reads both precede either commit each pass it, the second write
        // overwrites the first, and both callers are answered `200`. The
        // governed twin cannot reach that state because it re-reads the value
        // and re-derives the pin here; this arm has no unit to re-derive, so it
        // tests the premise directly.
        //
        // Compared against `change.held` — the value the caller's tag described
        // — and not against a freshly rendered tag, because `held` is what the
        // patch was authored over and what the audit record names as `before`.
        let standing = crate::infra::storage::repo::taxonomy_repo::find_value_on(
            runner,
            scope,
            tenant_id,
            class,
            &change.proposal.value,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        if standing.as_ref() != Some(&change.held) {
            return Err(DomainError::StaleVersion(format!(
                "`{}` in the {} taxonomy moved after you read it and before this edit could \
                commit; nothing was written. Re-read the value and author against the tag it \
                hands back",
                change.proposal.value, class
            )));
        }
        // **The governance premise, re-tested inside this transaction.**
        //
        // The handler chose this arm because `references_to` answered zero on a
        // plain connection: nothing published named the value, so D-355 makes the
        // edit the operator's own. That read and this write are not the same
        // transaction, and the gap is writable — a price row or an overlay scope
        // published against the value in between makes the very same edit one the
        // dual control owns. Without this re-test the request that lost the race
        // still commits on a single principal, which is the control failing open
        // rather than a stale read.
        //
        // `ConcurrentMutation` and not `StaleVersion`: nothing the caller
        // presented was wrong, and the whole remedy is to re-send — the retry
        // reads the reference that now exists and opens the unit.
        let references = crate::infra::storage::repo::taxonomy_repo::references_to(
            runner,
            tenant_id,
            class,
            &change.proposal.value,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        if crate::domain::taxonomy::edit_is_governed(references) {
            return Err(DomainError::ConcurrentMutation(format!(
                "`{}` in the {} taxonomy became referenced by a published row while this edit \
                 was committing, so it is no longer an edit one operator may make alone; \
                 nothing was written. Re-send it and it will open an approval unit",
                change.proposal.value, class
            )));
        }
        let report = crate::infra::storage::repo::taxonomy_repo::judge_value_patch(
            runner,
            tenant_id,
            class,
            &change.held,
            &next,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        if let Some(violation) = report.violations.first() {
            return Err(DomainError::TaxonomyValueInUse(violation.detail.clone()));
        }
        // **The one-principal commit, said out loud.**
        //
        // D-355 gives a taxonomy-value edit two doors, and which one a request
        // took is not recoverable from the audit record afterwards: the record
        // this writes names the same actor, value and states as a governed commit
        // does, and only the absent approval id distinguishes them — an absence,
        // and the governed twin writes that column from a unit the reader would
        // have to go and fetch. An operator asking "who changed the region
        // taxonomy without a second pair of eyes, and when" has no query. `warn`
        // rather than `info` for the reason `infra::approval`'s skipped-threshold
        // arms use it: a control that did not run is not routine traffic, even
        // when not running it was correct.
        tracing::warn!(
            tenant_id = %tenant_id,
            actor_principal_id = %stamp.actor_principal_id,
            correlation_id = %stamp.correlation_id,
            class = %class,
            value = %change.proposal.value,
            "bss-pricing: taxonomy value edited on a single principal; no published price row \
             or overlay scope names it, so D-355 makes the edit the operator's own and no \
             approval unit was opened"
        );
        crate::infra::storage::repo::taxonomy_repo::write_value_patch(
            runner,
            scope,
            tenant_id,
            class,
            &change.held,
            &next,
            None,
            stamp,
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        Ok(next)
    }

    pub async fn list(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        states: &[crate::domain::approval::ApprovalState],
        after: Option<Uuid>,
        limit: u64,
    ) -> Result<Vec<ApprovalRecord>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("bss-pricing: approval list: {e}")))?;
        approval_repo::list_page(&conn, scope, tenant_id, states, after, limit)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Pending units for plans the caller has already been authorized to read.
    /// Includes direct revision, window and price units; never indirect effects.
    ///
    /// # Why the scope is minted here and not taken from the caller
    ///
    /// Every other method on this service takes the caller's `&AccessScope`,
    /// because the caller's gate is the right constraint. This read cannot, and
    /// the reason is a mismatch between two resources: the plan reads pass the
    /// PDP a scope pinned `resource_id = plan_id`, while `pricing_approval`
    /// declares `resource_col = approval_id` — so under a real PDP that pin
    /// compiles to `approval_id IN (plan_id)` and matches nothing. A plan in
    /// review would read as *not* in review, in production, with every
    /// in-process test green: the test enforcer is flat and cannot express the
    /// pin at all.
    ///
    /// So the narrowing happens here rather than in the handler, which is also
    /// what `rest_authz::no_handler_can_build_an_access_scope_of_its_own`
    /// requires: *"the only producer of one on a REST path is
    /// `crate::authz::access_scope`, which **is** the gate"*. `infra` is where a
    /// narrowed scope is sanctioned — `infra::repricing`'s per-tenant writes do
    /// the same — and `for_tenant` only ever **narrows**: it constrains
    /// `OWNER_TENANT_ID` alone and can admit nothing the caller's own tenant
    /// does not own.
    ///
    /// What the caller still owes is its own gate. Pass only ids returned by a
    /// scoped plan read **after** `plan × read` has been answered. Only those
    /// plans' units are returned, and handlers expose only pending metadata;
    /// the unit's content stays behind `approval × read`.
    ///
    /// # Errors
    /// [`RepoError`] on a scope or storage failure.
    pub async fn pending_for_plans(
        &self,
        tenant_id: Uuid,
        plan_ids: &[crate::domain::scope_key::PlanId],
    ) -> Result<
        std::collections::BTreeMap<
            crate::domain::scope_key::PlanId,
            Vec<approval_repo::ApprovalRecord>,
        >,
        RepoError,
    > {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("bss-pricing: pending unit: {e}")))?;
        approval_repo::pending_for_plans(
            &conn,
            &AccessScope::for_tenant(tenant_id),
            tenant_id,
            plan_ids,
        )
        .await
    }

    /// Totals over all approvals visible under the caller's original read scope.
    ///
    /// # Errors
    /// [`RepoError`] on a scope, storage or corrupt persisted value failure.
    pub async fn counts(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
    ) -> Result<approval_repo::ApprovalCounts, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("bss-pricing: approval counts: {e}")))?;
        approval_repo::counts(&conn, scope, tenant_id).await
    }

    /// One `OData` page of the tenant's approval records.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on an
    /// invalid query or cursor.
    pub async fn list_odata(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<ApprovalRecord>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("bss-pricing: approval list: {e}")))?;
        approval_repo::list_odata(&conn, scope, tenant_id, query).await
    }

    /// One record **and the content its pin covers** — D-61's reviewability
    /// invariant.
    ///
    /// § 3's invariant is explicit that `GET /bss-pricing/v1/approvals/{id}`
    /// *"MUST return the **pinned content** the approval's `content_hash`
    /// covers (not the hash alone), so approval is never hash-blind even where
    /// the subject resource is read-restricted"*. A reviewer handed 32 bytes is
    /// signing a digest.
    ///
    /// **What the store cannot give, stated rather than implied.** §6's
    /// `pricing_approval` carries a `content_hash` and **no content column**, so
    /// there is no document to hand back: the only content this gear holds is
    /// the subject itself. So the subject is re-derived and
    /// [`ApprovalDetail::content_matches_pin`] says whether its digest still
    /// equals the pin. On a `submitted` unit the two agree or the TOCTOU guard
    /// has already voided the record, which is exactly the case a reviewer acts
    /// in; on a decided or voided one the flag is the honest answer to "is this
    /// still what was signed". Returning the re-derivation **without** the flag
    /// would be the hash-blind failure with extra steps — a document that looks
    /// like the pinned one and is not.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure, and on a `subject_ref`
    /// this crate cannot have written.
    pub async fn find(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        approval_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<ApprovalDetail>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("bss-pricing: approval read: {e}")))?;
        let Some(record) = approval_repo::read(&conn, scope, tenant_id, approval_id)
            .await
            .map_err(|e| repo_failure(&e))?
        else {
            return Ok(None);
        };
        // The record was read through the caller's approval scope. Its taxonomy
        // subject is tenant configuration, not a row keyed by approval_id.
        let subject_scope = taxonomy_effect_scope(scope, &record);
        let subject = re_derive(&conn, &subject_scope, tenant_id, &record, now).await?;
        let content_matches_pin = subject
            .as_ref()
            .map(PinnedSubject::content_hash)
            .is_some_and(|digest| digest.as_slice() == record.content_hash);
        Ok(Some(ApprovalDetail {
            record,
            subject,
            content_matches_pin,
        }))
    }

    /// The **approved** unit over exactly this subject **and this content**, if
    /// there is one.
    ///
    /// The publish route's question, and it is deliberately not "does this
    /// revision carry an approval": an approval is a decision about a change
    /// set, so one whose content has moved does not answer. See
    /// [`approval_repo::find_approved_for_content`] for why matching the subject
    /// alone would make a plan that was edited after its approval permanently
    /// unpublishable.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn approved_unit(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        subject_ref: &str,
        content_hash: &[u8],
    ) -> Result<Option<ApprovalRecord>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("bss-pricing: approved unit: {e}")))?;
        approval_repo::find_approved_for_content(&conn, scope, tenant_id, subject_ref, content_hash)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Decide a pending unit, or refuse and record the attempt.
    ///
    /// Approving a taxonomy-value unit also applies its pinned patch and writes
    /// the taxonomy audit in this transaction. Failure rolls back the decision
    /// too; callers must not re-send PATCH to apply a newly approved unit.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when no record in scope answers to the id.
    /// One of [`DomainError::SelfApprovalForbidden`],
    /// [`DomainError::RegionScopeDenied`],
    /// [`DomainError::ApprovalContentMismatch`],
    /// [`DomainError::ReasonRequired`] or [`DomainError::ApprovalNotPending`]
    /// when [`authorize_decision`] refuses — the first two **after** the `deny`
    /// record has been committed, by the same transaction that judged the
    /// attempt. [`DomainError::Internal`] on a storage failure, including a
    /// failure to write that record: the whole transaction rolls back with it,
    /// because an attempted violation this gear could not record must not be
    /// reported as a plain 403, or the operator believes the trail is complete.
    pub async fn decide(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        request: DecideRequest,
    ) -> Result<ApprovalRecord, DomainError> {
        let scope_for_tx = scope.clone();
        let request_for_tx = request.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Outcome, DomainError, _>(move |txn| {
                Box::pin(async move { judge(txn, &scope_for_tx, tenant_id, &request_for_tx).await })
            })
            .await;

        match outcome.map_err(into_domain)? {
            Outcome::Decided(record) => Ok(*record),
            Outcome::Refused {
                refusal,
                approval_id,
            } => Err(refusal_to_domain(refusal, approval_id)),
        }
    }
}

/// Scope for a taxonomy unit's already-authorized subject and audit effects.
///
/// Call only after reading the specific approval under the original PDP scope.
/// `approval × approve` authorizes the proposal stored in that record, not an
/// arbitrary config write. Keep the PDP's tenant constraints, and use exact
/// tenant/class/value predicates from the record for the effect. The approval
/// resource pin cannot be reused as a taxonomy tenant id or audit chain id.
/// `tenant_only` fails closed when no tenant constraint exists; it never mints
/// a new allow or asks the reviewer to hold the submitter's config-write role.
fn taxonomy_effect_scope(scope: &AccessScope, record: &ApprovalRecord) -> AccessScope {
    if record.subject_kind == AuditSubjectKind::TaxonomyValue {
        scope.tenant_only()
    } else {
        scope.clone()
    }
}

/// Write the `deny` record of one refused attempt, **through the judgement's own
/// runner**.
///
/// Not its own transaction, and the module doc's Ruling 1 is the reversal: the
/// judgement transaction commits on a refusal, so the record commits with the
/// judgement rather than after it, and there is no window in which the attempt
/// has been refused and nothing says so.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure. Returned rather than
/// swallowed, and it now rolls the judgement back with it — see
/// [`ApprovalService::decide`]'s error contract.
async fn record_denial(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: &ApprovalRecord,
    refusal: DecisionRefusal,
    request: &DecideRequest,
) -> Result<(), DomainError> {
    let aggregate = approval_repo::subject_aggregate(record).map_err(|e| repo_failure(&e))?;
    let entry = NewAuditEntry {
        tenant_id,
        // D-135: the attempt lands on the segment of the subject's **aggregate**, so
        // it sits beside the mutations it was about. `subject_aggregate` resolves it
        // per kind, and since G6 that is no longer always a plan: a `policy` unit's
        // aggregate is the tenant's threshold policy and its segment is
        // `audit_repo::policy_chain`.
        chain_id: aggregate.chain_id(),
        recorded_at: request.stamp.recorded_at,
        // The principal who **attempted** it, off the stamp: the platform
        // authenticated them, and unlike `decider()` the stamp is total. It
        // is also the same actor the successful decisions record, so the
        // trail names one principal one way across the whole plane.
        actor_principal_id: request.stamp.actor_principal_id,
        action: AuditAction::Deny,
        subject_kind: record.subject_kind,
        subject_ref: record.subject_ref.clone(),
        before_state: Some(json!({
            "approvalState": record.state.as_str(),
            "subjectRef": record.subject_ref,
            "submitterPrincipal": record.submitter_principal,
        })),
        after_state: Some(json!({
            "refusedWith": refusal.code(),
            "attemptedDecision": request.decision.decision().outcome().as_str(),
            "attemptedBy": request.decision.decider(),
        })),
        approval_ref: Some(record.approval_id),
        // D-178: the request's own, established at the gear's HTTP edge and
        // shared with every other record this call writes. Never a fresh mint —
        // this is the one record of the plane written on a path no other record
        // shares, so a mint here would go unnoticed by every equality the
        // authoring plane's tests can express.
        correlation_id: request.stamp.correlation_id,
    };
    audit_repo::append(runner, scope, entry)
        .await
        .map(|_| ())
        .map_err(|e| repo_failure(&e))
}

/// Read, re-derive, judge, and then either swap or record the refused attempt.
///
/// All inside one transaction, which is what makes the pin a pin: the digest is
/// computed over the subject as this transaction sees it, and the decision lands
/// against the record this transaction read. **Both** branches write through
/// that transaction — the allowed one swaps the unit and appends its trail, the
/// refused one appends the `deny` record — and both commit, because a refusal is
/// returned as `Ok` (see [`Outcome`] and the module doc's Ruling 1).
async fn judge(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    request: &DecideRequest,
) -> Result<Outcome, DomainError> {
    let record = approval_repo::read(runner, scope, tenant_id, request.approval_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| DomainError::NotFound {
            subject: "approval".to_owned(),
            id: request.approval_id.to_string(),
        })?;
    let effect_scope = taxonomy_effect_scope(scope, &record);

    // A withdraw judges nothing about the content and is exempt from both the
    // pin and the scope rule, so the subject is not re-derived for it at all —
    // which also means a withdraw still works when the subject is gone, and
    // `inst-as-void`'s "free the pinned scope key" escape hatch is not itself
    // blocked by the thing it exists to escape.
    let subject = if matches!(request.decision, DecisionBy::Void(_)) {
        None
    } else {
        re_derive(
            runner,
            &effect_scope,
            tenant_id,
            &record,
            request.stamp.recorded_at,
        )
        .await?
    };

    let change_set_regions = subject
        .as_ref()
        .map(PinnedSubject::regions)
        .transpose()?
        .unwrap_or_default();
    let current_content_hash = subject.as_ref().map(PinnedSubject::content_hash);
    // **Both sides of the scope rule come from this transaction's one
    // re-derivation.** Where the grant is untransported the synthesis happens
    // here rather than at the surface, and that placement is the fix rather
    // than tidiness: a surface that read the regions before calling was reading
    // a world a concurrent mutation could move, and the disagreement rendered as
    // `OutOfScope` — a 403 plus an attempted-authority-violation record against a
    // reviewer who had done nothing.
    let approver_regions = match approver_reach(&request.approver_regions) {
        // **`inst-ap-scope` is not evaluated here**, and the arm is written out
        // so that the unenforced case has a site rather than looking like a
        // covering grant. The alternative to synthesising is fail-closed, and
        // `RegionGrant::Untransported`'s doc records what it costs: an empty
        // grant refuses every change set carrying a single price row, so no unit
        // could be approved and no plan could publish, in the name of a rule
        // whose operand nobody can supply. What an operator can see instead is
        // the boot report `api::rest::approvals::report_region_grant_transport`
        // writes and the alarm it raises.
        ApproverReach::Unmeasured => change_set_regions.clone(),
        ApproverReach::Granted(regions) => regions,
    };

    let judgement = authorize_decision(&DecisionRequest {
        record_state: record.state,
        submitter_principal: record.submitter_principal,
        decision: request.decision,
        reason: request.reason.as_deref(),
        pinned_content_hash: &record.content_hash,
        current_content_hash,
        approver_regions: &approver_regions,
        change_set_regions: &change_set_regions,
        withdraw_authority: request.withdraw_authority,
    });

    if let Err(refusal) = judgement {
        // The attempted-violation record, **here**, inside the transaction that
        // judged it — which commits, because a refusal is this transaction's
        // answer and not its failure (Ruling 1). The two audited refusals are the
        // authority ones; the other three are races and malformed requests, and
        // recording those would bury the two that matter.
        if refusal.is_an_audited_violation() {
            record_denial(runner, &effect_scope, tenant_id, &record, refusal, request).await?;
        }
        return Ok(Outcome::Refused {
            refusal,
            approval_id: record.approval_id,
        });
    }

    // `approver()`, never `decider()`. The column is `approver_principal` and
    // `chk_pricing_approval_distinct_principals` reads it as one: a value equal
    // to `submitter_principal` is refused outright. A withdraw's decider **is**
    // the submitter (`inst-as-void`: "the submitter (or a `CatalogAdmin`)
    // explicitly withdraws it"), so writing the decider here made the one actor
    // §4 names for that path fail the CHECK — a 500 on the escape hatch §4 added
    // so a pending unit is not an indefinite lock. `approver()` is `None` on
    // every void, which is what `chk_pricing_approval_approver` exempts a
    // `voided` row for and what `void_pending_for_plan` has always written.
    //
    // **The identity is not lost.** The record does not
    // say who withdrew it — the column is named for a different role and the
    // CHECK refuses the submitter in it — so the withdrawer lands on the **trail**
    // instead: `approval_repo::decide` takes the stamp and writes a `withdraw`
    // record under `stamp.actor_principal_id`. That is what `inst-as-void`'s
    // "audited" asks for, and it is a place the distinctness rule has no opinion
    // about, which is the whole reason the trail is the right home for it.
    let decided = approval_repo::decide(
        runner,
        &effect_scope,
        tenant_id,
        request.approval_id,
        request.decision.decision(),
        request.decision.approver(),
        request.reason.clone(),
        request.stamp,
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    if decided.subject_kind == AuditSubjectKind::TaxonomyValue
        && decided.state == ApprovalState::Approved
    {
        // Re-read the held value and run domain guards inside this transaction.
        // A failure propagates as Err, rolling back the verdict and both audits.
        ApprovalService::commit_taxonomy_value_in(
            runner,
            &effect_scope,
            tenant_id,
            &decided,
            request.stamp,
        )
        .await?;
    }
    Ok(Outcome::Decided(Box::new(decided)))
}

/// The canonical scope keys a plan-revision unit's change set touches.
///
/// Off the **assembled** shape, so the set is the publish check's own row set —
/// `CANDIDATE_ROW_STATES`, the plan's `published` rows plus the `draft` rows a
/// publish would produce. That is the right set rather than a convenient one: the
/// unit under review *is* that publish, so what it holds is what it would freeze.
///
/// A `BTreeSet`, so two rows of one plan on one key (a supersession's predecessor
/// and successor, which `refuse_overlap` reads for the same reason) hold that key
/// once. Deduplicating in the store instead would make an ordinary shape collide
/// with its own register rows.
fn touched_keys(shape: &PlanShape) -> BTreeSet<String> {
    shape
        .rows
        .iter()
        .map(|record| record.scope_key.to_string())
        .collect()
}

/// The **approved** unit that authorizes an act, if one exists (D-62, D-88).
///
/// Subject **and** content, which is `approval_repo::find_approved_for_content`'s own
/// rule and the reason it takes both: an approval is a decision about a change set, so
/// a unit whose pinned content has since moved authorizes nothing. For both callers the
/// pinned content is the **plan shape** — D-99 makes windows plan facts and a
/// supersession's change set is one row of one plan — so an edit to the plan's rows
/// between the approve and the act re-opens the review rather than riding the old
/// signature.
///
/// The digest is taken over the shape **the caller's transaction assembled**, never a
/// second read.
///
/// # `pub(crate)` here rather than private to `infra::window`
///
/// It lived in `infra::window` while there was one caller. D-88's unit is the second and
/// asks the identical question, so it moved to the module that owns approvals rather
/// than being copied — the same argument [`refuse_held_key`] carries one function down,
/// and the one this whole file makes about a refusal with two authors. Nothing about the
/// body was window-specific: the subject is a string and the pin is a shape.
///
/// # A record naming one principal twice authorizes nothing
///
/// The same check `api::rest::publish::authorization_of` makes, for the reason its doc
/// gives: reading a stored `approver_principal` without comparing it would mean this
/// path trusts a column rather than the rule, and a row written around the store — a
/// migration, a restore, a later slice's writer — is exactly the case where the CHECK
/// did not run. It is spelled here rather than reused because that function lives in
/// `api::rest`, and an `infra` module reaching into a route module is a layer
/// inversion; three lines duplicated is the cheaper of the two.
///
/// # Errors
/// [`DomainError::SelfApprovalForbidden`] when the record names one principal as both
/// submitter and approver; [`DomainError::Internal`] on a storage failure or on an
/// `approved` record with no approver at all.
pub(crate) async fn authorizing_unit(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    shape: &PlanShape,
    subject_ref: &str,
) -> Result<Option<ApprovalRecord>, DomainError> {
    let pin = content_hash(shape);
    let found =
        approval_repo::find_approved_for_content(runner, scope, tenant_id, subject_ref, &pin)
            .await
            .map_err(|e| repo_failure(&e))?;
    let Some(record) = found else {
        return Ok(None);
    };
    independent_approver(&record)?;
    Ok(Some(record))
}

/// `inst-co-single-pending` over a set of keys: refuse if a pending unit holds one.
///
/// The **check**, whose value over `uq_pricing_approval_key_pending` is that it can
/// name the unit holding the key and the key it holds — an index violation can name
/// neither, and "something else is pending" without saying what is a 409 an operator
/// cannot act on. The index is what closes the race the check cannot see; the
/// migration's module doc has the pair.
///
/// **`pub(crate)` and the only place this sentence is written.** `infra::window`'s
/// mutation path asks the same question about the one key it moves, and it used to
/// build its own copy of the message — two spellings of one refusal, free to drift
/// apart in exactly the field an operator reads. The closing instruction is also what
/// `storage_tests` holds the *constraint* path's body to, so the sentence has one owner
/// on both paths.
///
/// # Errors
/// [`DomainError::PendingChangeUnitExists`] naming the holder and the key;
/// [`DomainError::Internal`] on a storage failure.
pub(crate) async fn refuse_held_key(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    keys: &BTreeSet<String>,
) -> Result<(), DomainError> {
    let held = approval_repo::find_pending_key_holder(runner, scope, tenant_id, keys)
        .await
        .map_err(|e| repo_failure(&e))?;
    match held {
        None => Ok(()),
        Some((holder, key)) => Err(DomainError::PendingChangeUnitExists(format!(
            "scope key {key}: approval {} is still submitted over {}; decide it, or withdraw it \
             to free the key",
            holder.approval_id, holder.subject_ref
        ))),
    }
}

/// [`ApprovalService::commit_membership_move_in`]'s replay witness: does
/// `proposal`'s payer already hold a membership on exactly its `(group_value,
/// effective_from)`?
///
/// `Some` means an earlier call already applied this proposal — the row it
/// would have created already exists. `None` means this proposal has not
/// been applied yet (the ordinary path), including the case where the payer
/// holds a *different* interval that merely happens to overlap in some other
/// way; only an exact match on the two fields this proposal actually names
/// counts, because those are the only two facts the proposal pins.
///
/// A linear scan over the payer's own intervals rather than a query filtered
/// on both columns: a payer's interval count is bounded by their own
/// membership history, never by tenant size, and `group_membership_repo`
/// carries no such filtered read today — adding one for a single call site
/// would be the wrong end of the trade.
async fn already_applied(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    proposal: &crate::domain::membership_change::MembershipMoveProposal,
) -> Result<Option<crate::infra::storage::repo::group_membership_repo::MembershipRow>, DomainError>
{
    let intervals = crate::infra::storage::repo::group_membership_repo::intervals_for_payer(
        runner,
        scope,
        tenant_id,
        proposal.payer_tenant_id,
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    Ok(intervals.into_iter().find(|row| {
        row.group_value == proposal.group_value && row.effective_from == proposal.effective_from
    }))
}

/// Open the D-10 unit over a proposed threshold-policy version, **inside the
/// caller's transaction**.
///
/// # Why a free function and not a third `ApprovalService` method
///
/// [`ApprovalService::submit`] and [`ApprovalService::submit_window_mutation`] each
/// own their transaction, because each resolves its own subject out of the store
/// before pinning it. A policy proposal cannot: the version it pins **does not
/// exist yet** — `threshold_repo::open_version` writes it — so the write and the
/// pin have to be in one transaction or the two halves of D-10 can commit apart. A
/// version with no unit is a proposal nobody is reviewing that
/// `infra::threshold::effective_policy` will never make effective; a unit whose
/// version failed to commit is a signature over nothing. So the transaction belongs
/// to `ThresholdService::propose` and this is the approval-plane half it calls, in
/// the shape `threshold_repo`'s own "runner-taking, not provider-holding" doc
/// anticipated.
///
/// # What it holds, and it is nothing
///
/// `held_keys` is empty, deliberately. A threshold policy touches no canonical
/// scope key — it is tenant-wide and prices nothing — so a proposal must not freeze
/// any of them. Holding the plan keys of a tenant while a reviewer thinks about a
/// threshold would make every publish in that tenant wait on a governance edit,
/// which is the opposite of what `inst-co-single-pending` protects. What serializes
/// two proposals instead is the pair below — the per-tenant reading of the same
/// rule, and the index that makes it a rule rather than a comparison.
///
/// # Two guards, and the read is not the guarantee (D-192 clause (2))
///
/// The check below is a **read-then-write**: it reads
/// [`approval_repo::find_pending_policy_unit`] and then inserts, so under
/// `READ COMMITTED` two proposals can both read a store with no pending policy unit
/// and both commit — and because `ThresholdService::propose` mints its version number
/// off `threshold_repo::latest_version` in the same transaction, both then land on
/// version *n*. `pricing_approval_threshold`'s key is `(tenant, version, currency)`
/// and permits disjoint currency sets, so the tenant is left holding one version
/// number over a row set no approver signed, on a table that refuses `UPDATE` and
/// `DELETE`.
///
/// `uq_pricing_approval_policy_pending` (`pricing_approval`) is what makes that state
/// unreachable, and the check **stays** — D-148's arrangement, for D-148's reason: the
/// read is the ordinary answer and the only one that can name the unit holding the
/// proposal open, which is what an operator acts on; the index is the invariant, which
/// no reader racing a writer can step through. The loser of the race is told the same
/// code by way of [`crate::infra::storage::RepoError::PendingPolicyUnitHeld`], so a
/// caller's remedy does not depend on which transaction committed first.
///
/// # Errors
/// [`DomainError::PendingChangeUnitExists`] (409) when the tenant already holds a
/// pending proposal — from the check below, or from the index when the check lost a
/// race; [`DomainError::Internal`] on a storage failure.
pub(crate) async fn open_policy_unit(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    approval_id: Uuid,
    version: &ThresholdVersion,
    materiality: JsonValue,
    stamp: AuditStamp,
) -> Result<ApprovalRecord, DomainError> {
    if let Some(held) = approval_repo::find_pending_policy_unit(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?
    {
        return Err(DomainError::PendingChangeUnitExists(format!(
            "approval-threshold policy: approval {} is still submitted over version {}; decide \
             it, or withdraw it to propose another",
            held.approval_id, held.subject_ref
        )));
    }
    let new = NewApproval {
        approval_id,
        tenant_id,
        subject_ref: audit_repo::policy_version_ref(version.version()),
        subject_kind: AuditSubjectKind::Policy,
        content_hash: threshold_content_hash(version).to_vec(),
        materiality,
        held_keys: BTreeSet::new(),
    };
    approval_repo::open(runner, scope, new, stamp)
        .await
        .map_err(|e| repo_failure(&e))
}

/// The plan revision a `<plan>/composition/<revision>` subject stands at, if the
/// ref is one.
///
/// [`is_retirement_unit`]'s shape, returning the revision rather than a `bool`
/// because this arm needs it: the pin folds that revision's `row_version`.
fn composition_unit_revision(subject_ref: &str) -> Option<u64> {
    let (_, rest) = subject_ref.split_once('/')?;
    rest.strip_prefix("composition/")?.parse().ok()
}

/// Does this subject ref name a **retirement** unit?
///
/// Matched on the act segment `retirement_unit_ref` writes. A retirement's
/// subject is the plan's current published revision, so its pin re-derives the
/// way a window's does rather than the way a draft's does.
fn is_retirement_unit(subject_ref: &str) -> bool {
    subject_ref
        .split_once('/')
        .is_some_and(|(_, rest)| rest.starts_with("retirement/"))
}

/// The shape of the plan **as it currently stands** — the assembly every unit
/// whose subject is a published fact re-derives under.
///
/// Extracted rather than copied into a third arm: three hand-maintained copies of
/// one resolution is how the `price_unit` arm came to disagree with the `window`
/// arm in the first place.
async fn current_revision_shape(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    now: OffsetDateTime,
) -> Result<Option<PinnedSubject>, DomainError> {
    let Some(revision) = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
    else {
        return Ok(None);
    };
    match crate::infra::publish::assemble_from(runner, scope, tenant_id, plan_id, revision, now)
        .await
    {
        Ok(shape) => Ok(Some(PinnedSubject::Plan(Box::new(shape)))),
        Err(DomainError::NotFound { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

/// The pinned subject as it stands **now**, or `None` if it is gone.
///
/// `None` rather than an error: a subject that has vanished is what
/// [`DecisionRefusal::ContentMismatch`] is for, and answering 404 would tell the
/// reviewer their approval record was missing when it is sitting in front of
/// them. Every other assembly failure is still an error — a storage fault is not
/// a mismatch.
async fn re_derive(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: &ApprovalRecord,
    now: OffsetDateTime,
) -> Result<Option<PinnedSubject>, DomainError> {
    // **The same assembly the pin was taken under, per subject kind**, and that is
    // the whole of what makes a re-derivation comparable to a pin rather than to a
    // different document. A plan-revision unit reviews an open draft, so
    // `assemble` is right and its `NotFound` means the draft is gone. A window
    // unit's subject is a *published* plan with no draft at all, so assembling that
    // way would answer `NotFound` for every window unit in existence — a
    // `content_matches_pin` of `false` on every read, and `APPROVAL_CONTENT_MISMATCH`
    // on every decision, for a subject nobody touched.
    //
    // The plan is resolved **inside** the three plan-bearing arms, never before the
    // match: a policy unit has no plan, so a resolution hoisted above the match
    // would answer `CorruptRow` — a 500 — on every read of every D-10 unit, for a
    // record that is perfectly well formed.
    match record.subject_kind {
        AuditSubjectKind::PlanRevision => {
            let plan_id = plan_of(record)?;
            // **A retirement unit carries this kind and resolves the other way**,
            // and the distinction is the same one the two arms below are already
            // built on: what decides the assembly is not the subject *kind* but
            // whether the subject is a draft being reviewed or a fact about the
            // plan as it currently stands. A retirement reviews a **published**
            // revision - it is the act of ending it - so a plan with no open
            // draft would assemble to `NotFound`, `content_matches_pin` would
            // answer `false`, and every retirement unit would be openable and
            // never approvable.
            //
            // That is the **third** instance of this defect in this function:
            // the window arm records it, the `price_unit` arm records finding it
            // again with "nothing caught it because no test
            // decided a supersession unit", and this one was caught by
            // `sqlite_publish_commit`'s `every_declared_action_has_a_production_
            // writer` - a census that *drives* every audited path rather than
            // grepping for it, so the unapprovable unit surfaced the moment a
            // retirement had to actually commit.
            //
            // Discriminated on the subject ref rather than on a new subject kind
            // because the kind is right: the thing being decided **is** a plan
            // revision, and `pricing_audit_log` should say so.
            if is_retirement_unit(&record.subject_ref) {
                return current_revision_shape(runner, scope, tenant_id, plan_id, now).await;
            }
            // **A composition unit carries this kind and re-derives with one more
            // fact.** Discriminated on the subject ref for the retirement arm's
            // reason — the thing being decided *is* a plan revision and
            // `pricing_audit_log` should say so — but its pin folds the revision's
            // `row_version`, because a composition edit moves nothing else the
            // shape carries. Re-deriving it as a plain `Plan` is what let an
            // approve authorize a component set nobody reviewed.
            if let Some(revision) = composition_unit_revision(&record.subject_ref) {
                let Some(row) =
                    plan_repo::load_revision(runner, scope, tenant_id, plan_id, revision)
                        .await
                        .map_err(|e| repo_failure(&e))?
                else {
                    return Ok(None);
                };
                return match crate::infra::publish::assemble(runner, scope, tenant_id, plan_id, now)
                    .await
                {
                    Ok(shape) => {
                        let composition = bundle_repo::load_composition_on(
                            runner, scope, tenant_id, plan_id, revision,
                        )
                        .await
                        .map_err(|e| repo_failure(&e))?;
                        Ok(Some(PinnedSubject::BundleComposition(
                            Box::new(shape),
                            row.row_version,
                            Box::new(composition),
                        )))
                    }
                    Err(DomainError::NotFound { .. }) => Ok(None),
                    Err(other) => Err(other),
                };
            }
            match crate::infra::publish::assemble(runner, scope, tenant_id, plan_id, now).await {
                Ok(shape) => Ok(Some(PinnedSubject::Plan(Box::new(shape)))),
                Err(DomainError::NotFound { .. }) => Ok(None),
                Err(other) => Err(other),
            }
        }
        // **`price_unit` resolves the *current* revision, not the open draft**, and
        // that grouping was a defect rather than a shortcut. It sat with
        // `plan_revision` above, on the reading that both are plan-content subjects —
        // and the only writer of a `price_unit` approval is
        // [`ApprovalService::submit_supersession_on`], whose subject is a **published**
        // plan's canonical scope key. A published plan nobody has revised has no open
        // draft, so `assemble` answered `NotFound`, this function answered `None`,
        // `content_matches_pin` answered `false`, and **every** supersession unit was
        // `APPROVAL_CONTENT_MISMATCH` on the first attempt to decide it — a unit that
        // could be opened and never approved, for a subject nobody had touched. That
        // is the exact failure the paragraph above describes for a window unit, reached
        // by the same route one arm along, and nothing caught it because no test
        // decided a supersession unit until D-88's orchestrator had one to decide
        // (found while building it).
        //
        // So it joins the window arm, and the two are one arm rather than two copies:
        // what they share is not the *kind* but the resolution — a subject that is a
        // fact about a plan as it currently stands.
        AuditSubjectKind::PriceUnit | AuditSubjectKind::Window => {
            let plan_id = plan_of(record)?;
            current_revision_shape(runner, scope, tenant_id, plan_id, now).await
        }
        // The proposed version's own rows, read back from the store that holds
        // them. It cannot move — `pricing_approval_threshold` refuses every UPDATE
        // and every DELETE — so the digest a reviewer verifies against is the one
        // taken at submit unless somebody **widened** the version by inserting a
        // currency it did not have, which is the one mutation the primary key
        // permits and the one this re-derivation is what catches.
        AuditSubjectKind::Policy => policy_version_of(runner, scope, tenant_id, record).await,
        // **The overlay revision the unit pins, re-read in the deciding
        // transaction.** This arm refused outright — "this crate opens none" — which
        // was true for exactly as long as nothing opened one and became a false claim
        // in code the moment `submit_overlay` existed. An un-approvable unit is a unit
        // that can be opened and never decided, which is worse than none.
        //
        // `None` means the revision is **gone**, and that is the right answer here
        // rather than an error: an absent subject is `ContentMismatch`'s business, not
        // a 404 about the record the reviewer is looking at. An overlay draft can be
        // discarded while its unit pends, and the reviewer must be told their subject
        // changed rather than told the store failed.
        AuditSubjectKind::Overlay => {
            let (price_overlay_id, revision) = overlay_of(record)?;
            let found = crate::infra::storage::repo::overlay_repo::load_on(
                runner,
                scope,
                tenant_id,
                price_overlay_id,
                revision,
            )
            .await
            .map_err(|e| repo_failure(&e))?;
            Ok(found.map(|record| PinnedSubject::Overlay(Box::new(record.content()))))
        }
        // `api::rest::repricing_runs::advance_on_verdict`
        // opens a `bulk_operation` unit on the material edge now, so a record of
        // this kind reaching `decide` can come from here — but nothing re-derives
        // its pinned content yet, because a repricing run's approval carries no
        // per-row pin to re-read against (`inst-bk-approval-subset`'s per-row
        // content hash is still unbuilt, see `api::rest::repricing_runs`'s content
        // pin doc). Refusing here rather than answering a `ContentMismatch` that
        // has nothing to compare against is the honest interim answer; the decide
        // surface for this kind is owed, not this arm's fix.
        //
        // `Err` rather than `Ok(None)`, and the earlier arms' own note explains why:
        // `None` means *the subject is gone*, which is not what "nothing to
        // re-derive yet" means.
        // **Paid here.** This arm returned `Err(Internal)` outright — the same
        // "storable and not resolvable" posture `Overlay` held before D-225 and
        // `Membership` held before the customer-group plane — and the cost was
        // larger than that phrasing suggests: `re_derive` runs on **every**
        // decision and on the reviewer's own read, so a `bulk_operation` unit
        // could be neither approved, nor rejected, nor even read in detail. Every
        // material mass-repricing run was therefore stranded in
        // `awaiting_approval` with a 500 on both decisions, its `run_id` spent
        // against `uq_pricing_bulk_operation_client_key` — which is precisely the
        // strand D-267 added `rejected` to close, reopened one door along. It
        // also made `api::rest::approvals::apply_approved_repricing_run`
        // unreachable code from the day it was written: `decide_record` fails
        // before the hook is consulted.
        //
        // This is the fourth instance of the defect this function's own comments
        // record three times — a unit that can be opened and never decided, which
        // is worse than none.
        //
        // **What is re-derived**: the run's frozen `report`, read back from
        // `pricing_bulk_operation` in the deciding transaction. That is the
        // document the pin was taken over
        // (`api::rest::repricing_runs::advance_on_verdict`), and the table freezes
        // a run's identity and provenance so that only `state`, `report` and
        // `completed_at` can move (D-260) — so the one mutation this
        // re-derivation can catch is a rewritten report, which is exactly the one
        // that would change what a reviewer signed for.
        //
        // `None` when the run is gone, which is `Overlay`'s reading rather than a
        // 404: an absent subject is `ContentMismatch`'s business and not an error
        // about the record the reviewer is looking at.
        AuditSubjectKind::BulkOperation => {
            let operation_id =
                approval_repo::subject_bulk_operation(record).map_err(|e| repo_failure(&e))?;
            let Some(run) = bulk_repo::read(runner, scope, tenant_id, operation_id)
                .await
                .map_err(|e| repo_failure(&e))?
            else {
                return Ok(None);
            };
            Ok(Some(PinnedSubject::BulkRun {
                report: run.report,
                operation_id,
                regions: run_regions(runner, scope, tenant_id, operation_id).await?,
            }))
        }
        // (Task 7 of the customer-group plane).
        // `ApprovalService::submit_membership_move_on` is the writer now, and
        // this arm is the re-derivation the module doc's own note anticipated:
        // "the payload lives in the approval record", so re-deriving it is
        // `approval_repo::subject_membership_move` decoding `subject_ref` —
        // **no store read**, unlike every other arm above.
        //
        // `Ok(Some(...))` unconditionally rather than the `Option` every other
        // arm threads: there is no store for the subject to have vanished
        // *from*. A well-formed record's payload cannot fail to decode — the
        // same content was validated by `MembershipMoveSet::new` when the
        // unit was opened, over exactly this text — so a decode failure here
        // means the row was written around the store, and
        // `subject_membership_move`'s own `RepoError::CorruptRow` is the
        // honest answer for that rather than a silent `None` that reads as
        // "the subject moved".
        AuditSubjectKind::Membership => {
            let set =
                approval_repo::subject_membership_move(record).map_err(|e| repo_failure(&e))?;
            Ok(Some(PinnedSubject::Membership(set)))
        }
        // D-353: the proposal decodes off the ref like a membership move's; the
        // **held** half is re-read from the taxonomy store, so a value that moved
        // (or was never there) under the unit is the `None` / mismatch every other
        // re-derived arm answers.
        AuditSubjectKind::TaxonomyValue => {
            let proposal =
                approval_repo::subject_taxonomy_value(record).map_err(|e| repo_failure(&e))?;
            let held = crate::infra::storage::repo::taxonomy_repo::find_value_on(
                runner,
                scope,
                tenant_id,
                proposal.class,
                &proposal.value,
            )
            .await
            .map_err(|e| repo_failure(&e))?;
            Ok(held.map(|held| {
                PinnedSubject::TaxonomyValue(Box::new(TaxonomyValueChange { proposal, held }))
            }))
        }
    }
}

/// The pricing regions a repricing run's **selected rows** sit on
/// ([`PinnedSubject::BulkRun`]'s region set, and the scope rule's operand).
///
/// The journal is the run's row set (`inst-mr-journal`) and a row's region is on
/// its canonical scope key, so this is the same resolution
/// [`regions_of`] performs over a plan shape, reached through the one store that
/// knows which rows a run named. **Every** journal row is read, not only the
/// `pending` ones: a reviewer's authority is judged against what the run is
/// *about*, and a row the apply has already decided is still part of that.
///
/// An empty answer is a real one — a run whose journal is somehow empty reaches
/// no region — and it is safe rather than permissive here for the reason
/// `RUN_SELECTOR_EMPTY` exists: `open_repricing_run` refuses an empty expansion
/// before any run is opened, so no unit is ever opened over one.
///
/// # Errors
/// Whatever the journal or the price repository refuses with.
async fn run_regions(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<BTreeSet<Region>, DomainError> {
    let rows = repricing_journal_repo::list_for_run(runner, scope, tenant_id, operation_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    let price_ids: Vec<Uuid> = rows.iter().map(|row| row.price_id).collect();
    Ok(
        price_repo::load_scope_keys_for_ids(runner, scope, tenant_id, &price_ids)
            .await
            .map_err(|e| repo_failure(&e))?
            .into_iter()
            .map(|(_, key)| key.region().clone())
            .collect(),
    )
}

/// The `(overlay, revision)` an overlay unit's `subject_ref` names.
///
/// The subject is the **revision** and not the overlay, which is what stops a pin
/// taken over revision 3 from authorizing a decision about revision 4 — so both
/// halves are parsed here rather than the id alone.
fn overlay_of(record: &ApprovalRecord) -> Result<(Uuid, u64), DomainError> {
    record
        .subject_ref
        .split_once('/')
        .and_then(|(id, revision)| Some((Uuid::parse_str(id).ok()?, revision.parse::<u64>().ok()?)))
        .ok_or_else(|| {
            DomainError::Internal(format!(
                "approval {} names the subject {:?}, which is not \
                 <price_overlay_id>/<revision>",
                record.approval_id, record.subject_ref
            ))
        })
}

/// Re-read the threshold version a `policy` unit pins.
///
/// `None` on a version whose rows are gone or which never existed, for
/// [`re_derive`]'s stated reason: an absent subject is `ContentMismatch`'s
/// business and not a 404 about the record the reviewer is looking at. An entry set
/// the constructor refuses reads the same way — it is content this gear can no
/// longer present as a version, which is exactly "no longer derivable".
async fn policy_version_of(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: &ApprovalRecord,
) -> Result<Option<PinnedSubject>, DomainError> {
    let version = record.subject_ref.parse::<u64>().map_err(|_| {
        DomainError::Internal(format!(
            "bss-pricing: approval {} names the policy subject {:?}, which is not a version number",
            record.approval_id, record.subject_ref
        ))
    })?;
    read_threshold_version(runner, scope, tenant_id, version)
        .await
        .map(|held| held.map(PinnedSubject::ThresholdPolicy))
}

/// One stored threshold version as the domain holds it, or `None`.
///
/// Shared by the re-derivation above and by [`crate::infra::threshold`]'s effective
/// -policy walk, because both have to build the **same** value out of the same rows
/// or the pin they compare stops meaning anything.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, and on a stored row whose
/// currency the domain refuses — the CHECK holds the shape, so a code that fails
/// [`CurrencyCode::new`] means the table was written around.
pub(crate) async fn read_threshold_version(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    version: u64,
) -> Result<Option<ThresholdVersion>, DomainError> {
    let stored = i64::try_from(version).map_err(|_| {
        DomainError::Internal(format!(
            "bss-pricing: threshold version {version} is not a value this store can hold"
        ))
    })?;
    let read = threshold_repo::read_version(runner, scope, tenant_id, stored).await;
    let Some(stored) = (match read {
        Ok(held) => held,
        // **An unreadable version is skipped, not raised.** `repo_failure` folds
        // `CorruptRow` and `Db` into one arm, so the discrimination has to happen
        // here, at the boundary that still has it.
        //
        // The store permits one mutation of an approved version — an `INSERT` of a
        // currency it did not have, since the key is `(tenant, version, currency)` —
        // and that widening can bring a second `effectiveFrom` with it, which is
        // what `read_version` refuses as a corrupt row. Two proposals that both
        // minted one version number leave exactly that state.
        //
        // Raising it was a fail-open dressed as a fault: `effective_version` walks
        // greatest-first with `?`, so **one** unreadable version aborted the whole
        // walk and every publish, every window mutation and the policy `GET`
        // answered 500 for that tenant — permanently, because the table refuses
        // `UPDATE` and `DELETE` and nothing could clear it. Skipping is the
        // direction the module doc promises for every other step of this walk: the
        // version is not what an approver signed, so it is not the policy, so the
        // tenant falls back to an older approved version or to none at all — and
        // none makes every change material.
        Err(RepoError::CorruptRow(why)) => {
            tracing::warn!(
                tenant_id = %tenant_id,
                version = stored,
                reason = %why,
                "bss-pricing: threshold version is unreadable and is skipped; the tenant's \
                 effective policy falls back to an older approved version, or to none"
            );
            None
        }
        Err(other) => return Err(repo_failure(&other)),
    }) else {
        return Ok(None);
    };
    let effective_from = stored.effective_from;
    let mut entries = Vec::with_capacity(stored.entries.len());
    for row in stored.entries {
        let currency = CurrencyCode::new(&row.currency).map_err(|_| {
            DomainError::Internal(format!(
                "bss-pricing: threshold version {version} holds the currency {:?}, which is not a \
                 code this gear can have written",
                row.currency
            ))
        })?;
        let basis = row.basis().ok_or_else(|| {
            DomainError::Internal(format!(
                "bss-pricing: threshold version {version} entry {currency} sets neither basis nor \
                 both, which chk_pricing_approval_threshold_basis refuses"
            ))
        })?;
        entries.push(ThresholdEntry { currency, basis });
    }
    // **D-185: a version the store returned with no entries is the tombstone.**
    // `read_version` answers `None` for a version nobody proposed and `Some` with an
    // empty entry set only when `pricing_approval_threshold_tombstone` carries a row
    // for it, so the emptiness here is a stored fact rather than an absence — which is
    // exactly why it must not go through `ThresholdVersion::new`, whose `NoEntries`
    // refusal would fold the authored retirement back into "no such version" and leave
    // the tenant on the thresholds they had approved the removal of.
    if entries.is_empty() {
        return Ok(Some(ThresholdVersion::tombstone(version, effective_from)));
    }
    // The version's own rows are ordered by currency in SQL, which is the order the
    // pin was taken over; `ThresholdVersion::new` keeps the caller's order for
    // exactly that reason.
    // **A refusal here is skipped, and skipping it silently is what makes it
    // dangerous.** `effective_version` walks greatest-first, so a version the
    // domain will not build drops the tenant onto an *older* approved version
    // whose thresholds may be higher — a change that should be material is judged
    // immaterial and commits on one principal. The `CorruptRow` arm above warns
    // for the same class of stored fault; this one is the same fact one layer in.
    match ThresholdVersion::new(version, effective_from, entries) {
        Ok(built) => Ok(Some(built)),
        Err(why) => {
            tracing::warn!(
                tenant_id = %tenant_id,
                version = %version,
                error = ?why,
                "bss-pricing: stored threshold version refused by the domain; skipping it"
            );
            Ok(None)
        }
    }
}

/// What the scope rule has to measure the approver against — or the fact that
/// nothing does.
///
/// # Why the untransported case carries no regions
///
/// [`Self::Unmeasured`] is deliberately not "the change set, again". A resolver
/// handed the change set can answer it as a covering grant, and then the
/// unenforced case is indistinguishable from an approver who genuinely holds
/// every region the change touches — to a reader, to a metric and to a test.
/// `inst-ap-scope` being unevaluated is a different fact from it having been
/// evaluated and passed, and only one of the two is worth alerting on
/// ([`PricingAlarm::RegionGrantUntransported`]).
///
/// [`PricingAlarm::RegionGrantUntransported`]: crate::domain::ports::metrics::PricingAlarm::RegionGrantUntransported
#[derive(Debug, PartialEq, Eq)]
enum ApproverReach {
    /// No grant reached this judgement, so the rule has no operand and is not
    /// evaluated. What the caller does with that is the caller's to name.
    Unmeasured,
    /// The regions the grant covers, exactly as handed over — a narrower grant
    /// still refuses.
    Granted(BTreeSet<Region>),
}

/// Resolve the deciding principal's grant, **off the grant alone**.
///
/// A free function and a pure one, so the choice it encodes is executable rather
/// than a sentence. It takes no change set, and that is the point rather than an
/// economy: with the change set in hand the untransported arm can return it and
/// the rule then passes by construction with nothing in the code saying so. The
/// synthesis still happens — D2 keeps it, because fail-closed refuses every
/// change set carrying one price row and no plan could publish — but it happens
/// at a named site in [`judge`] instead of behind a set this function invented.
fn approver_reach(grant: &RegionGrant) -> ApproverReach {
    match grant {
        RegionGrant::Untransported => ApproverReach::Unmeasured,
        RegionGrant::Explicit(regions) => ApproverReach::Granted(regions.clone()),
    }
}

/// The pricing regions the pinned change set touches.
///
/// Off the rows' canonical scope keys, which is the only place a pricing region
/// lives. A pure plan-shape revision touches none, and the empty set is covered
/// by every grant — see the scope rule's own tests.
fn regions_of(shape: &PlanShape) -> BTreeSet<Region> {
    shape
        .rows
        .iter()
        .map(|row| row.scope_key.region().clone())
        .collect()
}

/// The plan a `plan_revision` record's `subject_ref` names, in this module's
/// error vocabulary.
///
/// The parse itself is [`approval_repo::subject_plan`]'s and is **not** repeated
/// here: the store writes the trail against the same segment this module
/// re-derives the subject from, and two parses of one ref are two answers to
/// which plan a unit is about. Which plan that is stays the **record's** fact
/// rather than the request's — a caller who could supply it could point a
/// decision at another plan's subject.
///
/// **Only the plan-bearing kinds reach here**, and the caller is what guarantees
/// it: [`re_derive`] matches on the subject kind first and calls this from its
/// three plan arms. A `policy` unit resolves to
/// [`SubjectAggregate::Policy`](crate::infra::storage::repo::approval_repo::SubjectAggregate::Policy),
/// which has no plan, and the arm below turns that into a named `Internal` rather
/// than a panic — the same reading `subject_plan` gives an unparseable ref: a
/// record in a shape this crate cannot have written is an invariant breach, and it
/// must not present as a caller's mistake.
///
/// # Errors
/// [`DomainError::Internal`] on a ref this crate cannot have written — the
/// CHECKs admit any text, so an unparseable one means the table was written
/// around — and on a subject whose aggregate is not a plan.
fn plan_of(record: &ApprovalRecord) -> Result<PlanId, DomainError> {
    approval_repo::subject_aggregate(record)
        .map_err(|e| repo_failure(&e))?
        .plan()
        .ok_or_else(|| {
            DomainError::Internal(format!(
                "bss-pricing: approval {} is a {} unit, whose aggregate is not a plan",
                record.approval_id, record.subject_kind
            ))
        })
}

/// The subject a retirement unit is opened under (`inst-re-governed`).
///
/// The **act** and the revision it stands at, never the revision alone: a unit
/// keyed on the revision would be satisfied by an approve given for that
/// revision's publish, so the second principal D-109 requires could be one who
/// only ever agreed to ship a change.
///
/// Deterministic, so a retry of the retirement resolves the unit an earlier
/// attempt opened rather than opening a second one.
///
/// **The plan id comes first, and that is a store invariant rather than a
/// style.** [`approval_repo::subject_plan`] parses the aggregate off the head of
/// every `subject_ref` — the column's `CHECK`s admit any text, so a ref shaped
/// otherwise is not refused at write time and instead reads back as a
/// `CorruptRow` later. A first draft of this function put the act first and both
/// orchestrator cases failed on exactly that, from inside the approval plane
/// rather than on an assertion. [`crate::infra::cutover::cutover_unit_ref`]
/// already had the shape right: `<plan_id>/<act>/<discriminator>`.
#[must_use]
pub fn retirement_unit_ref(plan_id: PlanId, revision: u64) -> String {
    format!("{}/retirement/{revision}", plan_id.get())
}

/// The subject a bundle composition publish opens its unit over (D-104).
///
/// [`retirement_unit_ref`]'s shape and its reason: the act is named in the ref, so
/// an approve taken for a *publish* of this revision cannot authorize a component
/// swap and a vendor re-split of it.
///
/// # It leads with the **plan**, not the bundle
///
/// Every `plan_revision`-kind ref does, because [`plan_of`] resolves the subject by
/// parsing the leading segment — so a ref led by the bundle id makes `re_derive`
/// assemble the wrong plan, `content_matches_pin` answer `false`, and every
/// composition unit be openable and never approvable. That is the failure this ref
/// was written with and it is the **fourth** instance of the defect `re_derive`'s
/// own comments record three times; it was caught here only because a positive
/// control tried to approve one.
///
/// Nothing is lost by dropping the bundle id: a plan carries at most one bundle
/// (`a_second_bundle_on_one_plan_is_a_conflict`), so the plan and the revision name
/// the composition uniquely.
///
/// A composition reviews the **open draft**, so it deliberately does *not* get an
/// [`is_retirement_unit`]-style arm — `re_derive`'s default `assemble` is the
/// assembly its pin was taken under.
#[must_use]
pub fn bundle_composition_unit_ref(plan_id: PlanId, plan_revision: u64) -> String {
    format!("{}/composition/{plan_revision}", plan_id.get())
}

/// Void every pending unit pinned to `plan_id` — the TOCTOU guard,
/// `inst-ap-pin` / `inst-as-void`.
///
/// # The enumeration this depends on, and where each surface lands
///
/// Its correctness is a property of a *set of surfaces*, so the set is written
/// down here rather than left to be rediscovered by whoever adds the next one.
/// The authoring mutations that exist today:
///
/// | Surface | Voids? | Why |
/// |---|---|---|
/// | `create_plan` (`POST /plans`) | **no** | It mints a plan and its revision `0`. No approval can be pinned to a subject that did not exist when every open unit was opened. |
/// | `patch_plan` — the plan facet (`PATCH /plans/{id}`) | yes | Replaces content of the pinned draft revision. |
/// | `patch_plan` — the phase graph | yes | The phase set is hashed into the pin; a trial 7 → 90 days is D-115's own example of a material change with no price delta. |
/// | `patch_plan` — the add-on rule set | yes | Same: hashed, and a required-flip changes what the customer must buy. |
/// | `patch_plan` — the descriptor set | yes | Same: GL code and invoice template are hashed. |
/// | `abandon_plan_draft` (`POST /plans/{id}/abandon`) | yes | The subject ceases to exist. A unit left pending would hold the scope key open against a revision nobody can publish. |
/// | `create_price` (`POST /plans/{id}/prices`) | yes | Adds a row to the candidate set the pin covers. |
/// | `patch_price` (`PATCH …/prices/{id}`) | yes | Replaces a pinned row's content. |
/// | `delete_price` (`DELETE …/prices/{id}`) | yes | Removes a pinned row. |
/// | `open_revision` (`PATCH /plans/{id}` with no open draft, `api::rest::plans`) | **yes** | It inserts a revision row and copies three child tables, and it appends its own `Create` record rather than going through the rail — so it calls this directly. What it can find is only an *orphan*: a unit pinned to the draft that has since published. See its own doc. |
///
/// Two more paths write on a plan's audit segment. One is a void site with its
/// **own**, narrower void; the other is not one at all:
///
/// - **the publish commit** — it voids, but through
///   [`approval_repo::void_pending_for_subject`] rather than through this
///   function, and the difference is the whole decision. Voiding there does not
///   make an approved publish undo its own authorization: the authorizing record
///   is `approved`, and every void in this module selects `state = 'submitted'`.
///   What the commit voids is the orphan — a unit still `submitted` over the
///   revision the commit just froze, which no approve can ever carry to a publish
///   and which re-derives to `APPROVAL_CONTENT_MISMATCH` for the rest of its
///   life. It matches the **exact** `<plan_id>/<revision>`, not this function's
///   plan prefix, so it cannot reach a unit over another revision or another
///   subject of the same plan;
/// - **`plan_repo::create_draft` / `create_draft_on`** — the `create_plan`
///   surface above. The reason is the table's, not the rail's: no unit can be
///   pinned to a plan that did not exist.
///
/// # Why it is wired to the audit writers and to one call site, and what that
/// does **not** buy
///
/// Every authoring mutation of a plan's shape or rows lands its audit record
/// through exactly two functions — `plan_repo::record_revision_mutation` and
/// `price_repo::record_price_mutation` — because the phase's Global Constraint
/// is that there is **no unaudited mutating entry point**. Calling this from
/// those two covers nine surfaces at two call sites, which is nine fewer chances
/// to forget.
///
/// It does **not** follow that a surface added later cannot mutate a pinned
/// subject without passing through them. `open_revision` is the standing
/// counter-example: it writes its own record and is outside the rail, reached
/// from `PATCH /bss-pricing/v1/plans/{planId}` whenever the plan has no open
/// draft. The obligation is therefore on the reader and is stated as one: **a
/// path that appends its own audit record is a void decision to be taken**, and
/// it belongs in the table above with its answer either way.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure. It runs inside the
/// mutation's own transaction, so a failure here rolls the mutation back with
/// it — which is the answer: a mutation that could not void the approval it
/// invalidated must not commit, or a reviewer approves content that moved.
pub async fn void_pending_units_of(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    voided_at: OffsetDateTime,
) -> Result<u64, RepoError> {
    approval_repo::void_pending_for_plan(runner, scope, tenant_id, plan_id, voided_at).await
}

/// The principal whose signature makes `record` a **second** one — or why it is
/// not one.
///
/// One spelling of `inst-tp-distinct` as a *reader* applies it. Three call sites
/// read a stored approval and act on it — the publish commit, a window mutation,
/// and the threshold policy's effective version — and the first two each carried
/// their own copy of these two checks while the third carried **neither**, which is
/// the asymmetry this function exists to remove. The path that had no checks was
/// the one deciding whether every *other* change needs a reviewer at all.
///
/// **Neither breach is reachable through this crate's store, and the guard is
/// written anyway** — for ownership rather than for defence in depth, which is the
/// argument `api::rest::publish::authorization_of` already carried for its copy.
/// `chk_pricing_approval_distinct_principals` and `chk_pricing_approval_approver`
/// forbid both rows in either backend, so what is left is the case where those
/// CHECKs did not run: a migration, a restore, a later slice's writer. Reading
/// `approver_principal` without comparing it would mean these paths trust a column
/// rather than the rule.
///
/// The two refusals are deliberately different: a self-approval is an authority
/// violation and names the principal, an absent approver on an `approved` row is a
/// broken invariant and names the constraint that should have refused it.
///
/// # Errors
/// [`DomainError::SelfApprovalForbidden`] when the record names one principal as
/// both submitter and approver; [`DomainError::Internal`] when an `approved`
/// record carries no approver at all.
pub(crate) fn independent_approver(
    record: &crate::infra::storage::repo::approval_repo::ApprovalRecord,
) -> Result<Uuid, DomainError> {
    let approver = record.approver_principal.ok_or_else(|| {
        DomainError::Internal(format!(
            "approval {} is approved and names no approver; chk_pricing_approval_approver was \
             written around",
            record.approval_id
        ))
    })?;
    if approver == record.submitter_principal {
        return Err(DomainError::SelfApprovalForbidden(format!(
            "approval {}: the record names {approver} as both submitter and approver, so it \
             authorizes nothing",
            record.approval_id
        )));
    }
    Ok(approver)
}

/// Map a refusal onto the wire vocabulary.
///
/// One place, so the code a refusal carries and the status it renders as cannot
/// be spelled twice.
fn refusal_to_domain(refusal: DecisionRefusal, approval_id: Uuid) -> DomainError {
    let detail = format!("approval {approval_id}: {refusal}");
    match refusal {
        DecisionRefusal::SelfApproval => DomainError::SelfApprovalForbidden(detail),
        DecisionRefusal::OutOfScope => DomainError::RegionScopeDenied(detail),
        DecisionRefusal::ContentMismatch => DomainError::ApprovalContentMismatch(detail),
        DecisionRefusal::NotPending => DomainError::ApprovalNotPending(detail),
        DecisionRefusal::ReasonRequired => DomainError::ReasonRequired(detail),
        DecisionRefusal::ForeignWithdraw => DomainError::WithdrawForbidden(detail),
    }
}

fn into_domain(err: toolkit_db::secure::TxError<DomainError>) -> DomainError {
    err.into_domain(|infra| DomainError::Internal(format!("bss-pricing: approval: {infra}")))
}

#[cfg(test)]
#[path = "approval_tests.rs"]
mod approval_tests;
