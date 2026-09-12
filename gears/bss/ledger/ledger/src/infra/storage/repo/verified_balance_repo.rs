//! Repository for the cumulative tie-out baseline (`ledger_verified_balance`,
//! VHP-1843). Two operations, both generic over `DBRunner` so they run inside
//! the caller's transaction: `snapshot` (write the current cache as the new
//! baseline, in the period-close txn) and `load_baseline` (read it back for the
//! incremental tie-out, in the daily / recon path). Tenant-scoped via
//! `SecureORM` (SQL-level BOLA). The caches the baseline mirrors are upsert-only
//! (never deleted), so the baseline mirrors them upsert-only too — a grain that
//! has appeared once stays in the baseline.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict,
};
use uuid::Uuid;

use crate::domain::model::RepoError;
use crate::infra::storage::entity::verified_balance;
use time::OffsetDateTime;

/// One grain instance's verified balance to snapshot — an ABSOLUTE total (not a
/// delta): the cumulative value of the cache row at close time.
#[derive(Clone, Debug)]
pub struct BaselineRow {
    /// One of the `verified_balance::GRAIN_*` discriminators.
    pub grain: String,
    /// Canonical per-instance key the tie-out fold produces for this grain.
    pub grain_key: String,
    /// Absolute cumulative balance, minor units.
    pub balance_minor: i64,
}

/// `SeaORM`-backed verified-baseline repository. Stateless: every method takes
/// the caller's `runner` (a txn or a scoped connection).
pub struct VerifiedBalanceRepo;

impl VerifiedBalanceRepo {
    /// Load the cumulative baseline for `tenant` (all grains). Empty before the
    /// tenant's first period close — the caller then falls back to the full
    /// fold. SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure.
    pub async fn load_baseline<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Vec<verified_balance::Model>, RepoError> {
        verified_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(verified_balance::Column::TenantId.eq(tenant)))
            .all(runner)
            .await
            .map_err(|e| RepoError::Db(format!("load verified_balance: {e}")))
    }

    /// Snapshot the current cache as the new baseline for `tenant`, verified
    /// `through_period` at `watermark_seq`. Each row is upserted to its ABSOLUTE
    /// value (the close txn has just proven the cache via a clean full tie-out).
    /// Idempotent on close re-entry; rolls back with the close txn on abort.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure.
    pub async fn snapshot<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        tenant: Uuid,
        through_period: &str,
        watermark_seq: i64,
        rows: &[BaselineRow],
    ) -> Result<(), RepoError> {
        let now = OffsetDateTime::now_utc();
        for row in rows {
            let am = verified_balance::ActiveModel {
                tenant_id: Set(tenant),
                grain: Set(row.grain.clone()),
                grain_key: Set(row.grain_key.clone()),
                verified_balance_minor: Set(row.balance_minor),
                through_period: Set(through_period.to_owned()),
                watermark_seq: Set(watermark_seq),
                updated_at_utc: Set(now),
            };
            // Set absolute (NOT add): the snapshot replaces the prior baseline
            // value for this grain with the freshly-verified cumulative total.
            let on_conflict = SecureOnConflict::<verified_balance::Entity>::columns([
                verified_balance::Column::TenantId,
                verified_balance::Column::Grain,
                verified_balance::Column::GrainKey,
            ])
            .value(
                verified_balance::Column::VerifiedBalanceMinor,
                Expr::value(row.balance_minor),
            )
            .and_then(|oc| {
                oc.value(
                    verified_balance::Column::ThroughPeriod,
                    Expr::value(through_period.to_owned()),
                )
            })
            .and_then(|oc| {
                oc.value(
                    verified_balance::Column::WatermarkSeq,
                    Expr::value(watermark_seq),
                )
            })
            .and_then(|oc| oc.value(verified_balance::Column::UpdatedAtUtc, Expr::value(now)))
            .map_err(|e| RepoError::Db(format!("verified_balance on_conflict: {e}")))?;

            verified_balance::Entity::insert(am.clone())
                .secure()
                .scope_with_model(scope, &am)
                .map_err(|e| RepoError::Db(format!("verified_balance scope: {e}")))?
                .on_conflict(on_conflict)
                .exec_with_returning(runner)
                .await
                .map_err(|e| RepoError::Db(format!("verified_balance upsert: {e}")))?;
        }
        Ok(())
    }
}
