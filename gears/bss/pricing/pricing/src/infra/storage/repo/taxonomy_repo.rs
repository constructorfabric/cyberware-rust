//! The four tenant taxonomies' read and write surface —
//! `design/04-currency-tax.md` §5's `GET/PUT /config/taxonomies/{…}`, §6's table,
//! and `inst-tx-mutation`'s retire guard.
//!
//! # This is the repository the four tables have been waiting for
//!
//! The tables landed on Slice 9's chain (`pricing_region_taxonomy`…`000031`) because
//! `inst-plv-scope` had to validate an overlay's scope value against *something*
//! and the alternative was shipping a rule against thin air — the D-211 defect
//! named in their own migration docs. What they got was one read,
//! [`overlay_repo::taxonomy_declares`](super::overlay_repo::OverlayRepo::taxonomy_declares),
//! and **no writer at all**. An operator could not author a brand-scoped overlay
//! end to end, because there was nowhere to put the brand.
//!
//! So the read here is deliberately *not* a second copy of that one. This module
//! owns the **set** reads an operator and the publish pipeline need — the whole
//! declared list, the active region universe, and D-01's readiness markers —
//! while the single-value membership probe stays where Slice 9 put it. Two
//! functions answering "is this value declared" would be two predicates to keep
//! in step, and the one that drifted would be the one nobody was looking at.
//!
//! # The region universe is never empty (D-354)
//!
//! A tenant that holds no region row reads `{global: active}` from every region
//! reader here — the list, the active universe, the two readiness reads — and
//! nothing is written on a read. The tenant's first region write, whatever door
//! it comes through, calls `materialise_region_seed` first, inside its own
//! transaction, so the row it goes on to touch is the one the reads answered.
//! "No row" is the predicate, not "no active row": a tenant that retired every
//! region holds rows and has the empty universe it asked for.
//!
//! # The `PUT` is the whole set, and absence is retirement rather than deletion
//!
//! §5 gives this resource an `ETag` and §6 gives its rows a `state` with a
//! guarded transition and a legal way back. Both point the same way: the resource
//! is *the tenant's taxonomy*, not a row, so the `PUT` carries the complete value
//! set exactly as the approval-threshold policy's does — *"the **whole** policy,
//! not a patch"*.
//!
//! What absence means is the one real decision, and it is **retire, never
//! delete**. Deleting would break the guard's whole purpose: a value a published
//! row names has to keep existing, because the row keeps naming it. Retiring is
//! the state §6 declares for exactly this, it is reversible (*"a `PUT` re-adding
//! an existing retired value re-activates it"*), and it is what makes the
//! resource a faithful representation — a value the operator left out of the
//! body is a value they are done with, and the response says so rather than
//! silently keeping it active.
//!
//! **A retirement the guard refuses fails the whole `PUT`.** One transaction, one
//! verdict: a partial application would leave the tenant holding a taxonomy that
//! is neither what they sent nor what they had, and the `ETag` they read it back
//! under would describe neither.
//!
//! # What counts as a reference, and the reading that is deliberate
//!
//! §3 step 3 refuses a retirement while the value is referenced by *"an active
//! published price row (`region`) **or** an active `PriceOverlay` scope of any
//! taxonomy-backed class"*. Both counts are over `lifecycle_state = 'published'`.
//!
//! `superseded` is deliberately **not** counted, and the argument is that
//! retiring a taxonomy value does not touch the rows that name it. A superseded
//! row keeps its `region` string, keeps its window, and keeps resolving for the
//! arrears periods `PROJECTED_ROW_STATES` exists to preserve; what retirement
//! withdraws is the value's ability to validate something *new*. Counting
//! history would refuse the retirement of every region a tenant ever
//! repriced — which is to say, of every region they actually use.
//!
//! `draft` is not counted for the mirror reason: a draft is not yet a
//! commitment, and a value blocked by somebody's unpublished experiment is a
//! value no operator can retire on a schedule.

use std::collections::{BTreeMap, BTreeSet};

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, QuerySelect, Set};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use crate::domain::concurrency::{PolicyTag, TaxonomyTagEntry};
use crate::domain::lifecycle::LifecycleState;
use crate::domain::overlay::{OverlayLifecycle, ScopeClass, ScopeValue};
use crate::domain::scope_key::Region;
use crate::domain::taxonomy::{
    RegionTaxMarkers, SEEDED_REGION, TAXONOMY_VALUE_IN_USE, TaxonomyClass, TaxonomyEntry,
    TaxonomyState, ValueReferences, check_retirable, check_tax_category_removable, seeded_region,
    tag_of,
};
use crate::domain::validation::ValidationReport;
use crate::infra::storage::entity::{
    brand_taxonomy, customer_group_taxonomy, gl_code_taxonomy, group_membership, org_tier_taxonomy,
    partner_taxonomy, plan, plan_descriptor_set, policy_object, price, price_overlay,
    region_taxonomy, rounding_policy_taxonomy,
};
use crate::infra::storage::{RepoError, contention_or_db};

use super::audit_repo::{self, NewAuditEntry};

/// Bind-list ceiling for an `is_in` predicate, as `price_repo` uses it: an
/// oversized list is a driver parameter-limit error the caller reads as a 500,
/// and the declared-value set this file chunks has no request-side bound.
const MAX_IN_BINDS: usize = 8192;

// ---------------------------------------------------------------------------
// The repository.
// ---------------------------------------------------------------------------

/// Reader and writer of the four tenant taxonomies.
#[derive(Clone)]
pub struct TaxonomyRepo {
    db: DBProvider<DbError>,
}

impl TaxonomyRepo {
    /// Build over one database provider.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Every declared value of one class, `active` and `retired` alike, ordered
    /// by value.
    ///
    /// **Retired values are in the list**, which is what makes the `PUT`'s
    /// round trip honest: an operator who reads, edits and writes back must be
    /// able to see the value they are about to re-activate, and a `GET` hiding
    /// retirements would make re-activation reachable only by guessing a string.
    ///
    /// The order is the repository's rather than the caller's so two reads of an
    /// unchanged taxonomy render one `ETag`; `BTreeMap`'s ordering is the same
    /// one, which is what keeps the digest stable.
    ///
    /// SQL-level BOLA: a foreign tenant's taxonomy reads empty.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored `state` or `value` is outside
    /// what its `CHECK` admits.
    pub async fn list(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        class: TaxonomyClass,
    ) -> Result<Vec<TaxonomyEntry>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("taxonomy conn: {e}")))?;
        list_on(&conn, scope, tenant_id, class).await
    }

    /// Replace one class's whole value set (§5's `PUT`).
    ///
    /// Returns the taxonomy as it now stands, so the caller holds the
    /// representation its `ETag` covers without a second read.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] on an unreadable stored row. A refused
    /// retirement is **not** an error here — it comes back as a non-empty
    /// [`ValidationReport`] beside the unchanged taxonomy, because §5 types it
    /// `TAXONOMY_VALUE_IN_USE` (409) and a caller has to be able to render the
    /// code and the counts rather than a storage failure.
    pub async fn replace(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        class: TaxonomyClass,
        entries: Vec<TaxonomyEntry>,
        asserted: &PolicyTag,
        stamp: AuditStamp,
    ) -> Result<Replaced, RepoError> {
        let scope = scope.clone();
        let asserted = asserted.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Replaced, RepoError, _>(move |txn| {
                Box::pin(async move {
                    apply_replace(txn, &scope, tenant_id, class, entries, &asserted, stamp).await
                })
            })
            .await;
        // `into_domain`, never a blanket `Db`: `in_transaction` answers
        // `TxError<RepoError>`, and flattening it turned a retriable
        // `ConcurrentMutation` into `Internal` — a **500** for a request whose
        // whole remedy is to retry. Every other repository here unwraps it this
        // way for exactly that reason.
        outcome
            .map_err(|e| e.into_domain(|infra| RepoError::Db(format!("taxonomy replace: {infra}"))))
    }

    /// One declared value of one class, `active` or `retired`; `None` when the
    /// tenant has never declared it.
    ///
    /// Read through [`list_on`] rather than by a keyed select, so the row is
    /// decoded by the one reader that owns the `CHECK`-to-enum translation; the
    /// sets are tens of values and the read is on the config plane.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] on an unreadable stored row.
    pub async fn find_value(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        class: TaxonomyClass,
        value: &ScopeValue,
    ) -> Result<Option<TaxonomyEntry>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("taxonomy conn: {e}")))?;
        find_value_on(&conn, scope, tenant_id, class, value).await
    }

    /// Declare **one** value (§5's `POST …/values`).
    ///
    /// The value is the resource's natural key, so the create is idempotent on
    /// it: a body identical to what is held is a **replay** and writes nothing;
    /// a body naming a held value with other content, or a retired one, is
    /// refused as [`Declared::Exists`] — the remedy is `PATCH` on that value. One
    /// transaction: the read that decides and the insert it decides on cannot
    /// be interleaved by a second declaration, and a concurrent insert of the
    /// same key surfaces as `ConcurrentMutation` through `contention_or_db`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] on an unreadable stored row.
    pub async fn declare_value(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        class: TaxonomyClass,
        entry: TaxonomyEntry,
        stamp: AuditStamp,
    ) -> Result<Declared, RepoError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Declared, RepoError, _>(move |txn| {
                Box::pin(
                    async move { apply_declare(txn, &scope, tenant_id, class, entry, stamp).await },
                )
            })
            .await;
        outcome
            .map_err(|e| e.into_domain(|infra| RepoError::Db(format!("taxonomy declare: {infra}"))))
    }

    /// Every declared customer-group value, `active` and `retired` alike,
    /// ordered by value — [`TaxonomyRepo::list`]'s sibling for
    /// `pricing_customer_group_taxonomy` (`inst-cg-taxonomy`).
    ///
    /// **Not an arm of [`TaxonomyRepo::list`].** That method is parameterised
    /// over [`TaxonomyClass`], which is deliberately the vocabulary of the
    /// shared `{class}` route alone (`crate::domain::taxonomy`'s module doc) —
    /// giving it a fifth arm would make this value set addressable through the
    /// exact route `design/05-governance.md`'s endpoint map excludes it from.
    /// This is a parallel, one-table method instead.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
    /// when a stored `state` or `value` is outside what its `CHECK` admits.
    pub async fn list_customer_groups(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
    ) -> Result<Vec<TaxonomyEntry>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("customer-group taxonomy conn: {e}")))?;
        list_customer_group_on(&conn, scope, tenant_id).await
    }

    /// Replace the whole customer-group value set —
    /// `design/09-price-overlays.md` §5's
    /// `PUT /bss-pricing/v1/customer-groups/taxonomy`, [`TaxonomyRepo::replace`]'s
    /// sibling.
    ///
    /// Same whole-set-replacement, retire-not-delete and single-transaction
    /// semantics as the four-class `PUT` — the module doc's argument applies
    /// unchanged, over one table instead of four. Unlike the four, a customer
    /// group is never a price-row axis, so there is no row plane to check, on
    /// `references_to`'s own reasoning for `brand`, `partner` and `orgTier` —
    /// but it has a plane the four classes do not: the retire guard here also
    /// counts **live rows of `pricing_group_membership`**
    /// (`references_to_customer_group`'s own doc says which count as live),
    /// alongside published overlay scopes (`ScopeClass::CustomerGroup`).
    /// There is likewise no D-245 tax-marker guard: this table carries no
    /// `tax_*` columns.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
    /// on an unreadable stored row. A refused retirement is **not** an error
    /// here, for [`TaxonomyRepo::replace`]'s reason.
    pub async fn replace_customer_groups(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        entries: Vec<TaxonomyEntry>,
        asserted: &PolicyTag,
        stamp: AuditStamp,
    ) -> Result<Replaced, RepoError> {
        let scope = scope.clone();
        let asserted = asserted.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Replaced, RepoError, _>(move |txn| {
                Box::pin(async move {
                    apply_replace_customer_group(txn, &scope, tenant_id, entries, &asserted, stamp)
                        .await
                })
            })
            .await;
        outcome.map_err(|e| {
            e.into_domain(|infra| {
                RepoError::Db(format!("customer-group taxonomy replace: {infra}"))
            })
        })
    }

    /// The tenant's declared rounding vocabulary, active and retired alike
    /// (D-334).
    ///
    /// A **parallel, one-table method** for `list_customer_groups`' reason, and
    /// it is a stronger reason here: [`TaxonomyClass::scope_class`] is a total
    /// function into [`ScopeClass`], so a fifth class would declare that an
    /// overlay may be scoped by rounding policy and would reach the
    /// `pricing_price_overlay.scope_class` `CHECK`. It cannot.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
    /// when a stored `state` or `value` is outside what its `CHECK` admits.
    pub async fn list_rounding_policies(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
    ) -> Result<Vec<TaxonomyEntry>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("rounding taxonomy conn: {e}")))?;
        list_rounding_policy_on(&conn, scope, tenant_id).await
    }

    /// Replace the declared rounding vocabulary wholesale, gated on the tag the
    /// caller read (D-334).
    ///
    /// `replace_customer_groups`' shape, including the reason the premise is
    /// tested **inside** the transaction: a `PUT` replaces the whole set, so two
    /// callers whose reads both precede either commit would each pass and the
    /// second would silently drop what the first added.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn replace_rounding_policies(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        entries: Vec<TaxonomyEntry>,
        asserted: &PolicyTag,
        stamp: AuditStamp,
    ) -> Result<Replaced, RepoError> {
        let scope = scope.clone();
        let asserted = asserted.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Replaced, RepoError, _>(move |txn| {
                Box::pin(async move {
                    apply_replace_rounding_policy(txn, &scope, tenant_id, entries, &asserted, stamp)
                        .await
                })
            })
            .await;
        outcome.map_err(|e| {
            e.into_domain(|infra| RepoError::Db(format!("rounding taxonomy replace: {infra}")))
        })
    }

    /// The tenant's declared GL-code vocabulary, active and retired alike
    /// (D-356).
    ///
    /// [`Self::list_rounding_policies`]' shape and its reason, one vocabulary
    /// over: a parallel one-table method, because a fifth [`TaxonomyClass`] would
    /// declare that an overlay may be scoped by GL code.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
    /// when a stored `state` or `value` is outside what its `CHECK` admits.
    pub async fn list_gl_codes(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
    ) -> Result<Vec<TaxonomyEntry>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("gl-code taxonomy conn: {e}")))?;
        list_gl_code_on(&conn, scope, tenant_id).await
    }

    /// Replace the declared GL-code vocabulary wholesale, gated on the tag the
    /// caller read (D-356).
    ///
    /// [`Self::replace_rounding_policies`]' shape, including the reason the
    /// premise is tested **inside** the transaction: a `PUT` replaces the whole
    /// set, so two callers whose reads both precede either commit would each pass
    /// and the second would silently drop what the first added.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn replace_gl_codes(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        entries: Vec<TaxonomyEntry>,
        asserted: &PolicyTag,
        stamp: AuditStamp,
    ) -> Result<Replaced, RepoError> {
        let scope = scope.clone();
        let asserted = asserted.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Replaced, RepoError, _>(move |txn| {
                Box::pin(async move {
                    apply_replace_gl_code(txn, &scope, tenant_id, entries, &asserted, stamp).await
                })
            })
            .await;
        outcome.map_err(|e| {
            e.into_domain(|infra| RepoError::Db(format!("gl-code taxonomy replace: {infra}")))
        })
    }
}

