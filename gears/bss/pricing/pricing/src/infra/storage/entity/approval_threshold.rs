//! `SeaORM` entity for `bss.pricing_approval_threshold` — one currency's
//! threshold entry in one version of a tenant's approval-threshold policy
//! (`design/05-governance.md` §6).
//!
//! Three columns are the key and every column is content, which is why the store
//! refuses both `DELETE` and `UPDATE`: a correction is a new `version`, because an
//! earlier version is what an approval's `content_hash` covers (D-10, and
//! `pricing_approval_threshold`'s migration doc has the argument).
//!
//! **There is no `state` column and that is deliberate.** Which version is the
//! tenant's policy is a fact about `pricing_approval` — the greatest version whose
//! unit an independent principal approved — and a column here would be a second
//! answer to it, free to disagree with the record that decided it.
//! [`crate::infra::threshold::effective_policy`] is the one reader of that fact.
//! (This line named `threshold_repo::effective_policy` until G6 wrote the function:
//! two docs spelled it two ways while neither module had it, and the repository was
//! the wrong of the two homes — it reads one store and the resolution reads both.)
//!
//! `percent_bp` is **basis points**, per `pricing_approval_threshold`'s recorded decision:
//! the design set declares no representation for §6's `percent > 0`, and basis
//! points is the set's own idiom (D-104's `share_bp`, `platform_cut_bp`). The unit
//! is in the column name so no reader has to infer it.


use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
use uuid::Uuid;
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "pricing_approval_threshold")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "tenant_id",
    no_owner,
    no_type
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// The policy version this entry belongs to. Append-only and monotone; the
    /// first version a tenant proposes is `0`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: i64,
    /// The ISO 4217 code this entry thresholds. A currency with **no** entry in
    /// the effective version is material — `inst-mat-percurrency`'s fail-safe
    /// half, which is the whole reason the store is keyed per currency.
    #[sea_orm(primary_key, auto_increment = false)]
    pub currency: String,
    /// The absolute threshold in this currency's minor units. Exactly one of this
    /// and [`Model::percent_bp`] is set, by CHECK.
    pub absolute_minor: Option<i64>,
    /// The relative threshold in **basis points** (`10_000` = 100%).
    pub percent_bp: Option<i32>,
    /// When this version takes effect, UTC.
    pub effective_from: OffsetDateTime,
    /// The pseudonymous principal that proposed this version. **Not** the
    /// approval trail — D-10 puts the second principal on `pricing_approval`.
    pub created_by: Uuid,
    pub created_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
