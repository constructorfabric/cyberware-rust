//! The writer of `pricing_catalog_version_ref` — the pending handle a publish
//! commit leaves behind, and the subject the projector will resolve it to.
//!
//! Runner-taking and stateless, like [`audit_repo`](super::audit_repo) and
//! [`outbox_repo`](super::outbox_repo), and for the sharpest version of their
//! reason: a ref recorded outside the publish transaction is either a pending
//! assignment for a publish that never happened — which trips
//! `pricing.catalogversion.commit_overdue` forever — or a publish whose
//! addressability nothing is tracking.
//!
//! # The subject linkage
//!
//! `pricing_catalog_version_ref` is `(tenant_id, pending_ref, subject_kind,
//! subject_ref, catalog_version, requested_at, committed_at)`, and the two
//! subject columns were added by an in-place amendment to
//! `pricing_catalog_version_ref` while building this path. The migration's own doc carries
//! the argument and the rejected alternative; what matters here is the shape of
//! the obligation. The projector arrives at `CatalogVersionPublished` holding
//! committed refs and must write **exactly the subjects of the publish units
//! that produced** each version (§4.4, D-86/D-91). This row is the only durable
//! answer to "which subject did this handle publish".
//!
//! [`PendingVersionRow::for_subject`] derives both columns from one
//! [`SubjectRef`], so a writer cannot produce a kind and a reference that
//! disagree — `SubjectRef` already makes an `overlay_index` reference
//! inseparable from its shard key, and this constructor is what carries that
//! property into storage.
//!
//! # The finalize, and the one answer it refuses
//!
//! [`finalize`] is a compare-and-swap whose predicate is `catalog_version IS
//! NULL`, so two sweeps resolving one handle cannot both win. Zero rows
//! affected is **not** silently fine: the row is re-read, and the two cases
//! that produce it are answered differently. Already committed at the **same**
//! version is the idempotent replay a re-drive is entitled to make and returns
//! `Ok`. Already committed at a **different** version means the registry
//! answered one handle two ways — which re-points a pin that posted periods
//! resolve through — and is refused as [`RepoError::CorruptRow`], whose doc
//! already says "an invariant breach, never a caller mistake". No new variant
//! and no wire code: this path has no client to report to.
//!
//! It is the storage-side sibling of
//! [`VersionRef::finalize`](crate::domain::snapshot::VersionRef::finalize),
//! which refuses a re-finalize in the domain for the same reason and in the
//! same words. The two are not one mechanism and cannot be: that one moves a
//! value a caller holds, this one moves a row several sweeps can reach, and the
//! guarantee here has to be a predicate on a statement.
//!
//! # The prefix D-114 speaks of is **this tenant's**, and that is a reading
//!
//! [`next_committed_version_after`] answers "the smallest committed version of
//! this tenant strictly above the frontier". D-114 says pin-eligibility
//! requires "every **earlier version** to be itself pin-eligible", and
//! `CatalogVersion` is minted by a **cross-tenant** registry — so a tenant's
//! committed versions are a *subset* of the global sequence and are not
//! contiguous. On the global reading no tenant could ever advance past a
//! version another tenant's publish consumed, and every frontier in the
//! deployment would stick at the first gap. `pricing_pin_frontier` is keyed
//! `tenant_id` alone, which settles which reading was meant.
//!
//! **The ambiguity is reported**; what is implemented is the only buildable
//! reading — every earlier version *this tenant has a ref for*.
//!
//! # What is deliberately absent, and whose it is
//!
//! - **The overdue query.** `pricing.catalogversion.commit_overdue` measures the
//!   age of a pending ref, and [`list_pending_for_tenant`] carries `requested_at`
//!   so the sweep can evaluate that itself. A second query returning "the overdue
//!   ones" would put the threshold — a config value — inside a SQL predicate,
//!   where the job that owns the threshold cannot see it.
//! - **`list_for_pending_ref`** is the handle-wide read `GET
//!   /catalog-version/refs/{pendingRef}` uses. `find` stays the four-column
//!   identity for finalize. The surface is declared in
//!   `docs/superpowers/specs/2026-09-02-pending-ref-status-design.md`.

use std::collections::BTreeMap;

use bss_pricing_sdk::CatalogVersion;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::lifecycle::LifecycleState;
use crate::domain::read_model::{SubjectKind, SubjectRef};
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::catalog_version_ref;
use crate::infra::storage::repo::plan_repo::read_token;

