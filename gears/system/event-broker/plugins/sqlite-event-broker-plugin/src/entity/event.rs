//! The durable, append-only `(topic, partition)` log every event lands in.
//!
//! `id` is a real `Uuid` primary key, so this entity gets the full
//! `resource_col` dimension rather than only a tenant one.

use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "event_broker_event")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub type_id: String,
    pub topic: String,
    pub tenant_id: Uuid,
    pub source: String,
    pub subject: String,
    pub subject_type: String,
    pub occurred_at: DateTime<Utc>,
    pub trace_parent: Option<String>,
    /// JSON-serialized event payload (`sea_orm`'s `Json` column type - not
    /// a plain `String` - so it round-trips through `serde_json::Value`
    /// directly).
    pub data: Json,
    pub partition: i32,
    pub sequence: i64,
    pub sequence_time: DateTime<Utc>,
    /// What this row counts against its partition's retention byte bound,
    /// stored rather than recomputed so a removal can subtract exactly what an
    /// insert added even if the sizing rule later changes.
    pub stored_bytes: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