/// What a `PUT` did, or refused to do.
///
/// A pair rather than a `Result`, because a refused retirement is a **domain**
/// answer carrying §5's code and the two reference counts, not a storage fault —
/// and because the caller renders the taxonomy either way: on refusal it is the
/// unchanged one, which is what the operator has to re-author against.
#[derive(Clone, Debug)]
pub struct Replaced {
    /// The taxonomy as it stands after the call.
    pub entries: Vec<TaxonomyEntry>,
    /// Empty on success; `TAXONOMY_VALUE_IN_USE` violations otherwise.
    pub report: ValidationReport,
    /// The asserted `If-Match` tag no longer described the stored taxonomy, so
    /// **nothing was written**.
    ///
    /// A flag rather than a `RepoError`, for the same reason `report` is not one:
    /// §5 types this refusal 409 `STALE_VERSION` and the caller renders the
    /// taxonomy either way — on refusal it is the one the operator must
    /// re-author against.
    pub stale: bool,
}

/// What a `POST …/values` did.
#[derive(Clone, Debug)]
pub enum Declared {
    /// A new row; the entry as stored.
    Created(TaxonomyEntry),
    /// The value was already held with **this exact** content: nothing was
    /// written, and the caller answers the create's replay.
    Replayed(TaxonomyEntry),
    /// The value is held with other content, or retired. Nothing was written;
    /// the entry is the one the caller is told to `PATCH`.
    Exists(TaxonomyEntry),
}

// ---------------------------------------------------------------------------
// The publish-time reads.
// ---------------------------------------------------------------------------

/// The tenant's **active** region values — `inst-tx-region`'s universe.
///
/// Called by `infra::publish::rule_params` — `inst-tx-region`'s universe — and
/// by the price authoring route for the save-time half.
///
/// It takes a runner rather than a provider so `rule_params` can resolve it
/// **inside** the commit transaction: §4.2 runs the rule set twice, and a read
/// that could not join the transaction would answer the second run against a
/// world the commit is not holding.
///
/// `active` only, which is `overlay_repo::declares`' predicate one plane over: a
/// value that reached `retired` anyway must not validate a new row against
/// itself.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a stored value is blank, which its `CHECK` refuses.
pub async fn active_regions(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<BTreeSet<Region>, RepoError> {
    // D-354: the seed is the universe until the tenant declares its own. "No row
    // at all", not "no active row": a tenant that retired everything has rows and
    // an empty universe, which is what it asked for.
    if !holds_a_region_row(runner, scope, tenant_id).await? {
        return Region::new(SEEDED_REGION)
            .map(|region| std::iter::once(region).collect())
            .map_err(|e| RepoError::CorruptRow(format!("seeded region `{SEEDED_REGION}`: {e}")));
    }
    let rows = region_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(region_taxonomy::Column::TenantId.eq(tenant_id))
                .add(region_taxonomy::Column::State.eq(TaxonomyState::Active.as_str())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_region_taxonomy: {e}")))?;
    rows.into_iter()
        .map(|row| {
            Region::new(&row.value).map_err(|e| {
                RepoError::CorruptRow(format!(
                    "pricing_region_taxonomy.value `{}`: {e}",
                    row.value
                ))
            })
        })
        .collect()
}

/// The tenant's **active** rounding references — `inst-tx-rounding`'s universe
/// (D-334).
///
/// [`active_regions`]' shape and its reasons: a runner rather than a provider so
/// `rule_params` resolves it inside the commit transaction, and `active` only so
/// a retired value cannot validate a new row against itself.
///
/// The values are returned as plain strings. Unlike a region there is no newtype
/// to parse into: this gear persists a reference to a policy it neither defines
/// nor applies, so it has nothing to validate the *shape* of a reference against
/// and inventing a parser here would be the legislating D-320 declined.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn active_rounding_policies(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<BTreeSet<String>, RepoError> {
    let rows = rounding_policy_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(rounding_policy_taxonomy::Column::TenantId.eq(tenant_id))
                .add(rounding_policy_taxonomy::Column::State.eq(TaxonomyState::Active.as_str())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_rounding_policy_taxonomy: {e}")))?;
    Ok(rows.into_iter().map(|row| row.value).collect())
}

/// The tenant's **active** GL codes — `inst-ds-glcode`'s universe (D-356).
///
/// [`active_rounding_policies`]' shape and its reasons: a runner rather than a
/// provider so `rule_params` resolves it inside the commit transaction, and
/// `active` only so a retired code cannot validate a new descriptor set against
/// itself. Plain strings for the same reason too: this gear persists a
/// reference to an account it neither defines nor posts to, so it has nothing to
/// validate the *shape* of a code against.
///
/// **This read is the provider seam D-356 names.** The publish rule is handed
/// the set and never learns where it came from; today the set is what the
/// tenant declared through `PUT /bss-pricing/v1/config/gl-codes`, and a future
/// ERP gear that populates or reconciles `pricing_gl_code_taxonomy` changes
/// nothing on this side of the seam.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn active_gl_codes(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<BTreeSet<String>, RepoError> {
    let rows = gl_code_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(gl_code_taxonomy::Column::TenantId.eq(tenant_id))
                .add(gl_code_taxonomy::Column::State.eq(TaxonomyState::Active.as_str())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_gl_code_taxonomy: {e}")))?;
    Ok(rows.into_iter().map(|row| row.value).collect())
}

/// C4's `RegionTaxReadiness` lookup: `(tenant, region) -> { taxCategory, ratePresent }`.
///
/// **`None` is an unknown region and C4 fails closed on it** — *"Readiness is
/// resolved per `(tenant, region)`; an unknown region fails closed"*. That is a
/// different fact from a *declared* region with no default category, which comes
/// back as `Some` with `tax_category: None` and which `inst-td-policy`'s coalesce
/// is entirely about. Collapsing the two would make a row whose own
/// `tax_category_ref` satisfies the check indistinguishable from a row in a
/// region nobody declared.
///
/// A **retired** region reads as `None` for [`active_regions`]' reason: it
/// declares nothing, so it has no readiness to report.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn region_readiness(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    region: &Region,
) -> Result<Option<RegionTaxMarkers>, RepoError> {
    // D-354: the seed reads as declared, with its fail-closed markers.
    if region.as_str() == SEEDED_REGION && !holds_a_region_row(runner, scope, tenant_id).await? {
        return Ok(seeded_region().tax);
    }
    let found = region_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(region_taxonomy::Column::TenantId.eq(tenant_id))
                .add(region_taxonomy::Column::Value.eq(region.as_str()))
                .add(region_taxonomy::Column::State.eq(TaxonomyState::Active.as_str())),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_region_taxonomy: {e}")))?;
    Ok(found.map(|row| RegionTaxMarkers {
        tax_category: row.tax_category,
        tax_rate_present: row.tax_rate_present,
    }))
}

/// Every declared region's readiness in one read — the publish path's shape.
///
/// One statement rather than one per row, for `rule_params`' reason: a plan at
/// C1's 20-currency floor spans as many markets, and twenty round trips inside
/// the commit transaction is twenty chances to hold it open longer than it needs.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn region_readiness_map(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<BTreeMap<String, RegionTaxMarkers>, RepoError> {
    // D-354, as `region_readiness` reads it, over the whole (one-member) universe.
    if !holds_a_region_row(runner, scope, tenant_id).await? {
        let seed = seeded_region();
        return Ok(seed
            .tax
            .into_iter()
            .map(|markers| (seed.value.as_str().to_owned(), markers))
            .collect());
    }
    let rows = region_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(region_taxonomy::Column::TenantId.eq(tenant_id))
                .add(region_taxonomy::Column::State.eq(TaxonomyState::Active.as_str())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_region_taxonomy: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.value,
                RegionTaxMarkers {
                    tax_category: row.tax_category,
                    tax_rate_present: row.tax_rate_present,
                },
            )
        })
        .collect())
}