/// One pending `CatalogVersion` request, as the row holds it.
///
/// The two subject columns are carried **as columns** rather than as a
/// [`SubjectRef`], because that is what a reader gets back: three of the four
/// kinds have no writer in this repository yet, and a read that could only
/// reconstruct the fourth would refuse rows a later group legitimately wrote.
/// The write side keeps the pair honest through
/// [`PendingVersionRow::for_subject`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingVersionRow {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The registry's handle for the pending assignment.
    pub pending_ref: String,
    /// What kind of subject the publish unit projects.
    pub subject_kind: SubjectKind,
    /// Which one — a plan id for a plan publish; the shard rendering for an
    /// overlay index.
    pub subject_ref: String,
    /// The revision of the subject the publish unit **judged**, when its kind
    /// has one.
    ///
    /// The projector reads this row rather than whatever revision is current
    /// when the sweep arrives, and the difference is a real defect: the
    /// registry batches at up to five minutes (D-47), a second publish of the
    /// same plan inside that window makes its revision current, and projecting
    /// from "current" then freezes content the earlier version's publish never
    /// judged — permanently, since a delta is INSERT-only on the seven-year
    /// horizon and a completed version never mutates.
    ///
    /// `None` for a subject kind that has no revision concept. A `plan` subject
    /// without one is refused by the projector rather than defaulted, which is
    /// the fail-closed shape: a default here is a guess about which content a
    /// frozen version froze.
    pub subject_revision: Option<u64>,
    /// The lifecycle state the publish unit **judged**, frozen with the revision
    /// it judged.
    ///
    /// Read back into the delta rather than the row's state as it now stands,
    /// because that state keeps moving: `published -> superseded` at the next
    /// revision's commit, `published -> retired` at retirement. `superseded` in
    /// particular is a value D-128 does not contemplate for a projected subject
    /// and that `plan_repo::load_current` could never return, so a delta
    /// carrying it — permanently, on an INSERT-only row — reads as unsellable
    /// to a consumer coding sellability predicate (4) as "is published".
    ///
    /// `None` for a subject kind with no lifecycle; a `plan` subject without one
    /// is refused by the projector, for the reason
    /// [`PendingVersionRow::subject_revision`] gives.
    pub subject_lifecycle_state: Option<LifecycleState>,
    /// The interval end the publish unit **judged** — the membership plane's
    /// pin, and `None` on every other kind.
    ///
    /// The same fix as the two fields above, on the one plane that was exempt
    /// from it while it minted a single publish unit per row (D-165's argument;
    /// `pricing_catalog_version_ref` carries the whole of it). A membership row has no
    /// revision-scoped content to pin, because it is mutated in place — and
    /// exactly one column of it moves, `effective_to`, which
    /// `group_membership_repo::end_membership` narrows. So that column is
    /// frozen here at commit and the projector reads it from here, while the
    /// facts no mutation touches are read from the row.
    ///
    /// `None` on a membership subject means **open-ended**, not "unpinned":
    /// [`PendingVersionRow::subject_revision`] carries the row version on that
    /// kind and the projector refuses a membership subject that arrives without
    /// one, so the pin's presence is never inferred from this field.
    pub subject_effective_to: Option<OffsetDateTime>,
    /// When addressability was requested, UTC.
    pub requested_at: OffsetDateTime,
    /// When this gear **first saw** the registry's answer for the handle
    /// (D-166), UTC — and the start instant every post-commit clause in the set
    /// was written against and none of them had.
    ///
    /// `None` means the registry has not answered yet, which is the whole of
    /// `commit_overdue`'s narrowed condition. `Some` on a row that is *still*
    /// pending is `PlanPublishDegraded`'s: the version committed and the warm
    /// has not landed. The two are disjoint over one clock by construction,
    /// which they were not while both measured `requested_at`.
    ///
    /// An **upper bound** on the registry's commit: the observation lags it by
    /// at most one sweep pass, so a signal derived from it is late rather than
    /// false. [`observe_commit`] writes it once and never moves it forward, so
    /// the degraded clock starts at the first sighting rather than resetting
    /// every pass.
    pub commit_observed_at: Option<OffsetDateTime>,
    /// The committed version, once the registry has assigned one.
    ///
    /// `None` is the pending state — the only one the publish commit can write
    /// — and the projector reads the row back to decide what to do with it,
    /// which is why the committed half is carried rather than left to a second
    /// query.
    pub catalog_version: Option<CatalogVersion>,
    /// When the version was assigned, UTC. Moves with
    /// [`PendingVersionRow::catalog_version`] and never separately —
    /// `chk_pricing_catalog_version_ref_commit` enforces the pairing
    /// physically, on both backends.
    pub committed_at: Option<OffsetDateTime>,
}

