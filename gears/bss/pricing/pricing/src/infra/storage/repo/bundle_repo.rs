//! Repository for a bundle and its **composition** — `pricing_bundle` plus the
//! three child tables that version with the plan revision
//! (`design/08-bundles.md` §6, D-92 + D-105).
//!
//! # The composition carries no entity tag of its own, and that is a decision
//!
//! A composition edit advances the **plan revision's** `row_version`, which is
//! [`plan_shape_repo`](super::plan_shape_repo)'s argument for a plan's child sets
//! applied to a bundle's: if editing the component set left the revision's tag
//! alone, two authors editing one draft would both satisfy `If-Match` and
//! silently interleave — the lost update `fr-concurrent-edit` exists to refuse.
//! [`BundleRepo::replace_composition`] therefore takes the revision's
//! [`RowVersion`] and bumps it in the same statement that matches on it, through
//! `plan_shape_repo::plan_revision_bump` rather than a second spelling of the
//! same swap.
//!
//! # The composition is replaced wholesale, never merged
//!
//! Not a convenience. Every Slice-8 rule is a property of the **set** — "every
//! referenced component" (`inst-bc-coverage`), the per-market coverage walk, the
//! frequency comparison across components, "sum to 100% **per** included vendor
//! SKU" — and a partial update leaves no set the rules could evaluate. It is also
//! `PriceRepo`'s band precedent and `PlanShapeRepo`'s phase precedent, for the
//! same reason both give.
//!
//! # The delete precedes the insert, and the read precedes the delete
//!
//! All three child tables refuse INSERT, UPDATE *and* DELETE against a non-draft
//! parent revision, and their refusal is a raw driver error carrying no state.
//! The read is there so a caller aiming at a frozen revision is told which
//! conjunct failed before any statement reaches a child table. It is **not** the
//! guard — the compare-and-swap is — and losing the revision to a concurrent
//! publish between them rolls the whole transaction back, deleted rows and all.
//!
//! `pricing_bundle_component`, `pricing_bundle_revshare_group` and
//! `pricing_bundle_revshare` are all three of them. It is the Slice-8 analogue of
//! [`plan_shape_repo`](super::plan_shape_repo), and it exists for the same
//! reason: a revision-scoped child set owes two things nothing else in this gear
//! supplies — a **copy forward** when a successor revision opens, and a **drop**
//! when a draft is abandoned. Both are called from
//! [`PlanRepo`](super::PlanRepo), because the revision row is what they are
//! ordered against and it is that repository's.
//!
//! # The composition hangs off the plan by one indirection
//!
//! The Slice-2 child tables carry `plan_id` and key directly on the revision.
//! These three key on `bundle_id`, and the plan is reached through
//! `pricing_bundle`. So every entry point here begins by resolving *which
//! bundle, if any, this plan is* — and a plan that is not a bundle is a
//! **no-op**, not an error: the overwhelming majority of plans are not bundles,
//! and `PlanRepo` calls these functions unconditionally.
//!
//! # The order inside each function is forced by the foreign keys
//!
//! A party row references its group, so the copy writes **groups before
//! parties** and the drop deletes **parties before groups**. Getting either
//! backwards is a foreign-key violation and not a silent reordering, which is
//! why the two are written as one function each rather than left to callers.
//!
//! # `effective_share_bp` is deliberately **not** carried forward
//!
//! It is the publish-time normalization of D-07 — what the *previous* revision
//! published — and the successor has not published. Copying it forward would
//! hand a draft an answer it has not computed, and a draft whose typed shares
//! were then edited would carry effective shares reconciling the **old** split.
//! The typed `share_bp` is the authored content and travels; the effective share
//! is minted by the publish that normalizes it.

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::odata::sea_orm_filter::paginate_odata;
use toolkit_db::secure::{
    AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
};
use toolkit_db::{DBProvider, DbError};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use bss_pricing_sdk::odata::BundleFilterField;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::bundle::{
    Absorber, InvoiceItemization, Party, PartyShare, PriceBasis, RevShareGroup,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::plan::PlanRevision;
use crate::domain::scope_key::PlanId;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{
    bundle, bundle_component, bundle_revshare, bundle_revshare_group,
};
use crate::infra::storage::odata_mapping::{
    BundleODataMapper, LIST_LIMIT_CFG, OdataPageError, domain_page, map_odata_err,
    query_with_default_order,
};
use crate::infra::storage::repo::plan_repo::{
    self, load_revision, mutable_draft, not_found, record_revision_mutation, refuse, swap_guard,
    tx_failure,
};
use crate::infra::storage::repo::plan_shape_repo::plan_revision_bump;
use crate::infra::storage::repo::{NewAuditEntry, audit_repo};

/// A revision number as the store holds it.
///
/// [`plan_shape_repo`](super::plan_shape_repo)'s helper of the same name, spelled
/// again rather than imported across module boundaries for one `try_from`.
///
/// **One failure answer for all four callers.** Returning `Option` left each of
/// them to decide, and they decided three different things about the same
/// unstorable revision: two refused, the drop path answered `Ok(())` for a
/// revision it never looked at, and the read answered an empty composition. A
/// caller cannot tell "cleared" from "never examined" in the first, or "no
/// components" from "unreadable" in the second.
/// [`RepoError::ValueOutOfRange`] and **not** [`RepoError::CorruptRow`], which is
/// the line that variant's own doc draws: `load_composition_on`'s revision is
/// parsed straight off `plan_revision` in the query string (`api::rest::bundles`),
/// so an out-of-range one is a caller mistake the request can be reshaped around,
/// not a table written around.
fn stored_revision(bundle_id: Uuid, revision: u64) -> Result<i64, RepoError> {
    i64::try_from(revision).map_err(|_| RepoError::ValueOutOfRange {
        field: format!("bundle {bundle_id} revision"),
        value: revision.to_string(),
    })
}

/// Which bundle this plan is, if it is one.
///
/// `None` is the ordinary answer and never an error: most plans are not bundles.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
async fn bundle_of_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<Uuid>, RepoError> {
    let row = bundle::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle::Column::TenantId.eq(tenant_id))
                .add(bundle::Column::PlanId.eq(plan_id.get())),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_bundle by plan: {e}")))?;
    Ok(row.map(|row| row.bundle_id))
}