// ---------------------------------------------------------------------------
// D-354 — the seeded region.
// ---------------------------------------------------------------------------

/// Does the tenant hold **any** region row, active or retired?
///
/// The seed's predicate. Not "any active row": a tenant that declared regions
/// and retired them all holds rows and has, deliberately, an empty universe.
async fn holds_a_region_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<bool, RepoError> {
    let count = region_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(region_taxonomy::Column::TenantId.eq(tenant_id)))
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_region_taxonomy: {e}")))?;
    Ok(count > 0)
}

/// Write the seeded `global` row for a tenant that holds no region row yet —
/// the first step of **every** region write (D-354), inside that write's own
/// transaction, so the row the write goes on to touch is the one the reads have
/// been answering. A no-op once the tenant holds any row. Writes no audit
/// record: the seed is the gear's declared default, not a tenant's act.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn materialise_region_seed(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<(), RepoError> {
    if holds_a_region_row(runner, scope, tenant_id).await? {
        return Ok(());
    }
    insert_entry(
        runner,
        scope,
        tenant_id,
        TaxonomyClass::Region,
        &seeded_region(),
    )
    .await
}

// ---------------------------------------------------------------------------
// The reference counts behind `inst-tx-mutation`.
// ---------------------------------------------------------------------------

/// What still names `value`, across both planes §3 step 3 enumerates.
///
/// # Why the operand is a tenant scope and not the caller's
///
/// This counts the tenant's published rows, which is a fact about the data and
/// not about who is asking: it is the operand of D-355's governance predicate
/// and of the retirement guard, so an answer that varies with the caller's grant
/// is a control that varies with the caller's grant. The caller's scope cannot
/// be used even when it is honest — a `config` grant compiles against
/// `resource_col = "tenant_id"`, while `pricing_price` and
/// `pricing_price_overlay` declare `price_id` and `price_overlay_id`, so a
/// resource-pinned grant matches nothing here and reads zero references. Nor can
/// it be projected with `tenant_only()`: that is deny-all on an unconstrained
/// scope, which reads zero the same way. `price_repo::aggregate_rows_for_authorized_plans`
/// mints its operand for this reason and states it the same way.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn references_to(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    class: TaxonomyClass,
    value: &ScopeValue,
) -> Result<ValueReferences, RepoError> {
    let scope = &AccessScope::for_tenant(tenant_id);
    // Only `region` is an axis of a price row: §3 step 2 is explicit that
    // `brand` is "**not** a price-row field (Foundation §4.1)", and the same is
    // true of the two D-120 classes. Counting the row plane for them would be a
    // query that can only ever answer zero.
    let published_price_rows = if class == TaxonomyClass::Region {
        price::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price::Column::TenantId.eq(tenant_id))
                    .add(price::Column::Region.eq(value.as_str()))
                    .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
            )
            .count(runner)
            .await
            .map_err(|e| RepoError::Db(format!("count pricing_price: {e}")))?
    } else {
        0
    };

    let active_overlay_scopes = price_overlay::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(price_overlay::Column::ScopeClass.eq(class.scope_class().as_str()))
                .add(price_overlay::Column::ScopeValue.eq(value.as_str()))
                .add(
                    price_overlay::Column::LifecycleState.eq(OverlayLifecycle::Published.as_str()),
                ),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_price_overlay: {e}")))?;

    Ok(ValueReferences {
        published_price_rows,
        active_overlay_scopes,
    })
}

/// Reference counts for every declared value, including unreferenced values.
///
/// Both list `references` and `edit_governed` use these same counts, and they
/// are the write gate's own predicate, so the two doors cannot disagree.
///
/// **Two grouped reads, not one per value.** Folding [`references_to`] over the
/// list was bounded by the declared taxonomy rather than by the price-row table,
/// which is the bound that was checked — but not by the request: nothing caps how
/// many values a tenant declares, `GET /config/taxonomies/{class}` lists the class
/// whole with no `limit`, and D-353 made declaring one at a time the only door, so
/// the loop grew one or two sequential round trips per declared value with the
/// tenant's own authoring as the only ceiling.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn references_for_values(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    class: TaxonomyClass,
    values: &[TaxonomyEntry],
) -> Result<BTreeMap<String, ValueReferences>, RepoError> {
    #[derive(sea_orm::FromQueryResult)]
    struct Group {
        value: String,
        cnt: i64,
    }

    let mut references: BTreeMap<String, ValueReferences> = values
        .iter()
        .map(|entry| (entry.value.as_str().to_owned(), ValueReferences::default()))
        .collect();
    if references.is_empty() {
        return Ok(references);
    }
    let scope = &AccessScope::for_tenant(tenant_id);
    let names: Vec<String> = references.keys().cloned().collect();

    // The overlay plane, for every class: one `GROUP BY scope_value` restricted
    // to the declared values, rather than one COUNT per value.
    for chunk in names.chunks(MAX_IN_BINDS) {
        let groups: Vec<Group> = price_overlay::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price_overlay::Column::TenantId.eq(tenant_id))
                    .add(price_overlay::Column::ScopeClass.eq(class.scope_class().as_str()))
                    .add(price_overlay::Column::ScopeValue.is_in(chunk.to_vec()))
                    .add(
                        price_overlay::Column::LifecycleState
                            .eq(OverlayLifecycle::Published.as_str()),
                    ),
            )
            .project_all(runner, |q| {
                q.select_only()
                    .column_as(Expr::col(price_overlay::Column::ScopeValue), "value")
                    .column_as(
                        Expr::col(price_overlay::Column::PriceOverlayId).count(),
                        "cnt",
                    )
                    .group_by(price_overlay::Column::ScopeValue)
                    .into_model::<Group>()
            })
            .await
            .map_err(|e| RepoError::Db(format!("group pricing_price_overlay: {e}")))?;
        for group in groups {
            if let Some(entry) = references.get_mut(&group.value) {
                entry.active_overlay_scopes = u64::try_from(group.cnt).unwrap_or(0);
            }
        }
    }

    // The row plane, only for `region` — [`references_to`] says why the other
    // three classes cannot have a price-row reference at all.
    if class == TaxonomyClass::Region {
        for chunk in names.chunks(MAX_IN_BINDS) {
            let groups: Vec<Group> = price::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(
                    Condition::all()
                        .add(price::Column::TenantId.eq(tenant_id))
                        .add(price::Column::Region.is_in(chunk.to_vec()))
                        .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
                )
                .project_all(runner, |q| {
                    q.select_only()
                        .column_as(Expr::col(price::Column::Region), "value")
                        .column_as(Expr::col(price::Column::PriceId).count(), "cnt")
                        .group_by(price::Column::Region)
                        .into_model::<Group>()
                })
                .await
                .map_err(|e| RepoError::Db(format!("group pricing_price: {e}")))?;
            for group in groups {
                if let Some(entry) = references.get_mut(&group.value) {
                    entry.published_price_rows = u64::try_from(group.cnt).unwrap_or(0);
                }
            }
        }
    }

    Ok(references)
}

/// Published rows in one region that state **no category of their own** — the
/// set D-245's marker guard counts.
///
/// Narrower than [`references_to`]'s row count on purpose, and the narrowing is
/// the rule: a published row carrying its own `tax_category_ref` does not resolve
/// through the region default and is unaffected by the default going away. Only a
/// row stating none leans on it, which is exactly `inst-td-policy`'s coalesce read
/// from the other end.
///
/// # Errors
/// [`RepoError::Db`] when the count fails.
pub async fn rows_resolving_category_through(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    value: &ScopeValue,
) -> Result<u64, RepoError> {
    let scope = &AccessScope::for_tenant(tenant_id);
    price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::Region.eq(value.as_str()))
                .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str()))
                .add(price::Column::TaxCategoryRef.is_null()),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_price on a region default: {e}")))
}

// ---------------------------------------------------------------------------
// The write.
// ---------------------------------------------------------------------------

