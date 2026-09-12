//! `SeaORM` entity for `bss.ledger_approval` — the dual-control approval
//! request current-state, one row per governed mutation that crossed a policy
//! threshold (§4.1 of the dual-control impl-design spec). Keyed by `approval_id`;
//! tenant-scoped via `SecureORM` (`resource_col = approval_id`). The lifecycle is
//! `PENDING → APPROVED | REJECTED | NEEDS_REWORK | CANCELLED | EXPIRED`; `intent`
//! is the deterministic replay payload executed on `approve`.

use sea_orm::entity::prelude::*;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_approval")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "approval_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub approval_id: Uuid,
    pub tenant_id: Uuid,
    pub kind: String,
    pub state: String,
    pub revision: i32,
    pub business_key: String,
    pub intent: JsonValue,
    pub amount_usd_eq_minor: Option<i64>,
    pub threshold_snapshot: JsonValue,
    pub reason_code: String,
    pub prepared_by: Uuid,
    pub prepared_at: OffsetDateTime,
    pub approved_by: Option<Uuid>,
    pub decided_at: Option<OffsetDateTime>,
    pub correlation_id: Uuid,
    pub expires_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