/// Copy one revision's whole composition onto the revision `to`.
///
/// Called from [`PlanRepo::open_revision`](super::PlanRepo::open_revision),
/// inside that method's transaction and **after** the destination revision row
/// is inserted: each of these tables refuses an INSERT whose new parent revision
/// is not `draft`, so the ordering is forced by the triggers rather than chosen.
///
/// A plan with no bundle copies nothing.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only triggers' refusal when the destination revision is not a `draft`,
/// and the destination revision not existing at all.
pub(super) async fn copy_composition(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    from: u64,
    to: u64,
) -> Result<(), RepoError> {
    let Some(bundle_id) = bundle_of_plan(runner, scope, tenant_id, plan_id).await? else {
        return Ok(());
    };
    let source = stored_revision(bundle_id, from)?;
    let target = stored_revision(bundle_id, to)?;

    let components = bundle_component::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, source, &ComponentCols))
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read bundle components: {e}")))?;
    if !components.is_empty() {
        let copies: Vec<_> = components
            .into_iter()
            .map(|row| bundle_component::ActiveModel {
                bundle_id: Set(row.bundle_id),
                plan_revision: Set(target),
                component_plan_id: Set(row.component_plan_id),
                tenant_id: Set(row.tenant_id),
                included_sku_id: Set(row.included_sku_id),
                min_qty: Set(row.min_qty),
                max_qty: Set(row.max_qty),
            })
            .collect();
        insert_bundle_component(runner, scope, copies).await?;
    }

    // Groups before parties: a party row references its group.
    let groups = bundle_revshare_group::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, source, &GroupCols))
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read bundle rev-share groups: {e}")))?;
    if !groups.is_empty() {
        let copies: Vec<_> = groups
            .into_iter()
            .map(|row| bundle_revshare_group::ActiveModel {
                bundle_id: Set(row.bundle_id),
                plan_revision: Set(target),
                vendor_sku_id: Set(row.vendor_sku_id),
                tenant_id: Set(row.tenant_id),
                platform_cut_bp: Set(row.platform_cut_bp),
                residual_absorber_party: Set(row.residual_absorber_party),
            })
            .collect();
        insert_bundle_revshare_group(runner, scope, copies).await?;
    }

    let parties = bundle_revshare::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, source, &PartyCols))
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read bundle rev-share parties: {e}")))?;
    if !parties.is_empty() {
        let copies: Vec<_> = parties
            .into_iter()
            .map(|row| bundle_revshare::ActiveModel {
                bundle_id: Set(row.bundle_id),
                plan_revision: Set(target),
                vendor_sku_id: Set(row.vendor_sku_id),
                party: Set(row.party),
                tenant_id: Set(row.tenant_id),
                share_bp: Set(row.share_bp),
                // Minted by the publish that normalizes it; see the module doc.
                effective_share_bp: Set(None),
            })
            .collect();
        insert_bundle_revshare(runner, scope, copies).await?;
    }
    Ok(())
}

/// Drop one revision's whole composition.
///
/// Called from [`PlanRepo::abandon_draft`](super::PlanRepo::abandon_draft)
/// **before** it flips the revision row: `abandoned` is not `draft` and these
/// tables' DELETE triggers refuse everything afterwards. The ordering is forced
/// by the triggers, exactly as the copy's is.
///
/// A plan with no bundle drops nothing.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure — which includes the
/// append-only triggers' refusal when the revision is not a `draft`;
/// [`RepoError::ValueOutOfRange`] when the revision is outside the storable range.
pub(super) async fn delete_composition(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<(), RepoError> {
    let Some(bundle_id) = bundle_of_plan(runner, scope, tenant_id, plan_id).await? else {
        return Ok(());
    };
    let number = stored_revision(bundle_id, revision)?;

    // Parties before groups: the foreign key points that way.
    bundle_revshare::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, number, &PartyCols))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete bundle rev-share parties: {e}")))?;
    bundle_revshare_group::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, number, &GroupCols))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete bundle rev-share groups: {e}")))?;
    bundle_component::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, number, &ComponentCols))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete bundle components: {e}")))?;
    Ok(())
}

