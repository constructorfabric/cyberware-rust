//! Tests for [`super::normalize_filter_literals`].
//!
//! Every case drives the *real* pipeline — parse the filter string the way
//! the `OData` extractor does, normalize, then validate with
//! `convert_expr_to_filter_node` — because the whole point of the pass is
//! what the validator does with its output. Asserting on the rewritten AST
//! alone would pass just as happily if the validator still rejected it.
//!
//! Two properties carry the pass, and both are asserted for both id
//! fields: a caller may spell an id filter either way, and a request the
//! validator rejects for a real reason is still rejected.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use toolkit_odata::filter::{
    FilterError, FilterNode, FilterOp, ODataValue, convert_expr_to_filter_node,
};

use super::normalize_filter_literals;
use crate::odata::{RoleAssignmentFilterField as Ra, RoleDefinitionFilterField as Rd};

/// A canonical UUID and its two other legal spellings on the wire. Upper
/// case is included because the parser's `hex` rule accepts `A-F`, so an
/// upper-case bare literal is a shape a caller can actually send.
const ID: &str = "01920000-0000-7000-8000-0000000000a1";
const ID_UPPER: &str = "01920000-0000-7000-8000-0000000000A1";

/// Parse → normalize → validate, for one field enum.
fn lower<F: toolkit_odata::filter::FilterField>(raw: &str) -> Result<FilterNode<F>, FilterError> {
    let expr = toolkit_odata::parse_filter_string(raw)
        .expect("test filter must parse")
        .into_expr();
    let normalized = normalize_filter_literals::<F>(&expr);
    convert_expr_to_filter_node::<F>(&normalized)
}

/// Same pipeline with the normalization pass removed, so a test can show
/// that a spelling really was rejected before and is accepted now rather
/// than asserting a behaviour that never differed.
fn lower_without_normalization<F: toolkit_odata::filter::FilterField>(
    raw: &str,
) -> Result<FilterNode<F>, FilterError> {
    let expr = toolkit_odata::parse_filter_string(raw)
        .expect("test filter must parse")
        .into_expr();
    convert_expr_to_filter_node::<F>(&expr)
}

/// Render a literal as `kind:text` so an assertion pins the *type* the
/// validator saw, not only the characters. `string:…` versus `uuid:…` is
/// exactly the distinction this module exists to control.
fn literal(value: &ODataValue) -> String {
    match value {
        ODataValue::String(text) => format!("string:{text}"),
        ODataValue::Uuid(id) => format!("uuid:{id}"),
        other => other.to_string(),
    }
}

/// Destructure a `Binary` node, or fail with the node that was produced
/// instead — a composite where a binary was expected is a normalization bug
/// worth seeing in full.
fn binary<F: toolkit_odata::filter::FilterField>(node: &FilterNode<F>) -> (F, FilterOp, String) {
    match node {
        FilterNode::Binary { field, op, value } => (*field, *op, literal(value)),
        other => panic!("expected a binary node, got {other:?}"),
    }
}

