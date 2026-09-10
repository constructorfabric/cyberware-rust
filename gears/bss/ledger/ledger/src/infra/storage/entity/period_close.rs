//! `SeaORM` entity for `bss.ledger_period_close` (the close-process owner:
//! status lifecycle OPEN→CLOSING→CLOSED→REOPENED, the last gate result in
//! `blocked_reasons`, the CLOSING recompute `recon_watermark`, and the REOPEN
//! audit linkage, keyed by `(tenant_id, legal_entity_id, period_id)` — Slice 7).
//! Tenant-scoped via `SecureORM`; the resource col is the business `period_id`.

use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_period_close")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "period_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub legal_entity_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub period_id: String,
    pub status: String,
    pub initiated_by: String,
    pub blocked_reasons: Option<JsonValue>,
    pub recon_watermark: Option<i64>,
    pub reopen_approval_id: Option<Uuid>,
    pub reopened_by: Option<String>,
    pub closed_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
