//! `SeaORM` entity for `bss.ledger_payment_allocation` (one row per
//! `(payment, invoice)` allocation split).

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_payment_allocation")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "allocation_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub allocation_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub invoice_id: String,
    pub payer_tenant_id: Uuid,
    pub payment_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub precedence_policy_ref: String,
    pub allocated_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
