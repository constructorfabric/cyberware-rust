//! `OData` filter/order/select vocabulary and validation for the collection
//! read (`GET /credstore/v1/credentials`, ADR-0005/ADR-0004).
//!
//! [`CredentialFilterField`] is a hand-written [`FilterField`] implementation
//! rather than `#[derive(ODataFilterable)]`: the derive names a field after
//! the Rust field identifier's `to_string()`, and a field named the keyword
//! `type` can only be written as the raw identifier `r#type` — whose
//! `to_string()` is `"r#type"`, not `"type"`, so the derive cannot produce
//! the wire field name ADR-0004/0005 specify. Hand-writing it also gives
//! control the derive does not: `FieldKind::allows` publishes one operator
//! table per *kind*, not per field, but ADR-0005 restricts `reference` and
//! `type` to `eq`/`in` specifically even though their `String` kind would
//! otherwise admit `contains`/`startswith`/`endswith` — a prefix or
//! substring scan over references is exactly the enumeration primitive the
//! ADR withholds. That narrower allowlist is enforced here, after the
//! generic parser has already checked field names and kinds.

use time::OffsetDateTime;
use toolkit_odata::ODataOrderBy;
use toolkit_odata::filter::{FieldKind, FilterField, FilterNode, FilterOp, ODataValue};
use uuid::Uuid;

use credstore_sdk::{GtsId, SharingMode};

use crate::domain::error::DomainError;
use crate::domain::secret::model::Fallback;

/// Stable machine-readable reason codes this module's validation failures
/// carry. `INVALID_FILTER`/`INVALID_ORDERBY_FIELD`/`INVALID_SELECT` mirror
/// the platform's standard `OData` reason codes (`guidelines/DNA/REST/
/// PAGINATION.md`, DESIGN §10); `VALUE_MODE_NO_PAGINATION`/
/// `VALUE_MODE_NO_ORDER`/`VALUE_MODE_SELECTOR`/`TOO_MANY_MATCHES` are the
/// value-mode-specific codes ADR-0004/0005 describe only in prose ("rejected
/// (400)") without naming — named here for Phase 3.
pub(crate) mod reasons {
    pub const INVALID_FILTER: &str = "INVALID_FILTER";
    pub const INVALID_ORDERBY_FIELD: &str = "INVALID_ORDERBY_FIELD";
    pub const INVALID_SELECT: &str = "INVALID_SELECT";
    pub const VALUE_MODE_NO_PAGINATION: &str = "VALUE_MODE_NO_PAGINATION";
    pub const VALUE_MODE_NO_ORDER: &str = "VALUE_MODE_NO_ORDER";
    pub const VALUE_MODE_SELECTOR: &str = "VALUE_MODE_SELECTOR";
    pub const TOO_MANY_MATCHES: &str = "TOO_MANY_MATCHES";
}

fn invalid_filter(detail: impl Into<String>) -> DomainError {
    DomainError::InvalidRequest {
        field: "$filter",
        reason: reasons::INVALID_FILTER,
        detail: detail.into(),
    }
}

fn invalid_orderby(detail: impl Into<String>) -> DomainError {
    DomainError::InvalidRequest {
        field: "$orderby",
        reason: reasons::INVALID_ORDERBY_FIELD,
        detail: detail.into(),
    }
}

/// The collection read's filter/order vocabulary (ADR-0005, "What stays out
/// of the filter"): `reference` and `type` are SQL clamps; `sharing`,
/// `fallback`, `expires_at` are applied in memory after reduction.
/// `inheritance`, `owner_tenant_id` and `updated_at` are deliberately not
/// members of this enum, so naming any of them in `$filter`/`$orderby` is an
/// unknown field (`INVALID_FILTER`/`INVALID_ORDERBY_FIELD`, 400) by
/// construction rather than a case this module has to special-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CredentialFilterField {
    Reference,
    Type,
    Sharing,
    Fallback,
    ExpiresAt,
}

impl FilterField for CredentialFilterField {
    const FIELDS: &'static [Self] = &[
        Self::Reference,
        Self::Type,
        Self::Sharing,
        Self::Fallback,
        Self::ExpiresAt,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Type => "type",
            Self::Sharing => "sharing",
            Self::Fallback => "fallback",
            Self::ExpiresAt => "expires_at",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Reference | Self::Type | Self::Sharing | Self::Fallback => FieldKind::String,
            Self::ExpiresAt => FieldKind::DateTimeUtc,
        }
    }
}

