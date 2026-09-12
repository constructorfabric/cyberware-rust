//! Repository for the plan aggregate's **revision chain** (`pricing_plan`,
//! D-56 / D-83 / D-90 / D-128).
//!
//! A plan is not a row, so this is not a CRUD repository over one. It is the
//! authoring half of `design/01-foundation.md` §4.2 step 1 — create a draft,
//! edit it, abandon it, or open the next revision — and every method exists
//! because the chain has an invariant that would otherwise be enforced nowhere
//! but in a caller's discipline.
//!
//! **The compare-and-swap is one statement, not a read then a write.**
//! [`update_draft`] and [`abandon_draft`] match on the row version the caller
//! read *inside* the UPDATE that acts on it, and the bump is
//! `row_version = row_version + 1` in that same statement. Computing the
//! successor in Rust would let two writers holding the same current version
//! compute the same next one and both write it — the silent overwrite
//! `cpt-cf-bss-pricing-fr-concurrent-edit` forbids, reintroduced by the helper
//! meant to prevent it. `domain::concurrency::RowVersion` has no increment for
//! exactly this reason.
//!
//! **Nothing here deletes a revision row.** A discarded draft is flipped to the
//! terminal `abandoned` state, so the `revision` number it consumed stays
//! consumed and `(plan_id, revision)` names one row for the life of the plan
//! (D-145). [`open_revision`] mints from `max(revision) + 1` over the plan's own
//! rows for the same reason — see its own note on what deriving from the
//! current revision would hand out twice.
//!
//! **A revision never moves without its shape.** D-83 versions the child shape
//! tables with the revision, so [`open_revision`] copies them forward and
//! [`abandon_draft`] drops the discarded revision's copies — each inside the
//! **one transaction** that moves the revision row, because a revision that
//! exists without its shape is a draft that looks like a plan whose author
//! deleted everything, and a tombstone that kept its copies is a frozen shape
//! hanging off a row nothing can reach. The two orderings are opposite and both
//! forced by the child tables' append-only trigger, which reads the parent
//! revision: the copy follows the insert because the INSERT arm requires the
//! **new** parent to be `draft`, and the drop precedes the flip because
//! `abandoned` is not `draft` and the DELETE arm would refuse it afterwards.
//!
//! **A failed swap gets three answers, not one.** Zero rows affected is
//! ambiguous by construction — the predicate is a conjunction — so the row is
//! read back once and the refusal names which conjunct failed:
//! [`RepoError::NotFound`] (absent, or another tenant's),
//! [`RepoError::NotDraft`] (frozen, which now includes an already-abandoned
//! revision), [`RepoError::StaleRowVersion`] (a read the caller never
//! refreshed). One undifferentiated conflict would tell an operator to retry in
//! the one case where retrying can never work.
//!
//! **The draft-only guard is enforced here as well as by the trigger.** §4.3 is
//! explicit that immutability is enforced twice, and the second enforcement is
//! not redundancy: the table trigger's answer is a raw database error carrying
//! no state and no subject, which reaches a caller as an internal fault. The
//! predicate on these statements is what turns "you are editing a frozen
//! revision" into something a surface can render.
//!
//! **One Slice-2 rule is unreachable through this file, and stays a rule.**
//! `chk_pricing_plan_purchase_qty` refuses `purchase_min_qty > purchase_max_qty`
//! at the column, so no row written here can ever be the subject
//! [`PURCHASE_QTY_RANGE_INVALID`](crate::domain::plan_rules::PURCHASE_QTY_RANGE_INVALID)
//! describes. The rule is not therefore redundant: the validation pipeline
//! judges a shape the **caller** assembled, before any of it is a row, and it
//! owes an author one report enumerating every finding. Deleting the rule would
//! move that finding onto the driver-error path, where a publish attempt answers
//! with an internal fault instead of a line somebody can act on.
//!
//! **The caller is `api::rest::plans`' `require_authorable_purchase_window`**, which
//! runs the same rule at the parse of both verbs, and these two statements are the
//! backstop the paragraph above claims they are. Without it the rule has no door:
//! every `run_publish_rules` subject is assembled *from storage*, so the only path
//! an inverted window would have is the `INSERT` here and the `UPDATE` in
//! [`update_draft_on`] — both of which answer `RepoError::Db`, rendering
//! `500 … please retry later` for a payload whose whole remedy is one field.
//!
//! [`update_draft`]: PlanRepo::update_draft
//! [`abandon_draft`]: PlanRepo::abandon_draft
//! [`open_revision`]: PlanRepo::open_revision

use std::collections::BTreeMap;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, ExprTrait, JsonValue, Order, QueryFilter, QuerySelect,
    QueryTrait,
};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, DbConn, DbTx, SecureEntityExt, SecureInsertExt, SecureUpdateExt, TxError,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind, subject_state};
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{
    EntitlementGrants, GrantSet, PlanChangeContract, UsageCounterOnPlanChange,
};
use crate::domain::lifecycle::LifecycleState;
use crate::domain::plan::{PlanRevision, PlanShapePatch};
use crate::domain::plan_shape::{BillingCycle, CustomIntervalUnit, Frequency};
use crate::domain::scope_key::PlanId;
use crate::infra::storage::entity::plan;
use crate::infra::storage::repo::bundle_repo;
use crate::infra::storage::repo::check_authored_instant;
use crate::infra::storage::repo::outbox_repo::{
    NewOutboxEvent, PlanCreatedPayload, PlanUpdatedPayload,
};
use crate::infra::storage::repo::plan_shape_repo::{
    copy_addon_rules, copy_composites, copy_descriptor_set, copy_period_floor_caps, copy_phases,
    delete_addon_rules, delete_composites, delete_descriptor_set, delete_period_floor_caps,
    delete_phases,
};
use crate::infra::storage::repo::{NewAuditEntry, audit_repo, outbox_repo};
use toolkit_odata::{ODataQuery, Page};

use crate::infra::storage::odata_mapping::OdataPageError;
use crate::infra::storage::{RepoError, contention_or_db};

#[path = "plan_list.rs"]
mod plan_list;
pub use plan_list::PlanListEntry;

/// The noun every **compare-and-swap** refusal names, so a caller that failed
/// to edit revision 3 and a caller that failed to delete it are told about the
/// same kind of thing in the same word.
///
/// It is not the only subject this file emits, and the doc says so rather than
/// pretending otherwise: [`PlanRepo::open_revision`] reports a missing
/// `current plan revision`, because the referent it could not find is the
/// **current** revision and not the one the caller named — "plan revision not
/// found" would be false and would send an author looking for a revision
/// nobody asked about. Two nouns, each true of what it refuses; the rule this
/// const enforces is the narrower and more useful one, that no single refusal
/// gets spelled two ways.
const SUBJECT: &str = "plan revision";

/// Everything a plan's **first** revision needs.
///
/// The plan id is caller-supplied rather than minted here: the authoring surface
/// that mints it (`api::rest::plans::create_plan`, which now exists) is also the
/// one that has to return it in a `Location` header before the row is durable,
/// and a repository that generated ids would make an idempotent retry create a
/// second plan. That surface mints inside the idempotency gate's transaction, so
/// a replay is answered the id the first call minted.
///
/// `created_at_utc` is caller-supplied for the same reason [`PlanRepo::open_revision`]
/// takes `now`: the catalog "mutates state only in response to explicit
/// authoring calls" (§2.2) and never self-originates a row, so the authoring
/// instant belongs to the request rather than to whichever database node
/// happened to evaluate `now()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlanDraft {
    /// The plan being created.
    pub plan_id: PlanId,
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// Pseudonymous principal id of the authoring actor.
    pub created_by: Uuid,
    /// When the request was authored, UTC.
    pub created_at_utc: OffsetDateTime,
    /// The catalog SKU this plan realizes, when one is bound.
    pub sku_id: Option<Uuid>,
    /// The plan's tier.
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318), or `None` when it has never been named.
    pub plan_name: Option<String>,
    /// The plan's billing cycle.
    pub billing_cycle: Option<BillingCycle>,
    /// The recurring frequency, interval and all.
    pub frequency: Option<Frequency>,
    /// Whether the tier diverges from the parent SKU's under an audited
    /// override (P3).
    pub plan_tier_override: bool,
    /// Minimum purchasable quantity (one-time plans).
    pub purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity (one-time plans).
    pub purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint (D-96).
    pub invoice_grouping_key: Option<String>,
    /// Start of the availability window, UTC.
    pub available_from: Option<OffsetDateTime>,
    /// End of the availability window, UTC.
    pub available_to: Option<OffsetDateTime>,
    /// The plan this one is being cloned from (`inst-cl-copy`), or `None` for an
    /// authored plan. Provenance, set once at create and frozen thereafter.
    pub cloned_from: Option<PlanId>,
    /// The causing request's correlation id (D-178).
    ///
    /// The third field of the [`AuditStamp`](crate::domain::audit::AuditStamp)
    /// this path writes its record under, and the only one the draft did not
    /// already carry: `created_by` is the actor and `created_at_utc` is the
    /// instant. Carried on the draft rather than as a second argument so the
    /// create cannot be called with a correlation belonging to another request.
    pub correlation_id: Uuid,
}

/// `SeaORM`-backed repository over the plan revision chain.
#[derive(Clone)]
pub struct PlanRepo {
    db: DBProvider<DbError>,
}