struct ComponentCols;
struct GroupCols;
struct PartyCols;

/// *This bundle, this tenant, this revision* — the one predicate all three
/// tables are ranged over by.
///
/// Written once rather than three times: three copies are three places for a
/// forgotten `tenant_id` to hide behind `SecureORM`'s own scoping.
fn revision_of<C>(bundle_id: Uuid, tenant_id: Uuid, revision: i64, cols: &C) -> Condition
where
    C: RevisionColumns,
{
    Condition::all()
        .add(cols.bundle_eq(bundle_id))
        .add(cols.tenant_eq(tenant_id))
        .add(cols.revision_eq(revision))
}

/// The column triple of one composition table.
trait RevisionColumns {
    fn bundle_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr;
    fn tenant_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr;
    fn revision_eq(&self, value: i64) -> sea_orm::sea_query::SimpleExpr;
}

impl RevisionColumns for ComponentCols {
    fn bundle_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr {
        bundle_component::Column::BundleId.eq(value)
    }
    fn tenant_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr {
        bundle_component::Column::TenantId.eq(value)
    }
    fn revision_eq(&self, value: i64) -> sea_orm::sea_query::SimpleExpr {
        bundle_component::Column::PlanRevision.eq(value)
    }
}

impl RevisionColumns for GroupCols {
    fn bundle_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr {
        bundle_revshare_group::Column::BundleId.eq(value)
    }
    fn tenant_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr {
        bundle_revshare_group::Column::TenantId.eq(value)
    }
    fn revision_eq(&self, value: i64) -> sea_orm::sea_query::SimpleExpr {
        bundle_revshare_group::Column::PlanRevision.eq(value)
    }
}

impl RevisionColumns for PartyCols {
    fn bundle_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr {
        bundle_revshare::Column::BundleId.eq(value)
    }
    fn tenant_eq(&self, value: Uuid) -> sea_orm::sea_query::SimpleExpr {
        bundle_revshare::Column::TenantId.eq(value)
    }
    fn revision_eq(&self, value: i64) -> sea_orm::sea_query::SimpleExpr {
        bundle_revshare::Column::PlanRevision.eq(value)
    }
}

// ---------------------------------------------------------------------------
// Writers - one row at a time, under `scope_with_model`.
// ---------------------------------------------------------------------------
//
// `insert_many` cannot carry a per-row scope check, and the check is the point:
// `scope_with_model` validates the tenant of the `ActiveModel` it is given, which
// is the second half of the rule the module doc states — the value is copied from
// the parent bundle, and then checked against the caller's scope. This is
// `plan_shape_repo::insert_addon_rules`' shape, for its reason.

async fn insert_bundle_component(
    runner: &impl DBRunner,
    scope: &AccessScope,
    rows: Vec<bundle_component::ActiveModel>,
) -> Result<(), RepoError> {
    for row in rows {
        bundle_component::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .map_err(|e| RepoError::Db(format!("pricing_bundle_component scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_bundle_component: {e}")))?;
    }
    Ok(())
}

async fn insert_bundle_revshare_group(
    runner: &impl DBRunner,
    scope: &AccessScope,
    rows: Vec<bundle_revshare_group::ActiveModel>,
) -> Result<(), RepoError> {
    for row in rows {
        bundle_revshare_group::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .map_err(|e| RepoError::Db(format!("pricing_bundle_revshare_group scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_bundle_revshare_group: {e}")))?;
    }
    Ok(())
}

async fn insert_bundle_revshare(
    runner: &impl DBRunner,
    scope: &AccessScope,
    rows: Vec<bundle_revshare::ActiveModel>,
) -> Result<(), RepoError> {
    for row in rows {
        bundle_revshare::Entity::insert(row.clone())
            .secure()
            .scope_with_model(scope, &row)
            .map_err(|e| RepoError::Db(format!("pricing_bundle_revshare scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_bundle_revshare: {e}")))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The authoring surface.
// ---------------------------------------------------------------------------

/// A bundle to create on a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBundle {
    /// The bundle's own identity, minted by the caller.
    pub bundle_id: Uuid,
    /// The tenant, checked against the caller's scope on the way in.
    pub tenant_id: Uuid,
    /// The `bundle`-type plan whose revisions the composition rides.
    pub plan_id: PlanId,
    /// `inst-bb-declared`'s declared basis.
    pub price_basis: PriceBasis,
    /// B3's invoice layout.
    pub invoice_itemization: InvoiceItemization,
}

/// One authored component of a composition.
///
/// No `bundle_id`, no `plan_revision` and no `tenant_id`: all three are the
/// repository's to fill from the parent it resolved, never the request's
/// (Global Constraint 9). That is why this type cannot carry them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleComponentDraft {
    /// The component's plan (B1).
    pub component_plan_id: Uuid,
    /// The registry SKU it publishes under.
    pub included_sku_id: Uuid,
    /// Selection-time lower bound.
    pub min_qty: Option<i32>,
    /// Selection-time upper bound.
    pub max_qty: Option<i32>,
}

