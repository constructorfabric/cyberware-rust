//! Unit tests for [`PluginError`] — classification and SDK projection.

use authz_resolver_sdk::AuthZResolverError as Sdk;

use super::PluginError;
use crate::domain::deny::{service_errors, validation_messages as vm};
use crate::domain::metrics_port::{ErrorType, FailClosedReason};

/// One instance of every variant.
///
/// The point of the typed error is that a new failure mode cannot ship
/// unclassified, so the tests below iterate this list rather than spot-checking:
/// add a variant and `every_variant_is_covered_here` fails until it is listed.
fn all_variants() -> Vec<PluginError> {
    vec![
        PluginError::UnknownSubjectType {
            value: "gts.cf.core.iam.robot.v1~".to_owned(),
        },
        PluginError::InvalidOperationEmpty,
        PluginError::InvalidOperationWildcard,
        PluginError::MissingResourceType,
        PluginError::UnreadableSubjectTenant {
            detail: "not a string".to_owned(),
        },
        PluginError::RbacUnavailable,
        PluginError::TenantResolverUnavailable,
        PluginError::ResourceGroupUnavailable,
        PluginError::GtsRegistryUnavailable,
        PluginError::TenantNotFound,
        PluginError::ResourceGroupNotFound,
        PluginError::RbacScopeProvenanceInvalid,
        PluginError::internal("cache returned a non-TenantSubtree value"),
    ]
}

/// Guards `all_variants()` against a silently added variant. `labels()` is
/// exhaustive so the compiler already forces a classification, but nothing
/// would force the new variant into these tests.
#[test]
fn every_variant_is_covered_here() {
    assert_eq!(
        all_variants().len(),
        13,
        "a PluginError variant was added or removed: extend all_variants() and \
         the per-variant assertions below, then update this count"
    );
}

/// The whole point of the type: a client fault must never bump the fail-closed
/// counter, and a system fault must always bump it.
#[test]
fn client_faults_are_invalid_request_and_never_fail_closed() {
    let client_faults = [
        PluginError::UnknownSubjectType {
            value: "x".to_owned(),
        },
        PluginError::InvalidOperationEmpty,
        PluginError::InvalidOperationWildcard,
        PluginError::MissingResourceType,
        PluginError::UnreadableSubjectTenant {
            detail: "not a UUID".to_owned(),
        },
    ];
    for err in client_faults {
        assert_eq!(
            err.labels(),
            (ErrorType::InvalidRequest, None),
            "{err} must classify as a client fault with no fail-closed reason"
        );
    }
}

#[test]
fn every_non_client_fault_is_fail_closed() {
    let client_labels = (ErrorType::InvalidRequest, None);
    for err in all_variants() {
        let (error_type, fail_closed) = err.labels();
        if (error_type, fail_closed) == client_labels {
            continue;
        }
        assert!(
            fail_closed.is_some(),
            "{err} denies access, so it must also bump the fail-closed counter"
        );
    }
}

/// A deterministic scope-resolution failure must NOT read as a transient
/// outage: routing it through `resolver_timeout` pages on-call for a phantom
/// outage whenever a stale grant names a deleted tenant or group.
#[test]
fn deterministic_scope_failures_are_not_labelled_as_timeouts() {
    assert_eq!(
        PluginError::TenantNotFound.labels(),
        (
            ErrorType::TenantResolverNotFound,
            Some(FailClosedReason::ScopeUnresolvable)
        )
    );
    assert_eq!(
        PluginError::ResourceGroupNotFound.labels(),
        (
            ErrorType::RgResolverNotFound,
            Some(FailClosedReason::ScopeUnresolvable)
        )
    );
}

#[test]
fn dependency_outages_carry_their_own_labels() {
    assert_eq!(
        PluginError::RbacUnavailable.labels(),
        (
            ErrorType::RbacUnavailable,
            Some(FailClosedReason::RbacUnavailable)
        )
    );
    assert_eq!(
        PluginError::TenantResolverUnavailable.labels(),
        (
            ErrorType::TenantResolverTimeout,
            Some(FailClosedReason::ResolverTimeout)
        )
    );
    assert_eq!(
        PluginError::ResourceGroupUnavailable.labels(),
        (
            ErrorType::RgResolverTimeout,
            Some(FailClosedReason::ResolverTimeout)
        )
    );
    // Its own pair, not the resolver catch-all — a registry outage must not
    // page on-call for a resolver.
    assert_eq!(
        PluginError::GtsRegistryUnavailable.labels(),
        (
            ErrorType::GtsRegistryUnavailable,
            Some(FailClosedReason::GtsRegistryUnavailable)
        )
    );
}

