//! Write-path repo methods (ADR-0006): garbage-collection bookkeeping,
//! `insert_active`, `switch_value`, `delete_by_id`, `list_expired` support,
//! `delete_expired_row`.
//!
//! Every method that touches more than one row/table runs inside ONE
//! [`toolkit_db::DBProvider::transaction`] call — see the module docs on
//! [`crate::domain::secret::repo::SecretRepo`].

use std::future::Future;
use std::pin::Pin;

use credstore_sdk::{SharingMode, TenantId, ValueId};
use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DbTx, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::secret::model::{GcEntry, GcReason, NewSecret, SecretRow, SecretStatus};
use crate::infra::storage::entity;
use crate::infra::storage::repo_impl::helpers::{
    SecretRepoImpl, entity_to_model, gc_entity_to_model, map_scope_err, sharing_to_i16,
};

// ── Garbage-collection bookkeeping (`credstore_value_gc`) ───────────────────

pub(super) async fn gc_insert_pending(
    repo: &SecretRepoImpl,
    value_id: ValueId,
    tenant_id: TenantId,
) -> Result<(), DomainError> {
    let conn = repo.db.conn()?;
    let am = entity::value_gc::ActiveModel {
        value_id: ActiveValue::Set(value_id.0),
        tenant_id: ActiveValue::Set(tenant_id.0),
        reason: ActiveValue::Set(GcReason::Pending.as_smallint()),
        enqueued_at: ActiveValue::Set(OffsetDateTime::now_utc()),
    };
    entity::value_gc::Entity::insert(am)
        .secure()
        .scope_unchecked(&AccessScope::allow_all())
        .map_err(map_scope_err)?
        .exec(&conn)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

pub(super) async fn gc_delete(
    repo: &SecretRepoImpl,
    value_id: ValueId,
) -> Result<bool, DomainError> {
    let conn = repo.db.conn()?;
    let rows_affected = entity::value_gc::Entity::delete_many()
        .filter(entity::value_gc::Column::ValueId.eq(value_id.0))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .map_err(map_scope_err)?
        .rows_affected;
    Ok(rows_affected > 0)
}

pub(super) async fn gc_mark(
    repo: &SecretRepoImpl,
    value_id: ValueId,
    reason: GcReason,
) -> Result<bool, DomainError> {
    let conn = repo.db.conn()?;
    let rows_affected = entity::value_gc::Entity::update_many()
        .col_expr(
            entity::value_gc::Column::Reason,
            Expr::value(reason.as_smallint()),
        )
        .filter(entity::value_gc::Column::ValueId.eq(value_id.0))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .map_err(map_scope_err)?
        .rows_affected;
    Ok(rows_affected > 0)
}

pub(super) async fn gc_list(
    repo: &SecretRepoImpl,
    limit: u64,
) -> Result<Vec<GcEntry>, DomainError> {
    let conn = repo.db.conn()?;
    let rows = entity::value_gc::Entity::find()
        .order_by(entity::value_gc::Column::EnqueuedAt, Order::Asc)
        .limit(limit)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .map_err(map_scope_err)?;
    rows.iter().map(gc_entity_to_model).collect()
}

/// `EXISTS` via `SELECT … LIMIT 1` — never `COUNT` (platform rule).
pub(super) async fn is_value_referenced(
    repo: &SecretRepoImpl,
    value_id: ValueId,
) -> Result<bool, DomainError> {
    let conn = repo.db.conn()?;
    let row = entity::secrets::Entity::find()
        .filter(entity::secrets::Column::ValueId.eq(value_id.0))
        .limit(1)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .map_err(map_scope_err)?;
    Ok(row.is_some())
}

// ── Write protocol (ADR-0006 §6.2) ──────────────────────────────────────────

pub(super) async fn insert_active(
    repo: &SecretRepoImpl,
    scope: &AccessScope,
    new: &NewSecret,
) -> Result<(), DomainError> {
    let scope = scope.clone();
    let new = new.clone();
    repo.db
        .transaction(move |tx: &DbTx<'_>| {
            Box::pin(async move { insert_active_tx(tx, &scope, &new).await })
                as Pin<Box<dyn Future<Output = Result<(), DomainError>> + Send + '_>>
        })
        .await
}

