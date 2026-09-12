//! `qe_contention_timeout_config`: per-metric acquisition contention timeout
//! (invariant I8). The `*` row is the platform default.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "qe_contention_timeout_config")]
#[secure(no_tenant, no_resource, no_owner, no_type)]
pub struct Model {
    /// Metric instance id, or `*` for the platform default.
    #[sea_orm(primary_key, auto_increment = false)]
    pub metric_key: String,
    /// Contention timeout in milliseconds. `0` means fail fast.
    pub timeout_ms: i64,
    /// Last write.
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
