//! Repository for the per-tenant FX revaluation mode
//! (`ledger_tenant_fx_revaluation_mode`, VHP-1986): resolve the version in effect
//! (latest `effective_from <= at`, highest `version` on a tie) and append a new
//! effective-dated version. Mirrors [`PostingPolicyRepo`](super::PostingPolicyRepo).
//! Tenant-scoped via `SecureORM` (SQL-level BOLA); out-of-txn on a fresh scoped
//! connection (the mode is admin-plane, never the hot money path).

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureInsertExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::fx::revaluation_mode::RevaluationMode;
use crate::domain::model::RepoError;
use crate::infra::storage::entity::fx_revaluation_mode;
use time::OffsetDateTime;

/// `SeaORM`-backed FX revaluation-mode repository.
#[derive(Clone)]
pub struct FxRevaluationModeRepo {
    db: DBProvider<DbError>,
}

impl FxRevaluationModeRepo {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Resolve the revaluation mode in effect for `tenant` at instant `at` — the
    /// row with the latest `effective_from <= at` (ties: highest `version`).
    /// Returns `None` when the tenant has NO effective row, so the caller can
    /// distinguish "unconfigured" (→ apply the fleet default
    /// [`RevaluationMode::fleet_default`]) from an explicit `ModeA` opt-out. SQL-level
    /// BOLA: a foreign tenant yields no rows (`None`).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure, or when a stored value
    /// fails to parse (an invariant breach — the column is CHECK-constrained and
    /// only written via the validated domain type).
    pub async fn read_effective_mode(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        at: OffsetDateTime,
    ) -> Result<Option<RevaluationMode>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = fx_revaluation_mode::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(fx_revaluation_mode::Column::TenantId.eq(tenant))
                    .add(fx_revaluation_mode::Column::EffectiveFrom.lte(at)),
            )
            .order_by(fx_revaluation_mode::Column::EffectiveFrom, Order::Desc)
            .order_by(fx_revaluation_mode::Column::Version, Order::Desc)
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read fx revaluation mode: {e}")))?;
        let Some(row) = row else {
            return Ok(None);
        };
        RevaluationMode::parse(&row.revaluation_mode)
            .map(Some)
            .map_err(|e| {
                RepoError::Db(format!(
                    "corrupt revaluation_mode for tenant {tenant} version {}: {e}",
                    row.version
                ))
            })
    }

    /// Append a new effective-dated mode version for `tenant`, effective from
    /// `effective_from`. The version is `max(version) + 1` (`0` for the first).
    /// Returns the new version. SQL-level BOLA. The `(tenant, version)` PK guards
    /// a concurrent double-write (the loser fails with a storage error — accepted
    /// for the rare admin-plane write).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure.
    pub async fn write_version(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        mode: RevaluationMode,
        effective_from: OffsetDateTime,
    ) -> Result<i64, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let current = fx_revaluation_mode::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(fx_revaluation_mode::Column::TenantId.eq(tenant)))
            .order_by(fx_revaluation_mode::Column::Version, Order::Desc)
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read max fx-revaluation-mode version: {e}")))?;
        let version = current.map_or(0, |r| r.version + 1);
        let am = fx_revaluation_mode::ActiveModel {
            tenant_id: Set(tenant),
            version: Set(version),
            effective_from: Set(effective_from),
            revaluation_mode: Set(mode.as_str().to_owned()),
            created_at_utc: Set(OffsetDateTime::now_utc()),
        };
        fx_revaluation_mode::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_tenant_fx_revaluation_mode scope: {e}")))?
            .exec(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("insert ledger_tenant_fx_revaluation_mode: {e}")))?;
        Ok(version)
    }

    /// In-txn variant of [`Self::read_effective_mode`] — resolve the mode in effect
    /// at `at` over an existing transaction/connection `runner`, so the period-close
    /// gate can read it inside its serializable snapshot. Returns `None` when the
    /// tenant has no effective row. Mirrors
    /// [`FxRevaluationRunRepo::is_period_complete`](super::FxRevaluationRunRepo::is_period_complete).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage failure or a corrupt stored value.
    pub async fn read_effective_mode_in_txn<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        tenant: Uuid,
        at: OffsetDateTime,
    ) -> Result<Option<RevaluationMode>, RepoError> {
        let row = fx_revaluation_mode::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(fx_revaluation_mode::Column::TenantId.eq(tenant))
                    .add(fx_revaluation_mode::Column::EffectiveFrom.lte(at)),
            )
            .order_by(fx_revaluation_mode::Column::EffectiveFrom, Order::Desc)
            .order_by(fx_revaluation_mode::Column::Version, Order::Desc)
            .one(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read fx revaluation mode (txn): {e}")))?;
        let Some(row) = row else {
            return Ok(None);
        };
        RevaluationMode::parse(&row.revaluation_mode)
            .map(Some)
            .map_err(|e| {
                RepoError::Db(format!(
                    "corrupt revaluation_mode for tenant {tenant} version {}: {e}",
                    row.version
                ))
            })
    }
}
