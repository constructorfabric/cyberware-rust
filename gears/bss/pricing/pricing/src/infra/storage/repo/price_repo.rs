//! Repository for **draft** price rows and the tier bands that belong to them
//! (`pricing_price` + `pricing_price_tier_band`, `design/03-price-structure.md`
//! §5, `design/01-foundation.md` §4.1 / §4.3).
//!
//! A price row is not one table. Everything single-valued about it — the
//! amount, the package block, the evaluation policy — sits on the row, and only
//! the band set, being many-per-row, has a table of its own. So every operation
//! here writes **both** tables or neither, and the reasons are physical rather
//! than tidy:
//!
//! **A row and its bands land together.** Bands inserted before their parent are
//! rejected outright by the band table's structural-exclusivity trigger, which
//! reads the parent's `model_kind`; bands landing after a committed parent leave
//! a window in which a `graduated` row has no geometry at all, and the Slice-3
//! rules would judge it as a tiered row that prices nothing. One transaction
//! removes both states.
//!
//! **The bands go first on the way out.** The foreign key declares the default
//! `NO ACTION` on both backends — nothing cascades and nothing nulls — and sqlx
//! turns `foreign_keys` ON for `SQLite`, so deleting a draft row that still has
//! bands fails with an opaque constraint error.
//! `ON DELETE CASCADE` is *not* the fix: Postgres fires the child's row triggers
//! on a cascade and `SQLite` fires them only under `recursive_triggers`, so
//! cascading would make the append-only guard mean two different things on the
//! two backends — the one divergence the mirrored schema exists to prevent.
//! Deleting the children explicitly, inside the transaction, is what keeps the
//! guard identical.
//!
//! **A band's `tenant_id` comes from its parent row, never from the request.**
//! The foreign key covers `price_id` alone, so nothing in the schema stops a
//! band carrying a different tenant from the row it points at — and under
//! `SecureORM` such a band is invisible to its true owner while still joined by
//! `price_id` to their price. The value is therefore copied from the row this
//! repository just wrote, and no caller is offered the choice.
//!
//! **There is deliberately no band-level mutation.** Bands carry no entity tag
//! of their own, so a band edit that did not move the parent's `row_version`
//! would let two authors editing different bands of one draft both satisfy
//! `If-Match` and silently interleave — the outcome
//! `cpt-cf-bss-pricing-fr-concurrent-edit` forbids. Bands are written only
//! through [`PriceRepo::create_draft`], [`PriceRepo::update_draft`] and
//! [`PriceRepo::delete_draft`], and the band set is replaced wholesale inside
//! the same transaction as the row's compare-and-swap.
//!
//! **The compare-and-swap is one statement, and a failed swap gets three
//! answers.** Both are exactly `PlanRepo`'s shape and for exactly its reasons:
//! the `row_version + 1` bump rides inside the UPDATE that matches on the
//! version the caller read, and `rows_affected == 0` is resolved by one extra
//! read into [`RepoError::NotFound`], [`RepoError::NotDraft`] or
//! [`RepoError::StaleRowVersion`], because only one of those three is worth
//! retrying.
//!
//! **The draft-only predicate is enforced here as well as by the triggers.**
//! `pricing_price_tier_band` rejects INSERT, UPDATE *and* DELETE against a
//! non-draft parent. This repository only ever touches bands of a draft parent,
//! and does not lean on the trigger for the caller-facing refusal: the trigger's
//! answer is a raw database error carrying no state, and a caller that edited a
//! published row deserves to be told that, not that the store is broken. What
//! guarantees it is one read taken inside the transaction, before the first band
//! statement — [`mutable_draft`] — because both mutating paths now reach the
//! band table *before* the compare-and-swap has answered. The swap is still the
//! authority; the read only decides which sentence a doomed call gets.
//!
//! **Bands come back ordered by `from_qty`, ascending.** That is a read-side
//! guarantee and it is load-bearing. The band table is keyed
//! `(price_id, from_qty)` and carries no ordinal, so authoring order does not
//! survive a round trip; `inst-tb-order` was amended for exactly this after a
//! rule judged band geometry by the author's in-memory ordering and therefore
//! reached one verdict at save and another at publish, from the identical rule.
//!
//! **`PriceRow::charge_kind` is the key's, not a second column.** The row type
//! carries the axis because half the shape rules are a function of it, and
//! `price_row.rs` says as much — it is "not authored on the row itself". So the
//! store keeps exactly one copy, the scope key's, and a record read back always
//! agrees with its own key; a caller that hands the two different answers is
//! given the key's rather than a refusal, because there is no second value for
//! them to have disagreed about.
//!
//! **Superseding a published scope key is a different door, in this file.** It is
//! the D-88 supersession unit — successor row, predecessor-window shorten and
//! successor-window schedule composed gap-free in one commit, setting
//! `supersedes_price_id` — and its row half is
//! [`insert_successor_draft_on`], whose occupancy precondition is the mirror
//! image of [`PriceRepo::create_draft`]'s: a published occupant is what it
//! requires rather than what it refuses (**D-195**). The row half of the commit is
//! below (`commit_supersession_rows`), the compose is `domain::supersession` and the
//! cross-plane commit is `infra::supersession`.
//!
//! **The door is mounted.** `SupersessionService` is built in [`crate::module`]'s
//! `init`, [`crate::api::rest::supersessions::router`] is merged into the gear's
//! tree, and `POST /bss-pricing/v1/plans/{planId}/supersessions` reaches
//! `commit_supersession_rows`. Neither of this file's two standing refusals turns
//! on that, and neither is waiting for it: `publish_rows` leaves a successor's key
//! collision to the index because `inst-su-commit`'s ordering makes the collision
//! unreachable on this path -- `commit_supersession_rows` supersedes the
//! predecessor before it publishes the successor, in the same transaction -- and
//! [`PriceRepo::create_draft`]'s `DUPLICATE_SCOPE_KEY` narrowing is owed to the
//! surface layer because recognizing a driver unique-violation means knowing the
//! backend, which a repository deliberately does not.
//!
//! `create_draft` is unchanged and must still not be reached for on an occupied
//! key: the authoring door refuses a key an occupied published row holds rather
//! than taking it over, which is `inst-pr-return`'s save-time duplicate check and
//! is what keeps one draft per key the most the two doors admit between them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, Order, QuerySelect, Value};
use serde_json::{Value as JsonValue, json};
use time::OffsetDateTime;
use toolkit_db::odata::sea_orm_filter::paginate_odata;
use toolkit_db::secure::{
    AccessScope, DBRunner, DbConn, DbTx, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
    SecureUpdateExt, TxError,
};
use toolkit_db::{DBProvider, DbError};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use bss_pricing_sdk::odata::{HistoryFilterField, PlanPriceFilterField};

use crate::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind, subject_state};
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{AnchorDay, BillingAnchorPolicy, ProrationBasis, ProrationContract};
use crate::domain::error::DomainError;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::price_record::{PriceContent, PriceRecord, canonical_usage_line};
use crate::domain::price_row::{
    AggregationFunction, AggregationGranularity, BandTop, BillingGranularity, IncludedAllowance,
    MinQtyUsageFallback, ModelKind, PriceRow, QuantitySource, ReservationFlavor, RolloverPolicy,
    TierAggregationWindow, TierBand, TierQualificationWindow, model_kind_wire,
};
use crate::domain::projection::PROJECTED_ROW_STATES;
use crate::domain::repricing::RunSelector;
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, PriceOverlay,
    Region, ScopeKey, ScopeKeyParts,
};
use crate::domain::tax_display::RegionTaxReadiness;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::{price, price_tier_band, price_window};
use crate::infra::storage::odata_mapping::{
    HistoryODataMapper, LIST_LIMIT_CFG, OdataPageError, PlanPriceODataMapper,
    filter_mentions_field, map_odata_err, query_with_default_order,
};
use crate::infra::storage::repo::check_authored_instant;
use crate::infra::storage::repo::outbox_repo::{NewOutboxEvent, PriceCreatedPayload};
use crate::infra::storage::repo::{NewAuditEntry, audit_repo, outbox_repo};

/// The noun the authoring refusals name, so one subject word reaches the wire
/// from every method here.
const SUBJECT: &str = "price row";

// ---------------------------------------------------------------------------
// The stored token sets.
//
// Each list is the inverse of an `as_str()` the domain already owns, written
// out here rather than added to the domain because parsing a column was a
// storage concern and nothing above this boundary had a token to read. A
// variant added to one of these enums without a line here reads back as
// `CorruptRow` — loud, and never as a silently different value.
//
// **They are `pub` now, and the REST surface reads through them rather than
// declaring a second set.** The authoring routes parse the same tokens off a
// request that these parse off a column, and two tables would be two answers to
// "is `partner` a legal overlay" — free to disagree the day a variant lands in
// one of them. The lists stay here rather than moving onto the domain enums
// because moving them would put a *parse* on a type whose job is to render, and
// because these are the tables whose completeness the `CorruptRow` path already
// depends on.
// ---------------------------------------------------------------------------

/// Axis 4. One value today; the list exists so a stored `partner` overlay is
/// refused rather than quietly read as `base`.
pub const PRICE_OVERLAYS: &[PriceOverlay] = &[PriceOverlay::Base];
/// Axis 6. All **three** normative classes: a list that carried only the two
/// the cutover machinery uses would read a stored `new_subscriptions_only` row
/// — the class PRD AC #59 and W3 both name — as an invariant breach.
pub const PRICE_ELIGIBILITIES: &[PriceEligibility] = &[
    PriceEligibility::AllSubscriptions,
    PriceEligibility::NewSubscriptionsOnly,
    PriceEligibility::ExistingGrandfathered,
];
/// Axis 7.
pub const CHARGE_KINDS: &[ChargeKind] = &[
    ChargeKind::Recurring,
    ChargeKind::Usage,
    ChargeKind::OneTime,
    ChargeKind::OneTimeSetup,
];
/// Where a non-usage `per_unit` row's quantity comes from.
pub const QUANTITY_SOURCES: &[QuantitySource] = &[
    QuantitySource::SubscriptionSeatCount,
    QuantitySource::Manual,
];
/// The billable-unit quantization.
pub const BILLING_GRANULARITIES: &[BillingGranularity] = &[
    BillingGranularity::PerSecond,
    BillingGranularity::PerMinute,
    BillingGranularity::PerHour,
    BillingGranularity::PerDay,
    BillingGranularity::WholeUnit,
];
/// The counter-reset window.
/// One roster, shared with the sentence a refusal offers: the set the wire
/// parser accepts and the set a remediation names are the same list. Two
/// hand-written copies of it are free to disagree by a member, and the direction
/// that costs an author is the offer being the shorter one.
pub const TIER_AGGREGATION_WINDOWS: &[TierAggregationWindow] = TierAggregationWindow::ALL;
/// The D-40 tier-qualification window.
pub const TIER_QUALIFICATION_WINDOWS: &[TierQualificationWindow] = &[
    TierQualificationWindow::Current,
    TierQualificationWindow::TrailingPeriod,
];
/// What a reservation reserves (`inst-rv-attrs`, S10).
pub const RESERVATION_FLAVORS: &[ReservationFlavor] =
    &[ReservationFlavor::Consumption, ReservationFlavor::Capacity];
/// What a `usage` floor does with below-floor usage (`inst-ft-fallback`, S10).
pub const MIN_QTY_USAGE_FALLBACKS: &[MinQtyUsageFallback] = &[MinQtyUsageFallback::Exception];
/// How the in-window `Q` is derived (D-44).
pub const AGGREGATION_FUNCTIONS: &[AggregationFunction] = &[
    AggregationFunction::Sum,
    AggregationFunction::Peak,
    AggregationFunction::TimeWeighted,
];
/// The granule a non-`sum` window is cut into (D-44).
pub const AGGREGATION_GRANULARITIES: &[AggregationGranularity] =
    &[AggregationGranularity::Hour, AggregationGranularity::Day];
/// What happens to an unused allowance (D-45).
pub const ROLLOVER_POLICIES: &[RolloverPolicy] = &[RolloverPolicy::None, RolloverPolicy::Carry];

/// Everything a **new** draft price row needs.
///
/// `price_id` and `created_at_utc` are caller-supplied for the reasons
/// [`super::NewPlanDraft`] states: the surface that mints an id has to return it
/// before the row is durable, and the catalog never self-originates a row, so
/// the authoring instant belongs to the request rather than to whichever
/// database node evaluated `now()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPriceDraft {
    /// The row being created.
    pub price_id: Uuid,
    /// The ten axes it is filed under.
    pub scope_key: ScopeKey,
    /// What the row says.
    pub content: PriceContent,
    /// Pseudonymous principal id of the authoring actor.
    pub created_by: Uuid,
    /// When the request was authored, UTC.
    pub created_at_utc: OffsetDateTime,
    /// The causing request's correlation id (D-178).
    ///
    /// The third field of the [`AuditStamp`](crate::domain::audit::AuditStamp)
    /// this path writes its record under, and the only one the draft did not
    /// already carry: `created_by` is the actor and `created_at_utc` is the
    /// instant. Carried on the draft rather than as a second argument so the
    /// create cannot be called with a correlation belonging to another request.
    pub correlation_id: Uuid,
}

/// Where a commit-ordered walk over the tenant's price history resumes.
///
/// **A pair, because neither column alone is a total order.**
/// `created_at_utc` ties for every pair of rows one request authored together —
/// a publish unit, a clone, a repricing run's plan transaction all write several
/// rows at one instant — so a walk keyed on it alone would either skip the rest
/// of a tied group or return it twice. `price_id` alone is a total order but is
/// not commit order, and D-125 asks history for commit order specifically. The
/// tie-break is `price_id` because it is the only other column of the row that
/// is unique.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryPosition {
    /// The authoring instant of the row the previous page ended at.
    pub authored_at: OffsetDateTime,
    /// That row's id — the tie-break within one instant.
    pub price_id: Uuid,
}

impl HistoryPosition {
    /// The position a page's last record leaves the walk at.
    ///
    /// Taken from the record the store handed back rather than from anything
    /// the caller holds, so the instant in the token is the one the column
    /// actually compares against on the next page.
    #[must_use]
    pub const fn of(record: &PriceRecord) -> Self {
        Self {
            authored_at: record.created_at_utc,
            price_id: record.price_id,
        }
    }
}

/// `SeaORM`-backed repository over draft price rows and their tier bands.
#[derive(Clone)]
pub struct PriceRepo {
    db: DBProvider<DbError>,
}

