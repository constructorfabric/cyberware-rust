//! `OData` field → `SeaORM` column mappers for pricing collection GETs.

use bss_pricing_sdk::odata::{
    ApprovalFilterField, AuditFilterField, BundleFilterField, HistoryFilterField,
    MembershipFilterField, MigrationFilterField, OverlayFilterField, PlanPriceFilterField,
    WindowFilterField,
};
use toolkit_db::odata::sea_orm_filter::{FieldToColumn, LimitCfg, ODataFieldMapping};
use toolkit_odata::filter::{FilterField, FilterOp, ODataValue};

use crate::domain::approval::ApprovalState;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::migration::MigrationState;
use crate::domain::overlay::{OverlayLifecycle, ScopeClass};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};

use crate::infra::storage::RepoError;

use crate::infra::storage::entity::approval::{
    Column as ApprovalColumn, Entity as ApprovalEntity, Model as ApprovalModel,
};
use crate::infra::storage::entity::audit_log::{
    Column as AuditColumn, Entity as AuditEntity, Model as AuditModel,
};
use crate::infra::storage::entity::bundle::{
    Column as BundleColumn, Entity as BundleEntity, Model as BundleModel,
};
use crate::infra::storage::entity::group_membership::{
    Column as MembershipColumn, Entity as MembershipEntity, Model as MembershipModel,
};
use crate::infra::storage::entity::migration::{
    Column as MigrationColumn, Entity as MigrationEntity, Model as MigrationModel,
};
use crate::infra::storage::entity::price::{
    Column as PriceColumn, Entity as PriceEntity, Model as PriceModel,
};
use crate::infra::storage::entity::price_overlay::{
    Column as OverlayColumn, Entity as OverlayEntity, Model as OverlayModel,
};
use crate::infra::storage::entity::price_window::{
    Column as WindowColumn, Entity as WindowEntity, Model as WindowModel,
};

/// One rule for every `lifecycle_state` filter over plan-vocabulary rows: the
/// token must name a declared [`LifecycleState`], or the request is a 400.
pub(super) fn lifecycle_token(value: &ODataValue) -> Result<ODataValue, String> {
    match value {
        ODataValue::String(token) => {
            if !LifecycleState::ALL.iter().any(|s| s.as_str() == token) {
                return Err(format!("unknown lifecycle state `{token}`"));
            }
            Ok(value.clone())
        }
        _ => Err("lifecycle state must be a string token".to_owned()),
    }
}

fn datetime_utc(instant: time::OffsetDateTime) -> sea_orm::Value {
    sea_orm::Value::TimeDateTimeWithTimeZone(Some(instant))
}

/// D-125 page size: default 100, hard cap 1 000.
pub const LIST_LIMIT_CFG: LimitCfg = LimitCfg {
    default: 100,
    max: 1_000,
};

/// Whether `$filter` names `field` (any operator). Used for authoring defaults.
pub fn filter_mentions_field<F: FilterField>(
    filter: Option<&toolkit_odata::ast::Expr>,
    field: F,
) -> bool {
    filter.is_some_and(|expr| mentions(expr, field.name()))
}

fn mentions(expr: &toolkit_odata::ast::Expr, field: &str) -> bool {
    use toolkit_odata::ast::Expr;
    match expr {
        Expr::And(a, b) | Expr::Or(a, b) => mentions(a, field) || mentions(b, field),
        Expr::Not(inner) => mentions(inner, field),
        Expr::Compare(left, _, right) => mentions(left, field) || mentions(right, field),
        Expr::In(inner, values) => {
            mentions(inner, field) || values.iter().any(|value| mentions(value, field))
        }
        Expr::Function(_, args) => args.iter().any(|arg| mentions(arg, field)),
        Expr::Identifier(name) => name == field,
        Expr::Value(_) => false,
    }
}