async fn apply_replace(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    entries: Vec<TaxonomyEntry>,
    asserted: &PolicyTag,
    stamp: AuditStamp,
) -> Result<Replaced, RepoError> {
    if class == TaxonomyClass::Region {
        materialise_region_seed(runner, scope, tenant_id).await?;
    }
    let held = list_on(runner, scope, tenant_id, class).await?;

    // **The `If-Match` premise is tested here, and only here** — D-186's division
    // of labour, and `infra::threshold::propose`'s arrangement exactly: the
    // transport refuses a request it cannot *understand*, and the store refuses
    // one whose premise has *moved*.
    //
    // A comparison in the handler reads the world, decides, and then hands the
    // decision to a statement that races it. That is not hypothetical here: the
    // `PUT` is a whole-set replacement, so two callers whose reads both precede
    // either commit each pass a handler-side check, and the second one's write
    // **retires** whatever the first added — a value the retire guard cannot
    // protect, because a value just created has no published references by
    // construction. Both callers then see 200.
    //
    // Computed from the same `held` the write below works from, so there is no
    // second read to disagree with.
    if tag_of(class, &held) != *asserted {
        return Ok(Replaced {
            entries: held,
            report: ValidationReport::default(),
            stale: true,
        });
    }
    let submitted: BTreeMap<String, TaxonomyEntry> = entries
        .into_iter()
        .map(|entry| (entry.value.as_str().to_owned(), entry))
        .collect();

    // Every retirement this request implies, both spellings of it: a value the
    // body left out, and a value the body carries with `state: retired`. They
    // are the same act and are guarded identically — an operator must not be
    // able to slip past the guard by choosing the other spelling.
    let mut report = ValidationReport::default();
    for existing in &held {
        let key = existing.value.as_str();
        let retiring = submitted
            .get(key)
            .is_none_or(|entry| entry.state == TaxonomyState::Retired);
        if !retiring || existing.state == TaxonomyState::Retired {
            // Already retired is not a retirement: re-asserting a value's
            // current state is a no-op, and guarding it would make a taxonomy
            // with one guarded retirement permanently un-`PUT`-able.
            continue;
        }
        let references = references_to(runner, tenant_id, class, &existing.value).await?;
        report.absorb(check_retirable(class, &existing.value, references));
    }

    // D-245: dropping a region's `taxCategory` is its own act, guarded on its own
    // read. It is **not** folded into the loop above because it is reachable by a
    // `PUT` that retires nothing at all — a body re-asserting every value and
    // merely omitting one marker — and the remedy is different: nothing is
    // retargeted, the operator re-declares the default or authors the category
    // onto the rows leaning on it.
    //
    // Only the `region` class: §6 gives the tax markers to that table alone, and
    // `TaxonomyClass::carries_tax_markers` is the one answer to that question.
    if class.carries_tax_markers() {
        for existing in &held {
            // A value the body drops entirely is a **retirement**, already judged
            // above. Guarding it here as well would refuse one act with two
            // violations naming two different remedies.
            let Some(entry) = submitted.get(existing.value.as_str()) else {
                continue;
            };
            let had = existing
                .tax
                .as_ref()
                .and_then(|t| t.tax_category.as_deref());
            let keeps = entry.tax.as_ref().and_then(|t| t.tax_category.as_deref());
            if had.is_some() && keeps.is_none() {
                let dependents =
                    rows_resolving_category_through(runner, tenant_id, &existing.value).await?;
                report.absorb(check_tax_category_removable(&existing.value, dependents));
            }
        }
    }

    if !report.is_publishable() {
        // One transaction, one verdict. Nothing has been written yet, so the
        // taxonomy handed back is the one the operator has to re-author against.
        return Ok(Replaced {
            entries: held,
            report,
            stale: false,
        });
    }

    write_set(runner, scope, tenant_id, class, &held, &submitted).await?;
    let now = list_on(runner, scope, tenant_id, class).await?;
    record_mutation(runner, scope, tenant_id, class, stamp).await?;
    Ok(Replaced {
        entries: now,
        report,
        stale: false,
    })
}

/// [`TaxonomyRepo::declare_value`]'s transaction body.
async fn apply_declare(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    entry: TaxonomyEntry,
    stamp: AuditStamp,
) -> Result<Declared, RepoError> {
    if class == TaxonomyClass::Region {
        materialise_region_seed(runner, scope, tenant_id).await?;
    }
    let held = list_on(runner, scope, tenant_id, class).await?;
    if let Some(existing) = held.into_iter().find(|h| h.value == entry.value) {
        return Ok(if existing == entry {
            Declared::Replayed(existing)
        } else {
            Declared::Exists(existing)
        });
    }
    insert_entry(runner, scope, tenant_id, class, &entry).await?;
    record_value_mutation(
        runner,
        scope,
        tenant_id,
        class,
        &entry.value,
        AuditAction::Create,
        None,
        Some(&entry),
        None,
        stamp,
    )
    .await?;
    Ok(Declared::Created(entry))
}

/// One declared value, read on the caller's runner — [`TaxonomyRepo::find_value`]
/// for a transaction, and the read every arm of the per-value `PATCH` works from.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] on an unreadable stored row.
pub async fn find_value_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    value: &ScopeValue,
) -> Result<Option<TaxonomyEntry>, RepoError> {
    Ok(list_on(runner, scope, tenant_id, class)
        .await?
        .into_iter()
        .find(|held| held.value == *value))
}

/// The two guards a value edit must pass, on the caller's runner: the retire
/// guard (`inst-tx-mutation`) when `next` retires an active value, and D-245's
/// cleared-category guard when a region default goes away under rows that
/// resolve through it. Empty when the edit is admissible.
///
/// Run at **submit** — so a unit that could never commit is refused rather than
/// opened (D-350's lesson, one plane over) — and again at **commit**, because
/// the world may have moved between the two.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn judge_value_patch(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    class: TaxonomyClass,
    held: &TaxonomyEntry,
    next: &TaxonomyEntry,
) -> Result<ValidationReport, RepoError> {
    let mut report = ValidationReport::default();
    if held.state == TaxonomyState::Active && next.state == TaxonomyState::Retired {
        let references = references_to(runner, tenant_id, class, &held.value).await?;
        report.absorb(check_retirable(class, &held.value, references));
    }
    if class.carries_tax_markers() && next.state != TaxonomyState::Retired {
        let had = held.tax.as_ref().and_then(|t| t.tax_category.as_deref());
        let keeps = next.tax.as_ref().and_then(|t| t.tax_category.as_deref());
        if had.is_some() && keeps.is_none() {
            let dependents =
                rows_resolving_category_through(runner, tenant_id, &held.value).await?;
            report.absorb(check_tax_category_removable(&held.value, dependents));
        }
    }
    Ok(report)
}

/// Write an admitted edit: the row, and the audit record naming the value with
/// its state before and after — and, on a governed commit, the approval it ran
/// under.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
#[allow(
    clippy::too_many_arguments,
    reason = "the edit's operands and its provenance, each a fact only the caller holds; a struct \
              for one call site would name nothing"
)]
pub async fn write_value_patch(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    held: &TaxonomyEntry,
    next: &TaxonomyEntry,
    approval_ref: Option<Uuid>,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    if class == TaxonomyClass::Region {
        materialise_region_seed(runner, scope, tenant_id).await?;
    }
    update_entry(runner, scope, tenant_id, class, next).await?;
    record_value_mutation(
        runner,
        scope,
        tenant_id,
        class,
        &held.value,
        AuditAction::Update,
        Some(held),
        Some(next),
        approval_ref,
        stamp,
    )
    .await
}

/// The per-value audit record: `create` on a declaration, `update` on a patch,
/// subject `taxonomy/{class}/{value}`, and — unlike the whole-set `PUT`'s record —
/// **with** the before and after states, because one value is bounded where a
/// whole list was not. This is what lets an auditor answer *who retired `EU`,
/// and when* from the chain rather than by diffing two snapshots.
///
/// Same chain as the set ([`audit_repo::policy_chain`]): the value is a row of the
/// tenant's config object, and its records sort beside the set's. Its **own**
/// subject kind, `taxonomy_value` (D-353), so an auditor filters the trail to
/// exactly these records and the approval unit that authorised an edit names the
/// same kind as the record the commit wrote.
#[allow(
    clippy::too_many_arguments,
    reason = "an audit record's fields, each a fact only the caller holds"
)]
async fn record_value_mutation(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    value: &ScopeValue,
    action: AuditAction,
    before: Option<&TaxonomyEntry>,
    after: Option<&TaxonomyEntry>,
    approval_ref: Option<Uuid>,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::policy_chain(),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action,
            subject_kind: AuditSubjectKind::TaxonomyValue,
            subject_ref: taxonomy_value_ref(class, value),
            before_state: before.map(value_state),
            after_state: after.map(value_state),
            // A declaration runs under no unit (D-353: adding a value is not the
            // governed act); an edit's commit names the unit that authorised it.
            approval_ref,
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
}

/// One value's state as the audit trail renders it: every field the `GET`
/// serves, so a `before`/`after` pair reads as the diff it is.
fn value_state(entry: &TaxonomyEntry) -> serde_json::Value {
    serde_json::json!({
        "state": entry.state.as_str(),
        "display_name": entry.display_name,
        "tax_category": entry.tax.as_ref().and_then(|t| t.tax_category.as_deref()),
        "tax_rate_present": entry.tax.as_ref().map(|t| t.tax_rate_present),
    })
}

/// Apply the submitted set over the held one.
///
/// Three moves and no fourth: update a value both sets carry, insert one only
/// the body carries, and retire one only the store carries. There is no delete —
/// see the module doc.
async fn write_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    held: &[TaxonomyEntry],
    submitted: &BTreeMap<String, TaxonomyEntry>,
) -> Result<(), RepoError> {
    let held_keys: BTreeSet<&str> = held.iter().map(|e| e.value.as_str()).collect();

    for entry in submitted.values() {
        if held_keys.contains(entry.value.as_str()) {
            update_entry(runner, scope, tenant_id, class, entry).await?;
        } else {
            insert_entry(runner, scope, tenant_id, class, entry).await?;
        }
    }
    for existing in held {
        if submitted.contains_key(existing.value.as_str()) {
            continue;
        }
        let retired = TaxonomyEntry {
            state: TaxonomyState::Retired,
            ..existing.clone()
        };
        update_entry(runner, scope, tenant_id, class, &retired).await?;
    }
    Ok(())
}

/// `inst-tx-mutation`'s audit half: taxonomy mutation is *"tenant-admin config,
/// audited"*.
///
/// Written **inside** the same transaction as the mutation, which is the whole of
/// what D-14 asks for and the arrangement every other mutating path here holds
/// to: a record that commits with its mutation cannot be lost by a crash between
/// the two, and a failure to write it rolls the mutation back rather than leaving
/// a trail that is silently incomplete.
///
/// One record per `PUT`, not one per value. The act an operator performed is
/// *"replaced the brand taxonomy"*; splitting it into a record per changed row
/// would make one decision look like several and would leave an auditor unable to
/// tell a re-authoring from a burst of unrelated edits.
///
/// # The subject kind is `policy`, and that is a decision rather than a default
///
/// S5 §6's aggregate list is *plan, overlay, payer, policy, bulk operation*, and
/// [`AuditSubjectKind`] carries exactly the tokens that list and
/// `chk_pricing_approval_subject_kind` admit. **There is no `taxonomy` token in
/// either**, and minting one here would be minting a design-set token this gear
/// does not own — the standing rule that kept `Window` and `Policy` out until
/// their writers existed cuts the other way too.
///
/// `policy` is the right one of the five rather than the least wrong. A taxonomy
/// is per-tenant configuration governed by the same `config × write` gate as the
/// tax-display policy, it is a per-tenant singleton like the policy object, and
/// [`audit_repo::policy_chain`] is already *"the per-tenant singleton segment"*.
/// The `subject_ref` discriminates within it — `taxonomy/brand` against the
/// policy object's own ref — so an auditor asking *what happened to this tenant's
/// brand list* walks one segment and filters, exactly as they do for the
/// threshold policy's versions.
///
/// Recorded as `T-5` in the owed register so the aggregate list is amended
/// deliberately if the set wants a sixth.
async fn record_mutation(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::policy_chain(),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action: AuditAction::Update,
            subject_kind: AuditSubjectKind::Policy,
            subject_ref: taxonomy_ref(class),
            // A taxonomy is a value **set** and the record names the set, not a
            // diff of it: the before/after columns hold version refs
            // (`inst-au-complete`), and a taxonomy has no version to refer to.
            // Rendering the whole list into them would put unbounded tenant
            // configuration on the hash chain for no question it answers.
            before_state: None,
            after_state: None,
            // No approval unit: taxonomy mutation is CatalogAdmin config (§10),
            // not one of the D-10 always-material acts.
            approval_ref: None,
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
}

