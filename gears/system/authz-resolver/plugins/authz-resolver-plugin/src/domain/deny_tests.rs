use super::*;

#[test]
fn build_deny_response_shape() {
    let response = build_deny_response(
        error_codes::SCOPE_MISMATCH_V1,
        Some("test details".to_owned()),
    );

    assert!(!response.decision);
    assert!(
        response.context.constraints.is_empty(),
        "deny responses must carry empty constraints"
    );

    let deny_reason = response
        .context
        .deny_reason
        .expect("deny_reason must be populated");
    assert_eq!(deny_reason.error_code, error_codes::SCOPE_MISMATCH_V1);
    assert_eq!(deny_reason.details.as_deref(), Some("test details"));
}

#[test]
fn build_deny_response_accepts_none_details() {
    let response = build_deny_response(error_codes::SCOPE_MISMATCH_V1, None);

    let deny_reason = response.context.deny_reason.expect("populated");
    assert!(deny_reason.details.is_none());
}

#[test]
fn build_allow_response_shape() {
    use authz_resolver_sdk::constraints::{EqPredicate, InPredicate, Predicate};
    let constraint = Constraint {
        predicates: vec![Predicate::Eq(EqPredicate::new("foo", "bar"))],
    };
    let response = build_allow_response(vec![constraint]);

    assert!(response.decision);
    assert_eq!(response.context.constraints.len(), 1);
    assert!(response.context.deny_reason.is_none());

    // Empty-constraints shape is also valid (future "unconstrained allow").
    let empty = build_allow_response(vec![]);
    assert!(empty.decision);
    assert!(empty.context.constraints.is_empty());
    assert!(empty.context.deny_reason.is_none());

    // Multi-constraint shape carries them in order (OR-combined per SDK).
    let multi = build_allow_response(vec![
        Constraint {
            predicates: vec![Predicate::Eq(EqPredicate::new("a", 1_i64))],
        },
        Constraint {
            predicates: vec![Predicate::In(InPredicate::new("b", vec!["x".to_owned()]))],
        },
    ]);
    assert_eq!(multi.context.constraints.len(), 2);
}

/// Every deny code shares one namespace on the canonical error base, and each
/// one round-trips through `build_deny_response` unchanged.
///
/// This replaces five near-identical `build_deny_response_*_shape` tests. Each
/// passed one code in and asserted the same code came back, which added no
/// deny-shape behaviour beyond the first test — and, being written out per
/// code, a NEW code could be added without any of them noticing. A table over
/// the constants cannot be forgotten: adding a code without adding it here
/// leaves it unpinned in one visible place instead of five invisible ones.
#[test]
fn every_deny_code_stays_in_the_canonical_namespace_and_round_trips() {
    // The canonical Constructor Fabric error base plus the single
    // `cf.authz.errors` instance namespace. There is deliberately no
    // per-vendor split; a code drifting to another prefix fails here.
    const NAMESPACE: &str = "gts.cf.core.errors.err.v1~cf.authz.errors.";

    for (code, leaf) in [
        (error_codes::SCOPE_MISMATCH_V1, "scope_mismatch.v1"),
        (
            error_codes::INSUFFICIENT_PERMISSIONS_V1,
            "insufficient_permissions.v1",
        ),
        (
            error_codes::UNSUPPORTED_PROPERTY_V1,
            "unsupported_property.v1",
        ),
        (
            error_codes::EXPANSION_INFEASIBLE_V1,
            "expansion_infeasible.v1",
        ),
        (
            error_codes::UNKNOWN_RESOURCE_TYPE_V1,
            "unknown_resource_type.v1",
        ),
        (
            error_codes::CONSTRAINTS_UNAVAILABLE_V1,
            "constraints_unavailable.v1",
        ),
        (error_codes::INVALID_REQUEST_V1, "invalid_request.v1"),
    ] {
        assert_eq!(
            code,
            format!("{NAMESPACE}{leaf}"),
            "deny codes must stay on the canonical error base in the \
             cf.authz.errors namespace, and name the leaf they claim to"
        );

        let response = build_deny_response(code, Some(format!("detail for {leaf}")));
        assert!(!response.decision, "{code} must build a deny");
        assert!(
            response.context.constraints.is_empty(),
            "{code}: a deny carries no constraints"
        );
        let deny_reason = response
            .context
            .deny_reason
            .unwrap_or_else(|| panic!("{code}: deny_reason must be populated"));
        assert_eq!(deny_reason.error_code, code);
        assert_eq!(
            deny_reason.details.as_deref(),
            Some(format!("detail for {leaf}").as_str())
        );
    }
}
