//! Repository for the materialized pin-eligibility frontier
//! (`pricing_pin_frontier`, D-136).
//!
//! Two operations, and the asymmetry between them is the design.
//! [`PinFrontierRepo::read`] is on the consumer path — it is what
//! `GET /bss-pricing/v1/catalog-version/frontier` serves and what a consumer
//! pins for the duration of a resolution run — so it is a single point lookup
//! returning the SDK's [`PinFrontier`], mapped at this boundary so the entity
//! never leaves infrastructure. [`advance`] is on the projector's path and
//! moves the watermark **forward only**.
//!
//! Forward-only is enforced twice, on purpose. The UPDATE carries its own
//! `catalog_version <:to` predicate, so even two projectors racing cannot walk
//! the frontier backwards — whichever loses simply updates nothing. And a
//! request to advance to an equal or lower version is reported as
//! [`RepoError::FrontierRegression`] rather than swallowed as a no-op: the
//! projector advances the frontier only inside the transaction completing the
//! frontier's **next** version in order, so such a request means that ordering
//! assumption has already broken. A receding frontier would let one pin resolve
//! two different contents over time, which is the entire reason the predicate is
//! materialized instead of recomputed; a silent no-op would leave the ordering
//! bug behind it invisible.
//!
//! # The advance is a free function, and it was unusable as a method
//!
//! [`advance`] takes the caller's runner and never acquires its own connection.
//! D-136 requires the advance to happen "in the **same transaction** that sets the
//! last outstanding `warm_completed` marker of the frontier's next version in
//! order", and `Db::conn()` inside an open transaction "is refused outright by the
//! toolkit's transaction-bypass guard"
//! ([`plan_repo::load_current`](super::plan_repo::load_current)'s doc states it one
//! table over). A version reaching for its own connection would not merely take the
//! wrong runner — it could not run at all on the only path D-136 permits.
//!
//! It is therefore a runner-taking free function, the shape
//! [`plan_repo::publish_revision`](super::plan_repo::publish_revision) already
//! has, and [`read_at`] joins it so the projector can read the watermark inside
//! its own transaction too. **Nothing about the guarantee moved** — the same
//! `catalog_version <:to` predicate, the same typed refusal, the same
//! argument for refusing rather than swallowing. Only where the statement runs.
//!
//! [`PinFrontierRepo::read`] stays a method, because its caller is the REST
//! path (`api::rest::frontier::get_frontier`, through `ApiState`) and that path
//! is not in a transaction and holds no runner.
//!
//! # One contradiction is recorded here rather than resolved
//!
//! [`repo_failure`](crate::infra::storage::repo_failure) maps
//! [`RepoError::FrontierRegression`] onto `DomainError::LifecycleForbidden`,
//! calling it "a precondition failure, not an internal fault". D-146 says of
//! this exact case the opposite: "a regression there is an internal fault, not
//! a lifecycle refusal, and a wire code for it would document an API a client
//! cannot provoke". They contradict.
//!
//! The mapping is **left alone and the contradiction reported**. Nothing on a
//! wire path can reach either rendering: this variant's first producer is the
//! background sweep landed beside this file, and a background job's error is
//! logged, never rendered. Changing the mapping would move a rendering under a
//! code the design set has already reasoned about, for no observable gain.

use bss_pricing_sdk::{CatalogVersion, PinFrontier};

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::infra::storage::RepoError;
use crate::infra::storage::entity::pin_frontier;

/// `SeaORM`-backed pin-frontier repository — the **read** half, for the REST
/// surface that has no transaction to join.
#[derive(Clone)]
pub struct PinFrontierRepo {
    db: DBProvider<DbError>,
}

impl PinFrontierRepo {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Read `tenant_id`'s current pin-eligibility frontier.
    ///
    /// `None` means the tenant has never had a version become pin-eligible —
    /// distinct from "version 0", and the distinction is load-bearing: a
    /// consumer with no frontier has nothing it may pin, and fails closed,
    /// rather than pinning an initial version whose content nothing warmed.
    /// SQL-level BOLA: a foreign tenant yields `None`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure;
    /// [`RepoError::CorruptRow`] when the stored version is outside the
    /// unsigned range [`CatalogVersion`] carries (the column is
    /// `CHECK (catalog_version >= 0)`, so this is an invariant breach).
    pub async fn read(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
    ) -> Result<Option<PinFrontier>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        read_at(&conn, scope, tenant_id).await
    }
}

/// Read `tenant_id`'s frontier through whichever runner the caller holds.
///
/// One implementation with two entry points, for
/// [`plan_repo::load_current`](super::plan_repo::load_current)'s reason: the
/// projector reads the watermark **inside** the transaction it may then advance
/// it in, and a second query would be a second thing that could disagree about
/// where the frontier stands — which is the disagreement the materialization
/// exists to remove.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when the stored version is outside the unsigned
/// range [`CatalogVersion`] carries.
pub async fn read_at(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
) -> Result<Option<PinFrontier>, RepoError> {
    let row = pin_frontier::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(pin_frontier::Column::TenantId.eq(tenant_id)))
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pin frontier: {e}")))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let version = u64::try_from(row.catalog_version).map_err(|e| {
        RepoError::CorruptRow(format!(
            "pin frontier for tenant {tenant_id} holds catalog_version {}: {e}",
            row.catalog_version
        ))
    })?;
    Ok(Some(PinFrontier {
        catalog_version: CatalogVersion::new(version),
        advanced_at: row.advanced_at,
    }))
}