/// Inject the list's default keyset order, or reconstruct it from `cursor.s`.
///
/// The extractor clears `$orderby` when a cursor is present (the pair is 400).
/// Page 2 therefore arrives with an empty `order` and must take the sort the
/// token already carries — the same derivation `paginate_odata` uses.
pub fn query_with_default_order<F: FilterField>(query: &ODataQuery, fields: &[F]) -> ODataQuery {
    let out = query.clone();
    if !out.order.is_empty() {
        return out;
    }
    if let Some(cursor) = &out.cursor {
        if let Ok(from_cursor) = ODataOrderBy::from_signed_tokens(&cursor.s) {
            return out.with_order(from_cursor);
        }
        return out;
    }
    out.with_order(ODataOrderBy(
        fields
            .iter()
            .map(|field| OrderKey {
                field: field.name().to_owned(),
                dir: SortDir::Asc,
            })
            .collect(),
    ))
}

/// Append the key halves a walk needs to land on a unique row.
///
/// `paginate_odata` appends exactly one tiebreaker, which is enough only where
/// that tiebreaker is the table's whole primary key. On a composite key it is
/// not: a caller's `$orderby` leaves the effective order `[field, tiebreaker]`,
/// and two rows sharing that pair collide in the keyset predicate, so one of
/// them is dropped at the page boundary and never appears on any page.
///
/// The extractor refuses `$orderby` beside a cursor, so page 2 rebuilds its
/// order from `cursor.s` — which already carries this suffix, and
/// [`ODataOrderBy::ensure_tiebreaker`] skips a field already present, so
/// applying this after [`query_with_default_order`] is idempotent.
pub fn query_with_unique_order<F: FilterField>(query: &ODataQuery, suffix: &[F]) -> ODataQuery {
    let mut order = query.order.clone();
    for field in suffix {
        order = order.ensure_tiebreaker(field.name(), SortDir::Asc);
    }
    query.clone().with_order(order)
}

/// `paginate_odata` failure split: client `$filter` / cursor vs storage.
#[derive(Debug, thiserror::Error)]
pub enum OdataPageError {
    /// Storage / connection failure whose only remaining shape is text.
    #[error("pricing list db error: {0}")]
    Db(String),
    /// A [`RepoError`] met on the list path, carried whole so `repo_failure`
    /// classifies it — a `CorruptRow` raises the corruption alarm here as it
    /// does on every other read, instead of flattening to "database error".
    #[error("pricing list repo error: {0}")]
    Repo(#[from] RepoError),
    /// Malformed `$filter` / `$orderby` / cursor.
    #[error("pricing list odata error: {0}")]
    Odata(#[from] toolkit_odata::Error),
}

/// Map a `paginate_odata` failure into [`OdataPageError`].
#[must_use]
pub fn map_odata_err(err: toolkit_odata::Error) -> OdataPageError {
    match err {
        toolkit_odata::Error::Db(d) => OdataPageError::Db(d),
        other => OdataPageError::Odata(other),
    }
}

/// Project domain-mapped rows onto a page, keeping a decode failure's class.
pub fn domain_page<T, M>(
    page: Page<M>,
    map: impl Fn(M) -> Result<T, RepoError>,
) -> Result<Page<T>, OdataPageError> {
    let items = page
        .items
        .into_iter()
        .map(map)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page {
        items,
        page_info: page.page_info,
    })
}

pub struct PlanPriceODataMapper;

impl FieldToColumn<PlanPriceFilterField> for PlanPriceODataMapper {
    type Column = PriceColumn;

    fn map_field(field: PlanPriceFilterField) -> PriceColumn {
        match field {
            PlanPriceFilterField::PriceId => PriceColumn::PriceId,
            PlanPriceFilterField::LifecycleState => PriceColumn::LifecycleState,
            PlanPriceFilterField::CreatedAtUtc => PriceColumn::CreatedAtUtc,
        }
    }

