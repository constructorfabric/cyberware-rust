//! `SeaORM` entity for `bss.ledger_refund` (the record of a PSP refund's
//! two-stage lifecycle, keyed by the surrogate `(tenant_id, refund_id)`).
//! Tenant-scoped via `SecureORM`; the resource col is the business `refund_id`
//! (mirrors the sibling note tables' `credit_note_id`/`debit_note_id` — a
//! `varchar(128)`, NOT a `uuid` column).
//!
//! The idempotency grain is the natural `(tenant_id, psp_refund_id, phase)` (a
//! separate `UNIQUE` index, NOT the PK): one PSP refund advances through several
//! `phase` rows. Both patterns carry origin `payment_id`; `invoice_id` is NULL
//! for Pattern A (`A_UNALLOCATED`) and required for Pattern B (`B_RESTORE_AR`).
//! `reverses_entry_id` is set ONLY on a stage-1 line-negation (PSP reject/void);
//! `relates_to_refund_id` is the refund-of-refund forward link.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_refund")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "refund_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub refund_id: String,
    pub psp_refund_id: String,
    pub phase: String,
    pub pattern: String,
    pub payment_id: String,
    pub invoice_id: Option<String>,
    pub currency: String,
    pub amount_minor: i64,
    pub clearing_state: String,
    pub relates_to_refund_id: Option<String>,
    pub reverses_entry_id: Option<Uuid>,
    pub created_at_utc: OffsetDateTime,
    pub version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