impl PriceRepo {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Create a price row in `draft` at `row_version = 0`, with its band set,
    /// in one transaction.
    ///
    /// The canonical scope key is checked for an occupant first. `inst-pr-return`
    /// (D-21) puts scope-key duplication among the **row-local** checks that run
    /// at save *and* re-run at publish, and the check has to happen here because
    /// the database's own `uq_pricing_price_scope_key_current` is partial over
    /// `lifecycle_state = 'published'` — it cannot see a second **draft** on one
    /// key, which is the ambiguity publish would fail on, discovered a round trip
    /// earlier.
    ///
    /// This is not the way to reprice an occupied key. That is the D-88
    /// supersession unit, whose row half is [`insert_successor_draft_on`] — a door
    /// of its own precisely so this one keeps refusing (D-195); see the module doc.
    ///
    /// # The race, and why its loser is not told `DUPLICATE_SCOPE_KEY`
    ///
    /// The check above is a **read**, so two concurrent creators can both pass
    /// it. What decides the winner is `uq_pricing_price_scope_key_draft`, the
    /// partial `UNIQUE` over `draft` — added for exactly this, because the
    /// published index is partial over `published` and cannot see a draft at
    /// all. The loser's INSERT therefore comes back as a driver error and
    /// reaches the caller as [`RepoError::Db`], i.e. a 500, where the identical
    /// collision caught by the check is a 409.
    ///
    /// That narrowing is **owed to the surface layer and is not paid here**.
    /// Recognizing the violation means introspecting a backend-specific error
    /// class, which is precisely the coupling [`RepoError`]'s own doc refuses —
    /// a variant per SQL error class would put the database's vocabulary into
    /// the gear's error surface — and the constraint name has to be mapped once
    /// for both backends, which only a layer that knows the backend can do.
    /// [`super::PlanRepo::open_revision`] carries the identical shape and the
    /// identical debt against `uq_pricing_plan_open_draft`, so the two read the
    /// same way.
    ///
    /// # Errors
    /// [`RepoError::DuplicateScopeKey`] naming the key and the draft or
    /// published row already on it;
    /// [`RepoError::GrandfatherHorizonOffClass`] when a grandfathering horizon
    /// is authored on a key that is not an `existing_grandfathered` generation;
    /// [`RepoError::ValueOutOfRange`] when an
    /// authored quantity is outside the range its column holds;
    /// [`RepoError::Db`] on a scope or storage failure — which **includes
    /// losing the race above**, and the band table's own refusal of bands on a
    /// kind that may not carry them; [`RepoError::CorruptRow`] only for a
    /// `row_version` that cannot be stored, which a created draft's `0` never
    /// is.
    pub async fn create_draft(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        draft: NewPriceDraft,
    ) -> Result<PriceRecord, RepoError> {
        // Rendered and value-checked before the transaction opens: a quantity no
        // column can hold, or a horizon off its class, is the caller's mistake,
        // and there is no reason to hold a transaction open to find it. That is
        // also why the split below is `prepare_draft` + `insert_prepared` rather
        // than one runner-taking body — `create_draft_on` runs both, which the
        // seam needs, and this path keeps the refusal ahead of the BEGIN.
        let prepared = prepare_draft(tenant_id, draft)?;

        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PriceRecord, RepoError, _>(move |txn| {
                Box::pin(async move { insert_prepared(txn, &scope, tenant_id, prepared).await })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Read one price row and its bands.
    ///
    /// Bands come back ordered by `from_qty` ascending — see the module doc; the
    /// order is a read-side guarantee, not a stored fact. SQL-level BOLA: a
    /// foreign tenant's row yields `None`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the stored row cannot be read as the
    /// domain value its columns are `CHECK`-constrained to hold.
    pub async fn find(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_id: Uuid,
    ) -> Result<Option<PriceRecord>, RepoError> {
        let conn = self.conn()?;
        load_record(&conn, scope, tenant_id, price_id).await
    }

    /// Read a plan's price rows in the given lifecycle states.
    ///
    /// Ordered by `price_id` ascending — stated here because the order is part
    /// of the contract and not an accident of the plan index. It is the same
    /// order on both backends: Postgres compares `uuid` byte-wise and `SQLite`
    /// compares the canonical lowercase hyphenated text, and hex digits sort in
    /// ASCII exactly as the bytes they spell do. D-125's cursor contract is a REST
    /// concern and it now **has** its list surface
    /// (`api::rest::prices::list_plan_prices`), which walks
    /// [`PriceRepo::list_for_plan_page`] over exactly this order. This method
    /// keeps its unbounded form for the callers that genuinely want the whole
    /// set - the publish assembler is one - and is not the paginated one.
    ///
    /// An **empty** `states` selects nothing. Reading it as "every state" would
    /// hand a caller whose filter computed to nothing the whole catalog, which
    /// is the one answer it certainly did not ask for.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
    /// value its columns are `CHECK`-constrained to hold.
    pub async fn list_for_plan(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        states: &[LifecycleState],
    ) -> Result<Vec<PriceRecord>, RepoError> {
        load_for_plan(&self.conn()?, scope, tenant_id, plan_id, states).await
    }

    /// One **page** of a plan's price rows, resuming strictly after `after`.
    ///
    /// [`PriceRepo::list_for_plan`] returns the whole `Vec` with no bound, and
    /// paginating that in memory would be a lie about D-125's contract: the
    /// promise is a walk that never skips or duplicates a row at or before the
    /// cursor, and a walk that read everything first would hold the whole plan
    /// in memory to serve a hundred rows of it. This is the keyset form, on the
    /// `price_id ASC` total order [`PriceRepo::list_for_plan`]'s doc already
    /// declares as part of its contract — the same order on both backends,
    /// because Postgres compares `uuid` byte-wise and `SQLite` compares the
    /// canonical lowercase hyphenated text, and hex digits sort in ASCII exactly
    /// as the bytes they spell do.
    ///
    /// **The caller asks for `limit + 1`** to learn whether another page exists
    /// without a second query; deciding `next_cursor` is the surface's, since
    /// only it knows the page size it promised.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
    /// value its columns are `CHECK`-constrained to hold.
    pub async fn list_for_plan_page(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        states: &[LifecycleState],
        after: Option<Uuid>,
        limit: u64,
    ) -> Result<Vec<PriceRecord>, RepoError> {
        load_page(
            &self.conn()?,
            scope,
            tenant_id,
            plan_id,
            states,
            after,
            Some(limit),
        )
        .await
    }

    /// One `OData` page of a plan's price rows. When `$filter` omits
    /// `lifecycle_state`, `default_states` is `ANDed` (authoring default).
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
    /// malformed `$filter` / `$orderby` / cursor.
    pub async fn list_for_plan_odata(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        plan_id: PlanId,
        query: &ODataQuery,
        default_states: &[LifecycleState],
    ) -> Result<Page<PriceRecord>, OdataPageError> {
        let conn = self.conn().map_err(OdataPageError::Repo)?;
        let mut filter = Condition::all()
            .add(price::Column::TenantId.eq(tenant_id))
            .add(price::Column::PlanId.eq(plan_id.get()));
        if !filter_mentions_field(
            query.filter.as_deref(),
            PlanPriceFilterField::LifecycleState,
        ) && !default_states.is_empty()
        {
            let tokens: Vec<&str> = default_states
                .iter()
                .copied()
                .map(LifecycleState::as_str)
                .collect();
            filter = filter.add(price::Column::LifecycleState.is_in(tokens));
        }
        let base_select = price::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(filter);
        let query = query_with_default_order(query, &[PlanPriceFilterField::PriceId]);
        let page = paginate_odata::<
            PlanPriceFilterField,
            PlanPriceODataMapper,
            price::Entity,
            price::Model,
            _,
            _,
        >(
            base_select,
            &conn,
            &query,
            ("price_id", SortDir::Asc),
            LIST_LIMIT_CFG,
            |m| m,
        )
        .await
        .map_err(map_odata_err)?;
        let items = hydrate_bands(&conn, scope, tenant_id, &page.items)
            .await
            .map_err(OdataPageError::Repo)?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// One `OData` page of the tenant's price history. Default order is
    /// `authored_at asc, price_id asc`.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
    /// malformed `$filter` / `$orderby` / cursor.
    pub async fn list_history_odata(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<PriceRecord>, OdataPageError> {
        let conn = self.conn().map_err(OdataPageError::Repo)?;
        let base_select = price::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(price::Column::TenantId.eq(tenant_id)));
        let query = query_with_default_order(
            query,
            &[HistoryFilterField::AuthoredAt, HistoryFilterField::PriceId],
        );
        let page = paginate_odata::<
            HistoryFilterField,
            HistoryODataMapper,
            price::Entity,
            price::Model,
            _,
            _,
        >(
            base_select,
            &conn,
            &query,
            ("price_id", SortDir::Asc),
            LIST_LIMIT_CFG,
            |m| m,
        )
        .await
        .map_err(map_odata_err)?;
        let items = hydrate_bands(&conn, scope, tenant_id, &page.items)
            .await
            .map_err(OdataPageError::Repo)?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// One **page** of the tenant's price history, in commit order.
    ///
    /// This is `inst-he-read`'s store half: the append-only `pricing_price` rows
    /// themselves, which is the whole of the history — `inst-he-nostore` adds no
    /// second store because the Foundation's immutability already is one. Every
    /// row of every lifecycle state is in scope, since a draft that was later
    /// published and a superseded predecessor are both things that happened.
    ///
    /// # Why this is not [`PriceRepo::list_for_plan_page`]
    ///
    /// That one is D-125's **catalog list** branch and this is D-125's
    /// **history** branch; the decision names the two separately and gives them
    /// different orders. `list_for_plan_page` walks `price_id ASC` — deterministic
    /// key order, which is what a catalog list wants and what makes its page
    /// stable — and is scoped to one plan and filtered to a state set. A history
    /// read wants neither: `price_id` is a v4 UUID and its order is unrelated to
    /// the order the rows were authored in, so a chronological read over it would
    /// be chronological in name only.
    ///
    /// # What "commit order" is here, and exactly where the guarantee stops
    ///
    /// The order is `(created_at_utc, price_id)` ascending, and `created_at_utc`
    /// is the **authoring instant carried on the request** ([`NewPriceDraft`]
    /// states why the column is caller-supplied), not the instant the row's
    /// transaction committed. The two agree for the walk's purpose in every
    /// ordinary case and can disagree in one: a row authored at `T1` whose
    /// transaction commits after a row authored at `T2 > T1` becomes visible
    /// *behind* a walk that has already passed `T1`, and is never returned.
    ///
    /// That is a real gap in D-125's "never skips a row at or before the cursor",
    /// and it is **stated rather than papered over**, because closing it needs a
    /// column this table does not have: a monotonic commit sequence assigned in
    /// the writing transaction, the way `pricing_price_window.mutation_seq` is
    /// assigned per act. Adding one is a migration. See `infra::history` for the
    /// same statement in the shape a caller meets it.
    ///
    /// **The caller asks for `limit + 1`** to learn whether another page exists
    /// without a second query, exactly as [`PriceRepo::list_for_plan_page`]'s
    /// caller does; deciding `next_cursor` is the surface's.
    ///
    /// There is deliberately **no unbounded form**. `list_for_plan` has one
    /// because a plan's row set is bounded by the plan; this one walks an
    /// append-only history of seven years and more, where the unbounded read is
    /// the query that takes the gear down.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
    /// value its columns are `CHECK`-constrained to hold.
    pub async fn list_history_page(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        after: Option<HistoryPosition>,
        limit: u64,
    ) -> Result<Vec<PriceRecord>, RepoError> {
        load_history_page(&self.conn()?, scope, tenant_id, after, limit).await
    }

    /// Replace an open draft's content, and its band set, under the caller's
    /// row version.
    ///
    /// One statement does the row: the content columns, the `row_version + 1`
    /// bump, and the conjunction that makes it a compare-and-swap — tenant, row,
    /// the submitted version, and `lifecycle_state = 'draft'`. The band set is
    /// replaced **wholesale** in the same transaction. It is replaced and not
    /// merged because bands are a set with no addressable members, and a partial
    /// band update has no geometry the Slice-3 rules could evaluate.
    ///
    /// **The band set goes first, and the row's own guard is the reason.**
    /// `trg_pricing_price_tier_band_parent_kind` refuses a row that still
    /// carries bands becoming a kind that carries none, so an edit turning a
    /// banded `graduated` row into a bandless `flat` one — an ordinary
    /// authoring move — would be refused by the store if the row were moved
    /// while the old bands were still standing. Nothing is lost by the order:
    /// the compare-and-swap is still the authority and a failure at any point
    /// rolls the whole transaction back, band set included. What the order costs
    /// is one read, taken so the frozen and absent cases are still answered by
    /// name: reaching the band DELETE with a published parent would have the
    /// band table's own trigger answer with a raw database error, and a caller
    /// who edited a published row deserves to be told that rather than that the
    /// store is broken. [`PriceRepo::delete_draft`] carries the identical read
    /// for the identical reason.
    ///
    /// The scope key is **not** part of the content and cannot be moved here.
    /// A row's key decides which duplicate it is, which supersession chain it
    /// joins and which window covers it; moving it would need the create-time
    /// duplicate check re-run against a different key, which is exactly what
    /// deleting the draft and authoring another one is.
    ///
    /// `content.row.charge_kind` is likewise **ignored**, and for the same
    /// reason [`PriceRepo::create_draft`] states: the axis is the key's, the
    /// copy on `PriceRow` is a convenience the shape rules read, and the store
    /// keeps exactly one copy. A caller that submits a different one is given
    /// the key's rather than a refusal, because there is no second stored value
    /// for them to have disagreed about — and the update path could not honour
    /// a different one anyway without moving the key it may not move.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such row is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::BulkRowLocked`] when an in-flight bulk
    /// run other than `on_behalf_of` holds the row, naming the run;
    /// [`RepoError::GrandfatherHorizonOffClass`] when a grandfathering horizon
    /// is submitted for a row whose key is not an `existing_grandfathered`
    /// generation; [`RepoError::ValueOutOfRange`] when an authored
    /// quantity is outside the range its column holds; [`RepoError::Db`] on a
    /// scope or storage failure, including the band table's refusal of a band
    /// set on a kind that may not carry one; [`RepoError::CorruptRow`] when the
    /// updated row reads back unusable.
    #[allow(
        clippy::too_many_arguments,
        reason = "the compare-and-swap's coordinate, the content, the D-135 stamp and the run \
                  the edit belongs to; the last is what tells an interactive edit from the \
                  bulk run that locked the row (`inst-bk-lock`)"
    )]
    pub async fn update_draft(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_id: Uuid,
        expected: RowVersion,
        content: PriceContent,
        stamp: AuditStamp,
        on_behalf_of: Option<Uuid>,
    ) -> Result<PriceRecord, RepoError> {
        let horizon = content.grandfather_until;
        // Before the row is even read: an instant the catalog cannot compare is
        // refused wherever it arrives, and this path can reject it without a
        // round trip.
        check_authored_instant("grandfatherUntil", horizon)?;
        let assignments = content_assignments(content_model(&content)?);
        let bands = band_models(tenant_id, price_id, &content.row.bands)?;
        // The **canonical** line, because that is what the stored side is: the
        // guard below compares the two, and comparing a raw submission against a
        // normalized column answered `USAGE_LINE_AXIS_MISMATCH` for a `PATCH`
        // that resubmitted the row's own line with a stray space — a refusal that
        // named a move nobody made and left the row unrepairable in place.
        let content_line = canonical_usage_line(&content.row);
        let Some(guard) = swap_guard(tenant_id, price_id, expected) else {
            let conn = self.conn()?;
            return Err(refuse(&conn, scope, tenant_id, price_id, expected).await);
        };

        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<PriceRecord, RepoError, _>(move |txn| {
                Box::pin(async move {
                    // `inst-bk-lock`, in the door rather than on a surface: a rule
                    // that lives on one authoring path is not a rule, and this is
                    // the second path onto the same rows.
                    refuse_if_locked_elsewhere(txn, &scope, tenant_id, price_id, on_behalf_of)
                        .await?;
                    let Some(row) =
                        mutable_draft(txn, &scope, tenant_id, price_id, expected).await?
                    else {
                        return Err(refuse(txn, &scope, tenant_id, price_id, expected).await);
                    };
                    // The row's class cannot move on an update, so the stored
                    // one is the one the submitted horizon has to pair with.
                    check_grandfather_horizon(horizon, read_eligibility(&row)?)?;
                    // **And neither can its usage line, since D-196 made the pair
                    // key axes.** This path used to rewrite `meter` and
                    // `dimension_key` as ordinary content columns, which — once
                    // they became axes — moved the row onto a *different*
                    // canonical scope key with no occupancy check of any kind: a
                    // `PATCH` could land a draft on a key another row already
                    // holds, and the only thing that would notice is the index,
                    // as a driver error. Found by this door's own round-trip test
                    // after clause (3) attached the pair to the loaded key.
                    //
                    // A refusal rather than `charge_kind`'s silent ignore, and
                    // this doc's own sentence is the reason: moving a key "is
                    // exactly what deleting the draft and authoring another one
                    // is", which is a remedy worth naming. `charge_kind` is
                    // ignored because the store keeps no second value to have
                    // disagreed about; here it keeps one and the caller can see
                    // it.
                    check_update_keeps_the_line(&row, &content_line)?;
                    delete_bands(txn, &scope, tenant_id, price_id).await?;
                    let mut update = price::Entity::update_many().secure().scope_with(&scope);
                    for (column, value) in assignments {
                        update = update.col_expr(column, Expr::value(value));
                    }
                    let result = update
                        .col_expr(
                            price::Column::RowVersion,
                            Expr::col(price::Column::RowVersion).add(1_i64),
                        )
                        .filter(guard)
                        .exec(txn)
                        .await
                        .map_err(|e| RepoError::Db(format!("update price draft: {e}")))?;
                    // The read above is not the guard — a concurrent publish can
                    // land between it and this statement — so a swap that
                    // matched nothing is still resolved, and the band delete it
                    // has already done is undone by the rollback.
                    if result.rows_affected == 0 {
                        return Err(refuse(txn, &scope, tenant_id, price_id, expected).await);
                    }
                    insert_bands(txn, &scope, bands).await?;
                    let updated = load_record(txn, &scope, tenant_id, price_id)
                        .await?
                        .ok_or_else(|| not_found(price_id))?;
                    record_price_mutation(
                        txn,
                        &scope,
                        tenant_id,
                        &updated,
                        AuditAction::Update,
                        Some(expected),
                        stamp,
                    )
                    .await?;
                    Ok(updated)
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// Delete an open draft row and its bands, under the caller's row version.
    ///
    /// Only a never-published `draft` is deletable (§4.3). The bands go first —
    /// the foreign key neither cascades nor nulls — and the row's
    /// compare-and-swap is still
    /// the authority: the read that precedes the band delete exists so a caller
    /// aiming at a published row is told so *before* the band table's own
    /// trigger answers with a raw database error, and so that nothing is
    /// deleted on the way to a refusal. Losing the row to a concurrent writer in
    /// the window between that read and the swap rolls the whole transaction
    /// back, band set included.
    ///
    /// # Errors
    /// [`RepoError::NotFound`] when no such row is visible to `scope`;
    /// [`RepoError::NotDraft`] when it is visible but frozen;
    /// [`RepoError::StaleRowVersion`] carrying both versions when the submitted
    /// one is not current; [`RepoError::BulkRowLocked`] when a bulk run **other
    /// than `on_behalf_of`** holds the row, naming the run;
    /// [`RepoError::PriceWindowScheduled`] when a window stands on the row, naming
    /// it ([`refuse_if_a_window_stands_on_it`]); [`RepoError::Db`] on a
    /// scope or storage failure; [`RepoError::CorruptRow`] when the row reads back
    /// unusable.
    ///
    /// # `on_behalf_of`, and why it is here before a caller needs it
    ///
    /// The run this delete belongs to, or `None` for an interactive one —
    /// [`PriceRepo::update_draft`]'s parameter, for
    /// [`refuse_if_locked_elsewhere`]'s reason: *what the lock excludes is
    /// somebody else*. It was hard-coded `None` here, which is fail-closed behind
    /// an absent lane rather than a live defect: neither bulk lane has a delete
    /// verb (`infra::bulk::commit_rows` has an edit arm and a create arm;
    /// `infra::repricing` stages successors and creates drafts), so the day one
    /// lands, a run's own delete of its own locked row would have been refused
    /// `BULK_ROW_LOCKED` naming the run itself — with every test green, because no
    /// test can hold a rule against a verb that does not exist. Threading the
    /// parameter now changes no behaviour and puts the rule in the door rather
    /// than leaving it for whoever writes that lane.
    ///
    /// **And a second refusal stands behind the guard, measured rather than
    /// assumed.** `fk_pricing_bulk_row_lock_price` references
    /// `pricing_price (price_id)` with no cascade (`pricing_bulk_row_lock`), so a held
    /// row cannot be deleted while its own lock row stands: past the guard the
    /// statement meets the foreign key and comes back as [`RepoError::Db`] — a 500,
    /// not a refusal an operator can act on. So the lane that lands a delete verb
    /// owes [`bulk_repo::release_locks`](super::bulk_repo::release_locks) for that
    /// row **inside the same transaction as the delete**, and both halves are
    /// pinned by `tests/sqlite_bulk_commit.rs::the_lock_stops_naming_the_run_that_holds_it_when_that_run_deletes`.
    /// Deliberately not done here: dropping another aggregate's lock row from a
    /// price repository is a design decision, not a repair.
    ///
    /// **A third refusal stands there too and is guarded rather than left to the
    /// key.** `fk_pricing_price_window_price` is the same shape as the lock's, and
    /// [`refuse_if_a_window_stands_on_it`] holds it — with the difference that no
    /// later act releases this one, so the guard exists to name the window rather
    /// than to tell a caller to retry. That function carries the measurement and
    /// the argument against cascading.
    pub async fn delete_draft(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        price_id: Uuid,
        expected: RowVersion,
        stamp: AuditStamp,
        on_behalf_of: Option<Uuid>,
    ) -> Result<(), RepoError> {
        let Some(guard) = swap_guard(tenant_id, price_id, expected) else {
            let conn = self.conn()?;
            return Err(refuse(&conn, scope, tenant_id, price_id, expected).await);
        };

        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<(), RepoError, _>(move |txn| {
                Box::pin(async move {
                    // `inst-bk-lock` in this door too. D-292 identified two
                    // authoring paths and guarded one — by its own standard, "a
                    // rule that lives on one authoring path is not a rule". A
                    // delete is not exempt: the foreign key refuses it anyway, so
                    // without this the operator gets a 500 saying the store is
                    // broken instead of a conflict naming the run.
                    //
                    // And it is `on_behalf_of` rather than a hard-coded `None`,
                    // for the guard's own reason: what the lock excludes is
                    // somebody else. See this method's doc.
                    refuse_if_locked_elsewhere(txn, &scope, tenant_id, price_id, on_behalf_of)
                        .await?;
                    let Some(_) = mutable_draft(txn, &scope, tenant_id, price_id, expected).await?
                    else {
                        return Err(refuse(txn, &scope, tenant_id, price_id, expected).await);
                    };
                    // Behind the draft check rather than in front of it: a
                    // published row a window stands on is refused for being
                    // published, and telling its author to go and cancel a
                    // window would send them to the wrong object.
                    refuse_if_a_window_stands_on_it(txn, &scope, tenant_id, price_id).await?;
                    // Read whole before the row goes, because the record names
                    // the plan whose chain it extends and that fact lives on the
                    // row being deleted.
                    let doomed = load_record(txn, &scope, tenant_id, price_id)
                        .await?
                        .ok_or_else(|| not_found(price_id))?;
                    delete_bands(txn, &scope, tenant_id, price_id).await?;
                    let result = price::Entity::delete_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(guard)
                        .exec(txn)
                        .await
                        .map_err(|e| RepoError::Db(format!("delete price draft: {e}")))?;
                    if result.rows_affected == 0 {
                        return Err(refuse(txn, &scope, tenant_id, price_id, expected).await);
                    }
                    record_price_mutation(
                        txn,
                        &scope,
                        tenant_id,
                        &doomed,
                        AuditAction::Delete,
                        Some(expected),
                        stamp,
                    )
                    .await
                })
            })
            .await;
        outcome.map_err(tx_failure)
    }

    /// The non-transactional runner, named once so four read paths spell the
    /// failure the same way.
    fn conn(&self) -> Result<DbConn<'_>, RepoError> {
        self.db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Transaction plumbing.
// ---------------------------------------------------------------------------

/// Refuse an edit to a row an **other** bulk run holds (`inst-bk-lock`).
///
/// `on_behalf_of` is the run the edit belongs to, or `None` for an interactive
/// one. **The distinction is load-bearing and easy to miss**: Phase 2 edits the
/// very rows it locked, so a guard that refused every locked row would make the
/// commit refuse its own batch. What the lock excludes is *somebody else*.
///
/// Read inside the caller's transaction, and the holder is nameable: a lock row is
/// inserted by a run already `committing`, so it is durable — which is what lets
/// the conflict say which run, as `fr-concurrent-edit` requires.
///
/// # Errors
/// [`RepoError::BulkRowLocked`] naming the holder; [`RepoError::Db`] on a scope or
/// storage failure.
async fn refuse_if_locked_elsewhere(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    on_behalf_of: Option<Uuid>,
) -> Result<(), RepoError> {
    let Some(holder) = super::bulk_repo::lock_holder(runner, scope, tenant_id, price_id).await?
    else {
        return Ok(());
    };
    if on_behalf_of == Some(holder) {
        return Ok(());
    }
    Err(RepoError::BulkRowLocked {
        price_id: price_id.to_string(),
        bulk_operation_id: holder.to_string(),
    })
}

/// Refuse a delete of a row a **window** stands on
/// (`fk_pricing_price_window_price`).
///
/// [`refuse_if_locked_elsewhere`]'s shape and that guard's own standard, one table
/// over: *a rule that lives on one authoring path is not a rule*. The key
/// references `pricing_price (price_id)` with no cascade on either backend and
/// sqlx turns `foreign_keys` ON for `SQLite`, so without this the statement meets
/// the key and comes back as [`RepoError::Db`] — the operator told the store is
/// broken about a row and a schedule they can both see. The read is inside the
/// caller's transaction and before the band delete, so nothing is removed on the
/// way to the refusal.
///
/// **Any window refuses, in any state, and that is the measured part.**
/// `pricing_price_window` refuses `DELETE` outright — "cancel is a state, not a
/// deletion" — so a `cancelled` or `expired` row still holds the key. Filtering to
/// the live states would answer `Ok` for a row the statement then fails on, which
/// is the shape a pre-check exists to prevent.
///
/// **`ON DELETE CASCADE` is the rejected alternative.** A window is the subject of
/// approval units, so deleting the windows with the draft costs the operator
/// decided, scheduled state they never asked about and cannot see they lost — which
/// is the state that table is append-only to protect.
///
/// The window table is read here rather than through
/// [`window_repo`](super::window_repo) deliberately: that module resolves its keys
/// through this one, and a call back the other way would make the two repositories
/// a cycle for one column.
///
/// # Errors
/// [`RepoError::PriceWindowScheduled`] naming the window; [`RepoError::Db`] on a
/// scope or storage failure.
async fn refuse_if_a_window_stands_on_it(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<(), RepoError> {
    let standing = price_window::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_window::Column::TenantId.eq(tenant_id))
                .add(price_window::Column::PriceId.eq(price_id)),
        )
        // Ordered so the refusal names the same window on every call: the table
        // carries no ordinal and an unordered read of two windows on one row would
        // name whichever the engine reached first.
        .order_by(price_window::Column::WindowId, Order::Asc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the windows standing on a price row: {e}")))?;
    match standing {
        None => Ok(()),
        Some(window) => Err(RepoError::PriceWindowScheduled {
            price_id: price_id.to_string(),
            window_id: window.window_id.to_string(),
        }),
    }
}

/// Flatten the transaction wrapper back into this repository's vocabulary.
///
/// The body's error type is [`RepoError`] rather than the driver's precisely so
/// a typed refusal survives the rollback: a `create_draft` that met an occupied
/// key has to reach the caller as `DUPLICATE_SCOPE_KEY`, not as "the store
/// failed". What arrives as infrastructure is only what happens outside the
/// body — beginning the transaction, and committing it.
fn tx_failure(err: TxError<RepoError>) -> RepoError {
    err.into_domain(|infra| RepoError::Db(format!("price draft transaction: {infra}")))
}

/// `(resolved tax category, resolved rounding policy)` — the pair a publish
/// freezes onto a row, and the key its rows are grouped by so one statement
/// serves every row sharing a resolution.
///
/// A named type because the tuple is a map key in one place and its shape is the
/// thing a reader has to hold: `Option` on both halves is "the resolution is
/// genuinely absent", not "not computed yet".
type ResolutionKey = (Option<String>, Option<String>);

/// Publish **exactly the validated price rows**, inside the caller's
/// transaction, returning the ids that moved.
///
/// # The set is an argument, and that is the whole guard
///
/// `validated` is `(price_id, row_version)` for every `draft` row of the subject
/// the rule set actually judged. This function publishes those rows at those
/// versions and nothing else — it does **not** re-derive "the plan's draft rows"
/// for itself.
///
/// It used to, and that was a defect. The publish commit assembles its subject,
/// runs the rules, then makes a network round-trip to the `CatalogVersion`
/// registry before it flips anything. `in_transaction` opens the engine default,
/// which on Postgres is READ COMMITTED, so **every statement takes a fresh
/// snapshot**: a `create_draft` committing inside that window inserts a row the
/// rule set never saw, and a `update_draft` committing inside it changes the
/// content of a row the rule set passed. A re-derived set would publish both.
/// The outcome is a consumer-visible `published` row that never passed §4.2's
/// rule set — one that could, for instance, carry no `roundingPolicyRef` on a
/// tenant with no default, which is precisely what the Foundation's rounding
/// rule exists to block.
///
/// **D-141 is what closes it.** Every price row carries its own entity tag
/// exactly so a per-row conflict means something, and this is that meaning: a
/// row whose content moved between validation and commit does not stand at
/// the version the subject was assembled from, so it does not match and the
/// publish is refused **naming the row**. A newly inserted row is not in the
/// set, so it stays `draft` and publishes with the next revision — correct by
/// construction, because nothing validated it.
///
/// The alternative was `SERIALIZABLE`, and it was rejected: it would hold
/// predicate locks across the registry's network round-trip, which is the one
/// cost the commit's ordering decision was chosen to bound. See
/// [`PublishService::commit`](crate::infra::publish::PublishService::commit)'s
/// CONTRACT paragraph, which now carries the full argument.
///
/// # Why the pre-read, and why it is not the guard
///
/// The version conjunction is checked **before** the UPDATE as well as by the
/// UPDATE's own row count, and the order is forced: after the UPDATE this
/// transaction reads its own writes, so a row that legitimately moved to
/// `published` is indistinguishable from one that never matched. Taken first,
/// the read names which conjunct failed — absent, frozen, or stale — the way
/// every other compare-and-swap in this file does. The **count** is still the
/// guard, and it covers the one window the read cannot: a concurrent commit
/// landing between two adjacent statements.
///
/// # The rest, unchanged
///
/// **One UPDATE, not a per-row negotiation**, and the two partial unique indexes
/// are why that is safe. The draft-plane index (D-148,
/// `WHERE lifecycle_state = 'draft'`) releases a key as its row leaves `draft`
/// and the published-plane index claims it as the row arrives, both inside this
/// statement. [`PriceRepo::create_draft`] already refuses a scope key occupied by
/// a `draft` **or** a `published` row, so a plan's draft rows sit on keys nothing
/// else holds; a concurrent publish of a *second* plan cannot collide because
/// every key carries `plan_id`.
///
/// # The one draft row that does **not** sit on a free key, and what it owes
///
/// The paragraph above was the whole argument while the authoring door was the
/// only door. It is not any more: [`insert_successor_draft_on`] stages a successor
/// **beside** the published row it will supersede (D-195), which is exactly a
/// draft row on a key something else holds — and this statement flips it while
/// that something is still `published`, so `uq_pricing_price_scope_key_current`
/// refuses it. Measured, not reasoned: the refusal arrives as a raw driver
/// unique-violation, so it reaches the caller as [`RepoError::Db`] — **a 500, not
/// a refusal an operator can act on**.
///
/// This function is nevertheless **unchanged**, and deliberately so. The remedy is
/// an ordering the supersession commit owes rather than a check here:
/// `inst-su-commit` flips the predecessor `published → superseded` **before** the
/// successor's publish, in the same transaction, and with the key free on the
/// published plane this statement is ordinary again. A guard here could only
/// re-refuse what the index already refuses, one round trip earlier and for every
/// publish, to catch a caller that has skipped a documented step of its own
/// commit; `tests/sqlite_price_repo.rs` pins both arms instead.
///
/// **`draft -> published` is the row's only edge out of `draft`** and this is the
/// sanctioned producer of it. The predicate is restated here rather than left to
/// the table because §4.3 enforces immutability twice and the physical guard's
/// answer is a raw driver error carrying no state — and the physical half now
/// exists on this table too: D-153's draft-plane whitelist was missing from
/// `pricing_price` (the trigger returned early for a draft row) and was amended
/// into `pricing_price` on both backends while this guard was being written.
///
/// The `row_version` is deliberately **not** bumped. §3.7 says the tag freezes
/// with the content it names (D-141) and this flip changes no content; after it
/// the row is frozen and no caller-driven mutation exists for a precondition to
/// guard, so a bump would move an entity tag under a representation nobody can
/// write to.
///
/// # Errors
/// [`RepoError::NotFound`] when a validated row is no longer visible to `scope`;
/// [`RepoError::NotDraft`] when one has left `draft` since it was validated;
/// [`RepoError::StaleRowVersion`] carrying both versions when one's content
/// moved since it was validated — the refusal this whole shape exists to
/// produce; [`RepoError::Db`] on a scope or storage failure, **including** a
/// concurrent commit landing between the pre-read and the UPDATE, reported as
/// the row-count mismatch it is; [`RepoError::CorruptRow`] when a stored row
/// cannot be read as the domain value its columns are `CHECK`-constrained to
/// hold; [`RepoError::RoundingPolicyUnresolved`] when a row carries no
/// `rounding_policy_ref` and no tenant default resolves one, and
/// [`RepoError::TaxCategoryUnresolved`] when it states no `tax_category_ref` and its
/// region declares no default — the two publish rules' refusals restated where every
/// door passes through them.
///
/// **"Every door" is three call sites and four doors**, which is worth spelling
/// because the two numbers are used interchangeably nearby: the sites are
/// `infra::publish`'s commit, [`commit_supersession_rows`] and
/// [`commit_cutover_rows`], and the fourth door — mass repricing — reaches the
/// second of them. The distinction matters to the paragraph below about which of
/// them runs a rule set.
///
/// **Both frozen resolutions are held to the same standard here.** Refusing them
/// at this function rather than only at the rule set is what makes the guarantee
/// one of construction: `TaxBasisComplete` is a `run_publish_rules` rule, so it
/// judges the ordinary publish and the two paths that re-run that set over the
/// post-commit shape, while the plain supersession runs no such set and asks
/// `infra::supersession::refuse_unresolved_tax_category` itself. A **new** caller
/// of `publish_rows` that neither ran the rule set nor asked first would
/// otherwise freeze `resolved_tax_category` NULL onto a published row, which the
/// table's append-only guard makes permanent with no repair available afterwards.
/// [`RepoError::TaxCategoryUnresolved`] is that refusal, carrying
/// `TAX_BASIS_INCOMPLETE` beside [`RepoError::RoundingPolicyUnresolved`]'s code.
///
/// Adding it made the test seeder `rest_support::Harness::publish_price` a violator
/// — it published against `RegionTaxReadiness::empty()` on purpose, so every
/// fixture-published row in the crate carried a NULL category. The seeder now
/// declares one; see `rest_support::fixture_readiness`. Repairing the fixture rather
/// than exempting the refusal is the point: a fixture quietly carrying the fault a
/// new rule judges is exactly what the rule exists to catch.
// Spanned so a publish is followed through the store: the flip, the freeze and
// the audit append are separate statements inside one transaction, and without a
// span the only thing tying them together in a log is wall-clock adjacency.
// `skip_all`, because the scope and the runner are the request's and the store's
// rather than facts about the act.
#[tracing::instrument(
    skip_all,
    fields(tenant_id = %tenant_id, plan_id = %plan_id, rows = validated.len())
)]
pub async fn publish_rows(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    validated: &[(Uuid, RowVersion)],
    readiness: &RegionTaxReadiness,
    default_rounding_policy: Option<&str>,
) -> Result<Vec<Uuid>, RepoError> {
    if validated.is_empty() {
        return Ok(Vec::new());
    }

    // Refused before anything flips. See the doc: this is not the guard, it is
    // the only place a precise refusal can still be produced.
    //
    // **One query for the whole set, not one per row.** §14's soft cap is 500
    // price rows per plan, and a per-row read would put five hundred sequential
    // round-trips inside a transaction that is already holding the registry's
    // network call open. The per-row *detail* comes from comparing what the read
    // returned against what was validated, not from having read them one at a
    // time — so the refusal below is exactly as precise, and is still taken
    // through [`refuse`] so its three answers are spelled in one place.
    let held: HashMap<Uuid, price::Model> =
        load_rows(txn, scope, tenant_id, validated.iter().map(|(id, _)| *id))
            .await?
            .into_iter()
            .map(|row| (row.price_id, row))
            .collect();
    for (price_id, expected) in validated {
        if !stands_at(held.get(price_id), *expected) {
            return Err(refuse(txn, scope, tenant_id, *price_id, *expected).await);
        }
    }

    let mut identities = Condition::any();
    for (price_id, expected) in validated {
        let stored = expected
            .to_stored()
            .map_err(|e| RepoError::CorruptRow(format!("price row {price_id} row version: {e}")))?;
        identities = identities.add(
            Condition::all()
                .add(price::Column::PriceId.eq(*price_id))
                .add(price::Column::RowVersion.eq(stored)),
        );
    }

    // **D-154's effective category is resolved and frozen here**, inside the
    // publish transaction and against the readiness the rule set judged these
    // rows with — not in the projector, which runs up to D-47's five-minute
    // batching maximum later and would re-derive it against a region taxonomy
    // any `config × write` holder may have re-declared in between. That was the
    // shape review found (`T-13`): a version could freeze a category no rule
    // ever judged, or lose one that was present when publish passed.
    //
    // Grouped by resolved value rather than one statement per row: a plan's rows
    // share very few distinct categories, so this is one or two statements where
    // §14's 500-row soft cap would otherwise mean five hundred inside a
    // transaction already holding the registry's network call open. `held` is the
    // read `publish_rows` already made, so no extra round trip resolves it.
    // **Two resolutions, frozen together** — the row's own value where it has one,
    // the tenant's default where it does not.
    //
    // `RoundingPolicyResolved` returns at its first line the moment a tenant
    // default exists, so the rule alone leaves the raw nullable column unresolved
    // and both renderers would freeze `"roundingPolicyRef": null` on every row of a
    // tenant that has a default. This is a freeze rather than a lookup for two
    // reasons, and the second is the load-bearing one: a consumer computing a charge
    // from the pinned version needs the rounding mode, **and** the tenant must not be
    // able to flip the default and silently re-round every already-frozen version.
    // `resolved_tax_category` beside it is frozen the same way, for the same reason.
    let mut by_resolution: BTreeMap<ResolutionKey, Vec<Uuid>> = BTreeMap::new();
    for (price_id, _) in validated {
        let row = held.get(price_id);
        let category = row.and_then(|row| {
            row.tax_category_ref.clone().or_else(|| {
                readiness
                    .of_str(&row.region)
                    .and_then(|markers| markers.tax_category.clone())
            })
        });
        let rounding = row.and_then(|row| {
            row.rounding_policy_ref
                .clone()
                .or_else(|| default_rounding_policy.map(ToOwned::to_owned))
        });
        by_resolution
            .entry((category, rounding))
            .or_default()
            .push(*price_id);
    }

    // **The rounding resolution is refused here, not merely frozen here**.
    // `foundation.rounding_policy_resolved` judges exactly this and runs on one of this
    // function's four doors; `infra::supersession`, `infra::cutover` and mass repricing reach
    // the freeze through `commit_supersession_rows` with a default they read themselves and no
    // rule over it. Freezing `None` on those three doors writes the state `pricing_price`'s
    // migration header calls impossible onto a **published** row, and
    // `trg_pricing_price_append_only` then makes that row immutable — so there is no repair
    // after the fact and the check has to be on this side of the write.
    //
    // Before the loop rather than inside it: the refusal must land before any
    // group's statement executes, or a plan with two resolutions publishes half
    // of itself and then refuses.
    //
    // **Both resolutions are refused here** — the category half
    // was not, and closing that is H14 of a review. The two `find`s
    // are separate because they name different columns and different remedies; the
    // predicate below reads only the second element of the key, the one above only
    // the first.
    if let Some(price_id) = by_resolution
        .iter()
        .find(|((category, _), _)| category.is_none())
        .and_then(|(_, price_ids)| price_ids.first())
    {
        return Err(RepoError::TaxCategoryUnresolved {
            price_id: price_id.to_string(),
        });
    }
    if let Some(price_id) = by_resolution
        .iter()
        .find(|((_, rounding), _)| rounding.is_none())
        .and_then(|(_, price_ids)| price_ids.first())
    {
        return Err(RepoError::RoundingPolicyUnresolved {
            price_id: price_id.to_string(),
        });
    }

    let mut result_rows = 0_u64;
    for ((resolved, rounding), price_ids) in by_resolution {
        let mut group = Condition::any();
        for price_id in &price_ids {
            group = group.add(price::Column::PriceId.eq(*price_id));
        }
        let outcome = price::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                price::Column::LifecycleState,
                Expr::value(LifecycleState::Published.as_str()),
            )
            .col_expr(
                price::Column::ResolvedTaxCategory,
                Expr::value(resolved.clone()),
            )
            // **Beside the authored column, never over it.** Writing `RoundingPolicyRef`
            // itself would bring a row that carries no policy of its own back from publish
            // naming one: the tenant default would stop reaching it, `authored_content` and
            // `infra::supersession` would carry the resolution into every successor draft as
            // authored, and `RoundingPolicyDeclared` would have nothing left to judge.
            // `ResolvedTaxCategory` one clause up has the same shape.
            .col_expr(
                price::Column::ResolvedRoundingPolicy,
                Expr::value(rounding.clone()),
            )
            .filter(
                plan_rows_in_state(tenant_id, plan_id, LifecycleState::Draft)
                    .add(identities.clone())
                    .add(group),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("publish plan price rows: {e}")))?;
        result_rows += outcome.rows_affected;
    }

    // Checked rather than cast: a cast that wrapped would report a mismatched
    // set as a matching one, which is the one answer this comparison must never
    // be able to give.
    let expected_count = u64::try_from(validated.len()).map_err(|_| {
        RepoError::CorruptRow(format!(
            "plan {plan_id} carries {} validated price rows, more than a row count can hold",
            validated.len()
        ))
    })?;
    if result_rows != expected_count {
        // **Two causes reach this shortfall and the statement cannot tell them
        // apart**, which is why it stays a `Db` and why the message no longer
        // asserts one of them. A concurrent commit moving a row between the check
        // and the flip produces it — and so does a validated set naming a row of
        // another plan, which this statement's own `plan_id` predicate excludes and
        // which `a_validated_row_of_another_plan_is_caught_by_the_count` pins. The
        // first is contention a retry resolves; the second is a caller fault a
        // retry repeats forever, and labelling the pair as either one tells half
        // the callers the wrong thing.
        return Err(RepoError::Db(format!(
            "plan {plan_id} validated {expected_count} price rows and {result_rows} moved; \
             either a concurrent commit changed one between the check and the flip, or the \
             validated set named a row this plan does not hold"
        )));
    }
    Ok(validated.iter().map(|(price_id, _)| *price_id).collect())
}

/// What a publish froze onto one row: the effective values, both of them.
///
/// Read together because they are frozen together, by one statement, for one
/// reason — a consumer replaying a pinned `CatalogVersion` must see the tax
/// category and the rounding mode the rule set judged the row with, not the ones
/// the tenant's mutable config happens to name now. Two readers would be two
/// scans of the same rows on the projector's sweep.
///
/// `None` on either half means the publish resolved nothing for it, which is a
/// state a draft row is always in and a published row must not be — but the two
/// halves are guaranteed by different mechanisms and the difference is worth
/// knowing before trusting either:
///
/// - **Rounding.** [`publish_rows`] itself refuses a set resolving none, so every
///   door that writes this column is held to it by construction.
/// - **Category.** `TaxBasisComplete` refuses a row resolving none, and it is a
///   `run_publish_rules` rule — so the guarantee is held by the *callers* that run
///   that set (the ordinary publish, and the cutover and mass repricing, which
///   re-run it over the post-commit shape). The plain supersession runs no rule
///   set and refuses it at the door instead
///   (`infra::supersession::supersede_in`). It is a property of the doors, not of
///   this type: a door that does neither freezes NULL here, and the table's
///   append-only guard then makes it permanent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrozenResolution {
    /// `coalesce(tax_category_ref, readiness.taxCategory)` as judged (D-154).
    pub tax_category: Option<String>,
    /// `coalesce(rounding_policy_ref, tenant.default_rounding_policy_ref)` as
    /// judged. Lives in its own column since `pricing_price`; the publish used
    /// to write it over the authored `rounding_policy_ref`.
    pub rounding_policy: Option<String>,
}

/// Each row's **frozen** resolved tax category, by `price_id` (D-154).
///
/// Read rather than re-derived: the value was resolved inside the publish
/// transaction against the readiness that judged the row, and the region taxonomy
/// has been mutable ever since. `None` in the map is a row that has one and it is
/// null; a row absent from the map has not published.
///
/// **The lifecycle filter is what makes that second sentence true.** With
/// `tenant_id` and `plan_id` as the only predicates, every `draft` row of the plan
/// would be in the map carrying `None` — the one value the sentence above reads as
/// "published, with no category". The filter is here so the **key set** means what
/// the doc says and a draft's NULL cannot be carried into a projection by a second
/// reader.
///
/// The set is [`PROJECTED_ROW_STATES`] itself rather than `published` alone,
/// because "has published" includes `superseded`: rating pins current versions and
/// rates past instants, so a superseded predecessor's frozen category is still
/// resolved against (D-121). `chk_pricing_price_lifecycle_state` admits exactly
/// `draft`, `published` and `superseded` as the chain now stands
/// (`pricing_price`, unwidened since), so the filter's complement is the draft
/// plane and nothing else.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn frozen_resolutions(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<BTreeMap<Uuid, FrozenResolution>, RepoError> {
    Ok(price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PlanId.eq(plan_id.get()))
                .add(
                    price::Column::LifecycleState
                        .is_in(PROJECTED_ROW_STATES.iter().map(|state| state.as_str())),
                ),
        )
        .all(runner)
        .await
        .map_err(|e| {
            RepoError::Db(format!(
                "read the frozen resolutions of plan {plan_id}: {e}"
            ))
        })?
        .into_iter()
        .map(|row| {
            (
                row.price_id,
                FrozenResolution {
                    tax_category: row.resolved_tax_category,
                    rounding_policy: row.resolved_rounding_policy,
                },
            )
        })
        .collect())
}

/// The supersession unit's **row half**: the predecessor leaves the published
/// plane, then the successor arrives on it.
///
/// # The ordering is the function
///
/// `inst-su-commit`'s moves include these two, and D-195 makes their order
/// normative: §3.7 admits at most one published row per key, so a successor
/// flipped `draft → published` while its predecessor still reads `published`
/// violates `uq_pricing_price_scope_key_current` — and violates it as a raw driver
/// error, a 500 carrying nothing an operator can act on rather than a refusal.
///
/// They are therefore **one function rather than two calls a caller sequences**. A
/// caller who can order them can order them wrongly, and the failure mode is not a
/// refused request but a fault; the whole reason the ordering had to be written
/// into the design set is that it is invisible until it fires. Withholding the
/// separate moves is the point of this shape, not an oversight in it.
///
/// # Atomicity, and which move goes first
///
/// Both statements run in the caller's transaction, so `inst-su-commit`'s "or
/// everything rolls back" holds for the pair. The order serves the *failure* path
/// as well as the index: with the flip first, a successor the publish refuses
/// leaves a transaction that rolls the flip back with it, and the state that is
/// never briefly true is the one where the key has no current row at all —
/// coverage removed, nothing selling — which is worse than either move not having
/// happened.
///
/// # The predecessor's tag does not move, and is not a precondition either
///
/// The flip is a compare-and-swap on the row's **state**, not on its version.
/// D-141 freezes the row version with the published row's content and exempts the
/// `lifecycle_state` flips from moving it, so there is no version for a caller to
/// present and none to bump: a tag that moved under a representation no caller can
/// write to would report a stale cache that is not stale. What the swap is against
/// is `lifecycle_state = 'published'`, and that is what makes a replayed commit
/// refuse rather than re-flip a row that has already moved.
///
/// The successor's version **is** presented, because its content stays mutable
/// right up to this statement. That precondition is [`publish_rows`]', unchanged,
/// and this function delegates to it rather than restating it.
///
/// # No audit record here
///
/// The publish commit writes one record for the whole act at the subject's level
/// rather than one per row moved — see
/// [`PublishService::commit`](crate::infra::publish::PublishService::commit) step 6,
/// where [`publish_rows`] is likewise silent. The supersession unit's record is
/// [`crate::infra::supersession::SupersessionService`]'s, which is the layer holding
/// its approval reference.
///
/// # The pair is checked, and the key is the load-bearing half
///
/// The two ids arrive as independent arguments, so nothing about the *call* says they
/// belong together — and this function enforced only their **order** until a review
/// pointed out it enforced nothing about their **pairing**. A predecessor
/// on one key and a successor on another key of the same plan committed cleanly:
/// `supersede_row` never looks at the successor, and [`publish_rows`] filters by
/// `plan_id` rather than by key. The result is the predecessor's key left with **no
/// current row at all** — the state this function's own ordering argument calls worse
/// than either move not having happened.
///
/// So both rows are read first and two relations are required:
///
/// - **The same canonical scope key.** This is the load-bearing one. A supersession is
///   by definition a change on *one* key (`inst-ps-supersede`), so a differing key is
///   not a mispaired supersession, it is two unrelated acts wearing one transaction.
/// - **`successor.supersedes_price_id == Some(predecessor)`** (D-127). This is the
///   second belt, and it is second rather than first because it is **spoofable**:
///   `supersedes_price_id` is caller-supplied content on the authoring routes
///   (`PriceContentView` carries it, and `POST`/`PATCH` plumb it through unvalidated),
///   so [`insert_successor_draft_on`]'s claim to be the only writer of it is false. It
///   still earns its place — it catches a draft authored on a free key that the key's
///   later occupant would otherwise look paired with.
///
/// One extra read for the pair, which buys a refusal in place of a silently
/// uncovered key.
///
/// # Errors
/// [`RepoError::NotSupersedable`] when the predecessor is not the key's current row
/// (naming the state it was found in), when the two rows stand on different keys, or
/// when the successor's link names something else; [`RepoError::NotFound`] when either
/// row is invisible to `scope`; everything [`publish_rows`] answers for the successor,
/// notably [`RepoError::StaleRowVersion`]; [`RepoError::Db`] on a storage failure.
#[allow(
    clippy::too_many_arguments,
    reason = "the transaction, the scope, the tenant, the plan, the two ends of the supersession, \
              the region readiness the rules judged with and the tenant's rounding default. Every \
              one is a different authority and none is derivable from the others - the same \
              reading `retire_in` records for its own list"
)]
pub async fn commit_supersession_rows(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    predecessor: Uuid,
    successor: (Uuid, RowVersion),
    readiness: &RegionTaxReadiness,
    default_rounding_policy: Option<&str>,
) -> Result<(), RepoError> {
    refuse_mispaired(txn, scope, tenant_id, predecessor, successor.0).await?;
    supersede_row(txn, scope, tenant_id, predecessor).await?;
    publish_rows(
        txn,
        scope,
        tenant_id,
        plan_id,
        &[successor],
        readiness,
        default_rounding_policy,
    )
    .await?;
    Ok(())
}

