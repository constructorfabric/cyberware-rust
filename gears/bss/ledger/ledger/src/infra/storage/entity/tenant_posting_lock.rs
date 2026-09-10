//! `SeaORM` entity for `bss.ledger_tenant_posting_lock` (per-tenant posting kill switch).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_tenant_posting_lock")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "tenant_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    pub locked: bool,
    pub reason_code: Option<String>,
    pub set_by: Option<Uuid>,
    pub set_at: Option<OffsetDateTime>,
    pub cleared_by: Option<Uuid>,
    pub cleared_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
