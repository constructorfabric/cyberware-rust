//! `SeaORM` entity for `bss.ledger_recognition_run` (an orchestration wrapper
//! that releases due `recognition_segment`s for a period, keyed by
//! `(tenant_id, period_id, run_id)` — `period_id` is folded into the key so a
//! client reusing one `run_id` across two periods runs BOTH;
//! the entity PK MUST mirror the migration's 3-column PK so a per-row write
//! cannot match the wrong period's run). The run is **not** itself the at-most-once dedup key
//! (that is the per-segment `(tenant, RECOGNITION, schedule_id:segment_no)`
//! gate); run-trigger dedup `(tenant_id, period_id, run_id)` + a per-`(tenant,
//! period_id)` single-active-run advisory lock live at the orchestration layer
//! (Phase 2, design §4.3). Tenant-scoped via `SecureORM`; the resource col is
//! the business `run_id`.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_recognition_run")]
#[secure(tenant_col = "tenant_id", resource_col = "run_id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    // PK column order mirrors the migration's `PRIMARY KEY (tenant_id, period_id,
    // run_id)`: `period_id` is part of the key, so a per-run
    // write must qualify by it (a bare `(tenant, run_id)` filter would touch the
    // other period's run row when a client reuses one `run_id` across periods).
    #[sea_orm(primary_key, auto_increment = false)]
    pub period_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub run_id: Uuid,
    pub started_at_utc: OffsetDateTime,
    pub status: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