/// Refuse a predecessor and successor that are not each other's.
///
/// Both rows in one query — the pair is two ids and a round trip each would put a
/// second read inside a transaction that is about to hold four writes.
async fn refuse_mispaired(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    predecessor: Uuid,
    successor: Uuid,
) -> Result<(), RepoError> {
    let rows: HashMap<Uuid, price::Model> = load_rows(
        runner,
        scope,
        tenant_id,
        [predecessor, successor].into_iter(),
    )
    .await?
    .into_iter()
    .map(|row| (row.price_id, row))
    .collect();
    let (Some(before), Some(after)) = (rows.get(&predecessor), rows.get(&successor)) else {
        // Which one is missing is not this function's answer to give: the two sibling
        // moves below each produce a precise refusal for their own row, and answering
        // here would pre-empt whichever of them the caller actually needs to hear.
        return Ok(());
    };

    if scope_key_columns(before) != scope_key_columns(after) {
        return Err(RepoError::NotSupersedable {
            subject: SUBJECT.to_owned(),
            id: successor.to_string(),
            state: format!(
                "on a different canonical scope key from price {predecessor}; a supersession is a \
                 change on one key"
            ),
        });
    }
    if after.supersedes_price_id != Some(predecessor) {
        return Err(RepoError::NotSupersedable {
            subject: SUBJECT.to_owned(),
            id: successor.to_string(),
            state: format!(
                "carrying supersedesPriceId {:?}, which does not name price {predecessor}",
                after.supersedes_price_id
            ),
        });
    }
    Ok(())
}

/// The ten canonical scope-key columns of a stored row, as one comparable value.
///
/// A named alias rather than a bare tuple because the tuple grew past what clippy
/// will read at a glance, and a reader counting axes against `01-foundation.md` §4.1
/// should be counting them in one place.
type ScopeKeyColumns<'a> = (
    Uuid,
    &'a str,
    &'a str,
    &'a str,
    Uuid,
    &'a str,
    &'a str,
    &'a str,
    Option<&'a str>,
    &'a str,
);

/// The cutover's row plane: the predecessor leaves, the successor and the
/// grandfathered copy arrive, in one transaction (`inst-co-supersede`, D-100).
///
/// **The supersession's door with one more row and one looser pairing.** The
/// successor lands on the predecessor's **own** key, so it is checked by exactly the
/// guard a supersession's successor is. The copy does not: it lands on a *new
/// generation* of that key, which by construction differs in `priceEligibility` and
/// `cohort` and in nothing else. So the copy is paired modulo those two axes — a
/// comparison that must be written down rather than assumed, because "modulo two
/// axes" is precisely the kind of looseness that later reads as "unchecked".
///
/// **The flip goes first**, for `commit_supersession_rows`' reason and it is the same
/// index: Foundation §3.7 admits one published row per key, so publishing the
/// successor while the predecessor still reads `published` violates
/// `uq_pricing_price_scope_key_current` — and violates it as a raw driver error, a
/// 500 carrying nothing an operator can act on, rather than as a refusal. The copy is
/// on a different key and could publish in any order; it goes with the successor so
/// that one statement covers both and no future edit can separate them.
///
/// # Errors
///
/// [`RepoError::NotSupersedable`] when the successor is not on the predecessor's key
/// or does not name it, or when the copy is not a generation of that same market;
/// whatever the flip and the publish refuse otherwise.
#[allow(
    clippy::too_many_arguments,
    reason = "three row identities, the plan they belong to, and the act's instant — which is \
              not decoration: it is what makes the copy's cohort checkable rather than assumed"
)]
pub async fn commit_cutover_rows(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    predecessor: Uuid,
    successor: (Uuid, RowVersion),
    copy: (Uuid, RowVersion),
    cutover_at: OffsetDateTime,
    readiness: &RegionTaxReadiness,
    default_rounding_policy: Option<&str>,
) -> Result<(), RepoError> {
    refuse_mispaired(txn, scope, tenant_id, predecessor, successor.0).await?;
    refuse_ungenerational(txn, scope, tenant_id, predecessor, copy.0, cutover_at).await?;
    supersede_row(txn, scope, tenant_id, predecessor).await?;
    publish_rows(
        txn,
        scope,
        tenant_id,
        plan_id,
        &[successor, copy],
        readiness,
        default_rounding_policy,
    )
    .await?;
    Ok(())
}

/// The copy must be a **generation of the predecessor's market**, not a row from
/// somewhere else on the plan.
///
/// Compared modulo `priceEligibility` and `cohort`, which are the two axes
/// `inst-co-copy` moves and the only two it may. Everything else — the plan, the
/// market, the phase, the charge kind and, since D-196, the usage line — is the
/// predecessor's or the copy is not a copy of it.
///
/// **Both moved axes are pinned, cohort included.** The
/// class alone leaves every *previous* generation of the same market admissible: an
/// `existing_grandfathered` row on the market whose cohort names an earlier cutover
/// would pass a class-only check, and publishing it would republish an immutable
/// retained row that this act never composed — this is the row plane's last guard
/// before `publish_rows`. "Modulo two axes" has to mean the two axes are checked
/// against what this act says they are, not merely that they are allowed to differ.
///
/// The instant is the caller's rather than read back from the composition, for
/// `shorten_expected_seq`'s reason one layer up: this door is handed what the act
/// decided, and a door that re-derived the act's own instant could not tell a
/// mismatch from a recomputation.
async fn refuse_ungenerational(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    predecessor: Uuid,
    copy: Uuid,
    cutover_at: OffsetDateTime,
) -> Result<(), RepoError> {
    let rows: HashMap<Uuid, price::Model> =
        load_rows(runner, scope, tenant_id, [predecessor, copy].into_iter())
            .await?
            .into_iter()
            .map(|row| (row.price_id, row))
            .collect();
    // Which one is missing is the sibling moves' answer to give, exactly as in
    // `refuse_mispaired`.
    let (Some(before), Some(after)) = (rows.get(&predecessor), rows.get(&copy)) else {
        return Ok(());
    };
    if market_columns(before) != market_columns(after) {
        return Err(RepoError::NotSupersedable {
            subject: SUBJECT.to_owned(),
            id: copy.to_string(),
            state: format!(
                "not a grandfathered generation of price {predecessor}'s market; a cutover copy moves the eligibility class and the cohort and no other axis"
            ),
        });
    }
    if after.price_eligibility != PriceEligibility::ExistingGrandfathered.as_str() {
        return Err(RepoError::NotSupersedable {
            subject: SUBJECT.to_owned(),
            id: copy.to_string(),
            state: format!(
                "on eligibility class {}; a cutover copy is the existing_grandfathered row",
                after.price_eligibility
            ),
        });
    }
    let this_generation = Cohort::Generation(cutover_at).to_string();
    if after.cohort != this_generation {
        return Err(RepoError::NotSupersedable {
            subject: SUBJECT.to_owned(),
            id: copy.to_string(),
            state: format!(
                "on generation {}, not this cutover's {this_generation}; each cutover mints one \
                 generation keyed by its own instant, and an earlier one is a previous act's \
                 retained row",
                after.cohort
            ),
        });
    }
    Ok(())
}

