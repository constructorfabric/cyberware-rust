#![allow(clippy::panic)] // explicit panic in match fallbacks is appropriate in tests

use rbac_sdk::models::{PermissionRule, RolePolicy};

use super::{
    MAX_OPERATION_LEN, MAX_TARGET_TYPE_LEN, PermissionMatchResult, PermissionRuleField,
    ValidationError, is_permission_allowed, matches_operation, matches_target_type,
    validate_permission_rule,
};

/// Construct a [`PermissionRule`] from two string literals.
fn perm(operation: &str, target_type: &str) -> PermissionRule {
    PermissionRule::new(operation, target_type)
}

/// Construct a [`RolePolicy`] with the given allow / deny lists.
fn role(allow: Vec<(&str, &str)>, deny: Vec<(&str, &str)>) -> RolePolicy {
    let permissions = allow
        .into_iter()
        .map(|(op, tt)| PermissionRule::new(op, tt))
        .collect();
    let not_permissions = deny
        .into_iter()
        .map(|(op, tt)| PermissionRule::new(op, tt))
        .collect();
    RolePolicy::new(permissions, not_permissions)
}

// -----------------------------------------------------------------------
// U-27: exact operation match
// -----------------------------------------------------------------------

/// U-27: identical operation strings must match.
#[test]
fn matches_operation_exact_match() {
    assert!(matches_operation("read", "read"));
}

// -----------------------------------------------------------------------
// U-28: global wildcard matches any operation
// -----------------------------------------------------------------------

/// U-28: a rule operation of `"*"` must match any requested operation.
#[test]
fn matches_operation_wildcard_grants_all() {
    assert!(matches_operation("*", "delete"));
}

// -----------------------------------------------------------------------
// U-29: different operations must not match
// -----------------------------------------------------------------------

/// U-29: distinct operation strings must not match.
#[test]
fn matches_operation_different_ops_no_match() {
    assert!(!matches_operation("read", "write"));
}

// -----------------------------------------------------------------------
// Case sensitivity
// -----------------------------------------------------------------------

/// Matching is byte-for-byte (case-sensitive): `"read"` ≠ `"Read"`.
#[test]
fn matches_operation_case_sensitive() {
    assert!(!matches_operation("read", "Read"));
}

// -----------------------------------------------------------------------
// No partial wildcard forms
// -----------------------------------------------------------------------

/// A suffix-asterisk rule such as `"read*"` is treated as the literal
/// string `"read*"` — there are no partial wildcard forms.
#[test]
fn matches_operation_partial_wildcard_treated_as_literal() {
    assert!(!matches_operation("read*", "read_all"));
}

/// A rule that happens to be `"read*"` must equal the requested string
/// `"read*"` byte-for-byte (self-equality of the literal).
#[test]
fn matches_operation_literal_suffix_asterisk_self_equality() {
    assert!(matches_operation("read*", "read*"));
}

// -----------------------------------------------------------------------
// Empty-string edge cases for matches_operation
// -----------------------------------------------------------------------

/// An empty rule operation matches only an empty requested operation
/// (exact equality, not a wildcard).
#[test]
fn matches_operation_empty_rule_op_exact_equality_only() {
    assert!(matches_operation("", ""));
    assert!(!matches_operation("", "read"));
}

// -----------------------------------------------------------------------
// U-30: exact GTS target-type match
// -----------------------------------------------------------------------

/// U-30: a rule target equal to the requested GTS type must match.
#[test]
fn matches_target_type_exact_gts_match() {
    assert!(matches_target_type(
        "gts.cf.resources.compute.vm.v1~",
        "gts.cf.resources.compute.vm.v1~",
    ));
}

// -----------------------------------------------------------------------
// U-31: GTS OP#4 family wildcard match
// -----------------------------------------------------------------------

/// U-31: a `prefix.*` rule must match any type under that GTS namespace.
#[test]
fn matches_target_type_family_wildcard_match() {
    assert!(matches_target_type(
        "gts.cf.resources.compute.*",
        "gts.cf.resources.compute.vm.v1~",
    ));
}

// -----------------------------------------------------------------------
// U-32: GTS OP#4 family wildcard cross-family rejection
// -----------------------------------------------------------------------

