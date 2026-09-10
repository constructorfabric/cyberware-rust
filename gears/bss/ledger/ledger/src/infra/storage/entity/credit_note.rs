//! `SeaORM` entity for `bss.ledger_credit_note` (the record linking a posted
//! credit note to its originating posted invoice item, revenue stream, and the
//! recognized/deferred split basis, keyed by `(tenant_id, credit_note_id)`).
//! Tenant-scoped via `SecureORM`; the resource col is the business
//! `credit_note_id` (mirrors `recognition_schedule`'s `schedule_id`).
//!
//! `amount_minor` is incl-tax; `recognized_part_minor` + `deferred_part_minor`
//! are the ex-tax split parts recorded by the `RecognizedDeferredSplitter`
//! (Phase 1, Group B) — they do NOT sum to `amount_minor`, so there is
//! deliberately no `recognized + deferred == amount` CHECK. The headroom cap
//! lives on `invoice_exposure`; the deferred-portion schedule reduction
//! (`recognized_minor <= total_deferred_minor`) is guarded on
//! `recognition_schedule` (Slice 4), both written under the lock order.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_credit_note")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "credit_note_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub credit_note_id: String,
    pub origin_invoice_id: String,
    pub origin_invoice_item_ref: Option<String>,
    pub revenue_stream: String,
    pub currency: String,
    pub amount_minor: i64,
    pub recognized_part_minor: i64,
    pub deferred_part_minor: i64,
    pub split_basis_ref: Option<String>,
    pub reason_code: String,
    pub created_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