impl PendingVersionRow {
    /// Build a row whose subject columns cannot disagree.
    ///
    /// The kind comes from [`SubjectRef::kind`] and the reference from its
    /// `Display`, so there is no call site at which one could be set without the
    /// other.
    ///
    /// The committed half is `None` by construction and there is no parameter
    /// for it: the only writer of this table is the publish commit, which holds
    /// a handle and no version, and a constructor that let a caller supply one
    /// would let a publish claim addressability the registry never granted.
    /// [`finalize`] is the only way the pair moves.
    #[must_use]
    pub fn for_subject(
        tenant_id: Uuid,
        pending_ref: String,
        subject: &SubjectRef,
        subject_revision: Option<u64>,
        subject_lifecycle_state: Option<LifecycleState>,
        requested_at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant_id,
            pending_ref,
            subject_kind: subject.kind(),
            subject_ref: subject.to_string(),
            subject_revision,
            subject_lifecycle_state,
            // The membership plane's pin, and no other kind's — see
            // [`PendingVersionRow::for_membership`], which is the only
            // constructor that sets it.
            subject_effective_to: None,
            requested_at,
            commit_observed_at: None,
            catalog_version: None,
            committed_at: None,
        }
    }

    /// Build the row one **membership** publish unit records — the subject, and
    /// the state that unit's own mutation produced.
    ///
    /// Its own constructor rather than two more parameters on
    /// [`PendingVersionRow::for_subject`], for the reason the pin exists at all:
    /// the two values are the membership plane's and no other kind may set
    /// them, and a shared constructor taking them would let a plan publish pin
    /// an interval end. Every caller of this function is in
    /// [`crate::infra::membership_publish`].
    ///
    /// `row_version` is the version the mutation **produced** — `0` from an
    /// enrollment, the incremented value from an end — not the one it was
    /// presented as a precondition. `effective_to` is that same mutation's
    /// output, `None` for an open-ended membership.
    #[must_use]
    pub fn for_membership(
        tenant_id: Uuid,
        pending_ref: String,
        membership_id: Uuid,
        row_version: u64,
        effective_to: Option<OffsetDateTime>,
        requested_at: OffsetDateTime,
    ) -> Self {
        let subject = SubjectRef::GroupMembership(membership_id);
        Self {
            subject_revision: Some(row_version),
            subject_effective_to: effective_to,
            ..Self::for_subject(tenant_id, pending_ref, &subject, None, None, requested_at)
        }
    }
}

/// Record a pending version request inside `runner`'s transaction.
///
/// The row lands with `catalog_version` and `committed_at` both NULL, which is
/// the only shape `chk_pricing_catalog_version_ref_commit` admits before the
/// registry has assigned anything.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which **includes** a second
/// record of one `(tenant_id, pending_ref, subject_kind, subject_ref)`, refused by
/// the primary key. That is the right refusal rather than an upsert: the registry
/// is idempotent on `request_id`, so the **same subject** arriving twice under one
/// handle means two publish transactions believe they own the same assignment, and
/// silently overwriting the first would hand one publish's subject to the other's
/// version.
///
/// **The protection is per subject, not per handle**, which is the granularity
/// D-234 exists for: a handle names one assignment and an overlay publish unit
/// records two or three subjects against it, so a handle recorded once *per
/// subject* is the ordinary case rather than a fault. The key is **not**
/// `(tenant_id, pending_ref)`: that two-column form was dropped and recreated on
/// both engines, which is why a rule of this kind has to be read off the schema
/// rather than off the migration that first wrote it. [`find`] and
/// [`RefIdentity`] carry the wider key.
pub async fn record_pending(
    runner: &impl DBRunner,
    scope: &AccessScope,
    entry: PendingVersionRow,
) -> Result<(), RepoError> {
    let am = catalog_version_ref::ActiveModel {
        tenant_id: Set(entry.tenant_id),
        pending_ref: Set(entry.pending_ref.clone()),
        subject_kind: Set(entry.subject_kind.as_str().to_owned()),
        subject_ref: Set(entry.subject_ref.clone()),
        subject_revision: Set(stored_revision(entry.subject_revision)?),
        subject_lifecycle_state: Set(entry
            .subject_lifecycle_state
            .map(|state| state.as_str().to_owned())),
        subject_effective_to: Set(entry.subject_effective_to),
        // Both NULL until `CatalogVersionPublished`; the CHECK ties them.
        catalog_version: Set(None),
        requested_at: Set(entry.requested_at),
        // Nothing has been observed at the commit: the sweep is the only
        // observer and it has not run yet.
        commit_observed_at: Set(None),
        committed_at: Set(None),
    };
    catalog_version_ref::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)
        .map_err(|e| RepoError::Db(format!("pricing_catalog_version_ref scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("record pending catalog version ref: {e}")))?;
    Ok(())
}

/// Read one ref row back by its composite identity — the handle **and the
/// subject** (D-234).
///
/// The subject is part of the identity since `pricing_catalog_version_ref`: a handle names
/// one assignment and an overlay publish unit records two or three subjects
/// against it, so `(tenant, handle)` alone selects a set and `.one()` over it
/// would answer with whichever row the store returned first.
///
/// SQL-level BOLA: a foreign tenant's ref yields `None`.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when `subject_kind` holds a token outside the
/// enumeration its `CHECK` constrains it to — which means something reached the
/// table around this gear.
pub async fn find(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: RefIdentity<'_>,
) -> Result<Option<PendingVersionRow>, RepoError> {
    let row = catalog_version_ref::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(id.condition())
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pending catalog version ref: {e}")))?;
    row.map(to_domain).transpose()
}

/// The four columns that identify one ref row since `pricing_catalog_version_ref`.
///
/// One value rather than four parameters, and that is the point rather than a
/// convenience: [`find`] and [`finalize`] must select the **same** row — the
/// finalize's compare-and-swap and the re-read that interprets its zero-rows case
/// are one decision, and two spellings of the identity would let the swap miss a
/// row the re-read then reports as an invariant breach. A caller cannot supply
/// three of the four.
#[derive(Clone, Copy, Debug)]
pub struct RefIdentity<'a> {
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The registry's handle — one assignment, which a unit may record several
    /// subjects against.
    pub pending_ref: &'a str,
    /// Which kind of subject this row is about.
    pub subject_kind: SubjectKind,
    /// Which subject of that kind.
    pub subject_ref: &'a str,
}

impl<'a> RefIdentity<'a> {
    /// The identity of a row the caller already holds.
    ///
    /// The projector reads a subject and then finalizes **that** subject, so it
    /// never assembles an identity by hand — which is what keeps the finalize on
    /// the row whose projection shares its transaction.
    #[must_use]
    pub fn of(row: &'a PendingVersionRow) -> Self {
        Self {
            tenant_id: row.tenant_id,
            pending_ref: &row.pending_ref,
            subject_kind: row.subject_kind,
            subject_ref: &row.subject_ref,
        }
    }

    /// The `WHERE` this identity selects.
    fn condition(self) -> Condition {
        Condition::all()
            .add(catalog_version_ref::Column::TenantId.eq(self.tenant_id))
            .add(catalog_version_ref::Column::PendingRef.eq(self.pending_ref))
            .add(catalog_version_ref::Column::SubjectKind.eq(self.subject_kind.as_str()))
            .add(catalog_version_ref::Column::SubjectRef.eq(self.subject_ref))
    }
}

impl std::fmt::Display for RefIdentity<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}/{}",
            self.pending_ref, self.subject_kind, self.subject_ref
        )
    }
}

