//! Repository for a plan revision's **shape** — the Slice-2 child tables that
//! version with the revision (`design/02-plan-definition.md` §6, D-83).
//!
//! `pricing_plan_phase`, `pricing_plan_addon_rule`,
//! `pricing_plan_descriptor_set`, `pricing_composite_meter` and
//! `pricing_plan_period_floor_cap` are all five of them. It is separate from
//! [`PlanRepo`](super::PlanRepo) because the two answer different questions:
//! that repository owns the revision **chain** — which revision is current,
//! which is open, which numbers are consumed — while this one owns what a single
//! revision *is*. What they share is the revision row's compare-and-swap, and
//! they share it literally: `swap_guard`, `refuse` and `mutable_draft` are
//! [`super::plan_repo`]'s, imported rather than re-spelled.
//!
//! **The child set carries no entity tag of its own, and that is a decision.**
//! A phase edit advances the *plan revision's* `row_version`, which is
//! `price_record.rs`'s argument for tier bands applied to a plan's shape: if
//! editing a phase left the revision's tag alone, two authors editing different
//! phases of one draft would both satisfy `If-Match` and silently interleave —
//! the lost update `cpt-cf-bss-pricing-fr-concurrent-edit` exists to refuse.
//! Every mutating call here therefore takes the revision's `RowVersion` and
//! bumps it in the same statement that matches on it.
//!
//! **The phase set is replaced wholesale, never merged.** That is not a
//! convenience: `uq_pricing_plan_phase_terminal` admits one terminal phase per
//! revision, and re-ordering a chain one row at a time transiently holds two —
//! so a row-by-row edit would be refused by the index for an end state that is
//! perfectly legal. Wholesale replacement is also `PriceRepo`'s band precedent
//! exactly, and for the second reason that one gives: a phase set is judged as a
//! **graph** (`inst-ph-graph`), and a partial update has no graph the rules
//! could evaluate.
//!
//! **A child row's `tenant_id` comes from the parent revision, never from the
//! request** (Global Constraint 9). The foreign key covers
//! `(plan_id, plan_revision)` alone, so nothing in the schema stops a phase row
//! carrying a different tenant from the revision it points at — and under
//! `SecureORM` such a row is invisible to its true owner while still joined by
//! key to their plan. The value is read off the revision row this repository
//! resolved, and `scope_with_model` checks it against the caller's scope on the
//! way in.
//!
//! **The delete precedes the insert inside one transaction**, and the read
//! precedes the delete. `pricing_plan_phase` refuses INSERT, UPDATE *and*
//! DELETE against a non-draft parent revision, and its refusal is a raw driver
//! error carrying no state: the read is there so a caller aiming at a frozen
//! revision is told which conjunct failed before any statement reaches the child
//! table. It is not the guard — the compare-and-swap is — and losing the row to
//! a concurrent publish in the window between them rolls the whole transaction
//! back, deleted phases and all.
//!
//! **A conflict edge is stored on both of its ends, and this is where that is
//! made true** (D-16, §6: "conflicts stored normalized symmetric"). The
//! submitted set is closed under symmetry on the way in: if rule A conflicts
//! with B and B is a member of the same submitted set, B's stored conflict set
//! names A. Leaving it to a caller was rejected — an asymmetric pair is a
//! conflict that exists when read from one side and not from the other, so
//! `AddonConflictBothRequired` would reach two verdicts on one plan depending on
//! which row it started from, and which verdict an author saw would depend on
//! the read order. An edge naming a **non-member** is left exactly as authored:
//! there is no row for the back-edge to land on, and that edge is precisely what
//! `ADDON_INCOMPATIBLE` exists to report — and what bounds the closure to the
//! set is the **row set itself**, since a back-edge computed for an id no
//! submitted rule carries has no row to be written on. `depends_on` is
//! **directed** and is never symmetrized — the reverse edge would make every
//! dependency its own two-cycle and `ADDON_CYCLE` would fail every plan that has
//! one.
//!
//! **Ten functions here are not part of the repository's surface** — a
//! `copy_` and a `delete_` per table — and they exist because D-83 and D-145 are
//! `PlanRepo`'s obligations discharged against these tables: [`copy_phases`] /
//! [`copy_addon_rules`] / [`copy_descriptor_set`] / [`copy_composites`] /
//! [`copy_period_floor_caps`] write the current revision's rows again under a
//! newly opened one with their ids unchanged, and [`delete_phases`] /
//! [`delete_addon_rules`] / [`delete_descriptor_set`] / [`delete_composites`] /
//! [`delete_period_floor_caps`] drop a discarded revision's copies before its row
//! is flipped `abandoned`. **This paragraph is the statement of that obligation**,
//! so a table added without extending it leaves the next author reading a roster
//! that is short by one — which is how the count above stood at three tables and
//! six functions while five tables and eight functions existed. They live beside their table's other statements rather
//! than in `plan_repo.rs` so that every write to a child table is in one file,
//! and they take a runner rather than a connection because each is half of a
//! transaction its caller owns.

use std::collections::{BTreeMap, BTreeSet};

use sea_orm::ActiveValue::{self, Set};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, JsonValue, Order};
use serde_json::json;
use toolkit_db::secure::{
    AccessScope, DBRunner, DbTx, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
    SecureUpdateExt, TxError,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp};
use crate::domain::concurrency::RowVersion;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan::PlanRevision;
use crate::domain::plan_shape::{
    AddonRule, CompositeMeter, DescriptorSet, PeriodFloorCap, PhaseKind, PlanPhase,
};
use crate::domain::scope_key::{PhaseId, PlanId, Region};
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{
    composite_meter, plan, plan_addon_rule, plan_descriptor_set, plan_period_floor_cap, plan_phase,
};
use crate::infra::storage::repo::plan_repo::{
    load_revision, mutable_draft, not_found, read_token, record_revision_mutation, refuse,
    swap_guard,
};

/// `SeaORM`-backed repository over a plan revision's phase chain.
#[derive(Clone)]
pub struct PlanShapeRepo {
    db: DBProvider<DbError>,
}