/// The audited subject: one taxonomy of one tenant.
#[must_use]
pub fn taxonomy_ref(class: TaxonomyClass) -> String {
    format!("taxonomy/{}", class.path_segment())
}

/// The audited subject: one value of one taxonomy of one tenant.
#[must_use]
pub fn taxonomy_value_ref(class: TaxonomyClass, value: &ScopeValue) -> String {
    format!("{}/{}", taxonomy_ref(class), value.as_str())
}

// ---------------------------------------------------------------------------
// Per-table statements.
//
// One arm per table because each is a distinct entity, which is
// `overlay_repo::declares`' arrangement and its argument: `SeaORM` gives four
// generated types with no common trait to write these against, and a macro over
// them would hide which columns each carries — the `tax_*` pair being on exactly
// one of the four.
// ---------------------------------------------------------------------------

async fn list_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
) -> Result<Vec<TaxonomyEntry>, RepoError> {
    let rows: Vec<(String, String, String, Option<RegionTaxMarkers>)> = match class {
        TaxonomyClass::Region => region_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(region_taxonomy::Column::TenantId.eq(tenant_id)))
            .order_by(region_taxonomy::Column::Value, sea_orm::Order::Asc)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_region_taxonomy: {e}")))?
            .into_iter()
            .map(|r| {
                let markers = RegionTaxMarkers {
                    tax_category: r.tax_category,
                    tax_rate_present: r.tax_rate_present,
                };
                (r.value, r.display_name, r.state, Some(markers))
            })
            .collect(),
        TaxonomyClass::Brand => brand_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(brand_taxonomy::Column::TenantId.eq(tenant_id)))
            .order_by(brand_taxonomy::Column::Value, sea_orm::Order::Asc)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_brand_taxonomy: {e}")))?
            .into_iter()
            .map(|r| (r.value, r.display_name, r.state, None))
            .collect(),
        TaxonomyClass::Partner => partner_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(partner_taxonomy::Column::TenantId.eq(tenant_id)))
            .order_by(partner_taxonomy::Column::Value, sea_orm::Order::Asc)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_partner_taxonomy: {e}")))?
            .into_iter()
            .map(|r| (r.value, r.display_name, r.state, None))
            .collect(),
        TaxonomyClass::OrgTier => org_tier_taxonomy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(org_tier_taxonomy::Column::TenantId.eq(tenant_id)))
            .order_by(org_tier_taxonomy::Column::Value, sea_orm::Order::Asc)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read pricing_org_tier_taxonomy: {e}")))?
            .into_iter()
            .map(|r| (r.value, r.display_name, r.state, None))
            .collect(),
    };
    // D-354: a tenant that holds **no** region row reads the seeded `global`.
    // Virtual — nothing is written on a read; the first region write
    // materialises it (`materialise_region_seed`).
    if class == TaxonomyClass::Region && rows.is_empty() {
        return Ok(vec![seeded_region()]);
    }

    rows.into_iter()
        .map(|(value, display_name, state, tax)| {
            Ok(TaxonomyEntry {
                value: ScopeValue::new(&value).ok_or_else(|| {
                    RepoError::CorruptRow(format!("{}.value is blank", class.table()))
                })?,
                display_name,
                state: TaxonomyState::parse(&state).ok_or_else(|| {
                    RepoError::CorruptRow(format!("{}.state `{state}`", class.table()))
                })?,
                tax,
            })
        })
        .collect()
}

async fn insert_entry(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    entry: &TaxonomyEntry,
) -> Result<(), RepoError> {
    let value = entry.value.as_str().to_owned();
    let display_name = entry.display_name.clone();
    let state = entry.state.as_str().to_owned();
    match class {
        TaxonomyClass::Region => {
            let markers = entry.tax.clone().unwrap_or_default();
            let row = region_taxonomy::ActiveModel {
                tenant_id: Set(tenant_id),
                value: Set(value),
                display_name: Set(display_name),
                state: Set(state),
                tax_category: Set(markers.tax_category),
                tax_rate_present: Set(markers.tax_rate_present),
            };
            region_taxonomy::Entity::insert(row.clone())
                .secure()
                .scope_with_model(scope, &row)
                .map_err(|e| RepoError::Db(format!("scope pricing_region_taxonomy: {e}")))?
                .exec(runner)
                .await
                .map(|_| ())
                .map_err(|e| {
                    contention_or_db(
                        &e,
                        "pricing_region_taxonomy",
                        "insert pricing_region_taxonomy",
                    )
                })
        }
        TaxonomyClass::Brand => {
            let row = brand_taxonomy::ActiveModel {
                tenant_id: Set(tenant_id),
                value: Set(value),
                display_name: Set(display_name),
                state: Set(state),
            };
            brand_taxonomy::Entity::insert(row.clone())
                .secure()
                .scope_with_model(scope, &row)
                .map_err(|e| RepoError::Db(format!("scope pricing_brand_taxonomy: {e}")))?
                .exec(runner)
                .await
                .map(|_| ())
                .map_err(|e| {
                    contention_or_db(
                        &e,
                        "pricing_brand_taxonomy",
                        "insert pricing_brand_taxonomy",
                    )
                })
        }
        TaxonomyClass::Partner => {
            let row = partner_taxonomy::ActiveModel {
                tenant_id: Set(tenant_id),
                value: Set(value),
                display_name: Set(display_name),
                state: Set(state),
            };
            partner_taxonomy::Entity::insert(row.clone())
                .secure()
                .scope_with_model(scope, &row)
                .map_err(|e| RepoError::Db(format!("scope pricing_partner_taxonomy: {e}")))?
                .exec(runner)
                .await
                .map(|_| ())
                .map_err(|e| {
                    contention_or_db(
                        &e,
                        "pricing_partner_taxonomy",
                        "insert pricing_partner_taxonomy",
                    )
                })
        }
        TaxonomyClass::OrgTier => {
            let row = org_tier_taxonomy::ActiveModel {
                tenant_id: Set(tenant_id),
                value: Set(value),
                display_name: Set(display_name),
                state: Set(state),
            };
            org_tier_taxonomy::Entity::insert(row.clone())
                .secure()
                .scope_with_model(scope, &row)
                .map_err(|e| RepoError::Db(format!("scope pricing_org_tier_taxonomy: {e}")))?
                .exec(runner)
                .await
                .map(|_| ())
                .map_err(|e| {
                    contention_or_db(
                        &e,
                        "pricing_org_tier_taxonomy",
                        "insert pricing_org_tier_taxonomy",
                    )
                })
        }
    }
}