impl PlanRepo {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Create a plan by inserting its revision `0` in `draft`.
    ///
    /// Revision `0` is written literally rather than minted: creating a plan is
    /// the one call that knows the chain is empty, and the `PRIMARY KEY` is what
    /// tells it it was wrong. Minting `max(revision) + 1` here instead would
    /// turn a retried `POST /plans` on an existing plan id into a silent second
    /// revision of somebody else's plan — the collision below is the answer that
    /// call needs. The consequence, stated rather than designed around: a plan
    /// whose only revision is an abandoned `0` cannot be re-created under the
    /// same id, because the number stays consumed (D-145) and there is no
    /// current revision to open a successor from. Discarding a never-published
    /// plan discards the id with it.
    ///
    /// # Errors
    /// [`RepoError::TimestampPrecisionExceeded`] when an authored availability
    /// bound is finer than the millisecond quantum (D-144);
    /// [`RepoError::ValueOutOfRange`] when an authored purchase quantity or
    /// custom interval is past what its column can hold;
    /// [`RepoError::Db`] on a scope or storage failure — which **includes the
    /// collision case**: a plan id that already has a revision `0` is the
    /// table's `PRIMARY KEY` answer, and a second open draft is
    /// `uq_pricing_plan_open_draft`'s. Neither is pre-checked, because a
    /// pre-check would be a read the insert races with anyway.
    /// [`RepoError::CorruptRow`] from the audit append this transaction also
    /// makes — a segment head whose `row_hash` is not 32 bytes, or a record whose
    /// state cannot be canonicalized. It rolls the revision back with it, which
    /// is the point: a mutation whose record cannot be written must not commit.
    pub async fn create_draft(
        &self,
        scope: &AccessScope,
        draft: NewPlanDraft,
    ) -> Result<PlanRevision, RepoError> {
        // **A transaction, not `conn()`.** `create_draft_on` writes two rows now
        // - the revision and its audit record (D-135) - and `conn()` is the
        // non-transactional runner, so on a bare connection those were two
        // autocommit statements: an append failure left a committed revision with
        // no record of who created it, which is the silently-incomplete trail
        // this writer exists to prevent. `PriceRepo::create_draft` has always
        // supplied its own transaction for the row-and-bands pair; this is the
        // same shape, arrived at for the same reason one file over.
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move { create_draft_on(txn, &scope, draft).await })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Read one revision by its composite identity.
    ///
    /// SQL-level BOLA: a foreign tenant's revision yields `None`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the stored row cannot be read as the
    /// domain value its columns are `CHECK`-constrained to hold.
    pub async fn find_revision(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<Option<PlanRevision>, RepoError> {
        load_revision(&self.conn()?, scope, tenant_id, plan_id, revision).await
    }

    /// Read the plan's **current** revision.
    ///
    /// Current is `published` **or** `retired` (D-128), and the set is taken
    /// from [`LifecycleState::is_current_revision`] rather than restated here,
    /// so this query and the `uq_pricing_plan_current` partial index cannot
    /// drift apart. Retirement flips the only published revision; under the
    /// narrower predicate the projector, the sellability gate and every
    /// referential check would suddenly have no referent at all.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when more than one revision answers — the
    /// partial `UNIQUE` index makes that impossible, so seeing it means the
    /// index is gone and "the current revision" has stopped being well defined.
    pub async fn find_current(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
    ) -> Result<Option<PlanRevision>, RepoError> {
        load_current(&self.conn()?, scope, tenant_id, plan_id).await
    }

    /// Read the plan's open `draft` revision, when it has one.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the stored row cannot be read as the
    /// domain value its columns are `CHECK`-constrained to hold.
    pub async fn find_open_draft(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
    ) -> Result<Option<PlanRevision>, RepoError> {
        load_open_draft(&self.conn()?, scope, tenant_id, plan_id).await
    }

    /// One page of the tenant's plans as their **authoring** revisions.
    ///
    /// [`list_authoring_page`] holds the rule and the reason for it; this is the
    /// entry point for a caller that has a repository rather than a runner.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
    /// value its columns are `CHECK`-constrained to hold.
    pub async fn list_authoring(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        states: &[LifecycleState],
        after: Option<Uuid>,
        limit: u64,
    ) -> Result<Vec<PlanRevision>, RepoError> {
        list_authoring_page(&self.conn()?, scope, tenant_id, states, after, limit).await
    }

    /// One `OData` page: choose the authoring revision, then filter, sort and
    /// paginate. Derived values belong to the read projection, not stored pins.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
    /// malformed `$filter` / `$orderby` / cursor.
    pub async fn list_authoring_odata(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<PlanListEntry>, OdataPageError> {
        plan_list::list(
            &self.conn().map_err(OdataPageError::Repo)?,
            scope,
            tenant_id,
            query,
        )
        .await
    }

    /// Original plan creation time, from the earliest retained revision.
    ///
    /// # Errors
    /// Scope/storage failures and an absent plan fail the read.
    pub async fn created_at(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
    ) -> Result<OffsetDateTime, RepoError> {
        let row = plan::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(plan::Column::TenantId.eq(tenant_id))
                    .add(plan::Column::PlanId.eq(plan_id.get())),
            )
            .order_by(plan::Column::Revision, Order::Asc)
            .one(&self.conn()?)
            .await
            .map_err(|e| RepoError::Db(format!("read plan creation time: {e}")))?
            .ok_or_else(|| RepoError::NotFound {
                subject: "plan".to_owned(),
                id: plan_id.get().to_string(),
            })?;
        Ok(row.created_at_utc)
    }

    /// Apply `patch` to an open draft revision, under the caller's row version.
    ///
    /// One statement does all of it: the patched columns, the
    /// `row_version + 1` bump, and the conjunction that makes it a
    /// compare-and-swap — tenant, plan, revision, the submitted version, and
    /// `lifecycle_state = 'draft'`. An **empty patch is a valid request**: it
    /// asserts the caller's `ETag` and advances it, which is what keeps a no-op
    /// edit distinguishable from a lost one.
    ///
    /// The returned value is a fresh read rather than a computed one, so under
    /// a concurrent second edit it may already carry a later version. The swap
    /// itself is still atomic — what the caller sees is the row as it stands,
    /// which is the only honest answer a non-`RETURNING` path can give.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such revision is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::TimestampPrecisionExceeded`] when a
    /// patched availability bound is finer than the millisecond quantum
    /// (D-144); [`RepoError::ValueOutOfRange`] when a patched purchase quantity
    /// or custom interval is past what its column can hold;
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the updated row reads back unusable.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, \
                  revision, expected-version) the compare-and-swap addresses, the payload, and \
                  the D-135 audit stamp. `swap_guard` already takes those four together, so a \
                  `RevisionTarget` bundling them is the right shape and is owed - it is a \
                  signature change across every storage suite that pins these paths, and it \
                  changes no behaviour"
    )]
    pub async fn update_draft(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        patch: PlanShapePatch,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    update_draft_on(
                        txn, &scope, tenant_id, plan_id, revision, expected, patch, stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Discard an open draft revision, under the caller's row version: the row
    /// flips to the terminal `abandoned` state and keeps its number.
    ///
    /// **It is not a delete, and the difference is the whole operation** (D-145).
    /// Removing the row returned `max(revision)` to what it was before, so the
    /// next opened draft minted the same number — and `(plan_id, revision)` is a
    /// durable name the rest of the set dereferences (the grant table is keyed
    /// by it, every revision-scoped child copies under it, the audit trail
    /// records it). A caller holding the discarded `plan/2`'s row version would
    /// then submit against the **new** `plan/2` and its precondition would pass,
    /// most reachably at the initial version every freshly minted revision
    /// carries: the lost update `fr-concurrent-edit` exists to refuse, arriving
    /// through the key instead of the version.
    ///
    /// The shape is otherwise [`PlanRepo::update_draft`]'s exactly — one
    /// statement, the submitted version matched inside it, `lifecycle_state =
    /// 'draft'` in the same conjunction — and the tag advances with the flip,
    /// because the representation a caller cached really did change. It is the
    /// row's **last** tag: nothing can move a terminal row again, so the value
    /// handed back here, unlike `update_draft`'s, cannot already be stale.
    ///
    /// # The child copies (D-83), and why they go first
    ///
    /// D-145 drops the discarded revision's child copies in the **same
    /// transaction** as the flip: a tombstone that kept its shape would leave a
    /// frozen phase set hanging off a revision no path can reach, and the number
    /// it consumed would still be attached to a shape somebody could read.
    ///
    /// The order is mandatory and the child table's trigger is why. That trigger
    /// permits DML only while the parent revision is `draft`, and `abandoned`
    /// is **not** `draft` — so the DELETE has to happen before the flip or it is
    /// refused, by the guard that exists to stop exactly this table being
    /// rewritten under a frozen revision. Doing it inside the transaction is
    /// what makes the pair atomic: a compare-and-swap that matches nothing rolls
    /// the already-issued DELETE back with it.
    ///
    /// **All three child tables are dropped here** — the phase chain, the add-on
    /// rules and the descriptor set, in that order and every one of them before
    /// the flip. Slice 2 owns no fourth, so D-145's child-copy obligation is
    /// discharged rather than narrowed; what stands behind it now is a
    /// behavioural test per table in `tests/sqlite_plan_repo.rs` rather than the
    /// schema anchor that used to fail the moment a table appeared.
    ///
    /// # The audit record, paid
    ///
    /// D-145 is explicit that the flip "is audited exactly as the deletion was",
    /// and it now is: a record with `action = abandon`, in **this** transaction,
    /// so a compare-and-swap that matches nothing rolls the record back with the
    /// flip and the child deletes.
    ///
    /// An earlier version of this doc carried the debt in prose and named what
    /// paying it needed — "runner-taking forms for all four of those, an actor and
    /// a correlation id threaded through each, and one wave that lands all six
    /// paths at once". That is what happened: [`AuditStamp`] is the actor and the
    /// instant, [`PlanRepo::update_draft`] became transactional to hold its own
    /// record, and the six mutating authoring paths write together. What is
    /// **still** absent is the read surface (`inst-au-read`'s Auditor-only trail,
    /// Slice 12) — the records exist and nothing in this gear serves them.
    ///
    /// # The plan-level refusal, paid by the authoring surface
    ///
    /// This method names the revision it discards, so a caller that names one the
    /// plan does not have is told [`RepoError::NotFound`] — a 404. S2 §5 says
    /// abandoning a plan **that holds no open draft revision** answers
    /// `LIFECYCLE_FORBIDDEN`, which is a different question about a different
    /// subject: it is asked of the *plan*, and answering it means resolving the
    /// plan's open draft first. `POST …/plans/{planId}/abandon`
    /// (`api::rest::plans::abandon_plan_draft`) resolves it over
    /// [`PlanRepo::find_open_draft`], [`PlanRepo::find_current`] and
    /// [`PlanRepo::max_revision`], and discriminates the three answers the
    /// design set asks for: `LIFECYCLE_FORBIDDEN` for a plan with a current
    /// revision and no draft, `PLAN_ABANDONED_NO_SUCCESSOR` for a plan whose
    /// every revision is a tombstone, and a 404 for a plan with no revisions at
    /// all. Nothing about this method changed.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such revision is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen — which is also the
    /// answer to abandoning an already-abandoned revision, naming the state;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the tombstone reads back unusable.
    pub async fn abandon_draft(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
                        return Err(
                            refuse(txn, &scope, tenant_id, plan_id, revision, expected).await
                        );
                    };
                    // Refused before a child row is touched. Each child table's
                    // own trigger answers a DELETE under a frozen revision with
                    // a raw driver error carrying no state, and the caller is
                    // owed the three-way refusal. It is **not** the guard — the
                    // compare-and-swap below is, and a concurrent publish
                    // landing in between is closed by the rollback, not by this
                    // read.
                    if mutable_draft(txn, &scope, tenant_id, plan_id, revision, expected)
                        .await?
                        .is_none()
                    {
                        return Err(
                            refuse(txn, &scope, tenant_id, plan_id, revision, expected).await
                        );
                    }
                    delete_phases(txn, &scope, tenant_id, plan_id, revision).await?;
                    delete_addon_rules(txn, &scope, tenant_id, plan_id, revision).await?;
                    delete_descriptor_set(txn, &scope, tenant_id, plan_id, revision).await?;
                    // Slice 10's composite meters, on the same terms: `abandoned`
                    // is not `draft` and the table's DELETE trigger refuses
                    // everything after the flip.
                    delete_composites(txn, &scope, tenant_id, plan_id, revision).await?;
                    // D-319's period floor/cap set, on the same terms: it is a
                    // revision-scoped child and `abandoned` is not `draft`.
                    delete_period_floor_caps(txn, &scope, tenant_id, plan_id, revision).await?;
                    // Slice 8's three composition tables ride this revision too
                    // (D-92), and `abandoned` is not `draft`: the drop has to
                    // precede the flip for the same reason the three above do.
                    // A plan that is not a bundle drops nothing.
                    bundle_repo::delete_composition(txn, &scope, tenant_id, plan_id, revision)
                        .await?;
                    let result = plan::Entity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .col_expr(
                            plan::Column::LifecycleState,
                            Expr::value(LifecycleState::Abandoned.as_str()),
                        )
                        .col_expr(
                            plan::Column::RowVersion,
                            Expr::col(plan::Column::RowVersion).add(1_i64),
                        )
                        .filter(guard)
                        .exec(txn)
                        .await
                        .map_err(|e| RepoError::Db(format!("abandon plan draft: {e}")))?;

                    if result.rows_affected == 0 {
                        return Err(
                            refuse(txn, &scope, tenant_id, plan_id, revision, expected).await
                        );
                    }
                    let flipped = load_revision(txn, &scope, tenant_id, plan_id, revision)
                        .await?
                        .ok_or_else(|| not_found(plan_id, revision))?;
                    record_revision_mutation(
                        txn,
                        &scope,
                        tenant_id,
                        &flipped,
                        AuditAction::Abandon,
                        expected,
                        stamp,
                    )
                    .await?;
                    Ok(flipped)
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Open the next revision of a published plan: `max(revision) + 1` in
    /// `draft`, `row_version = 0`, the current revision's shape copied forward.
    ///
    /// This is only the **opening** half of D-90. The new revision publishes
    /// through the standard §4.2 path and flips its predecessor `superseded` in
    /// the same commit; that flip is [`publish_revision`]'s, and nothing here
    /// anticipates it.
    ///
    /// # The number comes from the whole chain, not from the current revision
    ///
    /// `max(revision) + 1` over the plan's own rows, which is a different
    /// question from "one past the row this revision succeeds" as soon as the
    /// chain holds a tombstone. Publish revision 1, open revision 2, abandon it:
    /// the current revision is still 1, so deriving from it hands out **2** a
    /// second time — the exact re-mint D-145 forbids, and the one the abandoned
    /// row was kept to prevent. The tombstone is in the chain precisely so this
    /// query can see it.
    ///
    /// # The child copy (D-83), with the ids left alone
    ///
    /// D-83 requires a new revision to **copy its child shape tables** with
    /// stable `phase_id`s, so the `phase` scope-key axis and same-key
    /// supersession survive the revision. `pricing_plan_phase` is copied here:
    /// every phase row of the current revision is written again under the new
    /// `plan_revision`, `phase_id` **unchanged**. Re-minting an id would move
    /// every continuing price row onto a key nothing is filed under — the rows
    /// would stop superseding their predecessors (D-56) and phase coverage
    /// would judge a chain nobody authored.
    ///
    /// The copy **follows** the revision insert and shares its transaction. It
    /// follows because the child table's INSERT trigger reads the *new* parent
    /// and requires it to be `draft`, so there is nothing to insert under until
    /// the revision row exists; it shares the transaction because a revision
    /// that exists without its shape is a draft that looks like a plan whose
    /// author deleted everything.
    ///
    /// `pricing_plan_addon_rule` is copied the same way and in the same
    /// transaction, its `addon_sku_id` unchanged — that id is the registry's
    /// rather than this gear's, so the risk there is not a re-mint but a
    /// **dropped** rule: a successor revision composing with fewer add-ons than
    /// its predecessor is a plan whose buyers lose an entitlement nobody
    /// removed.
    ///
    /// `pricing_plan_descriptor_set` is copied last, and only when the source
    /// revision has one: the successor of an unattached set is an unattached
    /// set, and writing an empty row would tell an authoring surface somebody
    /// had attached something.
    ///
    /// **That completes D-83 for Slice 2** — three child tables, all copied
    /// inside this transaction, beside the revision insert and the **audit
    /// record** of the identity this transaction mints (D-135). Five writes, and
    /// the record is one of them for the reason D-145 gives: a revision number is
    /// permanent, so a number minted with no record of the minting is a name
    /// nobody can account for. The three copy calls are written out rather than
    /// dispatched through a copier trait or a registry: an abstraction over
    /// three known statements hides which tables are covered, and what a reader
    /// needs from this method is exactly that list. When a later slice adds a
    /// revision-scoped child of its own, the compiler will not remind anybody —
    /// so the guard is a test per table in `tests/sqlite_plan_repo.rs`, which is
    /// what replaced the schema anchor that stood here while the tables were
    /// still landing.
    ///
    /// # It voids the plan's pending approval units, and that is a **sixth**
    /// write
    ///
    /// This path appends its audit record directly rather than through
    /// `record_revision_mutation`, so it does not inherit the TOCTOU void the
    /// way the five authoring mutations do — it has to call it, and it does,
    /// inside this transaction.
    ///
    /// What it closes is narrow and worth stating exactly, because the wide
    /// reading is wrong. A unit can only be pinned to a plan's **open draft**
    /// (`infra::publish::assemble` reads that revision and no other), and this
    /// method refuses outright when one exists — so no unit reachable here is
    /// pinned to a subject this call touches. Every unit it can find is one that
    /// survived the draft it pinned. It can never be approved again — the pin
    /// re-derives against whatever draft is open next and `revision` is hashed —
    /// but `inst-as-void` asks for `voided`, not for a record that refuses
    /// forever, and a record that refuses forever is an indefinite lock on the
    /// plan's subject under `PENDING_CHANGE_UNIT_EXISTS`.
    ///
    /// **This is no longer the only closer of that window.** The publish commit voids
    /// the units it did not consume, over the exact revision it froze
    /// ([`approval_repo::void_pending_for_subject`](super::approval_repo::void_pending_for_subject)),
    /// and `abandon_draft` voids through the rail — so between them, every path
    /// by which a draft stops being a draft now closes its own units, and the
    /// set this call can still find is, on today's paths, **empty**.
    ///
    /// It stays anyway, and the reason is the one this module's TOCTOU doc gives
    /// for not trusting an inheritance argument: "on today's paths" is a claim
    /// about a set of surfaces, and the last time it was made — that a mutation
    /// could not reach a pinned subject without passing through the two audit
    /// writers — `open_revision` itself was already the counter-example. A sweep
    /// that costs one `UPDATE` at the first authoring act after a publish is the
    /// backstop for the next one.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when the plan has no current revision — the
    /// same answer a plan outside `scope` gets;
    /// [`RepoError::NoSuccessorRevision`] when the current revision is
    /// `retired`, since a retired revision can never flip `superseded` and a
    /// successor it could never yield to would be unpublishable by
    /// construction; [`RepoError::OpenDraftExists`] naming the
    /// revision that already holds the plan's one editable slot;
    /// [`RepoError::Db`] on a scope or storage failure — which **includes
    /// losing the race** for that slot to a concurrent `open_revision`, since
    /// the three checks above are reads and `uq_pricing_plan_open_draft` is
    /// what actually decides the winner; [`RepoError::CorruptRow`] when the
    /// current revision reads back unusable, when the plan's greatest revision
    /// has no representable successor, or from the audit append — a segment head
    /// whose `row_hash` is not 32 bytes, which rolls the whole open back and is
    /// what `opening_a_successor_whose_record_cannot_be_written_mints_no_revision_number`
    /// asserts.
    pub async fn open_revision(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        // The successor's author and its authoring instant are the stamp's, and
        // so is the correlation its record carries. **That last one is the point
        // of the parameter** (D-178): this method is the first of *two* records a
        // single `PATCH` writes on the arm that opens a successor — this `Create`
        // and then the facet's `Update` — so a value minted here rather than
        // handed in would leave the two halves of one operator call uncorrelated,
        // which is the exact join D-135's per-aggregate segmentation makes the
        // correlation responsible for.
        let created_by = stamp.actor_principal_id;
        let now = stamp.recorded_at;
        let Some(current) = self.find_current(scope, tenant_id, plan_id).await? else {
            return Err(RepoError::NotFound {
                subject: "current plan revision".to_owned(),
                id: plan_id.to_string(),
            });
        };
        // Asked of the state machine rather than matched on `Retired`: what
        // actually blocks a successor is the predecessor's inability to be
        // superseded when that successor publishes.
        if !current
            .lifecycle_state
            .can_transition(LifecycleState::Superseded)
        {
            return Err(RepoError::NoSuccessorRevision {
                plan_id: plan_id.to_string(),
                state: current.lifecycle_state.to_string(),
            });
        }
        if let Some(open) = self.find_open_draft(scope, tenant_id, plan_id).await? {
            return Err(RepoError::OpenDraftExists {
                plan_id: plan_id.to_string(),
                revision: open.revision,
            });
        }

        // Over the whole chain, tombstones included — never `current.revision`,
        // which skips every number an abandoned draft consumed. The fallback is
        // a formality: `find_current` above read one of the very rows this
        // maximum ranges over, and no path deletes a revision row.
        let highest = self
            .max_revision(scope, tenant_id, plan_id)
            .await?
            .unwrap_or(current.revision);
        let next = highest.checked_add(1).ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "plan {plan_id} holds revision {highest}, which has no successor"
            ))
        })?;
        let source = current.revision;
        let opened = PlanRevision {
            plan_id,
            revision: next,
            sku_id: current.sku_id,
            plan_tier: current.plan_tier,
            plan_name: current.plan_name,
            billing_cycle: current.billing_cycle,
            frequency: current.frequency,
            plan_tier_override: current.plan_tier_override,
            purchase_min_qty: current.purchase_min_qty,
            purchase_max_qty: current.purchase_max_qty,
            invoice_grouping_key: current.invoice_grouping_key,
            available_from: current.available_from,
            available_to: current.available_to,
            // Carried forward with the rest of the content: a new revision opens
            // as a copy of the current one, and an edge list that reset itself
            // here would silently drop a plan out of self-service change on the
            // next edit of any unrelated field.
            entitlement_grants: current.entitlement_grants,
            change_contract: current.change_contract,
            lifecycle_state: LifecycleState::Draft,
            created_by,
            created_at_utc: now,
            // Lineage is the plan's rather than one revision's, so it carries
            // forward: a second revision of a cloned plan is still cloned.
            cloned_from: current.cloned_from,
            row_version: RowVersion::new(0),
        };
        // Rendered before the transaction opens: a value no column can hold is
        // a caller mistake, and there is no reason to hold a transaction open
        // to discover it.
        let row = revision_model(tenant_id, &opened)?;

        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    insert_revision(txn, &scope, row).await?;
                    copy_phases(txn, &scope, tenant_id, plan_id, source, next).await?;
                    copy_addon_rules(txn, &scope, tenant_id, plan_id, source, next).await?;
                    copy_descriptor_set(txn, &scope, tenant_id, plan_id, source, next).await?;
                    // Slice 10's composites, with `composite_id` preserved
                    // (D-106) so a formula edit on the draft leaves the published
                    // revision byte-identical.
                    copy_composites(txn, &scope, tenant_id, plan_id, source, next).await?;
                    // D-319's period floor/cap set. Copied like the four above
                    // and after the new revision row exists, the table refusing
                    // an INSERT whose parent revision is not `draft`.
                    copy_period_floor_caps(txn, &scope, tenant_id, plan_id, source, next).await?;
                    // Slice 8's composition, on the same terms and after the new
                    // revision row exists: these tables refuse an INSERT whose
                    // parent revision is not `draft`.
                    bundle_repo::copy_composition(txn, &scope, tenant_id, plan_id, source, next)
                        .await?;
                    // The record of the identity this transaction minted, in the
                    // transaction that minted it. The stamp is the call's own -
                    // `created_by` is the actor and `now` the instant - so this
                    // path needs nothing threaded that it did not already have.
                    //
                    // `before_state` is None because the revision did not exist:
                    // the same absence `create_draft_on` writes for revision 0,
                    // and the difference between minting a name and editing one.
                    // And the void, called **directly** because this path does
                    // not go through `record_revision_mutation`; see the method
                    // doc's own paragraph on why it voids at all.
                    crate::infra::approval::void_pending_units_of(
                        txn, &scope, tenant_id, plan_id, now,
                    )
                    .await?;
                    audit_repo::append(
                        txn,
                        &scope,
                        NewAuditEntry {
                            tenant_id,
                            chain_id: audit_repo::plan_chain(plan_id),
                            recorded_at: now,
                            actor_principal_id: created_by,
                            action: AuditAction::Create,
                            subject_kind: AuditSubjectKind::PlanRevision,
                            subject_ref: audit_repo::plan_revision_ref(plan_id, next),
                            before_state: None,
                            after_state: Some(subject_state(
                                opened.lifecycle_state,
                                opened.row_version.get(),
                                None,
                            )),
                            approval_ref: None,
                            correlation_id: stamp.correlation_id,
                        },
                    )
                    .await?;
                    Ok(opened)
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// The greatest `revision` the plan holds — **tombstones included**, which
    /// is the entire reason this is a query over the chain rather than a read of
    /// the current revision.
    ///
    /// Taken as one ordered row rather than as a `MAX()` aggregate so it runs
    /// through the same scoped path as every other read here: the maximum has to
    /// be the maximum *the caller can see*, and the ordered read is served by
    /// `idx_pricing_plan_tenant`, whose trailing column is `revision`.
    ///
    /// **Public because `None` is a fact the authoring surface needs and cannot
    /// get any other way.** A plan with no current revision and no open draft is
    /// either absent (`404`) or spent — every revision `abandoned`,
    /// `PLAN_ABANDONED_NO_SUCCESSOR` (D-145 as amended) — and the only thing
    /// separating the two is whether the chain holds a row at all. The
    /// alternatives were a fifth lifecycle state or answering the spent plan a
    /// not-found, which is what the surface used to do and what `02` §5 names as
    /// the defect.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored revision number is not a count.
    pub async fn max_revision(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
    ) -> Result<Option<u64>, RepoError> {
        max_revision_on(&self.conn()?, scope, tenant_id, plan_id).await
    }

    /// The non-transactional runner, named once so every read path here spells
    /// the failure the same way.
    fn conn(&self) -> Result<DbConn<'_>, RepoError> {
        self.db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Transaction plumbing.
