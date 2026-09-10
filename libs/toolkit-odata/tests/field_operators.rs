#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A field only accepts the operators its endpoint's contract publishes.
//!
//! `OperationBuilder::with_odata_filter` writes `x-odata-filter.allowedFields`
//! from `FieldKind::allows`, and the parser refuses anything outside it, so a
//! caller reading the contract and a caller probing the endpoint get the same
//! answer.

use toolkit_odata::filter::{FieldKind, FilterField, FilterOp, parse_odata_filter};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Name,
    Stars,
    Private,
    Id,
}

impl FilterField for Field {
    const FIELDS: &'static [Self] = &[Self::Name, Self::Stars, Self::Private, Self::Id];

    fn name(&self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Stars => "stars",
            Self::Private => "private",
            Self::Id => "id",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Name => FieldKind::String,
            Self::Stars => FieldKind::I64,
            Self::Private => FieldKind::Bool,
            Self::Id => FieldKind::Uuid,
        }
    }
}

fn accepted(filter: &str) -> bool {
    parse_odata_filter::<Field>(filter).is_ok()
}

#[test]
fn published_operators_are_accepted() {
    for filter in [
        "name eq 'rust'",
        "name ne 'rust'",
        "contains(name,'ru')",
        "startswith(name,'ru')",
        "endswith(name,'st')",
        "name in ('rust','cargo')",
        "stars gt 10",
        "stars le 10",
        "stars in (1,2,3)",
        "private eq true",
        "private ne false",
        "id eq 00000000-0000-0000-0000-000000000001",
    ] {
        assert!(accepted(filter), "{filter} must be accepted");
    }
}

#[test]
fn ordering_operators_are_refused_on_a_bool() {
    for filter in [
        "private gt false",
        "private ge false",
        "private lt true",
        "private le true",
        "private in (true,false)",
    ] {
        assert!(!accepted(filter), "{filter} must be refused");
    }
}

#[test]
fn ordering_operators_are_refused_on_a_string_or_uuid() {
    assert!(!accepted("name gt 'rust'"));
    assert!(!accepted("name le 'rust'"));
    assert!(!accepted("id gt 00000000-0000-0000-0000-000000000001"));
}

#[test]
fn the_table_the_contract_publishes_is_the_table_the_parser_uses() {
    assert!(FieldKind::Bool.allows(FilterOp::Eq));
    assert!(!FieldKind::Bool.allows(FilterOp::Gt));
    assert!(!FieldKind::Bool.allows(FilterOp::In));

    assert!(FieldKind::String.allows(FilterOp::Contains));
    assert!(!FieldKind::String.allows(FilterOp::Lt));

    assert!(FieldKind::Uuid.allows(FilterOp::In));
    assert!(!FieldKind::Uuid.allows(FilterOp::StartsWith));

    assert!(FieldKind::DateTimeUtc.allows(FilterOp::Ge));
    assert!(!FieldKind::DateTimeUtc.allows(FilterOp::EndsWith));
}