impl PlanShapeRepo {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Replace an open draft revision's whole phase set, under the caller's row
    /// version, in one transaction.
    ///
    /// The order inside it is: resolve the revision, drop its phases, insert the
    /// submitted set, then compare-and-swap the revision's `row_version + 1`
    /// under `expected`. The module doc says why each step sits where it does.
    ///
    /// The value handed back is the revision as it now stands, so the caller has
    /// the entity tag that covers the shape it just wrote. As with
    /// [`PlanRepo::update_draft`](super::PlanRepo::update_draft), it is a fresh
    /// read rather than a computed one.
    ///
    /// An **empty phase set is a valid request**: it is how an author clears a
    /// chain they are re-writing. Publishing such a revision is refused by
    /// `inst-ph-graph`, which owns the "at least one terminal phase" half of the
    /// rule that no index can carry.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such revision is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::ValueOutOfRange`] when an authored phase
    /// duration is past what its column can hold; [`RepoError::Db`] on a scope
    /// or storage failure, which **includes a second terminal phase in the
    /// submitted set** — `uq_pricing_plan_phase_terminal` is what decides that,
    /// and the pipeline is what reports it as `PHASE_GRAPH_INVALID` before a set
    /// ever gets here; [`RepoError::CorruptRow`] when the revision reads back
    /// unusable.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, \
                  revision, expected-version) the compare-and-swap addresses, the payload, and \
                  the D-135 audit stamp. `swap_guard` already takes those four together, so a \
                  `RevisionTarget` bundling them is the right shape and is owed - it is a \
                  signature change across every storage suite that pins these paths, and it \
                  changes no behaviour"
    )]
    pub async fn replace_phases(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        phases: Vec<PlanPhase>,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    replace_phases_on(
                        txn, &scope, tenant_id, plan_id, revision, expected, phases, stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Read one revision's phase chain, ordered by `ordinal` then `phase_id`.
    ///
    /// The order is here rather than left to the caller, and the tie-break is
    /// what makes it **total**: a duplicate `ordinal` is an authoring fault
    /// `PHASE_CHAIN_NONLINEAR` reports, and a set that came back in a different
    /// order on each read would make that report unreproducible — the same
    /// argument `domain::plan_shape::PhaseGraph::in_ordinal_order` gives for
    /// sorting stably.
    ///
    /// SQL-level BOLA: a foreign tenant's revision yields an empty chain.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
    /// value its columns are `CHECK`-constrained to hold.
    pub async fn list_phases(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<Vec<PlanPhase>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        load_phase_set(&conn, scope, tenant_id, plan_id, revision).await
    }

    /// Replace an open draft revision's whole period floor/cap set, under the
    /// caller's row version, in one transaction (S2 §6, **D-319**).
    ///
    /// Structurally [`PlanShapeRepo::replace_phases`] against the sibling table.
    /// An **empty set is a valid request**: it is how an author removes every
    /// bound, and a plan with no bound is the ordinary plan.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such revision is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::Db`] on a scope or storage failure,
    /// which **includes the table's four `CHECK`s** — a non-positive bound, a
    /// floor above its cap, and a row authoring neither; [`RepoError::CorruptRow`]
    /// when the revision reads back unusable.
    ///
    /// Those four are a **backstop**, and this doc used to claim they already were:
    /// it said the publish pipeline "reports [them] as
    /// `PERIOD_FLOOR_CAP_AMOUNT_INVALID` before a set ever gets here", which was the
    /// one thing nothing did. Publish judges a subject assembled *from storage*, so
    /// a `CHECK` on this table guarantees the rule can never see the row it
    /// describes, and the facet ran no door: the caller met a `0` bound as a 500
    /// carrying a constraint name. The door is `api::rest::plans`'
    /// `require_authorable_period_bounds`, which runs `inst-pfc-amount` over the
    /// submitted set at the parse — so the claim above is now true, of a different
    /// door than the one it named.
    #[allow(
        clippy::too_many_arguments,
        reason = "the siblings' signature verbatim: the scope, the (tenant, plan, revision, \
                  expected-version) the compare-and-swap addresses, the payload and the D-135 \
                  audit stamp"
    )]
    pub async fn replace_period_floor_caps(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        bounds: Vec<PeriodFloorCap>,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    replace_period_floor_caps_on(
                        txn, &scope, tenant_id, plan_id, revision, expected, bounds, stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Read one revision's period floor/cap set, ordered by `(currency,
    /// region)`.
    ///
    /// The order is here rather than left to the caller, for
    /// [`PlanShapeRepo::list_phases`]' reason: the market pair is the key, so
    /// ordering on it is total, and a set that came back differently on each
    /// read would make `PERIOD_FLOOR_CAP_MARKET_UNSOLD`'s report unreproducible.
    ///
    /// SQL-level BOLA: a foreign tenant's revision yields an empty set.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
    /// value its columns are `CHECK`-constrained to hold.
    pub async fn list_period_floor_caps(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<Vec<PeriodFloorCap>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        load_period_floor_cap_set(&conn, scope, tenant_id, plan_id, revision).await
    }

    /// Replace an open draft revision's whole composite-meter set, under the
    /// caller's row version, in one transaction (Slice 10 §6).
    ///
    /// Structurally [`PlanShapeRepo::replace_phases`] against the sibling table,
    /// and deliberately so: the guard, the ordering and the audit record are one
    /// argument each, and a second spelling of any of them is a second thing to
    /// keep true.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such revision is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the revision reads back unusable.
    ///
    /// The three siblings' text verbatim, and that is the correction: this list
    /// used to name `RepoError::LifecycleForbidden`, which is a
    /// [`DomainError`](crate::domain::error::DomainError) variant and has never
    /// been a `RepoError` one — a broken intra-doc link that survived because
    /// `infra` is `#[doc(hidden)]`. What the shared `refuse` answers for a frozen
    /// revision is [`RepoError::NotDraft`].
    #[allow(
        clippy::too_many_arguments,
        reason = "`replace_phases`' signature and `replace_phases`' reason, verbatim: the owed \
                  `RevisionTarget` bundle would change both together"
    )]
    pub async fn replace_composites(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        composites: Vec<CompositeMeter>,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    replace_composites_on(
                        txn, &scope, tenant_id, plan_id, revision, expected, composites, stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Read one revision's composite set, ordered by `output_unit` then id.
    ///
    /// SQL-level BOLA: a foreign tenant's revision yields an empty set.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when `constituent_units` is not a JSON array of
    /// strings.
    pub async fn list_composites(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<Vec<CompositeMeter>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        load_composite_set(&conn, scope, tenant_id, plan_id, revision).await
    }

    /// Replace an open draft revision's whole add-on rule set, under the
    /// caller's row version, in one transaction.
    ///
    /// Structurally [`PlanShapeRepo::replace_phases`] against the sibling table
    /// — resolve the revision, drop its rules, insert the submitted set,
    /// compare-and-swap the revision's `row_version + 1` — with one addition
    /// this table needs and the phase table does not: the submitted conflict
    /// edges are **closed under symmetry** before they are written. The module
    /// doc says why that belongs here rather than in a caller, and why
    /// `depends_on` is left directed.
    ///
    /// Wholesale replacement rather than per-rule merge, for
    /// [`PlanShapeRepo::replace_phases`]'s second reason: an add-on set is
    /// judged as a **graph** (`inst-cmp-addons` walks `depends_on` for cycles
    /// and pairs `conflicts_with` for both-required conflicts), and a partial
    /// update has no graph the rules could evaluate. Symmetry makes it the only
    /// coherent option besides: normalizing one row's conflicts can add an edge
    /// to another row, so a single-row write is already a write to the set.
    ///
    /// An **empty set is a valid request**: most plans compose with no add-ons
    /// at all, and it is also how an author clears a set they are re-writing.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such revision is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::ValueOutOfRange`] when an authored
    /// quantity bound is past what its column can hold; [`RepoError::Db`] on a
    /// scope or storage failure, which **includes** a submitted set naming one
    /// `addon_sku_id` twice (the `PRIMARY KEY` decides that), a `required` rule
    /// whose `max_qty` admits no selection, an inverted `min_qty`/`max_qty` pair
    /// and a non-positive `step_qty` — §5 names no code for any of the four, so
    /// they are the table's answer rather than a report line;
    /// [`RepoError::CorruptRow`] when the revision reads back unusable.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, \
                  revision, expected-version) the compare-and-swap addresses, the payload, and \
                  the D-135 audit stamp. `swap_guard` already takes those four together, so a \
                  `RevisionTarget` bundling them is the right shape and is owed - it is a \
                  signature change across every storage suite that pins these paths, and it \
                  changes no behaviour"
    )]
    pub async fn replace_addon_rules(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        rules: Vec<AddonRule>,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    replace_addon_rules_on(
                        txn, &scope, tenant_id, plan_id, revision, expected, rules, stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Read one revision's add-on rules, ordered by `addon_sku_id`.
    ///
    /// The order is here rather than left to the caller, and one column makes it
    /// **total**: `addon_sku_id` is the third key column, so within one
    /// `(plan_id, plan_revision)` no two rows share it. `inst-cmp-addons`
    /// reports a cycle by naming its members, and a set that came back in a
    /// different order on each read would make that report unreproducible — the
    /// same argument [`PlanShapeRepo::list_phases`] gives for its tie-break.
    ///
    /// SQL-level BOLA: a foreign tenant's revision yields an empty set.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
    /// value its columns are constrained to hold — including an edge column that
    /// is not a JSON array of uuids.
    pub async fn list_addon_rules(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<Vec<AddonRule>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        load_addon_rule_set(&conn, scope, tenant_id, plan_id, revision).await
    }

    /// Attach an open draft revision's billing descriptor set, under the
    /// caller's row version, in one transaction.
    ///
    /// **Upsert, and the two words mean the same call here.** The table is 1:1
    /// per revision, so there is no difference between attaching a set and
    /// replacing one: the row is deleted and written again, which is the same
    /// shape the two sibling child sets use and which keeps
    /// [`delete_descriptor_set`] the single spelling of "this revision's set is
    /// gone" — the one D-145 needs. It is deliberately **not** a column-wise
    /// UPDATE: a partial update makes clearing a descriptor indistinguishable
    /// from not mentioning it, and `flow-plan-author` step 4 attaches
    /// descriptors incrementally, so both operations are real.
    ///
    /// An **empty set is a valid request**, and it writes a row: a draft may be
    /// incomplete (`DESCRIPTOR_INCOMPLETE` reports the missing elements at
    /// publish, by name), and refusing to store what an author has typed so far
    /// would make the incremental path unusable.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such revision is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the revision reads back unusable.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, \
                  revision, expected-version) the compare-and-swap addresses, the payload, and \
                  the D-135 audit stamp. `swap_guard` already takes those four together, so a \
                  `RevisionTarget` bundling them is the right shape and is owed - it is a \
                  signature change across every storage suite that pins these paths, and it \
                  changes no behaviour"
    )]
    pub async fn set_descriptor_set(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        descriptors: DescriptorSet,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    set_descriptor_set_on(
                        txn,
                        &scope,
                        tenant_id,
                        plan_id,
                        revision,
                        expected,
                        descriptors,
                        stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Read one revision's descriptor set, when it has one.
    ///
    /// `None` is an **unattached** set, which is a different fact from an
    /// attached-but-empty one and is kept distinct all the way to the rule:
    /// `inst-ds-required` reports all three required fields in both cases, so
    /// the distinction changes no verdict — but collapsing it here would mean an
    /// authoring surface could not tell an author who has attached nothing from
    /// one who attached a set and cleared every field.
    ///
    /// SQL-level BOLA: a foreign tenant's revision yields `None`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when `additional_fields` is not a JSON object
    /// of strings.
    pub async fn find_descriptor_set(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<Option<DescriptorSet>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        load_descriptor(&conn, scope, tenant_id, plan_id, revision).await
    }
}

// ---------------------------------------------------------------------------
// Transaction plumbing.
// ---------------------------------------------------------------------------

/// Flatten the transaction wrapper back into this repository's vocabulary, so a
/// typed refusal survives the rollback rather than reaching a caller as "the
/// store failed".
fn tx_failure(err: TxError<RepoError>) -> RepoError {
    err.into_domain(|infra| RepoError::Db(format!("plan shape transaction: {infra}")))
}

// ---------------------------------------------------------------------------
// The revision's shape, through whichever runner the caller holds.
//
// Public and runner-taking because the publish path assembles the subject
// **twice** — once as the §4.2 step-2 pre-check and again inside the commit
// transaction, where `Db::conn()` is refused by the toolkit's
// transaction-bypass guard. One implementation with two entry points, because
// two queries are two things the two runs could disagree about, and that
// disagreement is what §4.2's re-validation exists to detect rather than to
// create.
// ---------------------------------------------------------------------------

/// A revision's phase chain, in the total order [`PlanShapeRepo::list_phases`]
/// promises.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
/// value its columns are `CHECK`-constrained to hold.
pub async fn load_phase_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<PlanPhase>, RepoError> {
    load_phases(runner, scope, tenant_id, plan_id, revision)
        .await?
        .iter()
        .map(to_domain)
        .collect()
}

/// A revision's add-on rule set; see [`load_phase_set`] for why this shape
/// exists.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
/// value its columns are constrained to hold — including an edge column that is
/// not a JSON array of uuids.
pub async fn load_addon_rule_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<AddonRule>, RepoError> {
    load_addon_rules(runner, scope, tenant_id, plan_id, revision)
        .await?
        .iter()
        .map(to_addon_rule)
        .collect()
}

/// A revision's billing descriptor set; see [`load_phase_set`].
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when `additional_fields` is not a JSON object of
/// strings.
pub async fn load_descriptor(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Option<DescriptorSet>, RepoError> {
    load_descriptor_set(runner, scope, tenant_id, plan_id, revision)
        .await?
        .as_ref()
        .map(to_descriptor_set)
        .transpose()
}

/// A revision's period floor/cap set; see [`load_phase_set`] for why this shape
/// exists (**D-319**).
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
/// value its columns are `CHECK`-constrained to hold — a negative amount, or a
/// currency code the ISO 4217 type refuses.
pub async fn load_period_floor_cap_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<PeriodFloorCap>, RepoError> {
    load_period_floor_caps(runner, scope, tenant_id, plan_id, revision)
        .await?
        .iter()
        .map(to_period_floor_cap)
        .collect()
}

// ---------------------------------------------------------------------------
// Statements.
// ---------------------------------------------------------------------------

/// Read one revision's stored phase rows, in the total order
/// [`PlanShapeRepo::list_phases`] promises.
async fn load_phases(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<plan_phase::Model>, RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(Vec::new());
    };
    plan_phase::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_phase::Column::TenantId.eq(tenant_id))
                .add(plan_phase::Column::PlanId.eq(plan_id.get()))
                .add(plan_phase::Column::PlanRevision.eq(number)),
        )
        .order_by(plan_phase::Column::Ordinal, Order::Asc)
        .order_by(plan_phase::Column::PhaseId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read plan phases: {e}")))
}