/// `$select` allowlist (ADR-0004): the `Credential` field names plus
/// `secret`. Selecting `secret` switches the request to value mode.
const SELECT_ALLOWLIST: &[&str] = &[
    "reference",
    "type",
    "sharing",
    "status",
    "fallback",
    "expires_at",
    "inheritance",
    "version",
    "updated_at",
    "secret",
];

/// Validates a parsed `$select` field list (already lower-cased by the
/// `OData` extractor) against [`SELECT_ALLOWLIST`].
pub(crate) fn validate_select(fields: &[String]) -> Result<(), DomainError> {
    for field in fields {
        if !SELECT_ALLOWLIST.contains(&field.as_str()) {
            return Err(DomainError::InvalidRequest {
                field: "$select",
                reason: reasons::INVALID_SELECT,
                detail: format!("unsupported $select field: {field}"),
            });
        }
    }
    Ok(())
}

/// `true` iff `fields` names `secret` — the value-mode switch (ADR-0004).
pub(crate) fn is_value_mode(fields: Option<&[String]>) -> bool {
    fields.is_some_and(|fields| fields.iter().any(|f| f == "secret"))
}

/// Canonical sort direction for the collection read's one orderable field,
/// `reference` (ADR-0005: no other field is orderable, and `id` is an
/// automatic, non-selectable tiebreaker folded into the SQL clamp's keyset,
/// not a client-visible order key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListDirection {
    Asc,
    Desc,
}

impl ListDirection {
    #[must_use]
    pub(crate) fn is_desc(self) -> bool {
        matches!(self, Self::Desc)
    }
}

/// Validate metadata-mode `$orderby`: absent (default ascending) or exactly
/// `reference` (`asc`/`desc`); anything else is `INVALID_ORDERBY_FIELD`.
/// Never called for a cursor-driven page (the `OData` extractor forces
/// `query.order` empty whenever a cursor is present; the cursor's own minted
/// order is validated separately by [`crate::domain::secret::list_filter`]'s
/// caller via `toolkit_odata::validate_cursor_against`).
pub(crate) fn validate_metadata_orderby(
    order: &ODataOrderBy,
) -> Result<ListDirection, DomainError> {
    if order.is_empty() {
        return Ok(ListDirection::Asc);
    }
    let [key] = order.0.as_slice() else {
        return Err(invalid_orderby(
            "only a single `reference` order key is supported",
        ));
    };
    if !key.field.eq_ignore_ascii_case("reference") {
        return Err(invalid_orderby(format!(
            "unsupported $orderby field: {}",
            key.field
        )));
    }
    Ok(match key.dir {
        toolkit_odata::SortDir::Asc => ListDirection::Asc,
        toolkit_odata::SortDir::Desc => ListDirection::Desc,
    })
}

/// The collection read's parsed `$filter`, split exactly as ADR-0005
/// requires: [`Self::reference_in`]/[`Self::type_uuid_in`] are SQL clamps;
/// [`Self::sharing_eq`]/[`Self::fallback_eq`]/[`Self::expires_at`] are
/// checked in memory, per reduced item, after reduction
/// ([`Self::matches_post_reduction`]).
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedFilter {
    pub reference_in: Option<Vec<String>>,
    pub type_uuid_in: Option<Vec<Uuid>>,
    pub sharing_eq: Option<SharingMode>,
    pub fallback_eq: Option<Fallback>,
    pub expires_at: Option<(FilterOp, OffsetDateTime)>,
}

