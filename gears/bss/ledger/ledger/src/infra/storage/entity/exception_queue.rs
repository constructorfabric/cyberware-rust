//! `SeaORM` entity for `bss.ledger_exception_queue` (durable, close-blocking
//! exceptions, keyed by `(tenant_id, exception_id)` — Slice 7). The per-slice
//! exception stubs + the reconciliation framework open rows here; the close gate
//! blocks while any OPEN close-blocking row exists for the period.
//! `GL_WRITEOFF_VARIANCE` → `APPROVED_EXCEPTION` is the one non-blocking
//! disposition. Tenant-scoped via `SecureORM`; the resource col is the
//! synthetic `exception_id`.

use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_exception_queue")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "exception_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub exception_id: Uuid,
    pub exception_type: String,
    pub business_ref: String,
    pub status: String,
    pub period_id: Option<String>,
    pub detail: Option<JsonValue>,
    pub opened_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    pub resolved_by: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