/// U-32: a `prefix.*` rule must NOT match a type in a sibling namespace.
#[test]
fn matches_target_type_cross_family_rejection() {
    assert!(!matches_target_type(
        "gts.cf.resources.compute.*",
        "gts.cf.resources.network.vnet.v1~",
    ));
}

// -----------------------------------------------------------------------
// Bare `*` for target_type is invalid shape and never matches at runtime
// -----------------------------------------------------------------------

/// Bare `"*"` is not a valid `target_type` per the JSON Schema and the
/// matcher returns `false` for every requested type. Use `gts.*` or
/// `gts.cf.*` for namespace-wide grants.
#[test]
fn matches_target_type_bare_star_never_matches() {
    assert!(!matches_target_type(
        "*",
        "gts.cf.resources.network.vnet.v1~"
    ));
    assert!(!matches_target_type("*", "gts.cf.resources.compute.vm.v1~"));
    assert!(!matches_target_type("*", ""));
}

/// "Every GTS type" is expressed as the family wildcard `gts.*` (which is
/// a valid `target_type` shape per the schema).
#[test]
fn matches_target_type_gts_family_wildcard_matches_any_gts_type() {
    assert!(matches_target_type(
        "gts.*",
        "gts.cf.resources.network.vnet.v1~"
    ));
    assert!(matches_target_type(
        "gts.*",
        "gts.cf.resources.compute.vm.v1~"
    ));
    assert!(!matches_target_type("gts.*", ""));
}

#[test]
fn matches_target_type_family_wildcard_rejects_empty_requested_type() {
    assert!(!matches_target_type("gts.cf.resources.*", ""));
}

// -----------------------------------------------------------------------
// Broader family wildcard (shallower prefix)
// -----------------------------------------------------------------------

/// A shallower wildcard (`gts.cf.resources.*`) must match types nested
/// several levels deeper within that GTS namespace.
#[test]
fn matches_target_type_broader_family_wildcard() {
    assert!(matches_target_type(
        "gts.cf.resources.*",
        "gts.cf.resources.compute.vm.v1~",
    ));
}

// -----------------------------------------------------------------------
// Dotted-prefix boundary regression guard
// -----------------------------------------------------------------------

/// `"compute.*"` (prefix `"compute."`) must NOT match `"computer.vm.v1~"`.
/// The retained trailing dot acts as a namespace separator; dropping it
/// would silently grant access across namespace boundaries.
#[test]
fn matches_target_type_dotted_prefix_boundary_guard() {
    assert!(!matches_target_type("compute.*", "computer.vm.v1~"));
}

// -----------------------------------------------------------------------
// Degenerate `".*"` rule target — pins non-regex semantics
// -----------------------------------------------------------------------

/// Rule `".*"` is a family wildcard with prefix `"."` (literal-dot
/// types). NOT a regex `.*`. Locks in the non-regex semantics.
#[test]
fn matches_target_type_degenerate_dot_star_rule() {
    assert!(!matches_target_type(
        ".*",
        "gts.cf.resources.compute.vm.v1~"
    ));
    assert!(matches_target_type(".*", ".leading-dot-type"));
    assert!(!matches_target_type(".*", ""));
}

// -----------------------------------------------------------------------
// U-4: empty operation rejected
// -----------------------------------------------------------------------

/// U-4: an empty `operation` must produce `EmptyField { Operation }`.
#[test]
fn validate_permission_rule_empty_operation() {
    assert_eq!(
        validate_permission_rule(&perm("", "gts.cf.resources.compute.vm.v1~")),
        Err(ValidationError::EmptyField {
            field: PermissionRuleField::Operation,
        }),
    );
}

// -----------------------------------------------------------------------
// U-5: empty target_type rejected
// -----------------------------------------------------------------------

/// U-5: an empty `target_type` must produce `EmptyField { TargetType }`.
#[test]
fn validate_permission_rule_empty_target_type() {
    assert_eq!(
        validate_permission_rule(&perm("read", "")),
        Err(ValidationError::EmptyField {
            field: PermissionRuleField::TargetType,
        }),
    );
}

// -----------------------------------------------------------------------
// Wildcard operation + family-wildcard target_type accepted
// -----------------------------------------------------------------------

