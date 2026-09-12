//! `SeaORM` entity for `bss.scope_freeze` (per-tenant tamper-freeze switch).
//! One row per `(tenant_id, scope, period_id)` STOPS further posting into the
//! frozen scope after the integrity verifier finds a broken chain. A row is
//! ACTIVE while `cleared_at IS NULL`; `period_id` is `'ALL'` for a tenant-wide
//! freeze or a concrete period to freeze just that period.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "scope_freeze")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub period_id: String,
    pub reason: String,
    pub frozen_at: OffsetDateTime,
    pub set_by: String,
    pub cleared_by: Option<String>,
    pub cleared_at: Option<OffsetDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