/// A whole composition, as an author submits it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompositionDraft {
    /// The referenced components.
    pub components: Vec<BundleComponentDraft>,
    /// The rev-share groups, one per included vendor SKU.
    pub rev_share_groups: Vec<RevShareGroup>,
}

/// A bundle as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleRecord {
    /// The bundle's identity.
    pub bundle_id: Uuid,
    /// The plan it rides.
    pub plan_id: PlanId,
    /// Its declared basis.
    pub price_basis: PriceBasis,
    /// Its invoice layout.
    pub invoice_itemization: InvoiceItemization,
}

/// Repository over `pricing_bundle` and its three composition children.
#[derive(Clone)]
pub struct BundleRepo {
    db: DBProvider<DbError>,
}

impl BundleRepo {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Create the bundle row on its plan.
    ///
    /// The row is the bundle's **identity** and is not revision-scoped, so this
    /// is not a composition edit and does not move the revision's tag. The two
    /// content columns it carries are the subject of owed-register entry B-1 and
    /// of `pricing_bundle`'s migration doc.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when `plan_id` names no plan the caller can read —
    /// a foreign one and an absent one alike, which is the point;
    /// [`RepoError::DuplicateBundleOnPlan`] when the plan already carries one
    /// (`uq_pricing_bundle_plan`), surfaced as a typed refusal rather than as a
    /// driver error; [`RepoError::Db`] on a scope or storage failure.
    pub async fn create(
        &self,
        scope: &AccessScope,
        new: NewBundle,
        stamp: AuditStamp,
    ) -> Result<Uuid, RepoError> {
        // A transaction, not `conn()`: `create_on` writes two rows — the bundle
        // row and its D-135 audit append — and a bare connection makes those two
        // autocommit statements, so a failed append left a committed bundle with
        // no record of its creation. Every sibling create in this layer
        // (`PlanRepo::create_draft`, `OverlayRepo::create`, `PriceRepo::create_draft`)
        // opens a transaction for the identical shape.
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Uuid, RepoError, _>(move |txn| {
                Box::pin(async move { create_on(txn, &scope, new, stamp).await })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// The bundle on this plan, if it is one.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] for a stored basis or layout token no `CHECK`
    /// should have admitted.
    pub async fn find_by_plan(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
    ) -> Result<Option<BundleRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_bundle conn: {e}")))?;
        find_by_plan_on(&conn, scope, tenant_id, plan_id).await
    }

    /// One page of the tenant's bundles, in `bundle_id` order.
    ///
    /// # Errors
    /// Exactly [`list_page`]'s.
    pub async fn list(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: Option<PlanId>,
        after: Option<Uuid>,
        limit: u64,
    ) -> Result<Vec<BundleRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_bundle conn: {e}")))?;
        list_page(&conn, scope, tenant_id, plan_id, after, limit).await
    }

    /// One `OData` page of the tenant's bundles. Default order is `bundle_id asc`.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
    /// malformed `$filter` / `$orderby` / cursor.
    pub async fn list_odata(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<BundleRecord>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("pricing_bundle conn: {e}")))?;
        list_odata(&conn, scope, tenant_id, query).await
    }

    /// The plan a bundle rides, by the bundle's own id.
    ///
    /// The composition routes address a **bundle** and every guard downstream
    /// addresses a **plan revision**, so this is the one hop between them. It is
    /// a read the handler makes rather than a column the routes carry, because a
    /// caller supplying both would be supplying a pair the store can already
    /// disagree with.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn plan_of(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        bundle_id: Uuid,
    ) -> Result<Option<PlanId>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_bundle conn: {e}")))?;
        let row = bundle::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(bundle::Column::TenantId.eq(tenant_id))
                    .add(bundle::Column::BundleId.eq(bundle_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_bundle by id: {e}")))?;
        Ok(row.map(|row| PlanId::new(row.plan_id)))
    }

    /// Replace an open draft revision's whole composition, under the caller's
    /// row version, in one transaction.
    ///
    /// The order is: resolve the revision, drop its composition, insert the
    /// submitted set, then compare-and-swap `row_version + 1` under `expected`.
    /// The module doc says why each step sits where it does.
    ///
    /// # Errors
    /// [`RepoError::StaleRowVersion`] / [`RepoError::NotDraft`] /
    /// [`RepoError::NotFound`] as [`refuse`] resolves them;
    /// [`RepoError::NotFound`] when the plan carries no bundle at all;
    /// [`RepoError::Db`] on a scope or storage failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "the compare-and-swap needs the whole coordinate — scope, tenant, \
                  plan, revision and the version read — beside the payload and the \
                  audit stamp; bundling them into a struct would put five values \
                  that are never carried together anywhere else into a type that \
                  exists only to satisfy a count. `PlanShapeRepo`'s three writers \
                  take the same eight for the same reason."
    )]
    pub async fn replace_composition(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
        expected: RowVersion,
        draft: CompositionDraft,
        stamp: AuditStamp,
    ) -> Result<PlanRevision, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PlanRevision, RepoError, _>(move |txn| {
                Box::pin(async move {
                    replace_composition_on(
                        txn, &scope, tenant_id, plan_id, revision, expected, draft, stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// One revision's whole composition, in a stable order.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] for a stored absorber or party value no `CHECK`
    /// should have admitted.
    pub async fn load_composition(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        revision: u64,
    ) -> Result<CompositionDraft, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("pricing_bundle conn: {e}")))?;
        load_composition_on(&conn, scope, tenant_id, plan_id, revision).await
    }
}

/// [`BundleRepo::find_by_plan`]'s read on a runner the caller owns.
///
/// # Errors
/// Exactly [`BundleRepo::find_by_plan`]'s.
pub async fn find_by_plan_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<BundleRecord>, RepoError> {
    let Some(row) = bundle_row_of_plan(runner, scope, tenant_id, plan_id).await? else {
        return Ok(None);
    };
    record_of(&row).map(Some)
}

/// One page of the tenant's bundles, in `bundle_id` order.
///
/// D-125's keyset walk over the bundle plane — `crate::api::rest::cursor`'s
/// module doc argues once, for every list surface, why the resume is a key and
/// never an offset.
///
/// # Why the narrowing parameter is `plan_id`
///
/// Because it is the only thing on the row that narrows anything. `pricing_bundle`
/// carries the bundle's **identity** — its id, its tenant, the plan it rides, its
/// price basis and its invoice layout — and no lifecycle column: the composition
/// is revision-scoped and lives in the three child tables, so a bundle's state is
/// its plan revision's state and is not readable here. `plan_id` is unique across
/// the table (`uq_pricing_bundle_plan`), which makes the filtered page the answer
/// to *"does this plan carry a bundle, and what is its id"* — the hop
/// [`BundleRepo::plan_of`] makes in the other direction.
///
/// What a page does **not** carry is the composition: a bundle's components and
/// revenue shares are three further queries per bundle, so a caller opens
/// `GET /bss-pricing/v1/bundles/{bundleId}` for one bundle's composition.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] for
/// a stored basis or layout token no `CHECK` should have admitted.
pub async fn list_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: Option<PlanId>,
    after: Option<Uuid>,
    limit: u64,
) -> Result<Vec<BundleRecord>, RepoError> {
    let mut filter = Condition::all().add(bundle::Column::TenantId.eq(tenant_id));
    if let Some(plan_id) = plan_id {
        filter = filter.add(bundle::Column::PlanId.eq(plan_id.get()));
    }
    if let Some(after) = after {
        filter = filter.add(bundle::Column::BundleId.gt(after));
    }
    bundle::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(bundle::Column::BundleId, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pricing_bundle: {e}")))?
        .iter()
        .map(record_of)
        .collect()
}

/// One `OData` page of the tenant's bundles. Default order is `bundle_id asc`.
///
/// # Errors
/// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
/// malformed `$filter` / `$orderby` / cursor.
pub async fn list_odata(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    query: &ODataQuery,
) -> Result<Page<BundleRecord>, OdataPageError> {
    let base_select = bundle::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(bundle::Column::TenantId.eq(tenant_id)));
    let query = query_with_default_order(query, &[BundleFilterField::BundleId]);
    let page = paginate_odata::<
        BundleFilterField,
        BundleODataMapper,
        bundle::Entity,
        bundle::Model,
        _,
        _,
    >(
        base_select,
        runner,
        &query,
        ("bundle_id", SortDir::Asc),
        LIST_LIMIT_CFG,
        |m| m,
    )
    .await
    .map_err(map_odata_err)?;
    domain_page(page, |m| record_of(&m))
}

