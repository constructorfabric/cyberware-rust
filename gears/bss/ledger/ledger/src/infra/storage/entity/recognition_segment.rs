//! `SeaORM` entity for `bss.ledger_recognition_segment` (one time- or
//! milestone-slice of a [`recognition_schedule`](super::recognition_schedule) —
//! the **at-most-once unit**, one per `(schedule, period)`, keyed by
//! `(tenant_id, schedule_id, segment_no)`). Tenant-scoped via `SecureORM`; the
//! resource col is the parent `schedule_id` (a segment is owned by its
//! schedule).
//!
//! `segment_no` is immutable and 1:1 with `period_id`
//! (`UNIQUE (tenant_id, schedule_id, period_id)`), so the dedup grain and the
//! UNIQUE grain are provably identical (design §4.1 / §7). The
//! `RecognitionRunner` (Phase 2) stamps `status=DONE`, `recognized_at`, and
//! `run_id` in the same transaction as the `DR CL / CR Revenue` post; the
//! `status=DONE`/`run_id` + the period UNIQUE prevent a second credit.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_recognition_segment")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "schedule_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub schedule_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub segment_no: i32,
    pub period_id: String,
    pub amount_minor: i64,
    pub status: String,
    pub recognized_at: Option<OffsetDateTime>,
    pub run_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