// ---------------------------------------------------------------------------

/// Append the audit record of one plan-revision mutation, inside the mutation's
/// own transaction.
///
/// `expected` is the version the caller presented, which is the revision's
/// **before** version by construction — the compare-and-swap matched on it — so
/// the before/after pair needs no second read of a row the transaction has
/// already moved.
///
/// # This is also the TOCTOU void's rail (`inst-ap-pin`)
///
/// Every authoring mutation of a plan's **shape** lands here — the plan facet,
/// the phase graph, the add-on rule set, the descriptor set, the derived meters
/// (Slice 10), a bundle's composition (Slice 8, D-92, which rides its plan's
/// revision) and the draft abandon — so a pending approval unit pinned to that
/// plan is voided here, in the mutation's own transaction.
/// `price_repo::record_price_mutation` is the same rail for the row plane. The
/// list is written out rather than counted: a facet joining the rail inherits the
/// void correctly, which is the property the placement buys, and a number in the
/// prose beside it would be the only thing that went stale.
///
/// Placed on the audit writer rather than at each of those seven call sites for
/// the reason the whole rule exists: this phase permits **no unaudited mutating
/// entry point**, so a surface a later slice adds cannot reach a pinned subject
/// without passing through here, and it inherits the void instead of needing to
/// remember it. The full surface enumeration, including the two paths that
/// deliberately do **not** void, is in
/// [`crate::infra::approval::void_pending_units_of`]'s doc.
///
/// The plan's **creation** paths ([`create_draft_on`] and
/// [`PlanRepo::create_draft`]) append their record directly and not through this
/// function, which is why `create_plan` does not void: no unit can be pinned to
/// a plan that did not exist.
///
/// # And it is the `PlanUpdated` rail, for the same reason
///
/// S2 §7 puts `PlanUpdated` "on authoring" and S10 §7 has the advanced
/// primitives "ride `PlanUpdated`" rather than mint a name; PRD AC #98 emits it
/// "after mutation". Every one of those mutations already passes through here,
/// so the producer is here too — at the choke point rather than at the six of
/// the seven call sites above that carry an edit, exactly as the void is, and
/// for the identical reason: a facet a later slice adds inherits the
/// announcement instead of having to remember it. Six hand-placed enqueues is
/// the shape that leaves the seventh silent.
///
/// **It is emitted on [`AuditAction::Update`] and on nothing else.** The other
/// action reaching this rail is [`AuditAction::Abandon`], and D-145's whole point
/// is that discarding a draft is a terminal lifecycle flip rather than an edit:
/// the frozen set has no name for it, and announcing it as a content change would
/// tell a consumer to re-read a revision that just stopped being readable. The
/// discrimination is on the act, not on the caller, so a future action added to
/// this rail is silent until someone decides what it announces.
///
/// # Errors
/// [`RepoError::Db`] or [`RepoError::CorruptRow`] from the chain append, both of
/// which roll the mutation back with them. That is the point: a mutation whose
/// record cannot be written must not commit, or the trail becomes silently
/// incomplete. [`RepoError::Db`] from the void, for the same reason one construct
/// over: a mutation that could not close the approval it invalidated must not
/// commit either. [`RepoError::ConcurrentMutation`] from the enqueue, which is
/// the same posture a third time: an edit that committed without its event would
/// leave a consumer resolving a draft nobody told it had moved.
// The choke point every audited revision mutation passes through, on three
// repositories — so one span here names the act for the append, the approval void
// and the outbox row it performs, whichever repository entered it.
#[tracing::instrument(
    skip_all,
    fields(tenant_id = %tenant_id, plan_id = %after.plan_id, revision = after.revision, action = %action)
)]
pub(super) async fn record_revision_mutation(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    after: &PlanRevision,
    action: AuditAction,
    expected: RowVersion,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    crate::infra::approval::void_pending_units_of(
        runner,
        scope,
        tenant_id,
        after.plan_id,
        stamp.recorded_at,
    )
    .await?;
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::plan_chain(after.plan_id),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action,
            subject_kind: AuditSubjectKind::PlanRevision,
            subject_ref: audit_repo::plan_revision_ref(after.plan_id, after.revision),
            before_state: Some(subject_state(LifecycleState::Draft, expected.get(), None)),
            after_state: Some(subject_state(
                after.lifecycle_state,
                after.row_version.get(),
                None,
            )),
            approval_ref: None,
            correlation_id: stamp.correlation_id,
        },
    )
    .await?;
    if action != AuditAction::Update {
        return Ok(());
    }
    outbox_repo::enqueue(
        runner,
        scope,
        NewOutboxEvent::plan_updated(
            tenant_id,
            &PlanUpdatedPayload {
                plan_id: after.plan_id,
                revision: after.revision,
                row_version: after.row_version.get(),
                correlation_id: stamp.correlation_id,
            },
            stamp.recorded_at,
        ),
    )
    .await
    .map(|_| ())
}

