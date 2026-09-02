use authz_resolver_sdk::EnforcerError;
use authz_resolver_sdk::models::DenyReason;
use quota_enforcement_sdk::{LeaseToken, PolicyId, QuotaId, StorageError};
use toolkit::plugins::ChoosePluginError;
use toolkit_canonical_errors::CanonicalError;
use uuid::Uuid;

use super::{Dependency, DomainError, PluginKind, ResourceKind};

#[test]
fn enforcer_denial_and_compile_failure_are_denials_and_transport_failure_is_unavailability() {
    let denied = DomainError::from_enforcer(EnforcerError::Denied {
        deny_reason: Some(DenyReason {
            error_code: "NO_GRANT".to_owned(),
            details: Some("operator log only".to_owned()),
        }),
    });
    assert_eq!(
        denied,
        DomainError::PdpDenied {
            reason: Some("NO_GRANT".to_owned())
        }
    );
    let bare = DomainError::from_enforcer(EnforcerError::Denied { deny_reason: None });
    assert_eq!(bare, DomainError::PdpDenied { reason: None });

    let failed = DomainError::from_enforcer(EnforcerError::EvaluationFailed(
        CanonicalError::internal("pdp down").create(),
    ));
    assert!(
        matches!(failed, DomainError::PdpUnavailable(_)),
        "{failed:?}"
    );
}

#[test]
fn storage_errors_lift_one_to_one_with_the_two_documented_exceptions() {
    let quota = QuotaId::new(Uuid::from_u128(0xa1));
    let cases: Vec<(StorageError, DomainError)> = vec![
        (
            StorageError::QuotaNotFound { id: quota },
            DomainError::NotFound {
                kind: ResourceKind::Quota,
                id: quota.to_string(),
            },
        ),
        (
            StorageError::SubjectOutOfScope,
            DomainError::PdpDenied {
                reason: Some(DomainError::SUBJECT_OUT_OF_SCOPE.to_owned()),
            },
        ),
        (
            StorageError::LeaseNotActive {
                token: LeaseToken::new(Uuid::from_u128(1)),
            },
            DomainError::LeaseNotActive {
                token: LeaseToken::new(Uuid::from_u128(1)),
            },
        ),
        (
            StorageError::CapBelowConsumed {
                new_cap: 1,
                consumed: 2,
            },
            DomainError::CapBelowConsumed {
                new_cap: 1,
                consumed: 2,
            },
        ),
        (
            StorageError::VersionRolledBack {
                policy_id: PolicyId::global(),
                version: 2,
            },
            DomainError::VersionRolledBack {
                policy_id: PolicyId::global(),
                version: 2,
            },
        ),
        (
            StorageError::Unavailable("db down".to_owned()),
            DomainError::BackendUnavailable("db down".to_owned()),
        ),
        (StorageError::PeriodClosed, DomainError::PeriodClosed),
        (
            StorageError::IdempotencyPayloadMismatch,
            DomainError::IdempotencyPayloadMismatch,
        ),
    ];
    for (storage, expected) in cases {
        assert_eq!(DomainError::from(storage), expected);
    }
    let mismatch = DomainError::from(StorageError::SchemaVersionMismatch {
        installed: 2,
        expected: 1,
    });
    assert!(
        matches!(mismatch, DomainError::Internal(_)),
        "a runtime schema mismatch is a plugin contract violation: {mismatch:?}"
    );
}

#[test]
fn plugin_selection_errors_carry_the_plugin_kind() {
    let not_found = DomainError::plugin_selection(
        PluginKind::Coordination,
        ChoosePluginError::PluginNotFound {
            type_id: "gts.x".to_owned(),
            vendor: "acme".to_owned(),
        },
    );
    assert_eq!(
        not_found,
        DomainError::PluginNotFound {
            kind: PluginKind::Coordination,
            vendor: "acme".to_owned(),
        }
    );
    assert!(not_found.to_string().contains("coordination"));
    let invalid = DomainError::plugin_selection(
        PluginKind::Storage,
        ChoosePluginError::InvalidPluginInstance {
            gts_id: "gts.bad".to_owned(),
            reason: "missing vendor".to_owned(),
        },
    );
    assert!(matches!(
        invalid,
        DomainError::InvalidPluginInstance {
            kind: PluginKind::Storage,
            ..
        }
    ));
}

#[test]
fn dependency_labels_are_stable_health_code_fragments() {
    let labels: Vec<&str> = [
        Dependency::Storage,
        Dependency::Coordination,
        Dependency::Pdp,
        Dependency::TypesRegistry,
    ]
    .iter()
    .map(|d| d.as_label())
    .collect();
    assert_eq!(
        labels,
        vec!["storage", "coordination", "pdp", "types_registry"]
    );
    let err = DomainError::NotReady {
        dependency: Dependency::Storage,
    };
    assert!(err.to_string().contains("storage"));
}