/// Write a phase set, one row at a time.
///
/// One statement per phase rather than a multi-row insert, because
/// `scope_with_model` validates the tenant of the `ActiveModel` it is given, and
/// that validation is the second half of the tenant rule the module doc states:
/// the value is copied from the parent revision, and then checked against the
/// caller's scope. A phase chain is a handful of rows, so the cost of being
/// checked is a handful of statements.
async fn insert_phases(
    runner: &impl DBRunner,
    scope: &AccessScope,
    phases: Vec<plan_phase::ActiveModel>,
) -> Result<(), RepoError> {
    for phase in phases {
        // The id is read off the model before the insert takes it, because it is
        // what the refusal has to name and the driver's error carries the
        // constraint but never the value.
        let phase_id = phase_id_of(&phase);
        plan_phase::Entity::insert(phase.clone())
            .secure()
            .scope_with_model(scope, &phase)
            .map_err(|e| RepoError::Db(format!("pricing_plan_phase scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| phase_id_in_use_or_db(&e, phase_id, "insert pricing_plan_phase"))?;
    }
    Ok(())
}

/// The taken-phase-id refusal, or an ordinary storage failure (**D-340**).
///
/// [`bundle_repo`](super::bundle_repo)'s `duplicate_bundle_or_db` verbatim, down
/// to the conjunct: the **typed unique class first**, the message only to say
/// *which* constraint. Reading the driver's text alone would answer
/// [`RepoError::PhaseIdInUse`] for any failure whose message happened to carry the
/// DDL name — a lock report naming the index, a planner error quoting it — telling
/// a caller to rename a phase for a fault that has nothing to do with one. The
/// fallback runs the safe way: an unrecognised failure stays
/// [`RepoError::Db`], which is what this call answered for everything before
/// D-340.
///
/// The one-row-at-a-time insert above is what makes the id knowable at all. A
/// multi-row insert would report the constraint and leave the caller to work out
/// which of the submitted phases tripped it — the fault D-340 measured, in a
/// smaller form.
///
/// `phase_id` is an `Option` because [`phase_id_of`] is total, and a model that
/// carries no id takes the storage-failure arm: this refusal exists to name the
/// id, so one that cannot name it has nothing to offer over the answer it
/// replaced.
///
/// # One other constraint on this table has a door in front of it now (M49 / H8)
///
/// `chk_pricing_plan_phase_display_trial_days` — `display_trial_days IS NULL OR
/// display_trial_days = phase_duration_days`, `pricing_plan_phase`, which supersedes
/// `pricing_plan_phase`'s two copies — is not a unique violation, so a payload that
/// drifts the pair takes the fallback arm here and would reach its author as a 500
/// advising a retry.
///
/// The verification register filed that as **M49** and left it open on
/// the reason *"needs the same vocabulary decision"* the three duplicate-key doors
/// were held for. Both halves of that reason are false, and this paragraph records
/// it so the entry is not re-filed: `DISPLAY_TRIAL_DAYS_INVALID` was already a
/// declared code with three raise sites, so no vocabulary was owed; and the door
/// landed the same day under the H8 heading —
/// [`require_no_stranded_rows`](crate::api::rest::plans) evaluates
/// `DisplayTrialDaysOnTrialPhase` and keeps its `violate_at_write` drift arm, ahead
/// of `target_revision`, on the `phases` facet.
///
/// That facet is the only author-facing writer of these rows —
/// [`PlanShapeRepo::replace_phases`] is called from there and nowhere else, and
/// [`copy_phases`] carries rows the CHECK has already passed. So this arm is the
/// CHECK's backstop rather than an author's answer, which is the same arrangement
/// D-148 states for the read-then-index pair.
fn phase_id_in_use_or_db(err: &ScopeError, phase_id: Option<Uuid>, context: &str) -> RepoError {
    if let Some(phase_id) = phase_id
        && err.is_unique_violation()
        && names_the_phase_key(&err.to_string())
    {
        return RepoError::PhaseIdInUse(phase_id.to_string());
    }
    RepoError::Db(format!("{context}: {err}"))
}

/// The `phase_id` a model carries, for the refusal to name.
///
/// Total rather than `ActiveValue::unwrap`, which panics on `NotSet`. Every
/// producer in this module — [`phase_model_for`], [`copy_phases`] — `Set`s the
/// column, so `None` is unreachable today; what it must not be is a panic
/// discovered on the failure path, where the ordinary answer is a storage error
/// nobody had to fabricate an id for.
fn phase_id_of(model: &plan_phase::ActiveModel) -> Option<Uuid> {
    match model.phase_id {
        ActiveValue::Set(id) | ActiveValue::Unchanged(id) => Some(id),
        ActiveValue::NotSet => None,
    }
}

/// Does this driver message name `pricing_plan_phase`'s **primary key** rather
/// than the other unique constraint on the same table?
///
/// Two renderings, one per backend, both of them names `pricing_plan_phase` writes:
/// Postgres names the **constraint**, `SQLite` spells `PRIMARY KEY` inside the
/// `CREATE TABLE` and so names the indexed **columns**. Neither engine produces
/// the other's spelling, so both arms are live.
///
/// **The other unique constraint is what makes this a name match and not "a
/// unique index refused"**: `uq_pricing_plan_phase_terminal` is partial on
/// `(plan_id, plan_revision) WHERE converts_to_phase_id IS NULL`, so a payload
/// carrying two terminal phases trips it — a *different* refusal whose remedy is to
/// give one of them a `convertsToPhaseId`, not to rename it.
///
/// **No longer reachable through the `phases` facet, and this arm is now a
/// backstop**. Routing that constraint here was right and the
/// class it landed in was not: `RepoError::Db` renders `500 … please retry later`
/// for a caller-owned fault no retry fixes, which is what D-340 filed as `[H]` about
/// this very statement's other constraint. It is fixed at the door instead —
/// `inst-ph-graph` owns "exactly one terminal phase" and now runs at the write stage
/// on that facet — rather than by giving this function a second typed code, which
/// would have put two owners on one requirement. The discrimination below is
/// therefore unchanged, and both arms still have to be right: a bulk or internal
/// writer that bypasses the facet reaches this line, and answering such a caller
/// `PHASE_ID_IN_USE` would send them to change the one thing that is not wrong.
///
/// Its `SQLite` rendering names `pricing_plan_phase.plan_id,
/// pricing_plan_phase.plan_revision` and cannot contain `pricing_plan_phase.phase_id`;
/// the qualified column is matched rather than the bare one so that
/// `converts_to_phase_id` and other tables' `phase_id` columns cannot be mistaken
/// for it.
///
/// Split out and taking a `&str` so both renderings are asserted directly,
/// without staging a database race for either — `names_the_bundle_plan_slot`'s
/// arrangement, for the same reason.
fn names_the_phase_key(message: &str) -> bool {
    message.contains("pricing_plan_phase_pkey") || message.contains("pricing_plan_phase.phase_id")
}

/// Drop one revision's whole phase set.
///
/// `pub(super)` because it is half of D-145's discharge as well as half of a
/// replacement: [`PlanRepo::abandon_draft`](super::PlanRepo::abandon_draft)
/// calls it **before** it flips the revision row, since `abandoned` is not
/// `draft` and the table's DELETE trigger refuses everything afterwards.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the revision is not a `draft`.
pub(super) async fn delete_phases(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(());
    };
    plan_phase::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_phase::Column::TenantId.eq(tenant_id))
                .add(plan_phase::Column::PlanId.eq(plan_id.get()))
                .add(plan_phase::Column::PlanRevision.eq(number)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete plan phases: {e}")))?;
    Ok(())
}

/// Copy `from`'s phase rows onto revision `to`, **`phase_id` unchanged**.
///
/// D-83's copy-on-new-revision, discharged against this table.
/// [`PlanRepo::open_revision`](super::PlanRepo::open_revision) calls it inside
/// the transaction that inserted the new revision row, and after that insert,
/// because the table's INSERT trigger reads the *new* parent and requires it to
/// be `draft`.
///
/// Stable ids are the whole point of the copy. The `phase` axis of the canonical
/// scope key holds a bare `phase_id` (D-19) and same-key supersession compares
/// it (D-56), so re-minting one would move every continuing price row onto a key
/// nothing is filed under — the rows would stop superseding their predecessors,
/// and the damage would first surface as a rating miss on a plan that published
/// cleanly.
///
/// The rows are re-written from the stored models rather than round-tripped
/// through the domain: a copy is the one operation that must reproduce what is
/// there, and a value the domain cannot currently read is still a value the
/// successor revision has to inherit rather than silently lose.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the destination revision is not a
/// `draft`, and the destination revision not existing at all.
pub(super) async fn copy_phases(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    from: u64,
    to: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(to) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} revision {to} exceeds the storable range"
        )));
    };
    let source = load_phases(runner, scope, tenant_id, plan_id, from).await?;
    let copies = source
        .into_iter()
        .map(|row| plan_phase::ActiveModel {
            // Unchanged, deliberately and load-bearingly.
            phase_id: Set(row.phase_id),
            plan_revision: Set(number),
            tenant_id: Set(row.tenant_id),
            plan_id: Set(row.plan_id),
            kind: Set(row.kind),
            display_name: Set(row.display_name),
            ordinal: Set(row.ordinal),
            // The successor edge names a phase, and phase ids are stable, so it
            // needs no rewriting either: within the new revision it resolves to
            // the copy of the same phase.
            converts_to_phase_id: Set(row.converts_to_phase_id),
            phase_duration_days: Set(row.phase_duration_days),
            display_trial_days: Set(row.display_trial_days),
        })
        .collect();
    insert_phases(runner, scope, copies).await
}

