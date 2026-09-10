//! `SeaORM` entity for `bss.ledger_debit_note` (the record linking a posted debit
//! note — an additional charge — to its originating posted invoice and its
//! recognized/deferred split, keyed by `(tenant_id, debit_note_id)`).
//! Tenant-scoped via `SecureORM`; the resource col is the business
//! `debit_note_id` (mirrors `recognition_schedule`'s `schedule_id`).
//!
//! `amount_minor` is incl-tax; `recognized_part_minor` + `deferred_part_minor`
//! are the ex-tax split parts — as with `credit_note`, they do NOT sum to
//! `amount_minor`, so there is deliberately no `recognized + deferred == amount`
//! CHECK. A debit note raises the invoice's headroom
//! (`invoice_exposure.debit_note_total_minor += amount`) under the lock order.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_debit_note")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "debit_note_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub debit_note_id: String,
    pub origin_invoice_id: String,
    pub currency: String,
    pub amount_minor: i64,
    pub recognized_part_minor: i64,
    pub deferred_part_minor: i64,
    pub created_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
