//! `SeaORM` entity for `bss.pricing_outbox` — one transactionally-enqueued
//! event (`design/01-foundation.md` §3.7).
//!
//! `event_name` is one of the frozen names
//! (`domain::events::CatalogEvent`, which is the roster and the only place its
//! size is stated), `seq` orders events per
//! `(tenant_id, aggregate_id)` and nowhere else, and `dedup_key` is what makes
//! at-least-once delivery safe for a consumer.


use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_outbox")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "aggregate_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub outbox_id: Uuid,
    pub tenant_id: Uuid,
    pub aggregate_id: Uuid,
    pub event_name: String,
    pub seq: i64,
    pub payload: JsonValue,
    pub dedup_key: String,
    pub correlation_id: Uuid,
    pub enqueued_at: OffsetDateTime,
    /// `None` while the relay has not delivered the row yet.
    pub published_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
