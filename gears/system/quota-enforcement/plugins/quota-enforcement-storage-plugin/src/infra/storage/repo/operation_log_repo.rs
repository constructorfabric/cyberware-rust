//! `qe_operation_log`: append-only, in the mutation's transaction.

use sea_orm::sea_query::Condition;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::ports::Actor;
use crate::infra::storage::entity::operation_log::{self, Column, Entity};

/// Operation name of a Quota creation.
pub const OP_QUOTA_CREATE: &str = "quota.create";
/// Operation name of a Quota update.
pub const OP_QUOTA_UPDATE: &str = "quota.update";
/// Operation name of a Quota deactivation.
pub const OP_QUOTA_DEACTIVATE: &str = "quota.deactivate";

/// One entry to append.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    /// Tenant of the target.
    pub tenant_id: Uuid,
    /// Target Quota.
    pub quota_id: Uuid,
    /// One of the `OP_*` constants.
    pub operation: &'static str,
    /// Who did it.
    pub actor: &'a Actor,
    /// The Quota's record version after the operation.
    pub record_version: i32,
    /// Content-free detail.
    pub detail: String,
    /// When.
    pub occurred_at: OffsetDateTime,
}

/// Append one entry.
///
/// # Errors
///
/// The scope or database error of the insert.
pub async fn append(
    runner: &impl DBRunner,
    scope: &AccessScope,
    entry: Entry<'_>,
) -> Result<operation_log::Model, ScopeError> {
    secure_insert::<Entity>(
        operation_log::ActiveModel {
            id: ActiveValue::Set(Uuid::now_v7()),
            tenant_id: ActiveValue::Set(entry.tenant_id),
            quota_id: ActiveValue::Set(Some(entry.quota_id)),
            operation: ActiveValue::Set(entry.operation.to_owned()),
            actor_subject_id: ActiveValue::Set(entry.actor.subject_id),
            actor_subject_type: ActiveValue::Set(entry.actor.subject_type.clone()),
            record_version: ActiveValue::Set(Some(entry.record_version)),
            detail: ActiveValue::Set(entry.detail),
            occurred_at: ActiveValue::Set(entry.occurred_at),
        },
        scope,
        runner,
    )
    .await
}

/// Entries of one Quota inside `scope`, oldest first.
///
/// # Errors
///
/// The scope or database error of the read.
pub async fn entries_for_quota(
    runner: &impl DBRunner,
    scope: &AccessScope,
    quota_id: Uuid,
) -> Result<Vec<operation_log::Model>, ScopeError> {
    Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(Column::QuotaId.eq(quota_id)))
        .order_by(Column::OccurredAt, sea_orm::Order::Asc)
        .all(runner)
        .await
}
