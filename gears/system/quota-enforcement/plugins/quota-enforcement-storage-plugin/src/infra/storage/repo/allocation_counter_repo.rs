//! `qe_quota_allocation_counters`: the initial row of an allocation Quota and
//! the locked read the cap guard (invariant I6) takes under the Quota's lock.
//! Lock order is Quota row, then its counter rows (ADR-0002); the lease and
//! consumption features keep it.

use sea_orm::sea_query::LockType;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::infra::storage::entity::quota_allocation_counter::{self, Column, Entity};

/// The counter row a new allocation Quota starts with: nothing in flight.
///
/// # Errors
///
/// The scope or database error of the insert.
pub async fn insert_initial(
    runner: &impl DBRunner,
    scope: &AccessScope,
    quota_id: Uuid,
    tenant_id: Uuid,
    now: OffsetDateTime,
) -> Result<quota_allocation_counter::Model, ScopeError> {
    secure_insert::<Entity>(
        quota_allocation_counter::ActiveModel {
            quota_id: ActiveValue::Set(quota_id),
            tenant_id: ActiveValue::Set(tenant_id),
            in_flight: ActiveValue::Set(0),
            record_version: ActiveValue::Set(1),
            updated_at: ActiveValue::Set(now),
        },
        scope,
        runner,
    )
    .await
}

/// The in-flight amount of `quota_id`, locked for update; `None` when the
/// Quota has no counter row (it is not an allocation Quota).
///
/// # Errors
///
/// The scope or database error of the read.
pub async fn read_in_flight_for_update(
    runner: &impl DBRunner,
    scope: &AccessScope,
    quota_id: Uuid,
) -> Result<Option<i64>, ScopeError> {
    let row = Entity::find()
        .filter(Column::QuotaId.eq(quota_id))
        .lock(LockType::Update)
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?;
    Ok(row.map(|r| r.in_flight))
}