    // A price row's `lifecycle_state` is the plan vocabulary; an unknown token
    // is a 400, as on the plans list, not an empty page.
    fn map_value(
        field: PlanPriceFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            PlanPriceFilterField::LifecycleState => lifecycle_token(value),
            // Listed rather than `_`: `map_field` and `extract_cursor_value` in this
            // same impl match exhaustively, so a field added to the SDK enum is a
            // compile error there and used to be a silent pass-through here — the
            // one of the three whose job is to refuse a token the column cannot
            // hold. A new field defaulting to "no validation" answers an empty
            // page where the retired named keys answered 400.
            PlanPriceFilterField::PriceId | PlanPriceFilterField::CreatedAtUtc => Ok(value.clone()),
        }
    }
}

impl ODataFieldMapping<PlanPriceFilterField> for PlanPriceODataMapper {
    type Entity = PriceEntity;

    fn extract_cursor_value(model: &PriceModel, field: PlanPriceFilterField) -> sea_orm::Value {
        match field {
            PlanPriceFilterField::PriceId => sea_orm::Value::Uuid(Some(model.price_id)),
            PlanPriceFilterField::LifecycleState => {
                sea_orm::Value::String(Some(model.lifecycle_state.clone()))
            }
            PlanPriceFilterField::CreatedAtUtc => datetime_utc(model.created_at_utc),
        }
    }
}

pub struct OverlayODataMapper;

impl FieldToColumn<OverlayFilterField> for OverlayODataMapper {
    type Column = OverlayColumn;

    fn map_field(field: OverlayFilterField) -> OverlayColumn {
        match field {
            OverlayFilterField::PriceOverlayId => OverlayColumn::PriceOverlayId,
            OverlayFilterField::ScopeClass => OverlayColumn::ScopeClass,
            OverlayFilterField::LifecycleState => OverlayColumn::LifecycleState,
            OverlayFilterField::Precedence => OverlayColumn::Precedence,
            OverlayFilterField::Revision => OverlayColumn::Revision,
        }
    }

    // The retired `?scope_class=` parsed its token through `ScopeClass::parse`
    // and answered 400 on an unknown one; the overlay has its own lifecycle
    // vocabulary. Both keep that diagnostic on `$filter`.
    fn map_value(
        field: OverlayFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            OverlayFilterField::ScopeClass => match value {
                ODataValue::String(token) => {
                    if ScopeClass::parse(token).is_none() {
                        return Err(format!("unknown scope class `{token}`"));
                    }
                    Ok(value.clone())
                }
                _ => Err("scope class must be a string token".to_owned()),
            },
            OverlayFilterField::LifecycleState => match value {
                ODataValue::String(token) => {
                    if OverlayLifecycle::parse(token).is_none() {
                        return Err(format!("unknown overlay lifecycle state `{token}`"));
                    }
                    Ok(value.clone())
                }
                _ => Err("overlay lifecycle state must be a string token".to_owned()),
            },
            // Listed rather than `_`: `map_field` and `extract_cursor_value` in this
            // same impl match exhaustively, so a field added to the SDK enum is a
            // compile error there and used to be a silent pass-through here — the
            // one of the three whose job is to refuse a token the column cannot
            // hold. A new field defaulting to "no validation" answers an empty
            // page where the retired named keys answered 400.
            OverlayFilterField::PriceOverlayId
            | OverlayFilterField::Precedence
            | OverlayFilterField::Revision => Ok(value.clone()),
        }
    }
}

impl ODataFieldMapping<OverlayFilterField> for OverlayODataMapper {
    type Entity = OverlayEntity;

    fn extract_cursor_value(model: &OverlayModel, field: OverlayFilterField) -> sea_orm::Value {
        match field {
            OverlayFilterField::PriceOverlayId => {
                sea_orm::Value::Uuid(Some(model.price_overlay_id))
            }
            OverlayFilterField::ScopeClass => {
                sea_orm::Value::String(Some(model.scope_class.clone()))
            }
            OverlayFilterField::LifecycleState => {
                sea_orm::Value::String(Some(model.lifecycle_state.clone()))
            }
            OverlayFilterField::Precedence => sea_orm::Value::Int(Some(model.precedence)),
            OverlayFilterField::Revision => sea_orm::Value::BigInt(Some(model.revision)),
        }
    }
}