impl ParsedFilter {
    /// `true` once reduction has picked the effective row: applies the
    /// in-memory-only predicates (ADR-0005 §"What stays out of the
    /// filter"). `fallback` describes the caller's own row only, so an item
    /// with no own row (`fallback: None`) never matches a `fallback`
    /// predicate.
    #[must_use]
    pub(crate) fn matches_post_reduction(
        &self,
        sharing: SharingMode,
        fallback: Option<Fallback>,
        expires_at: Option<OffsetDateTime>,
    ) -> bool {
        if let Some(want) = self.sharing_eq
            && sharing != want
        {
            return false;
        }
        if let Some(want) = self.fallback_eq
            && fallback != Some(want)
        {
            return false;
        }
        if let Some((op, at)) = self.expires_at {
            let Some(actual) = expires_at else {
                return false;
            };
            let ok = match op {
                FilterOp::Eq => actual == at,
                FilterOp::Ne => actual != at,
                FilterOp::Gt => actual > at,
                FilterOp::Ge => actual >= at,
                FilterOp::Lt => actual < at,
                // `apply_leaf` only ever stores `Le` here (every other
                // operator on `expires_at` is rejected at parse time), so
                // this default is `Le`'s case, not a silent catch-all.
                _ => actual <= at,
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// Value mode's selector shape (ADR-0005 "Value mode has no cursor at
    /// all"; ADR-0004 "Bulk secret read"): the filter must be **exactly**
    /// one of `reference` (`eq`/`in`) or `type` (`eq`/`in`) — nothing else,
    /// and not both together.
    pub(crate) fn require_value_mode_selector(&self) -> Result<(), DomainError> {
        let selector_error = || DomainError::InvalidRequest {
            field: "$filter",
            reason: reasons::VALUE_MODE_SELECTOR,
            detail: "value mode requires $filter to be exactly `reference eq/in (...)` or \
                     `type eq/in (...)`"
                .to_owned(),
        };
        let extra_predicate =
            self.sharing_eq.is_some() || self.fallback_eq.is_some() || self.expires_at.is_some();
        match (
            self.reference_in.as_ref(),
            self.type_uuid_in.as_ref(),
            extra_predicate,
        ) {
            (Some(_), None, false) | (None, Some(_), false) => Ok(()),
            _ => Err(selector_error()),
        }
    }
}

/// Parse and validate `raw_filter` (already extracted from `$filter` by the
/// `OData` extractor) into a [`ParsedFilter`]. Rejects `or`/`not` anywhere
/// (only a top-level conjunction of single-field predicates is supported),
/// an operator ADR-0005 does not allow for a field (`reference`/`type`:
/// `eq`/`in` only), a field named more than once, or a `type` value that is
/// not a well-formed GTS id.
pub(crate) fn parse_filter(expr: &toolkit_odata::ast::Expr) -> Result<ParsedFilter, DomainError> {
    let node: FilterNode<CredentialFilterField> =
        toolkit_odata::filter::convert_expr_to_filter_node(expr)
            .map_err(|e| invalid_filter(e.to_string()))?;

    let mut leaves = Vec::new();
    flatten_conjunction(&node, &mut leaves)?;

    let mut parsed = ParsedFilter::default();
    for leaf in leaves {
        apply_leaf(&mut parsed, leaf)?;
    }
    Ok(parsed)
}

/// One `field <op> value(s)` predicate, after `$filter`'s top-level
/// conjunction has been flattened.
enum Leaf {
    Binary(CredentialFilterField, FilterOp, ODataValue),
    InList(CredentialFilterField, Vec<ODataValue>),
}

fn flatten_conjunction(
    node: &FilterNode<CredentialFilterField>,
    out: &mut Vec<Leaf>,
) -> Result<(), DomainError> {
    match node {
        FilterNode::Composite {
            op: FilterOp::And,
            children,
        } => {
            for child in children {
                flatten_conjunction(child, out)?;
            }
            Ok(())
        }
        FilterNode::Composite { op, .. } => Err(invalid_filter(format!(
            "`{op}` is not supported in $filter; combine predicates with `and` only"
        ))),
        FilterNode::Not(_) => Err(invalid_filter("`not` is not supported in $filter")),
        FilterNode::Binary { field, op, value } => {
            out.push(Leaf::Binary(*field, *op, value.clone()));
            Ok(())
        }
        FilterNode::InList { field, values } => {
            out.push(Leaf::InList(*field, values.clone()));
            Ok(())
        }
    }
}

fn duplicate_field(name: &str) -> DomainError {
    invalid_filter(format!("$filter names `{name}` more than once"))
}

fn unsupported_operator(name: &str, op: FilterOp) -> DomainError {
    invalid_filter(format!(
        "`{op}` is not supported on `{name}`; use `eq`/`in`"
    ))
}

fn string_value(field: CredentialFilterField, value: &ODataValue) -> Result<String, DomainError> {
    match value {
        ODataValue::String(s) => Ok(s.clone()),
        // The generic parser already rejected any non-`String` value for a
        // `FieldKind::String` field before this is reached.
        _ => Err(invalid_filter(format!(
            "`{}` requires a string value",
            field.name()
        ))),
    }
}

fn type_uuid_value(value: &ODataValue) -> Result<Uuid, DomainError> {
    let raw = string_value(CredentialFilterField::Type, value)?;
    GtsId::try_new(&raw)
        .map(|id| id.to_uuid())
        .map_err(|_| invalid_filter(format!("`type` must be a full GTS type id: {raw}")))
}

fn sharing_value(value: &ODataValue) -> Result<SharingMode, DomainError> {
    match string_value(CredentialFilterField::Sharing, value)?.as_str() {
        "private" => Ok(SharingMode::Private),
        "tenant" => Ok(SharingMode::Tenant),
        "shared" => Ok(SharingMode::Shared),
        other => Err(invalid_filter(format!(
            "`sharing` must be one of private/tenant/shared, got `{other}`"
        ))),
    }
}

fn fallback_value(value: &ODataValue) -> Result<Fallback, DomainError> {
    match string_value(CredentialFilterField::Fallback, value)?.as_str() {
        "inherit" => Ok(Fallback::Inherit),
        "none" => Ok(Fallback::None),
        other => Err(invalid_filter(format!(
            "`fallback` must be one of inherit/none, got `{other}`"
        ))),
    }
}

fn expires_at_value(value: &ODataValue) -> Result<OffsetDateTime, DomainError> {
    match value {
        ODataValue::DateTime(dt) => {
            let nanos = i128::from(dt.timestamp()) * 1_000_000_000
                + i128::from(dt.timestamp_subsec_nanos());
            OffsetDateTime::from_unix_timestamp_nanos(nanos)
                .map_err(|e| invalid_filter(format!("`expires_at` is out of range: {e}")))
        }
        _ => Err(invalid_filter("`expires_at` requires a datetime value")),
    }
}

fn apply_leaf(parsed: &mut ParsedFilter, leaf: Leaf) -> Result<(), DomainError> {
    match leaf {
        Leaf::Binary(CredentialFilterField::Reference, FilterOp::Eq, value) => {
            if parsed.reference_in.is_some() {
                return Err(duplicate_field("reference"));
            }
            parsed.reference_in = Some(vec![string_value(
                CredentialFilterField::Reference,
                &value,
            )?]);
        }
        Leaf::Binary(CredentialFilterField::Reference, op, _) => {
            return Err(unsupported_operator("reference", op));
        }
        Leaf::InList(CredentialFilterField::Reference, values) => {
            if parsed.reference_in.is_some() {
                return Err(duplicate_field("reference"));
            }
            parsed.reference_in = Some(
                values
                    .iter()
                    .map(|v| string_value(CredentialFilterField::Reference, v))
                    .collect::<Result<_, _>>()?,
            );
        }
        Leaf::Binary(CredentialFilterField::Type, FilterOp::Eq, value) => {
            if parsed.type_uuid_in.is_some() {
                return Err(duplicate_field("type"));
            }
            parsed.type_uuid_in = Some(vec![type_uuid_value(&value)?]);
        }
        Leaf::Binary(CredentialFilterField::Type, op, _) => {
            return Err(unsupported_operator("type", op));
        }
        Leaf::InList(CredentialFilterField::Type, values) => {
            if parsed.type_uuid_in.is_some() {
                return Err(duplicate_field("type"));
            }
            parsed.type_uuid_in = Some(
                values
                    .iter()
                    .map(type_uuid_value)
                    .collect::<Result<_, _>>()?,
            );
        }
        Leaf::Binary(CredentialFilterField::Sharing, FilterOp::Eq, value) => {
            if parsed.sharing_eq.is_some() {
                return Err(duplicate_field("sharing"));
            }
            parsed.sharing_eq = Some(sharing_value(&value)?);
        }
        Leaf::Binary(CredentialFilterField::Sharing, op, _) => {
            return Err(unsupported_operator("sharing", op));
        }
        Leaf::InList(CredentialFilterField::Sharing, _) => {
            return Err(unsupported_operator("sharing", FilterOp::In));
        }
        Leaf::Binary(CredentialFilterField::Fallback, FilterOp::Eq, value) => {
            if parsed.fallback_eq.is_some() {
                return Err(duplicate_field("fallback"));
            }
            parsed.fallback_eq = Some(fallback_value(&value)?);
        }
        Leaf::Binary(CredentialFilterField::Fallback, op, _) => {
            return Err(unsupported_operator("fallback", op));
        }
        Leaf::InList(CredentialFilterField::Fallback, _) => {
            return Err(unsupported_operator("fallback", FilterOp::In));
        }
        Leaf::Binary(CredentialFilterField::ExpiresAt, op, value) => {
            if parsed.expires_at.is_some() {
                return Err(duplicate_field("expires_at"));
            }
            parsed.expires_at = Some((op, expires_at_value(&value)?));
        }
        Leaf::InList(CredentialFilterField::ExpiresAt, _) => {
            return Err(unsupported_operator("expires_at", FilterOp::In));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "list_filter_tests.rs"]
mod tests;