async fn update_entry(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    class: TaxonomyClass,
    entry: &TaxonomyEntry,
) -> Result<(), RepoError> {
    let value = entry.value.as_str().to_owned();
    let display_name = entry.display_name.clone();
    let state = entry.state.as_str().to_owned();
    match class {
        TaxonomyClass::Region => {
            let markers = entry.tax.clone().unwrap_or_default();
            region_taxonomy::Entity::update_many()
                .secure()
                .scope_with(scope)
                .col_expr(
                    region_taxonomy::Column::DisplayName,
                    sea_orm::sea_query::Expr::value(display_name),
                )
                .col_expr(
                    region_taxonomy::Column::State,
                    sea_orm::sea_query::Expr::value(state),
                )
                .col_expr(
                    region_taxonomy::Column::TaxCategory,
                    sea_orm::sea_query::Expr::value(markers.tax_category),
                )
                .col_expr(
                    region_taxonomy::Column::TaxRatePresent,
                    sea_orm::sea_query::Expr::value(markers.tax_rate_present),
                )
                .filter(
                    Condition::all()
                        .add(region_taxonomy::Column::TenantId.eq(tenant_id))
                        .add(region_taxonomy::Column::Value.eq(value)),
                )
                .exec(runner)
                .await
                .map(|_| ())
                .map_err(|e| RepoError::Db(format!("update pricing_region_taxonomy: {e}")))
        }
        TaxonomyClass::Brand => brand_taxonomy::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                brand_taxonomy::Column::DisplayName,
                sea_orm::sea_query::Expr::value(display_name),
            )
            .col_expr(
                brand_taxonomy::Column::State,
                sea_orm::sea_query::Expr::value(state),
            )
            .filter(
                Condition::all()
                    .add(brand_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(brand_taxonomy::Column::Value.eq(value)),
            )
            .exec(runner)
            .await
            .map(|_| ())
            .map_err(|e| RepoError::Db(format!("update pricing_brand_taxonomy: {e}"))),
        TaxonomyClass::Partner => partner_taxonomy::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                partner_taxonomy::Column::DisplayName,
                sea_orm::sea_query::Expr::value(display_name),
            )
            .col_expr(
                partner_taxonomy::Column::State,
                sea_orm::sea_query::Expr::value(state),
            )
            .filter(
                Condition::all()
                    .add(partner_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(partner_taxonomy::Column::Value.eq(value)),
            )
            .exec(runner)
            .await
            .map(|_| ())
            .map_err(|e| RepoError::Db(format!("update pricing_partner_taxonomy: {e}"))),
        TaxonomyClass::OrgTier => org_tier_taxonomy::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                org_tier_taxonomy::Column::DisplayName,
                sea_orm::sea_query::Expr::value(display_name),
            )
            .col_expr(
                org_tier_taxonomy::Column::State,
                sea_orm::sea_query::Expr::value(state),
            )
            .filter(
                Condition::all()
                    .add(org_tier_taxonomy::Column::TenantId.eq(tenant_id))
                    .add(org_tier_taxonomy::Column::Value.eq(value)),
            )
            .exec(runner)
            .await
            .map(|_| ())
            .map_err(|e| RepoError::Db(format!("update pricing_org_tier_taxonomy: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// `pricing_customer_group_taxonomy` — its own table, its own route
// (`inst-cg-taxonomy`).
//
// A parallel set of statements rather than a fifth arm above: those functions
// are keyed on `TaxonomyClass`, which is deliberately the shared `{class}`
// route's own vocabulary (`crate::domain::taxonomy`'s module doc), and this
// table is deliberately not addressable through it
// (`design/05-governance.md`'s endpoint map). Keying this table's statements on
// the same enum would make the two facts disagree the day somebody trusted the
// type instead of the route table.
// ---------------------------------------------------------------------------

async fn list_customer_group_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Vec<TaxonomyEntry>, RepoError> {
    let rows = customer_group_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(customer_group_taxonomy::Column::TenantId.eq(tenant_id)))
        .order_by(customer_group_taxonomy::Column::Value, sea_orm::Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_customer_group_taxonomy: {e}")))?;

    rows.into_iter().map(customer_group_entry).collect()
}

/// Resolve one customer-group value, whatever state it stands in — the
/// membership routes' own reader (Task 6, `GROUP_UNKNOWN`,
/// `design/09-price-overlays.md` §5:257).
///
/// **Every state, not `active` alone**, because the caller has to tell "never
/// declared" from "declared and then retired" apart before it can decide the
/// refusal it means: both are `GROUP_UNKNOWN`, but only the second is
/// `inst-cg-taxonomy`'s retire guard doing its job — see
/// [`DomainError::GroupUnknown`](crate::domain::error::DomainError::GroupUnknown)'s
/// own doc.
///
/// `pub(crate)`: called from a transaction the caller already opened
/// (`crate::infra::membership_publish`), so it takes a runner rather than
/// opening a connection of its own the way [`TaxonomyRepo::list_customer_groups`]
/// does for a plain read.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// on an unreadable stored row.
pub(crate) async fn find_customer_group_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    value: &str,
) -> Result<Option<TaxonomyEntry>, RepoError> {
    // One row by its key, not the tenant's whole taxonomy filtered in Rust. The
    // list read carries no `Value` predicate and no limit, so resolving a single
    // group pulled every group the tenant has — the shape `region_readiness` above
    // already avoids for the same question one table over.
    let found = customer_group_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(customer_group_taxonomy::Column::TenantId.eq(tenant_id))
                .add(customer_group_taxonomy::Column::Value.eq(value)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_customer_group_taxonomy: {e}")))?;
    found.map(customer_group_entry).transpose()
}

/// One stored customer-group row as the domain entry, shared by the list read and
/// the by-key read so the two cannot disagree about what a row means.
fn customer_group_entry(row: customer_group_taxonomy::Model) -> Result<TaxonomyEntry, RepoError> {
    Ok(TaxonomyEntry {
        value: ScopeValue::new(&row.value).ok_or_else(|| {
            RepoError::CorruptRow("pricing_customer_group_taxonomy.value is blank".into())
        })?,
        display_name: row.display_name,
        state: TaxonomyState::parse(&row.state).ok_or_else(|| {
            RepoError::CorruptRow(format!(
                "pricing_customer_group_taxonomy.state `{}`",
                row.state
            ))
        })?,
        // No `tax_*` columns on this table (this table's migration doc), so there
        // is nothing to carry — [`TaxonomyEntry::tax`] is the region taxonomy's
        // alone.
        tax: None,
    })
}

/// What still names a customer-group value — the retire guard's two planes.
///
/// There is no row-plane query to run, unlike [`references_to`]: a customer
/// group is never a price-row axis (Foundation §4.1's row shape has no such
/// field) — a structural zero, on `check_retirable`'s own reasoning for
/// `brand`, `partner` and `orgTier`. But it *is*
/// `pricing_group_membership.group_value`'s axis, and an earlier cut of this
/// function (Task 6) counted the overlay plane alone and never queried that
/// table at all — found by review (Task 8): a group with live members
/// retired unguarded, and those memberships were left pointing at a retired
/// value with nothing having checked.
///
/// # Which memberships count as a reference
///
/// A membership whose interval has already **ended** before `now`
/// (`effective_to.is_some_and(|end| end <= now)`) is not a live reference —
/// the same "history is not a reference" reading this module's own doc gives
/// a `superseded` price row: the group stopped naming that payer, and
/// retiring the value does not reach backward to change what a past
/// enrollment named.
///
/// A membership whose interval has **not started yet** (`effective_from >
/// now`) **is** counted. It is not a draft an operator could still redirect
/// — it is a commitment already recorded, `enroll`'s own write, and
/// refusing the retire guard here is the same protection an already-active
/// membership gets; the only difference is that the collision has not
/// arrived yet. So "live" reduces to "not yet ended":
/// `effective_to IS NULL OR effective_to > now`, with `effective_from`
/// unconsulted — a row that has not started is exactly as much a reference
/// as one already running.
///
/// `now` is the caller's own authored instant
/// ([`AuditStamp::recorded_at`]), the same discipline
/// [`group_membership_repo::resolve_active_membership`](super::group_membership_repo::resolve_active_membership)
/// and every interval check in that module hold to: the guard judges
/// liveness against the instant the mutation is recorded at, not a second,
/// unpinned `OffsetDateTime::now_utc()` read mid-transaction.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
async fn references_to_customer_group(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    value: &ScopeValue,
    now: OffsetDateTime,
) -> Result<CustomerGroupReferences, RepoError> {
    let active_overlay_scopes = price_overlay::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_overlay::Column::TenantId.eq(tenant_id))
                .add(price_overlay::Column::ScopeClass.eq(ScopeClass::CustomerGroup.as_str()))
                .add(price_overlay::Column::ScopeValue.eq(value.as_str()))
                .add(
                    price_overlay::Column::LifecycleState.eq(OverlayLifecycle::Published.as_str()),
                ),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_price_overlay: {e}")))?;

    let live_memberships = group_membership::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(group_membership::Column::TenantId.eq(tenant_id))
                .add(group_membership::Column::GroupValue.eq(value.as_str()))
                .add(
                    Condition::any()
                        .add(group_membership::Column::EffectiveTo.is_null())
                        .add(group_membership::Column::EffectiveTo.gt(now)),
                ),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_group_membership: {e}")))?;

    Ok(CustomerGroupReferences {
        active_overlay_scopes,
        live_memberships,
    })
}

/// The two planes [`references_to_customer_group`] counts —
/// [`crate::domain::taxonomy::ValueReferences`]'s shape, one field renamed
/// for its own axis: a customer group has no row plane to fold `region`'s
/// `published_price_rows` into, and `live_memberships` measures a different
/// thing than a published/draft lifecycle state (see that function's own
/// doc), so this is its own small struct rather than a strained reuse of the
/// four-class one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CustomerGroupReferences {
    active_overlay_scopes: u64,
    live_memberships: u64,
}

impl CustomerGroupReferences {
    const fn any(self) -> bool {
        self.active_overlay_scopes > 0 || self.live_memberships > 0
    }
}

/// `inst-tx-mutation`'s guard, for the customer-group taxonomy —
/// [`check_retirable`]'s analogue over one label rather than a
/// [`TaxonomyClass`].
///
/// **Not built on [`check_retirable`]** even though the two share a shape —
/// "no references, an empty report; otherwise name every plane that is
/// non-zero and violate once" — because they no longer count the same
/// planes: [`check_retirable`] has the row plane customer groups structurally
/// lack, and this has the membership plane the four-class taxonomies
/// structurally lack (`references_to_customer_group`'s own doc). On top of
/// that, [`check_retirable`] interpolates `{class}` — a [`TaxonomyClass`] —
/// into its message via [`std::fmt::Display`], and this taxonomy deliberately
/// has no member of that enum to pass it (the module doc's whole argument).
/// Duplicating the message-building here is cheaper than widening a type
/// whose narrowness is the point.
#[must_use]
fn check_customer_group_retirable(
    value: &ScopeValue,
    references: CustomerGroupReferences,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    if !references.any() {
        return report;
    }
    // Named separately and only when present, `check_retirable`'s own
    // reasoning: an operator retiring a group referenced by overlays alone
    // should not be told "0 live membership(s)" and sent to audit a plane
    // that was never the problem.
    let mut named = Vec::with_capacity(2);
    if references.active_overlay_scopes > 0 {
        named.push(format!(
            "{} published overlay scope(s) select on it",
            references.active_overlay_scopes
        ));
    }
    if references.live_memberships > 0 {
        named.push(format!(
            "{} live membership(s) still name it",
            references.live_memberships
        ));
    }
    report.violate(
        TAXONOMY_VALUE_IN_USE,
        value.as_str(),
        format!(
            "customer_group value `{value}` cannot retire: {}; retirement is guarded rather than \
             cascading, so end or retarget every reference first (inst-cg-taxonomy)",
            named.join(" and ")
        ),
    );
    report
}

/// [`apply_replace`]'s single-table analogue.
async fn apply_replace_customer_group(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    entries: Vec<TaxonomyEntry>,
    asserted: &PolicyTag,
    stamp: AuditStamp,
) -> Result<Replaced, RepoError> {
    let held = list_customer_group_on(runner, scope, tenant_id).await?;

    // The `If-Match` premise, tested here and only here — `apply_replace`'s own
    // reasoning: the store refuses a premise that has *moved*, inside the
    // transaction that writes, computed from the same `held` the write below
    // works from.
    if customer_group_tag_of(&held) != *asserted {
        return Ok(Replaced {
            entries: held,
            report: ValidationReport::default(),
            stale: true,
        });
    }

    let submitted: BTreeMap<String, TaxonomyEntry> = entries
        .into_iter()
        .map(|entry| (entry.value.as_str().to_owned(), entry))
        .collect();

    let mut report = ValidationReport::default();
    for existing in &held {
        let key = existing.value.as_str();
        let retiring = submitted
            .get(key)
            .is_none_or(|entry| entry.state == TaxonomyState::Retired);
        if !retiring || existing.state == TaxonomyState::Retired {
            continue;
        }
        let references = references_to_customer_group(
            runner,
            scope,
            tenant_id,
            &existing.value,
            stamp.recorded_at,
        )
        .await?;
        report.absorb(check_customer_group_retirable(&existing.value, references));
    }

    if !report.is_publishable() {
        return Ok(Replaced {
            entries: held,
            report,
            stale: false,
        });
    }

    write_customer_group_set(runner, scope, tenant_id, &held, &submitted).await?;
    let now = list_customer_group_on(runner, scope, tenant_id).await?;
    record_single_table_mutation(
        runner,
        scope,
        tenant_id,
        ScopeClass::CustomerGroup.as_str(),
        stamp,
    )
    .await?;
    Ok(Replaced {
        entries: now,
        report,
        stale: false,
    })
}

/// [`write_set`]'s single-table analogue.
/// The rounding vocabulary's `PUT`, inside its transaction (D-334).
///
/// `apply_replace_customer_group`'s shape exactly: premise tested here and only
/// here against the same `held` the write works from, retirement guarded per
/// value, and nothing written when either refuses.
async fn apply_replace_rounding_policy(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    entries: Vec<TaxonomyEntry>,
    asserted: &PolicyTag,
    stamp: AuditStamp,
) -> Result<Replaced, RepoError> {
    let held = list_rounding_policy_on(runner, scope, tenant_id).await?;

    if rounding_policy_tag_of(&held) != *asserted {
        return Ok(Replaced {
            entries: held,
            report: ValidationReport::default(),
            stale: true,
        });
    }

    let submitted: BTreeMap<String, TaxonomyEntry> = entries
        .into_iter()
        .map(|entry| (entry.value.as_str().to_owned(), entry))
        .collect();

    let mut report = ValidationReport::default();
    for existing in &held {
        let key = existing.value.as_str();
        let retiring = submitted
            .get(key)
            .is_none_or(|entry| entry.state == TaxonomyState::Retired);
        if !retiring || existing.state == TaxonomyState::Retired {
            continue;
        }
        let (rows, default_names_it) =
            references_to_rounding_policy(runner, scope, tenant_id, &existing.value).await?;
        report.absorb(check_rounding_policy_retirable(
            &existing.value,
            rows,
            default_names_it,
        ));
    }

    if !report.is_publishable() {
        return Ok(Replaced {
            entries: held,
            report,
            stale: false,
        });
    }

    write_rounding_policy_set(runner, scope, tenant_id, &held, &submitted).await?;
    let now = list_rounding_policy_on(runner, scope, tenant_id).await?;
    record_single_table_mutation(runner, scope, tenant_id, ROUNDING_POLICY_RESOURCE, stamp).await?;
    Ok(Replaced {
        entries: now,
        report,
        stale: false,
    })
}

/// [`write_customer_group_set`]'s shape on this table.
async fn write_rounding_policy_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    held: &[TaxonomyEntry],
    submitted: &BTreeMap<String, TaxonomyEntry>,
) -> Result<(), RepoError> {
    let held_keys: BTreeSet<&str> = held.iter().map(|e| e.value.as_str()).collect();

    for entry in submitted.values() {
        let row = rounding_policy_taxonomy::ActiveModel {
            tenant_id: Set(tenant_id),
            value: Set(entry.value.as_str().to_owned()),
            display_name: Set(entry.display_name.clone()),
            state: Set(entry.state.as_str().to_owned()),
        };
        if held_keys.contains(entry.value.as_str()) {
            update_rounding_policy_entry(runner, scope, tenant_id, entry).await?;
        } else {
            let inserted = row.clone();
            rounding_policy_taxonomy::Entity::insert(inserted.clone())
                .secure()
                .scope_with_model(scope, &inserted)
                .map_err(|e| RepoError::Db(format!("scope pricing_rounding_policy_taxonomy: {e}")))?
                .exec(runner)
                .await
                .map(|_| ())
                .map_err(|e| {
                    contention_or_db(
                        &e,
                        "pricing_rounding_policy_taxonomy",
                        "insert pricing_rounding_policy_taxonomy",
                    )
                })?;
        }
    }
    // A value the caller omitted is **retired, never deleted** — the taxonomies'
    // rule: something published may still resolve against it, and a deletion
    // would make that reference dangle rather than merely unauthorable.
    for existing in held {
        if submitted.contains_key(existing.value.as_str()) {
            continue;
        }
        let retired = TaxonomyEntry {
            state: TaxonomyState::Retired,
            ..existing.clone()
        };
        update_rounding_policy_entry(runner, scope, tenant_id, &retired).await?;
    }
    Ok(())
}

/// [`update_customer_group_entry`]'s shape on this table.
async fn update_rounding_policy_entry(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    entry: &TaxonomyEntry,
) -> Result<(), RepoError> {
    rounding_policy_taxonomy::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            rounding_policy_taxonomy::Column::DisplayName,
            sea_orm::sea_query::Expr::value(entry.display_name.clone()),
        )
        .col_expr(
            rounding_policy_taxonomy::Column::State,
            sea_orm::sea_query::Expr::value(entry.state.as_str().to_owned()),
        )
        .filter(
            Condition::all()
                .add(rounding_policy_taxonomy::Column::TenantId.eq(tenant_id))
                .add(rounding_policy_taxonomy::Column::Value.eq(entry.value.as_str().to_owned())),
        )
        .exec(runner)
        .await
        .map(|_| ())
        .map_err(|e| RepoError::Db(format!("update pricing_rounding_policy_taxonomy: {e}")))
}