pub struct WindowODataMapper;

impl FieldToColumn<WindowFilterField> for WindowODataMapper {
    type Column = WindowColumn;

    fn map_field(field: WindowFilterField) -> WindowColumn {
        match field {
            WindowFilterField::PriceId => WindowColumn::PriceId,
            WindowFilterField::WindowId => WindowColumn::WindowId,
        }
    }
}

impl ODataFieldMapping<WindowFilterField> for WindowODataMapper {
    type Entity = WindowEntity;

    fn extract_cursor_value(model: &WindowModel, field: WindowFilterField) -> sea_orm::Value {
        match field {
            WindowFilterField::PriceId => sea_orm::Value::Uuid(Some(model.price_id)),
            WindowFilterField::WindowId => sea_orm::Value::Uuid(Some(model.window_id)),
        }
    }
}

pub struct MembershipODataMapper;

impl FieldToColumn<MembershipFilterField> for MembershipODataMapper {
    type Column = MembershipColumn;

    fn map_field(field: MembershipFilterField) -> MembershipColumn {
        match field {
            MembershipFilterField::PayerTenantId => MembershipColumn::PayerTenantId,
            MembershipFilterField::MembershipId => MembershipColumn::MembershipId,
            MembershipFilterField::EffectiveFrom => MembershipColumn::EffectiveFrom,
        }
    }
}

impl ODataFieldMapping<MembershipFilterField> for MembershipODataMapper {
    type Entity = MembershipEntity;

    fn extract_cursor_value(
        model: &MembershipModel,
        field: MembershipFilterField,
    ) -> sea_orm::Value {
        match field {
            MembershipFilterField::PayerTenantId => {
                sea_orm::Value::Uuid(Some(model.payer_tenant_id))
            }
            MembershipFilterField::MembershipId => sea_orm::Value::Uuid(Some(model.membership_id)),
            MembershipFilterField::EffectiveFrom => datetime_utc(model.effective_from),
        }
    }
}

pub struct ApprovalODataMapper;

impl FieldToColumn<ApprovalFilterField> for ApprovalODataMapper {
    type Column = ApprovalColumn;

    fn map_field(field: ApprovalFilterField) -> ApprovalColumn {
        match field {
            ApprovalFilterField::State => ApprovalColumn::State,
            ApprovalFilterField::ApprovalId => ApprovalColumn::ApprovalId,
        }
    }

    fn map_value(
        field: ApprovalFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            ApprovalFilterField::State => match value {
                ODataValue::String(token) => {
                    if ApprovalState::from_token(token).is_none() {
                        return Err(format!("unknown approval state `{token}`"));
                    }
                    Ok(value.clone())
                }
                _ => Err("approval state must be a string token".to_owned()),
            },
            ApprovalFilterField::ApprovalId => Ok(value.clone()),
        }
    }
}

impl ODataFieldMapping<ApprovalFilterField> for ApprovalODataMapper {
    type Entity = ApprovalEntity;

    fn extract_cursor_value(model: &ApprovalModel, field: ApprovalFilterField) -> sea_orm::Value {
        match field {
            ApprovalFilterField::State => sea_orm::Value::String(Some(model.state.clone())),
            ApprovalFilterField::ApprovalId => sea_orm::Value::Uuid(Some(model.approval_id)),
        }
    }
}

pub struct MigrationODataMapper;

impl FieldToColumn<MigrationFilterField> for MigrationODataMapper {
    type Column = MigrationColumn;

    fn map_field(field: MigrationFilterField) -> MigrationColumn {
        match field {
            MigrationFilterField::State => MigrationColumn::State,
            MigrationFilterField::MigrationId => MigrationColumn::MigrationId,
        }
    }