// ---------------------------------------------------------------------------
// Statements - `pricing_plan_period_floor_cap` (S2 §6, D-319).
// ---------------------------------------------------------------------------
//
// The phase set's four statements again, against the fourth revision-scoped
// child: a loader, a writer, a copier and a dropper. Nothing here is
// structurally new — what is new is only which columns the rows carry.

/// Read one revision's stored bound rows, ordered by the market pair.
async fn load_period_floor_caps(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<plan_period_floor_cap::Model>, RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(Vec::new());
    };
    plan_period_floor_cap::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_period_floor_cap::Column::TenantId.eq(tenant_id))
                .add(plan_period_floor_cap::Column::PlanId.eq(plan_id.get()))
                .add(plan_period_floor_cap::Column::PlanRevision.eq(number)),
        )
        .order_by(plan_period_floor_cap::Column::Currency, Order::Asc)
        .order_by(plan_period_floor_cap::Column::Region, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read plan period floor/caps: {e}")))
}

/// Write a bound set, one row at a time — [`insert_phases`]' reason verbatim.
///
/// # Errors
/// [`RepoError::Db`] on any storage failure, **including** the
/// `(plan_id, plan_revision, currency, region)` collision a set naming one market
/// twice produces — the door's own key is the `(currency, region)` half, which is
/// what makes it the exact match for this one. That is not the untyped class it looks like: the collision cannot
/// reach here from the wire, because `api::rest::plans::require_distinct_period_markets`
/// refuses it at the parse, ahead of `target_revision` and therefore ahead of any
/// write. What is left for the index is a caller inside this crate, and a fault
/// with no wire path is a storage fault.
async fn insert_period_floor_caps(
    runner: &impl DBRunner,
    scope: &AccessScope,
    bounds: Vec<plan_period_floor_cap::ActiveModel>,
) -> Result<(), RepoError> {
    for bound in bounds {
        plan_period_floor_cap::Entity::insert(bound.clone())
            .secure()
            .scope_with_model(scope, &bound)
            .map_err(|e| RepoError::Db(format!("pricing_plan_period_floor_cap scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_plan_period_floor_cap: {e}")))?;
    }
    Ok(())
}

/// Drop one revision's whole bound set.
///
/// `pub(super)` for [`delete_phases`]' reason: it is half of D-145's discharge
/// as well as half of a replacement, and `abandon_draft` calls it **before** it
/// flips the revision.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the revision is not a `draft`.
pub(super) async fn delete_period_floor_caps(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(());
    };
    plan_period_floor_cap::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_period_floor_cap::Column::TenantId.eq(tenant_id))
                .add(plan_period_floor_cap::Column::PlanId.eq(plan_id.get()))
                .add(plan_period_floor_cap::Column::PlanRevision.eq(number)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete plan period floor/caps: {e}")))?;
    Ok(())
}

/// Copy `from`'s bound rows onto revision `to` (D-83's copy-on-new-revision).
///
/// There is no id to keep stable here — the market pair is the key and it is
/// carried across unchanged — but the rows are still re-written from the stored
/// models rather than round-tripped through the domain, for [`copy_phases`]'
/// reason: a copy must reproduce what is there, and a value the domain cannot
/// currently read is still one the successor revision has to inherit.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the destination revision is not a
/// `draft`, and the destination revision not existing at all.
pub(super) async fn copy_period_floor_caps(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    from: u64,
    to: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(to) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} revision {to} exceeds the storable range"
        )));
    };
    let source = load_period_floor_caps(runner, scope, tenant_id, plan_id, from).await?;
    let copies = source
        .into_iter()
        .map(|row| plan_period_floor_cap::ActiveModel {
            plan_id: Set(row.plan_id),
            plan_revision: Set(number),
            currency: Set(row.currency),
            region: Set(row.region),
            tenant_id: Set(row.tenant_id),
            floor_minor: Set(row.floor_minor),
            cap_minor: Set(row.cap_minor),
        })
        .collect();
    insert_period_floor_caps(runner, scope, copies).await
}

// ---------------------------------------------------------------------------
// Statements - `pricing_composite_meter` (Slice 10 §6, D-106).
// ---------------------------------------------------------------------------
//
// The phase set's four statements exactly, and the parallel is the point: a
// revision-scoped table owes a loader, a writer, a copier and a dropper, and
// `sqlite_plan_repo::the_revision_scoped_tables_are_a_closed_set_and_each_one_is_copied_and_dropped`
// refuses to let a fifth table exist without all four.

/// One revision's composite definitions, in a deterministic order.
pub(super) async fn load_composites(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<composite_meter::Model>, RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(Vec::new());
    };
    composite_meter::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(composite_meter::Column::TenantId.eq(tenant_id))
                .add(composite_meter::Column::PlanId.eq(plan_id.get()))
                .add(composite_meter::Column::PlanRevision.eq(number)),
        )
        .order_by(composite_meter::Column::OutputUnit, Order::Asc)
        .order_by(composite_meter::Column::CompositeId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read composite meters: {e}")))
}

/// The domain view of one revision's composites.
///
/// # Errors
/// [`RepoError::CorruptRow`] when a stored `constituent_units` is not a JSON
/// array of strings — a shape no writer here produces and no `CHECK` can state.
pub async fn load_composite_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<CompositeMeter>, RepoError> {
    load_composites(runner, scope, tenant_id, plan_id, revision)
        .await?
        .iter()
        .map(to_composite)
        .collect()
}

