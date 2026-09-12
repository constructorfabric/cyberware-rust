//! The approval-threshold policy as a *composition of two stores* — which version
//! a tenant proposed, and which of those an independent principal approved.
//!
//! `pricing_approval_threshold` holds versions and knows nothing about approval;
//! `pricing_approval` holds units and knows nothing about thresholds. Neither can
//! answer "what is this tenant's policy", and neither should: a `state` column on
//! the version table would be a second answer free to disagree with the record that
//! decided it, and a threshold column on the approval table would put policy content
//! in a store whose whole shape is `content_hash` and no content.
//!
//! # `effective_policy` is the fail-safe, executable
//!
//! [`effective_policy`] answers `None` for a tenant with no approved version, and
//! `None` is what makes [`crate::domain::materiality::evaluate`] answer
//! `Material { NoConfiguredThreshold }` — `inst-mat-failsafe`, G1. Every step of
//! the walk fails in that direction:
//!
//! * a tenant that has proposed nothing has no versions to walk;
//! * a version whose unit is still `submitted` is not approved, so it is skipped —
//!   **a proposal does not take effect while it is being reviewed**, which is the
//!   whole content of D-10;
//! * a version whose unit was rejected, voided or never opened is skipped for the
//!   same reason and by the same read;
//! * a version whose rows no longer hash to what its unit pinned is skipped, and
//!   that one is worth its own sentence. `pricing_approval_threshold` refuses every
//!   `UPDATE` and every `DELETE`, but its primary key is
//!   `(tenant_id, version, currency)` — so an `INSERT` of a **currency the version
//!   did not have** is the one mutation of an approved version the store permits.
//!   Matching on the pin rather than on the version number is what makes that
//!   widening take the version out of effect instead of quietly extending what an
//!   approver signed;
//! * a version whose `effective_from` has not arrived is skipped — see below.
//!
//! # The fail-safe is a state a tenant can **return** to (D-185)
//!
//! Everything above is about reaching `None` by *failing*. There is one way to reach
//! it by **authoring**: the tombstone, a version that positively says this tenant has
//! no thresholds. [`effective_version`] answers `Some` for it — it is in force, and
//! it is a version an approver signed — and [`effective_policy`] answers `None`,
//! because [`ThresholdVersion::policy`] reads an empty entry set as the absence
//! `ThresholdPolicy::of_entries` says it is. So the two functions disagree on purpose
//! and the disagreement is the decision: *there is a version, and it configures
//! nothing*.
//!
//! It is not a way round D-10. The retirement is minted with the next number, pinned,
//! and reviewed by the same always-material unit every other policy diff rides
//! ([`ThresholdService::retire`]), so the principal proposing that the two-person rule
//! come back for everything is not the principal who grants it. And the walk treats it
//! as an ordinary version in every other respect: a later entry version supersedes it,
//! a future-dated tombstone is skipped like any future-dated version, and its rows
//! cannot move, the tombstone table refusing `UPDATE` and `DELETE` like its sibling.
//!
//! **What a reader above this module must not do is read `effective_policy`'s `None`
//! as "no version".** They are two states now — a tenant that never configured one,
//! and a tenant that authored `none` — and they are only distinguishable through
//! [`effective_version`]. The policy `GET` is the one surface that has to tell them
//! apart, and it does: `effective: null` against `effective: { entries: [] }`.
//!
//! # Why the walk is greatest-first and stops at the first hit
//!
//! A tenant's policy is its **latest approved** version, not the union of its
//! approved versions: a version is a complete entry set, so a currency dropped
//! between version 3 and version 4 has to actually disappear, and a union would
//! resurrect it. Descending order plus an early return says exactly that, and it
//! also means the ordinary case — the newest version is the approved one — costs
//! one iteration.
//!
//! # `effective_from` is authored, pinned, rendered — and now **read** (D-188)
//!
//! This section reported a divergence rather than fixing it: [`effective_version`]
//! answered the greatest **approved** version and compared its `effective_from`
//! against nothing, so a version approved with an `effective_from` a year out
//! governed *today's* publishes. The column was never decoration — it is inside the
//! approval pin (`content_pin` hashes it, deliberately, so an operator cannot move
//! when approved thresholds start applying after a reviewer signed) and the `GET`
//! renders it — which made the gap sharper rather than softer: **a reviewer signed an
//! instant that had no reader**, and one who signed "these looser bars start in 2030"
//! had authorized them starting immediately.
//!
//! **The direction was fail-open**, the only step of this walk that did not fail safe:
//! between approval and `effective_from` the tenant's policy is the design set's
//! *unset* state, so every change should be material, and instead each was thresholded
//! against a bar nobody had reached yet.
//!
//! It is now the walk's third skip, and the fail-safe direction is preserved by
//! construction rather than argued: a version whose start is ahead drops out, so the
//! tenant falls back to an older approved version — or to none, and none makes
//! everything material. Skipping (rather than stopping) is what keeps the fallback
//! from becoming a second failure: a walk that returned `None` on meeting a
//! future-dated version would take away a policy the tenant already had, tightening
//! where nothing asked it to.
//!
//! **Where the clock comes from.** [`effective_version_at`] takes the instant, and
//! [`effective_version`] is the `OffsetDateTime::now_utc()` wrapper over it. The comparison is
//! against "now" at read time by design — the same rows answer differently before and
//! after the instant, which is what an authored start *means* — and taking it as an
//! argument is what keeps the walk idempotent: one clock, one answer, however many
//! times it is asked. The publish route passes the submit's own instant, so the
//! verdict and the stamp on the unit it opens are about one moment; `infra::window`
//! and the policy `GET` read the wall clock, which for those callers is the same
//! instant to within the call.
//!
//! # The `If-Match` premise is this module's to refuse (D-186)
//!
//! §5 gives the policy row the Idempotency cell *"`ETag` + approval unit"*, and the
//! `ETag` half lands here rather than at the transport. `api::rest::preconditions`
//! draws the line and this side of it is the store's: *that* module refuses a
//! request it cannot understand, and this one refuses a request whose premise has
//! moved. The comparison therefore happens **inside** [`ThresholdService::propose`]
//! and [`ThresholdService::retire`]'s transaction, recomputed from the same two
//! reads [`state_at`] composes, and before a version number is minted — a
//! comparison in the handler would read the world, decide, and hand the decision to
//! a statement that races it.
//!
//! The tag itself has exactly one producer, [`ThresholdState::tag`], because the
//! `GET` that issues it and the transaction that tests it must agree to the byte:
//! a second derivation is free to drift, and a drift in either direction is either
//! a precondition nobody can satisfy or one that refuses nothing.
//!
//! The walk is bounded by the number of versions a tenant has ever proposed, which
//! is a governance act performed by a human through a two-person review. If that
//! ever stops being a handful, the remedy is an index-backed join and not a cache:
//! a cached effective policy is a threshold that keeps applying after the unit
//! authorizing it was withdrawn.