/// Flatten the transaction wrapper back into this repository's vocabulary.
///
/// The body's error type is [`RepoError`] rather than the driver's precisely so
/// a typed refusal survives the rollback: an `abandon_draft` that met a frozen
/// revision has to reach the caller as [`RepoError::NotDraft`], not as "the
/// store failed". What arrives as infrastructure is only what happens outside
/// the body — beginning the transaction, and committing it.
/// `pub(super)` because [`bundle_repo`](super::bundle_repo) opens transactions of
/// the same shape and must render their failures the same way.
pub(super) fn tx_failure(err: TxError<RepoError>) -> RepoError {
    err.into_domain(|infra| RepoError::Db(format!("plan revision transaction: {infra}")))
}

// ---------------------------------------------------------------------------
// Statements, taken through whichever runner the caller holds.
//
// Free functions rather than methods because half their callers are inside a
// transaction, where `Db::conn()` is refused outright by the toolkit's
// transaction-bypass guard — and because `plan_shape_repo` writes the child
// rows of these very revisions under the same compare-and-swap.
// ---------------------------------------------------------------------------

/// [`PlanRepo::max_revision`] on a runner the caller owns.
///
/// A caller inside a transaction needs this reading to be **of that
/// transaction** — a guard that opened its own connection would judge a world
/// its own write is not part of, and on an idempotent door it would judge one
/// the replay it is guarding has already moved.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored revision number is not a count.
pub async fn max_revision_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<u64>, RepoError> {
    let row = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get())),
        )
        .order_by(plan::Column::Revision, Order::Desc)
        .limit(1)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read greatest plan revision: {e}")))?;
    row.map(|row| {
        u64::try_from(row.revision).map_err(|e| {
            RepoError::CorruptRow(format!(
                "pricing_plan row for plan {plan_id} holds revision {}: {e}",
                row.revision
            ))
        })
    })
    .transpose()
}

/// Create a plan's revision `0` through whichever runner the caller holds.
///
/// **Every caller supplies a transaction, and none may not.** This function
/// writes **two** rows — the revision and its audit record (D-135) — so a bare
/// connection would make them two autocommit statements and an append failure
/// would leave a committed revision nobody recorded creating.
/// [`PlanRepo::create_draft`] therefore opens one of its own (it used to delegate
/// here on a plain connection, which is what the audit record made unsafe). The
/// other callers are `infra::idempotent`, reached through `api::rest::plans`, and
/// `infra::clone` since D-275; the first has to run the insert inside the
/// **same** transaction as the idempotency claim that guards it — split
/// across two transactions the claim commits while the mutation does not, and
/// every retry of a request that never happened is answered "already done",
/// forever (`idempotency_repo`'s module doc). One implementation with two
/// entry points rather than two spellings of one insert, which is
/// [`load_current`]'s reason applied to a write: the seam and the direct caller
/// must issue the same statements, and the proof is that the pre-existing
/// `tests/sqlite_plan_repo.rs` suite still pins this path unedited.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] when an authored availability bound
/// is finer than the millisecond quantum (D-144);
/// [`RepoError::ValueOutOfRange`] when an authored purchase quantity or custom
/// interval is past what its column can hold; [`RepoError::Db`] on a scope or
/// storage failure, including the primary-key collision
/// [`PlanRepo::create_draft`]'s own doc describes.
pub async fn create_draft_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    draft: NewPlanDraft,
) -> Result<PlanRevision, RepoError> {
    create_granted_draft_on(runner, scope, draft, AuthoredGrants::default()).await
}

/// The two governed facets a create may author alongside its draft.
///
/// A type rather than two more members on [`NewPlanDraft`], and the reason is a
/// boundary rather than a taste: `NewPlanDraft` is constructed as an exhaustive
/// literal at a dozen sites across `infra::clone`, `infra::repricing` and nine test
/// suites, so widening it is a change to all of them, while what the authoring
/// surface needed was one call able to say "and these two columns as well".
///
/// [`Default`] is the fail-safe both columns already had: no grants, and no
/// self-service change (`inst-pc-failsafe`), which is what [`create_draft_on`]
/// passes and what every path that authors neither gets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthoredGrants {
    /// The entitlement grant set (Slice 6 §6, D-41).
    pub entitlement_grants: EntitlementGrants,
    /// The plan-change contract (Slice 6 §6, D-113).
    pub change_contract: PlanChangeContract,
}

/// [`create_draft_on`], with the two governed facets the caller authored.
///
/// **One implementation, two entry points**, which is [`load_current`]'s discipline
/// applied to a write: the seam and the direct caller issue the same statements, and
/// the only difference between the two forms is what the two columns hold.
///
/// It exists because `POST /plans` was **accepting both members and storing
/// neither**. `PlanShapeRequest` carries them, the create digests the whole body into
/// its idempotency hash, and `shape_of` mapped eleven of its thirteen members — so a
/// caller authoring feature flags, quotas or a change contract was answered `201` for
/// a plan holding none of them, invisibly:
/// [`PlanView`](crate::api::rest::plans::PlanView) renders neither member, so there
/// was no response field in which the loss could be seen. `PATCH` honoured both
/// through [`patched_columns`], which made one request type mean two different things
/// on two verbs.
///
/// **In the create's own `INSERT`, not a follow-up `UPDATE`.** `infra::clone` patches
/// these two columns after its create because its grant map needs the phase remap
/// that only exists by then; a plain authored create has both values in hand at the
/// insert, and writing them here keeps revision 0 at row version **0** and leaves one
/// audit record. A plan that came into existence at version 1 would be one born
/// carrying an edit nobody made, and the `If-Match` the create hands back is the tag
/// a client's next call sends.
///
/// # Errors
/// [`create_draft_on`]'s, verbatim: this is that function.
pub async fn create_granted_draft_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    draft: NewPlanDraft,
    granted: AuthoredGrants,
) -> Result<PlanRevision, RepoError> {
    let tenant_id = draft.tenant_id;
    let correlation_id = draft.correlation_id;
    check_authored_instant("availableFrom", draft.available_from)?;
    check_authored_instant("availableTo", draft.available_to)?;
    let opened = PlanRevision {
        plan_id: draft.plan_id,
        revision: 0,
        sku_id: draft.sku_id,
        plan_tier: draft.plan_tier,
        plan_name: draft.plan_name,
        billing_cycle: draft.billing_cycle,
        frequency: draft.frequency,
        plan_tier_override: draft.plan_tier_override,
        purchase_min_qty: draft.purchase_min_qty,
        purchase_max_qty: draft.purchase_max_qty,
        invoice_grouping_key: draft.invoice_grouping_key,
        available_from: draft.available_from,
        available_to: draft.available_to,
        // **What the caller authored, never `default()`.** `NewPlanDraft` carries no
        // field for it -- the field is on `AuthoredGrants`, for the reason stated there
        // -- but the create's own request type carries both members, so writing the
        // default here would accept them and throw them away.
        //
        // `AuthoredGrants::default()` is still what every other caller passes, and it
        // is still the fail-safe it was: a plan nobody has authored edges on offers no
        // self-service change (`inst-pc-failsafe`).
        entitlement_grants: granted.entitlement_grants,
        change_contract: granted.change_contract,
        lifecycle_state: LifecycleState::Draft,
        created_by: draft.created_by,
        created_at_utc: draft.created_at_utc,
        cloned_from: draft.cloned_from,
        row_version: RowVersion::new(0),
    };
    let row = revision_model(tenant_id, &opened)?;
    insert_revision(runner, scope, row).await?;
    // The record, in the same transaction as the insert (D-135). The stamp is
    // the draft's own: `created_by` is the actor, `created_at_utc` is the instant
    // and `correlation_id` is the request's (D-178), which the draft carries for
    // exactly this line.
    // `before_state` is None because nothing existed - which is the difference
    // between a create and an edit, and it is expressible only as an absence.
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::plan_chain(opened.plan_id),
            recorded_at: opened.created_at_utc,
            actor_principal_id: opened.created_by,
            action: AuditAction::Create,
            subject_kind: AuditSubjectKind::PlanRevision,
            subject_ref: audit_repo::plan_revision_ref(opened.plan_id, opened.revision),
            before_state: None,
            after_state: Some(subject_state(
                opened.lifecycle_state,
                opened.row_version.get(),
                None,
            )),
            approval_ref: None,
            correlation_id,
        },
    )
    .await?;
    // `PlanCreated`, in the same transaction as the insert and the record, for
    // the reason the outbox exists: an event exists if and only if its commit
    // happened, so a create that rolls back announces nothing and one that
    // commits cannot fail to announce.
    //
    // **Here rather than at the two call sites.** `create_draft_on` is the only
    // implementation of "a plan comes into existence" — `api::rest::plans` reaches
    // it through the idempotency guard and `infra::clone` reaches it directly
    // (D-275) — so a producer at the surface would have announced authored plans
    // and stayed silent about cloned ones, which is the asymmetry across two doors
    // that this event's absence already was across two planes.
    outbox_repo::enqueue(
        runner,
        scope,
        NewOutboxEvent::plan_created(
            tenant_id,
            &PlanCreatedPayload {
                plan_id: opened.plan_id,
                revision: opened.revision,
                correlation_id,
            },
            opened.created_at_utc,
        ),
    )
    .await?;
    Ok(opened)
}

/// Read one revision's stored row by its composite identity, scoped.
async fn load_revision_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Option<plan::Model>, RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(None);
    };
    plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(number)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read plan revision: {e}")))
}

/// D-90's flip, both halves of it, inside the caller's transaction.
///
/// Two moves in **this order**, and the order is forced by an index rather than
/// chosen:
///
/// 1. the plan's current revision, if it has one, `published -> superseded`;
/// 2. the named draft revision `draft -> published`, under a compare-and-swap
///    on its row version.
///
/// **Publishing the draft first does not work.** `uq_pricing_plan_current` is
/// partial on `lifecycle_state IN ('published','retired')`, and both backends
/// evaluate a unique index per statement rather than deferring to commit — so
/// for the duration of that first statement two rows of one plan would satisfy
/// the predicate and the index would reject. Demoting the predecessor first
/// leaves the slot empty for its successor. A later reader "simplifying" the
/// order reintroduces exactly that.
///
/// **The demotion must not touch `row_version`.** The plan table's frozen-column
/// guard fires on any UPDATE of a non-draft row that moves a listed column, and
/// `row_version` is on that list — the entity tag freezes with the content it
/// names (D-141's argument, applied to the plan's own row). The draft's flip
/// **does** bump it, because that row is still `draft` when the statement runs
/// and the representation a caller cached really did change.
///
/// **Takes a transaction handle, not a runner, and the type is the contract.**
/// This call performs *two* statements that must not be separable: on a bare
/// connection a compare-and-swap failing after the demotion leaves the plan with
/// **no current revision at all** — the exact defect the `mutable_draft` read
/// below was added to close, reachable again through the runner. The
/// [`IdempotencyGate`](super::idempotency_repo::IdempotencyGate) takes a
/// transaction handle for the same reason, and says so in the same words. It
/// still opens no transaction of its own: it is one step of the publish commit,
/// and the flips, the version request, the outbox row and the audit row either
/// all commit or none do.
///
/// # Errors
/// [`RepoError::NoSuccessorRevision`] when the plan's current revision can never
/// be superseded — a `retired` plan, asked of the state machine rather than
/// matched on, exactly as [`PlanRepo::open_revision`] asks it;
/// [`RepoError::NotFound`] when the named revision is not visible to `scope`;
/// [`RepoError::NotDraft`] when it is visible but already frozen — which is what
/// a second publish of one revision gets, and is why the revision publishes at
/// most once; [`RepoError::StaleRowVersion`] carrying both versions when the
/// submitted one is not current; [`RepoError::Db`] on a scope or storage
/// failure; [`RepoError::ConcurrentMutation`] on **losing the current-revision slot
/// to a concurrent publish** between the read and the demotion — recognised from
/// `uq_pricing_plan_current`'s own violation as of D-159, so the loser is told to
/// retry rather than told the store failed; [`RepoError::CorruptRow`] when the
/// published row reads back unusable.
pub async fn publish_revision(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
) -> Result<PlanRevision, RepoError> {
    // Read before anything is demoted. It is **not** the guard — the
    // compare-and-swap below is, and a concurrent writer landing in between is
    // closed by the caller's rollback.
    //
    // What it buys is precision and a wasted write avoided. Without it a repeat
    // publish of an already-published revision finds that revision *as the
    // current one*, demotes it, and only then discovers it is not a draft. That
    // demotion is no longer *observable* — this function takes a `DbTx`, so it
    // rolls back with the refusal — but it is still a write issued on the way to
    // saying no, taken against the very row the caller was asking about.
    // [`PriceRepo::update_draft`] takes the identical read for the identical
    // reason.
    if mutable_draft(txn, scope, tenant_id, plan_id, revision, expected)
        .await?
        .is_none()
    {
        return Err(refuse(txn, scope, tenant_id, plan_id, revision, expected).await);
    }

    if let Some(current) = load_current(txn, scope, tenant_id, plan_id).await? {
        // Asked of the state machine rather than matched on `Retired`: what
        // blocks the successor is the predecessor's inability to be superseded.
        if !current
            .lifecycle_state
            .can_transition(LifecycleState::Superseded)
        {
            return Err(RepoError::NoSuccessorRevision {
                plan_id: plan_id.to_string(),
                state: current.lifecycle_state.to_string(),
            });
        }
        supersede_current(txn, scope, tenant_id, plan_id, current.revision).await?;
    }

    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        return Err(refuse(txn, scope, tenant_id, plan_id, revision, expected).await);
    };
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Published.as_str()),
        )
        .col_expr(
            plan::Column::RowVersion,
            Expr::col(plan::Column::RowVersion).add(1_i64),
        )
        .filter(guard)
        .exec(txn)
        .await
        .map_err(|e| {
            // D-159's third serialization point: `uq_pricing_plan_current`. Two
            // publishes racing to make their revision the plan's current one -
            // the index is what decides it, and the loser's remedy is to
            // re-validate against the revision that won.
            contention_or_db(&e, &format!("plan {plan_id}"), "publish plan revision")
        })?;
    if result.rows_affected == 0 {
        return Err(refuse(txn, scope, tenant_id, plan_id, revision, expected).await);
    }
    load_revision(txn, scope, tenant_id, plan_id, revision)
        .await?
        .ok_or_else(|| not_found(plan_id, revision))
}

