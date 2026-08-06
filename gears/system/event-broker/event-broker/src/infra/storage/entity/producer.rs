//! `Storage`'s producer-registration row (backs `ProducerRegistry`,
//! eb-single-process-implementation D2/decision log entry 28).
//! `tenant_id` is a new field vs. today's in-memory stand-in - captured from
//! `ctx.subject_tenant_id()` at `register()` time, never overridable, matching
//! `ConsumerGroup`'s own `tenant_id`/`owner_principal_id` shape.

use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "event_broker_producer")]
#[secure(tenant_col = "tenant_id", owner_col = "owner_id", no_resource, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// The registering subject (`ctx.subject_id()`) - `ProducerRegistry::
    /// register`'s existing `owner` parameter.
    pub owner_id: Uuid,
    /// `"chained"` | `"monotonic"` (`domain::ingest::ProducerMode`).
    pub mode: String,
    pub client_agent: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
