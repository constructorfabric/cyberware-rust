//! `SeaORM` entity for `bss.ledger_reconciliation_run` (one reconciliation
//! check execution + its variance result, keyed by `(tenant_id, run_id)` —
//! Slice 7). An out-of-tolerance run opens an `exception_queue` row and feeds
//! the period-close gate; `watermark` is the max in-period `created_seq` the run
//! covered. Tenant-scoped via `SecureORM`; the resource col is the business
//! `run_id`.

use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_reconciliation_run")]
#[secure(tenant_col = "tenant_id", resource_col = "run_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub run_id: Uuid,
    pub period_id: String,
    pub check_type: String,
    pub variance_minor: i64,
    pub within_tolerance: bool,
    pub status: String,
    pub watermark: Option<i64>,
    pub detail: Option<JsonValue>,
    pub at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