    // The retired `?state=` validated its token and answered 400 on an unknown
    // one. Without this arm `$filter=state eq 'bogus'` is a 200 with an empty
    // page, which reads to a caller as "no migrations in that state" rather
    // than "there is no such state" — `ApprovalODataMapper`'s reason, same shape.
    fn map_value(
        field: MigrationFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            MigrationFilterField::State => match value {
                ODataValue::String(token) => {
                    if MigrationState::parse(token).is_none() {
                        return Err(format!("unknown migration state `{token}`"));
                    }
                    Ok(value.clone())
                }
                _ => Err("migration state must be a string token".to_owned()),
            },
            MigrationFilterField::MigrationId => Ok(value.clone()),
        }
    }
}

impl ODataFieldMapping<MigrationFilterField> for MigrationODataMapper {
    type Entity = MigrationEntity;

    fn extract_cursor_value(model: &MigrationModel, field: MigrationFilterField) -> sea_orm::Value {
        match field {
            MigrationFilterField::State => sea_orm::Value::String(Some(model.state.clone())),
            MigrationFilterField::MigrationId => sea_orm::Value::Uuid(Some(model.migration_id)),
        }
    }
}

pub struct BundleODataMapper;

impl FieldToColumn<BundleFilterField> for BundleODataMapper {
    type Column = BundleColumn;

    fn map_field(field: BundleFilterField) -> BundleColumn {
        match field {
            BundleFilterField::PlanId => BundleColumn::PlanId,
            BundleFilterField::BundleId => BundleColumn::BundleId,
        }
    }
}

impl ODataFieldMapping<BundleFilterField> for BundleODataMapper {
    type Entity = BundleEntity;

    fn extract_cursor_value(model: &BundleModel, field: BundleFilterField) -> sea_orm::Value {
        match field {
            BundleFilterField::PlanId => sea_orm::Value::Uuid(Some(model.plan_id)),
            BundleFilterField::BundleId => sea_orm::Value::Uuid(Some(model.bundle_id)),
        }
    }
}

pub struct HistoryODataMapper;

impl FieldToColumn<HistoryFilterField> for HistoryODataMapper {
    type Column = PriceColumn;

    fn map_field(field: HistoryFilterField) -> PriceColumn {
        match field {
            HistoryFilterField::PriceId => PriceColumn::PriceId,
            HistoryFilterField::PlanId => PriceColumn::PlanId,
            HistoryFilterField::LifecycleState => PriceColumn::LifecycleState,
            HistoryFilterField::AuthoredAt => PriceColumn::CreatedAtUtc,
            HistoryFilterField::Actor => PriceColumn::CreatedBy,
        }
    }

    // History walks the same price rows, so the same vocabulary and the same 400.
    fn map_value(
        field: HistoryFilterField,
        _op: FilterOp,
        value: &ODataValue,
    ) -> Result<ODataValue, String> {
        match field {
            HistoryFilterField::LifecycleState => lifecycle_token(value),
            // Listed rather than `_`: `map_field` and `extract_cursor_value` in this
            // same impl match exhaustively, so a field added to the SDK enum is a
            // compile error there and used to be a silent pass-through here — the
            // one of the three whose job is to refuse a token the column cannot
            // hold. A new field defaulting to "no validation" answers an empty
            // page where the retired named keys answered 400.
            HistoryFilterField::PriceId
            | HistoryFilterField::PlanId
            | HistoryFilterField::AuthoredAt
            | HistoryFilterField::Actor => Ok(value.clone()),
        }
    }
}

impl ODataFieldMapping<HistoryFilterField> for HistoryODataMapper {
    type Entity = PriceEntity;

    fn extract_cursor_value(model: &PriceModel, field: HistoryFilterField) -> sea_orm::Value {
        match field {
            HistoryFilterField::PriceId => sea_orm::Value::Uuid(Some(model.price_id)),
            HistoryFilterField::PlanId => sea_orm::Value::Uuid(Some(model.plan_id)),
            HistoryFilterField::LifecycleState => {
                sea_orm::Value::String(Some(model.lifecycle_state.clone()))
            }
            HistoryFilterField::AuthoredAt => datetime_utc(model.created_at_utc),
            HistoryFilterField::Actor => sea_orm::Value::Uuid(Some(model.created_by)),
        }
    }
}