/// The GL-code vocabulary's `PUT`, inside its transaction (D-356).
///
/// [`apply_replace_rounding_policy`]'s shape exactly: premise tested here and
/// only here against the same `held` the write works from, retirement guarded
/// per value, and nothing written when either refuses.
async fn apply_replace_gl_code(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    entries: Vec<TaxonomyEntry>,
    asserted: &PolicyTag,
    stamp: AuditStamp,
) -> Result<Replaced, RepoError> {
    let held = list_gl_code_on(runner, scope, tenant_id).await?;

    if gl_code_tag_of(&held) != *asserted {
        return Ok(Replaced {
            entries: held,
            report: ValidationReport::default(),
            stale: true,
        });
    }

    let submitted: BTreeMap<String, TaxonomyEntry> = entries
        .into_iter()
        .map(|entry| (entry.value.as_str().to_owned(), entry))
        .collect();

    let mut report = ValidationReport::default();
    for existing in &held {
        let key = existing.value.as_str();
        let retiring = submitted
            .get(key)
            .is_none_or(|entry| entry.state == TaxonomyState::Retired);
        if !retiring || existing.state == TaxonomyState::Retired {
            continue;
        }
        let revisions = references_to_gl_code(runner, tenant_id, &existing.value).await?;
        report.absorb(check_gl_code_retirable(&existing.value, revisions));
    }

    if !report.is_publishable() {
        return Ok(Replaced {
            entries: held,
            report,
            stale: false,
        });
    }

    write_gl_code_set(runner, scope, tenant_id, &held, &submitted).await?;
    let now = list_gl_code_on(runner, scope, tenant_id).await?;
    record_single_table_mutation(runner, scope, tenant_id, GL_CODE_RESOURCE, stamp).await?;
    Ok(Replaced {
        entries: now,
        report,
        stale: false,
    })
}

/// [`write_rounding_policy_set`]'s shape on this table.
async fn write_gl_code_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    held: &[TaxonomyEntry],
    submitted: &BTreeMap<String, TaxonomyEntry>,
) -> Result<(), RepoError> {
    let held_keys: BTreeSet<&str> = held.iter().map(|e| e.value.as_str()).collect();

    for entry in submitted.values() {
        let row = gl_code_taxonomy::ActiveModel {
            tenant_id: Set(tenant_id),
            value: Set(entry.value.as_str().to_owned()),
            display_name: Set(entry.display_name.clone()),
            state: Set(entry.state.as_str().to_owned()),
        };
        if held_keys.contains(entry.value.as_str()) {
            update_gl_code_entry(runner, scope, tenant_id, entry).await?;
        } else {
            let inserted = row.clone();
            gl_code_taxonomy::Entity::insert(inserted.clone())
                .secure()
                .scope_with_model(scope, &inserted)
                .map_err(|e| RepoError::Db(format!("scope pricing_gl_code_taxonomy: {e}")))?
                .exec(runner)
                .await
                .map(|_| ())
                .map_err(|e| {
                    contention_or_db(
                        &e,
                        "pricing_gl_code_taxonomy",
                        "insert pricing_gl_code_taxonomy",
                    )
                })?;
        }
    }
    // A value the caller omitted is **retired, never deleted** — the taxonomies'
    // rule: a published descriptor set may still name it, and a deletion would
    // make that reference dangle rather than merely unauthorable.
    for existing in held {
        if submitted.contains_key(existing.value.as_str()) {
            continue;
        }
        let retired = TaxonomyEntry {
            state: TaxonomyState::Retired,
            ..existing.clone()
        };
        update_gl_code_entry(runner, scope, tenant_id, &retired).await?;
    }
    Ok(())
}

/// [`update_rounding_policy_entry`]'s shape on this table.
async fn update_gl_code_entry(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    entry: &TaxonomyEntry,
) -> Result<(), RepoError> {
    gl_code_taxonomy::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            gl_code_taxonomy::Column::DisplayName,
            sea_orm::sea_query::Expr::value(entry.display_name.clone()),
        )
        .col_expr(
            gl_code_taxonomy::Column::State,
            sea_orm::sea_query::Expr::value(entry.state.as_str().to_owned()),
        )
        .filter(
            Condition::all()
                .add(gl_code_taxonomy::Column::TenantId.eq(tenant_id))
                .add(gl_code_taxonomy::Column::Value.eq(entry.value.as_str().to_owned())),
        )
        .exec(runner)
        .await
        .map(|_| ())
        .map_err(|e| RepoError::Db(format!("update pricing_gl_code_taxonomy: {e}")))
}

async fn write_customer_group_set(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    held: &[TaxonomyEntry],
    submitted: &BTreeMap<String, TaxonomyEntry>,
) -> Result<(), RepoError> {
    let held_keys: BTreeSet<&str> = held.iter().map(|e| e.value.as_str()).collect();

    for entry in submitted.values() {
        if held_keys.contains(entry.value.as_str()) {
            update_customer_group_entry(runner, scope, tenant_id, entry).await?;
        } else {
            insert_customer_group_entry(runner, scope, tenant_id, entry).await?;
        }
    }
    for existing in held {
        if submitted.contains_key(existing.value.as_str()) {
            continue;
        }
        let retired = TaxonomyEntry {
            state: TaxonomyState::Retired,
            ..existing.clone()
        };
        update_customer_group_entry(runner, scope, tenant_id, &retired).await?;
    }
    Ok(())
}

async fn insert_customer_group_entry(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    entry: &TaxonomyEntry,
) -> Result<(), RepoError> {
    let row = customer_group_taxonomy::ActiveModel {
        tenant_id: Set(tenant_id),
        value: Set(entry.value.as_str().to_owned()),
        display_name: Set(entry.display_name.clone()),
        state: Set(entry.state.as_str().to_owned()),
    };
    customer_group_taxonomy::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(|e| RepoError::Db(format!("scope pricing_customer_group_taxonomy: {e}")))?
        .exec(runner)
        .await
        .map(|_| ())
        .map_err(|e| {
            contention_or_db(
                &e,
                "pricing_customer_group_taxonomy",
                "insert pricing_customer_group_taxonomy",
            )
        })
}

async fn update_customer_group_entry(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    entry: &TaxonomyEntry,
) -> Result<(), RepoError> {
    customer_group_taxonomy::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            customer_group_taxonomy::Column::DisplayName,
            sea_orm::sea_query::Expr::value(entry.display_name.clone()),
        )
        .col_expr(
            customer_group_taxonomy::Column::State,
            sea_orm::sea_query::Expr::value(entry.state.as_str().to_owned()),
        )
        .filter(
            Condition::all()
                .add(customer_group_taxonomy::Column::TenantId.eq(tenant_id))
                .add(customer_group_taxonomy::Column::Value.eq(entry.value.as_str().to_owned())),
        )
        .exec(runner)
        .await
        .map(|_| ())
        .map_err(|e| RepoError::Db(format!("update pricing_customer_group_taxonomy: {e}")))
}

/// The declared rounding vocabulary, ordered by value.
async fn list_rounding_policy_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Vec<TaxonomyEntry>, RepoError> {
    let rows = rounding_policy_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(rounding_policy_taxonomy::Column::TenantId.eq(tenant_id)))
        .order_by(rounding_policy_taxonomy::Column::Value, sea_orm::Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_rounding_policy_taxonomy: {e}")))?;

    rows.into_iter()
        .map(|row| {
            Ok(TaxonomyEntry {
                value: ScopeValue::new(&row.value).ok_or_else(|| {
                    RepoError::CorruptRow("pricing_rounding_policy_taxonomy.value is blank".into())
                })?,
                display_name: row.display_name,
                state: TaxonomyState::parse(&row.state).ok_or_else(|| {
                    RepoError::CorruptRow(format!(
                        "pricing_rounding_policy_taxonomy.state `{}`",
                        row.state
                    ))
                })?,
                tax: None,
            })
        })
        .collect()
}

