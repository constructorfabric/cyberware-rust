//! `SeaORM` entity for `bss.chain_checkpoint` (per-tenant retention
//! checkpoint). One row records a contiguous range of the tamper-evidence hash
//! chain (`from_row_hash` .. `to_row_hash`) plus the number of journal entries
//! it covers, so a future partition-rotation pass can prove a detached
//! partition is anchored by a checkpoint before it retires the rows.
//!
//! Dormant seam (Slice 6 §4.8): partitioning / rotation is Foundation debt, so
//! nothing writes a checkpoint on a schedule yet. `signature` is `Option` —
//! signing / WORM is post-MVP (Bucket A); an MVP checkpoint is unsigned.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "chain_checkpoint")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub checkpoint_id: Uuid,
    pub tenant_id: Uuid,
    pub from_row_hash: Vec<u8>,
    pub to_row_hash: Vec<u8>,
    pub covered_entry_count: i64,
    pub signature: Option<Vec<u8>>,
    pub created_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
