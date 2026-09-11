//! Per-`(topic, partition)` bookkeeping: the next broker-logical sequence to
//! assign, the outbox-retry dedup's `last_chain_sequence`, and the partition's
//! event count and stored byte total.
//!
//! Backend-internal state, not tenant-scoped data - a topic can carry events
//! from many tenants - hence `#[secure(unrestricted)]`.

use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "event_broker_partition_state")]
#[secure(unrestricted)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub topic: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub partition: i32,
    pub next_sequence: i64,
    /// `NULL` until the first chained/monotonic-mode event lands in this
    /// `(topic, partition)` - stateless-mode events never update this,
    /// matching stateless mode's documented "no broker-side dedup".
    pub last_chain_sequence: Option<i64>,
    /// Events currently stored in this partition, maintained by counting rows
    /// as they land and as they are removed - never derived by subtracting one
    /// sequence number from another. Sequences are ordinals: after a prefix
    /// removal the distance between the lowest and highest surviving sequence
    /// is not the number of surviving events.
    pub event_count: i64,
    /// Bytes those events occupy, summed the same way, and what the retention
    /// size bound is measured against.
    pub stored_bytes: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
