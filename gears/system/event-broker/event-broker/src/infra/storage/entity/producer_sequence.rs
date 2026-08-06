//! `Storage`'s per-`(producer, topic, partition)` chain-sequence row - backs
//! `IdempotencyGuard::check_and_record` (eb-single-process-implementation
//! D2). `tenant_id` is denormalized from the owning `producer` row at write
//! time (same pattern as `cursor::Model`'s denormalized `tenant_id` from its
//! owning `consumer_group`) - `check_and_record`'s own signature carries no
//! tenant, so the repo implementation resolves it via the producer row
//! before the first insert.

use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "event_broker_producer_sequence")]
#[secure(tenant_col = "tenant_id", no_owner, no_resource, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub producer_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub topic: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub partition: i32,
    pub tenant_id: Uuid,
    pub last_sequence: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
