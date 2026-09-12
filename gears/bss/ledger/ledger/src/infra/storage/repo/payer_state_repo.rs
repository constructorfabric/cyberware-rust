//! `PayerStateRepo` — the payer lifecycle row (`bss.ledger_payer_state`) plus the
//! closure balance check (VHP-1852 Phase 2). Absence of a row means OPEN; closure
//! upserts the row to `CLOSED` with the approver + the closed-with-open-balance
//! marker. The outstanding-balance check reads the per-payer AR cache
//! (`ledger_ar_payer_balance`); a non-zero grain means closing strands a balance,
//! which routes the closure through dual-control.
//!
//! Reads + the closure upsert run out-of-txn through the PDP-compiled scope
//! (SQL-level BOLA). The closure upsert is a single idempotent statement
//! (`INSERT … ON CONFLICT DO UPDATE`), so it needs no explicit transaction.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::status::PAYER_LIFECYCLE_CLOSED;
use crate::infra::storage::entity::{ar_payer_balance, payer_state};
use time::OffsetDateTime;

/// SeaORM-backed payer-lifecycle repository.
#[derive(Clone)]
pub struct PayerStateRepo {
    db: DBProvider<DbError>,
}

impl PayerStateRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Read the payer-lifecycle row, or `None` (absence ⇒ OPEN).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn read(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer_tenant_id: Uuid,
    ) -> Result<Option<payer_state::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        payer_state::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(payer_state::Column::TenantId.eq(tenant))
                    .add(payer_state::Column::PayerTenantId.eq(payer_tenant_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read ledger_payer_state: {e}")))
    }

    /// Whether the payer still holds a non-zero AR grain — closing it would strand
    /// a balance, so the closure must go through dual-control (design 01 §4.2).
    /// MVP reads the AR cache only; unallocated / reusable-credit customer balances
    /// are a follow-up.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn has_outstanding_balance(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer_tenant_id: Uuid,
    ) -> Result<bool, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let nonzero = ar_payer_balance::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(ar_payer_balance::Column::TenantId.eq(tenant))
                    .add(ar_payer_balance::Column::PayerTenantId.eq(payer_tenant_id))
                    .add(ar_payer_balance::Column::BalanceMinor.ne(0)),
            )
            .one(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read ledger_ar_payer_balance: {e}")))?;
        Ok(nonzero.is_some())
    }

    /// Upsert the payer-lifecycle row to `CLOSED`, stamping the approver, the
    /// closed-with-open-balance marker, and the change time. Idempotent
    /// (`ON CONFLICT DO UPDATE`); a re-close lands on the same PK.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn close(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        payer_tenant_id: Uuid,
        approved_by: Uuid,
        closed_with_open_balance: bool,
    ) -> Result<(), DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let now = OffsetDateTime::now_utc();
        let am = payer_state::ActiveModel {
            tenant_id: Set(tenant),
            payer_tenant_id: Set(payer_tenant_id),
            lifecycle_state: Set(PAYER_LIFECYCLE_CLOSED.to_owned()),
            closed_with_open_balance: Set(closed_with_open_balance),
            approved_by: Set(Some(approved_by)),
            changed_at: Set(Some(now)),
        };
        let on_conflict = SecureOnConflict::<payer_state::Entity>::columns([
            payer_state::Column::TenantId,
            payer_state::Column::PayerTenantId,
        ])
        .value(
            payer_state::Column::LifecycleState,
            Expr::value(PAYER_LIFECYCLE_CLOSED.to_owned()),
        )
        .and_then(|oc| {
            oc.value(
                payer_state::Column::ClosedWithOpenBalance,
                Expr::value(closed_with_open_balance),
            )
        })
        .and_then(|oc| oc.value(payer_state::Column::ApprovedBy, Expr::value(approved_by)))
        .and_then(|oc| oc.value(payer_state::Column::ChangedAt, Expr::value(now)))
        .map_err(|e| DomainError::Internal(format!("ledger_payer_state on_conflict: {e}")))?;
        payer_state::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| DomainError::Internal(format!("ledger_payer_state scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("close ledger_payer_state: {e}")))?;
        Ok(())
    }
}