/// [`BundleRepo::load_composition`]'s read on a runner the caller owns.
///
/// Runner-taking so that a caller copying a composition reads the source through
/// the same transaction it writes the copy in (D-274) — otherwise the read is of
/// a state the write is not protected against changing.
///
/// # Errors
/// Exactly [`BundleRepo::load_composition`]'s.
pub async fn load_composition_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<CompositionDraft, RepoError> {
    let Some(bundle_id) = bundle_of_plan(runner, scope, tenant_id, plan_id).await? else {
        return Ok(CompositionDraft::default());
    };
    let number = stored_revision(bundle_id, revision)?;
    read_composition(runner, scope, bundle_id, tenant_id, number).await
}

/// [`BundleRepo::create`]'s body on a runner the caller owns.
///
/// The method opens its own transaction; this form runs on the caller's, so a
/// creator composing the bundle identity with the composition that follows it
/// gets both or neither (D-272, D-274).
///
/// # The record is filed against the bundle, under the plan's segment
///
/// [`super::audit_repo::bundle_ref`] carries the whole argument: the subject-kind
/// roster has no `bundle` member and D-158 binds it to `pricing_approval`'s, so
/// the bundle's own acts ride the plan they are a composition of — which is
/// already how [`replace_composition_on`] files an edit to what the bundle
/// contains. Without a record here an auditor could see every change to a
/// composition and nothing about the bundle coming into existence.
///
/// # Errors
/// Exactly [`BundleRepo::create`]'s.
#[tracing::instrument(
    skip_all,
    fields(tenant_id = %new.tenant_id, bundle_id = %new.bundle_id, plan_id = %new.plan_id)
)]
pub async fn create_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    new: NewBundle,
    stamp: AuditStamp,
) -> Result<Uuid, RepoError> {
    // **The plan is resolved in the caller's scope before anything else.**
    //
    // `plan_id` arrives in the request body of `POST /bss-pricing/v1/bundles` and
    // `pricing_bundle` holds no foreign key onto `pricing_plan` — one is not
    // expressible, for the reason `pricing_bundle` gives — so until this read
    // existed **nothing between the wire and the insert looked at the plan at
    // all**. The only read here was the one below, which looks for *a bundle* and
    // not for the plan, and it is tenant-scoped: so a caller naming another
    // tenant's `plan_id` was told `DuplicateBundleOnPlan` when that tenant had a
    // bundle and `201 Created` when it did not, the whole discrimination coming
    // from `uq_pricing_bundle_plan`, which carried no tenant. That is an existence
    // oracle over another tenant's plans, and in the `201` case the row *landed* —
    // occupying a slot its owner could then never claim, against a row invisible
    // to it, with no `DELETE` anywhere in the API.
    //
    // `NotFound` and not a forbidden: `RepoError::NotFound`'s own doc argues the
    // fold, so a foreign plan is indistinguishable from an absent one and the 404
    // confirms nothing. `pricing_bundle` widens the index in the same change;
    // this half closes the oracle and that half stops the slot being takeable.
    if !plan_repo::holds_a_revision(runner, scope, new.tenant_id, new.plan_id).await? {
        return Err(RepoError::NotFound {
            subject: "plan".to_owned(),
            id: new.plan_id.get().to_string(),
        });
    }
    // The read is the explanatory path and the index is the guarantee — the
    // read-then-index arrangement D-148 describes, here for a uniqueness the
    // caller can be told about in its own words.
    if bundle_of_plan(runner, scope, new.tenant_id, new.plan_id)
        .await?
        .is_some()
    {
        return Err(RepoError::DuplicateBundleOnPlan {
            plan_id: new.plan_id.get().to_string(),
        });
    }
    let row = bundle::ActiveModel {
        bundle_id: Set(new.bundle_id),
        tenant_id: Set(new.tenant_id),
        plan_id: Set(new.plan_id.get()),
        price_basis: Set(new.price_basis.as_str().to_owned()),
        invoice_itemization: Set(new.invoice_itemization.as_str().to_owned()),
    };
    bundle::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(|e| RepoError::Db(format!("pricing_bundle scope: {e}")))?
        .exec(runner)
        .await
        // The index is the guarantee; a racing creator loses here rather
        // than on the read above.
        .map_err(|e| duplicate_bundle_or_db(&e, new.plan_id, "insert pricing_bundle"))?;

    // Inside the caller's transaction, after the insert: D-135 puts the record in
    // the mutation's own transaction, so a rolled-back create leaves no record of
    // having happened. `before_state` is `None` because there was nothing.
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id: new.tenant_id,
            chain_id: audit_repo::plan_chain(new.plan_id),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action: AuditAction::Create,
            subject_kind: AuditSubjectKind::PlanRevision,
            subject_ref: audit_repo::bundle_ref(new.bundle_id),
            before_state: None,
            after_state: Some(serde_json::json!({
                "planId": new.plan_id.get().to_string(),
                "priceBasis": new.price_basis.as_str(),
                "invoiceItemization": new.invoice_itemization.as_str(),
            })),
            approval_ref: None,
            correlation_id: stamp.correlation_id,
        },
    )
    .await?;

    Ok(new.bundle_id)
}