/// The eight scope-key columns a generation shares with the row it was copied from.
type MarketColumns<'a> = (
    Uuid,
    &'a str,
    &'a str,
    &'a str,
    Uuid,
    &'a str,
    Option<&'a str>,
    &'a str,
);

/// The scope-key columns a generation shares with the row it was copied from — all
/// of them but `priceEligibility` and `cohort`.
fn market_columns(row: &price::Model) -> MarketColumns<'_> {
    (
        row.plan_id,
        row.currency.as_str(),
        row.region.as_str(),
        row.price_overlay.as_str(),
        row.phase,
        row.charge_kind.as_str(),
        row.meter.as_deref(),
        row.dimension_key.as_str(),
    )
}

/// The canonical scope-key columns of a stored row, as a comparable tuple.
///
/// Compared column-wise rather than by parsing back into a [`ScopeKey`]: a row whose
/// stored axis is outside its enumeration is a [`RepoError::CorruptRow`] that the
/// callers' own reads will report with the subject attached, and a comparison that
/// had to parse first would answer "corrupt" where the honest answer is "these two
/// rows are not on one key".
///
/// **All ten axes, not the eight D-196 clause (3) is easy to attach to.** A
/// comparison that omits the usage line reads a successor on a **different meter
/// of the same market** as being on the predecessor's key, and this function's
/// whole sentence is about key *identity*. D-82's unit guard would catch that
/// particular pair by comparing `meter` between the two rows and answering
/// `SUPERSESSION_UNIT_MISMATCH` — but a guard that cannot see two axes of the
/// thing it is comparing is wrong even while a sibling covers for it.
fn scope_key_columns(row: &price::Model) -> ScopeKeyColumns<'_> {
    (
        row.plan_id,
        row.currency.as_str(),
        row.region.as_str(),
        row.price_overlay.as_str(),
        row.phase,
        row.price_eligibility.as_str(),
        row.charge_kind.as_str(),
        row.cohort.as_str(),
        row.meter.as_deref(),
        row.dimension_key.as_str(),
    )
}

/// Move one published row to `superseded`, refusing anything else.
///
/// The swap is on the state alone — see [`commit_supersession_rows`] for why there
/// is no version to present. The **row count is the guard**, and the re-read that
/// produces a precise refusal is taken *after* the count comes back short: the
/// opposite order from [`publish_rows`]', for the opposite reason. There a pre-read
/// is what names which of three conjuncts failed; here there is one conjunct, so
/// the count is the whole question and a pre-read would only widen the window
/// between asking and acting.
async fn supersede_row(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<(), RepoError> {
    let result = price::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            price::Column::LifecycleState,
            Expr::value(LifecycleState::Superseded.as_str()),
        )
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PriceId.eq(price_id))
                .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str())),
        )
        .exec(txn)
        .await
        .map_err(|e| RepoError::Db(format!("supersede price row: {e}")))?;

    if result.rows_affected == 1 {
        return Ok(());
    }
    Err(refuse_unsupersedable(txn, scope, tenant_id, price_id).await)
}

/// Why a row would not flip: it is gone, it is unreadable, or it is not current.
async fn refuse_unsupersedable(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> RepoError {
    match load_row(runner, scope, tenant_id, price_id).await {
        Err(err) => err,
        Ok(None) => not_found(price_id),
        Ok(Some(row)) => match read_lifecycle(&row.lifecycle_state) {
            Err(err) => err,
            Ok(state) => RepoError::NotSupersedable {
                subject: SUBJECT.to_owned(),
                id: price_id.to_string(),
                state: state.to_string(),
            },
        },
    }
}

/// Read a set of rows by id, scoped, in one query.
///
/// Deliberately **not** filtered by `plan_id`: it mirrors [`mutable_draft`]'s
/// predicate exactly, which is tenant plus identity. A row of another plan of
/// the same tenant therefore passes this read and is excluded by the UPDATE's
/// own plan filter — a mismatch the count assertion catches, which is the one
/// arm of that assertion reachable without concurrency and is what
/// `a_validated_row_of_another_plan_is_caught_by_the_count` exercises.
async fn load_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    ids: impl Iterator<Item = Uuid>,
) -> Result<Vec<price::Model>, RepoError> {
    price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PriceId.is_in(ids)),
        )
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read validated price rows: {e}")))
}

/// Is this row an editable `draft` standing at exactly `expected`?
///
/// The batched half of [`mutable_draft`]'s question, over a row already in hand.
/// A reading that fails — a `lifecycle_state` or `row_version` the column's
/// constraints forbid — answers `false` rather than erroring, because the
/// caller's next move is [`refuse`], which re-reads the row and surfaces the
/// [`RepoError::CorruptRow`] with the subject attached.
fn stands_at(row: Option<&price::Model>, expected: RowVersion) -> bool {
    let Some(row) = row else {
        return false;
    };
    let Ok(state) = read_lifecycle(&row.lifecycle_state) else {
        return false;
    };
    let Ok(current) = read_row_version(row.price_id, row.row_version) else {
        return false;
    };
    state.is_content_mutable() && current == expected
}

/// A plan's rows in one lifecycle state, as a filter. One spelling, so the read
/// that names the ids and the UPDATE that moves them cannot address different
/// sets.
fn plan_rows_in_state(tenant_id: Uuid, plan_id: PlanId, state: LifecycleState) -> Condition {
    Condition::all()
        .add(price::Column::TenantId.eq(tenant_id))
        .add(price::Column::PlanId.eq(plan_id.get()))
        .add(price::Column::LifecycleState.eq(state.as_str()))
}

/// A plan's price rows in the given lifecycle states, through whichever runner
/// the caller holds.
///
/// Public and runner-taking because the publish path assembles its subject
/// **twice** — once as the §4.2 step-2 pre-check and again inside the commit
/// transaction, where `Db::conn()` is refused by the toolkit's
/// transaction-bypass guard. One implementation with two entry points: a second
/// query is a second thing the two runs could disagree about, and that
/// disagreement is what §4.2's re-validation exists to detect rather than to
/// manufacture.
///
/// Ordered by `price_id` ascending, and an **empty** `states` selects nothing —
/// both are [`PriceRepo::list_for_plan`]'s contract and its doc carries the
/// argument.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
/// value its columns are `CHECK`-constrained to hold.
/// The largest `IN (...)` bind list this crate builds in one statement.
///
/// A `Vec<Uuid>` handed to `is_in` is one bind parameter per element, and both
/// engines refuse past a ceiling: Postgres at 65535 parameters, `SQLite` at
/// `SQLITE_MAX_VARIABLE_NUMBER` — 32766 on a current build, 999 on one compiled
/// before 3.32. Past it a read that worked yesterday becomes a driver error the
/// caller reads as a 500, and the id sets here are sized by the tenant's
/// catalogue rather than by the request: a repricing selector naming no
/// `plan_id` spans all of it.
///
/// 8192 leaves headroom under both current ceilings for the predicate's own
/// binds, and keeps the round-trip count a small constant against a per-plan
/// loop's O(plans).
const MAX_IN_BINDS: usize = 8192;

/// The three list-page facts about one plan's **authoring** rows — its `draft`
/// and `published` rows, the set `GET /plans/{planId}/prices` returns under no
/// `$filter` — for every plan on one list page, in one grouped read (D-360).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanRowAggregate {
    /// How many `draft` + `published` rows the plan holds — the item count the
    /// prices list answers unfiltered.
    pub price_row_count: u64,
    /// The distinct `model_kind`s of those rows, sorted. A row with none
    /// contributes to the count and to no kind.
    pub model_kinds: Vec<String>,
    /// The distinct currencies of those rows, sorted.
    pub currencies: Vec<String>,
}

/// [`PlanRowAggregate`] for each of `plan_ids` that holds at least one authoring
/// row (D-360).
///
/// **One grouped read for the whole page**, not one per plan: grouped by
/// `(plan_id, model_kind, currency)`, so the row count is the sum of the group
/// counts and the two vocabularies are the distinct group keys, and the result is
/// bounded by a plan's model×currency combinations — single digits — never by
/// its rows. Built with `project_all`, the secure layer's one projection door,
/// so the scope is compiled before the grouping runs and nothing loads rows to
/// count them.
///
/// A plan with no authoring row has **no entry**; the caller renders zero and two
/// empty lists. A `NULL` or empty `model_kind` contributes to the count and to no
/// kind — the reading the Studio's `summarizePlanRows` takes ("tiered
/// (unspecified)" is the absence of a model, not a variety of one), kept so the
/// numbers do not move when the Studio switches to these fields.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn aggregate_authoring_rows_for_plans(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_ids: &[PlanId],
) -> Result<HashMap<PlanId, PlanRowAggregate>, RepoError> {
    #[derive(sea_orm::FromQueryResult)]
    struct Group {
        plan_id: Uuid,
        model_kind: Option<String>,
        currency: String,
        cnt: i64,
    }
    if plan_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<Uuid> = plan_ids.iter().map(|plan| plan.get()).collect();
    let mut groups = Vec::new();
    // **Chunked on `MAX_IN_BINDS`, as every other `is_in` in this file is.**
    // Today's only caller is a page and cannot exceed D-125's limit, but that
    // bound is the caller's and not this signature's - `load_for_plans` takes the
    // same predicate over the same table and chunks, and the day a
    // catalogue-sized id set arrives here an unchunked bind list is a driver
    // parameter-limit error the caller reads as a 500.
    for chunk in ids.chunks(MAX_IN_BINDS) {
        let page = price::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price::Column::TenantId.eq(tenant_id))
                    .add(price::Column::PlanId.is_in(chunk.to_vec()))
                    .add(price::Column::LifecycleState.is_in([
                        LifecycleState::Draft.as_str(),
                        LifecycleState::Published.as_str(),
                    ])),
            )
            .project_all(runner, |q| {
                q.select_only()
                    .column(price::Column::PlanId)
                    .column(price::Column::ModelKind)
                    .column(price::Column::Currency)
                    .column_as(Expr::col(price::Column::PriceId).count(), "cnt")
                    .group_by(price::Column::PlanId)
                    .group_by(price::Column::ModelKind)
                    .group_by(price::Column::Currency)
                    .into_model::<Group>()
            })
            .await
            .map_err(|e| RepoError::Db(format!("aggregate authoring rows per plan: {e}")))?;
        groups.extend(page);
    }

    let mut out: HashMap<PlanId, PlanRowAggregate> = HashMap::new();
    for group in groups {
        let aggregate = out.entry(PlanId::new(group.plan_id)).or_default();
        aggregate.price_row_count += u64::try_from(group.cnt).unwrap_or(0);
        if let Some(kind) = group.model_kind.filter(|kind| !kind.is_empty())
            && !aggregate.model_kinds.contains(&kind)
        {
            aggregate.model_kinds.push(kind);
        }
        if !aggregate.currencies.contains(&group.currency) {
            aggregate.currencies.push(group.currency);
        }
    }
    for aggregate in out.values_mut() {
        aggregate.model_kinds.sort();
        aggregate.currencies.sort();
    }
    Ok(out)
}

/// Plan-owned metadata for IDs already returned by an original scoped plan read.
/// A plan resource pin cannot be applied to `pricing_price.price_id`; use a
/// tenant projection intersected with exactly those authorized plan IDs.
///
/// # Errors
/// Scope/storage failures are propagated; no failure becomes empty aggregates.
pub async fn aggregate_rows_for_authorized_plans(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    plan_ids: &[PlanId],
) -> Result<HashMap<PlanId, PlanRowAggregate>, RepoError> {
    aggregate_authoring_rows_for_plans(
        runner,
        &AccessScope::for_tenant(tenant_id),
        tenant_id,
        plan_ids,
    )
    .await
}

pub async fn load_for_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    states: &[LifecycleState],
) -> Result<Vec<PriceRecord>, RepoError> {
    load_page(runner, scope, tenant_id, plan_id, states, None, None).await
}

/// [`load_for_plan`] over several plans in one round trip.
///
/// A caller holding a set of plans and calling the singular form per plan pays
/// one query **and one band hydration** each, inside whatever transaction it
/// holds open. `repricing_runs::run_materiality` is that caller: its selector
/// need name no `plan_id` at all, in which case the set is the tenant's whole
/// catalogue and the loop is O(plans) round trips inside the transaction that
/// froze the run's journal.
///
/// **The order is the singular form's, concatenated in `plan_id` order**, and
/// it is sorted here rather than left to `ORDER BY`: `plan_id` is `uuid` on one
/// engine and `text` on the other, so the sort that reproduces a caller's loop
/// exactly is the one this crate does itself.
///
/// An empty plan set selects nothing, as an empty `states` does.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored row cannot be read as the domain
/// value its columns are `CHECK`-constrained to hold.
pub async fn load_for_plans(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_ids: impl IntoIterator<Item = PlanId>,
    states: &[LifecycleState],
) -> Result<Vec<PriceRecord>, RepoError> {
    let plans: Vec<Uuid> = plan_ids.into_iter().map(PlanId::get).collect();
    if plans.is_empty() || states.is_empty() {
        return Ok(Vec::new());
    }
    let tokens: Vec<&str> = states.iter().copied().map(LifecycleState::as_str).collect();
    // **Chunked on [`MAX_IN_BINDS`], `hydrate_bands`' reason applied to the plan
    // axis**: one bind per plan against a set this function's own doc describes as
    // possibly the tenant's whole catalogue, and both engines cap a statement's
    // binds. Past the cap an unchunked `IN` is a hard error rather than a slow
    // query, which is a worse answer than the per-plan loop this replaced. The
    // order is unaffected: the sort below is this function's own and runs over the
    // concatenation.
    let mut rows: Vec<price::Model> = Vec::new();
    for chunk in plans.chunks(MAX_IN_BINDS) {
        let page = price::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price::Column::TenantId.eq(tenant_id))
                    .add(price::Column::PlanId.is_in(chunk.to_vec()))
                    .add(price::Column::LifecycleState.is_in(tokens.clone())),
            )
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("list price rows of several plans: {e}")))?;
        rows.extend(page);
    }

    let mut records = hydrate_bands(runner, scope, tenant_id, &rows).await?;
    records.sort_by_key(|record| (record.scope_key.plan_id(), record.price_id));
    Ok(records)
}

/// The keyset walk under [`load_for_plan`] and [`PriceRepo::list_for_plan_page`].
///
/// `after` resumes **strictly after** a `price_id`, and `limit` bounds the page.
/// Both ride the `price_id ASC` total order [`PriceRepo::list_for_plan`]'s
/// contract already promises on both backends, which is exactly what makes a
/// keyset walk possible: an offset over an append-only store names a different
/// row every time somebody inserts ahead of it (D-125 forbids one for that
/// reason).
async fn load_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    states: &[LifecycleState],
    after: Option<Uuid>,
    limit: Option<u64>,
) -> Result<Vec<PriceRecord>, RepoError> {
    if states.is_empty() {
        return Ok(Vec::new());
    }
    let tokens: Vec<&str> = states.iter().copied().map(LifecycleState::as_str).collect();
    let mut filter = Condition::all()
        .add(price::Column::TenantId.eq(tenant_id))
        .add(price::Column::PlanId.eq(plan_id.get()))
        .add(price::Column::LifecycleState.is_in(tokens));
    if let Some(cursor) = after {
        filter = filter.add(price::Column::PriceId.gt(cursor));
    }
    let mut query = price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(price::Column::PriceId, Order::Asc);
    if let Some(limit) = limit {
        query = query.limit(limit);
    }
    let rows = query
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list plan price rows: {e}")))?;

    hydrate_bands(runner, scope, tenant_id, &rows).await
}

/// The lexicographic `>` over `(created_at_utc, price_id)`, spelled as the
/// OR-of-ANDs a `WHERE` clause can carry.
///
/// The keyset predicate of a **composite** order, and it has to be written out:
/// `created_at_utc > c` alone would skip every row that shares the cursor's
/// instant, and `>=` would return the cursor's own row again on every page.
/// Written as a row comparison it would also be an engine-dependent predicate —
/// `audit_repo::after_position` is this function's twin one table over and its
/// doc carries that half of the argument.
///
/// **Lifted out of [`load_history_page`] so it has a unit-level owner** (review
/// Z2-10). Inline it was reachable only by driving a store, and the failure it
/// guards against — a page silently skipping or repeating a row — is invisible
/// to any fixture whose rows all carry distinct instants, which is every fixture
/// a suite writes without meaning to build this case.
fn after_history_position(position: HistoryPosition) -> Condition {
    Condition::any()
        .add(price::Column::CreatedAtUtc.gt(position.authored_at))
        .add(
            Condition::all()
                .add(price::Column::CreatedAtUtc.eq(position.authored_at))
                .add(price::Column::PriceId.gt(position.price_id)),
        )
}

/// One **page** of the tenant's price history, in commit order, resuming
/// strictly after `after`.
///
/// The keyset walk under [`PriceRepo::list_history_page`]; its doc carries the
/// argument for the order and for what the order cannot promise.
async fn load_history_page(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    after: Option<HistoryPosition>,
    limit: u64,
) -> Result<Vec<PriceRecord>, RepoError> {
    let mut filter = Condition::all().add(price::Column::TenantId.eq(tenant_id));
    if let Some(position) = after {
        filter = filter.add(after_history_position(position));
    }
    let rows = price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(price::Column::CreatedAtUtc, Order::Asc)
        .order_by(price::Column::PriceId, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list price history: {e}")))?;

    hydrate_bands(runner, scope, tenant_id, &rows).await
}

/// Attach each row's band set and map the pair into the domain value.
///
/// **One band query for the whole page rather than one per row**: the bands
/// arrive already sorted, so grouping preserves the `from_qty` order the
/// read-side guarantee promises. Shared by the two page walks above so that a
/// row read by either arrives with the same geometry — a second copy of this
/// join is a second answer to "what bands does this row have", and the band
/// order is the read-side guarantee `inst-tb-order` was amended to make single.
async fn hydrate_bands(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    rows: &[price::Model],
) -> Result<Vec<PriceRecord>, RepoError> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.price_id).collect();
    let mut grouped: HashMap<Uuid, Vec<price_tier_band::Model>> = HashMap::new();
    // Chunked, and the grouping is what makes that free: `chunks` partitions by
    // `price_id`, so every band of one row lands in one query and the `from_qty`
    // order inside a group is the order this loop appends in. An empty `ids`
    // yields no chunk, so the empty case needs no guard of its own.
    for chunk in ids.chunks(MAX_IN_BINDS) {
        let bands = price_tier_band::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price_tier_band::Column::TenantId.eq(tenant_id))
                    .add(price_tier_band::Column::PriceId.is_in(chunk.to_vec())),
            )
            .order_by(price_tier_band::Column::PriceId, Order::Asc)
            .order_by(price_tier_band::Column::FromQty, Order::Asc)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("list plan price bands: {e}")))?;
        for band in bands {
            grouped.entry(band.price_id).or_default().push(band);
        }
    }

    rows.iter()
        .map(|row| {
            let bands = grouped.remove(&row.price_id).unwrap_or_default();
            to_record(row, &bands)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Statements.

/// The rows one page of [`gated_markets`]' walk reads into memory.
///
/// `readmodel_warm::FRONTIER_SCAN_LIMIT`'s value and, deliberately, **not** its
/// meaning: that constant truncates a scan and this one only breaks it up, because
/// the frontier scan discards the tail it can prove is uninteresting while a
/// truncated market count would be a wrong number reported to §7's alarm.
///
/// A thousand rather than a hundred because the walk pays one round trip per page
/// on a 60-second cadence and the value it computes moves in months (D-250, and the
/// PRD risk table's "an estimated eight months"), so the cost worth minimizing here
/// is statements, not the peak page. No operator has a decision to make about it,
/// which is why it is a constant and not a `JobsConfig` knob.
const GATED_MARKETS_PAGE: u64 = 1_000;

/// D-246: the catalog-wide GA backlog — **how many tenant-markets are gated now**.
///
/// # Why this reads the truth store and not the read model
///
/// D-246 assumed the read model. Measured, that is the wrong source and a much
/// harder one. The predicate is
/// `is_not_sellable_ga(row, TAX_ENGINE_GA) == row.tax_inclusive && !TAX_ENGINE_GA`,
/// and `TAX_ENGINE_GA` is a compile-time `false` — so a gated market is exactly a
/// market carrying a **published, non-grandfathered, tax-inclusive** row, which is
/// a statement about `pricing_price` and nothing else. The read model merely
/// projects it, so reading there would add the projector's lag and a
/// per-subject greatest-completed-≤-frontier walk (D-86, D-114) for a number the
/// store answers directly.
///
/// # Distinct over `(tenant, currency, region)`, and the tenant is part of it
///
/// §10's series is un-dimensioned, so this is one number for the process. A
/// market belongs to a tenant — two tenants gating `EUR/EU` are two markets
/// somebody has to act on, not one — so the tenant is part of the key rather
/// than collapsed out of it. Within one tenant this is the same dedup
/// `report_market_metrics` performs per plan, one level up.
///
/// Grandfathered generations are excluded for `sold_markets`' reason (ADR-0002):
/// a market reached only through a frozen generation can never be un-gated by
/// re-publishing, so counting it would put a market in a backlog no action can
/// clear.
///
/// **Cross-tenant by construction**, so the caller passes the scope that admits
/// it — `pin_frontier_repo::list_all`'s arrangement, and for its reason: a
/// per-tenant read cannot answer a catalog-wide question.
///
/// # Bounded, in pages of [`GATED_MARKETS_PAGE`]
///
/// This is [`gated_markets_in_pages`] spending the deployment constant; see it for
/// what the page buys and for the bound the walk does **not** have.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn gated_markets(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tax_engine_ga: bool,
) -> Result<i64, RepoError> {
    gated_markets_in_pages(runner, scope, tax_engine_ga, GATED_MARKETS_PAGE).await
}

/// [`gated_markets`], with the page size the caller's.
///
/// # Why the page exists
///
/// This read was `price::Entity::find()….all(runner)` — the **only** unbounded
/// read on any of the three tickers, where `readmodel_warm`'s frontier scan,
/// `window_activation`'s due set and both of D-163's ref bounds each carry an
/// explicit cap and a documented degradation. Its stated bound was *"the cost is
/// bounded by the thing being measured — the gated rows **are** the backlog"*, and
/// that is not one: the backlog is every published, non-grandfathered,
/// tax-inclusive row of every tenant, which §10's risk table expects to stand *"an
/// estimated eight months"*, re-read whole every 60 seconds.
///
/// # What the page bounds, and what it deliberately does not
///
/// It bounds the **rows held in memory at once** to one page. It does *not*
/// truncate: the walk runs to the end of the catalog and the answer is exact,
/// because this number is a gauge feeding §7's alarm and a truncated count is a
/// *wrong* number rather than a late one — the opposite of `FRONTIER_SCAN_LIMIT`,
/// where the discarded tail is provably the part that cannot be stale. What still
/// grows with the catalog is the deduplicated market set, and that is the answer's
/// own size: `(tenant, currency, region)` triples somebody has to act on, not rows.
///
/// So the residual cost is one statement per [`GATED_MARKETS_PAGE`] gated rows, on a
/// 60-second cadence, and the reason that is the right trade rather than a smaller
/// page is stated at the constant.
///
/// The `tax_engine_ga` short-circuit is **above** the walk: past GA there is no
/// backlog to page through at all.
///
/// # The cursor is backend-agnostic even though `uuid` ordering is not
///
/// Postgres orders `uuid` by its bytes and the `SQLite` mirror orders the same column
/// as text, so the two backends walk these pages in **different orders** — the shape
/// `tests/sqlite_window_activation.rs` exists to arm against on the timestamp column.
/// It is harmless here, and by construction rather than by luck: the `ORDER BY` and
/// the `>` compare read the same column through the same backend's own collation, so
/// whichever total order that is, every matching row is visited exactly once. The
/// answer is a set's cardinality and is invariant under the order the set is built
/// in. A page **bound** would not be — a truncating cap would keep a different subset
/// per backend — which is a second reason this walk does not truncate.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn gated_markets_in_pages(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tax_engine_ga: bool,
    page: u64,
) -> Result<i64, RepoError> {
    // **The flag is a parameter so this site is reachable from the constant.**
    // It was reasoned about in prose here and named nowhere in the code, while
    // `metrics.rs` read `TAX_ENGINE_GA` directly — two consumers of one
    // predicate, diverging the moment it moves. On the day GA lands the alarm
    // correctly stops firing and this gauge would have kept publishing the full
    // count of published tax-inclusive markets forever: §7's backlog series
    // pinned at a number no action can clear, which is the opposite of what the
    // D-246 rebuild was for.
    //
    // Whoever flips the constant greps for `TAX_ENGINE_GA`. Before this they
    // found `tax_display.rs` and `metrics.rs`, and did not find this file.
    if tax_engine_ga {
        return Ok(0);
    }
    // A zero page would read nothing and never come back short of its bound, so the
    // walk would spin forever on an empty statement. Clamped rather than refused:
    // the production caller spends a constant and this parameter exists so a test
    // can span pages, so an error return would be a `Result` arm nothing produces.
    // `Ord::max`, spelled out: sea-orm 2's `ExprTrait` is blanket-implemented
    // for every `T`, so a bare `.max(..)` on an integer is ambiguous between the
    // numeric maximum and a SQL `MAX(..)` expression builder. The compiler
    // refuses rather than choosing, which is the good outcome; this names which
    // one was always meant.
    let page = Ord::max(page, 1);
    // **Deduplicated here rather than by the database**, and the reason is the
    // scoping wrapper rather than preference: `SecureSelect` exposes `all`,
    // `one`, `count` and `filter` and deliberately no `select_only` / `distinct`,
    // because a projection that dropped the scope columns would be a read the
    // scope can no longer be checked against. So the rows come back whole and the
    // set is built here — a page at a time, which is what keeps "the rows come back
    // whole" from meaning *all* of them at once.
    let mut markets: BTreeSet<(Uuid, String, String)> = BTreeSet::new();
    // The keyset cursor. `price_id` is the primary key on its own — not
    // `(tenant_id, price_id)` — so one column totally orders the table and the
    // cursor needs no compound compare.
    let mut after: Option<Uuid> = None;
    loop {
        let mut filter = Condition::all()
            .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str()))
            .add(price::Column::TaxInclusive.eq(true))
            .add(
                price::Column::PriceEligibility
                    .ne(PriceEligibility::ExistingGrandfathered.as_str()),
            );
        if let Some(above) = after {
            filter = filter.add(price::Column::PriceId.gt(above));
        }
        let rows = price::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(filter)
            .order_by(price::Column::PriceId, Order::Asc)
            .limit(page)
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read gated rows: {e}")))?;
        // A page under its bound is the end of the table above the cursor. Read
        // before the rows are consumed, since the loop moves them into the set.
        let exhausted = u64::try_from(rows.len()).unwrap_or(u64::MAX) < page;
        for row in rows {
            after = Some(row.price_id);
            markets.insert((row.tenant_id, row.currency, row.region));
        }
        if exhausted {
            break;
        }
    }

    i64::try_from(markets.len())
        .map_err(|_| RepoError::Db("gated market count exceeds i64".to_owned()))
}