#[test]
fn provenance_drift_and_unexpected_are_distinct() {
    assert_eq!(
        PluginError::RbacScopeProvenanceInvalid.labels(),
        (
            ErrorType::RbacScopeProvenanceInvalid,
            Some(FailClosedReason::RbacScopeProvenanceInvalid)
        )
    );
    assert_eq!(
        PluginError::internal("cache invariant").labels(),
        (
            ErrorType::Unexpected,
            Some(FailClosedReason::AllConstraintsFailed)
        )
    );
}

/// The message strings are a contract: integration tests and log dashboards pin
/// them. Typing the error must not have reworded anything.
#[test]
fn display_reproduces_the_canonical_message_strings() {
    let cases: Vec<(PluginError, String)> = vec![
        (
            PluginError::UnknownSubjectType {
                value: "robot".to_owned(),
            },
            format!("{} robot", vm::UNKNOWN_SUBJECT_TYPE_PREFIX),
        ),
        (
            PluginError::InvalidOperationEmpty,
            vm::INVALID_OPERATION_EMPTY.to_owned(),
        ),
        (
            PluginError::InvalidOperationWildcard,
            vm::INVALID_OPERATION_WILDCARD.to_owned(),
        ),
        (
            PluginError::MissingResourceType,
            vm::MISSING_RESOURCE_TYPE.to_owned(),
        ),
        (
            PluginError::UnreadableSubjectTenant {
                detail: "not a string".to_owned(),
            },
            format!("{} not a string", vm::UNREADABLE_SUBJECT_TENANT_PREFIX),
        ),
        (
            PluginError::RbacUnavailable,
            service_errors::RBAC_UNAVAILABLE.to_owned(),
        ),
        (
            PluginError::TenantResolverUnavailable,
            service_errors::TENANT_RESOLVER_UNAVAILABLE.to_owned(),
        ),
        (
            PluginError::ResourceGroupUnavailable,
            service_errors::RESOURCE_GROUP_UNAVAILABLE.to_owned(),
        ),
        (
            PluginError::GtsRegistryUnavailable,
            service_errors::GTS_REGISTRY_UNAVAILABLE.to_owned(),
        ),
        (
            PluginError::TenantNotFound,
            service_errors::TENANT_NOT_FOUND.to_owned(),
        ),
        (
            PluginError::ResourceGroupNotFound,
            service_errors::RESOURCE_GROUP_NOT_FOUND.to_owned(),
        ),
        (
            PluginError::RbacScopeProvenanceInvalid,
            service_errors::RBAC_SCOPE_PROVENANCE_INVALID.to_owned(),
        ),
        (
            PluginError::internal("cache invariant"),
            "cache invariant".to_owned(),
        ),
    ];
    assert_eq!(
        cases.len(),
        all_variants().len(),
        "every variant needs a pinned message"
    );
    for (err, expected) in cases {
        assert_eq!(err.to_string(), expected);
    }
}

/// Transient outages AND the deterministic scope failures project onto
/// `ServiceUnavailable`; every other variant onto `Internal`.
#[test]
fn sdk_projection_splits_service_unavailable_from_internal() {
    let service_unavailable = [
        PluginError::RbacUnavailable,
        PluginError::TenantResolverUnavailable,
        PluginError::ResourceGroupUnavailable,
        PluginError::GtsRegistryUnavailable,
        PluginError::TenantNotFound,
        PluginError::ResourceGroupNotFound,
    ];
    // `AuthZResolverError` is not `PartialEq`, so assert on the shape and the
    // payload separately.
    for err in service_unavailable {
        let message = err.to_string();
        match Sdk::from(err) {
            Sdk::ServiceUnavailable(actual) => assert_eq!(actual, message),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    let internal = [
        PluginError::InvalidOperationEmpty,
        PluginError::MissingResourceType,
        PluginError::UnknownSubjectType {
            value: "x".to_owned(),
        },
        PluginError::UnreadableSubjectTenant {
            detail: "not a UUID".to_owned(),
        },
        PluginError::RbacScopeProvenanceInvalid,
        PluginError::internal("boom"),
    ];
    for err in internal {
        let message = err.to_string();
        match Sdk::from(err) {
            Sdk::Internal(actual) => assert_eq!(actual, message),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}

// The singleflight error-sharing contract this module's `Clone` exists for is
// covered where it actually happens:
// `hierarchy_cache_tests::singleflight_waiters_all_receive_the_leader_s_error`
// drives the leader/waiter path and asserts every waiter gets the leader's
// error. A `for err in all_variants() { assert_eq!(err.clone(), err) }` here
// only restated that `#[derive(Clone, PartialEq)]` compiles.
