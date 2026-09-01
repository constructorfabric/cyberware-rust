//! Typed projection from `toolkit_odata::Error` → `DomainError`.
//!
//! `toolkit_odata::Error::Db(String)` carries raw driver text (SQL
//! fragments, DSN snippets, etc.). Forwarding that `Display` chain
//! straight into `DomainError::Internal { diagnostic }` bypasses the
//! redaction discipline used by [`super::error_conv::redacted_db_diagnostic`]
//! everywhere else in this layer. This helper maps each typed variant
//! to the right `DomainError` category, with the `Db(_)` arm
//! substituted by a static label so operator-supplied text cannot leak
//! into the audit-side diagnostic.
//!
//! Caller-facing parse/usage errors (`InvalidFilter`, `InvalidCursor`,
//! etc.) get `Validation`. Anything else stays `Internal` but with the
//! diagnostic body bounded.
//!
//! This module also hosts the shared list-ordering policy
//! ([`list_query_with_default_order`]) so the per-resource `list` repos
//! can't drift apart on default pagination order.

use crate::domain::error::DomainError;
use toolkit_odata::{Error as ODataError, ODataOrderBy, ODataQuery, OrderKey, SortDir};

/// Map a `toolkit_odata::Error` to a `DomainError`, keeping any DB-side
/// driver text out of the diagnostic body. See module docs.
pub fn map_odata_err_to_domain(err: ODataError) -> DomainError {
    match err {
        ODataError::InvalidFilter(d) => DomainError::Validation {
            detail: format!("$filter: {d}"),
        },
        ODataError::InvalidOrderByField(d) => DomainError::Validation {
            detail: format!("$orderby: {d}"),
        },
        ODataError::OrderMismatch => DomainError::Validation {
            detail: "ORDER_MISMATCH".to_owned(),
        },
        ODataError::FilterMismatch => DomainError::Validation {
            detail: "FILTER_MISMATCH".to_owned(),
        },
        ODataError::InvalidCursor
        | ODataError::CursorInvalidBase64
        | ODataError::CursorInvalidJson
        | ODataError::CursorInvalidVersion
        | ODataError::CursorInvalidKeys
        | ODataError::CursorInvalidFields
        | ODataError::CursorInvalidDirection => DomainError::Validation {
            detail: "INVALID_CURSOR".to_owned(),
        },
        ODataError::InvalidLimit => DomainError::Validation {
            detail: "INVALID_LIMIT".to_owned(),
        },
        ODataError::OrderWithCursor => DomainError::Validation {
            detail: "ORDER_WITH_CURSOR".to_owned(),
        },
        // Driver text dropped: keep the diagnostic body static so the
        // audit-side `Internal::diagnostic` cannot carry SQL fragments
        // or DSN snippets emitted by the underlying database driver.
        ODataError::Db(_) => {
            DomainError::internal("paginate_odata: database error (driver text redacted)")
        }
        ODataError::ParsingUnavailable(reason) => {
            DomainError::internal(format!("paginate_odata: parsing unavailable: {reason}"))
        }
    }
}

/// Shared default list-ordering policy for the RBAC `list` repos.
///
/// Returns a copy of `query` with the user `$filter` stripped — callers
/// apply it to `base_select` themselves, and `paginate_odata` would
/// otherwise re-apply it — and, when the caller supplied neither
/// `$orderby` nor a `cursor`, a default `created_at DESC` order so the
/// keyset hits the `(created_at, id)` index. Both `list` impls funnel
/// through here so they cannot drift apart on default ordering.
pub fn list_query_with_default_order(query: &ODataQuery) -> ODataQuery {
    let mut out = query.clone();
    out.filter = None;
    if out.cursor.is_none() && out.order.is_empty() {
        out = out.with_order(ODataOrderBy(vec![OrderKey {
            field: "created_at".to_owned(),
            dir: SortDir::Desc,
        }]));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::{list_query_with_default_order, map_odata_err_to_domain};
    use crate::domain::error::DomainError;
    use toolkit_odata::{Error as ODataError, ODataOrderBy, ODataQuery, OrderKey, SortDir};

    #[test]
    fn invalid_filter_maps_to_validation() {
        match map_odata_err_to_domain(ODataError::InvalidFilter("bad".to_owned())) {
            DomainError::Validation { detail } => {
                assert!(detail.contains("$filter"), "got: {detail}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn invalid_orderby_field_maps_to_validation() {
        match map_odata_err_to_domain(ODataError::InvalidOrderByField("nope".to_owned())) {
            DomainError::Validation { detail } => {
                assert!(detail.contains("$orderby"), "got: {detail}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn invalid_cursor_maps_to_validation() {
        match map_odata_err_to_domain(ODataError::InvalidCursor) {
            DomainError::Validation { detail } => assert_eq!(detail, "INVALID_CURSOR"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// The redaction contract: a `Db` error becomes `Internal` and the
    /// diagnostic MUST NOT echo the driver text (SQL / DSN fragments).
    #[test]
    fn db_error_is_internal_with_redacted_diagnostic() {
        let leak = "near \"SELECT\": syntax error; dsn=postgres://u:secretpw@host/db";
        match map_odata_err_to_domain(ODataError::Db(leak.to_owned())) {
            DomainError::Internal { diagnostic, .. } => assert!(
                !diagnostic.contains("secretpw")
                    && !diagnostic.contains("dsn")
                    && !diagnostic.contains("SELECT"),
                "driver text MUST NOT leak into the diagnostic, got: {diagnostic}"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn parsing_unavailable_maps_to_internal() {
        match map_odata_err_to_domain(ODataError::ParsingUnavailable("no parser")) {
            DomainError::Internal { diagnostic, .. } => {
                assert!(
                    diagnostic.contains("parsing unavailable"),
                    "got: {diagnostic}"
                );
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn default_order_appended_and_filter_cleared_on_bare_query() {
        let filter = toolkit_odata::parse_filter_string("name eq 'x'")
            .expect("test filter parses")
            .into_expr();
        let q = ODataQuery::new().with_filter(filter);
        let out = list_query_with_default_order(&q);
        assert!(out.filter.is_none(), "the user $filter MUST be stripped");
        assert_eq!(out.order.0.len(), 1, "a default order MUST be injected");
        assert_eq!(out.order.0[0].field, "created_at");
        assert!(matches!(out.order.0[0].dir, SortDir::Desc));
    }

    #[test]
    fn existing_order_is_left_untouched() {
        let q = ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
            field: "name".to_owned(),
            dir: SortDir::Asc,
        }]));
        let out = list_query_with_default_order(&q);
        assert_eq!(out.order.0.len(), 1);
        assert_eq!(
            out.order.0[0].field, "name",
            "an explicit $orderby MUST be preserved, not overwritten by the default"
        );
        assert!(matches!(out.order.0[0].dir, SortDir::Asc));
    }
}
