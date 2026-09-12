//! `SeaORM` entity for `bss.entry_annotation` (the typed controlled
//! non-financial annotation overlay). MUTABLE current-state: each row holds the
//! CURRENT `description` for one journal entry / line, upserted in place. The
//! append-only history of changes lives in the secured-audit chain
//! (`metadata-change` records) — this table carries no append-only trigger.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "entry_annotation")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub target_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub target_kind: String,
    pub target_period_id: String,
    pub description: Option<String>,
    pub actor_ref: String,
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