/// Demote the plan's published revision, leaving its entity tag where it is.
///
/// The predicate names `published` explicitly rather than reusing
/// [`current_tokens`]: `retired` is refused by the caller above with a refusal
/// an operator can act on, and a predicate that swept it in would produce the
/// forbidden `retired -> superseded` edge as a raw trigger error instead.
#[tracing::instrument(skip_all, fields(tenant_id = %tenant_id, plan_id = %plan_id, revision))]
async fn supersede_current(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} holds a current revision {revision} no column can address"
        )));
    };
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Superseded.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(number))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("supersede plan revision: {e}")))?;
    if result.rows_affected == 0 {
        // **Contention, not a storage fault.** Zero rows here means only that a
        // concurrent publish took the current-revision slot between
        // `load_current` and this statement -- which the message below has said
        // all along while the variant said 500. `retire_revision` classifies the
        // identical zero-row condition as contention, and this is the same race
        // one verb over.
        //
        // Logged as well as answered: the caller's 409 is the caller's business,
        // but a lost race for the current-revision slot is nobody's mistake, and
        // its rate is what tells an operator that two publishers are fighting.
        // `repo_failure` logs the internal arm only, so a refusal leaving through
        // it is otherwise invisible from the server's side.
        tracing::warn!(
            tenant_id = %tenant_id,
            plan_id = %plan_id,
            revision,
            "bss-pricing: a concurrent publish took the current-revision slot before the supersede"
        );
        return Err(RepoError::ConcurrentMutation {
            aggregate: format!("plan {plan_id}"),
        });
    }
    Ok(())
}

/// Read the plan's **current** revision through whichever runner the caller
/// holds.
///
/// Public and runner-taking because the publish path reads it **twice** and the
/// second read is inside the commit transaction, where `Db::conn()` is refused
/// outright by the toolkit's transaction-bypass guard. One implementation with
/// two entry points, rather than a second query the two runs could disagree
/// about — the disagreement being exactly what §4.2's re-validation exists to
/// detect.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when more than one revision answers — the partial
/// `UNIQUE` index makes that impossible, so seeing it means the index is gone
/// and "the current revision" has stopped being well defined.
pub async fn load_current(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<PlanRevision>, RepoError> {
    let rows = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::LifecycleState.is_in(current_tokens())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read current plan revision: {e}")))?;
    if rows.len() > 1 {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} holds {} current revisions; uq_pricing_plan_current permits one",
            rows.len()
        )));
    }
    rows.into_iter().next().map(to_domain).transpose()
}

/// Does this `plan_id` name a plan **the caller can read** — any revision of it,
/// in any lifecycle state?
///
/// The weakest question about a plan reference there is, and deliberately so.
/// Its one caller, [`bundle_repo::create_on`](super::bundle_repo::create_on),
/// is resolving a `plan_id` a client put in a request body against a table that
/// carries **no foreign key onto `pricing_plan`** — `pricing_bundle`'s module
/// doc explains why one is not expressible — so what it needs to know is whether
/// the reference resolves inside the caller's own scope, and nothing more.
/// Narrowing it to a *current* or *draft* revision would be a second, unstated
/// rule about which plans may be bundled; that is a product decision and this is
/// an isolation check.
///
/// Returned as a `bool` rather than as a [`PlanRevision`] because there is no one
/// revision to return: a plan holds a chain, the bundle row is not
/// revision-scoped (D-92 puts the revision discipline on the three composition
/// tables, not on the identity row), and handing back "some revision" would invite
/// a caller to read a lifecycle state off a row chosen arbitrarily.
///
/// The scope is what makes the answer a refusal worth having: `scope_with` binds
/// `tenant_id` in SQL, so a plan in another tenant answers `false` exactly as an
/// absent one does — the fold [`RepoError::NotFound`]'s own doc argues for.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn holds_a_revision(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<bool, RepoError> {
    let found = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get())),
        )
        .limit(1)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read plan by id: {e}")))?;
    Ok(found.is_some())
}

/// Read the plan's open `draft` revision through whichever runner the caller
/// holds; see [`load_current`] for why this shape exists.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when the stored row cannot be read as the domain
/// value its columns are `CHECK`-constrained to hold.
pub async fn load_open_draft(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<PlanRevision>, RepoError> {
    let row = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Draft.as_str())),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read open plan draft: {e}")))?;
    row.map(to_domain).transpose()
}

/// How many plans the tenant holds in each **authoring** state (D-360).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanCounts {
    /// The number of distinct plans in the authoring set; the three states sum
    /// to it.
    pub total: u64,
    /// Plans holding an open draft — whatever their current revision's state.
    pub draft: u64,
    /// Plans whose current revision is `published` and that hold no draft.
    pub published: u64,
    /// Plans whose current revision is `retired` and that hold no draft.
    pub retired: u64,
}

/// [`PlanCounts`] over the **whole** catalogue — what the list shows per row,
/// counted rather than paged (D-360).
///
/// The row set is [`list_authoring_page`]'s empty-filter set — `current_tokens()`
/// plus `draft` — and the fold is its collapse, stated once: a plan with an open
/// draft **is** `draft` (the draft is the revision an author holds; its number is
/// `max + 1`), else it is its current revision's state. At most two rows per
/// plan reach this read (`uq_pricing_plan_current` admits one current revision,
/// `OPEN_DRAFT_REVISION_EXISTS` one open draft), so it is bounded by 2 × plans,
/// and `total` is the number of distinct plans. `abandoned` and `superseded`
/// revisions are outside the set, as they are outside the list.
///
/// A tab strip built from one page of `GET /plans` is wrong past that page; this
/// is the read that is not.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] on
/// a state token outside the enumeration.
/// The plans of one tenant that hold an open draft — the anti-join
/// [`count_plans_by_authoring_state`] needs to keep its own count at
/// `O(states)`.
///
/// A subquery rather than a second round trip, and **not** scope-narrowed, for
/// `window_repo::projected_price_rows`' reason: it yields `plan_id`s of one
/// named tenant, `plan_id` is the table's own key, and it is intersected with a
/// **scoped** outer read — so it cannot admit a plan the outer scope did not
/// already admit.
fn plans_holding_a_draft(tenant_id: Uuid) -> sea_orm::sea_query::SelectStatement {
    plan::Entity::find()
        .select_only()
        .column(plan::Column::PlanId)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Draft.as_str())),
        )
        .into_query()
}

/// [`PlanCounts`] for the whole catalogue, counted **in SQL**.
///
/// # Why two grouped reads and not one fold in Rust
///
/// `GET /plans/counts` is a tab strip: it fires on every plan-screen open. An
/// earlier version selected every `(plan_id, lifecycle_state)` row of the
/// tenant's authoring set — no `LIMIT`, no cursor, no aggregate — and collapsed
/// them into a `BTreeMap` here, so the transfer grew with the catalogue on a
/// route whose whole answer is four integers, while every other list read in the
/// crate is capped at D-125's 1 000. Both reads below return **at most one row
/// per lifecycle state**.
///
/// # Why `COUNT(*)` per state is already the distinct-plan count
///
/// The schema says so, not a convention: `uq_pricing_plan_open_draft` is unique
/// on `plan_id` where the state is `draft`, and `uq_pricing_plan_current` is
/// unique on `plan_id` across `('published','retired')` **together**. So inside
/// this filter a plan contributes at most one draft row and at most one current
/// row, and no `DISTINCT` is needed to count plans.
///
/// # The collapse, expressed as a subtraction
///
/// A plan holding an open draft counts as `draft` whatever its current revision
/// says (`PlanCounts`' own contract). The first read counts plans per state
/// independently; the second counts, per **current** state, the plans that also
/// hold a draft — the ones the first read has already counted under `draft`. So
/// `published` and `retired` are their totals minus that shadow, `draft` stands,
/// and the three sum to `total` because every plan lands in exactly one bucket.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure, or when a tally names a state
/// this function's own filter excludes.
pub async fn count_plans_by_authoring_state(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<PlanCounts, RepoError> {
    #[derive(sea_orm::FromQueryResult)]
    struct Tally {
        lifecycle_state: String,
        cnt: i64,
    }

    let mut tokens = current_tokens();
    tokens.push(LifecycleState::Draft.as_str());
    let per_state = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::LifecycleState.is_in(tokens)),
        )
        .project_all(runner, |q| {
            q.select_only()
                .column(plan::Column::LifecycleState)
                .column_as(Expr::col(plan::Column::PlanId).count(), "cnt")
                .group_by(plan::Column::LifecycleState)
                .into_model::<Tally>()
        })
        .await
        .map_err(|e| RepoError::Db(format!("count plans by authoring state: {e}")))?;

    let shadowed = plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::LifecycleState.is_in(current_tokens()))
                .add(plan::Column::PlanId.in_subquery(plans_holding_a_draft(tenant_id))),
        )
        .project_all(runner, |q| {
            q.select_only()
                .column(plan::Column::LifecycleState)
                .column_as(Expr::col(plan::Column::PlanId).count(), "cnt")
                .group_by(plan::Column::LifecycleState)
                .into_model::<Tally>()
        })
        .await
        .map_err(|e| RepoError::Db(format!("count plans shadowed by a draft: {e}")))?;

    let tallied = |tally: &Tally| -> Result<(LifecycleState, u64), RepoError> {
        let state = read_token(
            "pricing_plan.lifecycle_state",
            &tally.lifecycle_state,
            LifecycleState::ALL,
            LifecycleState::as_str,
        )?;
        Ok((state, u64::try_from(tally.cnt).unwrap_or(0)))
    };
    // **Every variant named, and no wildcard.** The token has already been
    // validated against `LifecycleState::ALL` and selected by this function's own
    // filter, so a state outside the authoring set is not a corrupt row - it is
    // this function disagreeing with its own filter. Spelling the arms out makes
    // a sixth state a compile error here instead of a runtime alarm on a tab
    // strip.
    let mut counts = PlanCounts::default();
    for tally in &per_state {
        match tallied(tally)? {
            (LifecycleState::Draft, n) => counts.draft = n,
            (LifecycleState::Published, n) => counts.published = n,
            (LifecycleState::Retired, n) => counts.retired = n,
            (state @ (LifecycleState::Abandoned | LifecycleState::Superseded), _) => {
                return Err(RepoError::Db(format!(
                    "pricing_plan authoring tally met `{state}`, which this read's own \
                    lifecycle_state filter excludes"
                )));
            }
        }
    }
    for tally in &shadowed {
        match tallied(tally)? {
            (LifecycleState::Published, n) => counts.published = counts.published.saturating_sub(n),
            (LifecycleState::Retired, n) => counts.retired = counts.retired.saturating_sub(n),
            (
                state @ (LifecycleState::Draft
                | LifecycleState::Abandoned
                | LifecycleState::Superseded),
                _,
            ) => {
                return Err(RepoError::Db(format!(
                    "pricing_plan draft-shadow tally met `{state}`, which this read's own \
                    current-state filter excludes"
                )));
            }
        }
    }
    counts.total = counts.draft + counts.published + counts.retired;
    Ok(counts)
}