/// `("*", "gts.cf.*")` is the canonical shape for a "do anything to any
/// CF resource" rule (used by the platform's built-in `Owner` role) and
/// MUST validate `Ok(())`.
#[test]
fn validate_permission_rule_wildcard_op_with_family_target_accepted() {
    assert_eq!(validate_permission_rule(&perm("*", "gts.cf.*")), Ok(()),);
}

/// The schema regex requires at least one identifier segment between
/// the `gts.` prefix and the trailing `.v<N>~` / `.*` token. Broadest
/// valid family wildcard is `gts.<seg>.*` (e.g. `gts.cf.*`); bare
/// `gts.*` is invalid.
#[test]
fn validate_permission_rule_broadest_family_wildcard_accepted() {
    assert_eq!(validate_permission_rule(&perm("*", "gts.cf.*")), Ok(()));
    assert_eq!(validate_permission_rule(&perm("*", "gts.os.*")), Ok(()));
}

/// `gts.*` (zero head segments) is rejected as `InvalidShape` — the
/// schema regex requires at least one segment between `gts.` and the
/// trailing wildcard / version token.
#[test]
fn validate_permission_rule_rejects_bare_gts_wildcard() {
    assert_eq!(
        validate_permission_rule(&perm("read", "gts.*")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        }),
    );
}

// -----------------------------------------------------------------------
// Concrete GTS rule accepted
// -----------------------------------------------------------------------

/// A valid concrete rule must validate `Ok(())`.
#[test]
fn validate_permission_rule_concrete_gts_accepted() {
    assert_eq!(
        validate_permission_rule(&perm("read", "gts.cf.resources.compute.vm.v1~")),
        Ok(()),
    );
}

/// A chained per-type authz id (`umbrella~rtd`) is a canonical GTS type id and
/// MUST validate: the shape check delegates to the GTS parser, which accepts a
/// mid-string `~`.
#[test]
fn validate_permission_rule_accepts_chained_type_id() {
    assert_eq!(
        validate_permission_rule(&perm(
            "read",
            "gts.cf.resources.rms.resource.v1~x.storage.s3.bucket.v1~"
        )),
        Ok(()),
    );
}

/// The `_` namespace placeholder is a valid GTS segment, so an RTD-style id
/// using it MUST validate even though every other segment is `[a-z]`-leading.
#[test]
fn validate_permission_rule_accepts_underscore_namespace() {
    assert_eq!(
        validate_permission_rule(&perm("read", "gts.vhp.rms._.resource.v1~")),
        Ok(()),
    );
}

/// Guard against the gts parser's stricter 5-token shape silently rejecting a
/// `target_type` the platform actually uses: every in-tree concrete authz
/// label must validate.
#[test]
fn validate_permission_rule_accepts_in_tree_concrete_labels() {
    for target in [
        "gts.cf.core.rbac.role_definition.v1~",
        "gts.cf.toolkit.authz.permission.v1~",
        "gts.cf.resources.rms.deployment.v1~",
    ] {
        assert_eq!(
            validate_permission_rule(&perm("read", target)),
            Ok(()),
            "in-tree concrete label must validate: {target}"
        );
    }
}

/// The built-in family wildcard MUST match a chained per-type id by prefix —
/// this is what lets Reader/Contributor/Owner authorize every per-type RMS
/// resource through one rule. (Matcher unchanged; pins the per-type contract.)
#[test]
fn matches_target_type_family_wildcard_matches_chained() {
    assert!(matches_target_type(
        "gts.cf.resources.*",
        "gts.cf.resources.rms.resource.v1~x.storage.s3.bucket.v1~"
    ));
}

// Schema-shape rejections — schema / validator / matcher MUST agree.

/// Bare `"*"` is invalid for `target_type` and MUST be rejected as
/// `InvalidShape`.
#[test]
fn validate_permission_rule_rejects_bare_star_target_type() {
    assert_eq!(
        validate_permission_rule(&perm("read", "*")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        }),
    );
}

/// `"target_type"` values without the canonical `gts.` prefix are
/// rejected as `InvalidShape`.
#[test]
fn validate_permission_rule_rejects_target_type_without_gts_prefix() {
    assert_eq!(
        validate_permission_rule(&perm("read", "vm")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        }),
    );
    assert_eq!(
        validate_permission_rule(&perm("read", "cf.resources.compute.vm.v1~")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        }),
    );
}

