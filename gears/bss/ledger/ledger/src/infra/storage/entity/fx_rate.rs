//! `SeaORM` entity for `bss.ledger_fx_rate` (mutable "latest known" rate store).
//!
//! Reference data upserted by the `RateSyncJob` / ingest endpoint and read by
//! `RateSource` at lock time (the per-lock freeze lands in `fx_rate_snapshot`).
//! Exchange rates are not BOLA-sensitive object data, so there is no resource
//! axis — but every read/write from gear code must still flow through the
//! `SecureORM` runner (the `DBRunner`/`DBRunnerInternal` executor is sealed; a
//! gear cannot obtain a raw `&DatabaseConnection`). So this carries a
//! tenant-only `Scopable` (`no_resource`/`no_owner`/`no_type`, the same shape as
//! `chain_state`): the `tenant_id` predicate is applied at the SQL level via
//! `.secure().scope_with(&AccessScope::for_tenant(tenant))` rather than a manual
//! `.filter(tenant_id)`, which is the only mechanically available way to run a
//! query against the toolkit connection.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_fx_rate")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub base_currency: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub quote_currency: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub provider: String,
    pub rate_micro: i64,
    pub as_of: OffsetDateTime,
    pub fallback_order: i32,
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