/// One page of the tenant's plans, each rendered as its **authoring** revision.
///
/// `plans.rs::authoring_revision` decides, per plan, that the open draft
/// outranks the current revision; this is that rule over a page rather than over
/// one id, and it exists because [`LifecycleState::is_current_revision`] is
/// `Published | Retired`: a listing over [`current_tokens`] would omit every
/// plan whose only revision is the draft somebody is authoring right now, which
/// is precisely the plan an authoring surface most needs to enumerate.
///
/// The rule is expressed as an ordering rather than as a second query.
/// `plan_id ASC, revision DESC` puts each plan's highest surviving revision
/// first, and a draft is always higher than the revision it reopened —
/// [`PlanRepo::open_revision`] mints `max(revision) + 1` over the whole chain —
/// so the **first** row of each plan is already the authoring one. Keeping only
/// that row is the whole collapse.
///
/// `states` narrows to the caller's lifecycle filter; an **empty** slice is the
/// authoring set — the current tokens plus `draft` — rather than nothing, for
/// the reason `approval_repo::list_page` states: a caller that named no filter
/// asked for everything it could act on.
///
/// # Why the walk re-fetches rather than over-fetching once
///
/// Under the empty filter a plan contributes at most two candidate rows —
/// `uq_pricing_plan_current` permits one current revision and
/// `OPEN_DRAFT_REVISION_EXISTS` permits one open draft — so a single window of
/// `2 × limit` rows would always hold `limit` distinct plans. That bound is a
/// property of *those two* states and of nothing else: a caller filtering on
/// `superseded` is asking about a state a plan holds once **per revision**, and
/// a single over-fetch would then return a short page, which a paginating caller
/// reads as the end of the result. So the window is drawn repeatedly, resuming
/// past the last plan it yielded, until the page is full or the store is
/// exhausted. Progress is guaranteed because each redraw starts strictly after a
/// `plan_id` the previous one returned.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a stored row cannot be read as the domain value its columns are
/// `CHECK`-constrained to hold.
pub async fn list_authoring_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    states: &[LifecycleState],
    after: Option<Uuid>,
    limit: u64,
) -> Result<Vec<PlanRevision>, RepoError> {
    let tokens: Vec<&str> = if states.is_empty() {
        let mut set = current_tokens();
        set.push(LifecycleState::Draft.as_str());
        set
    } else {
        states.iter().map(|state| state.as_str()).collect()
    };

    // Two rows per plan is the ordinary case; the window is never narrower than
    // that, so a `limit` of one still asks a question a plan's pair can answer.
    // `Ord::max`, spelled out: sea-orm 2's `ExprTrait` is blanket-implemented
    // for every `T`, so a bare `.max(..)` on an integer is ambiguous between the
    // numeric maximum and a SQL `MAX(..)` expression builder. The compiler
    // refuses rather than choosing, which is the good outcome; this names which
    // one was always meant.
    let window = Ord::max(limit.saturating_mul(2), 2);
    let mut out: Vec<PlanRevision> = Vec::new();
    let mut cursor = after;
    loop {
        let mut filter = Condition::all()
            .add(plan::Column::TenantId.eq(tenant_id))
            .add(plan::Column::LifecycleState.is_in(tokens.clone()));
        if let Some(cursor) = cursor {
            filter = filter.add(plan::Column::PlanId.gt(cursor));
        }

        let rows = plan::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(filter)
            .order_by(plan::Column::PlanId, Order::Asc)
            .order_by(plan::Column::Revision, Order::Desc)
            .limit(window)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("list pricing_plan: {e}")))?;
        let exhausted = u64::try_from(rows.len()).unwrap_or(u64::MAX) < window;

        let mut full = false;
        for row in rows {
            if cursor == Some(row.plan_id) {
                // A later revision of the plan this window already answered for.
                continue;
            }
            cursor = Some(row.plan_id);
            out.push(to_domain(row)?);
            if u64::try_from(out.len()).unwrap_or(u64::MAX) >= limit {
                full = true;
                break;
            }
        }
        if full || exhausted {
            return Ok(out);
        }
    }
}

/// Read one revision by its composite identity, scoped.
/// Read one revision by its composite identity, through whichever runner the
/// caller holds.
///
/// Public and runner-taking for [`load_current`]'s reason, and with one caller
/// of its own outside this module: the projector reads the **revision the
/// publish judged**, pinned on the ref row at commit time, rather than whatever
/// revision is current when the sweep arrives.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when the stored row cannot be read as the domain
/// value its columns are `CHECK`-constrained to hold.
pub async fn load_revision(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Option<PlanRevision>, RepoError> {
    load_revision_row(runner, scope, tenant_id, plan_id, revision)
        .await?
        .map(to_domain)
        .transpose()
}

/// The greatest revision **strictly below** `revision` whose content has ever
/// been live — the predecessor a publish at `revision` supersedes.
///
/// # Why this and not [`load_current`]
///
/// A caller asking *"what did a consumer have before this publish"* wants the
/// revision this one replaces, and the current revision is not always it. The two
/// come apart in the ordinary case, not a contrived one: a plan publishes revision
/// `0` and then a bundle publishes its composition at revision `0` as well, so the
/// current revision **is** the candidate and a diff against it is a diff against
/// itself — which would report every publish as changing nothing. Walking the
/// chain backwards from the candidate is the answer that holds whether or not the
/// candidate has published yet, and holds identically on a re-submitted publish:
/// `revision`'s own state moves `draft → published` and its predecessor's moves
/// `published → superseded`, and neither move changes which row this returns.
/// That stability is load-bearing — a bundle publish is a two-call act (submit,
/// then publish on the strength of an approval), and a classification that
/// differed between the two calls would put one token in front of the reviewer and
/// store another.
///
/// **`has_ever_been_live` and not "published"**: by the time a successor exists
/// the predecessor is `superseded`, which is exactly the row wanted. `draft` and
/// `abandoned` are excluded there, so a tombstone occupying a revision number
/// cannot become a baseline — the number it holds is D-145's whole mechanism and
/// its content was never anybody's but its author's.
///
/// `None` means no revision below this one ever published, which for a
/// composition is *"this is the bundle's creation"* rather than a missing read.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when `revision` or a stored row is outside the
/// storable range.
pub async fn load_live_predecessor(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Option<PlanRevision>, RepoError> {
    let Ok(number) = i64::try_from(revision) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} revision {revision} exceeds the storable range"
        )));
    };
    let live: Vec<&str> = LifecycleState::ALL
        .iter()
        .copied()
        .filter(|state| state.has_ever_been_live())
        .map(LifecycleState::as_str)
        .collect();
    // Ordered and limited rather than aggregated, for `load_current`'s reason:
    // the maximum has to be the maximum *this caller can see*, so it runs through
    // the same scoped path as every other read here.
    plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.lt(number))
                .add(plan::Column::LifecycleState.is_in(live)),
        )
        .order_by(plan::Column::Revision, Order::Desc)
        .limit(1)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the live predecessor revision: {e}")))?
        .into_iter()
        .next()
        .map(to_domain)
        .transpose()
}