/// Record that this gear has seen the registry's answer for `pending_ref`
/// (D-166 clause 1).
///
/// **Write-once.** The predicate is `commit_observed_at IS NULL`, so the instant
/// is the **first** sighting and no later pass moves it forward. That is not a
/// nicety: a stamp that advanced every pass would keep the degraded clock at
/// zero forever, and the one signal `fr-publish-fanout-atomicity` exists to
/// raise would never raise.
///
/// **Independent of the projection, by placement.** The caller stamps this
/// before it attempts to project, in a statement of its own, because the whole
/// point of the column is to survive a projection that fails — the finalize and
/// the warm share a transaction, so a stamp inside it would be rolled back
/// exactly on the path the signal is for.
///
/// A row that is already committed is matched too when it has no observation,
/// which is the shape a ref finalized by something other than the sweep would
/// leave; there is no such writer today and the predicate costs nothing.
///
/// # Handle-scoped, and the one function here that is
///
/// This is the only statement in this module that addresses ref rows by
/// `(tenant_id, pending_ref)` rather than through [`RefIdentity`], whose whole
/// purpose is that *"a caller cannot supply three of the four"*. It is deliberate
/// and it is not the same granularity question [`finalize`] answers below: what
/// this records is **the registry's answer**, and the registry answers a *handle*
/// — one question, one instant — so stamping every subject row of that handle at
/// that instant is the honest observation, and a per-subject stamp would record
/// one arrival as several. The observation also has no transaction to share:
/// [`finalize`] is per subject precisely so a subject's finalize and its
/// projection commit together, while this one is written **outside** that
/// transaction on purpose, so it survives a projection that fails.
///
/// The four-column identity is unchanged by the pin columns this table has since
/// gained (`subject_lifecycle_state`, `subject_effective_to`,
/// `pricing_catalog_version_ref`): they are what a version froze, not what selects a row, and
/// the primary key is still `pricing_catalog_version_ref`'s
/// `(tenant_id, pending_ref, subject_kind, subject_ref)`. So "identify" still
/// means those four, and this function deliberately identifies something else —
/// a handle. A `HandleIdentity` type would name the second granularity, and is
/// not built for one call site.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure. An absent row is **not** an
/// error: the sweep read the ref moments ago and a row that has since gone is
/// nothing this observation can or should assert about.
pub async fn observe_commit(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    pending_ref: &str,
    observed_at: OffsetDateTime,
) -> Result<(), RepoError> {
    catalog_version_ref::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            catalog_version_ref::Column::CommitObservedAt,
            Expr::value(observed_at),
        )
        .filter(
            Condition::all()
                .add(catalog_version_ref::Column::TenantId.eq(tenant_id))
                .add(catalog_version_ref::Column::PendingRef.eq(pending_ref))
                .add(catalog_version_ref::Column::CommitObservedAt.is_null()),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("observe catalog version commit: {e}")))?;
    Ok(())
}

