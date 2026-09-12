use uuid::Uuid;

use super::{CONTRACT_MAJOR, StorageError};
use crate::models::{BootstrapBundle, ConfigDefaults, LeaseToken, PolicyId, QuotaId};

#[test]
fn contract_major_is_one_for_the_first_gear_major() {
    assert_eq!(CONTRACT_MAJOR, 1);
}

#[test]
fn foundation_bundle_carries_the_contract_major_and_prd_defaults() {
    let bundle = BootstrapBundle::foundation();
    assert_eq!(bundle.contract_major, CONTRACT_MAJOR);
    assert!(bundle.global_policy.is_none(), "seeded by a later feature");
    assert_eq!(
        bundle.config_defaults,
        ConfigDefaults {
            contention_timeout_ms: 0,
            max_active_leases: 1000,
            idempotency_retention_secs: 86_400,
        }
    );
}

#[test]
fn storage_error_messages_carry_their_identifiers() {
    let token = LeaseToken::new(Uuid::from_u128(0xabc));
    let msg = StorageError::LeaseNotActive { token }.to_string();
    assert!(msg.contains(&token.to_string()), "{msg}");

    let quota = QuotaId::new(Uuid::from_u128(0xdef));
    assert!(
        StorageError::QuotaNotFound { id: quota }
            .to_string()
            .contains(&quota.to_string())
    );

    let mismatch = StorageError::SchemaVersionMismatch {
        installed: 2,
        expected: CONTRACT_MAJOR,
    };
    let msg = mismatch.to_string();
    assert!(msg.contains('2') && msg.contains('1'), "{msg}");

    let rolled = StorageError::VersionRolledBack {
        policy_id: PolicyId::global(),
        version: 3,
    };
    assert!(rolled.to_string().contains("global"));
    assert!(rolled.to_string().contains('3'));
}

#[test]
fn storage_error_variants_compare_structurally() {
    let a = StorageError::VersionConflict {
        expected: 1,
        actual: 2,
    };
    let b = StorageError::VersionConflict {
        expected: 1,
        actual: 2,
    };
    let c = StorageError::VersionConflict {
        expected: 1,
        actual: 3,
    };
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_ne!(
        StorageError::Unavailable("x".into()),
        StorageError::Internal("x".into())
    );
}