/// Write a rendered revision row.
async fn insert_revision(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row: plan::ActiveModel,
) -> Result<(), RepoError> {
    plan::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(|e| RepoError::Db(format!("pricing_plan scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("insert pricing_plan: {e}")))?;
    Ok(())
}

/// The revision's row, when it is a `draft` standing at exactly `expected`;
/// `None` when it is anything else, including absent.
///
/// Asked by the paths that touch a **child** row before the compare-and-swap
/// has answered, so a caller aiming at a frozen revision is told so by this
/// repository rather than by the child table's trigger — whose refusal is a raw
/// driver error carrying no state and no subject. It is deliberately not
/// trusted as the guard: between this read and the swap a concurrent publish
/// may land, and what closes that window is the rollback.
///
/// It hands the stored row back rather than a `bool` because a child row's
/// `tenant_id` is **copied from the parent revision** and never taken from a
/// request (Global Constraint 9): the child's foreign key covers the plan key
/// alone, so nothing in the schema stops a child carrying a foreign tenant, and
/// under `SecureORM` such a child is invisible to its true owner while still
/// joined by key to their plan.
pub(super) async fn mutable_draft(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
) -> Result<Option<plan::Model>, RepoError> {
    let Some(row) = load_revision_row(runner, scope, tenant_id, plan_id, revision).await? else {
        return Ok(None);
    };
    let state = read_lifecycle(&row)?;
    let current = read_row_version(&row)?;
    Ok((state.is_content_mutable() && current == expected).then_some(row))
}

/// Name which conjunct of a failed compare-and-swap actually failed.
///
/// One extra read, taken only on the refusal path. It costs nothing in the
/// normal case and is the difference between an operator being told to retry
/// and being told to stop retrying.
pub(super) async fn refuse(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
) -> RepoError {
    match load_revision(runner, scope, tenant_id, plan_id, revision).await {
        Err(err) => err,
        Ok(None) => not_found(plan_id, revision),
        Ok(Some(row)) if !row.lifecycle_state.is_content_mutable() => RepoError::NotDraft {
            subject: SUBJECT.to_owned(),
            id: revision_ref(plan_id, revision),
            state: row.lifecycle_state.to_string(),
        },
        Ok(Some(row)) => RepoError::StaleRowVersion {
            subject: SUBJECT.to_owned(),
            id: revision_ref(plan_id, revision),
            current: row.row_version.get(),
            submitted: expected.get(),
        },
    }
}

/// Render a revision value into the columns that hold it.
///
/// Pure, and separate from the insert, so every caller can refuse a value no
/// column can hold **before** it opens a transaction.
///
/// # Errors
/// [`RepoError::CorruptRow`] for a revision number or row version past its
/// column's range; [`RepoError::ValueOutOfRange`] for an authored purchase
/// quantity or custom interval past its own.
fn revision_model(
    tenant_id: Uuid,
    revision: &PlanRevision,
) -> Result<plan::ActiveModel, RepoError> {
    let number = stored_revision(revision.revision).ok_or_else(|| {
        RepoError::CorruptRow(format!(
            "plan {} revision {} exceeds the storable range",
            revision.plan_id, revision.revision
        ))
    })?;
    let version = revision.row_version.to_stored().map_err(|e| {
        RepoError::CorruptRow(format!(
            "plan {} revision {}: {e}",
            revision.plan_id, revision.revision
        ))
    })?;
    let interval = StoredFrequency::render(revision.frequency)?;
    Ok(plan::ActiveModel {
        plan_id: Set(revision.plan_id.get()),
        revision: Set(number),
        tenant_id: Set(tenant_id),
        sku_id: Set(revision.sku_id),
        plan_tier: Set(revision.plan_tier.clone()),
        plan_name: Set(revision.plan_name.clone()),
        billing_cycle: Set(revision
            .billing_cycle
            .map(|cycle| cycle.as_str().to_owned())),
        frequency: Set(interval.token),
        custom_interval_n: Set(interval.n),
        custom_interval_unit: Set(interval.unit),
        plan_tier_override: Set(revision.plan_tier_override),
        purchase_min_qty: Set(stored_qty("purchaseMinQty", revision.purchase_min_qty)?),
        purchase_max_qty: Set(stored_qty("purchaseMaxQty", revision.purchase_max_qty)?),
        invoice_grouping_key: Set(revision.invoice_grouping_key.clone()),
        entitlement_grants: Set(entitlement_grants_json(&revision.entitlement_grants)),
        allowed_change_targets: Set(change_targets_json(
            revision.change_contract.allowed_change_targets.as_deref(),
        )),
        comparability_rank: Set(revision.change_contract.comparability_rank),
        usage_counter_on_plan_change: Set(Some(
            revision
                .change_contract
                .usage_counter_on_plan_change
                .as_str()
                .to_owned(),
        )),
        lifecycle_state: Set(revision.lifecycle_state.as_str().to_owned()),
        available_from: Set(revision.available_from),
        available_to: Set(revision.available_to),
        created_by: Set(revision.created_by),
        created_at_utc: Set(revision.created_at_utc),
        cloned_from: Set(revision.cloned_from.map(PlanId::get)),
        row_version: Set(version),
    })
}

/// The stored tokens the domain machine calls **current**.
///
/// Derived from [`LifecycleState::is_current_revision`] rather than written out,
/// so widening or narrowing that predicate moves this query with it. D-128
/// widened it once already, and the version of this list that did not move would
/// have been the one silently returning `None` for every retired plan.
///
/// `pub(crate)` because `infra::currency_binding` asks the same question of the
/// same column — which plan revisions still sell an add-on — and a second
/// derivation of "current" is a second rule free to disagree with this one.
pub(crate) fn current_tokens() -> Vec<&'static str> {
    LifecycleState::ALL
        .iter()
        .copied()
        .filter(|state| state.is_current_revision())
        .map(LifecycleState::as_str)
        .collect()
}

/// The conjunction that makes an UPDATE or DELETE a compare-and-swap on one
/// **draft** revision.
///
/// `None` when a number the caller supplied cannot be stored at all; every
/// caller then resolves it through [`refuse`], because no row can hold such a
/// value and the truthful answer is the one an absent row already gets.
///
/// Shared with `plan_shape_repo` rather than copied: a revision's child rows
/// are covered by the revision's **own** entity tag (D-141's argument for the
/// price row's bands, applied to a plan's shape), so the swap that admits a
/// phase edit has to be the same conjunction, on the same row, as the swap that
/// admits a column edit. Two spellings of it are two rules free to disagree.
pub(super) fn swap_guard(
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
) -> Option<Condition> {
    let number = stored_revision(revision)?;
    let version = expected.to_stored().ok()?;
    Some(
        Condition::all()
            .add(plan::Column::TenantId.eq(tenant_id))
            .add(plan::Column::PlanId.eq(plan_id.get()))
            .add(plan::Column::Revision.eq(number))
            .add(plan::Column::RowVersion.eq(version))
            .add(plan::Column::LifecycleState.eq(LifecycleState::Draft.as_str())),
    )
}

/// The columns a patch moves, paired with the values it moves them to.
///
/// Built before the compare-and-swap rather than inside it, so a value no
/// column can hold is refused with the row's tag exactly where it was — the
/// same posture `check_authored_instant` takes one line above the call.
///
/// # Errors
/// [`RepoError::ValueOutOfRange`] for a purchase quantity or custom interval
/// past its column's range.
///
/// # A field added to the patch is a compile error here
/// The patch is **destructured field by field, with no `..` rest pattern**, and
/// every binding is used below. This is the same guard `content_assignments`
/// took for the price row's columns, applied to the plan's, and for the reason
/// that one was taken: a run of `if let Some(x) = patch.x` arms is a hand-written
/// enumeration wearing the shape of a complete one. Under it a field added to
/// [`PlanShapePatch`] compiles, is accepted by the surface, never reaches the
/// `UPDATE`, and answers success — which is how `unit_rate_nano` fell out of the
/// price row's assignment list while `PATCH` rendered the reverted value from the
/// stored record as if the author had asked for it. Here a new field is
/// `error[E0027]` on this pattern until someone decides which column it moves,
/// or binds it and states on the line why it moves none.
fn patched_columns(patch: PlanShapePatch) -> Result<Vec<(plan::Column, SimpleExpr)>, RepoError> {
    let PlanShapePatch {
        sku_id,
        plan_tier,
        plan_name,
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
    } = patch;

    let mut columns: Vec<(plan::Column, SimpleExpr)> = Vec::new();
    if let Some(sku_id) = sku_id {
        columns.push((plan::Column::SkuId, Expr::value(sku_id)));
    }
    if let Some(plan_tier) = plan_tier {
        columns.push((plan::Column::PlanTier, Expr::value(plan_tier)));
    }
    if let Some(plan_name) = plan_name {
        columns.push((plan::Column::PlanName, Expr::value(plan_name)));
    }
    if let Some(billing_cycle) = billing_cycle {
        columns.push((
            plan::Column::BillingCycle,
            Expr::value(billing_cycle.as_str()),
        ));
    }
    // One value, three columns. Moving the token on its own would leave a fixed
    // frequency wearing the interval of the custom one it replaced - the
    // pairing `chk_pricing_plan_custom_interval_pairing` refuses and
    // `Frequency` cannot represent - so the interval columns travel with it,
    // NULL and all.
    if let Some(frequency) = frequency {
        let interval = StoredFrequency::render(Some(frequency))?;
        columns.push((plan::Column::Frequency, Expr::value(interval.token)));
        columns.push((plan::Column::CustomIntervalN, Expr::value(interval.n)));
        columns.push((plan::Column::CustomIntervalUnit, Expr::value(interval.unit)));
    }
    if let Some(plan_tier_override) = plan_tier_override {
        columns.push((
            plan::Column::PlanTierOverride,
            Expr::value(plan_tier_override),
        ));
    }
    if let Some(min_qty) = stored_qty("purchaseMinQty", purchase_min_qty)? {
        columns.push((plan::Column::PurchaseMinQty, Expr::value(min_qty)));
    }
    if let Some(max_qty) = stored_qty("purchaseMaxQty", purchase_max_qty)? {
        columns.push((plan::Column::PurchaseMaxQty, Expr::value(max_qty)));
    }
    if let Some(grouping_key) = invoice_grouping_key {
        columns.push((plan::Column::InvoiceGroupingKey, Expr::value(grouping_key)));
    }
    if let Some(available_from) = available_from {
        columns.push((plan::Column::AvailableFrom, Expr::value(available_from)));
    }
    if let Some(available_to) = available_to {
        columns.push((plan::Column::AvailableTo, Expr::value(available_to)));
    }
    if let Some(grants) = entitlement_grants {
        columns.push((
            plan::Column::EntitlementGrants,
            Expr::value(entitlement_grants_json(&grants)),
        ));
    }
    // One value, three columns, and they travel together for `Frequency`'s
    // reason: K4 ties the rank to whether the edge list names anyone, so moving
    // one on its own could leave a plan with edges and no rank -- a state no
    // publish accepts and no caller meant. `PlanShapePatch::change_contract`
    // replaces the contract wholesale precisely so this stays impossible.
    if let Some(contract) = change_contract {
        columns.push((
            plan::Column::AllowedChangeTargets,
            Expr::value(change_targets_json(
                contract.allowed_change_targets.as_deref(),
            )),
        ));
        columns.push((
            plan::Column::ComparabilityRank,
            Expr::value(contract.comparability_rank),
        ));
        columns.push((
            plan::Column::UsageCounterOnPlanChange,
            Expr::value(contract.usage_counter_on_plan_change.as_str()),
        ));
    }
    Ok(columns)
}

/// The edge list as the column holds it: a JSON array of `planId` strings, or
/// `NULL` for the fail-safe.
///
/// `NULL` and `[]` are **different** and both are kept: `inst-pc-failsafe` makes
/// absence mean "no self-service change", while an empty array is an author who
/// stated the edge set and left it empty. A renderer that collapsed one into the
/// other would erase the distinction `inst-cr-nodefault` promises a consumer it
/// can rely on.
fn change_targets_json(targets: Option<&[Uuid]>) -> Option<JsonValue> {
    targets.map(|ids| {
        JsonValue::Array(
            ids.iter()
                .map(|id| JsonValue::String(id.to_string()))
                .collect(),
        )
    })
}

/// Read the edge list back, refusing anything the renderer above cannot have
/// written.
///
/// A stored value that is not an array of `uuid` strings is **corruption**, not
/// an absent contract: the only writer is `change_targets_json`, and reading a
/// malformed one as `None` would silently turn a plan with edges into one
/// advertising no self-service change at all.
fn read_change_targets(
    plan_id: Uuid,
    stored: Option<&JsonValue>,
) -> Result<Option<Vec<Uuid>>, RepoError> {
    let Some(value) = stored else {
        return Ok(None);
    };
    let JsonValue::Array(items) = value else {
        return Err(RepoError::CorruptRow(format!(
            "pricing_plan {plan_id}: allowed_change_targets holds {value}, which is not an array"
        )));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| {
                    RepoError::CorruptRow(format!(
                        "pricing_plan {plan_id}: allowed_change_targets holds {item}, which is not \
                         a planId"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// The grant set as the column holds it: `null` when nothing was authored.
///
/// `null` rather than an empty object, so "no grant set" and "an empty grant
/// set" stay one fact rather than two spellings of one -- `EntitlementGrants`
/// makes them the same value (`is_absent`), and writing `{}` would invite a
/// later reader to believe the distinction exists.
fn entitlement_grants_json(grants: &EntitlementGrants) -> Option<JsonValue> {
    if grants.is_absent() {
        return None;
    }
    Some(serde_json::json!({
        "planTierRef": grants.plan_tier_ref,
        "featureFlags": grants.plan_level.feature_flags,
        "quotas": grants.plan_level.quotas,
        "perPhase": grants
            .per_phase
            .iter()
            .map(|(id, set)| {
                (
                    id.to_string(),
                    serde_json::json!({
                        "featureFlags": set.feature_flags,
                        "quotas": set.quotas,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>(),
    }))
}

/// Read one grant set out of its object.
fn read_grant_set(plan_id: Uuid, value: &JsonValue) -> Result<GrantSet, RepoError> {
    let corrupt = |what: &str| {
        RepoError::CorruptRow(format!("pricing_plan {plan_id}: entitlement_grants {what}"))
    };
    let mut set = GrantSet::default();
    if let Some(flags) = value.get("featureFlags") {
        let obj = flags
            .as_object()
            .ok_or_else(|| corrupt("featureFlags is not an object"))?;
        for (key, on) in obj {
            set.feature_flags.insert(
                key.clone(),
                on.as_bool()
                    .ok_or_else(|| corrupt("a featureFlag is not a bool"))?,
            );
        }
    }
    if let Some(quotas) = value.get("quotas") {
        let obj = quotas
            .as_object()
            .ok_or_else(|| corrupt("quotas is not an object"))?;
        for (key, amount) in obj {
            set.quotas.insert(
                key.clone(),
                amount
                    .as_i64()
                    .ok_or_else(|| corrupt("a quota is not an integer"))?,
            );
        }
    }
    Ok(set)
}

/// Rebuild the grant set from its column.
///
/// A malformed value is **corruption**, not an absent set: reading it as absent
/// would publish a plan advertising no entitlements at all, which Subscriptions
/// would provision against.
fn to_entitlement_grants(row: &plan::Model) -> Result<EntitlementGrants, RepoError> {
    let Some(value) = row.entitlement_grants.as_ref() else {
        return Ok(EntitlementGrants::default());
    };
    let plan_id = row.plan_id;
    let mut grants = EntitlementGrants {
        // Present-but-not-a-string is **corruption**, not absence, exactly as this
        // function's own doc says of every sibling member of this document. Read
        // through `and_then(as_str)` it comes back as `None`, so a malformed tier
        // reference publishes a plan advertising no tier — silently, and on the one
        // member of the set whose siblings all refuse.
        plan_tier_ref: match value.get("planTierRef") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(other) => {
                return Err(RepoError::CorruptRow(format!(
                    "pricing_plan {plan_id} holds a non-string planTierRef ({other})"
                )));
            }
        },
        plan_level: read_grant_set(plan_id, value)?,
        per_phase: BTreeMap::new(),
    };
    if let Some(per_phase) = value.get("perPhase") {
        let obj = per_phase.as_object().ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_plan {plan_id}: entitlement_grants perPhase is not an object"
            ))
        })?;
        for (key, set) in obj {
            let phase_id = Uuid::parse_str(key).map_err(|_| {
                RepoError::CorruptRow(format!(
                    "pricing_plan {plan_id}: entitlement_grants perPhase is keyed {key}, which is \
                     not a phaseId"
                ))
            })?;
            grants
                .per_phase
                .insert(phase_id, read_grant_set(plan_id, set)?);
        }
    }
    Ok(grants)
}

/// Rebuild the plan-change contract from its three columns.
fn to_change_contract(row: &plan::Model) -> Result<PlanChangeContract, RepoError> {
    Ok(PlanChangeContract {
        allowed_change_targets: read_change_targets(
            row.plan_id,
            row.allowed_change_targets.as_ref(),
        )?,
        comparability_rank: row.comparability_rank,
        // NULL reads as the ratified default rather than as a third state:
        // D-113 says absence **is** `reset`, so an old row and one that
        // authored `reset` are the same plan.
        usage_counter_on_plan_change: match row.usage_counter_on_plan_change.as_deref() {
            None => UsageCounterOnPlanChange::Reset,
            Some(token) => read_token(
                "pricing_plan.usage_counter_on_plan_change",
                token,
                UsageCounterOnPlanChange::ALL,
                UsageCounterOnPlanChange::as_str,
            )?,
        },
    })
}

/// The three columns a [`Frequency`] occupies, rendered together.
///
/// One type rather than three expressions, because the three columns are one
/// fact: `frequency` alone cannot say what interval was authored, and the
/// interval pair alone cannot say the frequency is the custom one. Rendering
/// them at separate call sites is precisely how a `monthly` row acquires an
/// interval — the state `Frequency` cannot represent and
/// `chk_pricing_plan_custom_interval_pairing` refuses.
struct StoredFrequency {
    /// The `frequency` column: the bare discriminator, `None` when unauthored.
    token: Option<String>,
    /// `custom_interval_n`, set on the custom frequency and on nothing else.
    n: Option<i32>,
    /// `custom_interval_unit`, set on the custom frequency and on nothing else.
    unit: Option<String>,
}

impl StoredFrequency {
    /// Decompose an authored frequency into its three columns.
    ///
    /// The `match` is exhaustive with no wildcard on purpose: a frequency
    /// variant added later stops this compiling instead of quietly rendering as
    /// a fixed one with its payload dropped.
    fn render(frequency: Option<Frequency>) -> Result<Self, RepoError> {
        let Some(frequency) = frequency else {
            return Ok(Self {
                token: None,
                n: None,
                unit: None,
            });
        };
        let token = Some(frequency.as_str().to_owned());
        match frequency {
            Frequency::CustomEveryN { n, unit } => Ok(Self {
                token,
                n: Some(
                    i32::try_from(n).map_err(|_| out_of_range("customIntervalN", u64::from(n)))?,
                ),
                unit: Some(unit.as_str().to_owned()),
            }),
            Frequency::Monthly
            | Frequency::Quarterly
            | Frequency::Semiannual
            | Frequency::Annual => Ok(Self {
                token,
                n: None,
                unit: None,
            }),
        }
    }
}

/// Render an authored quantity for its `bigint` column.
///
/// Checked rather than cast: a cast would turn a quantity no plan could sell
/// into a plausible one, and the bound would then be enforced against a number
/// nobody authored.
fn stored_qty(field: &str, value: Option<u64>) -> Result<Option<i64>, RepoError> {
    let Some(value) = value else {
        return Ok(None);
    };
    i64::try_from(value)
        .map(Some)
        .map_err(|_| out_of_range(field, value))
}

/// The caller-mistake refusal for a value no column can hold.
fn out_of_range(field: &str, value: u64) -> RepoError {
    RepoError::ValueOutOfRange {
        field: field.to_owned(),
        value: value.to_string(),
    }
}

/// Render a revision number for its `bigint` column, `None` past the range the
/// column can hold. Checked rather than cast: a cast would turn an impossible
/// revision into a plausible one and quietly address a different row.
fn stored_revision(revision: u64) -> Option<i64> {
    i64::try_from(revision).ok()
}

/// The composite identity, as one reference a caller can read back in a URL.
fn revision_ref(plan_id: PlanId, revision: u64) -> String {
    format!("{plan_id}/{revision}")
}

/// The "absent, or not yours" refusal — deliberately one answer for both, so
/// the surface leaks no existence.
pub(super) fn not_found(plan_id: PlanId, revision: u64) -> RepoError {
    RepoError::NotFound {
        subject: SUBJECT.to_owned(),
        id: revision_ref(plan_id, revision),
    }
}

/// Map a stored row to the domain value, at this boundary and nowhere else.
///
/// Every reading that can fail is an **invariant breach, not a caller
/// mistake**: `lifecycle_state`, `billing_cycle`, `frequency` and
/// `custom_interval_unit` are each `CHECK`-constrained to the token set their
/// domain type renders, and `revision` / `row_version` / the purchase bounds
/// are columns that only ever hold counts. A row that reads otherwise means
/// something reached the table outside this gear, which is why it surfaces as
/// [`RepoError::CorruptRow`] rather than as a not-found.
fn to_domain(row: plan::Model) -> Result<PlanRevision, RepoError> {
    let revision = u64::try_from(row.revision).map_err(|e| {
        RepoError::CorruptRow(format!(
            "pricing_plan row for plan {} holds revision {}: {e}",
            row.plan_id, row.revision
        ))
    })?;
    let lifecycle_state = read_lifecycle(&row)?;
    let row_version = read_row_version(&row)?;
    let billing_cycle = read_optional_token(
        "pricing_plan.billing_cycle",
        row.billing_cycle.as_deref(),
        BillingCycle::ALL,
        BillingCycle::as_str,
    )?;
    let frequency = read_frequency(&row)?;
    let purchase_min_qty = read_qty("pricing_plan.purchase_min_qty", row.purchase_min_qty)?;
    let purchase_max_qty = read_qty("pricing_plan.purchase_max_qty", row.purchase_max_qty)?;
    // Read before the literal below consumes `row`, which is why this is a
    // `let` rather than an inline call.
    let change_contract = to_change_contract(&row)?;
    let entitlement_grants = to_entitlement_grants(&row)?;
    Ok(PlanRevision {
        plan_id: PlanId::new(row.plan_id),
        revision,
        sku_id: row.sku_id,
        plan_tier: row.plan_tier,
        plan_name: row.plan_name,
        billing_cycle,
        frequency,
        plan_tier_override: row.plan_tier_override,
        purchase_min_qty,
        purchase_max_qty,
        invoice_grouping_key: row.invoice_grouping_key,
        available_from: row.available_from,
        available_to: row.available_to,
        entitlement_grants,
        change_contract,
        lifecycle_state,
        created_by: row.created_by,
        created_at_utc: row.created_at_utc,
        cloned_from: row.cloned_from.map(PlanId::new),
        row_version,
    })
}

/// The revision's state, read back into the machine that owns its edges.
///
/// Separate from [`to_domain`] because [`mutable_draft`] needs this one reading
/// and not the whole value: it hands its caller the **stored row**, so that a
/// child row's `tenant_id` can be copied from the parent rather than trusted
/// from a request.
fn read_lifecycle(row: &plan::Model) -> Result<LifecycleState, RepoError> {
    LifecycleState::ALL
        .iter()
        .copied()
        .find(|state| state.as_str() == row.lifecycle_state)
        .ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_plan revision {} of plan {} holds lifecycle_state {}",
                row.revision, row.plan_id, row.lifecycle_state
            ))
        })
}

/// The revision's entity tag, for the same two readers.
fn read_row_version(row: &plan::Model) -> Result<RowVersion, RepoError> {
    RowVersion::from_stored(row.row_version).map_err(|e| {
        RepoError::CorruptRow(format!(
            "pricing_plan revision {} of plan {}: {e}",
            row.revision, row.plan_id
        ))
    })
}

/// Reassemble the three interval columns into the one value the domain holds.
///
/// **The `custom_every_n` member of [`Frequency::ALL`] is a variant
/// representative, not data.** Its `n` is
/// [`Frequency::CUSTOM_INTERVAL_PLACEHOLDER`] because a token cannot say what
/// interval somebody authored, so the match below uses that member for its
/// *shape* and rebuilds `n` and `unit` from the columns that actually hold
/// them. Keeping what the list carried would read every custom plan back as
/// "every 1 day" — silently, on the happy path, with no error anywhere.
///
/// The two pairings `chk_pricing_plan_custom_interval_pairing` refuses are
/// refused again here. The half-set pair the CHECK cannot see — one interval
/// column set under a fixed or absent frequency, where its right-hand side is
/// already false — is refused **only** here, which is why the reading is
/// written as a whitelist of the two legal shapes rather than as a lookup that
/// ignores whatever else the row carries.
/// `pub(crate)` because [`infra::bundle`](crate::infra::bundle) asks the same
/// question of a *component's* plan row: `inst-bc-frequency` compares components
/// to each other, and a second reader of this column would be a second answer to
/// what `custom_every_n` beside a null interval means.
pub(crate) fn read_frequency(row: &plan::Model) -> Result<Option<Frequency>, RepoError> {
    let Some(token) = row.frequency.as_deref() else {
        if row.custom_interval_n.is_some() || row.custom_interval_unit.is_some() {
            return Err(orphan_interval(row));
        }
        return Ok(None);
    };
    match read_token(
        "pricing_plan.frequency",
        token,
        Frequency::ALL,
        Frequency::as_str,
    )? {
        Frequency::CustomEveryN { .. } => {
            let (Some(n), Some(unit)) =
                (row.custom_interval_n, row.custom_interval_unit.as_deref())
            else {
                return Err(RepoError::CorruptRow(format!(
                    "pricing_plan revision {} of plan {} holds frequency custom_every_n \
                     with no interval",
                    row.revision, row.plan_id
                )));
            };
            Ok(Some(Frequency::CustomEveryN {
                n: u32::try_from(n).map_err(|_| {
                    RepoError::CorruptRow(format!(
                        "pricing_plan.custom_interval_n holds {n}, not an interval"
                    ))
                })?,
                unit: read_token(
                    "pricing_plan.custom_interval_unit",
                    unit,
                    CustomIntervalUnit::ALL,
                    CustomIntervalUnit::as_str,
                )?,
            }))
        }
        fixed @ (Frequency::Monthly
        | Frequency::Quarterly
        | Frequency::Semiannual
        | Frequency::Annual) => {
            if row.custom_interval_n.is_some() || row.custom_interval_unit.is_some() {
                return Err(orphan_interval(row));
            }
            Ok(Some(fixed))
        }
    }
}

/// An interval column set under a frequency that has no interval.
fn orphan_interval(row: &plan::Model) -> RepoError {
    RepoError::CorruptRow(format!(
        "pricing_plan revision {} of plan {} carries a custom interval under frequency {}",
        row.revision,
        row.plan_id,
        row.frequency.as_deref().unwrap_or("(none)")
    ))
}

/// Read a purchase bound back into the unsigned type the domain counts in.
fn read_qty(column: &str, stored: Option<i64>) -> Result<Option<u64>, RepoError> {
    let Some(value) = stored else {
        return Ok(None);
    };
    u64::try_from(value)
        .map(Some)
        .map_err(|_| RepoError::CorruptRow(format!("{column} holds {value}, not a quantity")))
}

/// Read a nullable token column.
fn read_optional_token<T: Copy>(
    column: &str,
    token: Option<&str>,
    candidates: &[T],
    render: fn(T) -> &'static str,
) -> Result<Option<T>, RepoError> {
    let Some(token) = token else {
        return Ok(None);
    };
    read_token(column, token, candidates, render).map(Some)
}

/// Read a stored token back into the domain value that renders it.
///
/// The candidate list is always the enum's own `ALL`, never a copy of it. A
/// hand-written inverse list is one a variant added later goes missing from
/// while everything still compiles, and what that produces is a perfectly legal
/// row read back as a corrupt one.
///
/// Shared with `plan_shape_repo`, which reads the same slice's tokens off this
/// revision's **child** tables: one lookup over one authority, rather than a
/// second copy a later variant can go missing from.
pub(super) fn read_token<T: Copy>(
    column: &str,
    token: &str,
    candidates: &[T],
    render: fn(T) -> &'static str,
) -> Result<T, RepoError> {
    candidates
        .iter()
        .copied()
        .find(|candidate| render(*candidate) == token)
        .ok_or_else(|| RepoError::CorruptRow(format!("{column} holds {token}")))
}

/// Flip the plan's **current published revision** to `retired`
/// (`inst-rt-cancel`, `inst-rt-event`, D-90, D-128).
///
/// The one flip in this module that mints no new row and consumes no revision
/// number: retirement is a state transition on the revision the plan already
/// stands at. D-90 makes that revision unique by construction — the partial
/// `uq_pricing_plan_current` index spans `('published','retired')` — so there is
/// exactly one row to move and no choice of target to get wrong.
///
/// **The predicate is the guard.** `lifecycle_state = 'published'` rides inside
/// the compare-and-swap rather than being checked ahead of it, for
/// [`publish_revision`]'s reason: a check made before the statement is a check
/// a concurrent writer can move behind. The caller asks
/// [`crate::domain::lifecycle::LifecycleState::can_transition`] first anyway —
/// not as the guard, but so an operator retiring an already-retired plan is told
/// which state it is in rather than reading a contention refusal for a race that
/// never happened.
///
/// **The entity tag stays where it is, and that is the store's rule rather than
/// a choice made here.** `trg_pricing_plan_frozen_columns` (and Postgres'
/// `pricing_plan_append_only`) freeze every content column the moment a row
/// leaves `draft`, and `row_version` is on that whitelist — so the **only**
/// update a published row admits is the bare `lifecycle_state` flip. Bumping the
/// tag alongside the flip meets `(code: 1811) revision is frozen` — a driver
/// refusal rather than an assertion, so every case in
/// `tests/sqlite_retirement.rs` reddens at once. [`supersede_current`] one screen
/// up says the same — *"leaving its entity tag where it is"* — for the same
/// reason on the same table.
///
/// No `expected: RowVersion` either. §5 keys this act **per revision** and
/// offers no `If-Match`; the state predicate is the stronger precondition
/// anyway, because it is the transition itself that may happen only once, and it
/// stays true under a concurrent price-row edit that would move a tag without
/// touching what retirement is about.
///
/// # Errors
/// [`RepoError::ConcurrentMutation`] when no published row answered the swap —
/// another retirement or a supersession took the current-revision slot first;
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when the revision number has no column
/// representation, or the flipped row cannot be read back.
#[tracing::instrument(skip_all, fields(tenant_id = %tenant_id, plan_id = %plan_id, revision))]
pub async fn retire_revision(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<PlanRevision, RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} stands at a revision {revision} no column can address"
        )));
    };
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Retired.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::PlanId.eq(plan_id.get()))
                .add(plan::Column::Revision.eq(number))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .exec(txn)
        .await
        .map_err(|e| contention_or_db(&e, &format!("plan {plan_id}"), "retire plan revision"))?;
    if result.rows_affected == 0 {
        // `supersede_current`'s sibling arm carries why this is logged while the
        // refusals around it are not.
        tracing::warn!(
            tenant_id = %tenant_id,
            plan_id = %plan_id,
            revision,
            "bss-pricing: the revision left `published` before the retirement could take it"
        );
        return Err(RepoError::ConcurrentMutation {
            aggregate: format!("plan {plan_id}"),
        });
    }
    load_revision(txn, scope, tenant_id, plan_id, revision)
        .await?
        .ok_or_else(|| not_found(plan_id, revision))
}