/// Resolve one **subject** of a pending handle to its committed version, inside
/// `runner`'s transaction.
///
/// A compare-and-swap on `catalog_version IS NULL`. See the module doc for why
/// the zero-rows case is re-read rather than accepted, and why one of its two
/// outcomes is an invariant breach.
///
/// # Per subject, not per handle (D-234)
///
/// A handle carries every subject its publish unit projects — one on the plan
/// plane, two or three on the overlay plane (D-112, D-133) — and the projector
/// finalizes a ref **inside the transaction that projects that subject**, so
/// that a crash between the two cannot leave a committed ref nothing ever
/// projected. Finalized handle-wide, one subject's transaction would commit its
/// siblings' rows too; a sibling whose own projection then failed would be
/// committed and unwarm, therefore absent from
/// [`list_pending_for_tenant`] — and no later pass would ever offer its version
/// to the projector again. The version would stand incomplete and the frontier
/// stuck, with both signals that would say so reading off pending refs.
///
/// # Errors
/// [`RepoError::NotFound`] when the tenant has no such handle **for this
/// subject** — a version was assigned to a publish this store never recorded,
/// which the sweep can only have reached by asking the registry about a ref it
/// read from this table.
/// [`RepoError::CorruptRow`] when the row already carries a **different**
/// committed version, or when `version` exceeds the signed range the column
/// stores. [`RepoError::Db`] on a scope or storage failure.
pub async fn finalize(
    runner: &impl DBRunner,
    scope: &AccessScope,
    id: RefIdentity<'_>,
    version: CatalogVersion,
    committed_at: OffsetDateTime,
) -> Result<(), RepoError> {
    let target = stored_version(version)?;
    let outcome = catalog_version_ref::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            catalog_version_ref::Column::CatalogVersion,
            Expr::value(target),
        )
        .col_expr(
            catalog_version_ref::Column::CommittedAt,
            Expr::value(committed_at),
        )
        .filter(
            id.condition()
                // The swap half: a row another sweep already finalized is not
                // matched at all, so two sweeps cannot both write a version.
                .add(catalog_version_ref::Column::CatalogVersion.is_null()),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("finalize catalog version ref: {e}")))?;
    if outcome.rows_affected > 0 {
        return Ok(());
    }

    let existing = find(runner, scope, id)
        .await?
        .ok_or_else(|| RepoError::NotFound {
            subject: "pending catalog version ref".to_owned(),
            id: id.to_string(),
        })?;
    let tenant_id = id.tenant_id;
    match existing.catalog_version {
        Some(already) if already == version => Ok(()),
        Some(already) => Err(RepoError::CorruptRow(format!(
            "ref {id} of tenant {tenant_id} is committed at catalog version {}, and the registry \
             answered {} for the same handle",
            already.get(),
            version.get()
        ))),
        // The CAS matched nothing and the row is still pending: nothing in this
        // gear can produce that, so it is the store disagreeing with itself
        // rather than a race this function lost.
        None => Err(RepoError::CorruptRow(format!(
            "ref {id} of tenant {tenant_id} refused a finalize to catalog version {} while still \
             holding no version",
            version.get()
        ))),
    }
}

