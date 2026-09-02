use quota_enforcement_sdk::{
    LEASE_RESOURCE, LeaseToken, OPERATION_RESOURCE, POLICY_RESOURCE, PolicyId, QUOTA_RESOURCE,
};
use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use crate::domain::error::{Dependency, DomainError, PluginKind, ResourceKind};

fn status(err: DomainError) -> u16 {
    Problem::from(CanonicalError::from(err))
        .status
        .expect("every canonical error carries a status")
}

#[test]
fn every_variant_family_maps_to_its_documented_status() {
    let cases: Vec<(DomainError, u16)> = vec![
        (
            DomainError::InvalidArgument {
                field: "tenant_id",
                reason: "TENANT_ID_REQUIRED",
            },
            400,
        ),
        (
            DomainError::ProjectionNotRegistered {
                projection: "gts.x~".to_owned(),
            },
            400,
        ),
        (
            DomainError::LeaseNotActive {
                token: LeaseToken::new(Uuid::from_u128(1)),
            },
            400,
        ),
        (
            DomainError::CapBelowConsumed {
                new_cap: 1,
                consumed: 2,
            },
            400,
        ),
        (DomainError::PeriodClosed, 400),
        (
            DomainError::VersionRolledBack {
                policy_id: PolicyId::global(),
                version: 2,
            },
            400,
        ),
        (DomainError::PdpDenied { reason: None }, 403),
        (
            DomainError::NotFound {
                kind: ResourceKind::Quota,
                id: "q".to_owned(),
            },
            404,
        ),
        (DomainError::IdempotencyPayloadMismatch, 409),
        (
            DomainError::VersionConflict {
                expected: 1,
                actual: 2,
            },
            409,
        ),
        (DomainError::LeaseContentionTimeout, 409),
        (DomainError::LeaseInflightLimitExceeded, 429),
        (
            DomainError::NotReady {
                dependency: Dependency::Storage,
            },
            503,
        ),
        (DomainError::PdpUnavailable("x".to_owned()), 503),
        (DomainError::BackendUnavailable("x".to_owned()), 503),
        (DomainError::TypesRegistryUnavailable("x".to_owned()), 503),
        (
            DomainError::PluginNotFound {
                kind: PluginKind::Storage,
                vendor: "acme".to_owned(),
            },
            503,
        ),
        (
            DomainError::SchemaVersionMismatch {
                installed: 2,
                expected: 1,
            },
            500,
        ),
        (DomainError::Internal("secret detail".to_owned()), 500),
    ];
    for (err, expected) in cases {
        let debug = format!("{err:?}");
        assert_eq!(status(err), expected, "{debug}");
    }
}

#[test]
fn not_found_carries_the_resource_type_and_name_of_its_kind() {
    let cases = vec![
        (ResourceKind::Quota, QUOTA_RESOURCE),
        (ResourceKind::Policy, POLICY_RESOURCE),
        (ResourceKind::Lease, LEASE_RESOURCE),
        (ResourceKind::Operation, OPERATION_RESOURCE),
    ];
    for (kind, resource_type) in cases {
        let err = CanonicalError::from(DomainError::NotFound {
            kind,
            id: "abc".to_owned(),
        });
        let CanonicalError::NotFound { .. } = &err else {
            panic!("expected NotFound for {kind}, got {err:?}");
        };
        let rendered = serde_json::to_string(&Problem::from(err)).expect("problem json");
        assert!(rendered.contains(resource_type), "{kind}: {rendered}");
        assert!(rendered.contains("abc"), "{kind}: {rendered}");
    }
}

#[test]
fn permission_denied_and_internal_errors_leak_no_detail() {
    let denied = Problem::from(CanonicalError::from(DomainError::PdpDenied {
        reason: Some("pdp said: subject lacks role X".to_owned()),
    }));
    let rendered = serde_json::to_string(&denied).expect("json");
    assert!(
        !rendered.contains("subject lacks role"),
        "PDP detail stays in logs: {rendered}"
    );
    assert!(rendered.contains("AUTHZ"), "{rendered}");

    let internal = Problem::from(CanonicalError::from(DomainError::Internal(
        "connection string postgres://user:pw@host".to_owned(),
    )));
    let rendered = serde_json::to_string(&internal).expect("json");
    assert!(!rendered.contains("postgres://"), "{rendered}");
}

#[test]
fn precondition_and_abort_envelopes_carry_their_upper_snake_tokens() {
    let precondition = Problem::from(CanonicalError::from(DomainError::CapBelowConsumed {
        new_cap: 5,
        consumed: 9,
    }));
    let rendered = serde_json::to_string(&precondition).expect("json");
    assert!(rendered.contains("CAP_BELOW_CONSUMED"), "{rendered}");

    let aborted = Problem::from(CanonicalError::from(
        DomainError::IdempotencyPayloadMismatch,
    ));
    let rendered = serde_json::to_string(&aborted).expect("json");
    assert!(
        rendered.contains("IDEMPOTENCY_PAYLOAD_MISMATCH"),
        "{rendered}"
    );

    let exhausted = Problem::from(CanonicalError::from(
        DomainError::LeaseInflightLimitExceeded,
    ));
    let rendered = serde_json::to_string(&exhausted).expect("json");
    assert!(
        rendered.contains("LEASE_INFLIGHT_LIMIT_EXCEEDED"),
        "{rendered}"
    );
}

#[test]
fn unavailability_names_the_dependency_class_but_not_the_cause() {
    let rendered = serde_json::to_string(&Problem::from(CanonicalError::from(
        DomainError::BackendUnavailable("host 10.0.0.7 refused".to_owned()),
    )))
    .expect("json");
    assert!(rendered.contains("DEPENDENCY_UNAVAILABLE"), "{rendered}");
    assert!(!rendered.contains("10.0.0.7"), "{rendered}");
}
