//! `SeaORM` entity for `bss.pricing_operator_flag` — an operator-plane
//! drift / divergence flag (`design/01-foundation.md` §3.7, D-85).
//!
//! Deliberately **not** part of the read model: a drift flag has no publish
//! unit, so writing it into a frozen `CatalogVersion` would be the in-place
//! mutation D-85 / D-99 forbid. Clearing a flag deletes the row.


use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_operator_flag")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "subject_ref",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_ref: String,
    /// `tier_divergent` | `grants_divergent` | `tax_readiness_divergent` |
    /// `meter_binding_divergent`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub flag: String,
    pub set_at: OffsetDateTime,
    pub set_by: Uuid,
    pub detail: JsonValue,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
