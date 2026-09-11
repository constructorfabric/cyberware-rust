//! `SpecificationManager`'s local topic/event-type cache
//! (eb-single-process-implementation D1) - the startup bulk-load target and
//! the source of the stable integer surrogate id every `Storage` foreign key
//! into a topic/event-type resolves through. Global/platform data (`Topic`/
//! `EventType` have no tenant field in the domain model), so `unrestricted`.

use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "event_broker_spec_cache")]
#[secure(unrestricted)]
pub struct Model {
    /// The stable integer surrogate id (`SpecificationManager::
    /// resolve_topic_id`/`resolve_event_type_id`'s return value). Never
    /// renumbered once assigned - startup bulk-load upserts by `gts_id`,
    /// only ever inserting a fresh row for a never-before-seen id.
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// The full GTS instance id string (`Topic.id`/`EventType.id`).
    #[sea_orm(unique)]
    pub gts_id: String,
    /// Discriminator: `"topic"` or `"event_type"` (see [`SpecKind`]).
    pub kind: String,
    /// The `Topic`/`EventType` domain struct, serialized as JSON - avoids a
    /// wide, mostly-nullable column set for two structurally different
    /// entities sharing one cache table.
    pub payload: String,
}

/// `Model::kind`'s two valid values - a plain enum (not `#[domain_model]`;
/// this is an infra-layer storage discriminator, not a domain concept
/// exposed to callers of `SpecificationManager`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecKind {
    Topic,
    EventType,
}

impl SpecKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SpecKind::Topic => "topic",
            SpecKind::EventType => "event_type",
        }
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
