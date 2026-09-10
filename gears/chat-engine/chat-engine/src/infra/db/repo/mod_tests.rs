//! Unit tests for the shared repo helpers.

use super::*;

use crate::domain::error::ChatEngineError;

/// A well-formed owner id parses through unchanged — the predicate the legacy
/// owner-filtered repo methods are built from.
#[test]
fn parse_owner_uuid_accepts_a_uuid() {
    let id = uuid::Uuid::new_v4();
    let parsed = parse_owner_uuid(&id.to_string(), "tenant_id").expect("a UUID string must parse");
    assert_eq!(parsed, id);
}

/// A corrupt owner id must fail closed. The alternative — defaulting to
/// `Uuid::nil()` — would build a predicate that matches whatever rows happen
/// to carry the nil owner, turning a data-integrity fault into a scoping one.
#[test]
fn parse_owner_uuid_fails_closed_on_a_non_uuid() {
    let err = parse_owner_uuid("not-a-uuid", "tenant_id")
        .expect_err("a non-UUID owner id must not reach the query");
    assert!(
        matches!(err, ChatEngineError::Internal { .. }),
        "a corrupt owner id is an invariant violation, not user input: {err:?}",
    );
    assert!(
        err.to_string().contains("tenant_id"),
        "the operator-facing reason must name the offending field: {err}",
    );
}

/// The field name is threaded through so a report distinguishes the two halves
/// of the owner pair.
#[test]
fn parse_owner_uuid_names_the_field_it_was_given() {
    let err = parse_owner_uuid("", "user_id").expect_err("an empty owner id must be rejected");
    assert!(err.to_string().contains("user_id"), "{err}");
}
