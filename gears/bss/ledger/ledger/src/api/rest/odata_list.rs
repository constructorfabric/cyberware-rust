//! Shared guards for ledger OData collection GETs.
//!
//! Extra query keys (including `?tenant_id=`) are 400. Seller tenant comes from
//! `$filter=tenant_id eq <uuid>` or the caller's tenant when that comparison is
//! absent. `limit` / `cursor` stay allowed so OpenAPI pagination matches AM.

use std::collections::HashMap;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_odata::ast::{CompareOperator, Expr, Value};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Keys an in-scope list may carry besides the OData extractor's `$` options.
const LIST_PAGINATION_KEYS: &[&str] = &["limit", "cursor"];

/// Reject a named filter / scope key on an OData list (`?tenant_id=`, `?status=`).
///
/// `$`-prefixed keys are out of scope: the extractor already 400s unknown
/// system options. `limit` and `cursor` are the AM-tenants pagination aliases.
pub(crate) fn reject_non_odata_list_params(
    query: &HashMap<String, String>,
) -> Result<(), CanonicalError> {
    reject_non_odata_list_params_allowing(query, &[])
}

/// Same as [`reject_non_odata_list_params`], plus extra non-filter keys the
/// route already advertises (balances `valuation`).
pub(crate) fn reject_non_odata_list_params_allowing(
    query: &HashMap<String, String>,
    extra: &[&str],
) -> Result<(), CanonicalError> {
    if let Some(unknown) = query.keys().find(|k| {
        !k.starts_with('$')
            && !LIST_PAGINATION_KEYS.contains(&k.as_str())
            && !extra.contains(&k.as_str())
    }) {
        return Err(CanonicalError::from(DomainError::InvalidRequest(format!(
            "unrecognized query parameter `{unknown}`; ledger list endpoints \
             accept OData parameters only (e.g. `$filter=tenant_id eq <uuid>`) \
             plus `limit` and `cursor`"
        ))));
    }
    Ok(())
}

/// Seller for an in-scope list: one `tenant_id eq <uuid>` or the caller.
///
/// Distinct `tenant_id eq` values under `or` (or anywhere) are 400 — one seller
/// per request, same as today's single `?tenant_id=`.
pub(crate) fn list_seller_tenant(
    filter: Option<&Expr>,
    caller: Uuid,
) -> Result<Uuid, CanonicalError> {
    let mut found = Vec::new();
    if let Some(expr) = filter {
        collect_tenant_eq(expr, &mut found)?;
    }
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [] => Ok(caller),
        [one] => Ok(*one),
        _ => Err(CanonicalError::from(DomainError::InvalidRequest(
            "list $filter may name at most one seller (`tenant_id eq <uuid>`)".to_owned(),
        ))),
    }
}

fn collect_tenant_eq(expr: &Expr, out: &mut Vec<Uuid>) -> Result<(), CanonicalError> {
    match expr {
        Expr::And(a, b) | Expr::Or(a, b) => {
            collect_tenant_eq(a, out)?;
            collect_tenant_eq(b, out)
        }
        Expr::Compare(left, CompareOperator::Eq, right) => {
            if let Some(id) = tenant_eq_value(left, right) {
                out.push(id);
            }
            Ok(())
        }
        Expr::In(inner, values) => {
            if matches!(inner.as_ref(), Expr::Identifier(name) if name == "tenant_id") {
                for value in values {
                    if let Expr::Value(v) = value
                        && let Some(id) = uuid_value(v)
                    {
                        out.push(id);
                    }
                }
            }
            Ok(())
        }
        // Nothing else names a seller: a negation asserts no equality, and the
        // remaining shapes carry no `tenant_id eq` to collect.
        Expr::Not(_)
        | Expr::Compare(_, _, _)
        | Expr::Function(_, _)
        | Expr::Identifier(_)
        | Expr::Value(_) => Ok(()),
    }
}

fn tenant_eq_value(left: &Expr, right: &Expr) -> Option<Uuid> {
    match (left, right) {
        (Expr::Identifier(name), Expr::Value(value)) if name == "tenant_id" => uuid_value(value),
        (Expr::Value(value), Expr::Identifier(name)) if name == "tenant_id" => uuid_value(value),
        _ => None,
    }
}

fn uuid_value(value: &Value) -> Option<Uuid> {
    match value {
        Value::Uuid(id) => Some(*id),
        Value::String(s) => Uuid::parse_str(s).ok(),
        _ => None,
    }
}

#[cfg(test)]
#[path = "odata_list_tests.rs"]
mod tests;