/// The tenants that hold at least one pending ref, at most `limit` of them,
/// ascending by tenant id.
///
/// **Why a discovery read rather than a cross-tenant page of refs** (D-163
/// clause 2). The bound on refs has to be **per tenant**: on a cross-tenant page
/// ordered by request instant, one tenant's stuck backlog fills the whole budget
/// and defers every other tenant's completions — and a completion deferred is a
/// frontier that does not advance, so the unfairness is not a latency detail but
/// a tenant's publishes going unpinnable because another tenant's are.
///
/// It is a **paged keyset walk**, and it deduplicates in memory: each query asks
/// for up to `limit` rows above the last tenant already seen, ordered by tenant
/// id, over the same `(tenant_id, catalog_version)` index the projector and the
/// frontier walk read. So the cost is proportional to the tenants that actually
/// have **outstanding publishes**, not to the tenants of the deployment — in a
/// healthy one a ref is pending for at most a batching delay and the walk stops
/// after one page.
///
/// # Why the page, when a single-row seek per tenant is the textbook loose scan
///
/// It **was** one `find().limit(1)` per tenant found, which is the textbook shape
/// and the wrong one here: the caller is a sweep at `readmodel_warm_tick_secs` —
/// 5 seconds — asking for up to `pending_tenants_per_pass` — 250 — tenants, so a
/// deployment with 250 tenants mid-publish paid 250 round trips every five
/// seconds before the two pending reads per tenant and the registry calls,
/// against a lease whose TTL *is* that same 5 seconds.
/// `tests/sqlite_pending_tenants_shape.rs` counts the statements: twelve tenants
/// took twelve, and take two.
///
/// The cursor still advances **past a whole tenant** (`TenantId.gt(last_seen)`),
/// which is what keeps the answer distinct tenants rather than rows and what
/// bounds the walk: every query after the first either yields a tenant this pass
/// had not seen or ends the walk. The residual worst case is one query per tenant
/// again, reached only when every tenant on the page carries `limit` or more
/// pending refs of its own — a backlog `pending_refs_per_tenant` and the
/// `commit_overdue` alarm are both already about — and never worse than the shape
/// this replaced.
///
/// `SecureSelect` exposes no `distinct` (see `price_repo::gated_markets` for the
/// same constraint and the reason it is deliberate), so the deduplication is here
/// rather than in SQL either way.
///
/// **`limit` and the ordering are values, and they are the owner's** — D-163
/// says so in as many words: *how many tenants a pass takes and in what order
/// needs a value and is carried in §F.2*. Ascending tenant id is deterministic
/// and it is **not** rotating: past `limit`, the tail of the tenant order is not
/// swept at all. Rotating it needs a cursor no table in §3.7 carries. `limit`
/// bounds the **tenants** answered, and it is spent as the page size too, so no
/// second knob decides how many rows one query may read.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn pending_tenants(
    runner: &impl DBRunner,
    scope: &AccessScope,
    limit: u64,
) -> Result<Vec<Uuid>, RepoError> {
    let mut found: Vec<Uuid> = Vec::new();
    let mut after: Option<Uuid> = None;
    while u64::try_from(found.len()).unwrap_or(u64::MAX) < limit {
        let mut filter =
            Condition::all().add(catalog_version_ref::Column::CatalogVersion.is_null());
        if let Some(above) = after {
            filter = filter.add(catalog_version_ref::Column::TenantId.gt(above));
        }
        let rows = catalog_version_ref::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(filter)
            .order_by(catalog_version_ref::Column::TenantId, Order::Asc)
            .limit(limit)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("list tenants with pending refs: {e}")))?;
        // A short page is the end of the table above the cursor: the read was
        // bounded and came back under its bound, so there is nothing beyond it.
        let exhausted = u64::try_from(rows.len()).unwrap_or(u64::MAX) < limit;
        for row in rows {
            // The rows arrive ordered by tenant, so the last one recorded is the
            // only value a duplicate can match — a set would be a second
            // structure for a fact the ordering already gives.
            if found.last() != Some(&row.tenant_id) {
                found.push(row.tenant_id);
            }
            after = Some(row.tenant_id);
        }
        if exhausted {
            break;
        }
    }
    // The page is spent as the row bound, so a page whose rows are many tenants
    // can overshoot the tenant bound. D-163's tail is cut at the end of the
    // ascending order, which is where the overshoot is.
    found.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(found)
}

/// The oldest `limit` refs of **one tenant** still awaiting a version.
///
/// Read under the sanctioned [`AccessScope::allow_all`] system scope by the
/// sweep, which narrows to `AccessScope::for_tenant` before any per-tenant
/// write — the pattern the sibling ledger's jobs document. Oldest first so a
/// backlog drains in the order it accumulated, and bounded so one pass cannot
/// read an unbounded backlog into memory.
///
/// **The bound is a fact the caller must act on, not just a safety valve.** A
/// full page means the tenant has refs this pass did not see, so the pass cannot
/// know a version's whole subject set and must decide **no** completion (D-163
/// clause 2). The caller compares the row count to the limit it passed; there is
/// no second query telling it, because the count it already holds says it.
///
/// `requested_at` and `commit_observed_at` both ride on the row because they are
/// what the two narrowed signals measure — the ref's age for
/// `pricing.catalogversion.commit_overdue`, the observation's age for
/// `PlanPublishDegraded` (D-166) — and the thresholds belong to the job rather
/// than to a SQL predicate here.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored row cannot be read as the value its
/// columns are `CHECK`-constrained to hold.
pub async fn list_pending_for_tenant(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    limit: u64,
) -> Result<Vec<PendingVersionRow>, RepoError> {
    let rows = catalog_version_ref::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(catalog_version_ref::Column::TenantId.eq(tenant_id))
                .add(catalog_version_ref::Column::CatalogVersion.is_null()),
        )
        .order_by(catalog_version_ref::Column::RequestedAt, Order::Asc)
        .order_by(catalog_version_ref::Column::PendingRef, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pending catalog version refs: {e}")))?;
    rows.into_iter().map(to_domain).collect()
}

/// Every subject row of one pending handle.
///
/// A handle names one registry assignment and a publish unit records one, two
/// or three subjects against it (D-234). The status route has only the handle
/// the receipt carried, so this is the read — [`find`] stays the four-column
/// identity finalize uses.
///
/// Empty is a legitimate answer: unknown handle, or a handle this tenant
/// (under this scope) does not hold.
///
/// # Errors
/// As [`list_pending_for_tenant`].
pub async fn list_for_pending_ref(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    pending_ref: &str,
) -> Result<Vec<PendingVersionRow>, RepoError> {
    let rows = catalog_version_ref::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(catalog_version_ref::Column::TenantId.eq(tenant_id))
                .add(catalog_version_ref::Column::PendingRef.eq(pending_ref)),
        )
        .order_by(catalog_version_ref::Column::SubjectKind, Order::Asc)
        .order_by(catalog_version_ref::Column::SubjectRef, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list catalog version refs by handle: {e}")))?;
    rows.into_iter().map(to_domain).collect()
}