fn to_composite(row: &composite_meter::Model) -> Result<CompositeMeter, RepoError> {
    let units = row.constituent_units.as_array().ok_or_else(|| {
        RepoError::CorruptRow(format!(
            "pricing_composite_meter {}: constituent_units is not a JSON array",
            row.composite_id
        ))
    })?;
    let constituent_units = units
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                RepoError::CorruptRow(format!(
                    "pricing_composite_meter {}: a constituent unit is not a string",
                    row.composite_id
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompositeMeter {
        composite_id: row.composite_id,
        output_unit: row.output_unit.clone(),
        constituent_units,
        formula: row.formula.clone(),
    })
}

/// Write a composite set, one row at a time — [`insert_phases`]' reason verbatim.
///
/// # Errors
/// [`RepoError::Db`] on any storage failure, including the `output_unit`
/// collision a set defining one unit twice produces. It cannot reach here from the
/// wire for [`insert_period_floor_caps`]' reason:
/// `api::rest::plans::require_authorable_composites` refuses it at the parse.
async fn insert_composites(
    runner: &impl DBRunner,
    scope: &AccessScope,
    composites: Vec<composite_meter::ActiveModel>,
) -> Result<(), RepoError> {
    for composite in composites {
        composite_meter::Entity::insert(composite.clone())
            .secure()
            .scope_with_model(scope, &composite)
            .map_err(|e| RepoError::Db(format!("pricing_composite_meter scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_composite_meter: {e}")))?;
    }
    Ok(())
}

/// Replace one draft revision's composite set wholesale, **off the parent
/// revision row**.
///
/// Every value that says *which* rows these are — `tenant_id`, `plan_id` and
/// `plan_revision` — is taken from `parent`, the revision row this repository
/// resolved, and none of them from the request (Global Constraint 9, stated in
/// this module's own doc). That is the three sibling model-builders' rule. Taking
/// the caller's `tenant_id` and `plan_id` arguments instead holds the invariant
/// only because `mutable_draft` → `load_revision_row` happens to filter on those
/// same two values; taking a `&plan::Model` is what makes the divergence
/// **unwritable** rather than merely absent — this function is `pub(super)`, and a
/// second caller resolving a revision by any other predicate could pass its own
/// arguments with no test able to tell.
///
/// The revision needs no `stored_revision` range check any more, for the reason
/// the siblings never needed one: `parent.revision` is a number the column is
/// already holding.
pub(super) async fn write_composites(
    runner: &impl DBRunner,
    scope: &AccessScope,
    parent: &plan::Model,
    composites: Vec<CompositeMeter>,
) -> Result<(), RepoError> {
    let revision = u64::try_from(parent.revision).map_err(|e| {
        RepoError::CorruptRow(format!(
            "pricing_plan row for plan {} holds revision {}: {e}",
            parent.plan_id, parent.revision
        ))
    })?;
    let rows = composite_models(parent, composites);
    delete_composites(
        runner,
        scope,
        parent.tenant_id,
        PlanId::new(parent.plan_id),
        revision,
    )
    .await?;
    insert_composites(runner, scope, rows).await
}

/// Render a submitted composite set into the rows that hold it.
///
/// Every row's `tenant_id`, `plan_id` and `plan_revision` are taken from `parent`
/// — the revision row this repository resolved — and none of them from the
/// submitted value, which is why [`CompositeMeter`] carries no such field. This is
/// [`phase_models`]' contract verbatim, and deliberately so.
fn composite_models(
    parent: &plan::Model,
    composites: Vec<CompositeMeter>,
) -> Vec<composite_meter::ActiveModel> {
    composites
        .into_iter()
        .map(|composite| composite_meter::ActiveModel {
            composite_id: Set(composite.composite_id),
            plan_revision: Set(parent.revision),
            tenant_id: Set(parent.tenant_id),
            plan_id: Set(parent.plan_id),
            output_unit: Set(composite.output_unit),
            constituent_units: Set(serde_json::Value::Array(
                composite
                    .constituent_units
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            )),
            formula: Set(composite.formula),
        })
        .collect()
}

/// Drop one revision's whole composite set.
pub(super) async fn delete_composites(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(());
    };
    composite_meter::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(composite_meter::Column::TenantId.eq(tenant_id))
                .add(composite_meter::Column::PlanId.eq(plan_id.get()))
                .add(composite_meter::Column::PlanRevision.eq(number)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete composite meters: {e}")))?;
    Ok(())
}

/// Copy one revision's composites onto `to`, **preserving `composite_id`** (D-106).
pub(super) async fn copy_composites(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    from: u64,
    to: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(to) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} revision {to} exceeds the storable range"
        )));
    };
    let source = load_composites(runner, scope, tenant_id, plan_id, from).await?;
    let copies = source
        .into_iter()
        .map(|row| composite_meter::ActiveModel {
            // Unchanged, deliberately and load-bearingly: a stable id is what
            // makes a draft's formula edit invisible to the published revision.
            composite_id: Set(row.composite_id),
            plan_revision: Set(number),
            tenant_id: Set(row.tenant_id),
            plan_id: Set(row.plan_id),
            output_unit: Set(row.output_unit),
            constituent_units: Set(row.constituent_units),
            formula: Set(row.formula),
        })
        .collect();
    insert_composites(runner, scope, copies).await
}

// ---------------------------------------------------------------------------
// Statements - `pricing_plan_addon_rule`.
// ---------------------------------------------------------------------------

/// Read one revision's stored add-on rule rows, in the total order
/// [`PlanShapeRepo::list_addon_rules`] promises.
async fn load_addon_rules(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Vec<plan_addon_rule::Model>, RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(Vec::new());
    };
    plan_addon_rule::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_addon_rule::Column::TenantId.eq(tenant_id))
                .add(plan_addon_rule::Column::PlanId.eq(plan_id.get()))
                .add(plan_addon_rule::Column::PlanRevision.eq(number)),
        )
        .order_by(plan_addon_rule::Column::AddonSkuId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read plan add-on rules: {e}")))
}

/// Write an add-on rule set, one row at a time.
///
/// One statement per rule for [`insert_phases`]'s reason: `scope_with_model`
/// validates the tenant of the `ActiveModel` it is given, and that validation is
/// the second half of the tenant rule the module doc states.
///
/// # Errors
/// [`RepoError::Db`] on any storage failure, including the
/// `(plan_id, plan_revision, addon_sku_id)` collision a set naming one add-on
/// twice produces. It cannot reach here from the wire for
/// [`insert_period_floor_caps`]' reason:
/// `api::rest::plans::require_distinct_addon_skus` refuses it at the parse.
///
/// So the typed refusal [`insert_phases`] carries is not owed here in the way its
/// shape suggests: `phase_id` is caller-mintable and its door is the store's, while
/// these three keys are all closed above. What a typed refusal would add is the
/// enumerated envelope the publish pre-check answers with — the three doors each
/// name the first duplicate only — and that is a design-set change, since a
/// refusal code in this gear is normative vocabulary `plan_rules_tests` refuses to
/// let a REST door mint.
async fn insert_addon_rules(
    runner: &impl DBRunner,
    scope: &AccessScope,
    rules: Vec<plan_addon_rule::ActiveModel>,
) -> Result<(), RepoError> {
    for rule in rules {
        plan_addon_rule::Entity::insert(rule.clone())
            .secure()
            .scope_with_model(scope, &rule)
            .map_err(|e| RepoError::Db(format!("pricing_plan_addon_rule scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_plan_addon_rule: {e}")))?;
    }
    Ok(())
}

/// Drop one revision's whole add-on rule set.
///
/// `pub(super)` because it is half of D-145's discharge as well as half of a
/// replacement: [`PlanRepo::abandon_draft`](super::PlanRepo::abandon_draft)
/// calls it **before** it flips the revision row, since `abandoned` is not
/// `draft` and the table's DELETE trigger refuses everything afterwards.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the revision is not a `draft`.
pub(super) async fn delete_addon_rules(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(());
    };
    plan_addon_rule::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_addon_rule::Column::TenantId.eq(tenant_id))
                .add(plan_addon_rule::Column::PlanId.eq(plan_id.get()))
                .add(plan_addon_rule::Column::PlanRevision.eq(number)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete plan add-on rules: {e}")))?;
    Ok(())
}

/// Copy `from`'s add-on rules onto revision `to`, **`addon_sku_id` unchanged**.
///
/// D-83's copy-on-new-revision, discharged against this table.
/// [`PlanRepo::open_revision`](super::PlanRepo::open_revision) calls it inside
/// the transaction that inserted the new revision row, and after that insert,
/// because the table's INSERT trigger reads the *new* parent and requires it to
/// be `draft`.
///
/// The discriminator is stable for a different reason than `phase_id`'s: it is
/// not this gear's id at all but the registry's SKU id, so "re-minting" it is
/// not even available. What the copy must not do is **drop** a rule — a
/// successor revision silently composing with fewer add-ons than its
/// predecessor is a plan whose buyers lose an entitlement nobody removed.
///
/// The rows are re-written from the stored models rather than round-tripped
/// through the domain, as [`copy_phases`] is and for the same reason, with one
/// extra consequence here: the edge sets travel as the stored JSON, so the
/// symmetry the write closed is carried forward by construction rather than
/// re-derived from a set the copy might read differently.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the destination revision is not a
/// `draft`, and the destination revision not existing at all.
pub(super) async fn copy_addon_rules(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    from: u64,
    to: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(to) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} revision {to} exceeds the storable range"
        )));
    };
    let source = load_addon_rules(runner, scope, tenant_id, plan_id, from).await?;
    let copies = source
        .into_iter()
        .map(|row| plan_addon_rule::ActiveModel {
            plan_id: Set(row.plan_id),
            plan_revision: Set(number),
            // Unchanged: it is the registry's SKU id, and it is what the edge
            // sets of every sibling rule name.
            addon_sku_id: Set(row.addon_sku_id),
            tenant_id: Set(row.tenant_id),
            required: Set(row.required),
            min_qty: Set(row.min_qty),
            max_qty: Set(row.max_qty),
            step_qty: Set(row.step_qty),
            price_override_ref: Set(row.price_override_ref),
            depends_on_addon_sku_id: Set(row.depends_on_addon_sku_id),
            conflicts_with_addon_sku_id: Set(row.conflicts_with_addon_sku_id),
        })
        .collect();
    insert_addon_rules(runner, scope, copies).await
}

// ---------------------------------------------------------------------------
// Statements - `pricing_plan_descriptor_set`.
// ---------------------------------------------------------------------------

/// Read one revision's stored descriptor row, when it has one.
async fn load_descriptor_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<Option<plan_descriptor_set::Model>, RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(None);
    };
    plan_descriptor_set::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_descriptor_set::Column::TenantId.eq(tenant_id))
                .add(plan_descriptor_set::Column::PlanId.eq(plan_id.get()))
                .add(plan_descriptor_set::Column::PlanRevision.eq(number)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read plan descriptor set: {e}")))
}

