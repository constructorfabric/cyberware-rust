use quota_enforcement_sdk::{
    LEASE_RESOURCE, LeaseToken, OPERATION_RESOURCE, POLICY_RESOURCE, PolicyId, QUOTA_RESOURCE,
};
use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use crate::domain::error::{Dependency, DomainError, PluginKind, ResourceKind};
use crate::domain::ports::metrics::ValidationReason;

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
            DomainError::ProjectionNotResolvable {
                projection: "gts.x~".to_owned(),
            },
            400,
        ),
        (
            DomainError::CatalogInvalid {
                reason: ValidationReason::Abstract,
                subject: "gts.x~".to_owned(),
            },
            503,
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
        (DomainError::CapMustBeNonNegative { cap: -1 }, 400),
        (DomainError::ThresholdsRequireBoundedCap, 400),
        (
            DomainError::ConstraintContractMismatch {
                contract: "gts.x~".to_owned(),
            },
            400,
        ),
        (
            DomainError::MetricClassificationInvalid {
                metric: "gts.m".to_owned(),
            },
            400,
        ),
        (
            DomainError::NotYetImplemented {
                feature: "rate quotas",
            },
            501,
        ),
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

#[test]
fn not_yet_implemented_is_unimplemented_with_the_token_leading_the_detail() {
    let problem = Problem::from(CanonicalError::from(DomainError::NotYetImplemented {
        feature: "rate quotas",
    }));
    assert_eq!(problem.status, Some(501));
    let detail = problem.detail.clone();
    assert!(
        detail.starts_with("NOT_YET_IMPLEMENTED:"),
        "the token leads the detail: {detail}"
    );
    let rendered = serde_json::to_string(&problem).expect("json");
    assert!(rendered.contains(QUOTA_RESOURCE), "{rendered}");
}

#[test]
fn quota_lifecycle_rejections_carry_their_tokens_and_subjects() {
    let cases = vec![
        (
            DomainError::CapMustBeNonNegative { cap: -5 },
            "CAP_MUST_BE_NON_NEGATIVE",
            "cap",
        ),
        (
            DomainError::ThresholdsRequireBoundedCap,
            "THRESHOLDS_REQUIRE_BOUNDED_CAP",
            "notification_thresholds",
        ),
        (
            DomainError::ConstraintContractMismatch {
                contract: "gts.cf.core.qe.constraint.v1~x.y.z.w.v1~".to_owned(),
            },
            "CONSTRAINT_CONTRACT_MISMATCH",
            "metadata",
        ),
        (
            DomainError::MetricClassificationInvalid {
                metric: "gts.cf.qe.metric.type.v1~cf.qe.metric.m.v1".to_owned(),
            },
            "METRIC_CLASSIFICATION_INVALID",
            "gts.cf.qe.metric.type.v1~cf.qe.metric.m.v1",
        ),
    ];
    for (err, token, subject) in cases {
        let rendered =
            serde_json::to_string(&Problem::from(CanonicalError::from(err))).expect("json");
        assert!(rendered.contains(token), "{token}: {rendered}");
        assert!(rendered.contains(subject), "{subject}: {rendered}");
    }
}
