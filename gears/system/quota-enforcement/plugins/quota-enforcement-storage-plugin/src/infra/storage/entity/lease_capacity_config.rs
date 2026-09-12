//! `qe_lease_capacity_config`: per-`(tenant, metric)` active-lease cap
//! (invariant I7). The `(*, *)` row is the platform default.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "qe_lease_capacity_config")]
#[secure(no_tenant, no_resource, no_owner, no_type)]
pub struct Model {
    /// Tenant id, or `*` for every tenant.
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_key: String,
    /// Metric instance id, or `*` for every metric.
    #[sea_orm(primary_key, auto_increment = false)]
    pub metric_key: String,
    /// Maximum concurrent active leases.
    pub max_active_leases: i32,
    /// Last write.
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
