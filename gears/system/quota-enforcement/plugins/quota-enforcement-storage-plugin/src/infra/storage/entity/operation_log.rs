//! `qe_operation_log`: who did what to which Quota. Appended in the same
//! transaction as the mutation it records.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "qe_operation_log")]
#[secure(tenant_col = "tenant_id", resource_col = "quota_id", no_owner, no_type)]
pub struct Model {
    /// Entry id, `UUIDv7`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// Tenant of the target.
    pub tenant_id: Uuid,
    /// Target Quota, when the operation has one.
    #[sea_orm(nullable)]
    pub quota_id: Option<Uuid>,
    /// One of the `OP_*` constants of the repository.
    pub operation: String,
    /// The caller's subject id.
    pub actor_subject_id: Uuid,
    /// The caller's subject type, when known.
    #[sea_orm(nullable)]
    pub actor_subject_type: Option<String>,
    /// The Quota's record version after the operation, when it has one.
    #[sea_orm(nullable)]
    pub record_version: Option<i32>,
    /// Free-form, content-free detail (field names, never values).
    pub detail: String,
    /// When the operation committed.
    pub occurred_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