use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::approval::content_pin::threshold_content_hash;
use crate::domain::audit::AuditStamp;
use crate::domain::concurrency::{PolicyTag, require_policy_match};
use crate::domain::error::DomainError;
use crate::domain::materiality::{ThresholdEntry, ThresholdPolicy, ThresholdVersion};
use crate::infra::approval::read_threshold_version;
use crate::infra::storage::repo::approval_repo::ApprovalRecord;
use crate::infra::storage::repo::{ThresholdEntryRow, approval_repo, threshold_repo};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// The tenant's threshold policy as the evaluator compares against it, or `None`.
///
/// Runner-taking, so a caller inside a transaction reads the same world it is
/// writing — which matters for the publish path: the materiality verdict and the
/// unit it opens must be about one policy, not one read before the transaction and
/// one inside it.
///
/// Reads the wall clock. A caller that already holds the instant its act is about —
/// the publish route holds the submit's — should call [`effective_policy_at`], so
/// that one act does not straddle two readings of "now".
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, or on a stored row the domain
/// refuses — see [`read_threshold_version`].
pub async fn effective_policy(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Option<ThresholdPolicy>, DomainError> {
    effective_policy_at(runner, scope, tenant_id, OffsetDateTime::now_utc()).await
}

/// [`effective_policy`] as of `now`.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, or on a stored row the domain
/// refuses — see [`read_threshold_version`].
pub async fn effective_policy_at(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<ThresholdPolicy>, DomainError> {
    Ok(effective_version_at(runner, scope, tenant_id, now)
        .await?
        .and_then(|version| version.policy()))
}

/// The greatest version whose unit an independent principal approved, whose content
/// still matches what they approved, **and whose `effective_from` has arrived**.
///
/// Reads the wall clock; see [`effective_version_at`] for the walk itself and the
/// module doc for the fail-safe argument behind each of its three skips.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, or on a stored row the domain
/// refuses.
pub async fn effective_version(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Option<ThresholdVersion>, DomainError> {
    effective_version_at(runner, scope, tenant_id, OffsetDateTime::now_utc()).await
}

/// [`effective_version`] as of `now`.
///
/// The module doc has the fail-safe argument for every step of the walk, including
/// the D-188 one this signature exists for: a version is **not effective before its
/// `effective_from`**, so the answer is a function of the clock and the clock is the
/// caller's rather than this function's.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, or on a stored row the domain
/// refuses.
pub async fn effective_version_at(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<ThresholdVersion>, DomainError> {
    let versions = threshold_repo::versions_desc(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    for stored in versions {
        let number = u64::try_from(stored).map_err(|_| {
            DomainError::Internal(format!(
                "bss-pricing: threshold version {stored} is negative, which \
                 chk_pricing_approval_threshold_version refuses"
            ))
        })?;
        let Some(version) = read_threshold_version(runner, scope, tenant_id, number).await? else {
            continue;
        };
        // **D-188, and it is a `continue` rather than a `break`.** A version whose
        // authored start is ahead of `now` is not this tenant's policy yet, so the
        // walk carries on to the next version down — which leaves the tenant on an
        // older approved version, or on none, and none makes everything material.
        // Stopping instead would take away a policy the tenant already has, which
        // fails in the *other* direction: a future-dated proposal would tighten
        // every act the moment it was approved.
        if !version.is_effective_at(now) {
            continue;
        }
        let approved = approval_repo::find_approved_for_content(
            runner,
            scope,
            tenant_id,
            &crate::infra::storage::repo::audit_repo::policy_version_ref(number),
            &threshold_content_hash(&version),
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        // The record has to be a *second* signature, not merely a row in
        // `approved`. This path used to accept `is_some()`, while both other
        // readers of `find_approved_for_content` re-checked the principals — the
        // asymmetry ran the wrong way, since a threshold policy is what decides
        // whether every other change needs a reviewer. See
        // `infra::approval::independent_approver` for why the guard is written
        // against rows this crate cannot produce.
        if let Some(record) = approved {
            crate::infra::approval::independent_approver(&record)?;
            return Ok(Some(version));
        }
    }
    Ok(None)
}

/// The threshold-policy surface: read the effective policy, and propose the next
/// version.
#[derive(Clone)]
pub struct ThresholdService {
    db: DBProvider<DbError>,
}

/// What a tenant's policy looks like from outside: the effective version, and
/// whether a proposal is waiting on a reviewer.
///
/// The two together because they are the two halves of one operator question — "is
/// my threshold set, and is my edit through yet" — and answering them from two
/// reads at two instants would let a `GET` show a version that had just been
/// superseded beside a proposal that had just been approved.
#[derive(Clone, Debug)]
pub struct ThresholdState {
    /// The greatest approved version, or `None` for a tenant that has never had one
    /// in force — which is the state `inst-mat-failsafe` is named for and not an
    /// error.
    ///
    /// `Some` carrying a version with **no entries** is the other unset state: an
    /// authored, approved tombstone (D-185). Both make every change material; only
    /// this field tells them apart, which is why the surface renders the version
    /// rather than collapsing it to a null.
    pub effective: Option<ThresholdVersion>,
    /// The unit reviewing a proposal, if one is open.
    pub pending: Option<ApprovalRecord>,
}

impl ThresholdState {
    /// The entity tag of this state (D-186).
    ///
    /// **The one producer of the rendering**, and that is the point rather than
    /// deduplication. The tag the `GET` emits and the tag a `PUT`'s transaction
    /// recomputes have to agree to the byte or the precondition is either
    /// unsatisfiable or vacuous, and the way that failure arrives is a second
    /// derivation that drifted. So both go through this method, over the same
    /// `ThresholdState` both already hold.
    ///
    /// It covers **both** fields, because both are served: a tag that moved only
    /// with `effective` would not change when a proposal opened, and a caller who
    /// read the policy before somebody else opened a proposal would be told their
    /// premise still held.
    #[must_use]
    pub fn tag(&self) -> PolicyTag {
        PolicyTag::of(
            self.effective.as_ref().map(ThresholdVersion::version),
            self.pending.as_ref().map(|unit| unit.approval_id),
        )
    }
}

/// The tenant's effective version and open proposal, read through one runner.
///
/// Extracted from [`ThresholdService::state`] so that the `GET` and the
/// comparison inside [`ThresholdService::propose`] compose the **same two reads**
/// rather than two arrangements of them. Runner-taking for
/// [`effective_policy`]'s reason: a caller inside a transaction has to read the
/// world it is about to write.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, or on a stored row the domain
/// refuses.
pub async fn state_at(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    now: OffsetDateTime,
) -> Result<ThresholdState, DomainError> {
    Ok(ThresholdState {
        effective: effective_version_at(runner, scope, tenant_id, now).await?,
        pending: approval_repo::find_pending_policy_unit(runner, scope, tenant_id)
            .await
            .map_err(|e| repo_failure(&e))?,
    })
}

/// The premise a policy `PUT` asserts, and the instant it is judged at (D-186).
///
/// **Two fields rather than two parameters, because they are one fact.** The tag
/// describes a representation, and this resource's representation is a function of
/// the clock — a version approved with an `effective_from` still ahead is not in
/// it (D-188). So a tag is only meaningful *as of* an instant, and passing the two
/// separately would let a call site compare against a wall-clock reading different
/// from the one its own act is stamped with. That straddle is exactly what
/// [`effective_version_at`]'s signature exists to prevent, and it would be
/// reintroduced here by two arguments nothing binds together.
#[derive(Clone, Debug)]
pub struct AssertedPolicy {
    /// The `ETag` the caller copied back out of the `GET`.
    pub tag: PolicyTag,
    /// The instant the whole act is about — the handler's `now`, the same one its
    /// [`AuditStamp`] carries.
    pub now: OffsetDateTime,
}

impl ThresholdService {
    /// Build the surface over one database provider.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// The tenant's effective policy and its open proposal, read together.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn state(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
    ) -> Result<ThresholdState, DomainError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<ThresholdState, DomainError, _>(move |txn| {
                Box::pin(async move {
                    state_at(txn, &scope, tenant_id, OffsetDateTime::now_utc()).await
                })
            })
            .await;
        outcome.map_err(into_domain)
    }

    /// Propose the next version, and open the D-10 unit that reviews it.
    ///
    /// One transaction, and the whole reason `threshold_repo` takes a runner: the
    /// version rows and the unit that pins them commit together or not at all. The
    /// version number is minted **inside** it off
    /// [`threshold_repo::latest_version`].
    ///
    /// **That is not by itself enough, and D-192 clause (2) is what closes it.** Two
    /// concurrent proposals under `READ COMMITTED` read the same `latest_version` and
    /// mint the same number, and `pricing_approval_threshold`'s key —
    /// `(tenant, version, currency)` — permits disjoint currency sets, so its primary
    /// key refuses only a *currency* the version already holds and admits the merge.
    /// What stops the pair reaching that point at all is
    /// `uq_pricing_approval_policy_pending`, the index behind
    /// [`crate::infra::approval::open_policy_unit`]'s check: at most one `submitted`
    /// policy unit per tenant, so at most one proposal is in flight to mint anything.
    ///
    /// **What that refusal reaches the caller as.** Not simply "the primary key
    /// refuses the loser", which reads as though the loser were told to retry: a key
    /// violation out of the *version* table arrives as
    /// [`crate::infra::storage::RepoError::Db`] and renders **500**, because that
    /// table's insert is not one of the writes `storage` classifies as contention.
    /// The one path here that *is* retriable is the **approval** insert — its primary
    /// key is `pricing_approval`'s, which `storage` maps to
    /// [`DomainError::ConcurrentMutation`] and 409, asserted by
    /// `rest_threshold_policy::a_policy_unit_whose_id_is_taken_is_a_retriable_conflict_and_not_a_storage_failure`.
    /// Both directions are safe — nothing partial commits — and the difference is what
    /// an operator is told to do about it, which is why it is written down rather than
    /// smoothed over. **Still open, and deliberately so**: the version table's key is
    /// now unreachable from this path — the mint guard above admits one proposal at a
    /// time, so no second caller is here to collide on a number — and classifying a
    /// violation of it would be classifying a write nothing can perform. It becomes a
    /// live question again only if `open_version` acquires a second caller, which is
    /// what the sentence is left here for. The approval table's own key *is* classified,
    /// beside the mint guard, and `storage::policy_guard_or_contention` is where the two
    /// are told apart.
    ///
    /// The diff is **not applied**: the rows land in a version nothing points at
    /// until [`effective_version`] finds its unit approved. That is D-10 — *"the
    /// diff applies only after an independent `FinanceReviewer` approves"* — and it
    /// is why the store is versioned rather than mutated in place.
    ///
    /// **The `If-Match` premise is tested here, not at the transport (D-186).**
    /// `api::rest::preconditions`' own division of labour says why: that module
    /// refuses a request it cannot understand, and the store refuses one whose
    /// premise has moved. A comparison in the handler would read the world, decide,
    /// and then hand the decision to a statement that races it — two proposals
    /// could each read a matching tag and both proceed. Recomputed from the same two
    /// reads [`state_at`] composes, inside this transaction, **before** a version
    /// number is minted, so a refused proposal consumes nothing.
    ///
    /// The instant comes off `asserted` rather than the wall clock: the tag is a
    /// function of the clock (D-188), and reading "now" a second time here would
    /// compare against a different representation from the one the caller's act is
    /// stamped with.
    ///
    /// # Errors
    /// [`DomainError::StaleVersion`] when the asserted tag no longer describes the
    /// tenant's policy; [`DomainError::ThresholdInvalid`] when the entry set is
    /// empty or names one currency twice;
    /// [`DomainError::PendingChangeUnitExists`] when the tenant already holds a
    /// pending proposal; [`DomainError::TimestampPrecisionExceeded`] on an
    /// unquantized `effective_from`; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "`ApprovalService::submit`'s reason, and the same facts: the scope and \
                  the tenant the gate compiled, the approval id the surface minted, the two \
                  halves of the proposal, the premise the caller asserted, the evaluator's \
                  verdict and the caller's stamp. \
                  Folding them into a request struct would name a type only this call site \
                  has, and the DTO the surface already parses is the wire shape rather than \
                  this one - `PutThresholdPolicyRequest` carries no approval id, no verdict \
                  and no stamp"
    )]
    pub async fn propose(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        approval_id: Uuid,
        effective_from: OffsetDateTime,
        entries: Vec<ThresholdEntry>,
        asserted: AssertedPolicy,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<(ThresholdVersion, ApprovalRecord), DomainError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<(ThresholdVersion, ApprovalRecord), DomainError, _>(move |txn| {
                Box::pin(async move {
                    require_policy_match(
                        &state_at(txn, &scope, tenant_id, asserted.now).await?.tag(),
                        &asserted.tag,
                    )?;
                    let previous = threshold_repo::latest_version(txn, &scope, tenant_id)
                        .await
                        .map_err(|e| repo_failure(&e))?;
                    // `None` is a tenant that has never proposed one, and its first
                    // version is `0` — not `1`, and not "unset means 0". See
                    // `latest_version`'s doc: the absence and version zero are
                    // different states and only one of them is a configured policy.
                    let next = previous.map_or(0, |held| held.saturating_add(1));
                    let number = u64::try_from(next).map_err(|_| {
                        DomainError::Internal(format!(
                            "bss-pricing: threshold version {next} is not a value this store can \
                             hold"
                        ))
                    })?;
                    let version = ThresholdVersion::new(number, effective_from, entries)
                        .map_err(|refusal| DomainError::ThresholdInvalid(refusal.detail()))?;
                    let rows: Vec<ThresholdEntryRow> = version
                        .entries()
                        .iter()
                        .map(row_of)
                        .collect::<Result<Vec<_>, DomainError>>()?;
                    // **The unit before the rows, and that ordering is the mint guard's
                    // (D-192 clause (2)).** Both are this transaction's, so atomicity is
                    // indifferent to the order; what is not indifferent is *which*
                    // constraint a loser meets first. `open_version` appends under the key
                    // `(tenant, version, currency)`, so two proposals that both minted
                    // version `n` collide there only if their currency sets **intersect**
                    // — and that collision renders 500, this store's insert not being one
                    // `storage` classifies. Opening the unit first puts
                    // `uq_pricing_approval_policy_pending` ahead of it, so the loser is
                    // refused `PENDING_CHANGE_UNIT_EXISTS` whatever the two proposals
                    // happen to price, and no version rows are written by a transaction
                    // that is going to roll back.
                    //
                    // `open_policy_unit` reads nothing from the version store — it hashes
                    // the in-memory `ThresholdVersion` and names it by number — so there is
                    // nothing here for the rows to have to exist for.
                    let record = crate::infra::approval::open_policy_unit(
                        txn,
                        &scope,
                        tenant_id,
                        approval_id,
                        &version,
                        materiality,
                        stamp,
                    )
                    .await?;
                    threshold_repo::open_version(
                        txn,
                        &scope,
                        tenant_id,
                        next,
                        effective_from,
                        &rows,
                        stamp,
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;
                    Ok((version, record))
                })
            })
            .await;
        outcome.map_err(into_domain)
    }

    /// Propose the **tombstone** — the version that says this tenant has no
    /// thresholds — and open the D-10 unit that reviews it (D-185).
    ///
    /// [`Self::propose`]'s transaction, its version minting, its unit and its
    /// refusals, over a version with no entries. What is *not* shared is worth
    /// stating: this does not relax [`ThresholdRefusal::NoEntries`]. An empty `PUT`
    /// is still refused, because an empty entry set is indistinguishable from
    /// absence — it writes zero rows, which no reader can tell from a version nobody
    /// proposed. A retirement is a **positive** marker, authored through its own
    /// door, and it writes a row of its own.
    ///
    /// **A second method rather than a flag on [`Self::propose`]**, because the two
    /// acts differ in what they write (`open_tombstone` against `open_version`), in
    /// which refusals can reach the caller (a tombstone has no entry set, so neither
    /// `NoEntries` nor `DuplicateCurrency` is expressible), and in what an operator
    /// is asking for. A boolean parameter would put the branch inside one function
    /// and leave the surface naming which side of it by passing a value — two
    /// functions with extra steps, and one of them reachable by accident.
    ///
    /// **It rides the same always-material unit, and that is the whole safety
    /// argument.** A tenant must not be able to revert the two-person rule
    /// single-handed: the retirement is minted, pinned and reviewed exactly as an
    /// entry version is, so the principal who proposes the removal of the reviewer
    /// requirement is not the principal who grants it. Until that unit is approved
    /// **and** the tombstone's `effective_from` has arrived (D-188), the tenant's
    /// policy is the version they already had.
    ///
    /// [`ThresholdRefusal::NoEntries`]: crate::domain::materiality::ThresholdRefusal::NoEntries
    ///
    /// It also rides [`Self::propose`]'s **`If-Match` premise**, tested in this
    /// transaction before the number is minted, and that is not a formality on this
    /// arm: a retirement is the one act that takes the tenant back to *everything is
    /// material*, so an operator authoring one against a stale reading may be
    /// retiring thresholds that a version approved since their read has already
    /// replaced.
    ///
    /// # Errors
    /// [`DomainError::StaleVersion`] when the asserted tag no longer describes the
    /// tenant's policy; [`DomainError::PendingChangeUnitExists`] when the tenant
    /// already holds a pending proposal;
    /// [`DomainError::TimestampPrecisionExceeded`] on an unquantized
    /// `effective_from`; [`DomainError::Internal`] on a storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "[`Self::propose`]'s reason, one argument short of it: this arm authors no \
                  entry set. The premise the caller asserted is what took it over the bound, \
                  and folding the rest into a request struct would name a type only these two \
                  call sites have"
    )]
    pub async fn retire(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        approval_id: Uuid,
        effective_from: OffsetDateTime,
        asserted: AssertedPolicy,
        materiality: JsonValue,
        stamp: AuditStamp,
    ) -> Result<(ThresholdVersion, ApprovalRecord), DomainError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<(ThresholdVersion, ApprovalRecord), DomainError, _>(move |txn| {
                Box::pin(async move {
                    require_policy_match(
                        &state_at(txn, &scope, tenant_id, asserted.now).await?.tag(),
                        &asserted.tag,
                    )?;
                    // The same mint as `propose`, off the same `latest_version` — which
                    // reads **both** threshold tables, so a retirement cannot take a
                    // number an entry version already holds and vice versa.
                    let previous = threshold_repo::latest_version(txn, &scope, tenant_id)
                        .await
                        .map_err(|e| repo_failure(&e))?;
                    let next = previous.map_or(0, |held| held.saturating_add(1));
                    let number = u64::try_from(next).map_err(|_| {
                        DomainError::Internal(format!(
                            "bss-pricing: threshold version {next} is not a value this store can \
                             hold"
                        ))
                    })?;
                    let version = ThresholdVersion::tombstone(number, effective_from);
                    // The unit before the row, for [`Self::propose`]'s reason. The
                    // tombstone table is keyed `(tenant, version)` rather than by
                    // currency, so a loser here would meet *its* key on any collision
                    // rather than only an intersecting one — the same 500, reached more
                    // easily.
                    let record = crate::infra::approval::open_policy_unit(
                        txn,
                        &scope,
                        tenant_id,
                        approval_id,
                        &version,
                        materiality,
                        stamp,
                    )
                    .await?;
                    threshold_repo::open_tombstone(
                        txn,
                        &scope,
                        tenant_id,
                        next,
                        effective_from,
                        stamp,
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;
                    Ok((version, record))
                })
            })
            .await;
        outcome.map_err(into_domain)
    }
}

/// One domain entry as the store's row shape.
///
/// The inverse of [`ThresholdEntryRow::basis`], and it lives beside it in spirit:
/// the enum-to-column-pair direction is total in the *shape* it writes (every basis
/// sets exactly one column) where the column-pair-to-enum direction is not, which is
/// the asymmetry the CHECK exists to close.
///
/// # A percent the column cannot hold is refused, not saturated
///
/// Not `i32::try_from(bp).unwrap_or(i32::MAX)`: the saturation is **not**
/// unreachable on the ground that "the surface" refuses anything above
/// `MAX_PERCENT_BP`. That constant is declared and enforced once, in
/// `api::rest::threshold_policy::parse_entries`;
/// `domain::materiality::ThresholdVersion::new` bounds only emptiness and
/// duplicate currencies, no bound on `ThresholdBasis::Percent { bp }` exists
/// anywhere in the domain, and [`ThresholdService::propose`] is a public `infra`
/// entry point taking `Vec<ThresholdEntry>` directly — so the ceiling sits two
/// layers above the value and a caller past the wire has nothing holding it.
///
/// What saturating cost was not a wrong threshold but an **unapprovable** one:
/// `open_policy_unit` pins the in-memory `ThresholdVersion`
/// (`content_pin::put_threshold_basis` frames the unsaturated `bp`), while the rows
/// written from here carry `i32::MAX`; `effective_version_at` re-derives
/// `threshold_content_hash` from the version read back **out of the store** and
/// requires `find_approved_for_content` to match. The two digests could never agree,
/// so the version could never afterwards be found approved — a silent permanent
/// wedge, in place of a refusal the proposer can act on.
///
/// The bound belongs in [`ThresholdEntry`] / `ThresholdVersion::new`, which is where
/// a value that is out of range should stop; that is a `domain::materiality` change
/// and is not made here. What is made here is the narrower guarantee this layer can
/// keep by construction: the row it writes and the digest the approval was opened
/// against name the same number, or nothing is written.
///
/// # Errors
/// [`DomainError::ThresholdInvalid`] when a percent basis is outside the range the
/// `percent_bp` column can hold.
fn row_of(entry: &ThresholdEntry) -> Result<ThresholdEntryRow, DomainError> {
    use crate::domain::materiality::ThresholdBasis;
    let (absolute_minor, percent_bp) = match entry.basis {
        ThresholdBasis::Absolute { minor } => (Some(minor), None),
        ThresholdBasis::Percent { bp } => {
            let stored = i32::try_from(bp).map_err(|_| {
                DomainError::ThresholdInvalid(format!(
                    "the {} threshold's percent basis is {bp} bp, which the percent_bp column \
                     cannot hold; the row would be stored saturated and would no longer match \
                     the content the approval was opened against",
                    entry.currency.as_str()
                ))
            })?;
            (None, Some(stored))
        }
    };
    Ok(ThresholdEntryRow {
        currency: entry.currency.as_str().to_owned(),
        absolute_minor,
        percent_bp,
    })
}

fn into_domain(err: toolkit_db::secure::TxError<DomainError>) -> DomainError {
    err.into_domain(|infra| DomainError::Internal(format!("bss-pricing: threshold: {infra}")))
}

#[cfg(test)]
#[path = "threshold_tests.rs"]
mod threshold_tests;