/// Every tenant's frontier, bounded.
///
/// The `pricing.readmodel.pin_eligibility_overdue` alarm's input, and the
/// reason it needs one: D-136 and [`PinFrontier::advanced_at`]'s own doc both
/// name the frontier's age as what that alarm fires on, and a per-tenant read
/// cannot find a tenant whose frontier is stale precisely because nothing of
/// that tenant has moved.
///
/// One row per tenant that has ever had a version become pin-eligible, so this
/// is O(active tenants) rather than O(publishes) — the same order the sibling
/// ledger's provisioned-tenant enumeration runs at, and bounded anyway.
///
/// # Errors
/// [`RepoError::Db`] on a scope or storage failure;
/// [`RepoError::CorruptRow`] when a stored version is outside the unsigned
/// range [`CatalogVersion`] carries.
pub async fn list_all(
    runner: &impl DBRunner,
    scope: &AccessScope,
    limit: u64,
) -> Result<Vec<(Uuid, PinFrontier)>, RepoError> {
    let rows = pin_frontier::Entity::find()
        .secure()
        .scope_with(scope)
        .order_by(pin_frontier::Column::AdvancedAt, Order::Asc)
        .limit(limit)
        .all(runner)
        .await
        .map_err(|e| RepoError::Db(format!("list pin frontiers: {e}")))?;
    rows.into_iter()
        .map(|row| {
            let version = u64::try_from(row.catalog_version).map_err(|e| {
                RepoError::CorruptRow(format!(
                    "pin frontier for tenant {} holds catalog_version {}: {e}",
                    row.tenant_id, row.catalog_version
                ))
            })?;
            Ok((
                row.tenant_id,
                PinFrontier {
                    catalog_version: CatalogVersion::new(version),
                    advanced_at: row.advanced_at,
                },
            ))
        })
        .collect()
}

/// Advance `tenant_id`'s frontier to `to`, stamping `at`, inside `runner`'s
/// transaction.
///
/// Only ever forward. The first advance inserts the tenant's row; every later
/// one is a conditional UPDATE guarded by `catalog_version < to`, so the
/// watermark cannot recede even under a concurrent advance.
///
/// Runner-taking because D-136 requires this statement to run in the very
/// transaction that completed the version — see the module doc for why the
/// method it replaces could not.
///
/// # Errors
/// [`RepoError::FrontierRegression`] when the frontier already stands at or
/// beyond `to` — including the case where a concurrent advance overtook this
/// one. [`RepoError::Db`] on a scope or storage failure, which includes losing
/// the insert race for a tenant's first frontier row (the `tenant_id` primary
/// key is what makes that a failure rather than a lost update).
/// [`RepoError::CorruptRow`] when `to` exceeds the signed range the column
/// stores.
pub async fn advance(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    to: CatalogVersion,
    at: OffsetDateTime,
) -> Result<(), RepoError> {
    let target = i64::try_from(to.get()).map_err(|e| {
        RepoError::CorruptRow(format!(
            "catalog version {} exceeds the storable range: {e}",
            to.get()
        ))
    })?;

    let current = pin_frontier::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(pin_frontier::Column::TenantId.eq(tenant_id)))
        .one(runner)
        .await
        .map_err(|e| RepoError::Db(format!("read pin frontier before advance: {e}")))?;

    let Some(row) = current else {
        let am = pin_frontier::ActiveModel {
            tenant_id: Set(tenant_id),
            catalog_version: Set(target),
            advanced_at: Set(at),
        };
        pin_frontier::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("pricing_pin_frontier scope: {e}")))?
            .exec(runner)
            .await
            .map_err(|e| RepoError::Db(format!("insert pricing_pin_frontier: {e}")))?;
        return Ok(());
    };

    if row.catalog_version >= target {
        return Err(regression(tenant_id, row.catalog_version, to));
    }

    // The `< target` predicate is the physical half of the forward-only
    // rule: a concurrent advance that already moved the frontier past
    // `target` leaves this UPDATE matching no row.
    let result = pin_frontier::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(pin_frontier::Column::CatalogVersion, Expr::value(target))
        .col_expr(pin_frontier::Column::AdvancedAt, Expr::value(at))
        .filter(
            Condition::all()
                .add(pin_frontier::Column::TenantId.eq(tenant_id))
                .add(pin_frontier::Column::CatalogVersion.lt(target)),
        )
        .exec(runner)
        .await
        .map_err(|e| RepoError::Db(format!("advance pricing_pin_frontier: {e}")))?;

    if result.rows_affected == 0 {
        return Err(regression(tenant_id, row.catalog_version, to));
    }
    Ok(())
}

/// Build the typed refusal, translating the stored `i64` back into the SDK's
/// unsigned vocabulary. A stored value outside that range would already have
/// failed [`read_at`]; here it is reported as `0` rather than masking the
/// regression behind a second error, since the caller's mistake is the same
/// either way.
fn regression(tenant_id: Uuid, current: i64, requested: CatalogVersion) -> RepoError {
    RepoError::FrontierRegression {
        tenant: tenant_id.to_string(),
        current: u64::try_from(current).unwrap_or(0),
        requested: requested.get(),
    }
}