/// The one-bundle-per-plan refusal, or an ordinary storage failure.
///
/// **The typed class first, the message only to say *which* constraint.** This
/// read the driver's text alone, with no `is_unique_violation()` conjunct, so any
/// failure whose message happened to carry the DDL name — a lock report naming the
/// index, a planner error quoting it — was answered as
/// [`RepoError::DuplicateBundleOnPlan`]: a refusal that tells the caller their plan
/// already has a bundle, for a fault that had nothing to do with one, and whose
/// remedy (go and edit the bundle you have) then leads nowhere.
///
/// The shape being matched is [`crate::infra::storage::policy_guard_or_contention`],
/// whose doc carries the argument for why message matching is the narrow case and
/// not a precedent, and the two overlay classifiers corrected for
/// this finding's other two sites. The fallback runs the safe way: an unrecognised
/// message is [`RepoError::Db`], which is today's answer for everything this does
/// not name.
fn duplicate_bundle_or_db(err: &ScopeError, plan_id: PlanId, context: &str) -> RepoError {
    if err.is_unique_violation() && names_the_bundle_plan_slot(&err.to_string()) {
        return RepoError::DuplicateBundleOnPlan {
            plan_id: plan_id.get().to_string(),
        };
    }
    RepoError::Db(format!("{context}: {err}"))
}

