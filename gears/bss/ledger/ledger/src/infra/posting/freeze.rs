//! `ScopeFreezeRepo` + [`TamperFreezeGuard`] — the per-tenant tamper-freeze
//! switch over `bss.scope_freeze`. When the integrity verifier finds a broken
//! tamper-evidence chain it `set`s a freeze row; while that row is ACTIVE
//! (`cleared_at IS NULL`) [`TamperFreezeGuard::check`] rejects any fresh post
//! into the frozen scope with [`DomainError::TamperVerificationFailed`], so the
//! ledger STOPS accepting writes until an operator `clear`s the freeze.
//!
//! A freeze row's `period_id` is `'ALL'` for a tenant-wide freeze or a concrete
//! period to freeze just that period; a post into period `P` is blocked iff an
//! ACTIVE row exists for the tenant whose `period_id` is `'ALL'` OR equals `P`.
//!
//! Stateless — every method runs inside the caller's posting transaction
//! (`txn`), so it holds no `DBProvider` (mirrors
//! [`crate::infra::posting::idempotency::IdempotencyGate`] /
//! [`crate::infra::storage::repo::ChainStateRepo`]); tenant isolation runs
//! through the `SecureORM` layer (`.secure().scope_with(scope)` for reads,
//! `.scope_with_model(scope, &am)` for the freeze upsert — the validating
//! variant rejects a mismatched `(scope, tenant)`).

use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use toolkit_db::DbError;
use toolkit_db::secure::{
    AccessScope, DbTx, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::infra::posting::service::business;
use crate::infra::storage::entity::scope_freeze;
use time::OffsetDateTime;

/// `period_id` sentinel for a tenant-wide freeze (every period in the tenant).
const PERIOD_ALL: &str = "ALL";

/// Map a [`ScopeError`] to [`DbError`] **preserving the inner `sea_orm::DbErr`
/// variant** (mirrors [`crate::infra::storage::repo::ChainStateRepo`]'s
/// `scope_to_db`). This is load-bearing for retry: [`TamperFreezeGuard::check`]
/// runs [`ScopeFreezeRepo::active_freeze`] inside the post's `SERIALIZABLE`
/// transaction, so a statement-time serialization failure (SSI 40001) on the
/// lockless freeze read surfaces as `ScopeError::Db(DbErr::Exec | DbErr::Query)`.
/// Keeping that variant lets the posting's `transaction_with_retry` contention
/// classifier recognise it and retry the post; stringifying it (the old
/// `RepoError::Db(format!(…))`) buried it in a `DbErr::Custom`, which the
/// classifier treats as the NON-retryable business sentinel — so a transient
/// abort on this read surfaced as a 500 instead of a retry.
fn scope_to_db(e: ScopeError) -> DbError {
    match e {
        ScopeError::Db(db_err) => DbError::Sea(db_err),
        other => DbError::Other(anyhow::anyhow!("scope-freeze scope: {other}")),
    }
}

/// Scope-freeze repository. Stateless — every method runs inside the caller's
/// posting transaction (`txn`).
#[derive(Clone, Default)]
pub struct ScopeFreezeRepo;

impl ScopeFreezeRepo {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Return `true` iff an ACTIVE (`cleared_at IS NULL`) freeze row exists for
    /// `tenant` whose `period_id` is `'ALL'` (tenant-wide) OR equals `period_id`
    /// (that period). The read takes no row lock (mirrors
    /// [`crate::infra::storage::repo::ChainStateRepo::read_tip`]); the posting's
    /// `SERIALIZABLE` transaction is the concurrency backstop.
    ///
    /// # Errors
    /// [`DbError`] on a storage / scope failure, with the inner `sea_orm::DbErr`
    /// variant preserved (see [`scope_to_db`]) so a serialization abort on this
    /// lockless read stays retryable on the post path.
    pub async fn active_freeze(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
    ) -> Result<bool, DbError> {
        let row = scope_freeze::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(scope_freeze::Column::TenantId.eq(tenant))
                    .add(scope_freeze::Column::ClearedAt.is_null())
                    .add(scope_freeze::Column::PeriodId.is_in([PERIOD_ALL, period_id])),
            )
            .one(txn)
            .await
            .map_err(scope_to_db)?;

        Ok(row.is_some())
    }

    /// Set (insert) a freeze row for `(tenant, scope_kind, period_id)` inside
    /// `txn`: `INSERT … ON CONFLICT (tenant_id, scope, period_id) DO NOTHING` —
    /// re-setting an existing freeze is a no-op (the original `frozen_at` /
    /// `reason` / `set_by` stand). `cleared_by` / `cleared_at` are NULL (the row
    /// is ACTIVE); `frozen_at` is `now()`.
    ///
    /// Returns `true` iff a row was newly inserted (the freeze actually
    /// transitioned from absent to active), and `false` if an active freeze
    /// already existed (the `ON CONFLICT DO NOTHING` no-op). The caller uses this
    /// to write the §5.2 `freeze-set-clear` secured-audit record ONLY on a real
    /// transition — so the daily Verifier re-freezing an already-frozen tenant
    /// does not flood the audit chain with duplicate set records.
    ///
    /// # Errors
    /// [`DbError`] on a storage / scope failure, with the inner `sea_orm::DbErr`
    /// variant preserved (see [`scope_to_db`]).
    #[allow(
        clippy::too_many_arguments,
        reason = "freeze identity (tenant/scope/period) + audit (reason/set_by) over the caller's txn/scope"
    )]
    pub async fn set(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        scope_kind: &str,
        period_id: &str,
        reason: &str,
        set_by: &str,
    ) -> Result<bool, DbError> {
        let am = scope_freeze::ActiveModel {
            tenant_id: Set(tenant),
            scope: Set(scope_kind.to_owned()),
            period_id: Set(period_id.to_owned()),
            reason: Set(reason.to_owned()),
            frozen_at: Set(OffsetDateTime::now_utc()),
            set_by: Set(set_by.to_owned()),
            cleared_by: Set(None),
            cleared_at: Set(None),
        };
        let on_conflict = OnConflict::columns([
            scope_freeze::Column::TenantId,
            scope_freeze::Column::Scope,
            scope_freeze::Column::PeriodId,
        ])
        .do_nothing()
        .to_owned();

        match scope_freeze::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(scope_to_db)?
            .on_conflict_raw(on_conflict)
            .exec(txn)
            .await
        {
            // A fresh insert — the freeze transitioned from absent to active.
            Ok(_) => Ok(true),
            // The freeze row already existed (the conflict swallowed the insert —
            // re-setting an existing freeze is a no-op, no state transition).
            Err(ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => Ok(false),
            Err(e) => Err(scope_to_db(e)),
        }
    }

    /// Clear an ACTIVE freeze row for `(tenant, scope_kind, period_id)` inside
    /// `txn`: `UPDATE … SET cleared_at = now(), cleared_by = $cleared_by` WHERE
    /// the PK matches AND `cleared_at IS NULL`. A no-op if the row is absent or
    /// already cleared (`rows_affected == 0`).
    ///
    /// # Errors
    /// [`DbError`] on a storage / scope failure, with the inner `sea_orm::DbErr`
    /// variant preserved (see [`scope_to_db`]).
    #[allow(
        dead_code,
        reason = "manual/Audit freeze-clear path (wired in a later slice)"
    )]
    pub async fn clear(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        scope_kind: &str,
        period_id: &str,
        cleared_by: &str,
    ) -> Result<(), DbError> {
        scope_freeze::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                scope_freeze::Column::ClearedAt,
                Expr::value(Some(OffsetDateTime::now_utc())),
            )
            .col_expr(
                scope_freeze::Column::ClearedBy,
                Expr::value(Some(cleared_by.to_owned())),
            )
            .filter(
                Condition::all()
                    .add(scope_freeze::Column::TenantId.eq(tenant))
                    .add(scope_freeze::Column::Scope.eq(scope_kind))
                    .add(scope_freeze::Column::PeriodId.eq(period_id))
                    .add(scope_freeze::Column::ClearedAt.is_null()),
            )
            .exec(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(())
    }
}

