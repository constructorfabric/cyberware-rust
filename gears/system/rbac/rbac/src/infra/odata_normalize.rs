//! Literal-shape normalization for caller-supplied `$filter` ASTs.
//!
//! The `OData` grammar decides a literal's type from its *spelling*: a bare
//! `550e8400-e29b-41d4-a716-446655440000` parses as `Value::Uuid`, the same
//! text in single quotes parses as `Value::String`. A field enum declares
//! one [`toolkit_odata::filter::FieldKind`] per field, and
//! `convert_expr_to_filter_node` rejects
//! any literal whose parsed type does not match it. Put together, that makes
//! the spelling of an id filter depend on a column type the caller cannot
//! see — and on `GET /rbac/v1/role-assignments` it makes it depend on
//! *which* id, because one response carries a `text` `principal_id` and a
//! `uuid` `role_definition_id`.
//!
//! This module removes the guessing without moving either field's declared
//! kind, because both of the obvious "just align the types" fixes are worse
//! than the asymmetry:
//!
//! * declaring `principal_id` as `Uuid` would delete `contains` /
//!   `startswith` / `endswith` on it — `toolkit_odata` offers those three
//!   functions on string fields only — and `principal_id` is a `text` column
//!   holding deliberately opaque provider-issued ids, not UUIDs;
//! * declaring `role_definition_id` as `String` would bind a text parameter
//!   against a `uuid` column, which Postgres answers with
//!   `operator does not exist: uuid = text`. A 400 that told the caller how
//!   to spell the filter would become a 500 that told them nothing.
//!
//! So the coercion happens on the *literal*, in one pass over the AST,
//! before validation: each literal is re-shaped into the kind its field
//! declares, and anything that cannot be re-shaped is left exactly as the
//! caller wrote it so the existing type-mismatch 400 still fires.
//!
//! Generic over the field enum via [`toolkit_odata::filter::FilterField`], so
//! a list endpoint opts in by calling `normalize_filter_literals` with its own
//! `F` — there is
//! no per-field or per-endpoint table here to fall out of date.

use toolkit_odata::ast::{Expr, Value};
use toolkit_odata::filter::{FieldKind, FilterField};

