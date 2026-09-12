//! `SeaORM` entity for `bss.pricing_pin_frontier` — the materialized
//! pin-eligibility watermark, one row per tenant
//! (`design/01-foundation.md` §4.4, D-136).
//!
//! It is the **only** definition of "the newest pin-eligible `CatalogVersion`":
//! consumers pin its value, the <= 5s lag rule is measured against it, and
//! `pricing.readmodel.pin_eligibility_overdue` fires on its age. Nothing
//! recomputes the predicate at read time.


use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_pin_frontier")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "tenant_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// The newest pin-eligible version. Moves forward only.
    pub catalog_version: i64,
    /// UTC instant the frontier last advanced.
    pub advanced_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