// ---------------------------------------------------------------------------

/// Read one row by identity, scoped.
async fn load_row(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<Option<price::Model>, RepoError> {
    price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PriceId.eq(price_id)),
        )
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read price row: {e}")))
}

/// The canonical scope key of one price row, through whichever runner the caller
/// holds.
///
/// The window store's question, and it is deliberately narrower than
/// [`PriceRepo::find`]'s. A window is bound to a **row** (§6 makes the binding
/// immutable) while non-overlap and coverage are per **key**, so every window read
/// has to resolve one into the other — and the band set, the authored shape and
/// the entity tag are no part of that question. Answering it with a whole
/// [`PriceRecord`] would put a second query on the path for values the caller
/// discards, on the publish path and inside every window mutation's transaction.
///
/// `None` means no such row **or** one outside the caller's scope, deliberately
/// the same answer either way for [`PriceRepo::find`]'s reason.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a stored axis is outside the enumeration its CHECK admits, or when the
/// ten axes together do not form a legal key.
pub async fn load_scope_key(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<Option<ScopeKey>, RepoError> {
    let Some(row) = load_row(runner, scope, tenant_id, price_id).await? else {
        return Ok(None);
    };
    to_scope_key(&row).map(Some)
}

/// Every row of one plan paired with its canonical scope key.
///
/// The window plane's other question: which of a plan's rows share a key, because
/// that is the set non-overlap is checked across and the set coverage is computed
/// per.
///
/// **No lifecycle filter**, and that is the substance rather than an omission.
/// `chk_pricing_price_lifecycle_state` admits three states and a window's
/// key-mates include every one of them: a `superseded` predecessor still occupies
/// the key over its shortened interval (which is exactly what a supersession
/// leaves behind), and a `draft` row is one publish away from being current on the
/// key its window would then take effect on. A filter here would let two
/// intervals on one key pass the overlap check by having one of them belong to a
/// row in a state the filter dropped.
///
/// Ordered by `price_id` ascending, [`load_for_plan`]'s order and on both
/// backends for its reason.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a stored axis is outside the enumeration its CHECK admits.
pub async fn load_scope_keys_for_plan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<Vec<(Uuid, ScopeKey)>, RepoError> {
    price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PlanId.eq(plan_id.get())),
        )
        .order_by(price::Column::PriceId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read the scope keys of plan {plan_id}: {e}")))?
        .iter()
        .map(|row| Ok((row.price_id, to_scope_key(row)?)))
        .collect()
}

/// The canonical scope keys of a **named set** of price rows, in `price_id` order.
///
/// [`load_scope_keys_for_plan`]'s question asked the other way round, and it
/// exists for the one reader whose starting point is the window plane rather than
/// a plan: `window_repo::list_page` walks `pricing_price_window` — a table
/// carrying `price_id` and no plan reference at all — and every
/// [`WindowRecord`](crate::infra::storage::repo::window_repo::WindowRecord) it
/// renders has to carry its row's key. Resolving that key per window would be one
/// query per row of the page; this is the same resolution in **one** query for
/// the whole page.
///
/// The **no lifecycle filter** paragraph on [`load_scope_keys_for_plan`] applies
/// here verbatim and for its own reason: a window belongs to whatever row it
/// names, in whatever state that row stands.
///
/// An empty `price_ids` yields an empty result without touching the store — an
/// `IN ()` is a predicate no backend spells the same way, and the answer is known.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
/// when a stored axis is outside the enumeration its CHECK admits.
pub async fn load_scope_keys_for_ids(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_ids: &[Uuid],
) -> Result<Vec<(Uuid, ScopeKey)>, RepoError> {
    if price_ids.is_empty() {
        return Ok(Vec::new());
    }
    price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PriceId.is_in(price_ids.iter().copied())),
        )
        .order_by(price::Column::PriceId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| {
            RepoError::Db(format!(
                "read the scope keys of {} rows: {e}",
                price_ids.len()
            ))
        })?
        .iter()
        .map(|row| Ok((row.price_id, to_scope_key(row)?)))
        .collect()
}

/// The **published** rows a mass-repricing run's selector reaches, in `price_id`
/// order (`design/12-operator-efficiency.md` `inst-mr-api`, `inst-mp-grandfathered`;
/// D-307).
///
/// One `AND` over the axes the selector names, and nothing for the axes it does
/// not — so a selector naming `currency` alone is *every published row of that
/// currency*, which is §2's own "a currency segment".
///
/// # Why the state filter is `published` alone
///
/// The run's domain is the published plane, stated three ways by the design set:
/// `inst-mp-standard` applies every row through the Foundation's versioned path,
/// D-88's supersession units are what that path is for, and D-118 gives the
/// **draft** plane to the bulk import instead — with `IMPORT_TARGETS_PUBLISHED`
/// naming a repricing run as the remedy for a draft-plane row that strays onto a
/// published key. A `superseded` row is excluded for a sharper reason than
/// symmetry: it is no longer the current row on its key, so repricing one would
/// author a successor to a predecessor and put two live successors on one key.
///
/// # The grandfathered class is excluded unless the selector names it
///
/// `inst-mp-grandfathered` clause 1, and [`RunSelector::admits_grandfathered`]
/// carries the argument. The exclusion is expressed as a `<>` on the eligibility
/// column rather than by filtering the result, because a row that never enters the
/// set cannot be silently dropped from it later.
///
/// **Only ids come back.** The apply groups by plan (D-134) and reads content per
/// row inside its own transaction; hydrating whole [`PriceRecord`]s here would
/// load every band of every selected row to build a journal that stores neither.
/// [`load_plan_ids`] is the grouping query when the apply needs it.
///
/// # There is no page bound, and that is a real edge
///
/// An unconstrained selector expands over the tenant's whole published catalog in
/// one `Vec`. §5 declares no cap for this surface and [`LimitsConfig`] holds none,
/// so imposing one here would be this gear inventing a limit the design set has
/// not ratified; what the run *is* bounded by is O3's throughput SLO, which is the
/// apply's concern. Stated rather than papered over.
///
/// [`RunSelector::admits_grandfathered`]: crate::domain::repricing::RunSelector::admits_grandfathered
/// [`LimitsConfig`]: crate::config::LimitsConfig
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn load_published_for_selector(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    selector: &RunSelector,
) -> Result<Vec<Uuid>, RepoError> {
    let mut filter = Condition::all()
        .add(price::Column::TenantId.eq(tenant_id))
        .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str()));
    if !selector.admits_grandfathered() {
        filter = filter.add(
            price::Column::PriceEligibility.ne(PriceEligibility::ExistingGrandfathered.as_str()),
        );
    }
    if let Some(plan_id) = selector.plan_id {
        filter = filter.add(price::Column::PlanId.eq(plan_id.get()));
    }
    if let Some(currency) = selector.currency.as_ref() {
        filter = filter.add(price::Column::Currency.eq(currency.as_str()));
    }
    if let Some(region) = selector.region.as_ref() {
        filter = filter.add(price::Column::Region.eq(region.as_str()));
    }
    if let Some(phase) = selector.phase {
        filter = filter.add(price::Column::Phase.eq(phase.get()));
    }
    if let Some(eligibility) = selector.price_eligibility {
        filter = filter.add(price::Column::PriceEligibility.eq(eligibility.as_str()));
    }
    if let Some(charge_kind) = selector.charge_kind {
        filter = filter.add(price::Column::ChargeKind.eq(charge_kind.as_str()));
    }
    if let Some(cohort) = selector.cohort {
        // The column holds the domain token — `none`, or the generation's epoch
        // milliseconds — so the match is against `Cohort`'s own rendering rather
        // than against an RFC 3339 instant. `read_cohort` parses this exact
        // spelling back, which is what keeps the two ends of the axis comparable.
        filter = filter.add(price::Column::Cohort.eq(cohort.to_string()));
    }
    if let Some(meter) = selector.meter.as_ref() {
        filter = filter.add(price::Column::Meter.eq(meter.as_str()));
    }
    if let Some(dimension_key) = selector.dimension_key.as_ref() {
        filter = filter.add(price::Column::DimensionKey.eq(dimension_key.as_str()));
    }

    Ok(price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .order_by(price::Column::PriceId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("expand a repricing run's selector: {e}")))?
        .into_iter()
        .map(|row| row.price_id)
        .collect())
}

/// Which plan each of a set of price rows belongs to, **across tenants**.
///
/// The activation sweep's question, and the only one it has about
/// `pricing_price`. Window events are ordered per `(tenant, plan)` (§7) and the
/// outbox aggregate is therefore the plan, while `pricing_price_window` carries
/// a `price_id` and no plan — so a cross-tenant sweep holding a page of due
/// windows needs exactly this map and nothing else on the row.
///
/// **Deliberately not [`load_scope_key`] per row.** The key is ten axes and
/// resolving it costs one query per window; the sweep needs one column, and a
/// pass over a thousand due windows would otherwise take a thousand round trips
/// to discard nine axes each time. The window *mutation* paths still resolve
/// the whole key, because non-overlap and coverage are per key — this is the one
/// caller whose question really is "which aggregate does this event belong to".
///
/// **No `tenant_id` argument**, which is what makes it the sweep's: the caller
/// reads under the sanctioned [`AccessScope::allow_all`] system scope and the
/// scope is the only gate. A per-tenant caller passes its own scope and gets its
/// own rows, because `price_id` is a primary key and cannot be another tenant's
/// under a narrowed scope.
///
/// Ordered by `price_id`, [`load_for_plan`]'s order and for its reason.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure.
pub async fn load_plan_ids(
    runner: &impl DBRunner,
    scope: &AccessScope,
    price_ids: impl Iterator<Item = Uuid>,
) -> Result<Vec<(Uuid, PlanId)>, RepoError> {
    let ids: Vec<Uuid> = price_ids.collect();
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(MAX_IN_BINDS) {
        out.extend(
            price::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(Condition::all().add(price::Column::PriceId.is_in(chunk.to_vec())))
                .all(runner)
                .await
                .map_err(|e| RepoError::Db(format!("read the plans of a price-row set: {e}")))?
                .into_iter()
                .map(|row| (row.price_id, PlanId::new(row.plan_id))),
        );
    }
    // Sorted here rather than by `ORDER BY`, which can only order within a chunk.
    out.sort_by_key(|(price_id, _)| *price_id);
    Ok(out)
}

/// Read one row's bands, ascending by lower bound. See the module doc for why
/// the order is here and not left to the caller.
async fn load_bands(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<Vec<price_tier_band::Model>, RepoError> {
    price_tier_band::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_tier_band::Column::TenantId.eq(tenant_id))
                .add(price_tier_band::Column::PriceId.eq(price_id)),
        )
        .order_by(price_tier_band::Column::FromQty, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read price tier bands: {e}")))
}

/// Read one whole record — the row and its bands — or nothing.
async fn load_record(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<Option<PriceRecord>, RepoError> {
    let Some(row) = load_row(runner, scope, tenant_id, price_id).await? else {
        return Ok(None);
    };
    let bands = load_bands(runner, scope, tenant_id, price_id).await?;
    to_record(&row, &bands).map(Some)
}

/// [`PriceRepo::find`] through a runner the caller already holds.
///
/// The `_on` half of that method, in this module's existing split: the method
/// opens a connection of its own, and an orchestrator that must read inside its
/// **writing** transaction (D-176) cannot use one. `infra::grandfather` is the
/// caller — it reads the generation, judges it and swaps on it, and a read taken
/// outside the transaction would make its class, state and horizon checks hints
/// rather than preconditions.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`] when
/// the stored row cannot be read as the domain value its columns are
/// `CHECK`-constrained to hold.
pub async fn find_on(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<Option<PriceRecord>, RepoError> {
    load_record(runner, scope, tenant_id, price_id).await
}

/// The draft or published row already on `key`, if there is one.
///
/// A `superseded` row is deliberately not an occupant: it is history and is not
/// the current row on its key (§4.3), so refusing a new draft because of one
/// would make a key unusable forever after its first reprice. It is also the
/// only non-occupant state there is — the price-row state machine
/// (`03-price-structure.md` §4) has three states and `retired` is not one of
/// them, which `chk_pricing_price_lifecycle_state` now says out loud.
///
/// A key can hold a draft **and** a published row at once — the partial `UNIQUE`
/// only forbids two published ones, and [`insert_successor_draft_on`] is the path
/// that reaches that state (D-88's row half; **D-195** is why it is a second door
/// rather than a relaxation of this reader). Both block a new draft equally, so
/// the query takes the lowest `price_id` rather than whichever row the plan index
/// happened to reach first: a refusal that named a different row on each attempt
/// would send an author looking in a different place each time.
///
/// That collapse is also why the supersession door does not use this reader: it
/// needs the two planes told apart, and [`read_key_occupants`] is what tells them.
async fn find_key_occupant(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    key: &ScopeKey,
) -> Result<Option<price::Model>, RepoError> {
    price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            scope_key_filter(tenant_id, key).add(price::Column::LifecycleState.is_in([
                LifecycleState::Draft.as_str(),
                LifecycleState::Published.as_str(),
            ])),
        )
        .order_by(price::Column::PriceId, Order::Asc)
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read scope-key occupant: {e}")))
}

/// The draft and the published row standing on one key, told apart.
///
/// [`find_key_occupant`] answers "is this key taken" and collapses the two states
/// into one `Option` deliberately — that is the authoring door's question, and
/// both answers mean the same thing to it. The supersession door asks a different
/// question of the same two rows: the published one is **what it supersedes** and
/// the draft one is **what refuses it** (D-195), so it needs the pair and cannot
/// use a reader that hands back whichever has the lower `price_id`.
///
/// At most one of each can exist — the two partial `UNIQUE` indexes over the
/// scope key say so, one per plane (§3.7) — so a third row on this key is a
/// corrupt store rather than a case, and the read reports it as one rather than
/// silently keeping the first.
struct KeyOccupants {
    /// The key's current row, if it has one.
    published: Option<price::Model>,
    /// A draft already standing on the key, if there is one.
    draft: Option<price::Model>,
}

/// Read both planes of one key in a single statement.
async fn read_key_occupants(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    key: &ScopeKey,
) -> Result<KeyOccupants, RepoError> {
    let rows = price::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            scope_key_filter(tenant_id, key).add(price::Column::LifecycleState.is_in([
                LifecycleState::Draft.as_str(),
                LifecycleState::Published.as_str(),
            ])),
        )
        .order_by(price::Column::PriceId, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read scope-key occupants: {e}")))?;

    let mut occupants = KeyOccupants {
        published: None,
        draft: None,
    };
    for row in rows {
        let plane = if row.lifecycle_state == LifecycleState::Published.as_str() {
            &mut occupants.published
        } else {
            &mut occupants.draft
        };
        if let Some(first) = plane {
            return Err(RepoError::CorruptRow(format!(
                "{key} carries two {} rows, {} and {}; the plane's partial UNIQUE admits one",
                row.lifecycle_state, first.price_id, row.price_id
            )));
        }
        *plane = Some(row);
    }
    Ok(occupants)
}

/// Refuse a canonical scope key whose eligibility class may not be superseded at
/// all (Foundation §4.3).
///
/// An `existing_grandfathered` row is **immutable in price and MUST NOT be
/// superseded**, and S7 §1.7's UC table says the same ("only tightening
/// `grandfatherUntil` is allowed") — and that exception is a
/// door rather than a sentence: [`tighten_grandfather_until`], reached through
/// `infra::grandfather`. So the two halves of §4.3's clause now have one
/// implementation each, and a reader arriving here asking "then how *does* the
/// horizon move" has somewhere to go.
///
/// Its absence was money, not tidiness, and compounded: the successor would rewrite
/// the retained cohort's price, and the shorten would walk that generation's coverage
/// inward while nothing enforced D-04's `inst-co-bounds` at all, stranding bound
/// subscribers mid-cycle with no guard on either side. The other side of that pair
/// is closed —
/// `infra::window`'s `refuse_horizon_uncovered` judges the three acts
/// `infra::window::mutate_in` performs on a grandfathered key against
/// `grandfather_until` plus W6's margin. **Not every window mutation**, which this
/// sentence claimed: cutover, supersession, retirement and the activation sweep
/// reach `window_repo` without passing it, and `window_repo`'s own roster now
/// carries which of the four are argued shut and which is merely dormant. This
/// door is unaffected either way: it refuses the supersession outright, one act
/// earlier, and it is *why* the supersession path needs no bound of its own.
///
/// # Why it is a function of the key alone, and has two callers
///
/// **The door is the floor, not the only asker.** Every caller of
/// [`insert_successor_draft_on`] is refused here; the orchestrator holds the whole
/// canonical scope key exactly as this door does, so `infra::supersession`'s unit
/// asks the same question one step earlier. D-156 puts the `CatalogVersion`
/// request before the writes and this refusal is **permanent**, so a handle
/// requested for an act no retry can ever complete would stand pending forever —
/// the class `PublishService::commit` hoists `refuse_unpublishable_predecessor`
/// for. (The unit guard compares `PriceRow` fields and sees no eligibility class;
/// `compose_windows` sees intervals and states. Neither can ask.)
///
/// One spelling, two callers, so the two cannot disagree about which classes are
/// immutable.
///
/// # Errors
/// [`RepoError::NotSupersedable`] naming the key and the class.
pub fn refuse_unsupersedable_class(key: &ScopeKey) -> Result<(), RepoError> {
    if key.price_eligibility() == PriceEligibility::ExistingGrandfathered {
        return Err(RepoError::NotSupersedable {
            subject: SUBJECT.to_owned(),
            id: key.to_string(),
            state: "an existing_grandfathered generation, which is immutable in price and may not \
                    be superseded; only its grandfatherUntil may be tightened"
                .to_owned(),
        });
    }
    Ok(())
}

/// Move a **published** generation's `grandfather_until` earlier, inside the
/// caller's transaction (`inst-gs-bound`, `inst-gs-tighten`).
///
/// The one in-place mutation of a published row's *content* Foundation §4.3
/// sanctions, beside the `published → superseded` flip [`supersede_row`] performs.
/// Everything about it that looks unusual is that sentence's consequence.
///
/// # The compare-and-swap is on the horizon, because the row version cannot move
///
/// Every other mutating verb in this crate swaps on `row_version`. This one cannot:
/// D-141 puts that column on the published row's **frozen** whitelist — *"neither
/// sanctioned in-place mutation moves it: not the `lifecycle_state` flips, not the
/// monotonic `grandfather_until` tightening"* — and the physical guard enforces it,
/// so a statement that advanced the version would be refused by the trigger rather
/// than by a rule. A frozen counter is also a useless premise: it reads the same
/// before and after every tightening, so it cannot tell a caller who read the row
/// an hour ago from one who read it after somebody else's act.
///
/// What can serve is the **column itself**. The horizon is monotone by rule, so its
/// current value is a version of the only fact this statement changes: `prior` is
/// what the transaction read, it rides the `WHERE`, and a concurrent tightening
/// between the read and the write moves it and takes the row out of the predicate.
/// The loser is answered [`RepoError::ConcurrentMutation`] rather than silently
/// overwriting a horizon a second principal approved. `IS NULL` and `= <instant>`
/// are two spellings of that predicate because SQL equality does not hold over
/// null, and the null case is `inst-gs-bound`'s whole arm.
///
/// The `lifecycle_state = 'published'` conjunct is in the same predicate rather than
/// trusted from the read above it, for [`update_draft`]'s stated reason: a read is a
/// hint and the statement is the guard.
///
/// # What it does not judge
///
/// The direction of travel (`domain::grandfather::check_tightening`), the row's
/// eligibility class (D-147), and D-04's coverage span are all the orchestrator's —
/// `infra::grandfather` asks all three against facts this repository does not hold
/// (the plan's shape, the key's window set). What is here is the quantum, the swap
/// and the disambiguation of a swap that matched nothing.
///
/// # Errors
/// [`RepoError::TimestampPrecisionExceeded`] for an instant finer than the
/// millisecond quantum (D-144); [`RepoError::NotFound`] when no such row is visible
/// to `scope`; [`RepoError::NotDraft`] carrying the state when the row is not
/// published; [`RepoError::ConcurrentMutation`] when the horizon moved under the
/// transaction; [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when the row reads back unusable.
pub async fn tighten_grandfather_until(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    prior: Option<OffsetDateTime>,
    horizon: OffsetDateTime,
) -> Result<PriceRecord, RepoError> {
    check_authored_instant("grandfatherUntil", Some(horizon))?;
    let prior_matches = prior.map_or_else(
        || price::Column::GrandfatherUntil.is_null(),
        |at| price::Column::GrandfatherUntil.eq(at),
    );
    let result = price::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(price::Column::GrandfatherUntil, Expr::value(horizon))
        .filter(
            Condition::all()
                .add(price::Column::TenantId.eq(tenant_id))
                .add(price::Column::PriceId.eq(price_id))
                .add(price::Column::LifecycleState.eq(LifecycleState::Published.as_str()))
                .add(prior_matches),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("tighten price grandfather_until: {e}")))?;
    if result.rows_affected != 1 {
        return Err(refuse_untightenable(runner, scope, tenant_id, price_id, prior).await);
    }
    load_record(runner, scope, tenant_id, price_id)
        .await?
        .ok_or_else(|| not_found(price_id))
}