/// Rewrite every literal in `expr` into the [`FieldKind`] its field
/// declares, leaving the tree's shape untouched.
///
/// Two coercions, and only two:
///
/// * a `Uuid` literal on a `String` field becomes the canonical hyphenated
///   lowercase text of that UUID — the spelling
///   `Uuid::to_string` produces, and therefore the spelling every
///   RBAC writer stores when it stores a UUID in a text column;
/// * a `String` literal on a `Uuid` field becomes a `Uuid`, but only when it
///   actually parses as one.
///
/// Everything else — a non-UUID string on a `Uuid` field, a number on a
/// `Bool` field, an unknown field name, a bare identifier — is passed
/// through verbatim. That is deliberate: this pass must never turn a request
/// the validator would have rejected into one it accepts by accident, so
/// every diagnostic (`UnknownField`, `TypeMismatch`, `BareLiteral`, …) is
/// still raised by `convert_expr_to_filter_node` on the normalized tree.
///
/// Caveat, for the `Uuid`-to-text direction: matching is still textual. A
/// row whose stored text is a non-canonical spelling of the same UUID is not
/// matched by a normalized bare literal. That is how a `text` column has
/// always behaved, not something this pass introduces, and a caller holding
/// a non-canonical id can quote it to match it byte for byte.
#[must_use]
pub fn normalize_filter_literals<F: FilterField>(expr: &Expr) -> Expr {
    match expr {
        // Logical structure: recurse, rebuild, coerce nothing.
        Expr::And(left, right) => Expr::And(
            Box::new(normalize_filter_literals::<F>(left)),
            Box::new(normalize_filter_literals::<F>(right)),
        ),
        Expr::Or(left, right) => Expr::Or(
            Box::new(normalize_filter_literals::<F>(left)),
            Box::new(normalize_filter_literals::<F>(right)),
        ),
        Expr::Not(inner) => Expr::Not(Box::new(normalize_filter_literals::<F>(inner))),

        // `field op literal`: the one shape the validator type-checks.
        // Field-to-field and literal-to-literal comparisons are rejected
        // downstream, so they are rebuilt unchanged rather than special-cased
        // here.
        Expr::Compare(left, op, right) => {
            let right = match (&**left, &**right) {
                (Expr::Identifier(name), Expr::Value(value)) => {
                    Expr::Value(coerce_for_field::<F>(name, value))
                }
                _ => (**right).clone(),
            };
            Expr::Compare(left.clone(), *op, Box::new(right))
        }

        // `field in (a, b, c)`: every element is checked against the same
        // field, so every element gets the same coercion. A mixed list
        // (`id in (<bare>, '<quoted>')`) normalizes to one kind and is
        // accepted; without this pass it is a guaranteed 400 on whichever
        // element was spelled "wrong".
        Expr::In(left, items) => {
            let items = match &**left {
                Expr::Identifier(name) => items
                    .iter()
                    .map(|item| match item {
                        Expr::Value(value) => Expr::Value(coerce_for_field::<F>(name, value)),
                        other => other.clone(),
                    })
                    .collect(),
                _ => items.clone(),
            };
            Expr::In(left.clone(), items)
        }

        // `contains(field, 'x')` and friends. These accept a *string*
        // literal on a *string* field and nothing else, so only the
        // uuid-to-text direction is applied: `startswith(principal_id,
        // <bare-uuid>)` becomes the quoted form the function requires
        // instead of an `UnsupportedOperation`. The reverse direction is
        // deliberately skipped — turning `contains(role_definition_id,
        // '<uuid>')` into a `Uuid` argument would trade that request's
        // "wrong type for this field" 400 for a vaguer "unsupported
        // function" 400 and tell the caller less than before.
        Expr::Function(name, args) => {
            let args = match args.as_slice() {
                [Expr::Identifier(field), Expr::Value(value)] => vec![
                    Expr::Identifier(field.clone()),
                    Expr::Value(uuid_literal_as_text::<F>(field, value)),
                ],
                _ => args.clone(),
            };
            Expr::Function(name.clone(), args)
        }

        // Leaves: nothing to rewrite.
        Expr::Identifier(_) | Expr::Value(_) => expr.clone(),
    }
}

/// Coerce one literal for one named field, or hand it back untouched.
///
/// An unresolvable field name is *not* an error here — reporting it is
/// `convert_expr_to_filter_node`'s job, and duplicating the check would give
/// the same request two different error messages depending on which layer
/// noticed first.
fn coerce_for_field<F: FilterField>(field_name: &str, value: &Value) -> Value {
    let Some(field) = F::from_name(field_name) else {
        return value.clone();
    };
    match (field.kind(), value) {
        // Bare uuid on a text column: canonical hyphenated lowercase, which
        // is what `Uuid: Display` emits.
        (FieldKind::String, Value::Uuid(id)) => Value::String(id.to_string()),
        // Quoted uuid on a uuid column: only when it really is one, so a
        // genuine type mismatch keeps its 400.
        (FieldKind::Uuid, Value::String(text)) => {
            uuid::Uuid::parse_str(text).map_or_else(|_| value.clone(), Value::Uuid)
        }
        _ => value.clone(),
    }
}

/// The uuid-to-text half of [`coerce_for_field`], for positions where a
/// string literal is the only legal shape (the `contains` / `startswith` /
/// `endswith` argument).
fn uuid_literal_as_text<F: FilterField>(field_name: &str, value: &Value) -> Value {
    match (F::from_name(field_name).map(|field| field.kind()), value) {
        (Some(FieldKind::String), Value::Uuid(id)) => Value::String(id.to_string()),
        _ => value.clone(),
    }
}

#[cfg(test)]
#[path = "odata_normalize_tests.rs"]
mod odata_normalize_tests;
