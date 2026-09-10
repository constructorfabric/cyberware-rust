//! Repository for the Mode-B FX-revaluation completion marker
//! (`ledger_fx_revaluation_run`, VHP-1859 review C3). The revaluation job marks a
//! period COMPLETE after a clean period-end run; the period-close gate reads it.
//! Both ops are generic over `DBRunner` so they run on the caller's transaction.
//! Tenant-scoped via `SecureORM` (SQL-level BOLA).

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use uuid::Uuid;

use crate::domain::model::RepoError;
use crate::infra::storage::entity::fx_revaluation_run::{self, SCOPE_PERIOD, STATUS_COMPLETE};
use time::OffsetDateTime;

/// `SeaORM`-backed FX-revaluation marker repository. Stateless: every method
/// takes the caller's `runner` (a txn or a scoped connection).
pub struct FxRevaluationRunRepo;

impl FxRevaluationRunRepo {
    /// Mark the period-end revaluation COMPLETE for `(tenant, period_id)` —
    /// idempotent upsert (a re-run refreshes `completed_at_utc`). Called by the
    /// revaluation job once `run_period` finishes every scope without error.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure.
    pub async fn mark_complete<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
    ) -> Result<(), RepoError> {
        let now = OffsetDateTime::now_utc();
        let am = fx_revaluation_run::ActiveModel {
            tenant_id: Set(tenant),
            period_id: Set(period_id.to_owned()),
            scope: Set(SCOPE_PERIOD.to_owned()),
            status: Set(STATUS_COMPLETE.to_owned()),
            completed_at_utc: Set(now),
        };
        let on_conflict = SecureOnConflict::<fx_revaluation_run::Entity>::columns([
            fx_revaluation_run::Column::TenantId,
            fx_revaluation_run::Column::PeriodId,
        ])
        .value(
            fx_revaluation_run::Column::Status,
            Expr::value(STATUS_COMPLETE),
        )
        .and_then(|oc| oc.value(fx_revaluation_run::Column::CompletedAtUtc, Expr::value(now)))
        .map_err(|e| RepoError::Db(format!("fx_revaluation_run on_conflict: {e}")))?;

        fx_revaluation_run::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("fx_revaluation_run scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(runner)
            .await
            .map_err(|e| RepoError::Db(format!("fx_revaluation_run upsert: {e}")))?;
        Ok(())
    }

    /// `true` when a COMPLETE marker exists for `(tenant, period_id)` — the
    /// close gate's check that the period-end revaluation actually ran.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure.
    pub async fn is_period_complete<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
    ) -> Result<bool, RepoError> {
        let row = fx_revaluation_run::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(fx_revaluation_run::Column::TenantId.eq(tenant))
                    .add(fx_revaluation_run::Column::PeriodId.eq(period_id))
                    .add(fx_revaluation_run::Column::Status.eq(STATUS_COMPLETE)),
            )
            .one(runner)
            .await
            .map_err(|e| RepoError::Db(format!("read fx_revaluation_run: {e}")))?;
        Ok(row.is_some())
    }
}