/// Posting guard that rejects a fresh post into a tamper-frozen scope. Runs as
/// a fail-fast step in [`crate::infra::posting::service::PostingService`]: a
/// fresh post (Claimed) into a frozen scope is rejected BEFORE any write; an
/// idempotent replay is unaffected (it returns earlier in the posting body).
///
/// Stateless — holds only a stateless [`ScopeFreezeRepo`] (mirrors
/// [`crate::infra::posting::chain::ChainSealer`]).
#[derive(Clone, Default)]
pub struct TamperFreezeGuard {
    freeze: ScopeFreezeRepo,
}

impl TamperFreezeGuard {
    #[must_use]
    pub fn new() -> Self {
        Self {
            freeze: ScopeFreezeRepo::new(),
        }
    }

    /// Reject the post if an ACTIVE freeze covers `(tenant, period_id)`.
    ///
    /// Returns the sentinel-encoded [`DomainError::TamperVerificationFailed`]
    /// (a `DbError` that forces rollback and is NON-retryable) when the scope is
    /// frozen, so the post fails fast before any write; `Ok(())` otherwise.
    ///
    /// # Errors
    /// A sentinel [`DbError`] carrying [`DomainError::TamperVerificationFailed`]
    /// when the scope is frozen; an infrastructure [`DbError`] on a storage /
    /// scope failure, with the inner `sea_orm::DbErr` variant preserved (see
    /// [`scope_to_db`]) so a serialization abort on the freeze read stays
    /// retryable on the post path.
    pub async fn check(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
    ) -> Result<(), DbError> {
        let frozen = self
            .freeze
            .active_freeze(txn, scope, tenant, period_id)
            .await?;
        if frozen {
            return Err(business(DomainError::TamperVerificationFailed(format!(
                "scope frozen for tenant {tenant} period {period_id}"
            ))));
        }
        Ok(())
    }
}