/// Every ref of one committed version — the subject set that version must
/// project.
///
/// This is the projector's input, and D-157 is why it can exist at all: the
/// subject columns on this row are the only durable path from a committed
/// handle back to what it published. The result is legitimately a **set**,
/// which is the batched case the version index stopped being unique for.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored row cannot be read as the value its
/// columns are `CHECK`-constrained to hold, or when `catalog_version` exceeds
/// the signed range the column stores.
pub async fn list_at_version(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
) -> Result<Vec<PendingVersionRow>, RepoError> {
    let target = stored_version(catalog_version)?;
    let rows = catalog_version_ref::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(catalog_version_ref::Column::TenantId.eq(tenant_id))
                .add(catalog_version_ref::Column::CatalogVersion.eq(target)),
        )
        .order_by(catalog_version_ref::Column::PendingRef, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list catalog version refs at a version: {e}")))?;
    rows.into_iter().map(to_domain).collect()
}

/// Every overlay this tenant had published **at or below** `catalog_version`,
/// each at the revision its greatest such publish pinned (D-234).
///
/// # This is the `overlay_index` shard's source, and the reason it is the ref
/// table rather than the overlay table
///
/// S9 §7 requires the index to enumerate *"every scope-matching overlay **live at
/// V**"*, and pairs it with one per-subject document read per matching overlay.
/// The overlay table answers a different question — what is published **now** —
/// and the projector arrives up to D-47's five-minute batching maximum after the
/// commit. Read from the overlay table, a second overlay publishing inside that
/// window would land in the *earlier* version's frozen shard, and a consumer
/// pinned at `V` would enumerate an overlay whose document has no delta at or
/// below `V`. That is permanent: a delta is INSERT-only on the seven-year
/// horizon.
///
/// **The greatest ref, not any ref.** An overlay that published revisions 0 and 1
/// has two refs, and at a `V` between them the shard must speak of revision 0 —
/// which is Foundation §4.4's own greatest-completed-≤-pin rule, applied to
/// choose which revision the *index* describes rather than which delta a reader
/// resolves. It also gives D-133's "two shards when a revision moves the scope
/// value" with no special case: the overlay appears in whichever shard its
/// greatest-≤-`V` revision names, so it leaves the old shard and joins the new
/// one at the version that moved it.
///
/// **Committed refs only.** A pending ref carries no version, so it belongs to no
/// `V` yet; the caller adds the refs of the version it is *currently* projecting,
/// which is the same union [`version_subjects`](crate::infra::read_model) makes
/// and for the same reason — those rows are not finalized yet and this query
/// cannot see them.
///
/// # The `<= V` bound is the correct rule and **nothing currently exercises it**
///
/// Stated as a premise rather than left to be assumed, because a probe found it:
/// removing the bound entirely reddens no test. What keeps a *later* version's
/// overlays out of this set today is not the bound but the `IS NOT NULL` filter
/// beside it — a version is projected while every later version's refs are still
/// **pending**, since the sweep groups by version and walks them ascending. And a
/// version cannot normally be projected *after* a later one has committed either:
/// D-114's prefix closure means the frontier would already have passed it, and
/// [`refuse_projection_below_frontier`](crate::infra::read_model) refuses that
/// loudly.
///
/// So the bound is defence rather than the load-bearing guard, and it stays for
/// the reason it is written: it says what the rule *is*. The two mechanisms that
/// enforce it today are incidental to this query, and a change to either — a
/// sweep that projected versions out of order, a re-drive reaching a version
/// below a committed sibling — would make this bound the only thing standing
/// between a frozen shard and an overlay from the future.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] on a stored token outside its `CHECK`.
pub async fn overlay_revisions_at_or_below(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
) -> Result<BTreeMap<Uuid, u64>, RepoError> {
    let target = stored_version(catalog_version)?;
    let rows = catalog_version_ref::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(catalog_version_ref::Column::TenantId.eq(tenant_id))
                .add(
                    catalog_version_ref::Column::SubjectKind.eq(SubjectKind::PriceOverlay.as_str()),
                )
                .add(catalog_version_ref::Column::CatalogVersion.is_not_null())
                .add(catalog_version_ref::Column::CatalogVersion.lte(target)),
        )
        // Ascending, so the fold below keeps the **greatest** ref of each overlay
        // by simply letting later rows win. The revision is the tie-break and it
        // is load-bearing, not tidiness: an order total on `catalog_version` alone
        // leaves two publishes of ONE overlay batched into one version — D-47's
        // ordinary case — in an order SQL does not define, and the fold would then
        // freeze whichever the store happened to return last into the
        // `overlay_index` shard, permanently. Same discipline as
        // [`list_pending_for_tenant`], which orders by `pending_ref` after
        // `requested_at` for the same reason.
        .order_by(catalog_version_ref::Column::CatalogVersion, Order::Asc)
        .order_by(catalog_version_ref::Column::SubjectRevision, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list overlay refs at or below a version: {e}")))?;

    let mut latest = BTreeMap::new();
    for row in rows {
        let entry = to_domain(row)?;
        let Ok(price_overlay_id) = Uuid::parse_str(&entry.subject_ref) else {
            return Err(RepoError::CorruptRow(format!(
                "pricing_catalog_version_ref names price overlay subject {}, which is not an \
                 overlay id",
                entry.subject_ref
            )));
        };
        let Some(revision) = entry.subject_revision else {
            return Err(RepoError::CorruptRow(format!(
                "pricing_catalog_version_ref names price overlay subject {price_overlay_id} and \
                 no revision; the revision the publish judged is what the index speaks of"
            )));
        };
        latest.insert(price_overlay_id, revision);
    }
    Ok(latest)
}