fn in_list<F: toolkit_odata::filter::FilterField>(node: &FilterNode<F>) -> (F, Vec<String>) {
    match node {
        FilterNode::InList { field, values } => {
            (*field, values.iter().map(literal).collect::<Vec<_>>())
        }
        other => panic!("expected an in-list node, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Both spellings, both id fields
// ---------------------------------------------------------------------------

/// The consumer trap, from the `principal_id` side: without normalization a
/// bare UUID is a 400 on a field whose column is `text`. The pass lands it
/// as canonical text.
#[test]
fn bare_uuid_is_accepted_on_the_text_principal_id_field() {
    assert!(
        matches!(
            lower_without_normalization::<Ra>(&format!("principal_id eq {ID}")),
            Err(FilterError::TypeMismatch { .. })
        ),
        "precondition: the bare spelling MUST be what normalization fixes"
    );

    let node = lower::<Ra>(&format!("principal_id eq {ID}")).expect("bare uuid must be accepted");
    assert_eq!(
        binary(&node),
        (Ra::PrincipalId, FilterOp::Eq, format!("string:{ID}")),
        "a bare uuid on a text column must arrive as text, or SQL binds the wrong type"
    );
}

/// …and from the `role_definition_id` side: without normalization a quoted
/// UUID is a 400 on a field whose column is `uuid`.
#[test]
fn quoted_uuid_is_accepted_on_the_uuid_role_definition_id_field() {
    assert!(
        matches!(
            lower_without_normalization::<Ra>(&format!("role_definition_id eq '{ID}'")),
            Err(FilterError::TypeMismatch { .. })
        ),
        "precondition: the quoted spelling MUST be what normalization fixes"
    );

    let node = lower::<Ra>(&format!("role_definition_id eq '{ID}'"))
        .expect("quoted uuid must be accepted");
    assert_eq!(
        binary(&node),
        (Ra::RoleDefinitionId, FilterOp::Eq, format!("uuid:{ID}")),
        "a quoted uuid on a uuid column must arrive as a Uuid, never as text"
    );
}

/// The spellings that already worked keep working untouched — the pass must
/// be additive, not a swap of which half of the surface is broken.
#[test]
fn the_previously_working_spellings_are_unchanged() {
    let node = lower::<Ra>("principal_id eq 'alice'").expect("quoted text must still work");
    assert_eq!(
        binary(&node),
        (Ra::PrincipalId, FilterOp::Eq, "string:alice".to_owned())
    );

    let node =
        lower::<Ra>(&format!("role_definition_id eq {ID}")).expect("bare uuid must still work");
    assert_eq!(
        binary(&node),
        (Ra::RoleDefinitionId, FilterOp::Eq, format!("uuid:{ID}"))
    );
}

/// An upper-case bare literal is canonicalised to lowercase hyphenated
/// text. Pinned rather than left implicit because it is the one case where
/// the caller's bytes are *changed*: a row stored with the upper-case
/// spelling will not match, which is the documented behaviour of an opaque
/// text column and the reason the quoted form stays available for it.
#[test]
fn a_bare_uuid_is_canonicalised_to_lowercase_on_a_text_field() {
    let node =
        lower::<Ra>(&format!("principal_id eq {ID_UPPER}")).expect("bare uuid must be accepted");
    assert_eq!(
        binary(&node),
        (Ra::PrincipalId, FilterOp::Eq, format!("string:{ID}"))
    );
}

// ---------------------------------------------------------------------------
// Lists, nesting, functions
// ---------------------------------------------------------------------------

/// `in` lists are coerced element-wise, so a list may even mix the two
/// spellings. Element-wise is not an optimisation: the validator type-checks
/// every element, so one wrongly spelled member would reject the whole
/// clause.
#[test]
fn in_lists_are_coerced_element_wise_for_both_kinds() {
    let node = lower::<Ra>(&format!("principal_id in ({ID}, 'alice')"))
        .expect("a mixed list on a text field must be accepted");
    assert_eq!(
        in_list(&node),
        (
            Ra::PrincipalId,
            vec![format!("string:{ID}"), "string:alice".to_owned()]
        )
    );

    let node = lower::<Ra>(&format!("role_definition_id in ('{ID}', {ID})"))
        .expect("a mixed list on a uuid field must be accepted");
    assert_eq!(
        in_list(&node),
        (
            Ra::RoleDefinitionId,
            vec![format!("uuid:{ID}"), format!("uuid:{ID}")]
        )
    );
}

/// Coercion reaches every leaf of a nested boolean expression, including
/// through `not`. A pass that only looked at the top-level node would make
/// the accepted spelling depend on how many parentheses the caller used.
#[test]
fn nested_and_or_not_are_normalized_at_every_leaf() {
    // Declared before the statements below: an item after a statement is
    // confusing (it is in scope for the whole block anyway) and the
    // workspace lints against it.
    fn leaves(node: &FilterNode<Ra>, out: &mut Vec<String>) {
        match node {
            FilterNode::Binary { value, .. } => out.push(literal(value)),
            FilterNode::InList { values, .. } => out.extend(values.iter().map(literal)),
            FilterNode::Composite { children, .. } => {
                for child in children {
                    leaves(child, out);
                }
            }
            FilterNode::Not(inner) => leaves(inner, out),
        }
    }

    let raw = format!(
        "(principal_id eq {ID} or not (role_definition_id eq '{ID}')) and principal_type eq 'User'"
    );
    let node = lower::<Ra>(&raw).expect("nested expression must be accepted");

    // Walk the tree and collect every literal the validator ended up with,
    // rather than asserting on one hard-coded tree shape: the parser's
    // associativity is not this module's contract.
    let mut found = Vec::new();
    leaves(&node, &mut found);
    found.sort();
    let mut expected = vec![
        format!("string:{ID}"),
        format!("uuid:{ID}"),
        "string:User".to_owned(),
    ];
    expected.sort();
    assert_eq!(found, expected);
}

/// Substring predicates on `principal_id` are the reason it stays a
/// `String` field, so they have to keep working — this is the assertion that
/// fails if someone "fixes" the asymmetry by retyping the field.
#[test]
fn substring_functions_on_principal_id_still_work() {
    for (raw, op) in [
        ("contains(principal_id, 'part')", FilterOp::Contains),
        ("startswith(principal_id, 'part')", FilterOp::StartsWith),
        ("endswith(principal_id, 'part')", FilterOp::EndsWith),
    ] {
        let node = lower::<Ra>(raw).unwrap_or_else(|e| panic!("{raw} must be accepted: {e}"));
        assert_eq!(
            binary(&node),
            (Ra::PrincipalId, op, "string:part".to_owned()),
            "for {raw}"
        );
    }
}

/// A bare UUID inside a substring function is coerced to text too, so
/// `startswith(principal_id, <bare-uuid>)` does not fail as an
/// unsupported function.
#[test]
fn a_bare_uuid_inside_a_substring_function_becomes_text() {
    let node = lower::<Ra>(&format!("startswith(principal_id, {ID})"))
        .expect("a bare uuid argument must be accepted");
    assert_eq!(
        binary(&node),
        (
            Ra::PrincipalId,
            FilterOp::StartsWith,
            format!("string:{ID}")
        )
    );
}

// ---------------------------------------------------------------------------
// What must still be rejected
// ---------------------------------------------------------------------------

/// A string that is not a UUID on a `uuid` field stays a type mismatch. The
/// alternative — passing it through as text — is precisely the 500 this
/// design avoids: Postgres answers `operator does not exist: uuid = text`.
#[test]
fn a_non_uuid_string_on_a_uuid_field_is_still_rejected() {
    for raw in [
        "role_definition_id eq 'not-a-uuid'",
        "id eq 'abc'",
        "role_definition_id in ('not-a-uuid')",
    ] {
        assert!(
            matches!(
                lower::<Ra>(raw),
                Err(FilterError::TypeMismatch { .. } | FilterError::InvalidExpression(_))
            ),
            "{raw} MUST NOT be normalized into a valid filter, got {:?}",
            lower::<Ra>(raw)
        );
    }
}

/// An unknown field is still an unknown field: the pass silently skips names
/// it cannot resolve so the diagnostic keeps coming from the one layer that
/// owns it.
#[test]
fn unknown_fields_are_left_for_the_validator_to_reject() {
    assert!(matches!(
        lower::<Ra>(&format!("principal_uuid eq {ID}")),
        Err(FilterError::UnknownField(_))
    ));
    assert!(matches!(
        lower::<Ra>("principal_name eq 'Ada'"),
        Err(FilterError::UnknownField(_))
    ));
}

/// A literal of a wholly unrelated type is not "helpfully" converted — only
/// the two id shapes are in scope, everything else keeps its 400.
#[test]
fn unrelated_type_mismatches_are_not_coerced_away() {
    assert!(matches!(
        lower::<Ra>("created_at eq 'yesterday'"),
        Err(FilterError::TypeMismatch { .. })
    ));
    assert!(matches!(
        lower::<Ra>("principal_id eq 42"),
        Err(FilterError::TypeMismatch { .. })
    ));
}

// ---------------------------------------------------------------------------
// The pass is generic, so the sibling endpoint gets it too
// ---------------------------------------------------------------------------

/// The role-definition list drives the same normalizer with its own field
/// enum, and needs no per-endpoint configuration to benefit: `id` and
/// `owner_tenant_id` are `uuid` columns and take the quoted spelling,
/// while `name` is text and takes a bare uuid.
#[test]
fn the_role_definition_field_enum_is_normalized_by_the_same_pass() {
    let node = lower::<Rd>(&format!("id eq '{ID}'")).expect("quoted uuid must be accepted");
    assert_eq!(binary(&node), (Rd::Id, FilterOp::Eq, format!("uuid:{ID}")));

    let node =
        lower::<Rd>(&format!("owner_tenant_id eq '{ID}'")).expect("quoted uuid must be accepted");
    assert_eq!(
        binary(&node),
        (Rd::OwnerTenantId, FilterOp::Eq, format!("uuid:{ID}"))
    );

    let node = lower::<Rd>(&format!("name eq {ID}")).expect("bare uuid must be accepted as text");
    assert_eq!(
        binary(&node),
        (Rd::Name, FilterOp::Eq, format!("string:{ID}"))
    );

    // A bool field is untouched, which is the guard that "generic over the
    // field enum" did not become "coerces anything into anything".
    let node = lower::<Rd>("is_built_in eq true").expect("bool filter must still work");
    match &node {
        FilterNode::Binary { field, value, .. } => {
            assert_eq!(*field, Rd::IsBuiltIn);
            assert!(matches!(value, ODataValue::Bool(true)));
        }
        other => panic!("expected a binary node, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// created_by (the newly filterable column)
// ---------------------------------------------------------------------------

/// `created_by` is a text column, so it behaves exactly like `principal_id`
/// — including accepting a bare uuid, which is the spelling a caller holding
/// a Keycloak subject id will reach for first.
#[test]
fn created_by_accepts_both_spellings_like_any_text_field() {
    let node = lower::<Ra>("created_by eq 'tester'").expect("quoted subject must be accepted");
    assert_eq!(
        binary(&node),
        (Ra::CreatedBy, FilterOp::Eq, "string:tester".to_owned())
    );

    let node = lower::<Ra>(&format!("created_by eq {ID}")).expect("bare uuid must be accepted");
    assert_eq!(
        binary(&node),
        (Ra::CreatedBy, FilterOp::Eq, format!("string:{ID}"))
    );
}