/// The declared GL-code vocabulary, ordered by value.
async fn list_gl_code_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Vec<TaxonomyEntry>, RepoError> {
    let rows = gl_code_taxonomy::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(gl_code_taxonomy::Column::TenantId.eq(tenant_id)))
        .order_by(gl_code_taxonomy::Column::Value, sea_orm::Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_gl_code_taxonomy: {e}")))?;

    rows.into_iter()
        .map(|row| {
            Ok(TaxonomyEntry {
                value: ScopeValue::new(&row.value).ok_or_else(|| {
                    RepoError::CorruptRow("pricing_gl_code_taxonomy.value is blank".into())
                })?,
                display_name: row.display_name,
                state: TaxonomyState::parse(&row.state).ok_or_else(|| {
                    RepoError::CorruptRow(format!("pricing_gl_code_taxonomy.state `{}`", row.state))
                })?,
                tax: None,
            })
        })
        .collect()
}

/// How many **published** price rows name this rounding reference, plus whether
/// the tenant default does (D-334).
///
/// The retirement guard's operand. A value nothing references may be retired; a
/// value a live row resolves against may not, which is `inst-tx-mutation`'s rule
/// on every taxonomy in this gear. Draft rows are deliberately **not** counted:
/// a draft is being authored and its author can change the reference, while a
/// published row's is frozen into a `CatalogVersion` a consumer resolves.
///
/// # Both rounding columns are counted, and the second is the one that was missed
///
/// A published row names its policy in **either** of two columns since
/// `pricing_price` split them: `rounding_policy_ref` is what the
/// author typed, and `resolved_rounding_policy` is what the publish froze —
/// `coalesce(authored, tenant default)`. A row that leaned on the tenant default
/// carries NULL in the authored column and the value in the resolved one, so a
/// count over the authored column alone saw **nothing** the moment the tenant
/// default moved on: `default_names_it` went false, the row count read zero, and
/// the guard let an operator retire a value that immutable published rows still
/// resolve against — the dangling reference
/// [`check_rounding_policy_retirable`]'s own message exists to prevent. Before
/// the column split the same query counted those rows correctly, which is what
/// made it a silent regression rather than a gap (
/// `sqlite_taxonomy_repo::a_value_only_the_frozen_resolution_names_still_blocks_retirement`
/// pins it).
async fn references_to_rounding_policy(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    value: &ScopeValue,
) -> Result<(u64, bool), RepoError> {
    let rows = price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(
                    Condition::any()
                        .add(price::Column::RoundingPolicyRef.eq(value.as_str()))
                        .add(price::Column::ResolvedRoundingPolicy.eq(value.as_str())),
                )
                .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_price: {e}")))?;

    let default_names_it = policy_object::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(policy_object::Column::TenantId.eq(tenant_id))
                .add(policy_object::Column::DefaultRoundingPolicyRef.eq(value.as_str())),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_policy_object: {e}")))?
        > 0;

    Ok((rows, default_names_it))
}

/// `TAXONOMY_VALUE_IN_USE`, when something still resolves against the value.
///
/// The two references are named separately for `check_customer_group_retirable`'s
/// reason: an operator whose only reference is the tenant default should not be
/// sent to audit price rows that were never the problem.
fn check_rounding_policy_retirable(
    value: &ScopeValue,
    published_rows: u64,
    default_names_it: bool,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    if published_rows == 0 && !default_names_it {
        return report;
    }
    let mut named = Vec::with_capacity(2);
    if published_rows > 0 {
        named.push(format!("{published_rows} published price row(s) name it"));
    }
    if default_names_it {
        named.push("the tenant default names it".to_owned());
    }
    report.violate(
        TAXONOMY_VALUE_IN_USE,
        value.as_str().to_owned(),
        format!(
            "rounding policy `{}` cannot be retired: {}. Re-point them first - a retired value \
             stops being authorable and what already resolves against it would have no declared \
             vocabulary entry",
            value.as_str(),
            named.join(" and ")
        ),
    );
    report
}

/// The rounding vocabulary's resource name.
///
/// `PUT /bss-pricing/v1/config/rounding-policies`' own last segment. Written once
/// because **two** representations are derived from it — the entity tag
/// [`rounding_policy_tag_of`] hashes and the `taxonomy/…` audit ref
/// [`record_single_table_mutation`] writes — and a taxonomy spelled two ways in
/// those two places is a tag that names one resource and a trail that names
/// another. The customer-group vocabulary's name comes from
/// [`ScopeClass::CustomerGroup`] for the same reason, which is why it has no
/// constant of its own here.
const ROUNDING_POLICY_RESOURCE: &str = "rounding-policies";

/// The rounding vocabulary's tag — [`customer_group_tag_of`]'s shape over its
/// own resource name.
#[must_use]
pub fn rounding_policy_tag_of(entries: &[TaxonomyEntry]) -> PolicyTag {
    PolicyTag::of_taxonomy(
        ROUNDING_POLICY_RESOURCE,
        entries.iter().map(|entry| TaxonomyTagEntry {
            value: entry.value.as_str(),
            state: entry.state.as_str(),
            display_name: entry.display_name.as_str(),
            tax_category: None,
            tax_rate_present: false,
        }),
    )
}

/// How many **published** plan revisions name this GL code in their billing
/// descriptor set (D-356).
///
/// The retirement guard's operand, [`references_to_rounding_policy`]'s
/// arrangement on the descriptor plane. Draft revisions are deliberately **not**
/// counted, for the same reason: a draft is being authored and its author can
/// change the code, while a published revision's descriptor set is frozen into a
/// `CatalogVersion` an ERP posts against — the dangling reference
/// [`check_gl_code_retirable`]'s message exists to prevent.
///
/// `pricing_plan_descriptor_set` carries no lifecycle of its own (its entity doc
/// says why), so the revisions that name the code are read first and the
/// **parent** `pricing_plan` rows decide which of them are published. Two reads
/// rather than a join because the scoping wrapper exposes no join, and the first
/// read is bounded by how many revisions name one code — not by the tenant's
/// catalog.
async fn references_to_gl_code(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    value: &ScopeValue,
) -> Result<u64, RepoError> {
    let scope = &AccessScope::for_tenant(tenant_id);
    let naming = plan_descriptor_set::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_descriptor_set::Column::TenantId.eq(tenant_id))
                .add(plan_descriptor_set::Column::GlCode.eq(value.as_str())),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pricing_plan_descriptor_set: {e}")))?;
    if naming.is_empty() {
        return Ok(0);
    }

    let mut any_named = Condition::any();
    for row in &naming {
        any_named = any_named.add(
            Condition::all()
                .add(plan::Column::PlanId.eq(row.plan_id))
                .add(plan::Column::Revision.eq(row.plan_revision)),
        );
    }
    plan::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan::Column::TenantId.eq(tenant_id))
                .add(plan::Column::LifecycleState.eq(LifecycleState::Published.as_str()))
                .add(any_named),
        )
        .count(runner)
        .await
        .map_err(|e| RepoError::Db(format!("count pricing_plan: {e}")))
}

/// `TAXONOMY_VALUE_IN_USE`, when a published descriptor set still names the code.
///
/// [`check_rounding_policy_retirable`]'s message on the descriptor plane, with
/// one reference kind rather than two: a GL code has no tenant default to fall
/// back to.
fn check_gl_code_retirable(value: &ScopeValue, published_revisions: u64) -> ValidationReport {
    let mut report = ValidationReport::default();
    if published_revisions == 0 {
        return report;
    }
    report.violate(
        TAXONOMY_VALUE_IN_USE,
        value.as_str().to_owned(),
        format!(
            "GL code `{}` cannot be retired: {published_revisions} published plan revision(s) \
             name it in their billing descriptor set. Re-point them first - a retired value \
             stops being authorable and what already resolves against it would have no declared \
             vocabulary entry",
            value.as_str(),
        ),
    );
    report
}

/// The GL-code vocabulary's resource name.
///
/// `PUT /bss-pricing/v1/config/gl-codes`' own last segment, written once for
/// [`ROUNDING_POLICY_RESOURCE`]'s reason: the entity tag [`gl_code_tag_of`]
/// hashes it and the `taxonomy/…` audit ref [`record_single_table_mutation`]
/// writes it, and a taxonomy spelled two ways there is a tag that names one
/// resource and a trail that names another.
const GL_CODE_RESOURCE: &str = "gl-codes";

/// The GL-code vocabulary's tag — [`rounding_policy_tag_of`]'s shape over its
/// own resource name.
#[must_use]
pub fn gl_code_tag_of(entries: &[TaxonomyEntry]) -> PolicyTag {
    PolicyTag::of_taxonomy(
        GL_CODE_RESOURCE,
        entries.iter().map(|entry| TaxonomyTagEntry {
            value: entry.value.as_str(),
            state: entry.state.as_str(),
            display_name: entry.display_name.as_str(),
            tax_category: None,
            tax_rate_present: false,
        }),
    )
}

/// The entity tag of the customer-group taxonomy's representation —
/// [`tag_of`]'s sibling, over one label rather than a [`TaxonomyClass`].
///
/// Public so `api::rest::customer_groups` can render and compare it exactly as
/// `api::rest::taxonomies` does through [`tag_of`]: the `GET` renders it and the
/// `PUT`'s precondition is compared against it inside the write transaction, so
/// two constructions would be two answers to "what does this taxonomy hash to".
#[must_use]
pub fn customer_group_tag_of(entries: &[TaxonomyEntry]) -> PolicyTag {
    PolicyTag::of_taxonomy(
        ScopeClass::CustomerGroup.as_str(),
        entries.iter().map(|entry| TaxonomyTagEntry {
            value: entry.value.as_str(),
            state: entry.state.as_str(),
            display_name: entry.display_name.as_str(),
            tax_category: None,
            tax_rate_present: false,
        }),
    )
}

/// `inst-tx-mutation`'s audit half for the two **single-table** vocabularies —
/// [`record_mutation`]'s sibling, over a resource name rather than a
/// [`TaxonomyClass`].
///
/// **The resource is a parameter, and that is the whole of the fix here.** This
/// function was `record_customer_group_mutation` and hard-coded
/// `taxonomy/customer_group`, while [`apply_replace_rounding_policy`] called it
/// too — so a rounding-vocabulary `PUT` was recorded as a customer-group taxonomy
/// mutation, and one string made both trails wrong at once: an auditor filtering
/// the per-tenant policy segment found **no record at all** under the D-334
/// vocabulary, and customer-group entries for acts that never touched that
/// taxonomy. Nothing pinned the ref, so nothing caught it.
///
/// The names passed in are the two routes' **own** resource names, which is also
/// what [`customer_group_tag_of`] and [`rounding_policy_tag_of`] hash under, so
/// the audit ref and the entity tag cannot come to spell one taxonomy two ways.
async fn record_single_table_mutation(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    resource: &str,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::policy_chain(),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action: AuditAction::Update,
            subject_kind: AuditSubjectKind::Policy,
            subject_ref: format!("taxonomy/{resource}"),
            before_state: None,
            after_state: None,
            approval_ref: None,
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
}