/// [`PlanRepo::update_draft`]'s body, on a runner the caller owns.
///
/// **The runner must be a transaction**, for [`create_draft_on`]'s reason: this
/// writes the revision's columns *and* its D-135 audit record, so on a bare
/// connection an append failure would leave a committed edit nobody recorded
/// making. Both current callers supply one.
///
/// The value refusals come first here, as they did in the method — but they are
/// no longer *ahead of the transaction*, and that is a real change D-274 did not
/// name. The method is now a delegation, so a patch refused on its value alone
/// costs a `BEGIN` and a `ROLLBACK` where it used to cost neither. Error identity
/// is preserved (`tx_failure` unwraps `TxError::Domain` untouched), and the
/// alternative — keeping the checks in the method and duplicating them for the
/// transaction-owning caller — is the two-implementations shape this whole
/// extraction exists to remove. `PriceRepo::create_draft` makes the opposite
/// trade with `prepare_draft` and says so; the difference is that its check is
/// a whole rendering pass, and these three are comparisons.
///
/// # Errors
/// Whatever [`PlanRepo::update_draft`] documents — this is that method, minus
/// the transaction it opens for itself.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, revision, expected-version) the compare-and-swap addresses, the patch and the D-135 audit stamp. The method above carries the same set; this form exists so a caller that already owns a transaction can join it rather than open a second (D-272)."
)]
#[tracing::instrument(skip_all, fields(tenant_id = %tenant_id, plan_id = %plan_id, revision))]
pub async fn update_draft_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
    patch: PlanShapePatch,
    stamp: AuditStamp,
) -> Result<PlanRevision, RepoError> {
    // Ahead of the compare-and-swap: a value the store cannot hold is refused
    // whether or not the caller also holds a current row version, and refusing
    // it here leaves the row's tag where it was. Not ahead of the *transaction*:
    // the method is a delegation — see this function's doc.
    check_authored_instant("availableFrom", patch.available_from)?;
    check_authored_instant("availableTo", patch.available_to)?;
    let columns = patched_columns(patch)?;
    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        // The method above opens a fresh connection here because it has not
        // entered its transaction yet; this form is already on the caller's
        // runner, so the refusal is read there — which is also the more honest
        // read, being the state the mutation would have seen.
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };

    // **The caller's transaction, never a bare statement.**
    // D-135 puts the audit record inside the mutation's own transaction, so a
    // rolled-back edit leaves no record of having happened - which is the
    // property a post-hoc writer passes by accident and fails under contention.
    // This function opens none of its own; it is the doc's requirement that the
    // runner be one, and every caller that holds this to it.
    let mut update = plan::Entity::update_many().secure().scope_with(scope);
    for (column, value) in columns {
        update = update.col_expr(column, value);
    }
    let result = update
        .col_expr(
            plan::Column::RowVersion,
            Expr::col(plan::Column::RowVersion).add(1_i64),
        )
        .filter(guard)
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("update plan draft: {e}")))?;
    if result.rows_affected == 0 {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    }
    let updated = load_revision(runner, scope, tenant_id, plan_id, revision)
        .await?
        .ok_or_else(|| not_found(plan_id, revision))?;
    record_revision_mutation(
        runner,
        scope,
        tenant_id,
        &updated,
        AuditAction::Update,
        expected,
        stamp,
    )
    .await?;
    Ok(updated)
}
