//! `SeaORM` entity for `bss.ledger_fx_rate_snapshot` (immutable per-lock FX
//! rate, frozen on a journal line via `journal_line.rate_snapshot_ref`).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_fx_rate_snapshot")]
#[secure(tenant_col = "tenant_id", resource_col = "rate_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub rate_id: Uuid,
    pub base_currency: String,
    pub quote_currency: String,
    pub rate_micro: i64,
    pub as_of: OffsetDateTime,
    pub provider: String,
    pub stale: bool,
    pub fallback_order: i32,
    pub triangulated_via: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
