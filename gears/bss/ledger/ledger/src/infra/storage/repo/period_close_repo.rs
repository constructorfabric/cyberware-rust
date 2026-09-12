//! `PeriodCloseRepo` — the close-process table (`bss.ledger_period_close`),
//! keyed by `(tenant_id, legal_entity_id, period_id)`. Owns the lifecycle row
//! the two-phase close drives (Slice 7, design §4.5). The Foundation
//! `fiscal_period.status` stays the posting gate; this row tracks the process.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::RepoError;
use crate::infra::storage::entity::period_close;
use time::OffsetDateTime;

/// SeaORM-backed period-close-process repository.
#[derive(Clone)]
pub struct PeriodCloseRepo {
    db: DBProvider<DbError>,
}

impl PeriodCloseRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Upsert the close-process row to `status` (create-or-advance). The mutable
    /// columns (`status`, `blocked_reasons`, `recon_watermark`, `closed_at`) are
    /// set from the args; the PK identifies the period. `initiated_by` is set
    /// only on the initial INSERT (the first closer).
    ///
    /// # Errors
    /// Returns [`RepoError::Db`] if conflict-clause construction, scope
    /// validation, or the upsert fails.
    #[allow(
        clippy::too_many_arguments,
        reason = "the close-process row carries the full gate result; a param object would not reduce the call-site count"
    )]
    pub async fn upsert_status(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        legal_entity: Uuid,
        period_id: &str,
        status: &str,
        initiated_by: &str,
        blocked_reasons: Option<JsonValue>,
        recon_watermark: Option<i64>,
        closed_at: Option<OffsetDateTime>,
    ) -> Result<(), RepoError> {
        let am = period_close::ActiveModel {
            tenant_id: Set(tenant),
            legal_entity_id: Set(legal_entity),
            period_id: Set(period_id.to_owned()),
            status: Set(status.to_owned()),
            initiated_by: Set(initiated_by.to_owned()),
            blocked_reasons: Set(blocked_reasons.clone()),
            recon_watermark: Set(recon_watermark),
            reopen_approval_id: Set(None),
            reopened_by: Set(None),
            closed_at: Set(closed_at),
        };
        let on_conflict = SecureOnConflict::<period_close::Entity>::columns([
            period_close::Column::TenantId,
            period_close::Column::LegalEntityId,
            period_close::Column::PeriodId,
        ])
        .value(period_close::Column::Status, Expr::value(status))
        .and_then(|oc| {
            oc.value(
                period_close::Column::BlockedReasons,
                Expr::value(blocked_reasons),
            )
        })
        .and_then(|oc| {
            oc.value(
                period_close::Column::ReconWatermark,
                Expr::value(recon_watermark),
            )
        })
        .and_then(|oc| oc.value(period_close::Column::ClosedAt, Expr::value(closed_at)))
        .map_err(|e| RepoError::Db(format!("ledger_period_close on_conflict: {e}")))?;
        period_close::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_period_close scope: {e}")))?
            .on_conflict(on_conflict)
            .exec_with_returning(txn)
            .await
            .map_err(|e| RepoError::Db(format!("upsert ledger_period_close: {e}")))?;
        Ok(())
    }

    /// Read the close-process row (out-of-txn). SQL-level BOLA: a foreign tenant
    /// yields no row.
    ///
    /// # Errors
    /// Returns [`DomainError::Internal`] if acquiring a connection or reading
    /// the close-process row fails.
    pub async fn read(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        legal_entity: Uuid,
        period_id: &str,
    ) -> Result<Option<period_close::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let row = period_close::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(period_close::Column::TenantId.eq(tenant))
                    .add(period_close::Column::LegalEntityId.eq(legal_entity))
                    .add(period_close::Column::PeriodId.eq(period_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read ledger_period_close: {e}")))?;
        Ok(row)
    }
}