/// Say precisely why [`tighten_grandfather_until`]'s swap matched nothing.
///
/// [`refuse`]'s shape for the horizon door's predicate. Taken **after** the count
/// comes back short rather than before, because the read that preceded the
/// statement is not the guard.
///
/// Two outcomes and not three. An absent row is [`RepoError::NotFound`]; anything
/// else is a **serialization** answer, because the orchestrator read this row's
/// class, state and horizon inside this same transaction and refused every shape
/// it could refuse. A row that no longer satisfies the predicate therefore moved
/// under the transaction — the horizon tightened by somebody else, or the row
/// flipped `superseded` — and both leave the caller the same act: read the
/// generation again and decide whether their instant is still a tightening. The
/// message names the horizon at both ends, which is the value that decides it.
async fn refuse_untightenable(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    prior: Option<OffsetDateTime>,
) -> RepoError {
    let row = match load_row(runner, scope, tenant_id, price_id).await {
        Err(err) => return err,
        Ok(None) => return not_found(price_id),
        Ok(Some(row)) => row,
    };
    RepoError::ConcurrentMutation {
        aggregate: format!(
            "price row {price_id}: this transaction read a published generation whose \
             grandfatherUntil was {}, and the store now holds a {} row whose grandfatherUntil is \
             {}",
            prior.map_or_else(|| "null".to_owned(), format_rfc3339),
            row.lifecycle_state,
            row.grandfather_until
                .map_or_else(|| "null".to_owned(), format_rfc3339)
        ),
    }
}

/// Author the supersession unit's successor **draft** beside the published row it
/// will supersede, inside the caller's transaction, and answer the predecessor it
/// found.
///
/// # This is the other door, and its precondition is the authoring door's inverted
///
/// [`PriceRepo::create_draft`] refuses a key held by a `draft` **or** a
/// `published` row, and that stays true: `inst-pr-return` (D-21) puts scope-key
/// duplication in the save-time row-local set, and the bulk plane refuses the same
/// shape per row. So the draft-beside-published shape §3.7's two disjoint partial
/// `UNIQUE`s permit has no authoring door at all — which is correct, and is why
/// this one exists (**D-195**). Occupancy is a property of the door rather than of
/// the table, and this door's reading of the same two rows is the mirror image:
///
/// - a **published** occupant is the **precondition**. It is the row being
///   superseded, and its absence means the key has no current row to supersede —
///   the same presupposition of current coverage `inst-su-compose` states from the
///   window side when it fails compose on a dormant key, read here off the row
///   plane. The remedy the design names for that key is a plain publish plus a
///   window schedule, not a supersession, so the refusal is `NotFound` on the key
///   rather than a conflict inviting a retry.
/// - a **draft** occupant is the **refusal**. On a key carrying a published row a
///   hand-authored draft is impossible by the paragraph above, so a draft here is
///   a composition that already staged its successor. Between the two doors, one
///   draft per key therefore stays the most that is admitted, which is the
///   guarantee §3.7's D-148 argument rests on.
///
/// The order of the two is not arbitrary: a key with no current row is answered as
/// unsupersedable even when a draft stands on it, because "publish this key
/// instead" is the actionable half and the draft is beside the point.
///
/// # The refusal a draft occupant gets is the floor, not the whole answer
///
/// `DUPLICATE_SCOPE_KEY` is what the store can say. The operator-facing answer for
/// a key a pending unit holds is `PENDING_CHANGE_UNIT_EXISTS`
/// (`inst-co-single-pending`), and it belongs to the approval plane, which knows
/// about units; this refusal is what remains for a draft no live unit explains.
/// The read is also still a **read**, so two concurrent composers can both pass
/// it and the draft-plane partial `UNIQUE` decides — the same race, with the same
/// narrowing owed to the surface, that [`PriceRepo::create_draft`]'s doc sets out.
///
/// # The link is stamped here
///
/// `supersedes_price_id` is written from the same read that validated the key
/// (D-127 requires the successor to carry it), overwriting whatever the caller sent —
/// this door is the only reader positioned to be right about it.
///
/// **It is not the only *writer*.** `PriceContentView` carries
/// `supersedes_price_id` as a request field and both `POST …/prices` and
/// `PATCH …/prices/{priceId}` plumb it through unvalidated, so any client on the
/// authoring plane can give a hand-authored draft an arbitrary link. Stamping here
/// therefore makes the link right on *this* path and cannot make a disagreement
/// unexpressible — which is why `commit_supersession_rows` checks the pair rather than
/// trusting the stamp, with key equality as the load-bearing half.
///
/// # What this does **not** do
///
/// It does not flip the predecessor. That is the commit's move and it is ordered
/// **before** the successor's publish (`inst-su-commit`, D-195): §3.7 admits one
/// published row per key, so a successor flipped while its predecessor still reads
/// `published` dies on `uq_pricing_price_scope_key_current` as a raw driver error.
/// After compose, both rows legitimately stand on the key and the predecessor is
/// untouched.
///
/// **What it does beyond the three writes**: it reaches [`record_price_mutation`] through
/// [`write_prepared`], so it appends the row's audit entry **and voids the plan's
/// pending `plan_revision` units** ([`crate::infra::approval::void_pending_units_of`],
/// keyed on the plan rather than the key). Composing on one key therefore voids a
/// pending plan-revision unit whose change set is about *another* key, which is wider
/// than `inst-co-single-pending`'s per-key rule and is pre-existing
/// `void_pending_units_of` semantics rather than this door's invention.
///
/// **"Every `submitted` unit of the plan" stood here and was measured false**:
/// `approval_repo::void_pending_for_plan` filters `subject_kind = 'plan_revision'`, so it
/// reaches neither a `window` unit nor a `price_unit` one — and a supersession's own unit is a
/// `price_unit`. The consequence the sentence was warning about therefore does not exist: this
/// door **cannot** void the unit composing it, so the ordering the orchestrator keeps (stage,
/// then open) is not a defence against that. It is kept for the reason a probe measured instead
/// — the unit's content pin has to cover the staged successor, or every attempt to approve it
/// answers `APPROVAL_CONTENT_MISMATCH` — and
/// [`crate::infra::supersession::submitted_for_approval`] carries that argument. What the
/// narrow filter leaves owed is recorded in the register: a row edit **orphans** a pending
/// window or supersession unit rather than voiding it.
///
/// # Errors
/// [`RepoError::NotFound`] when the key has no current row to supersede;
/// [`RepoError::DuplicateScopeKey`] naming the draft already standing on it;
/// [`RepoError::CorruptRow`] when one plane of the key carries two rows;
/// and everything [`PriceRepo::create_draft`] answers for the content itself —
/// [`RepoError::GrandfatherHorizonOffClass`], [`RepoError::ValueOutOfRange`],
/// [`RepoError::Db`].
pub async fn insert_successor_draft_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    draft: NewPriceDraft,
) -> Result<(PriceRecord, Uuid), RepoError> {
    let mut prepared = prepare_draft(tenant_id, draft)?;
    let key = prepared.record.scope_key.clone();

    // Foundation §4.3, refused before the key is even read. See
    // [`refuse_unsupersedable_class`] — the floor beneath every caller, and the
    // orchestrator asks it earlier for D-156's reason.
    refuse_unsupersedable_class(&key)?;

    let occupants = read_key_occupants(runner, scope, tenant_id, &key).await?;

    let Some(predecessor) = occupants.published else {
        return Err(RepoError::NotFound {
            subject: "current price row on canonical scope key".to_owned(),
            id: key.to_string(),
        });
    };
    if let Some(standing) = occupants.draft {
        return Err(duplicate_key(&key, &standing));
    }

    prepared.record.supersedes_price_id = Some(predecessor.price_id);
    prepared.row.supersedes_price_id = Set(Some(predecessor.price_id));

    let record = write_prepared(runner, scope, tenant_id, prepared).await?;
    Ok((record, predecessor.price_id))
}

/// Write the price row itself.
async fn insert_price(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row: price::ActiveModel,
) -> Result<(), RepoError> {
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(scope, &row)
        .map_err(|e| RepoError::Db(format!("pricing_price scope: {e}")))?
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("insert pricing_price: {e}")))?;
    Ok(())
}

/// Write a band set, one row at a time.
///
/// One statement per band rather than a multi-row insert, because
/// `scope_with_model` validates the tenant of the `ActiveModel` it is given and
/// that validation is the second half of the tenant rule the module doc states:
/// the value is copied from the parent, and then checked against the caller's
/// scope. A band set is a handful of rows, so the cost of being checked is a
/// handful of statements.
async fn insert_bands(
    runner: &impl DBRunner,
    scope: &AccessScope,
    bands: Vec<price_tier_band::ActiveModel>,
) -> Result<(), RepoError> {
    for band in bands {
        price_tier_band::Entity::insert(band.clone())
            .secure()
            .scope_with_model(scope, &band)
            .map_err(|e| RepoError::Db(format!("pricing_price_tier_band scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_price_tier_band: {e}")))?;
    }
    Ok(())
}

/// Drop a row's whole band set.
async fn delete_bands(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
) -> Result<(), RepoError> {
    price_tier_band::Entity::delete_many()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(price_tier_band::Column::TenantId.eq(tenant_id))
                .add(price_tier_band::Column::PriceId.eq(price_id)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("delete price tier bands: {e}")))?;
    Ok(())
}

/// The row, when it is a draft standing at exactly `expected`; `None` when it
/// is anything else, including absent.
///
/// Asked so [`PriceRepo::update_draft`] and [`PriceRepo::delete_draft`] can
/// refuse *before* they touch a band: both replace or remove the band set, and
/// a band statement against a frozen parent is answered by the band table's own
/// trigger with a raw database error carrying no state. It is **not** the guard
/// — each statement's own conjunction is — and it is deliberately not trusted as
/// one: between this read and that statement a concurrent publish may land, and
/// what closes the window is the rollback, not this answer.
///
/// It hands the row back rather than a `bool` so the one caller that needs a
/// stored column — `update_draft`, comparing a submitted grandfathering horizon
/// against the row's eligibility class — does not read the row a second time.
async fn mutable_draft(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    expected: RowVersion,
) -> Result<Option<price::Model>, RepoError> {
    let Some(row) = load_row(runner, scope, tenant_id, price_id).await? else {
        return Ok(None);
    };
    let state = read_lifecycle(&row.lifecycle_state)?;
    let current = read_row_version(price_id, row.row_version)?;
    Ok((state.is_content_mutable() && current == expected).then_some(row))
}

/// Refuse a grandfathering horizon on a class that may not carry one.
///
/// `chk_pricing_price_grandfather_until` pairs `grandfather_until` with
/// `price_eligibility = 'existing_grandfathered'`, and `grandfather_until` is
/// ordinary caller-supplied content on the draft plane — so without this check
/// the pairing is discovered by the driver, reaches the caller as
/// [`RepoError::Db`] and renders as a 500 for a request whose author only has to
/// clear one field.
///
/// The pairing was a physical CHECK with no rule behind it when this refusal was
/// written; D-147 states it normatively and gives it
/// `GRANDFATHER_UNTIL_FORBIDDEN`, so the refusal now carries a code of its own
/// instead of borrowing the generic bad-request answer. One direction only: a
/// grandfathered row with no horizon is indefinite, and this check says nothing
/// about it.
///
/// # Errors
/// [`RepoError::GrandfatherHorizonOffClass`] naming the class the key holds.
fn check_grandfather_horizon(
    horizon: Option<OffsetDateTime>,
    eligibility: PriceEligibility,
) -> Result<(), RepoError> {
    if horizon.is_none() || matches!(eligibility, PriceEligibility::ExistingGrandfathered) {
        return Ok(());
    }
    Err(RepoError::GrandfatherHorizonOffClass {
        eligibility: eligibility.to_string(),
    })
}

/// Resolve the row's `(meter, dimensionKey)` line against its key's (D-196).
///
/// **Each axis is derived from wherever the wire can actually say it**, and that
/// is what makes this the mirror image of [`authored_content`] rather than an
/// inconsistency with it. `charge_kind` is expressible only on the **key** view,
/// so `content_of` fills the row's copy with a placeholder and the door rewrites
/// it *from the key*. The usage line is expressible only on the **content**
/// view — `ScopeKeyRequest` has no such member — so the row's fields are the
/// author's statement and the key's axes are derived *from the row*. Neither
/// direction is a preference: in each case the other half has nothing to say.
///
/// So two arms, with a reason each:
///
/// - **The key names no line** — the wire path, where the key literally cannot
///   carry one. The line is attached, and `check_usage_line_axes` then refuses a
///   meter on a charge kind that may not have one, so a metered `recurring` row
///   is answered here rather than by the meter-line index three statements later.
/// - **Both name a line and they differ** — an internal caller built both halves
///   (the supersession door passes the predecessor's key), and this is a
///   **refusal**, never a rewrite. A door that overwrote the successor's meter
///   with the key's would make the D-82/D-98/D-127 unit guard's `meter` and
///   `dimensionKey` clauses structurally unreachable — two of the four
///   components of the tier counter's own key — which is exactly how
///   `charge_kind`'s placeholder made `is_usage()` unreachable and cost three
///   Criticals. One rewrite of that shape per crate is one too
///   many, and this is the one.
///
/// The store has a single home for the pair — the `meter` and `dimension_key`
/// columns are both the key's axes and the row's fields — so a disagreement
/// cannot survive a round trip. It exists only between a [`ScopeKey`] value and
/// a [`PriceRow`] value, which is where it is still cheap to answer.
///
/// **That single home is only worth what the two writers agree on.** This
/// function compares trimmed against trimmed — the key's axes are
/// `Meter::new`/`DimensionKey::new` values and `row_meter` above is built the
/// same way — so a `content_model` that rendered the caller's raw string into the
/// columns would leave the column holding a value the key it is filed under does
/// not: `scope_key_filter` renders the axis and matches nothing, and `"api_calls "`
/// then `"api_calls"` land two rows on one canonical key. What makes the sentence
/// above true is
/// [`canonical_usage_line`](crate::domain::price_record::canonical_usage_line),
/// spent on the columns and on the answered record alike; this function still
/// **refuses** a genuine disagreement rather than resolving one, which is the
/// distinction the two arms above are about.
pub(crate) fn resolve_authored_usage_line(
    key: &ScopeKey,
    row: &PriceRow,
) -> Result<ScopeKey, RepoError> {
    let row_meter = row
        .meter
        .as_deref()
        .map(Meter::new)
        .transpose()
        .map_err(|e| RepoError::ValueOutOfRange {
            field: "meter".to_owned(),
            value: e.to_string(),
        })?;
    let row_dimension = DimensionKey::new(&row.dimension_key);
    if key.meter().is_none() && key.dimension_key().is_none() {
        // Rendered before the move, and in the shape the arm below uses: the
        // refusal names the two lines the caller has to reconcile. It carried the
        // key's **charge kind** against the constructor's message, so
        // `USAGE_LINE_AXIS_MISMATCH` reported `"usage"` against a sentence and named
        // neither line.
        let key_line = format!(
            "{}/{}",
            key.meter().map_or("none", Meter::as_str),
            key.dimension_key().as_str()
        );
        let row_line = format!(
            "{}/{}",
            row_meter.as_ref().map_or("none", Meter::as_str),
            row_dimension.as_str()
        );
        // **One class, both operands.** `with_usage_line` refuses either because the
        // charge kind and the line disagree or because a value carries the key
        // separator or the absent-axis token, and on this path both arrive from a
        // request: `PriceContentView::dimension_key` is free-form and
        // `DimensionKey::new` is total by design. Neither is a row written around the
        // domain, so splitting the second into `CorruptRow` would answer a caller
        // mistake with a 500 and an operator alarm. What changed is the operands.
        return key
            .clone()
            .with_usage_line(row_meter, row_dimension)
            .map_err(|_| RepoError::UsageLineDisagrees { key_line, row_line });
    }
    let key_line = (key.meter().map(Meter::as_str), key.dimension_key().as_str());
    let row_line = (
        row_meter.as_ref().map(Meter::as_str),
        row_dimension.as_str(),
    );
    if key_line == row_line {
        return Ok(key.clone());
    }
    Err(RepoError::UsageLineDisagrees {
        key_line: format!("{}/{}", key_line.0.unwrap_or("none"), key_line.1),
        row_line: format!("{}/{}", row_line.0.unwrap_or("none"), row_line.1),
    })
}

/// An update may not move the row's `(meter, dimensionKey)` line (D-196).
///
/// The sibling of [`resolve_authored_usage_line`] on the other door, and the
/// asymmetry is the point: **create** derives the key's line from the content,
/// because that is the author's only way to state it; **update** compares the
/// submitted line against the stored one and refuses a move, because by then the
/// row is filed under a key and the pair are two of its axes. The eight axes an
/// update cannot touch are simply absent from `PriceContent`; these two are not,
/// which is the whole reason this check has to exist as code rather than as a
/// property of the type.
///
/// **A move is a move of the axes, not of the spelling.** Both sides are
/// normalized before the comparison — see the body — so whitespace around a
/// meter is not a line change, which is what
/// [`Meter::normalized`](crate::domain::scope_key::Meter::normalized) means when
/// it calls the trim the axis's spelling.
fn check_update_keeps_the_line(
    row: &price::Model,
    submitted: &(Option<String>, String),
) -> Result<(), RepoError> {
    // **Both sides are normalized, not one.** `submitted` arrives from
    // `canonical_usage_line`, and `content_model` renders the same way, so every
    // row this store writes today reads back canonical and this trim is a no-op
    // on it. A row written *before* that rendering existed does not: its column
    // held whatever the author typed while the key axis held the trimmed value,
    // which is the divergence `canonical_usage_line` was added for. Comparing
    // that raw column against a canonical submission answers
    // `USAGE_LINE_AXIS_MISMATCH` for a `PATCH` that resubmitted the row's own
    // line verbatim — the exact failure the caller's own comment records fixing,
    // one direction over — and the row cannot be repaired in place, because the
    // whole rule here is that an update may not move the line. Normalizing this
    // side too lets the first `PATCH` of such a row write it back canonical
    // instead of freezing it.
    //
    // The two primitives are the ones `canonical_usage_line` itself spends
    // (`Meter::normalized`'s doc: "the one trim it spends, so there is exactly
    // one statement in the crate of what an axis value's spelling is"), called
    // rather than the function, because that function reads a `PriceRow` and this
    // is the stored `price::Model`.
    let stored = (
        row.meter
            .as_deref()
            .map(|meter| Meter::normalized(meter).to_owned()),
        DimensionKey::new(&row.dimension_key).as_str().to_owned(),
    );
    if &stored == submitted {
        return Ok(());
    }
    Err(RepoError::UsageLineDisagrees {
        key_line: format!("{}/{}", stored.0.as_deref().unwrap_or("none"), stored.1),
        row_line: format!(
            "{}/{}",
            submitted.0.as_deref().unwrap_or("none"),
            submitted.1
        ),
    })
}

/// Name which conjunct of a failed compare-and-swap actually failed.
///
/// One extra read, taken only on the refusal path. It costs nothing in the
/// normal case and is the difference between an operator being told to retry
/// and being told to stop retrying.
async fn refuse(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_id: Uuid,
    expected: RowVersion,
) -> RepoError {
    let row = match load_row(runner, scope, tenant_id, price_id).await {
        Err(err) => return err,
        Ok(None) => return not_found(price_id),
        Ok(Some(row)) => row,
    };
    let state = match read_lifecycle(&row.lifecycle_state) {
        Ok(state) => state,
        Err(err) => return err,
    };
    if !state.is_content_mutable() {
        return RepoError::NotDraft {
            subject: SUBJECT.to_owned(),
            id: price_id.to_string(),
            state: state.to_string(),
        };
    }
    match read_row_version(price_id, row.row_version) {
        Err(err) => err,
        Ok(current) => RepoError::StaleRowVersion {
            subject: SUBJECT.to_owned(),
            id: price_id.to_string(),
            current: current.get(),
            submitted: expected.get(),
        },
    }
}

/// The conjunction that makes an UPDATE or DELETE a compare-and-swap on one
/// **draft** row.
///
/// `None` when the submitted version cannot be stored at all; every caller then
/// resolves it through [`refuse`], because no row can hold such a value and the
/// truthful answer is whichever one the row itself gives.
fn swap_guard(tenant_id: Uuid, price_id: Uuid, expected: RowVersion) -> Option<Condition> {
    let version = expected.to_stored().ok()?;
    Some(
        Condition::all()
            .add(price::Column::TenantId.eq(tenant_id))
            .add(price::Column::PriceId.eq(price_id))
            .add(price::Column::RowVersion.eq(version))
            .add(price::Column::LifecycleState.eq(LifecycleState::Draft.as_str())),
    )
}

/// The ten axes as a filter, in normative order.
///
/// One spelling, so no statement here can decide "the same key" by fewer axes
/// than the key actually has — the mistake that would report a collision
/// between two rows that do not share a key at all.
fn scope_key_filter(tenant_id: Uuid, key: &ScopeKey) -> Condition {
    // Destructured through `parts()`, which is what this function's own doc says
    // it needs: *"One spelling, so no statement here can decide 'the same key' by
    // fewer axes than the key actually has."* It could, and it did — D-196 widened
    // this key from eight axes to ten and three sites went unchanged. An eleventh
    // axis is now a compile error here rather than a `Condition` that silently
    // matches too much.
    let ScopeKeyParts {
        plan_id,
        currency,
        region,
        price_overlay,
        phase,
        price_eligibility,
        charge_kind,
        cohort,
        meter,
        dimension_key,
    } = key.parts();
    Condition::all()
        .add(price::Column::TenantId.eq(tenant_id))
        .add(price::Column::PlanId.eq(plan_id.get()))
        .add(price::Column::Currency.eq(currency.as_str()))
        .add(price::Column::Region.eq(region.as_str()))
        .add(price::Column::PriceOverlay.eq(price_overlay.as_str()))
        .add(price::Column::Phase.eq(phase.get()))
        .add(price::Column::PriceEligibility.eq(price_eligibility.as_str()))
        .add(price::Column::ChargeKind.eq(charge_kind.as_str()))
        .add(price::Column::Cohort.eq(cohort.to_string()))
        // **The NULL trap the indexes have, one layer up (D-196).** A key with
        // no meter renders `meter IS NULL`, and `Column::Meter.eq(None)` is
        // `meter = NULL`, which matches nothing — so this read would answer
        // "the key is free" over an occupied one and the duplicate would arrive
        // as the index's driver error rather than as this door's refusal. The
        // store closes the same hole with `COALESCE(meter, '')`; here the
        // `Option` is in hand, so the arm is explicit.
        .add(match meter {
            Some(meter) => price::Column::Meter.eq(meter.as_str()),
            None => price::Column::Meter.is_null(),
        })
        .add(price::Column::DimensionKey.eq(dimension_key.as_str()))
}

/// The "absent, or not yours" refusal — deliberately one answer for both, so
/// the surface leaks no existence.
fn not_found(price_id: Uuid) -> RepoError {
    RepoError::NotFound {
        subject: SUBJECT.to_owned(),
        id: price_id.to_string(),
    }
}

/// The occupied-key refusal.
///
/// The key's own ten-axis rendering leads, verbatim, because that is what a
/// `DUPLICATE_SCOPE_KEY` response has to carry; the occupant's id and state
/// follow, because "this key is taken" without saying by what leaves the author
/// to go looking.
fn duplicate_key(key: &ScopeKey, occupant: &price::Model) -> RepoError {
    RepoError::DuplicateScopeKey(format!(
        "{key} is held by {} price {}",
        occupant.lifecycle_state, occupant.price_id
    ))
}

