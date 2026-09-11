//! `Storage`'s `cursor` namespace (eb-single-process-implementation D2) -
//! the one durable piece of consumer-group progress (DESIGN.md's own
//! "the cursor (the only persistent piece)"). Composite natural key
//! `(consumer_group, topic_id, partition)`, matching
//! `CursorRepo::find_cursor`'s own lookup shape. `topic_id` is the
//! `SpecificationManager`-resolved integer surrogate (D1), not the raw GTS
//! id string - resolved by the repo implementation before every read/write,
//! not stored redundantly as a string here. `tenant_id` is denormalized from
//! the owning `ConsumerGroup` at write time (decision log entry 28) - no
//! independent owner/resource dimension for this entity.

use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "event_broker_cursor")]
#[secure(tenant_col = "tenant_id", no_owner, no_resource, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub consumer_group: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub topic_id: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub partition: i32,
    pub tenant_id: Uuid,
    /// Stored as an integer: sea-orm owns the column type. The crossing
    /// into `Sequence` happens at the storage facade, not here.
    pub offset: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
