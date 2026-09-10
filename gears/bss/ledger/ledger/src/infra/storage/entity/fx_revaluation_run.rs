//! `SeaORM` entity for `bss.ledger_fx_revaluation_run` — the Mode-B
//! FX-revaluation completion marker (VHP-1859 review C3). One COMPLETE row per
//! `(tenant_id, period_id)`, written by the period-end revaluation job after a
//! clean `run_period`; the close gate requires it when Mode-B is enabled, so a
//! failed/lagged run BLOCKS close instead of leaving a forever-unpostable missing
//! `FX_REVALUATION`. Tenant-scoped via `SecureORM`.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

/// The only persisted status — the period-end run finished every scope cleanly.
pub const STATUS_COMPLETE: &str = "COMPLETE";
/// Forward-compat `scope` value the whole-period run records (the marker is keyed
/// per period; the column is retained for a future per-scope granularity).
pub const SCOPE_PERIOD: &str = "PERIOD";

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_fx_revaluation_run")]
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
    pub period_id: String,
    /// Forward-compat scope discriminator (`PERIOD` for the whole-period run).
    pub scope: String,
    /// Lifecycle status — only [`STATUS_COMPLETE`] is persisted.
    pub status: String,
    pub completed_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