/// The tenant's smallest committed version strictly above `frontier` — or its
/// smallest committed version at all, when the tenant has no frontier yet.
///
/// This is what "the frontier's **next** version in order" means for a store
/// whose version numbers come from a cross-tenant registry; the module doc
/// carries the argument and reports the ambiguity.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when `frontier` or a stored version lies outside
/// the range its column holds.
pub async fn next_committed_version_after(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    frontier: Option<CatalogVersion>,
) -> Result<Option<CatalogVersion>, RepoError> {
    let mut filter = Condition::all()
        .add(catalog_version_ref::Column::TenantId.eq(tenant_id))
        .add(catalog_version_ref::Column::CatalogVersion.is_not_null());
    if let Some(frontier) = frontier {
        filter =
            filter.add(catalog_version_ref::Column::CatalogVersion.gt(stored_version(frontier)?));
    }
    let row = catalog_version_ref::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(catalog_version_ref::Column::CatalogVersion, Order::Asc)
        .limit(1)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read next committed catalog version: {e}")))?;
    let Some(row) = row else {
        return Ok(None);
    };
    read_version(&row)
}

/// Map a stored row into the value the rest of the system reasons about.
fn to_domain(row: catalog_version_ref::Model) -> Result<PendingVersionRow, RepoError> {
    let subject_kind = read_token(
        "pricing_catalog_version_ref.subject_kind",
        &row.subject_kind,
        SubjectKind::ALL,
        SubjectKind::as_str,
    )?;
    let catalog_version = read_version(&row)?;
    let subject_revision = read_revision(&row)?;
    let subject_lifecycle_state = row
        .subject_lifecycle_state
        .as_deref()
        .map(|token| {
            read_token(
                "pricing_catalog_version_ref.subject_lifecycle_state",
                token,
                LifecycleState::ALL,
                LifecycleState::as_str,
            )
        })
        .transpose()?;
    Ok(PendingVersionRow {
        tenant_id: row.tenant_id,
        pending_ref: row.pending_ref,
        subject_kind,
        subject_ref: row.subject_ref,
        subject_revision,
        subject_lifecycle_state,
        subject_effective_to: row.subject_effective_to,
        requested_at: row.requested_at,
        commit_observed_at: row.commit_observed_at,
        catalog_version,
        committed_at: row.committed_at,
    })
}

/// The row's judged revision, in the domain's unsigned vocabulary.
fn read_revision(row: &catalog_version_ref::Model) -> Result<Option<u64>, RepoError> {
    row.subject_revision
        .map(|stored| {
            u64::try_from(stored).map_err(|e| {
                RepoError::CorruptRow(format!(
                    "pending ref {} of tenant {} holds subject_revision {stored}: {e}",
                    row.pending_ref, row.tenant_id
                ))
            })
        })
        .transpose()
}

/// A revision as its `bigint` column holds it.
fn stored_revision(revision: Option<u64>) -> Result<Option<i64>, RepoError> {
    revision
        .map(|value| {
            i64::try_from(value).map_err(|e| RepoError::ValueOutOfRange {
                field: "subjectRevision".to_owned(),
                value: format!("{value}: {e}"),
            })
        })
        .transpose()
}

/// The row's committed version, in the SDK's unsigned vocabulary.
///
/// A stored value outside that range is [`RepoError::CorruptRow`] rather than a
/// silent `None`: the column is `CHECK (catalog_version IS NULL OR
/// catalog_version >= 0)`, so a negative one means something reached the table
/// around this gear — and a committed row read back as pending would be
/// re-finalized at whatever the registry says next.
fn read_version(row: &catalog_version_ref::Model) -> Result<Option<CatalogVersion>, RepoError> {
    row.catalog_version
        .map(|stored| {
            u64::try_from(stored).map(CatalogVersion::new).map_err(|e| {
                RepoError::CorruptRow(format!(
                    "pending ref {} of tenant {} holds catalog_version {stored}: {e}",
                    row.pending_ref, row.tenant_id
                ))
            })
        })
        .transpose()
}

/// A version as its `bigint` column holds it.
fn stored_version(version: CatalogVersion) -> Result<i64, RepoError> {
    i64::try_from(version.get()).map_err(|e| {
        RepoError::CorruptRow(format!(
            "catalog version {} exceeds the storable range: {e}",
            version.get()
        ))
    })
}