pub struct AuditODataMapper;

impl FieldToColumn<AuditFilterField> for AuditODataMapper {
    type Column = AuditColumn;

    fn map_field(field: AuditFilterField) -> AuditColumn {
        match field {
            AuditFilterField::ChainId => AuditColumn::ChainId,
            AuditFilterField::Seq => AuditColumn::Seq,
            AuditFilterField::EntryKind => AuditColumn::EntryKind,
            AuditFilterField::RecordedAt => AuditColumn::RecordedAt,
            AuditFilterField::ActorPrincipalId => AuditColumn::ActorPrincipalId,
            AuditFilterField::Action => AuditColumn::Action,
            AuditFilterField::SubjectKind => AuditColumn::SubjectKind,
        }
    }

    // `pricing_audit_log` is INSERT-only and multi-year, and it carries three
    // indexes: the `(tenant_id, chain_id, seq)` key, `(tenant_id, recorded_at)`
    // and `(tenant_id, subject_kind, subject_ref, recorded_at)`. Ordering on any
    // other field has no index to seek, so each page of the keyset walk would
    // scan and sort the tenant's whole history. Only the index-backed keys are
    // admitted; `chain_id` and `seq` must stay orderable because the walk's own
    // unique suffix goes through this same gate.
    fn is_orderable(field: AuditFilterField) -> bool {
        matches!(
            field,
            AuditFilterField::RecordedAt
                | AuditFilterField::ChainId
                | AuditFilterField::Seq
                | AuditFilterField::SubjectKind
        )
    }
}

impl ODataFieldMapping<AuditFilterField> for AuditODataMapper {
    type Entity = AuditEntity;