/// `target_type` MUST end with either `.v<N>~` (where `<N>` is one or
/// more ASCII digits) or `.*` (the family wildcard); anything else is
/// `InvalidShape`.
#[test]
fn validate_permission_rule_rejects_malformed_target_type_tail() {
    // No version suffix, no wildcard.
    assert_eq!(
        validate_permission_rule(&perm("read", "gts.cf.resources.compute.vm")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        }),
    );
    // Wrong tilde / version shape.
    assert_eq!(
        validate_permission_rule(&perm("read", "gts.cf.resources.compute.vm.v~")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        }),
    );
    // Missing trailing `~` on the version segment.
    assert_eq!(
        validate_permission_rule(&perm("read", "gts.cf.resources.compute.vm.v1")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::TargetType,
        }),
    );
}

/// Uppercase characters in `operation` are rejected — the JSON Schema
/// regex is `^(\*|[a-z][a-z0-9_]*)$`, so `Read`, `READ`, etc. all
/// produce `InvalidShape`.
#[test]
fn validate_permission_rule_rejects_uppercase_operation() {
    assert_eq!(
        validate_permission_rule(&perm("Read", "gts.cf.resources.compute.vm.v1~")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::Operation,
        }),
    );
    assert_eq!(
        validate_permission_rule(&perm("READ", "gts.cf.resources.compute.vm.v1~")),
        Err(ValidationError::InvalidShape {
            field: PermissionRuleField::Operation,
        }),
    );
}

/// `operation` MUST start with a lowercase letter — leading digit /
/// underscore / dash are all `InvalidShape`.
#[test]
fn validate_permission_rule_rejects_operation_with_bad_leading_char() {
    for bad in ["1read", "_read", "-read", " read"] {
        assert_eq!(
            validate_permission_rule(&perm(bad, "gts.cf.resources.compute.vm.v1~")),
            Err(ValidationError::InvalidShape {
                field: PermissionRuleField::Operation,
            }),
            "operation `{bad}` MUST be rejected as InvalidShape",
        );
    }
}

// -----------------------------------------------------------------------
// Operation length bound
// -----------------------------------------------------------------------

/// An operation of `MAX_OPERATION_LEN + 1` bytes must be rejected with
/// `FieldTooLong { Operation, max: MAX_OPERATION_LEN, actual }`.
#[test]
fn validate_permission_rule_operation_too_long() {
    let long_op = "a".repeat(MAX_OPERATION_LEN + 1);
    assert_eq!(
        validate_permission_rule(&perm(&long_op, "gts.cf.resources.compute.vm.v1~")),
        Err(ValidationError::FieldTooLong {
            field: PermissionRuleField::Operation,
            max: MAX_OPERATION_LEN,
            actual: MAX_OPERATION_LEN + 1,
        }),
    );
}

// -----------------------------------------------------------------------
// Target type length bound
// -----------------------------------------------------------------------

/// A `target_type` of `MAX_TARGET_TYPE_LEN + 1` bytes must be rejected
/// with `FieldTooLong { TargetType, max: MAX_TARGET_TYPE_LEN, actual }`.
#[test]
fn validate_permission_rule_target_type_too_long() {
    let long_tt = "a".repeat(MAX_TARGET_TYPE_LEN + 1);
    assert_eq!(
        validate_permission_rule(&perm("read", &long_tt)),
        Err(ValidationError::FieldTooLong {
            field: PermissionRuleField::TargetType,
            max: MAX_TARGET_TYPE_LEN,
            actual: MAX_TARGET_TYPE_LEN + 1,
        }),
    );
}

// -----------------------------------------------------------------------
// Boundary acceptance (exactly at the limit, with valid schema shape)
// -----------------------------------------------------------------------

/// Bounds are inclusive: rules whose `operation` and `target_type` are
/// exactly at the byte limit and shape-valid must validate `Ok(())`.
#[test]
fn validate_permission_rule_boundary_acceptance() {
    let op_at_limit = "a".repeat(MAX_OPERATION_LEN);
    // Byte-counting helpers, not GTS identifiers — DE0901's
    // `starts_with("gts.")` exemption does not extend to length
    // arithmetic, so suppress on this binding.
    #[allow(unknown_lints, de0901_gts_string_pattern)]
    let segment_len = MAX_TARGET_TYPE_LEN - "gts.".len() - ".*".len();
    let tt_at_limit = format!("gts.{}.*", "a".repeat(segment_len));
    assert_eq!(tt_at_limit.len(), MAX_TARGET_TYPE_LEN);
    assert_eq!(
        validate_permission_rule(&perm(&op_at_limit, &tt_at_limit)),
        Ok(()),
    );
}