/// Write one revision's descriptor row.
///
/// `scope_with_model` validates the tenant of the `ActiveModel` it is given,
/// which is the second half of the tenant rule the module doc states: the value
/// is copied from the parent revision, and then checked against the caller's
/// scope.
async fn insert_descriptor_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row: plan_descriptor_set::ActiveModel,
) -> Result<(), RepoError> {
    plan_descriptor_set::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(|e| RepoError::Db(format!("pricing_plan_descriptor_set scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("insert pricing_plan_descriptor_set: {e}")))?;
    Ok(())
}

/// Drop one revision's descriptor set.
///
/// `pub(super)` because it is half of D-145's discharge as well as half of an
/// upsert: [`PlanRepo::abandon_draft`](super::PlanRepo::abandon_draft) calls it
/// **before** it flips the revision row, since `abandoned` is not `draft` and
/// the table's DELETE trigger refuses everything afterwards.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the revision is not a `draft`.
pub(super) async fn delete_descriptor_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(revision) else {
        return Ok(());
    };
    plan_descriptor_set::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_descriptor_set::Column::TenantId.eq(tenant_id))
                .add(plan_descriptor_set::Column::PlanId.eq(plan_id.get()))
                .add(plan_descriptor_set::Column::PlanRevision.eq(number)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete plan descriptor set: {e}")))?;
    Ok(())
}

/// Copy `from`'s descriptor set onto revision `to`.
///
/// D-83's copy-on-new-revision, discharged against the last of the three child
/// tables. [`PlanRepo::open_revision`](super::PlanRepo::open_revision) calls it
/// inside the transaction that inserted the new revision row, and after that
/// insert, because the table's INSERT trigger reads the *new* parent and
/// requires it to be `draft`.
///
/// A revision with no set copies nothing rather than writing an empty row: the
/// successor of an unattached set is an unattached set, and materializing an
/// empty one would tell an authoring surface that somebody had attached
/// something.
///
/// The row is re-written from the stored model rather than round-tripped
/// through the domain, as the two sibling copies are: a copy is the one
/// operation that must reproduce what is there, and a value the domain cannot
/// currently read — a P5 extra field a later deployment adds — is still a value
/// the successor revision has to inherit rather than silently lose.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only trigger's refusal when the destination revision is not a
/// `draft`, and the destination revision not existing at all.
pub(super) async fn copy_descriptor_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    from: u64,
    to: u64,
) -> Result<(), RepoError> {
    let Some(number) = stored_revision(to) else {
        return Err(RepoError::CorruptRow(format!(
            "plan {plan_id} revision {to} exceeds the storable range"
        )));
    };
    let Some(source) = load_descriptor_set(runner, scope, tenant_id, plan_id, from).await? else {
        return Ok(());
    };
    let copy = plan_descriptor_set::ActiveModel {
        plan_id: Set(source.plan_id),
        plan_revision: Set(number),
        tenant_id: Set(source.tenant_id),
        invoice_line_template: Set(source.invoice_line_template),
        gl_code: Set(source.gl_code),
        itemization_rule: Set(source.itemization_rule),
        additional_fields: Set(source.additional_fields),
    };
    insert_descriptor_set(runner, scope, copy).await
}

/// Bump the revision's `row_version` under `guard`, and say how many rows moved.
///
/// The bump is `row_version + 1` **inside** the statement that matches on the
/// version the caller read, for the reason [`super::plan_repo`]'s module doc
/// gives: computing the successor in Rust would let two writers holding the same
/// current version compute the same next one and both write it.
/// Bump the revision's `row_version` under `guard`, in the same statement that
/// matches on the version the caller read.
///
/// `pub(super)` because [`bundle_repo`](super::bundle_repo) performs the same
/// compare-and-swap for the Slice-8 composition: a bundle's component and
/// rev-share sets ride the plan revision's entity tag exactly as this module's
/// child sets do, so re-spelling the swap there would be a second copy of the
/// one statement that must not drift.
pub(super) async fn plan_revision_bump(
    runner: &impl DBRunner,
    scope: &AccessScope,
    guard: Condition,
) -> Result<u64, RepoError> {
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            plan::Column::RowVersion,
            Expr::col(plan::Column::RowVersion).add(1_i64),
        )
        .filter(guard)
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("bump plan revision for a shape edit: {e}")))?;
    Ok(result.rows_affected)
}

// ---------------------------------------------------------------------------
// Domain <-> storage.
// ---------------------------------------------------------------------------

/// One phase as a row of `pricing_plan_phase`.
///
/// Extracted from [`phase_models`] so the creation-time seed and the whole-set
/// replace build the row **one** way: two constructions of the same row are two
/// places for a column added later to be forgotten.
fn phase_model_for(
    tenant_id: Uuid,
    plan_id: Uuid,
    revision: i64,
    phase: &PlanPhase,
) -> Result<plan_phase::ActiveModel, RepoError> {
    Ok(plan_phase::ActiveModel {
        phase_id: Set(phase.phase_id.get()),
        plan_revision: Set(revision),
        tenant_id: Set(tenant_id),
        plan_id: Set(plan_id),
        kind: Set(phase.kind.as_str().to_owned()),
        display_name: Set(phase.display_name.clone()),
        ordinal: Set(phase.ordinal),
        converts_to_phase_id: Set(phase.converts_to_phase_id.map(PhaseId::get)),
        phase_duration_days: Set(stored_count(
            "phaseDurationDays",
            phase.phase_duration_days,
        )?),
        display_trial_days: Set(stored_count("displayTrialDays", phase.display_trial_days)?),
    })
}

/// Render a submitted phase set into the rows that hold it.
///
/// Every row's `tenant_id`, `plan_id` and `plan_revision` are taken from
/// `parent` — the revision row this repository resolved — and none of them from
/// the submitted value, which is why [`PlanPhase`] carries no such field to
/// begin with.
fn phase_models(
    parent: &plan::Model,
    phases: &[PlanPhase],
) -> Result<Vec<plan_phase::ActiveModel>, RepoError> {
    phases
        .iter()
        .map(|phase| phase_model_for(parent.tenant_id, parent.plan_id, parent.revision, phase))
        .collect()
}

/// Write the implicit terminal phase a plan is created with (D-19,
/// `inst-ph-default`).
///
/// **Not [`replace_phases_on`].** That is a compare-and-swap: it re-reads the
/// draft, bumps the revision's row version and writes its own D-135 audit
/// record. A plan created through it would be born at version 1 carrying a
/// record of an edit nobody made, and the tag the create answered would already
/// be stale. Creation is one act; the phase is part of it, so this writes the
/// child row and nothing else.
///
/// **The runner must be the create's transaction** —
/// [`plan_repo::create_draft_on`](super::plan_repo::create_draft_on)'s house
/// rule: a committed plan with no phase is the state this call exists to
/// abolish, so the two writes commit together or neither does.
///
/// `tenant_id` is a parameter because [`PlanRevision`] carries none — the
/// revision is identified by `(plan_id, revision)` and the tenant is the
/// caller's, checked against the caller's scope by `scope_with_model` on the way
/// in, exactly as [`phase_models`]' rows are.
///
/// # Errors
/// [`RepoError::ValueOutOfRange`] if the revision number is past what its
/// `bigint` column holds — checked rather than cast, since a cast would address
/// a different revision. [`RepoError::Db`] if the insert fails or the model's
/// tenant does not match the caller's scope, which includes the child table's
/// append-only trigger refusing a parent that is not a `draft`.
pub async fn seed_terminal_phase_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    revision: &PlanRevision,
    phase: &PlanPhase,
) -> Result<(), RepoError> {
    let Some(stored) = stored_revision(revision.revision) else {
        return Err(RepoError::ValueOutOfRange {
            field: "revision".to_owned(),
            value: revision.revision.to_string(),
        });
    };
    let model = phase_model_for(tenant_id, revision.plan_id.get(), stored, phase)?;
    insert_phases(runner, scope, vec![model]).await
}

/// Map a stored row to the domain value, at this boundary and nowhere else.
///
/// Both readings that can fail are **invariant breaches, not caller mistakes**:
/// `kind` is `CHECK`-constrained to the token set [`PhaseKind`] renders, and the
/// two day columns only ever hold counts. A row that reads otherwise means
/// something reached the table outside this gear.
fn to_domain(row: &plan_phase::Model) -> Result<PlanPhase, RepoError> {
    Ok(PlanPhase {
        phase_id: PhaseId::new(row.phase_id),
        kind: read_token(
            "pricing_plan_phase.kind",
            &row.kind,
            PhaseKind::ALL,
            PhaseKind::as_str,
        )?,
        display_name: row.display_name.clone(),
        ordinal: row.ordinal,
        converts_to_phase_id: row.converts_to_phase_id.map(PhaseId::new),
        phase_duration_days: read_count(
            "pricing_plan_phase.phase_duration_days",
            row.phase_duration_days,
        )?,
        display_trial_days: read_count(
            "pricing_plan_phase.display_trial_days",
            row.display_trial_days,
        )?,
    })
}

/// Render a submitted period floor/cap set into the rows that hold it
/// (**D-319**).
///
/// Every row's `tenant_id`, `plan_id` and `plan_revision` are taken from
/// `parent`, which is why [`PeriodFloorCap`] carries no such field. The market
/// pair **is** taken from the submitted value: it is the authored key, not a
/// fact about the revision.
fn period_floor_cap_models(
    parent: &plan::Model,
    bounds: &[PeriodFloorCap],
) -> Vec<plan_period_floor_cap::ActiveModel> {
    bounds
        .iter()
        .map(|bound| plan_period_floor_cap::ActiveModel {
            plan_id: Set(parent.plan_id),
            plan_revision: Set(parent.revision),
            currency: Set(bound.currency.as_str().to_owned()),
            region: Set(bound.region.as_str().to_owned()),
            tenant_id: Set(parent.tenant_id),
            floor_minor: Set(bound.floor_minor.map(MinorAmount::get)),
            cap_minor: Set(bound.cap_minor.map(MinorAmount::get)),
        })
        .collect()
}

