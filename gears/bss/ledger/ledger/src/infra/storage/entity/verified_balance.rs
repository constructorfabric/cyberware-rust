//! `SeaORM` entity for `bss.ledger_verified_balance` — the cumulative VERIFIED
//! tie-out baseline per grain through the last closed period (VHP-1843
//! incremental tie-out). One row per `(tenant_id, grain, grain_key)`; the daily
//! job and the `AR_DERIVED` recon check verify `baseline + fold(open periods) ==
//! cache` instead of folding all-time. Written in the period-close txn right
//! after the clean full tie-out passes. Tenant-scoped via `SecureORM`.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

/// Grain discriminators — mirror the derived caches the tie-out folds, and the
/// `chk_verified_balance_grain` CHECK. `ar_invoice` and `ar_invoice_disputed`
/// share the invoice cache row but verify two independent columns.
pub const GRAIN_ACCOUNT: &str = "account";
pub const GRAIN_AR_PAYER: &str = "ar_payer";
pub const GRAIN_AR_INVOICE: &str = "ar_invoice";
pub const GRAIN_AR_INVOICE_DISPUTED: &str = "ar_invoice_disputed";
pub const GRAIN_TAX: &str = "tax";
pub const GRAIN_UNALLOCATED: &str = "unallocated";
pub const GRAIN_REUSABLE_CREDIT: &str = "reusable_credit";

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "ledger_verified_balance")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "tenant_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// Cache discriminator — one of the `GRAIN_*` constants.
    #[sea_orm(primary_key, auto_increment = false)]
    pub grain: String,
    /// Canonical per-instance key the tie-out fold produces for this grain
    /// (e.g. `account_id|currency`), so the verify compares like-for-like.
    #[sea_orm(primary_key, auto_increment = false)]
    pub grain_key: String,
    /// Cumulative verified balance through `through_period`, in minor units.
    pub verified_balance_minor: i64,
    /// The last closed period this baseline is verified through.
    pub through_period: String,
    /// Max in-period `created_seq` covered by this baseline (the incremental
    /// boundary — the open fold starts strictly after the closed periods).
    pub watermark_seq: i64,
    pub updated_at_utc: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
