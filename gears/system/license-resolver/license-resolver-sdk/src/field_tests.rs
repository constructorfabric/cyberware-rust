//! Unit tests for the violation `field` / `reason` vocabulary.

use super::{
    CONTRACT_ABSTRACT, CONTRACT_NOT_DERIVED, CONTRACT_NOT_REGISTERED, CONTRACT_TYPE_MALFORMED,
    RESOURCE_FIELD, RESOURCE_TYPE_FIELD, SCHEMA_MISMATCH, SUBJECT_FIELD, SUBJECT_NOT_ADMITTED,
    SUBJECT_TYPE_FIELD, ValidationReason,
};

#[test]
fn validation_reason_round_trips_each_constant() {
    let cases = vec![
        (
            CONTRACT_NOT_REGISTERED,
            ValidationReason::ContractNotRegistered,
        ),
        (
            CONTRACT_TYPE_MALFORMED,
            ValidationReason::ContractTypeMalformed,
        ),
        (CONTRACT_NOT_DERIVED, ValidationReason::ContractNotDerived),
        (CONTRACT_ABSTRACT, ValidationReason::ContractAbstract),
        (SCHEMA_MISMATCH, ValidationReason::SchemaMismatch),
        (SUBJECT_NOT_ADMITTED, ValidationReason::SubjectNotAdmitted),
    ];
    for (wire, expected) in cases {
        assert_eq!(
            ValidationReason::from_wire(wire),
            expected,
            "from_wire({wire})"
        );
        assert_eq!(expected.as_wire(), wire, "as_wire round-trip for {wire}");
        assert_eq!(expected.to_string(), wire, "Display renders the wire code");
    }
}

#[test]
fn validation_reason_preserves_unknown_wire_string() {
    let raw = "CONTRACT_EXPIRED";
    let reason = ValidationReason::from_wire(raw);
    assert_eq!(reason, ValidationReason::Unknown(raw.to_owned()));
    assert_eq!(reason.as_wire(), raw);
}

/// The composed contract-type `field` values must stay in step with the roots
/// they extend, so a consumer can reach either through the other.
#[test]
fn contract_type_fields_extend_their_role_roots() {
    assert_eq!(SUBJECT_TYPE_FIELD, format!("{SUBJECT_FIELD}/type"));
    assert_eq!(RESOURCE_TYPE_FIELD, format!("{RESOURCE_FIELD}/type"));
}