// ---------------------------------------------------------------------------
// Domain -> storage.
// ---------------------------------------------------------------------------

/// Render a draft's **content** into the columns that hold it.
///
/// Identity, the scope key and the provenance columns are left `NotSet`: they
/// are what [`insert_model`] adds on creation, and what an update may never
/// move.
///
/// **`meter` and `dimension_key` are the exception to that first sentence**, and
/// the reason they are rendered here at all is that they are content *and* the
/// key's ninth and tenth axes (D-196) — one column each, so there is no second
/// place for the axis to live. They go in through
/// [`canonical_usage_line`](crate::domain::price_record::canonical_usage_line)
/// rather than as the caller sent them: this function is the one render into the
/// columns and therefore the only place that can promise the stored axis is the
/// axis the row is filed under, whichever door wrote it. Taking the raw strings
/// instead would break that promise — see `canonical_usage_line` for the
/// duplicate key that admits.
fn content_model(content: &PriceContent) -> Result<price::ActiveModel, RepoError> {
    let row = &content.row;
    let (meter, dimension_key) = canonical_usage_line(row);
    Ok(price::ActiveModel {
        amount_minor: Set(row.amount_minor.map(MinorAmount::get)),
        unit_rate_nano: Set(row.unit_rate.map(RateMinor::nano_minor)),
        model_kind: Set(row.model_kind.map(model_kind_wire).map(str::to_owned)),
        tax_inclusive: Set(content.tax_inclusive),
        tax_category_ref: Set(content.tax_category_ref.clone()),
        billing_timing: Set(content.billing_timing.clone()),
        // The four proration columns, flattened from the one domain value that
        // holds them. `anchor_day` is read off the policy rather than carried
        // beside it, which is what makes a `fixed_day` with no day and a day
        // beside `calendar_month` both unreachable from here.
        billing_anchor_policy: Set(content
            .proration_contract
            .map(|c| c.billing_anchor_policy.as_str().to_owned())),
        anchor_day: Set(content
            .proration_contract
            .and_then(|c| c.billing_anchor_policy.anchor_day())
            .map(|d| i32::from(d.get()))),
        proration_basis: Set(content
            .proration_contract
            .map(|c| c.proration_basis.as_str().to_owned())),
        credit_on_downgrade: Set(content.proration_contract.map(|c| c.credit_on_downgrade)),
        quantity_source: Set(row.quantity_source.map(|s| s.as_str().to_owned())),
        manual_quantity: Set(stored_count("manual_quantity", row.manual_quantity)?),
        package_size: Set(stored_count("package_size", row.package_size)?),
        package_price_minor: Set(row.package_price_minor.map(MinorAmount::get)),
        meter: Set(meter),
        dimension_key: Set(dimension_key),
        billing_granularity: Set(row.billing_granularity.map(|g| g.as_str().to_owned())),
        aggregation_function: Set(row.aggregation_function.map(|f| f.as_str().to_owned())),
        aggregation_granularity: Set(row.aggregation_granularity.map(|g| g.as_str().to_owned())),
        tier_aggregation_window: Set(row.tier_aggregation_window.map(|w| w.as_str().to_owned())),
        tier_qualification_window: Set(row
            .tier_qualification_window
            .map(|w| w.as_str().to_owned())),
        max_hold_granules: Set(stored_count("max_hold_granules", row.max_hold_granules)?),
        included_allowance: Set(row.included_allowance.map(allowance_json)),
        reserved_rate_nano: Set(row.reserved_rate.map(RateMinor::nano_minor)),
        reservation_flavor: Set(row.reservation_flavor.map(|f| f.as_str().to_owned())),
        min_qty_purchase: Set(stored_count("min_qty_purchase", row.min_qty_purchase)?),
        min_qty_usage: Set(stored_count("min_qty_usage", row.min_qty_usage)?),
        min_qty_usage_fallback: Set(row.min_qty_usage_fallback.map(|f| f.as_str().to_owned())),
        discount_ref: Set(row.discount_ref.clone()),
        rounding_policy_ref: Set(content.rounding_policy_ref.clone()),
        grandfather_until: Set(content.grandfather_until),
        supersedes_price_id: Set(content.supersedes_price_id),
        ..price::ActiveModel::default()
    })
}

/// A create, rendered and value-checked, with no statement issued yet.
///
/// It exists so [`PriceRepo::create_draft`] can keep refusing a caller mistake
/// **before** it opens a transaction while [`create_draft_on`] still has one
/// runner-taking entry point. Both go through this and through
/// [`insert_prepared`]; neither restates the other's body.
struct PreparedDraft {
    /// The record as it will be answered, bands already in read order.
    record: PriceRecord,
    /// The row, rendered.
    row: price::ActiveModel,
    /// Its band set, rendered.
    bands: Vec<price_tier_band::ActiveModel>,
    /// The causing request's correlation (D-178), carried through from the draft.
    ///
    /// [`PriceRecord`] holds the row's columns and the correlation is not one —
    /// it belongs to the request, not to the price — so it rides here rather than
    /// being smuggled onto the record and stored.
    correlation_id: Uuid,
}

use crate::domain::instant::format_rfc3339;
/// The two rewrites a price row's content undergoes on its way into the store.
///
/// Defined in [`crate::domain::price_record`] and re-exported here, where it was
/// written and where every caller still spells it `price_repo::authored_content`.
/// It moved because D-312's bulk-import arm judges a row in `domain::import`,
/// which may not reach into `infra` — and a second copy of the projection is the
/// fault that produced the two Criticals its own documentation records.
pub use crate::domain::price_record::authored_content;

/// Render a create and refuse every value the store cannot take, statement-free.
fn prepare_draft(tenant_id: Uuid, draft: NewPriceDraft) -> Result<PreparedDraft, RepoError> {
    // The key the row is actually filed under: the usage line comes from the
    // content, because that is the half of the request that can carry it (D-196
    // — see `resolve_authored_usage_line` for why this is the mirror image of
    // `authored_content` rather than a disagreement with it).
    let scope_key = resolve_authored_usage_line(&draft.scope_key, &draft.content.row)?;
    let record = PriceRecord {
        price_id: draft.price_id,
        scope_key: scope_key.clone(),
        // The two rewrites this door performs, applied through their **one**
        // spelling — see [`authored_content`]. Applied here rather than open-coded
        // because a second caller now has to be able to predict them.
        row: authored_content(&scope_key, draft.content.clone()).row,
        tax_inclusive: draft.content.tax_inclusive,
        tax_category_ref: draft.content.tax_category_ref.clone(),
        billing_timing: draft.content.billing_timing,
        proration_contract: draft.content.proration_contract,
        rounding_policy_ref: draft.content.rounding_policy_ref,
        grandfather_until: draft.content.grandfather_until,
        supersedes_price_id: draft.content.supersedes_price_id,
        lifecycle_state: LifecycleState::Draft,
        created_by: draft.created_by,
        created_at_utc: draft.created_at_utc,
        row_version: RowVersion::new(0),
    };
    // Before the CHECK that would otherwise refuse it: the key is in hand here,
    // so the pairing costs a comparison rather than a round trip and a 500.
    check_grandfather_horizon(
        record.grandfather_until,
        record.scope_key.price_eligibility(),
    )?;

    check_authored_instant("grandfatherUntil", record.grandfather_until)?;
    let row = insert_model(tenant_id, &record)?;
    let bands = band_models(tenant_id, record.price_id, &record.row.bands)?;
    Ok(PreparedDraft {
        record,
        row,
        bands,
        correlation_id: draft.correlation_id,
    })
}

/// The three statements a create issues, through whichever runner holds them.
///
/// The occupancy read, the row and the band set are one unit by rule rather than
/// by preference: a row whose geometry can land a moment late is a row that is
/// briefly wrong, and the duplicate check is a read the insert would otherwise
/// race with across a commit boundary.
async fn insert_prepared(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    prepared: PreparedDraft,
) -> Result<PriceRecord, RepoError> {
    if let Some(occupant) =
        find_key_occupant(runner, scope, tenant_id, &prepared.record.scope_key).await?
    {
        return Err(duplicate_key(&prepared.record.scope_key, &occupant));
    }
    write_prepared(runner, scope, tenant_id, prepared).await
}

/// The two row writes and the audit record, with the occupancy question already
/// answered by whichever door asked it.
///
/// Split out for [`insert_successor_draft_on`], which asks the **opposite**
/// question of the same key and must not re-ask this one: `find_key_occupant`
/// answers "is this key taken", and for the supersession door a taken key is the
/// precondition rather than the refusal (D-195). What the two doors share is
/// everything after that answer, and it is shared rather than restated for the
/// reason [`PreparedDraft`]'s doc gives about the other split in this file.
async fn write_prepared(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    prepared: PreparedDraft,
) -> Result<PriceRecord, RepoError> {
    let PreparedDraft {
        record,
        row,
        bands,
        correlation_id,
    } = prepared;
    insert_price(runner, scope, row).await?;
    insert_bands(runner, scope, bands).await?;
    // The record, in the same transaction as the insert (D-135). The stamp is the
    // draft's own - `created_by` is the actor, `created_at_utc` the instant, and
    // `correlation_id` the request's (D-178), which the draft carries for exactly
    // this line.
    record_price_mutation(
        runner,
        scope,
        tenant_id,
        &record,
        AuditAction::Create,
        None,
        AuditStamp {
            actor_principal_id: record.created_by,
            recorded_at: record.created_at_utc,
            correlation_id,
        },
    )
    .await?;
    Ok(record)
}

/// Append the audit record of one price-row mutation, inside the mutation's own
/// transaction.
///
/// The chain segment is the **plan's** (D-135 keys it on the audited subject's
/// aggregate, and a price row's aggregate is the plan it prices), so a plan's
/// authoring history and its rows' edits are one walk.
///
/// `before` is the version the caller presented — `None` on a create, where
/// nothing existed, which is the difference between a create and an edit and is
/// expressible only as an absence.
///
/// # This is also the TOCTOU void's rail (`inst-ap-pin`)
///
/// All three row mutations land here — create, update and delete — so a pending
/// approval unit pinned to the row's plan is voided here, in the mutation's own
/// transaction. `plan_repo::record_revision_mutation` is the same rail for the
/// shape plane, and its doc explains why the void rides the audit writer rather
/// than nine call sites; the surface enumeration is in
/// [`crate::infra::approval::void_pending_units_of`].
///
/// **A price create voids**, unlike a plan create. The difference is not the
/// verb: a new row joins the candidate set the pin already covers, while a new
/// plan cannot join a pin taken before it existed.
///
/// # Errors
/// [`RepoError::Db`] or [`RepoError::CorruptRow`] from the chain append, both of
/// which roll the mutation back with them. That is the point: a mutation whose
/// record cannot be written must not commit. [`RepoError::Db`] from the void,
/// for the same reason: nor may one that could not close the approval it
/// invalidated.
async fn record_price_mutation(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    record: &PriceRecord,
    action: AuditAction,
    before: Option<RowVersion>,
    stamp: AuditStamp,
) -> Result<(), RepoError> {
    crate::infra::approval::void_pending_units_of(
        runner,
        scope,
        tenant_id,
        record.scope_key.plan_id(),
        stamp.recorded_at,
    )
    .await?;
    // **`PriceCreated`, and only on the act that creates** (S3 §17.5: "a draft price
    // row is authored on the canonical scope key... `PriceCreated` emits per row").
    // The event was declared when the gear was created and had **no producer
    // anywhere** until now, so every consumer counting row creations counted zero.
    // It fires here rather than at publish because S3 puts it on authoring, and the
    // payload carries no version ref for the same reason: a draft is addressable at
    // no `CatalogVersion`.
    //
    // The cutover's commit is the **second** producer (R-02, `inst-gc-commit`), and
    // it will call this same spelling: its two rows are born published rather than
    // through this door, so neither path can be the other's, and one event name with
    // two producers is what makes it mean "a price row came into existence".
    if matches!(action, AuditAction::Create) {
        outbox_repo::enqueue(
            runner,
            scope,
            NewOutboxEvent::price_created(
                tenant_id,
                &PriceCreatedPayload {
                    plan_id: record.scope_key.plan_id(),
                    price_id: record.price_id,
                    scope_key: record.scope_key.to_string(),
                    correlation_id: stamp.correlation_id,
                },
                stamp.recorded_at,
            ),
        )
        .await?;
    }
    audit_repo::append(
        runner,
        scope,
        NewAuditEntry {
            tenant_id,
            chain_id: audit_repo::plan_chain(record.scope_key.plan_id()),
            recorded_at: stamp.recorded_at,
            actor_principal_id: stamp.actor_principal_id,
            action,
            subject_kind: AuditSubjectKind::PriceUnit,
            subject_ref: audit_repo::price_unit_ref(record.price_id),
            before_state: before
                .map(|version| subject_state(LifecycleState::Draft, version.get(), None)),
            after_state: match action {
                // A delete has no after-state, and an empty object would be a
                // claim that the row still stands in some shape.
                AuditAction::Delete => None,
                _ => Some(subject_state(
                    record.lifecycle_state,
                    record.row_version.get(),
                    None,
                )),
            },
            approval_ref: None,
            correlation_id: stamp.correlation_id,
        },
    )
    .await
    .map(|_| ())
}

/// Create a draft price row and its bands through whichever runner the caller
/// holds.
///
/// [`PriceRepo::create_draft`] supplies its own transaction; the other callers —
/// `infra::idempotent` (reached through `api::rest::prices`), `infra::cutover`
/// and `infra::clone` — each already own one, and the first must run the insert
/// inside the **same** transaction as the idempotency claim guarding it — see
/// [`create_draft_on`](super::plan_repo::create_draft_on)'s sibling note and
/// `idempotency_repo`'s module doc for why splitting the two inverts the
/// guarantee. The row-and-bands-in-one-transaction invariant this module's doc
/// names holds either way: the seam supplies the transaction, the method
/// supplies its own.
///
/// # Errors
/// [`RepoError::DuplicateScopeKey`] naming the key and its occupant;
/// [`RepoError::GrandfatherHorizonOffClass`] for a horizon off the grandfathered
/// class; [`RepoError::TimestampPrecisionExceeded`] for a horizon finer than the
/// millisecond quantum; [`RepoError::ValueOutOfRange`] for an authored quantity
/// past its column; [`RepoError::Db`] on a scope or storage failure, including
/// losing the `uq_pricing_price_scope_key_draft` race;
/// [`RepoError::CorruptRow`] only for a `row_version` that cannot be stored.
pub async fn create_draft_on(
    runner: &DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    draft: NewPriceDraft,
) -> Result<PriceRecord, RepoError> {
    insert_prepared(runner, scope, tenant_id, prepare_draft(tenant_id, draft)?).await
}

/// The whole insert: content, plus the columns only a creation writes.
///
/// # Why the content model is destructured field by field, with no `..`
///
/// [`content_assignments`] is the update path's version of this decision and its
/// doc is the argument: a column that reached [`content_model`] and not the
/// assignment list was written by nothing, twice, silently. Its remedy — an
/// exhaustive destructure, so *"adding a column to [`price::Model`] makes this
/// pattern non-exhaustive and the crate stops compiling until someone decides,
/// here, whether a draft edit may move it"* — reached only the update door.
///
/// This one ended `..content`, and [`content_model`] ends
/// `..price::ActiveModel::default()`, so a column added to the table and to
/// neither renderer compiled, inserted as `NotSet`, and landed as the column's
/// default or NULL — while the *update* path reddened. The asymmetry was
/// one-directional and the loud half fires first in practice, since any new
/// content column is reachable by `PATCH`; it is closed here because the sibling's
/// doc reads as though the class is closed, and it was closed on one door of two.
///
/// Every binding is either written from `record` on the line that shadows it or
/// carried through from `content` verbatim, so an omission is not expressible.
fn insert_model(tenant_id: Uuid, record: &PriceRecord) -> Result<price::ActiveModel, RepoError> {
    let key = &record.scope_key;
    let price::ActiveModel {
        // Identity, the scope key and the provenance columns: `content_model`
        // leaves all of them `NotSet` and this function is what sets them, from
        // the record rather than from the content. Bound and ignored so the
        // pattern stays exhaustive.
        price_id: _,
        tenant_id: _,
        plan_id: _,
        currency: _,
        region: _,
        price_overlay: _,
        phase: _,
        price_eligibility: _,
        charge_kind: _,
        cohort: _,
        lifecycle_state: _,
        created_by: _,
        created_at_utc: _,
        row_version: _,
        // D-154's resolved category is frozen inside the publish transaction and
        // is NULL on a draft by definition, so a creation has nothing to say
        // about it either — `content_model` leaves it `NotSet` and so does this.
        resolved_tax_category: _,
        // The rounding resolution, frozen by the same statement for the same
        // reason (`pricing_price`). A creation says nothing about it either.
        resolved_rounding_policy: _,
        // --- content, carried through exactly as `content_model` rendered it ---
        amount_minor,
        unit_rate_nano,
        model_kind,
        tax_inclusive,
        tax_category_ref,
        billing_timing,
        billing_anchor_policy,
        anchor_day,
        proration_basis,
        credit_on_downgrade,
        quantity_source,
        manual_quantity,
        package_size,
        package_price_minor,
        meter,
        dimension_key,
        billing_granularity,
        aggregation_function,
        aggregation_granularity,
        tier_aggregation_window,
        tier_qualification_window,
        max_hold_granules,
        included_allowance,
        reserved_rate_nano,
        reservation_flavor,
        min_qty_purchase,
        min_qty_usage,
        min_qty_usage_fallback,
        discount_ref,
        rounding_policy_ref,
        grandfather_until,
        supersedes_price_id,
    } = content_model(&record.content())?;
    Ok(price::ActiveModel {
        price_id: Set(record.price_id),
        tenant_id: Set(tenant_id),
        plan_id: Set(key.plan_id().get()),
        currency: Set(key.currency().as_str().to_owned()),
        region: Set(key.region().as_str().to_owned()),
        price_overlay: Set(key.price_overlay().as_str().to_owned()),
        phase: Set(key.phase().get()),
        price_eligibility: Set(key.price_eligibility().as_str().to_owned()),
        charge_kind: Set(key.charge_kind().as_str().to_owned()),
        // The axis is a `NOT NULL` text token — `none`, or the cutover instant
        // — rather than a nullable timestamp, because distinct `NULL`s do not
        // collide in the partial `UNIQUE` that decides row uniqueness.
        cohort: Set(key.cohort().to_string()),
        amount_minor,
        unit_rate_nano,
        model_kind,
        tax_inclusive,
        tax_category_ref,
        resolved_tax_category: NotSet,
        resolved_rounding_policy: NotSet,
        billing_timing,
        billing_anchor_policy,
        anchor_day,
        proration_basis,
        credit_on_downgrade,
        quantity_source,
        manual_quantity,
        package_size,
        package_price_minor,
        meter,
        dimension_key,
        billing_granularity,
        aggregation_function,
        aggregation_granularity,
        tier_aggregation_window,
        tier_qualification_window,
        max_hold_granules,
        included_allowance,
        reserved_rate_nano,
        reservation_flavor,
        min_qty_purchase,
        min_qty_usage,
        min_qty_usage_fallback,
        discount_ref,
        rounding_policy_ref,
        grandfather_until,
        supersedes_price_id,
        lifecycle_state: Set(record.lifecycle_state.as_str().to_owned()),
        created_by: Set(record.created_by),
        created_at_utc: Set(record.created_at_utc),
        row_version: Set(record.row_version.to_stored().map_err(|e| {
            RepoError::CorruptRow(format!("pricing_price {}: {e}", record.price_id))
        })?),
    })
}

/// The columns an `update_draft` writes: the whole of the row's content, and
/// nothing of its identity, its scope key or its provenance.
///
/// The values are read off the very `ActiveModel` the insert path builds, so the
/// two writers cannot render one column two different ways. Which columns a draft
/// edit may move is still a **decision** — a column added to the table must not
/// become editable merely by existing — but it is now a decision the compiler
/// makes you take.
///
/// # Why the argument is destructured field by field, with no `..`
///
/// Because the list was hand-written against an `ActiveModel` the compiler never
/// checked it against, and twice a column reached [`content_model`] and never
/// reached this list. Both were silent: the `UPDATE` simply did not name the
/// column, the row kept its old value, and `PATCH` answered `200` with a body
/// rendered from the **stored** record — so the surface reported success and
/// showed the reverted field as if the caller had asked for it.
///
/// - `tax_category_ref` (D-110/D-154): a correction that cannot be made before
///   publish freezes the category for seven years.
/// - `unit_rate_nano`: D-311 gave a `per_unit` row's money a column of its own, and
///   this was the third site that one column split stranded. A grep for the class
///   finds neither, because a hand-written list of `(Column, Value)` pairs looks
///   nothing like the code such a grep matches.
///
/// The exhaustive destructure is the guard that ends the class: adding a column
/// to [`price::Model`] makes this pattern non-exhaustive and the crate stops
/// compiling until someone decides, here, whether a draft edit may move it. The
/// guard was recommended when the *first* of the three sites was found and not
/// taken; had it been, neither of the two omissions above could have existed.
///
/// Fields a draft edit may **not** move are bound and ignored with the reason on
/// the line, so an exclusion is a decision on the record rather than an omission
/// that reads identically to one.
fn content_assignments(model: price::ActiveModel) -> Vec<(price::Column, Value)> {
    let price::ActiveModel {
        // Identity. `insert_model` writes it once; an update addresses the row by
        // it and can no more move it than a caller can rename what they are editing.
        price_id: _,
        tenant_id: _,
        // The canonical scope key, all ten axes. Moving one is a different row:
        // the key decides which duplicate a row is, which supersession chain it
        // joins and which window covers it, so the remedy is a delete and a new
        // draft — see `update_draft`'s own doc. (`meter` and `dimension_key` are
        // axes too since D-196, but they are also content, so they are written
        // below; `check_update_keeps_the_line` refuses an edit that moves them,
        // which is what makes writing them back a no-op rather than a key move.)
        plan_id: _,
        currency: _,
        region: _,
        price_overlay: _,
        phase: _,
        price_eligibility: _,
        charge_kind: _,
        cohort: _,
        // --- content: every one of these is what a draft edit is for ---
        amount_minor,
        unit_rate_nano,
        model_kind,
        tax_inclusive,
        tax_category_ref,
        // D-154's resolved category, frozen inside the publish transaction. NULL
        // on a draft by definition — there is nothing to freeze until a publish
        // resolves it — so a draft edit has nothing to say about it.
        resolved_tax_category: _,
        // Its rounding twin, on the same terms: the publish resolves and freezes
        // it, and a draft edit must not be able to reach it. Before
        // `pricing_price` the resolution went into `rounding_policy_ref` -
        // which a draft edit *can* reach - so a `PATCH` could overwrite what a
        // publish had frozen.
        resolved_rounding_policy: _,
        billing_timing,
        billing_anchor_policy,
        anchor_day,
        proration_basis,
        credit_on_downgrade,
        quantity_source,
        manual_quantity,
        package_size,
        package_price_minor,
        meter,
        dimension_key,
        billing_granularity,
        aggregation_function,
        aggregation_granularity,
        tier_aggregation_window,
        tier_qualification_window,
        max_hold_granules,
        included_allowance,
        reserved_rate_nano,
        reservation_flavor,
        min_qty_purchase,
        min_qty_usage,
        min_qty_usage_fallback,
        discount_ref,
        rounding_policy_ref,
        grandfather_until,
        supersedes_price_id,
        // Lifecycle is moved by publish and supersession, never by an edit; the
        // table's own trigger enumerates the sanctioned transitions.
        lifecycle_state: _,
        // Provenance: who authored the row and when. An edit is not an authoring,
        // and the Slice-12 history export reads these as the original facts.
        created_by: _,
        created_at_utc: _,
        // The `ETag`. `update_draft` advances it with `RowVersion + 1` in the same
        // statement, under the compare-and-swap guard; an assignment here would
        // overwrite the swap with the value the caller submitted.
        row_version: _,
    } = model;

    [
        (price::Column::AmountMinor, amount_minor.into_value()),
        // `content_model` sets it (D-311) and `to_record` reads it back, so
        // omitting it here discards a draft edit that re-rates a metered row — the
        // commonest edit there is on a `per_unit` row, since this column *is* its
        // price — while the call answers success.
        (price::Column::UnitRateNano, unit_rate_nano.into_value()),
        (price::Column::ModelKind, model_kind.into_value()),
        (price::Column::TaxInclusive, tax_inclusive.into_value()),
        // `charge_kind` and the usage line each get a paragraph in `update_draft`'s
        // doc explaining why they do not move. This column moves, and omitting it
        // would be an omission rather than a decision: `PATCH` answers 200 with a
        // body rendered from the stored record, so the field would silently revert.
        //
        // D-110 makes this column the source of truth and the only place a
        // category lives, and D-154 freezes `coalesce(tax_category_ref,
        // readiness.taxCategory)` inside the publish transaction into a version
        // that is INSERT-only over the seven-year horizon. So a correction made on
        // a draft never landed and the row published the category its author
        // already knew was wrong — and `04-currency-tax.md:198` names authoring
        // this very field as D-245's remedy, which was therefore unexpressible.
        (price::Column::TaxCategoryRef, tax_category_ref.into_value()),
        (price::Column::BillingTiming, billing_timing.into_value()),
        (
            price::Column::BillingAnchorPolicy,
            billing_anchor_policy.into_value(),
        ),
        (price::Column::AnchorDay, anchor_day.into_value()),
        (price::Column::ProrationBasis, proration_basis.into_value()),
        (
            price::Column::CreditOnDowngrade,
            credit_on_downgrade.into_value(),
        ),
        (price::Column::QuantitySource, quantity_source.into_value()),
        (price::Column::ManualQuantity, manual_quantity.into_value()),
        (price::Column::PackageSize, package_size.into_value()),
        (
            price::Column::PackagePriceMinor,
            package_price_minor.into_value(),
        ),
        (price::Column::Meter, meter.into_value()),
        (price::Column::DimensionKey, dimension_key.into_value()),
        (
            price::Column::BillingGranularity,
            billing_granularity.into_value(),
        ),
        (
            price::Column::AggregationFunction,
            aggregation_function.into_value(),
        ),
        (
            price::Column::AggregationGranularity,
            aggregation_granularity.into_value(),
        ),
        (
            price::Column::TierAggregationWindow,
            tier_aggregation_window.into_value(),
        ),
        (
            price::Column::TierQualificationWindow,
            tier_qualification_window.into_value(),
        ),
        (
            price::Column::MaxHoldGranules,
            max_hold_granules.into_value(),
        ),
        (
            price::Column::IncludedAllowance,
            included_allowance.into_value(),
        ),
        (
            price::Column::ReservedRateNano,
            reserved_rate_nano.into_value(),
        ),
        (
            price::Column::ReservationFlavor,
            reservation_flavor.into_value(),
        ),
        (price::Column::MinQtyPurchase, min_qty_purchase.into_value()),
        (price::Column::MinQtyUsage, min_qty_usage.into_value()),
        (
            price::Column::MinQtyUsageFallback,
            min_qty_usage_fallback.into_value(),
        ),
        (price::Column::DiscountRef, discount_ref.into_value()),
        (
            price::Column::RoundingPolicyRef,
            rounding_policy_ref.into_value(),
        ),
        (
            price::Column::GrandfatherUntil,
            grandfather_until.into_value(),
        ),
        (
            price::Column::SupersedesPriceId,
            supersedes_price_id.into_value(),
        ),
    ]
    .into_iter()
    .filter_map(|(column, value)| value.map(|value| (column, value)))
    .collect()
}