// Convenience literals — schema-valid shapes used by the matcher tests
// below.
const VM_TYPE: &str = "gts.cf.resources.compute.vm.v1~";
const CF_FAMILY_WILDCARD: &str = "gts.cf.*";

// -----------------------------------------------------------------------
// U-33: not_permissions overrides an identical permissions grant
// -----------------------------------------------------------------------

/// U-33: when the same rule appears in both `permissions` and
/// `not_permissions`, the exclusion wins. Asserts the returned
/// `matching_rule` is the deny rule that fired.
#[test]
fn is_permission_allowed_exclusion_overrides_grant() {
    let r = role(vec![("read", VM_TYPE)], vec![("read", VM_TYPE)]);
    match is_permission_allowed(&r, "read", VM_TYPE) {
        PermissionMatchResult::ExcludedByNotPermission { matching_rule } => {
            assert_eq!(matching_rule.operation, "read");
            assert_eq!(matching_rule.target_type, VM_TYPE);
        }
        other => panic!("expected ExcludedByNotPermission, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// U-34: non-excluded grant is allowed
// -----------------------------------------------------------------------

/// U-34: when only one operation in `permissions` is excluded, the other
/// is still allowed.
#[test]
fn is_permission_allowed_non_excluded_grant_allowed() {
    let r = role(
        vec![("read", VM_TYPE), ("write", VM_TYPE)],
        vec![("write", VM_TYPE)],
    );
    assert!(matches!(
        is_permission_allowed(&r, "read", VM_TYPE),
        PermissionMatchResult::Allowed { .. },
    ));
}

// -----------------------------------------------------------------------
// U-35: no matching grant → NoMatch
// -----------------------------------------------------------------------

/// U-35: a request that does not match any `permissions` rule and has no
/// `not_permissions` match returns `NoMatch`.
#[test]
fn is_permission_allowed_no_match() {
    let r = role(vec![("read", VM_TYPE)], vec![]);
    assert_eq!(
        is_permission_allowed(&r, "write", VM_TYPE),
        PermissionMatchResult::NoMatch,
    );
}

// -----------------------------------------------------------------------
// Empty role → NoMatch
// -----------------------------------------------------------------------

/// A role with no `permissions` and no `not_permissions` is silent on
/// every request and must return `NoMatch`.
#[test]
fn is_permission_allowed_empty_role_returns_no_match() {
    let r = role(vec![], vec![]);
    assert_eq!(
        is_permission_allowed(&r, "read", VM_TYPE),
        PermissionMatchResult::NoMatch,
    );
}

// -----------------------------------------------------------------------
// All-deny role (broad family wildcard for both grants and exclusions)
// -----------------------------------------------------------------------

/// A role whose `permissions` and `not_permissions` both wildcard CF
/// types must exclude every CF-type request — the deny pass runs first.
#[test]
fn is_permission_allowed_all_deny_role() {
    let r = role(
        vec![("*", CF_FAMILY_WILDCARD)],
        vec![("*", CF_FAMILY_WILDCARD)],
    );
    assert!(matches!(
        is_permission_allowed(&r, "read", VM_TYPE),
        PermissionMatchResult::ExcludedByNotPermission { .. },
    ));
}

// -----------------------------------------------------------------------
// Wildcard grant with targeted exclusion
// -----------------------------------------------------------------------

/// A `("*", "gts.cf.*")` grant with a targeted `not_permissions` entry
/// must produce `ExcludedByNotPermission` for the excluded operation on
/// the excluded type.
#[test]
fn is_permission_allowed_wildcard_grant_targeted_exclusion() {
    let r = role(vec![("*", CF_FAMILY_WILDCARD)], vec![("delete", VM_TYPE)]);
    assert!(matches!(
        is_permission_allowed(&r, "delete", VM_TYPE),
        PermissionMatchResult::ExcludedByNotPermission { .. },
    ));
}

// -----------------------------------------------------------------------
// Wildcard grant for non-excluded operation
// -----------------------------------------------------------------------

/// Same role: a non-excluded operation under the family-wildcard grant
/// returns `Allowed`.
#[test]
fn is_permission_allowed_wildcard_grant_non_excluded_op_allowed() {
    let r = role(vec![("*", CF_FAMILY_WILDCARD)], vec![("delete", VM_TYPE)]);
    assert!(matches!(
        is_permission_allowed(&r, "read", VM_TYPE),
        PermissionMatchResult::Allowed { .. },
    ));
}

// -----------------------------------------------------------------------
// matching_rule payload fidelity
// -----------------------------------------------------------------------

/// The `matching_rule` inside `Allowed` and `ExcludedByNotPermission` must
/// be the originating `PermissionRule` — `operation` and `target_type` must
/// equal the rule that triggered the match (design.md Decision 2 payload
/// contract).
#[test]
fn is_permission_allowed_matching_rule_payload_fidelity() {
    // Allowed payload
    let r_allow = role(vec![("read", "gts.cf.resources.compute.vm.v1~")], vec![]);
    match is_permission_allowed(&r_allow, "read", "gts.cf.resources.compute.vm.v1~") {
        PermissionMatchResult::Allowed { matching_rule } => {
            assert_eq!(matching_rule.operation, "read");
            assert_eq!(matching_rule.target_type, "gts.cf.resources.compute.vm.v1~");
        }
        other => panic!("expected Allowed, got {other:?}"),
    }

    // ExcludedByNotPermission payload
    let r_excl = role(
        vec![("read", "gts.cf.resources.compute.vm.v1~")],
        vec![("read", "gts.cf.resources.compute.vm.v1~")],
    );
    match is_permission_allowed(&r_excl, "read", "gts.cf.resources.compute.vm.v1~") {
        PermissionMatchResult::ExcludedByNotPermission { matching_rule } => {
            assert_eq!(matching_rule.operation, "read");
            assert_eq!(matching_rule.target_type, "gts.cf.resources.compute.vm.v1~");
        }
        other => panic!("expected ExcludedByNotPermission, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// First-match-wins ordering
// -----------------------------------------------------------------------

/// When multiple `permissions` rules match, the first one in list order
/// is the one returned as `matching_rule`.
#[test]
fn is_permission_allowed_permissions_first_match_wins() {
    // The broader family wildcard comes first; the exact rule comes second.
    let r = role(
        vec![
            ("read", "gts.cf.resources.*"),
            ("read", "gts.cf.resources.compute.vm.v1~"),
        ],
        vec![],
    );
    match is_permission_allowed(&r, "read", "gts.cf.resources.compute.vm.v1~") {
        PermissionMatchResult::Allowed { matching_rule } => {
            assert_eq!(matching_rule.target_type, "gts.cf.resources.*");
        }
        other => panic!("expected Allowed, got {other:?}"),
    }
}

/// When multiple `not_permissions` rules match, the first one in list
/// order is the one returned as `matching_rule`.
#[test]
fn is_permission_allowed_not_permissions_first_match_wins() {
    const STORAGE_TYPE: &str = "gts.cf.resources.storage.disk.v1~";
    let r = role(
        vec![("*", CF_FAMILY_WILDCARD)],
        vec![("read", VM_TYPE), ("read", STORAGE_TYPE)],
    );
    match is_permission_allowed(&r, "read", VM_TYPE) {
        PermissionMatchResult::ExcludedByNotPermission { matching_rule } => {
            assert_eq!(matching_rule.target_type, VM_TYPE);
        }
        other => panic!("expected ExcludedByNotPermission, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Intra-role evaluation isolation
// -----------------------------------------------------------------------

/// `is_permission_allowed` evaluates one role at a time. Cross-role
/// union is the Permission Evaluator's job; this test pins the
/// boundary.
#[test]
fn is_permission_allowed_intra_role_isolation() {
    let role_a = role(vec![("*", CF_FAMILY_WILDCARD)], vec![("delete", VM_TYPE)]);
    let role_b = role(vec![("delete", VM_TYPE)], vec![]);

    assert!(matches!(
        is_permission_allowed(&role_a, "delete", VM_TYPE),
        PermissionMatchResult::ExcludedByNotPermission { .. },
    ));
    assert!(matches!(
        is_permission_allowed(&role_b, "delete", VM_TYPE),
        PermissionMatchResult::Allowed { .. },
    ));
}