async fn insert_active_tx(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    new: &NewSecret,
) -> Result<(), DomainError> {
    let now = OffsetDateTime::now_utc();
    let am = entity::secrets::ActiveModel {
        id: ActiveValue::Set(new.id),
        tenant_id: ActiveValue::Set(new.tenant_id.0),
        reference: ActiveValue::Set(new.reference.as_ref().to_owned()),
        sharing: ActiveValue::Set(sharing_to_i16(new.sharing)),
        owner_id: ActiveValue::Set(new.owner_id.0),
        status: ActiveValue::Set(SecretStatus::Active.as_smallint()),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
        version: ActiveValue::NotSet,
        secret_type_uuid: ActiveValue::Set(new.secret_type_uuid),
        expires_at: ActiveValue::Set(new.expires_at),
        value_id: ActiveValue::Set(Some(new.value_id.0)),
        value_fp: ActiveValue::Set(Some(new.value_fp.clone())),
        fp_key_id: ActiveValue::Set(Some(new.fp_key_id)),
        fallback: ActiveValue::Set(1), // Fallback::Inherit — a fresh row always inherits.
    };
    // scope_unchecked: INSERT cannot subtree-clamp on a row that doesn't exist yet.
    entity::secrets::Entity::insert(am)
        .secure()
        .scope_unchecked(scope)
        .map_err(map_scope_err)?
        .exec(tx)
        .await
        .map_err(map_scope_err)?;

    // The intent is now realized: drop the pending gc entry for this id.
    entity::value_gc::Entity::delete_many()
        .filter(entity::value_gc::Column::ValueId.eq(new.value_id.0))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(tx)
        .await
        .map_err(map_scope_err)?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one CAS with every field it may update"
)]
pub(super) async fn switch_value(
    repo: &SecretRepoImpl,
    scope: &AccessScope,
    id: Uuid,
    expected_version: Option<i64>,
    sharing: SharingMode,
    expires_at: Option<OffsetDateTime>,
    new_value_id: ValueId,
    value_fp: Vec<u8>,
    fp_key_id: i16,
) -> Result<Option<(SecretRow, Option<ValueId>)>, DomainError> {
    let scope = scope.clone();
    repo.db
        .transaction(move |tx: &DbTx<'_>| {
            Box::pin(async move {
                switch_value_tx(
                    tx,
                    &scope,
                    id,
                    expected_version,
                    sharing,
                    expires_at,
                    new_value_id,
                    value_fp,
                    fp_key_id,
                )
                .await
            })
                as Pin<
                    Box<
                        dyn Future<
                                Output = Result<Option<(SecretRow, Option<ValueId>)>, DomainError>,
                            > + Send
                            + '_,
                    >,
                >
        })
        .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "one CAS with every field it may update"
)]
async fn switch_value_tx(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    id: Uuid,
    expected_version: Option<i64>,
    sharing: SharingMode,
    expires_at: Option<OffsetDateTime>,
    new_value_id: ValueId,
    value_fp: Vec<u8>,
    fp_key_id: i16,
) -> Result<Option<(SecretRow, Option<ValueId>)>, DomainError> {
    let now = OffsetDateTime::now_utc();

    // Lock the row for the duration of this transaction (a no-op on SQLite,
    // which serialises writers anyway) so a concurrent switch's own read of
    // `value_id` can never observe a value between our read and our write.
    let current = entity::secrets::Entity::find()
        .lock_exclusive()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(entity::secrets::Column::Id.eq(id))
                .add(entity::secrets::Column::Status.eq(SecretStatus::Active.as_smallint())),
        )
        .one(tx)
        .await
        .map_err(map_scope_err)?;

    let Some(current) = current else {
        return Ok(None);
    };
    if let Some(expected) = expected_version
        && current.version != expected
    {
        return Ok(None);
    }
    let old_value_id = current.value_id;

    entity::secrets::Entity::update_many()
        .col_expr(
            entity::secrets::Column::ValueId,
            Expr::value(new_value_id.0),
        )
        .col_expr(
            entity::secrets::Column::ValueFp,
            Expr::value(Some(value_fp)),
        )
        .col_expr(
            entity::secrets::Column::FpKeyId,
            Expr::value(Some(fp_key_id)),
        )
        .col_expr(
            entity::secrets::Column::Sharing,
            Expr::value(sharing_to_i16(sharing)),
        )
        .col_expr(entity::secrets::Column::ExpiresAt, Expr::value(expires_at))
        .col_expr(
            entity::secrets::Column::Version,
            Expr::col(entity::secrets::Column::Version).add(1_i64),
        )
        .col_expr(entity::secrets::Column::UpdatedAt, Expr::value(now))
        .col_expr(
            entity::secrets::Column::Status,
            Expr::value(SecretStatus::Active.as_smallint()),
        )
        .filter(Condition::all().add(entity::secrets::Column::Id.eq(id)))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;

    // The intent is realized: drop the pending gc entry for the new id.
    entity::value_gc::Entity::delete_many()
        .filter(entity::value_gc::Column::ValueId.eq(new_value_id.0))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(tx)
        .await
        .map_err(map_scope_err)?;

    // The previous version, if any, is now unreferenced by this row.
    if let Some(old_id) = old_value_id {
        let am = entity::value_gc::ActiveModel {
            value_id: ActiveValue::Set(old_id),
            tenant_id: ActiveValue::Set(current.tenant_id),
            reason: ActiveValue::Set(GcReason::Superseded.as_smallint()),
            enqueued_at: ActiveValue::Set(now),
        };
        entity::value_gc::Entity::insert(am)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .map_err(map_scope_err)?
            .exec(tx)
            .await
            .map_err(map_scope_err)?;
    }

    let row = entity::secrets::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(entity::secrets::Column::Id.eq(id)))
        .one(tx)
        .await
        .map_err(map_scope_err)?
        .ok_or_else(|| DomainError::internal("switch_value: row vanished after its own update"))?;
    let row = entity_to_model(row)?;
    Ok(Some((row, old_value_id.map(ValueId))))
}