/// Map a stored bound row to the domain value, at this boundary and nowhere
/// else.
///
/// Every reading that can fail is an **invariant breach, not a caller
/// mistake**: `currency` and `region` are written from validated domain values,
/// and the two amount columns are `CHECK`-constrained strictly positive. A row
/// that reads otherwise means something reached the table outside this gear.
fn to_period_floor_cap(row: &plan_period_floor_cap::Model) -> Result<PeriodFloorCap, RepoError> {
    Ok(PeriodFloorCap {
        currency: CurrencyCode::new(&row.currency).map_err(|e| {
            RepoError::CorruptRow(format!(
                "pricing_plan_period_floor_cap.currency holds {}: {e}",
                row.currency
            ))
        })?,
        region: Region::new(&row.region).map_err(|e| {
            RepoError::CorruptRow(format!(
                "pricing_plan_period_floor_cap.region holds {}: {e}",
                row.region
            ))
        })?,
        floor_minor: read_minor("floor_minor", row.floor_minor)?,
        cap_minor: read_minor("cap_minor", row.cap_minor)?,
    })
}

/// Read a stored minor-unit amount back into the domain's checked type.
fn read_minor(column: &str, stored: Option<i64>) -> Result<Option<MinorAmount>, RepoError> {
    let Some(value) = stored else {
        return Ok(None);
    };
    MinorAmount::new(value).map(Some).map_err(|_| {
        RepoError::CorruptRow(format!(
            "pricing_plan_period_floor_cap.{column} holds {value}, not an amount"
        ))
    })
}

/// Render a submitted add-on rule set into the rows that hold it, **with the
/// conflict edges closed under symmetry**.
///
/// Every row's `tenant_id`, `plan_id` and `plan_revision` are taken from
/// `parent` — the revision row this repository resolved — and none of them from
/// the submitted value, which is why [`AddonRule`] carries no such field.
///
/// The closure is computed over the whole submitted set before any row is
/// rendered, because an edge authored on rule A changes what rule B stores; see
/// the module doc for why an asymmetric pair could not be left to a caller.
fn addon_rule_models(
    parent: &plan::Model,
    rules: &[AddonRule],
) -> Result<Vec<plan_addon_rule::ActiveModel>, RepoError> {
    let conflicts = symmetric_conflicts(rules);
    rules
        .iter()
        .map(|rule| {
            Ok(plan_addon_rule::ActiveModel {
                plan_id: Set(parent.plan_id),
                plan_revision: Set(parent.revision),
                addon_sku_id: Set(rule.addon_sku_id),
                tenant_id: Set(parent.tenant_id),
                required: Set(rule.required),
                min_qty: Set(stored_count("minQty", rule.min_qty)?),
                max_qty: Set(stored_count("maxQty", rule.max_qty)?),
                step_qty: Set(stored_count("stepQty", rule.step_qty)?),
                price_override_ref: Set(rule.price_override_ref),
                depends_on_addon_sku_id: Set(edge_json(rule.depends_on.iter().copied())),
                conflicts_with_addon_sku_id: Set(edge_json(
                    conflicts
                        .get(&rule.addon_sku_id)
                        .into_iter()
                        .flatten()
                        .copied(),
                )),
            })
        })
        .collect()
}

/// The submitted conflict edges, closed under symmetry.
///
/// The closure is unconditional and the **row set is what bounds it**:
/// [`addon_rule_models`] renders one row per submitted rule and looks this map
/// up by that rule's `addon_sku_id`, so a back-edge computed against an id no
/// submitted rule carries is simply never read. An edge naming a non-member
/// therefore stays one-sided in storage without a filter here saying so — and it
/// must, because it is exactly what `ADDON_INCOMPATIBLE` reports, and a row
/// invented to carry its reverse would turn a finding the author has to see into
/// a silent repair.
///
/// # There is deliberately no membership filter here, and it must not come back
///
/// The obvious-looking guard — `if members.contains(target)` before the
/// back-edge, over a set of the submitted `addon_sku_id`s — was written first and
/// then deleted. It is not missing and it was not forgotten.
///
/// **It cannot fail.** The rendering loop reads this map only at ids that
/// *are* submitted rules, so an entry keyed by a non-member is unreachable by
/// construction; adding or omitting it changes no stored row, no report and no
/// verdict. Mutation testing confirmed it: with the filter removed, every test
/// in the crate still passed — which is the signature of a guard that reads as
/// enforcement while enforcing nothing, the same fault
/// [`crate::domain::rules`]'s absence section refuses for rules that always
/// pass. Re-adding it would buy a reader the impression that something is
/// checked here and would cost the next person a guard no test can hold.
///
/// What actually keeps back-edges off non-members is the row set, one line
/// below in [`addon_rule_models`]. If that loop is ever changed to render rows
/// from **this map's keys** rather than from the submitted rules, the bound
/// disappears with it and the filter becomes real work — that edit is the one
/// occasion to restore it, and it would need a test that fails without it.
fn symmetric_conflicts(rules: &[AddonRule]) -> BTreeMap<Uuid, BTreeSet<Uuid>> {
    let mut closed: BTreeMap<Uuid, BTreeSet<Uuid>> = BTreeMap::new();
    for rule in rules {
        for target in &rule.conflicts_with {
            closed.entry(rule.addon_sku_id).or_default().insert(*target);
            closed.entry(*target).or_default().insert(rule.addon_sku_id);
        }
    }
    closed
}

/// Render an edge set as the JSON array of uuid strings both backends hold.
///
/// Sorted and de-duplicated, because an edge set **is a set**: its order carries
/// no meaning, so imposing one costs nothing and buys a stored value that does
/// not depend on the order somebody authored in. Two authorings of the same set
/// then produce one row, a copy-forward is byte-identical to its source, and the
/// cycle walk sees one edge per pair rather than however many times it was
/// typed.
fn edge_json(edges: impl IntoIterator<Item = Uuid>) -> JsonValue {
    let unique: BTreeSet<Uuid> = edges.into_iter().collect();
    let rendered: Vec<String> = unique.iter().map(ToString::to_string).collect();
    json!(rendered)
}

/// Read an edge set back out of its JSON column.
///
/// A column that is not an array of uuid strings is an **invariant breach**: it
/// is `NOT NULL DEFAULT '[]'`, this repository is its only writer, and the value
/// it writes is rendered from [`Uuid`]s. So it surfaces as
/// [`RepoError::CorruptRow`], the way `pricing_price.included_allowance` does —
/// and resting on the same weaker ground, since no CHECK looks inside a JSON
/// document.
fn read_edges(column: &str, stored: &JsonValue) -> Result<Vec<Uuid>, RepoError> {
    let malformed =
        || RepoError::CorruptRow(format!("{column} is not a JSON array of uuids: {stored}"));
    stored
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .map(|value| {
            value
                .as_str()
                .and_then(|text| Uuid::parse_str(text).ok())
                .ok_or_else(malformed)
        })
        .collect()
}

/// Map a stored add-on rule row to the domain value, at this boundary and
/// nowhere else.
///
/// Both readings that can fail are **invariant breaches, not caller mistakes**:
/// the quantity columns only ever hold counts this repository wrote from a
/// `u32`, and the edge columns only ever hold what [`edge_json`] rendered.
fn to_addon_rule(row: &plan_addon_rule::Model) -> Result<AddonRule, RepoError> {
    Ok(AddonRule {
        addon_sku_id: row.addon_sku_id,
        required: row.required,
        min_qty: read_count("pricing_plan_addon_rule.min_qty", row.min_qty)?,
        max_qty: read_count("pricing_plan_addon_rule.max_qty", row.max_qty)?,
        step_qty: read_count("pricing_plan_addon_rule.step_qty", row.step_qty)?,
        price_override_ref: row.price_override_ref,
        depends_on: read_edges(
            "pricing_plan_addon_rule.depends_on_addon_sku_id",
            &row.depends_on_addon_sku_id,
        )?,
        conflicts_with: read_edges(
            "pricing_plan_addon_rule.conflicts_with_addon_sku_id",
            &row.conflicts_with_addon_sku_id,
        )?,
    })
}

/// Render a submitted descriptor set into the row that holds it.
///
/// `tenant_id`, `plan_id` and `plan_revision` are taken from `parent` — the
/// revision row this repository resolved — and none of them from the submitted
/// value, which is why [`DescriptorSet`] carries no such field.
///
/// Infallible: every column is nullable text, and the extra fields are a JSON
/// object of the strings the caller handed over. There is nothing here a column
/// can refuse, which is the shape D-48's contract has once `billingTiming` and
/// `taxCategory` ride the price row instead (D-110).
fn descriptor_model(
    parent: &plan::Model,
    descriptors: &DescriptorSet,
) -> plan_descriptor_set::ActiveModel {
    plan_descriptor_set::ActiveModel {
        plan_id: Set(parent.plan_id),
        plan_revision: Set(parent.revision),
        tenant_id: Set(parent.tenant_id),
        invoice_line_template: Set(descriptors.invoice_line_template.clone()),
        gl_code: Set(descriptors.gl_code.clone()),
        itemization_rule: Set(descriptors.itemization_rule.clone()),
        additional_fields: Set(json!(descriptors.additional)),
    }
}

/// Map a stored descriptor row to the domain value, at this boundary and nowhere
/// else.
///
/// The one reading that can fail is an **invariant breach, not a caller
/// mistake**: `additional_fields` is `NOT NULL DEFAULT '{}'`, this repository is
/// its only writer, and what it writes is a JSON object of strings.
fn to_descriptor_set(row: &plan_descriptor_set::Model) -> Result<DescriptorSet, RepoError> {
    Ok(DescriptorSet {
        invoice_line_template: row.invoice_line_template.clone(),
        gl_code: row.gl_code.clone(),
        itemization_rule: row.itemization_rule.clone(),
        additional: read_additional_fields(&row.additional_fields)?,
    })
}

