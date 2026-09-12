//! `SeaORM` entity for `bss.ledger_ar_invoice_balance` (per-invoice AR cache).

use chrono::NaiveDate;
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_ar_invoice_balance")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "account_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub payer_tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub account_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub invoice_id: String,
    pub currency: String,
    pub balance_minor: i64,
    /// Disputed sub-portion of the open AR (`0 <= disputed_minor <= balance_minor`).
    /// `balance_minor` stays the FULL open AR through a dispute reclass
    /// (AR-class-neutral); this carries the disputed slice. The per-invoice
    /// `ar_status` flag is DERIVED: `DISPUTED` iff `disputed_minor == balance_minor`
    /// (with `balance_minor > 0`), else `ACTIVE`.
    pub disputed_minor: i64,
    pub functional_balance_minor: Option<i64>,
    pub functional_currency: Option<String>,
    pub original_posted_at: Option<OffsetDateTime>,
    pub due_date: Option<NaiveDate>,
    pub last_entry_seq: Option<i64>,
    pub version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