pub(super) async fn delete_by_id(
    repo: &SecretRepoImpl,
    scope: &AccessScope,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<Option<ValueId>, DomainError> {
    let scope = scope.clone();
    repo.db
        .transaction(move |tx: &DbTx<'_>| {
            Box::pin(async move {
                delete_row_tx(tx, &scope, id, expected_version, GcReason::Removed).await
            })
                as Pin<Box<dyn Future<Output = Result<Option<ValueId>, DomainError>> + Send + '_>>
        })
        .await
}

pub(super) async fn delete_expired_row(
    repo: &SecretRepoImpl,
    id: Uuid,
) -> Result<Option<ValueId>, DomainError> {
    repo.db
        .transaction(move |tx: &DbTx<'_>| {
            Box::pin(async move {
                match delete_row_tx(tx, &AccessScope::allow_all(), id, None, GcReason::Removed)
                    .await
                {
                    // The maintenance job runs without a caller and races
                    // nothing that should surface as an error: a row already
                    // gone (a concurrent client delete beat the job to it) is
                    // simply nothing left to do.
                    Err(DomainError::NotFound) => Ok(None),
                    other => other,
                }
            })
                as Pin<Box<dyn Future<Output = Result<Option<ValueId>, DomainError>> + Send + '_>>
        })
        .await
}

async fn delete_row_tx(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    id: Uuid,
    expected_version: Option<i64>,
    reason: GcReason,
) -> Result<Option<ValueId>, DomainError> {
    let now = OffsetDateTime::now_utc();
    let mut filter = Condition::all().add(entity::secrets::Column::Id.eq(id));
    if let Some(v) = expected_version {
        filter = filter.add(entity::secrets::Column::Version.eq(v));
    }

    // Lock + read so the value_id we enqueue for gc is exactly the one this
    // transaction deletes, atomically.
    let current = entity::secrets::Entity::find()
        .lock_exclusive()
        .secure()
        .scope_with(scope)
        .filter(filter)
        .one(tx)
        .await
        .map_err(map_scope_err)?;
    let Some(current) = current else {
        return Err(DomainError::NotFound);
    };
    let value_id = current.value_id;

    entity::secrets::Entity::delete_many()
        .filter(Condition::all().add(entity::secrets::Column::Id.eq(id)))
        .secure()
        .scope_with(scope)
        .exec(tx)
        .await
        .map_err(map_scope_err)?;

    if let Some(vid) = value_id {
        let am = entity::value_gc::ActiveModel {
            value_id: ActiveValue::Set(vid),
            tenant_id: ActiveValue::Set(current.tenant_id),
            reason: ActiveValue::Set(reason.as_smallint()),
            enqueued_at: ActiveValue::Set(now),
        };
        entity::value_gc::Entity::insert(am)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .map_err(map_scope_err)?
            .exec(tx)
            .await
            .map_err(map_scope_err)?;
    }
    Ok(value_id.map(ValueId))
}
