//! `SeaORM` entity for `bss.ledger_dual_control_policy` — per-tenant,
//! append-only effective-dated dual-control threshold versions (§4.2): the D2
//! amount threshold, the A6 backdating window, and the pending-approval TTL. The
//! resolver picks the row in effect at decision time (latest `effective_from <=
//! now`, highest `version` on a tie); absent a row, the ratified platform
//! defaults apply. Mirrors the `tenant_precedence_policy` append-only shape.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_dual_control_policy")]
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
    /// D2 threshold in USD-equivalent minor units; validated `[10000 .. 100000000]`
    /// (100 .. 1,000,000 USD at scale 2) — out-of-range config is rejected.
    pub d2_threshold_minor: i64,
    /// A6 material-backdating window in business days; validated `[1 .. 30]`.
    pub a6_backdating_biz_days: i32,
    /// TTL applied to a fresh `PENDING`/`NEEDS_REWORK` record before it expires.
    pub pending_ttl_seconds: i64,
    pub created_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