/// Render a band set for its table.
///
/// `tenant_id` is the parent row's, never a caller's: the foreign key covers
/// `price_id` alone, so a band carrying a different tenant would be invisible to
/// its owner under `SecureORM` while still joined to their price.
fn band_models(
    tenant_id: Uuid,
    price_id: Uuid,
    bands: &[TierBand],
) -> Result<Vec<price_tier_band::ActiveModel>, RepoError> {
    bands
        .iter()
        .map(|band| {
            let from_qty = stored_bound("band from_qty", band.from_qty)?;
            let to_qty = match band.to_qty {
                BandTop::Open => None,
                BandTop::Closed(top) => Some(stored_bound("band to_qty", top)?),
            };
            Ok(price_tier_band::ActiveModel {
                band_id: Set(band_id(price_id, from_qty)),
                tenant_id: Set(tenant_id),
                price_id: Set(price_id),
                from_qty: Set(from_qty),
                to_qty: Set(to_qty),
                unit_price_nano: Set(band.unit_price_rate.nano_minor()),
            })
        })
        .collect()
}

/// A band's surrogate key, derived from the identity the table actually states:
/// `UNIQUE (price_id, from_qty)`.
///
/// `band_id` is a `PRIMARY KEY` with no default and nothing outside this module
/// reads it, so it could have been random. Deriving it makes the surrogate agree
/// with the real identity: replacing a band set with an identical one writes the
/// same ids back, so no consumer can come to depend on an id that changes on
/// every save, and two bands sharing a lower bound collide on both keys rather
/// than on only the one that happened to be checked.
fn band_id(price_id: Uuid, from_qty: i64) -> Uuid {
    Uuid::new_v5(&price_id, &from_qty.to_be_bytes())
}

/// The D-45 declaration as the column carries it, in the `{quantity,
/// rolloverPolicy}` spelling the design set uses. Persisted exactly as authored
/// and never compiled here: the compile is Slice 10's, and the D-129
/// supersession guard reads this field, so a round trip that reshaped it would
/// make the guard report a change nobody made.
fn allowance_json(allowance: IncludedAllowance) -> JsonValue {
    json!({
        "quantity": allowance.quantity,
        "rolloverPolicy": allowance.rollover_policy.as_str(),
    })
}

/// Render an authored count for its `bigint` column.
///
/// # Errors
/// [`RepoError::ValueOutOfRange`] past [`i64::MAX`]. Checked rather than cast: a
/// cast would turn an impossible quantity into a plausible one and price
/// something nobody authored.
fn stored_count(field: &str, value: Option<u64>) -> Result<Option<i64>, RepoError> {
    let Some(value) = value else {
        return Ok(None);
    };
    i64::try_from(value)
        .map(Some)
        .map_err(|_| out_of_range(field, value))
}

/// Render a band bound for its `bigint` column.
fn stored_bound(field: &str, value: u64) -> Result<i64, RepoError> {
    i64::try_from(value).map_err(|_| out_of_range(field, value))
}

/// The caller-mistake refusal for a value no column can hold.
fn out_of_range(field: &str, value: u64) -> RepoError {
    RepoError::ValueOutOfRange {
        field: field.to_owned(),
        value: value.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Storage -> domain.
//
// Every failure below is an **invariant breach, not a caller mistake**: each
// token *column* is `CHECK`-constrained to the set its domain enum renders, and
// every count column is either `CHECK`-constrained non-negative or only ever
// written from a `u64`. A row that reads otherwise means something reached the
// table outside this gear, which is why it surfaces as `CorruptRow` rather than
// as a not-found or a bad request.
//
// One token is not a column and cannot be constrained that way: the D-45
// `rolloverPolicy` lives **inside** the `included_allowance` jsonb, where no
// row CHECK can see it. It reads back through the same inverse list and gets
// the same `CorruptRow`, resting on the weaker of the two grounds — this
// repository is the only writer of the column and it renders the token from
// the domain enum. The reading is the same; what differs is that here the
// schema is not standing behind it, and a Slice-10 writer that started editing
// the declaration in place is what would find that out.
// ---------------------------------------------------------------------------

/// Map a stored row and its bands to the domain value, at this boundary and
/// nowhere else.
fn to_record(
    row: &price::Model,
    bands: &[price_tier_band::Model],
) -> Result<PriceRecord, RepoError> {
    let scope_key = to_scope_key(row)?;
    let shape = to_price_row(row, scope_key.charge_kind(), bands)?;
    Ok(PriceRecord {
        price_id: row.price_id,
        scope_key,
        row: shape,
        tax_inclusive: row.tax_inclusive,
        tax_category_ref: row.tax_category_ref.clone(),
        billing_timing: row.billing_timing.clone(),
        proration_contract: to_proration_contract(row)?,
        rounding_policy_ref: row.rounding_policy_ref.clone(),
        grandfather_until: row.grandfather_until,
        supersedes_price_id: row.supersedes_price_id,
        lifecycle_state: read_lifecycle(&row.lifecycle_state)?,
        created_by: row.created_by,
        created_at_utc: row.created_at_utc,
        row_version: read_row_version(row.price_id, row.row_version)?,
    })
}

/// Rebuild the canonical key from its ten columns, naming the row when it cannot.
///
/// **The row id is in the message, and that is the whole of this wrapper.**
/// `RepoError::CorruptRow` becomes `DomainError::Internal`, so a single
/// unreadable axis answers `500` for the *whole* listing the row appears in —
/// and until this wrapper existed the operator was told which column and not
/// which row, which on a plan of two hundred prices is not a repairable
/// diagnostic. `overlay_repo_tests::a_stored_currency_the_domain_cannot_read_is_a_corrupt_row`
/// states the standard the sibling door already holds — *"the refusal names the
/// column and the row"* — and this is that standard on this one.
///
/// It matters more here than it reads, because the population is real rather than
/// hypothetical: `pricing_price.region` and `.meter` carry **no** `CHECK` on
/// either engine (verified against the chain as it stands), while
/// `Region::new` and `Meter::new` refuse
/// [`KEY_SEPARATOR`](crate::domain::scope_key::KEY_SEPARATOR) and
/// have trimmed since D-196. So the store admits two spellings the loader now
/// refuses, and a row carrying one was writable by this gear's own earlier
/// history. **The chain cannot repair such a row**: the toolkit runner skips any
/// migration already in its ledger, so a normalization added in place to an
/// applied migration never executes on the databases that would need it. A repair
/// is an out-of-band data statement or the chain's re-issue, not a migration edit.
fn to_scope_key(row: &price::Model) -> Result<ScopeKey, RepoError> {
    read_scope_key(row).map_err(|e| match e {
        RepoError::CorruptRow(detail) => {
            RepoError::CorruptRow(format!("price row {}: {detail}", row.price_id))
        }
        other => other,
    })
}

/// [`to_scope_key`]'s body, unwrapped, so every arm below is covered by the one
/// row-naming `map_err` above rather than by a copy of it per axis.
///
/// The cohort / eligibility biconditional is re-established here rather than
/// assumed: the two axes are read back as two independent columns, so the
/// pairing has to hold on every rehydration and not only at first construction.
fn read_scope_key(row: &price::Model) -> Result<ScopeKey, RepoError> {
    let currency = CurrencyCode::new(&row.currency)
        .map_err(|e| RepoError::CorruptRow(format!("pricing_price.currency: {e}")))?;
    let region = Region::new(&row.region)
        .map_err(|e| RepoError::CorruptRow(format!("pricing_price.region: {e}")))?;
    // Asked even though `ScopeKey` takes no overlay: the constructor would
    // silently answer `base` for a row stored on any other plane, and a row the
    // authoring path could not have written must not read back as one it could.
    read_token(
        "pricing_price.price_overlay",
        &row.price_overlay,
        PRICE_OVERLAYS,
        PriceOverlay::as_str,
    )?;
    ScopeKey::new(
        PlanId::new(row.plan_id),
        currency,
        region,
        PhaseId::new(row.phase),
        read_eligibility(row)?,
        read_token(
            "pricing_price.charge_kind",
            &row.charge_kind,
            CHARGE_KINDS,
            ChargeKind::as_str,
        )?,
        read_cohort(&row.cohort)?,
    )
    .and_then(|key| {
        // The ninth and tenth axes, from the same columns the two scope-key
        // indexes read (D-196). Attached here rather than by every consumer,
        // for the reason the eight above are: the window plane, the approval
        // register and the supersession door all compare *loaded* keys, and a
        // key that dropped the pair would compare equal across two rows that
        // the store holds as two.
        let meter = row
            .meter
            .as_deref()
            .map(Meter::new)
            .transpose()
            .map_err(|e| DomainError::InvalidRequest(format!("pricing_price.meter: {e}")))?;
        key.with_usage_line(meter, DimensionKey::new(&row.dimension_key))
    })
    .map_err(|e| RepoError::CorruptRow(format!("pricing_price scope key: {e}")))
}

/// Read the eligibility axis back into the class the row is filed under.
///
/// One spelling, because two readers want it: the key rehydration below, and
/// the horizon pairing an update has to check against the *stored* class. Two
/// spellings would be two inverse lists, and the one that fell behind would read
/// a live class as a corrupt row.
fn read_eligibility(row: &price::Model) -> Result<PriceEligibility, RepoError> {
    read_token(
        "pricing_price.price_eligibility",
        &row.price_eligibility,
        PRICE_ELIGIBILITIES,
        PriceEligibility::as_str,
    )
}

/// Map the Slice-3 columns and the band set back into the shape the rules judge.
fn to_price_row(
    row: &price::Model,
    charge_kind: ChargeKind,
    bands: &[price_tier_band::Model],
) -> Result<PriceRow, RepoError> {
    Ok(PriceRow {
        charge_kind,
        model_kind: read_optional(
            "pricing_price.model_kind",
            row.model_kind.as_deref(),
            &ModelKind::ALL,
            model_kind_wire,
        )?,
        amount_minor: read_amount("pricing_price.amount_minor", row.amount_minor)?,
        unit_rate: row
            .unit_rate_nano
            .map(RateMinor::from_nano_minor)
            .transpose()
            .map_err(|e| RepoError::CorruptRow(format!("pricing_price.unit_rate_nano: {e}")))?,
        bands: bands.iter().map(to_band).collect::<Result<_, _>>()?,
        package_size: read_count("pricing_price.package_size", row.package_size)?,
        package_price_minor: read_amount(
            "pricing_price.package_price_minor",
            row.package_price_minor,
        )?,
        quantity_source: read_optional(
            "pricing_price.quantity_source",
            row.quantity_source.as_deref(),
            QUANTITY_SOURCES,
            QuantitySource::as_str,
        )?,
        manual_quantity: read_count("pricing_price.manual_quantity", row.manual_quantity)?,
        meter: row.meter.clone(),
        dimension_key: row.dimension_key.clone(),
        billing_granularity: read_optional(
            "pricing_price.billing_granularity",
            row.billing_granularity.as_deref(),
            BILLING_GRANULARITIES,
            BillingGranularity::as_str,
        )?,
        tier_aggregation_window: read_optional(
            "pricing_price.tier_aggregation_window",
            row.tier_aggregation_window.as_deref(),
            TIER_AGGREGATION_WINDOWS,
            TierAggregationWindow::as_str,
        )?,
        tier_qualification_window: read_optional(
            "pricing_price.tier_qualification_window",
            row.tier_qualification_window.as_deref(),
            TIER_QUALIFICATION_WINDOWS,
            TierQualificationWindow::as_str,
        )?,
        aggregation_function: read_optional(
            "pricing_price.aggregation_function",
            row.aggregation_function.as_deref(),
            AGGREGATION_FUNCTIONS,
            AggregationFunction::as_str,
        )?,
        aggregation_granularity: read_optional(
            "pricing_price.aggregation_granularity",
            row.aggregation_granularity.as_deref(),
            AGGREGATION_GRANULARITIES,
            AggregationGranularity::as_str,
        )?,
        max_hold_granules: read_count("pricing_price.max_hold_granules", row.max_hold_granules)?,
        included_allowance: row
            .included_allowance
            .as_ref()
            .map(read_allowance)
            .transpose()?,
        reserved_rate: row
            .reserved_rate_nano
            .map(RateMinor::from_nano_minor)
            .transpose()
            .map_err(|e| RepoError::CorruptRow(format!("pricing_price.reserved_rate_nano: {e}")))?,
        reservation_flavor: read_optional(
            "pricing_price.reservation_flavor",
            row.reservation_flavor.as_deref(),
            RESERVATION_FLAVORS,
            ReservationFlavor::as_str,
        )?,
        min_qty_purchase: read_count("pricing_price.min_qty_purchase", row.min_qty_purchase)?,
        min_qty_usage: read_count("pricing_price.min_qty_usage", row.min_qty_usage)?,
        min_qty_usage_fallback: read_optional(
            "pricing_price.min_qty_usage_fallback",
            row.min_qty_usage_fallback.as_deref(),
            MIN_QTY_USAGE_FALLBACKS,
            MinQtyUsageFallback::as_str,
        )?,
        discount_ref: row.discount_ref.clone(),
    })
}

/// Map one stored band. `NULL` `to_qty` is the **open top** — a state of the
/// band, not an absent value.
fn to_band(band: &price_tier_band::Model) -> Result<TierBand, RepoError> {
    let from_qty = read_bound("pricing_price_tier_band.from_qty", band.from_qty)?;
    let to_qty = match read_count("pricing_price_tier_band.to_qty", band.to_qty)? {
        None => BandTop::Open,
        Some(top) => BandTop::Closed(top),
    };
    let unit_price_rate = RateMinor::from_nano_minor(band.unit_price_nano).map_err(|e| {
        RepoError::CorruptRow(format!("pricing_price_tier_band.unit_price_nano: {e}"))
    })?;
    Ok(TierBand {
        from_qty,
        to_qty,
        unit_price_rate,
    })
}

/// Read the D-45 declaration back out of its JSON column.
fn read_allowance(stored: &JsonValue) -> Result<IncludedAllowance, RepoError> {
    let malformed = || {
        RepoError::CorruptRow(format!(
            "pricing_price.included_allowance is not a {{quantity, rolloverPolicy}} object: {stored}"
        ))
    };
    let quantity = stored
        .get("quantity")
        .and_then(JsonValue::as_u64)
        .ok_or_else(malformed)?;
    let token = stored
        .get("rolloverPolicy")
        .and_then(JsonValue::as_str)
        .ok_or_else(malformed)?;
    Ok(IncludedAllowance {
        quantity,
        rollover_policy: read_token(
            "pricing_price.included_allowance.rolloverPolicy",
            token,
            ROLLOVER_POLICIES,
            RolloverPolicy::as_str,
        )?,
    })
}

/// Read the cohort axis back: `none`, or the UTC cutover instant its own
/// rendering carries as epoch milliseconds.
///
/// Millisecond resolution is what [`Cohort`]'s rendering has, so it is what the
/// column can hold; a cutover instant authored with finer precision does not
/// round-trip, and would put a row on a key nothing later matches.
fn read_cohort(token: &str) -> Result<Cohort, RepoError> {
    if token == Cohort::None.to_string() {
        return Ok(Cohort::None);
    }
    let malformed = || {
        RepoError::CorruptRow(format!(
            "pricing_price.cohort holds {token}, not an instant"
        ))
    };
    let millis: i64 = token.parse().map_err(|_| malformed())?;
    crate::domain::instant::from_unix_millis(millis)
        .map(Cohort::Generation)
        .ok_or_else(malformed)
}

/// Read a lifecycle token back into the state machine's vocabulary.
fn read_lifecycle(token: &str) -> Result<LifecycleState, RepoError> {
    read_token(
        "pricing_price.lifecycle_state",
        token,
        LifecycleState::ALL,
        LifecycleState::as_str,
    )
}

/// Rebuild the proration contract from its four columns
/// (`06-consumer-contracts.md` §6, `pricing_price`).
///
/// **All four or none.** The three fields are required together on a recurring
/// row (`inst-pi-required`), so a row holding some of them is a row no writer in
/// this crate produced: `content_model` sets the four from one `Option`, and the
/// only other writer of `pricing_price` is the schema itself. A partial set is
/// therefore corruption and is reported as such rather than silently read as an
/// absent contract — which would turn a torn row into a publishable one.
///
/// The `anchor_day` pairing is re-established here rather than assumed, for the
/// reason [`to_scope_key`] re-establishes the cohort biconditional: the two
/// columns come back independently, and the domain enum will not hold a
/// mismatched pair, so this is where a mismatch has to be caught.
fn to_proration_contract(row: &price::Model) -> Result<Option<ProrationContract>, RepoError> {
    // `anchor_day` is in this array because the doc's "all four or none" is only
    // true with it: left out, a row carrying an anchor day alone takes the
    // all-absent early return and reads back as *no proration contract at all*,
    // never reaching the pairing check below that exists to catch exactly that.
    let present = [
        row.billing_anchor_policy.is_some(),
        row.proration_basis.is_some(),
        row.credit_on_downgrade.is_some(),
        row.anchor_day.is_some(),
    ];
    if present.iter().all(|p| !p) {
        return Ok(None);
    }
    let (Some(policy_token), Some(basis_token), Some(credit)) = (
        row.billing_anchor_policy.as_deref(),
        row.proration_basis.as_deref(),
        row.credit_on_downgrade,
    ) else {
        return Err(RepoError::CorruptRow(format!(
            "pricing_price {} holds a partial proration contract (anchor policy: {}, basis: {}, \
             credit: {}, anchor day: {}); they publish together or not at all",
            row.price_id, present[0], present[1], present[2], present[3]
        )));
    };

    // Through the enum's own roster, as the `proration_basis` field below already
    // was. The match this replaced re-spelled all three tokens in a module that
    // imports the enum, so a fourth K2 policy would have been read back as
    // corruption — which is a stored row this gear wrote refusing to load.
    let policy = read_token(
        "pricing_price.billing_anchor_policy",
        policy_token,
        BillingAnchorPolicy::ALL,
        BillingAnchorPolicy::as_str,
    )?;
    let billing_anchor_policy = match policy.anchor_day() {
        Some(_) => {
            let day = row.anchor_day.ok_or_else(|| {
                RepoError::CorruptRow(format!(
                    "pricing_price {}: a fixed_day anchor holds no anchor_day",
                    row.price_id
                ))
            })?;
            let day = u8::try_from(day)
                .ok()
                .and_then(|d| AnchorDay::new(d).ok())
                .ok_or_else(|| {
                    RepoError::CorruptRow(format!(
                        "pricing_price {}: anchor_day holds {day}",
                        row.price_id
                    ))
                })?;
            policy.with_anchor_day(day)
        }
        None => policy,
    };
    if billing_anchor_policy.anchor_day().is_none() && row.anchor_day.is_some() {
        return Err(RepoError::CorruptRow(format!(
            "pricing_price {}: {policy_token} carries an anchor_day, which only fixed_day has",
            row.price_id
        )));
    }

    Ok(Some(ProrationContract {
        billing_anchor_policy,
        proration_basis: read_token(
            "pricing_price.proration_basis",
            basis_token,
            ProrationBasis::ALL,
            ProrationBasis::as_str,
        )?,
        credit_on_downgrade: credit,
    }))
}

/// Read the entity tag back out of its `bigint` column.
fn read_row_version(price_id: Uuid, stored: i64) -> Result<RowVersion, RepoError> {
    RowVersion::from_stored(stored)
        .map_err(|e| RepoError::CorruptRow(format!("pricing_price {price_id}: {e}")))
}

/// Read a non-negative money column.
fn read_amount(column: &str, stored: Option<i64>) -> Result<Option<MinorAmount>, RepoError> {
    let Some(units) = stored else {
        return Ok(None);
    };
    MinorAmount::new(units)
        .map(Some)
        .map_err(|e| RepoError::CorruptRow(format!("{column}: {e}")))
}

/// Read a count column back into the unsigned type the domain counts in.
fn read_count(column: &str, stored: Option<i64>) -> Result<Option<u64>, RepoError> {
    let Some(value) = stored else {
        return Ok(None);
    };
    u64::try_from(value)
        .map(Some)
        .map_err(|_| RepoError::CorruptRow(format!("{column} holds {value}, not a count")))
}

/// Read a required count column back into the unsigned type the domain counts
/// in. `from_qty` has no null state — the open top lives on `to_qty` — so it is
/// read on its own rather than through an `Option` that could only ever be
/// `Some`.
fn read_bound(column: &str, stored: i64) -> Result<u64, RepoError> {
    u64::try_from(stored)
        .map_err(|_| RepoError::CorruptRow(format!("{column} holds {stored}, not a count")))
}

/// Read a nullable token column.
fn read_optional<T: Copy>(
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
fn read_token<T: Copy>(
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

#[cfg(test)]
#[path = "price_repo_tests.rs"]
mod price_repo_tests;
