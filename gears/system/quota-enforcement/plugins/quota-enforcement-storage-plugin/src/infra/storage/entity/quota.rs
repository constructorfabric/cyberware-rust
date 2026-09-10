//! `qe_quotas`: one row per Quota (DESIGN section 3.7).
//!
//! Closed enums are stored as their full GTS instance ids; `status` as the
//! contract's `snake_case` names; the two JSON columns as canonical text.
//! `cap` is `NULL` for an unbounded cap and `0..=i64::MAX` otherwise.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "qe_quotas")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    /// Server-assigned `UUIDv7`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// PDP-authorized owning tenant.
    pub tenant_id: Uuid,
    /// Subject projection type id.
    pub projection_type: String,
    /// Opaque subject identifier.
    pub subject_id: String,
    /// Metric instance id.
    pub metric: String,
    /// `QuotaType` as a GTS instance id.
    pub quota_type: String,
    /// `PeriodType` as a GTS instance id; consumption Quotas only.
    #[sea_orm(nullable)]
    pub period: Option<String>,
    /// `EnforcementMode` as a GTS instance id.
    pub enforcement_mode: String,
    /// Cap, `NULL` when unbounded.
    #[sea_orm(nullable)]
    pub cap: Option<i64>,
    /// JSON array of percentages.
    pub notification_thresholds: String,
    /// Validity start, inclusive.
    #[sea_orm(nullable)]
    pub validity_start: Option<OffsetDateTime>,
    /// Validity end, inclusive.
    #[sea_orm(nullable)]
    pub validity_end: Option<OffsetDateTime>,
    /// Informational fail-open hint.
    pub fail_open_hint: bool,
    /// JSON object, contract-validated by the gear.
    pub metadata: String,
    /// `QuotaSource` as a GTS instance id.
    pub source: String,
    /// `active` or `deactivated`.
    pub status: String,
    /// Constraint contract type id snapshotted at creation.
    pub constraint_contract_type: String,
    /// Constraint contract version snapshotted at creation.
    pub constraint_contract_version: i32,
    /// Increments once per accepted mutation.
    pub record_version: i32,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last mutation time.
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