    fn extract_cursor_value(model: &AuditModel, field: AuditFilterField) -> sea_orm::Value {
        match field {
            AuditFilterField::ChainId => sea_orm::Value::Uuid(Some(model.chain_id)),
            AuditFilterField::Seq => sea_orm::Value::BigInt(Some(model.seq)),
            AuditFilterField::EntryKind => sea_orm::Value::String(Some(model.entry_kind.clone())),
            AuditFilterField::RecordedAt => datetime_utc(model.recorded_at),
            AuditFilterField::ActorPrincipalId => {
                sea_orm::Value::Uuid(Some(model.actor_principal_id))
            }
            AuditFilterField::Action => sea_orm::Value::String(Some(model.action.clone())),
            AuditFilterField::SubjectKind => {
                sea_orm::Value::String(Some(model.subject_kind.clone()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bss_pricing_sdk::odata::PlanFilterField;
    use toolkit_odata::parse_filter_string;
    use toolkit_odata::{CursorV1, ODataQuery, SortDir};

    use super::*;

    fn expr(raw: &str) -> toolkit_odata::ast::Expr {
        parse_filter_string(raw)
            .expect("filter must parse")
            .as_expr()
            .clone()
    }

    #[test]
    fn filter_mentions_lifecycle_state_eq_in_and_not() {
        assert!(!filter_mentions_field(
            None,
            PlanFilterField::LifecycleState
        ));
        assert!(filter_mentions_field(
            Some(&expr("lifecycle_state eq 'draft'")),
            PlanFilterField::LifecycleState
        ));
        assert!(filter_mentions_field(
            Some(&expr("lifecycle_state in ('draft','published')")),
            PlanFilterField::LifecycleState
        ));
        assert!(filter_mentions_field(
            Some(&expr("not lifecycle_state eq 'draft'")),
            PlanFilterField::LifecycleState
        ));
        assert!(!filter_mentions_field(
            Some(&expr("plan_id eq 11111111-1111-1111-1111-111111111111")),
            PlanFilterField::LifecycleState
        ));
    }

    #[test]
    fn default_order_injects_when_empty_and_reconstructs_from_cursor() {
        let injected = query_with_default_order(&ODataQuery::default(), &[PlanFilterField::PlanId]);
        assert_eq!(injected.order.0.len(), 1);
        assert_eq!(injected.order.0[0].field, "plan_id");

        let already = ODataQuery::default().with_order(ODataOrderBy(vec![OrderKey {
            field: "sku_id".to_owned(),
            dir: SortDir::Desc,
        }]));
        let kept = query_with_default_order(&already, &[PlanFilterField::PlanId]);
        assert_eq!(kept.order.0[0].field, "sku_id");

        // `-plan_id` is a sort the default injection cannot produce — it always
        // builds `Asc` — so this stanza fails if the cursor branch stops running
        // and the default is injected instead. Asserted `Asc` before, which both
        // branches yield, so nothing here was under test.
        let with_cursor = ODataQuery::default().with_cursor(CursorV1 {
            k: vec!["11111111-1111-1111-1111-111111111111".to_owned()],
            o: SortDir::Desc,
            s: "-plan_id".to_owned(),
            f: None,
            d: "fwd".to_owned(),
        });
        let after_cursor = query_with_default_order(&with_cursor, &[PlanFilterField::PlanId]);
        assert_eq!(after_cursor.order.0.len(), 1);
        assert_eq!(after_cursor.order.0[0].field, "plan_id");
        assert_eq!(
            after_cursor.order.0[0].dir,
            SortDir::Desc,
            "page 2 must take the direction the token carries, not the default"
        );
    }

    // The audit walk admits only the keys its three indexes can seek. Asserted
    // as the whole set rather than field by field, so a variant added to
    // `AuditListQuery` later is admitted deliberately or not at all.
    #[test]
    fn audit_orders_only_on_its_indexed_keys() {
        for field in [
            AuditFilterField::RecordedAt,
            AuditFilterField::ChainId,
            AuditFilterField::Seq,
            AuditFilterField::SubjectKind,
        ] {
            assert!(
                AuditODataMapper::is_orderable(field),
                "an index-backed key must stay orderable: {field:?}"
            );
        }
        for field in [
            AuditFilterField::EntryKind,
            AuditFilterField::Action,
            AuditFilterField::ActorPrincipalId,
        ] {
            assert!(
                !AuditODataMapper::is_orderable(field),
                "ordering on an unindexed audit column scans the tenant's whole history: {field:?}"
            );
        }
    }

    // `seq` counts within a chain segment, so the walk must carry `chain_id`
    // too. The caller's own key stays first — appending must not reorder what
    // was asked for — and a second application changes nothing, which is what
    // lets page 2 re-apply it over an order rebuilt from `cursor.s`.
    #[test]
    fn unique_order_appends_the_missing_key_and_keeps_the_callers_first() {
        let asked = ODataQuery::default().with_order(ODataOrderBy(vec![OrderKey {
            field: "recorded_at".to_owned(),
            dir: SortDir::Desc,
        }]));

        let once = query_with_unique_order(&asked, &[AuditFilterField::ChainId]);
        let fields: Vec<&str> = once.order.0.iter().map(|k| k.field.as_str()).collect();
        assert_eq!(
            fields,
            vec!["recorded_at", "chain_id"],
            "the caller's key leads and the unique half follows"
        );
        assert_eq!(
            once.order.0[0].dir,
            SortDir::Desc,
            "appending must not rewrite the direction the caller asked for"
        );

        let twice = query_with_unique_order(&once, &[AuditFilterField::ChainId]);
        let again: Vec<&str> = twice.order.0.iter().map(|k| k.field.as_str()).collect();
        assert_eq!(again, fields, "a second application is a no-op");
    }

    // An unknown enum token is a diagnosable 400, not an empty page. The
    // retired named filters validated theirs; `$filter` has to as well, and on
    // the plans list a bad token would additionally suppress the authoring
    // default via `filter_mentions_field` and widen the walk.
    #[test]
    fn an_unknown_enum_token_is_refused_by_the_filter() {
        let bogus = ODataValue::String("bogus".to_owned());

        assert!(
            MigrationODataMapper::map_value(MigrationFilterField::State, FilterOp::Eq, &bogus)
                .is_err(),
            "an unknown migration state must not read as an empty page"
        );
        assert!(
            lifecycle_token(&bogus).is_err(),
            "an unknown lifecycle state must not read as an empty page"
        );

        for (what, err) in [
            (
                "scope class",
                OverlayODataMapper::map_value(OverlayFilterField::ScopeClass, FilterOp::Eq, &bogus),
            ),
            (
                "overlay lifecycle state",
                OverlayODataMapper::map_value(
                    OverlayFilterField::LifecycleState,
                    FilterOp::Eq,
                    &bogus,
                ),
            ),
            (
                "price lifecycle state",
                PlanPriceODataMapper::map_value(
                    PlanPriceFilterField::LifecycleState,
                    FilterOp::Eq,
                    &bogus,
                ),
            ),
            (
                "history lifecycle state",
                HistoryODataMapper::map_value(
                    HistoryFilterField::LifecycleState,
                    FilterOp::Eq,
                    &bogus,
                ),
            ),
            (
                "approval state",
                ApprovalODataMapper::map_value(ApprovalFilterField::State, FilterOp::Eq, &bogus),
            ),
        ] {
            assert!(
                err.is_err(),
                "an unknown {what} must not read as an empty page"
            );
        }
        for class in ScopeClass::ALL {
            let good = ODataValue::String(class.as_str().to_owned());
            assert!(
                OverlayODataMapper::map_value(OverlayFilterField::ScopeClass, FilterOp::Eq, &good)
                    .is_ok(),
                "a declared scope class must stay filterable: {class:?}"
            );
        }
        for state in OverlayLifecycle::ALL {
            let good = ODataValue::String(state.as_str().to_owned());
            assert!(
                OverlayODataMapper::map_value(
                    OverlayFilterField::LifecycleState,
                    FilterOp::Eq,
                    &good
                )
                .is_ok(),
                "a declared overlay lifecycle state must stay filterable: {state:?}"
            );
        }

        // A real token still passes, so the guard narrows nothing it should not.
        for token in MigrationState::ALL {
            let good = ODataValue::String(token.as_str().to_owned());
            assert!(
                MigrationODataMapper::map_value(MigrationFilterField::State, FilterOp::Eq, &good)
                    .is_ok(),
                "a declared migration state must stay filterable: {token}"
            );
        }
        for state in LifecycleState::ALL {
            let good = ODataValue::String(state.as_str().to_owned());
            assert!(
                lifecycle_token(&good).is_ok(),
                "a declared lifecycle state must stay filterable: {state}"
            );
        }
    }

    /// The `approvals_tests` this branch retired ranged the state filter over
    /// `ApprovalState::ALL`; the mapper is where that filter lives now, so the
    /// range moves here — a state added later arrives rather than becoming
    /// silently unfilterable.
    #[test]
    fn every_approval_state_is_reachable_through_the_filter() {
        for state in ApprovalState::ALL {
            let token = ODataValue::String(state.as_str().to_owned());
            let mapped =
                ApprovalODataMapper::map_value(ApprovalFilterField::State, FilterOp::Eq, &token)
                    .unwrap_or_else(|e| panic!("`{}` must pass the filter: {e}", state.as_str()));
            // The value is named, not printed: `ODataValue` can carry a `Uuid`
            // and CodeQL reads a `{:?}` of it as cleartext logging.
            assert!(
                matches!(&mapped, ODataValue::String(s) if s == state.as_str()),
                "`{}` must be bound as written, not re-spelt",
                state.as_str()
            );
        }
        assert!(
            ApprovalODataMapper::map_value(
                ApprovalFilterField::State,
                FilterOp::Eq,
                &ODataValue::Null
            )
            .is_err(),
            "a non-string state is refused, not coerced"
        );
    }
}