/// Read P5's extra descriptor fields back out of their JSON column.
///
/// A `BTreeMap` rather than the insertion order the JSON document happens to
/// carry: `DESCRIPTOR_INCOMPLETE` names every missing field, and a report whose
/// order depended on how a document was serialized would be unreproducible
/// between two runs of the same pipeline over the same plan.
fn read_additional_fields(stored: &JsonValue) -> Result<BTreeMap<String, String>, RepoError> {
    let malformed = || {
        RepoError::CorruptRow(format!(
            "pricing_plan_descriptor_set.additional_fields is not a JSON object of strings: \
             {stored}"
        ))
    };
    stored
        .as_object()
        .ok_or_else(malformed)?
        .iter()
        .map(|(name, value)| {
            value
                .as_str()
                .map(|text| (name.clone(), text.to_owned()))
                .ok_or_else(malformed)
        })
        .collect()
}

/// Render an authored count for its `int` column — a phase duration, a trial
/// projection, an add-on quantity bound.
///
/// Checked rather than cast: a cast would turn a duration no plan could run, or
/// a quantity no buyer could take, into a plausible one — and the CHECKs that
/// compare these columns (`display_trial_days = phase_duration_days`,
/// `min_qty <= max_qty`) would then be enforced against a number nobody
/// authored.
fn stored_count(field: &str, value: Option<u32>) -> Result<Option<i32>, RepoError> {
    let Some(value) = value else {
        return Ok(None);
    };
    i32::try_from(value)
        .map(Some)
        .map_err(|_| RepoError::ValueOutOfRange {
            field: field.to_owned(),
            value: value.to_string(),
        })
}

/// Read a count back into the unsigned type the domain counts in.
fn read_count(column: &str, stored: Option<i32>) -> Result<Option<u32>, RepoError> {
    let Some(value) = stored else {
        return Ok(None);
    };
    u32::try_from(value)
        .map(Some)
        .map_err(|_| RepoError::CorruptRow(format!("{column} holds {value}, not a count")))
}

/// Render a revision number for its `bigint` column, `None` past the range the
/// column can hold. Checked rather than cast, for the reason
/// [`super::plan_repo`] gives: a cast would quietly address a different
/// revision.
fn stored_revision(revision: u64) -> Option<i64> {
    i64::try_from(revision).ok()
}

/// [`PlanShapeRepo::replace_phases`]'s body, on a runner the caller owns.
///
/// **The runner must be a transaction.** This writes the child rows *and*
/// the revision's D-135 audit record, so on a bare connection they would be
/// separate autocommit statements and a failure part way would leave a
/// committed edit nobody recorded making — the property `create_draft_on`
/// states as the house rule. Every current caller supplies one.
///
/// # Errors
/// Whatever the method's own documentation states — this is that method,
/// minus the transaction it opens for itself.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, revision, expected-version) the compare-and-swap addresses, the payload and the D-135 audit stamp. The method above carries the same set; this form exists so a caller that already owns a transaction can join it rather than open a second (D-272)."
)]
pub async fn replace_phases_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
    phases: Vec<PlanPhase>,
    stamp: AuditStamp,
) -> Result<PlanRevision, RepoError> {
    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    let Some(parent) = mutable_draft(runner, scope, tenant_id, plan_id, revision, expected).await?
    else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    // The tenant every child row carries is the parent
    // revision's, taken off the row just read.
    let rows = phase_models(&parent, &phases)?;
    delete_phases(runner, scope, tenant_id, plan_id, revision).await?;
    insert_phases(runner, scope, rows).await?;
    let result = plan_revision_bump(runner, scope, guard).await?;
    // The read above is not the guard - a concurrent publish can
    // land between it and this statement - so a swap that
    // matched nothing is still resolved, and the phase set it
    // has already replaced is restored by the rollback.
    if result == 0 {
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

/// [`PlanShapeRepo::replace_period_floor_caps`]'s body, on a runner the caller
/// owns (**D-319**).
///
/// **The runner must be a transaction**, for [`replace_phases_on`]'s reason.
///
/// # Errors
/// Whatever the method's own documentation states — this is that method,
/// minus the transaction it opens for itself.
#[allow(
    clippy::too_many_arguments,
    reason = "the siblings' signature verbatim; this form exists so a caller that already owns a transaction can join it rather than open a second (D-272)."
)]
pub async fn replace_period_floor_caps_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
    bounds: Vec<PeriodFloorCap>,
    stamp: AuditStamp,
) -> Result<PlanRevision, RepoError> {
    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    let Some(parent) = mutable_draft(runner, scope, tenant_id, plan_id, revision, expected).await?
    else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    let rows = period_floor_cap_models(&parent, &bounds);
    delete_period_floor_caps(runner, scope, tenant_id, plan_id, revision).await?;
    insert_period_floor_caps(runner, scope, rows).await?;
    let result = plan_revision_bump(runner, scope, guard).await?;
    if result == 0 {
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

/// [`PlanShapeRepo::replace_addon_rules`]'s body, on a runner the caller owns.
///
/// **The runner must be a transaction.** This writes the child rows *and*
/// the revision's D-135 audit record, so on a bare connection they would be
/// separate autocommit statements and a failure part way would leave a
/// committed edit nobody recorded making — the property `create_draft_on`
/// states as the house rule. Every current caller supplies one.
///
/// # Errors
/// Whatever the method's own documentation states — this is that method,
/// minus the transaction it opens for itself.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, revision, expected-version) the compare-and-swap addresses, the payload and the D-135 audit stamp. The method above carries the same set; this form exists so a caller that already owns a transaction can join it rather than open a second (D-272)."
)]
pub async fn replace_addon_rules_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
    rules: Vec<AddonRule>,
    stamp: AuditStamp,
) -> Result<PlanRevision, RepoError> {
    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    let Some(parent) = mutable_draft(runner, scope, tenant_id, plan_id, revision, expected).await?
    else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    // The tenant every child row carries is the parent
    // revision's, taken off the row just read.
    let rows = addon_rule_models(&parent, &rules)?;
    delete_addon_rules(runner, scope, tenant_id, plan_id, revision).await?;
    insert_addon_rules(runner, scope, rows).await?;
    let result = plan_revision_bump(runner, scope, guard).await?;
    // The read above is not the guard - a concurrent publish can
    // land between it and this statement - so a swap that
    // matched nothing is still resolved, and the set it has
    // already replaced is restored by the rollback.
    if result == 0 {
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

/// [`PlanShapeRepo::set_descriptor_set`]'s body, on a runner the caller owns.
///
/// **The runner must be a transaction.** This writes the child rows *and*
/// the revision's D-135 audit record, so on a bare connection they would be
/// separate autocommit statements and a failure part way would leave a
/// committed edit nobody recorded making — the property `create_draft_on`
/// states as the house rule. Every current caller supplies one.
///
/// # Errors
/// Whatever the method's own documentation states — this is that method,
/// minus the transaction it opens for itself.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, revision, expected-version) the compare-and-swap addresses, the payload and the D-135 audit stamp. The method above carries the same set; this form exists so a caller that already owns a transaction can join it rather than open a second (D-272)."
)]
pub async fn set_descriptor_set_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
    descriptors: DescriptorSet,
    stamp: AuditStamp,
) -> Result<PlanRevision, RepoError> {
    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    let Some(parent) = mutable_draft(runner, scope, tenant_id, plan_id, revision, expected).await?
    else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    // The tenant the row carries is the parent revision's, taken
    // off the row just read.
    let row = descriptor_model(&parent, &descriptors);
    delete_descriptor_set(runner, scope, tenant_id, plan_id, revision).await?;
    insert_descriptor_set(runner, scope, row).await?;
    let result = plan_revision_bump(runner, scope, guard).await?;
    // The read above is not the guard - a concurrent publish can
    // land between it and this statement - so a swap that
    // matched nothing is still resolved, and the row it has
    // already replaced is restored by the rollback.
    if result == 0 {
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

/// [`PlanShapeRepo::replace_composites`]'s body, on a runner the caller owns.
///
/// **The runner must be a transaction.** This writes the child rows *and*
/// the revision's D-135 audit record, so on a bare connection they would be
/// separate autocommit statements and a failure part way would leave a
/// committed edit nobody recorded making — the property `create_draft_on`
/// states as the house rule. Every current caller supplies one.
///
/// # Errors
/// Whatever the method's own documentation states — this is that method,
/// minus the transaction it opens for itself.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is a fact only the caller holds: the scope, the (tenant, plan, revision, expected-version) the compare-and-swap addresses, the payload and the D-135 audit stamp. The method above carries the same set; this form exists so a caller that already owns a transaction can join it rather than open a second (D-272)."
)]
pub async fn replace_composites_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
    composites: Vec<CompositeMeter>,
    stamp: AuditStamp,
) -> Result<PlanRevision, RepoError> {
    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    let Some(parent) = mutable_draft(runner, scope, tenant_id, plan_id, revision, expected).await?
    else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    // The tenant every child row carries is the parent
    // revision's, taken off the row just read — the three
    // siblings' rule, and this path is inside it.
    write_composites(runner, scope, &parent, composites).await?;
    let result = plan_revision_bump(runner, scope, guard).await?;
    if result == 0 {
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

#[cfg(test)]
#[path = "plan_shape_repo_tests.rs"]
mod plan_shape_repo_tests;
