//! `qe_quota_allocation_counters`: the in-flight counter of an allocation
//! Quota, created with the Quota. The cap guard (invariant I6) reads it under
//! the Quota's row lock.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "qe_quota_allocation_counters")]
#[secure(tenant_col = "tenant_id", resource_col = "quota_id", no_owner, no_type)]
pub struct Model {
    /// The allocation Quota.
    #[sea_orm(primary_key, auto_increment = false)]
    pub quota_id: Uuid,
    /// The Quota's tenant, denormalized for scoping.
    pub tenant_id: Uuid,
    /// Capacity currently held.
    pub in_flight: i64,
    /// Increments once per accepted mutation.
    pub record_version: i32,
    /// Last write.
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
