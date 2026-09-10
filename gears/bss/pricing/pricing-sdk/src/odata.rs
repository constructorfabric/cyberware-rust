//! `OData` filter-field schemas for the pricing collection GETs.
//!
//! Dummy structs: the user-facing request shape is `ODataQuery`. The derive
//! generates `{Name}FilterField`, re-exported under the list names.

use time::OffsetDateTime;
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct PlanListQuery {
    #[odata(filter(kind = "Uuid"))]
    pub plan_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub lifecycle_state: String,
    #[odata(filter(kind = "Uuid"))]
    pub sku_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub plan_tier: String,
    #[odata(filter(kind = "String"))]
    pub billing_cycle: String,
    #[odata(filter(kind = "String"))]
    pub plan_name: String,
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: OffsetDateTime,
    /// Query-only predicate over authoring price rows; response is `model_kinds`.
    #[odata(filter(kind = "String"))]
    pub model_kind: String,
    /// Query-only predicate over authoring price rows; response is `currencies`.
    #[odata(filter(kind = "String"))]
    pub currency: String,
    #[odata(filter(kind = "Bool"))]
    pub has_pending_approvals: bool,
}

/// Sortable fields of the canonical authoring projection, before pagination.
#[derive(ODataFilterable)]
#[allow(dead_code)]
struct PlanListOrder {
    #[odata(filter(kind = "Uuid"))]
    pub plan_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub plan_name: String,
    #[odata(filter(kind = "String"))]
    pub lifecycle_state: String,
    #[odata(filter(kind = "String"))]
    pub billing_cycle: String,
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: OffsetDateTime,
    #[odata(filter(kind = "I64"))]
    pub price_row_count: i64,
}

/// Sortable fields of the audit trail.
///
/// Narrower than [`AuditListQuery`] on purpose, and the narrowing is the
/// contract rather than a subset that happens to be smaller: `pricing_audit_log`
/// is INSERT-only and multi-year, and it carries three indexes — the
/// `(tenant_id, chain_id, seq)` key, `(tenant_id, recorded_at)` and
/// `(tenant_id, subject_kind, subject_ref, recorded_at)`. Ordering on
/// `entry_kind`, `actor_principal_id` or `action` has no index to seek, so
/// `AuditODataMapper::is_orderable` refuses them with a 400. Declaring
/// `$orderby` from the filter enum published those three in `docs/api/api.json`
/// anyway, which is a generated client offering a sort the server never accepts.
#[derive(ODataFilterable)]
#[allow(dead_code)]
struct AuditListOrder {
    #[odata(filter(kind = "Uuid"))]
    pub chain_id: Uuid,
    #[odata(filter(kind = "I64"))]
    pub seq: i64,
    #[odata(filter(kind = "DateTimeUtc"))]
    pub recorded_at: OffsetDateTime,
    #[odata(filter(kind = "String"))]
    pub subject_kind: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct PlanPriceListQuery {
    #[odata(filter(kind = "Uuid"))]
    pub price_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub lifecycle_state: String,
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at_utc: OffsetDateTime,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct OverlayListQuery {
    #[odata(filter(kind = "Uuid"))]
    pub price_overlay_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub scope_class: String,
    #[odata(filter(kind = "String"))]
    pub lifecycle_state: String,
    #[odata(filter(kind = "I64"))]
    pub precedence: i64,
    /// Composite-key tiebreaker so a seekset walk does not drop revisions of
    /// the same overlay at a page boundary.
    #[odata(filter(kind = "I64"))]
    pub revision: i64,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct WindowListQuery {
    #[odata(filter(kind = "Uuid"))]
    pub price_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub window_id: Uuid,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct MembershipListQuery {
    // Named for the column and the response field, not shortened: `MembershipView`
    // reads this value back as `payer_tenant_id` and the column is
    // `pricing_group_membership.payer_tenant_id`. A `payer_id` filter key made one
    // value answer to two names, so a caller could not filter on what it read.
    #[odata(filter(kind = "Uuid"))]
    pub payer_tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub membership_id: Uuid,
    #[odata(filter(kind = "DateTimeUtc"))]
    pub effective_from: OffsetDateTime,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct ApprovalListQuery {
    #[odata(filter(kind = "String"))]
    pub state: String,
    #[odata(filter(kind = "Uuid"))]
    pub approval_id: Uuid,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct MigrationListQuery {
    #[odata(filter(kind = "String"))]
    pub state: String,
    #[odata(filter(kind = "Uuid"))]
    pub migration_id: Uuid,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct BundleListQuery {
    #[odata(filter(kind = "Uuid"))]
    pub plan_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub bundle_id: Uuid,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct HistoryListQuery {
    #[odata(filter(kind = "Uuid"))]
    pub price_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub plan_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub lifecycle_state: String,
    #[odata(filter(kind = "DateTimeUtc"))]
    pub authored_at: OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub actor: Uuid,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct AuditListQuery {
    #[odata(filter(kind = "Uuid"))]
    pub chain_id: Uuid,
    #[odata(filter(kind = "I64"))]
    pub seq: i64,
    #[odata(filter(kind = "String"))]
    pub entry_kind: String,
    #[odata(filter(kind = "DateTimeUtc"))]
    pub recorded_at: OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub actor_principal_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub action: String,
    #[odata(filter(kind = "String"))]
    pub subject_kind: String,
}

pub use ApprovalListQueryFilterField as ApprovalFilterField;
pub use AuditListOrderFilterField as AuditOrderField;
pub use AuditListQueryFilterField as AuditFilterField;
pub use BundleListQueryFilterField as BundleFilterField;
pub use HistoryListQueryFilterField as HistoryFilterField;
pub use MembershipListQueryFilterField as MembershipFilterField;
pub use MigrationListQueryFilterField as MigrationFilterField;
pub use OverlayListQueryFilterField as OverlayFilterField;
pub use PlanListOrderFilterField as PlanOrderField;
pub use PlanListQueryFilterField as PlanFilterField;
pub use PlanPriceListQueryFilterField as PlanPriceFilterField;
pub use WindowListQueryFilterField as WindowFilterField;
