//! `SeaORM` entity for `bss.ledger_tenant_posting_policy` — per-tenant,
//! append-only effective-dated invoice-posting policy versions (VHP-1853): the
//! missing-mapping mode (`SUSPENSE` | `HARD_BLOCK`) and the AR-aging bucket
//! thresholds (CSV upper-bounds). The orchestrator / aging read picks the row in
//! effect at decision time (latest `effective_from <= now`, highest `version` on
//! a tie); absent a row, the gear's built-in defaults apply (`SUSPENSE` +
//! `30,60,90`). Mirrors the `tenant_precedence_policy` / `dual_control_policy`
//! append-only shape.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_tenant_posting_policy")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "tenant_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: i64,
    pub effective_from: OffsetDateTime,
    /// Missing-mapping policy: `SUSPENSE` (route an unmapped item to suspense —
    /// the default) or `HARD_BLOCK` (reject the post `ACCOUNT_MAPPING_MISSING`).
    /// DB CHECK-constrained to those two literals.
    pub missing_mapping_mode: String,
    /// AR-aging bucket upper-bounds — a CSV of strict-increasing positive day
    /// counts (e.g. `30,60,90`). Parsed + validated in the domain on read/write.
    pub ar_aging_thresholds: String,
    pub created_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