/// Does this driver message name `uq_pricing_bundle_plan` rather than any other
/// constraint on the bundle table?
///
/// Two renderings, one per backend: Postgres names the **index**, `SQLite` names
/// the indexed **columns**. Neither engine produces the other's spelling, so both
/// arms are live. The qualified form `pricing_bundle.plan_id` is matched rather
/// than the bare column, so `pricing_bundle_component.component_plan_id` cannot be
/// mistaken for it — a bare `plan_id` substring matches that column too, and it is
/// on the one child table a bundle write touches in the same statement batch.
///
/// **None of the three tables carries a `plan_id`**, so the qualification cannot
/// be justified by one: `pricing_bundle_component` is `(bundle_id, plan_revision,
/// component_plan_id, tenant_id, included_sku_id, min_qty, max_qty)` on both
/// engines, and neither `pricing_bundle_revshare` nor
/// `pricing_bundle_revshare_group` has the column either. The qualification is
/// necessary and the matcher is unchanged; a reason that names a column nothing
/// has is the kind of false premise that gets it deleted as unnecessary.
///
/// The index is over `(tenant_id, plan_id)` and the `SQLite` rendering is
/// `pricing_bundle.tenant_id, pricing_bundle.plan_id`, which **contains** the
/// fragment below — so this matcher holds by construction rather than by luck. It is
/// the Postgres arm that carries the index name, and that name is deliberately stable
/// across a widening of the columns under it.
///
/// The other unique constraint on this table is its primary key on `bundle_id`
/// (`pricing_bundle`, and no later migration rebuilds the table), which is why
/// "a unique index refused" cannot on its own mean the plan slot.
///
/// Split out and taking a `&str` so both renderings are asserted directly, without
/// staging a database race for either — `names_the_policy_guard`'s arrangement, for
/// the same reason.
fn names_the_bundle_plan_slot(message: &str) -> bool {
    message.contains("uq_pricing_bundle_plan") || message.contains("pricing_bundle.plan_id")
}

/// [`BundleRepo::replace_composition`]'s body on a runner the caller owns.
///
/// **The runner must be a transaction.** This writes the child rows *and*
/// the revision's D-135 audit record, so on a bare connection they would be
/// separate autocommit statements and a failure part way would leave a
/// committed edit nobody recorded making — the property `create_draft_on`
/// states as the house rule. Every current caller supplies one.
///
/// The method opens its own transaction; this form joins one already open, which
/// is what a caller composing several writers into a single atomic act needs
/// (D-272, D-274). The two are the same statements in the same order — the method
/// is a delegation, so there is no second copy to drift.
///
/// # Errors
/// Exactly [`BundleRepo::replace_composition`]'s, less the transaction-driver
/// failure the caller now owns.
#[allow(
    clippy::too_many_arguments,
    reason = "the compare-and-swap needs the whole coordinate beside the payload \
              and the stamp; this is the method's own list, and a struct invented \
              to shorten it would exist only to satisfy a count (D-274)"
)]
#[tracing::instrument(skip_all, fields(tenant_id = %tenant_id, plan_id = %plan_id, revision))]
pub async fn replace_composition_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    expected: RowVersion,
    draft: CompositionDraft,
    stamp: AuditStamp,
) -> Result<PlanRevision, RepoError> {
    let Some(guard) = swap_guard(tenant_id, plan_id, revision, expected) else {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    };
    if mutable_draft(runner, scope, tenant_id, plan_id, revision, expected)
        .await?
        .is_none()
    {
        return Err(refuse(runner, scope, tenant_id, plan_id, revision, expected).await);
    }
    let Some(bundle_id) = bundle_of_plan(runner, scope, tenant_id, plan_id).await? else {
        return Err(RepoError::NotFound {
            subject: "bundle".to_owned(),
            id: plan_id.get().to_string(),
        });
    };
    let number = stored_revision(bundle_id, revision)?;

    drop_composition(runner, scope, bundle_id, tenant_id, number).await?;
    write_composition(runner, scope, bundle_id, tenant_id, number, &draft).await?;

    let moved = plan_revision_bump(runner, scope, guard).await?;
    // The read above is not the guard — a concurrent publish can
    // land between it and this statement — so a swap that matched
    // nothing is still resolved, and the composition it has
    // already replaced is restored by the rollback.
    if moved == 0 {
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

// ---------------------------------------------------------------------------
// Statements.
// ---------------------------------------------------------------------------

async fn bundle_row_of_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Option<bundle::Model>, RepoError> {
    bundle::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(bundle::Column::TenantId.eq(tenant_id))
                .add(bundle::Column::PlanId.eq(plan_id.get())),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_bundle by plan: {e}")))
}

/// A stored bundle row as the domain reads it.
///
/// The two token columns are parsed rather than carried as strings, so a value
/// no `CHECK` should have admitted is a [`RepoError::CorruptRow`] here instead of
/// an unmatched arm three layers up.
fn record_of(row: &bundle::Model) -> Result<BundleRecord, RepoError> {
    let price_basis = PriceBasis::parse(&row.price_basis).ok_or_else(|| {
        RepoError::CorruptRow(format!(
            "pricing_bundle {} carries an unknown price_basis `{}`",
            row.bundle_id, row.price_basis
        ))
    })?;
    let invoice_itemization =
        InvoiceItemization::parse(&row.invoice_itemization).ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_bundle {} carries an unknown invoice_itemization `{}`",
                row.bundle_id, row.invoice_itemization
            ))
        })?;
    Ok(BundleRecord {
        bundle_id: row.bundle_id,
        plan_id: PlanId::new(row.plan_id),
        price_basis,
        invoice_itemization,
    })
}

