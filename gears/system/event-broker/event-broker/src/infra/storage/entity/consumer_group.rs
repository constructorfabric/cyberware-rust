//! `Storage`'s `consumer_group` namespace (eb-single-process-implementation
//! D2) - `ConsumerGroup`'s durable row. `tenant_id`/`owner_principal_id` are
//! captured from `SecurityContext` at create time and immutable
//! (`domain::model::ConsumerGroup`'s own doc comment), matching
//! `chat-engine/src/infra/db/entity/session.rs`'s `tenant_col`+`owner_col`
//! shape. No `resource_col`: `AccessScope::for_resources` is `Uuid`-typed and
//! this entity's natural identity is a GTS instance id (`String`), not a
//! `Uuid` - resource-level PDP constraints aren't used for this entity.

use chrono::{DateTime, Utc};
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "event_broker_consumer_group")]
#[secure(
    tenant_col = "tenant_id",
    owner_col = "owner_principal_id",
    no_resource,
    no_type
)]
pub struct Model {
    /// The GTS instance id (`ConsumerGroup.id`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// `"anonymous"` | `"named"` (`domain::model::ConsumerGroupKind`).
    pub kind: String,
    pub tenant_id: Uuid,
    pub owner_principal_id: Uuid,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