/// Drop one revision's composition — parties before groups, per the foreign key.
async fn drop_composition(
    runner: &impl DBRunner,
    scope: &AccessScope,
    bundle_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
) -> Result<(), RepoError> {
    bundle_revshare::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, revision, &PartyCols))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("clear bundle rev-share parties: {e}")))?;
    bundle_revshare_group::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, revision, &GroupCols))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("clear bundle rev-share groups: {e}")))?;
    bundle_component::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, revision, &ComponentCols))
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("clear bundle components: {e}")))?;
    Ok(())
}

/// Write one revision's composition — groups before parties, per the foreign key.
async fn write_composition(
    runner: &impl DBRunner,
    scope: &AccessScope,
    bundle_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
    draft: &CompositionDraft,
) -> Result<(), RepoError> {
    let components: Vec<_> = draft
        .components
        .iter()
        .map(|c| bundle_component::ActiveModel {
            bundle_id: Set(bundle_id),
            plan_revision: Set(revision),
            component_plan_id: Set(c.component_plan_id),
            tenant_id: Set(tenant_id),
            included_sku_id: Set(c.included_sku_id),
            min_qty: Set(c.min_qty),
            max_qty: Set(c.max_qty),
        })
        .collect();
    insert_bundle_component(runner, scope, components).await?;

    let groups: Vec<_> = draft
        .rev_share_groups
        .iter()
        .map(|g| bundle_revshare_group::ActiveModel {
            bundle_id: Set(bundle_id),
            plan_revision: Set(revision),
            vendor_sku_id: Set(g.vendor_sku_id),
            tenant_id: Set(tenant_id),
            platform_cut_bp: Set(g.platform_cut_bp),
            residual_absorber_party: Set(g.residual_absorber.as_str().to_owned()),
        })
        .collect();
    insert_bundle_revshare_group(runner, scope, groups).await?;

    let parties: Vec<_> = draft
        .rev_share_groups
        .iter()
        .flat_map(|g| {
            g.parties.iter().map(move |p| bundle_revshare::ActiveModel {
                bundle_id: Set(bundle_id),
                plan_revision: Set(revision),
                vendor_sku_id: Set(g.vendor_sku_id),
                party: Set(p.party.get().to_owned()),
                tenant_id: Set(tenant_id),
                share_bp: Set(p.share_bp),
                // Minted by the publish that normalizes it (D-07).
                effective_share_bp: Set(None),
            })
        })
        .collect();
    insert_bundle_revshare(runner, scope, parties).await
}

/// Read one revision's composition back into the domain's shape.
async fn read_composition(
    runner: &impl DBRunner,
    scope: &AccessScope,
    bundle_id: Uuid,
    tenant_id: Uuid,
    revision: i64,
) -> Result<CompositionDraft, RepoError> {
    let components = bundle_component::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, revision, &ComponentCols))
        .order_by(bundle_component::Column::ComponentPlanId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read bundle components: {e}")))?
        .into_iter()
        .map(|row| BundleComponentDraft {
            component_plan_id: row.component_plan_id,
            included_sku_id: row.included_sku_id,
            min_qty: row.min_qty,
            max_qty: row.max_qty,
        })
        .collect();

    let groups = bundle_revshare_group::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, revision, &GroupCols))
        .order_by(bundle_revshare_group::Column::VendorSkuId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read bundle rev-share groups: {e}")))?;
    let parties = bundle_revshare::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(revision_of(bundle_id, tenant_id, revision, &PartyCols))
        .order_by(bundle_revshare::Column::VendorSkuId, Order::Asc)
        .order_by(bundle_revshare::Column::Party, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read bundle rev-share parties: {e}")))?;

    let mut rev_share_groups = Vec::with_capacity(groups.len());
    for group in groups {
        let absorber = Absorber::parse(&group.residual_absorber_party).ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "bundle {bundle_id} vendor {} carries an unusable absorber `{}`",
                group.vendor_sku_id, group.residual_absorber_party
            ))
        })?;
        let mut members = Vec::new();
        for party in parties
            .iter()
            .filter(|p| p.vendor_sku_id == group.vendor_sku_id)
        {
            let named = Party::new(&party.party).ok_or_else(|| {
                RepoError::CorruptRow(format!(
                    "bundle {bundle_id} carries an unusable party `{}`",
                    party.party
                ))
            })?;
            members.push(PartyShare {
                party: named,
                share_bp: party.share_bp,
            });
        }
        rev_share_groups.push(RevShareGroup {
            vendor_sku_id: group.vendor_sku_id,
            platform_cut_bp: group.platform_cut_bp,
            residual_absorber: absorber,
            parties: members,
        });
    }

    Ok(CompositionDraft {
        components,
        rev_share_groups,
    })
}

#[cfg(test)]
#[path = "bundle_repo_tests.rs"]
mod bundle_repo_tests;
