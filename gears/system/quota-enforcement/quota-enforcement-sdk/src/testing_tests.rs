//! The doubles must hold the contract semantics the gear's tests rely on.

use std::collections::BTreeMap;
use std::time::Duration;

use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{
    InMemoryCoordination, InMemoryStorage, empty_engine_config, quota_draft, test_metric,
    test_subject, test_tenant,
};
use crate::coordination_plugin::{CoordinationError, CoordinationPluginV1, LockScope};
use crate::models::{
    ApplicableQuotas, BootstrapBundle, CapPatch, ConfigDefaults, Decision, DecisionResult,
    IdempotencyScope, IdempotencySubjectKey, IdempotencyWrite, LeaseState, OperationType,
    PageRequest, PayloadHash, PolicyDraft, PolicyId, PolicyScope, PolicyUpdate, PolicyVersionState,
    QuotaDebitPlan, QuotaId, QuotaPatch,
};
use crate::storage_plugin::{CONTRACT_MAJOR, QuotaEnforcementStoragePluginV1, StorageError};

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5eed))
        .subject_tenant_id(test_tenant().as_uuid())
        .build()
        .expect("test security context")
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(test_tenant().as_uuid())
}

fn idem(op: OperationType, key: &str, payload: u8) -> IdempotencyWrite {
    IdempotencyWrite {
        scope: IdempotencyScope {
            tenant_id: test_tenant(),
            subject_key: IdempotencySubjectKey::from_bytes([1; 32]),
            operation_type: op,
            key: key.to_owned(),
        },
        payload_hash: PayloadHash::from_bytes([payload; 32]),
        decision: Decision {
            result: DecisionResult::Allowed,
            debit_plan: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
        },
        engine_id: "most-restrictive-wins".to_owned(),
        policy_id: PolicyId::global(),
        policy_version: 1,
    }
}

fn applicable() -> ApplicableQuotas {
    ApplicableQuotas {
        tenant_id: test_tenant(),
        subjects: vec![test_subject("u1")],
        metric: test_metric(),
    }
}

async fn seeded_quota(storage: &InMemoryStorage, cap: Option<u64>) -> QuotaId {
    storage
        .create_quota(&ctx(), &scope(), quota_draft(test_subject("u1"), cap), &[])
        .await
        .expect("create quota")
}

fn plan(id: QuotaId, amount: u64) -> BTreeMap<QuotaId, QuotaDebitPlan> {
    BTreeMap::from([(id, QuotaDebitPlan { amount })])
}

// --- coordination ----------------------------------------------------------

#[tokio::test]
async fn coordination_grants_one_holder_per_scope_and_frees_on_release() {
    let coord = InMemoryCoordination::new();
    let ttl = Duration::from_mins(1);
    let lock = coord
        .try_lock(LockScope::LeaseSweeper, ttl)
        .await
        .expect("first acquisition");
    assert!(coord.is_held(LockScope::LeaseSweeper));
    assert!(
        !coord.is_held(LockScope::RetentionSweeper),
        "scopes are independent"
    );

    let conflict = coord.try_lock(LockScope::LeaseSweeper, ttl).await;
    assert_eq!(
        conflict,
        Err(CoordinationError::Conflict {
            scope: LockScope::LeaseSweeper
        })
    );

    coord.release(lock).await.expect("release");
    assert!(!coord.is_held(LockScope::LeaseSweeper));
    coord
        .try_lock(LockScope::LeaseSweeper, ttl)
        .await
        .expect("re-acquire after release without a TTL wait");
    assert_eq!(coord.try_lock_calls(), 3);
    assert_eq!(coord.release_calls(), 1);
}

#[tokio::test]
async fn coordination_expired_hold_is_stealable_and_renew_reports_expiry() {
    let coord = InMemoryCoordination::new();
    let lock = coord
        .try_lock(LockScope::RetentionSweeper, Duration::from_secs(30))
        .await
        .expect("acquire");
    coord.expire_all();
    assert!(!coord.is_held(LockScope::RetentionSweeper));
    assert_eq!(
        coord.renew(&lock).await,
        Err(CoordinationError::LockExpired {
            scope: LockScope::RetentionSweeper
        })
    );
    coord
        .try_lock(LockScope::RetentionSweeper, Duration::from_secs(30))
        .await
        .expect("a survivor steals the expired hold");
    // The old holder's release must not evict the new holder.
    coord
        .release(lock)
        .await
        .expect("stale release is best-effort");
    assert!(coord.is_held(LockScope::RetentionSweeper));
}

#[tokio::test]
async fn coordination_injected_failure_covers_every_method_until_cleared() {
    let coord = InMemoryCoordination::new();
    let lock = coord
        .try_lock(LockScope::LeaseSweeper, Duration::from_secs(5))
        .await
        .expect("acquire");
    coord.fail_with(CoordinationError::BackendUnavailable("down".into()));
    assert!(matches!(
        coord
            .try_lock(LockScope::RetentionSweeper, Duration::from_secs(5))
            .await,
        Err(CoordinationError::BackendUnavailable(_))
    ));
    assert!(matches!(
        coord.renew(&lock).await,
        Err(CoordinationError::BackendUnavailable(_))
    ));
    coord.clear_failure();
    coord.renew(&lock).await.expect("renew after recovery");
    assert_eq!(coord.renew_calls(), 2);
}

// --- storage bootstrap -----------------------------------------------------

#[tokio::test]
async fn storage_bootstrap_is_idempotent_and_seeds_defaults_once() {
    let storage = InMemoryStorage::new();
    let bundle = BootstrapBundle::foundation();
    storage.bootstrap(&bundle).await.expect("first bootstrap");
    let mut second = bundle.clone();
    second.config_defaults.max_active_leases = 5;
    storage.bootstrap(&second).await.expect("second bootstrap");
    assert_eq!(storage.bootstrap_calls(), 2);
    assert_eq!(
        storage.seeded_defaults(),
        Some(ConfigDefaults::default()),
        "existing rows are kept on re-bootstrap"
    );
    assert_eq!(storage.bootstrapped_bundle(), Some(second));
}

#[tokio::test]
async fn storage_bootstrap_rejects_a_schema_major_mismatch() {
    let storage = InMemoryStorage::with_installed_schema_major(CONTRACT_MAJOR + 1);
    let err = storage
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect_err("mismatch must fail");
    assert_eq!(
        err,
        StorageError::SchemaVersionMismatch {
            installed: CONTRACT_MAJOR + 1,
            expected: CONTRACT_MAJOR,
        }
    );
    assert!(
        storage.seeded_defaults().is_none(),
        "nothing is seeded on failure"
    );
    assert_eq!(storage.bootstrap_calls(), 1);
}

#[tokio::test]
async fn storage_bootstrap_seeds_the_global_policy_when_the_bundle_carries_one() {
    let storage = InMemoryStorage::new();
    let mut bundle = BootstrapBundle::foundation();
    bundle.global_policy = Some(PolicyDraft {
        scope: PolicyScope::Global,
        engine_id: "most-restrictive-wins".to_owned(),
        engine_config: empty_engine_config(),
        timeout_ms: None,
        description: None,
        comment: None,
        created_by: "bootstrap".to_owned(),
    });
    storage.bootstrap(&bundle).await.expect("bootstrap");
    let global = storage
        .read_policy(&PolicyScope::Global)
        .await
        .expect("read")
        .expect("seeded");
    assert_eq!(global.version, 1);
    assert_eq!(global.policy_id, PolicyId::global());
    assert_eq!(global.state, PolicyVersionState::Active);
}

#[tokio::test]
async fn storage_injected_failure_blocks_every_call_until_cleared() {
    let storage = InMemoryStorage::new();
    storage.fail_with(StorageError::Unavailable("down".into()));
    assert!(matches!(
        storage.bootstrap(&BootstrapBundle::foundation()).await,
        Err(StorageError::Unavailable(_))
    ));
    assert!(matches!(
        storage
            .lookup_idempotency(&idem(OperationType::Debit, "k", 1).scope)
            .await,
        Err(StorageError::Unavailable(_))
    ));
    storage.clear_failure();
    storage
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect("bootstrap after recovery");
}

// --- storage quota and counter semantics -----------------------------------

#[tokio::test]
async fn storage_debit_plan_mutates_counters_once_and_replays_verbatim() {
    let storage = InMemoryStorage::new();
    let id = seeded_quota(&storage, Some(100)).await;
    let write = idem(OperationType::Debit, "k1", 7);
    let first = storage
        .apply_debit_plan(&ctx(), &scope(), &applicable(), &plan(id, 40), &write, &[])
        .await
        .expect("first debit");
    assert_eq!(first.counters[0].value, 40);
    assert_eq!(storage.consumed(id), 40);

    let replay = storage
        .apply_debit_plan(&ctx(), &scope(), &applicable(), &plan(id, 40), &write, &[])
        .await
        .expect("replay");
    assert_eq!(storage.consumed(id), 40, "replay must not mutate");
    assert_eq!(replay.counters[0].value, 40);

    let mismatch = idem(OperationType::Debit, "k1", 8);
    let err = storage
        .apply_debit_plan(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 1),
            &mismatch,
            &[],
        )
        .await
        .expect_err("different payload under the same key");
    assert_eq!(err, StorageError::IdempotencyPayloadMismatch);
    assert!(
        storage
            .lookup_idempotency(&write.scope)
            .await
            .expect("lookup")
            .is_some()
    );
}

#[tokio::test]
async fn storage_update_enforces_cap_versus_consumed_and_bumps_the_version() {
    let storage = InMemoryStorage::new();
    let id = seeded_quota(&storage, Some(100)).await;
    storage
        .apply_debit_plan(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 60),
            &idem(OperationType::Debit, "d", 1),
            &[],
        )
        .await
        .expect("debit");
    let err = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                cap: Some(CapPatch::Bounded(50)),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect_err("cap below consumed");
    assert_eq!(
        err,
        StorageError::CapBelowConsumed {
            new_cap: 50,
            consumed: 60
        }
    );
    let updated = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                cap: Some(CapPatch::Unbounded),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect("raise to unbounded");
    assert_eq!(updated.cap, None);
    assert_eq!(updated.record_version, 2);
    let missing = storage
        .update_quota(
            &ctx(),
            &scope(),
            QuotaId::generate(),
            QuotaPatch::default(),
            &[],
        )
        .await
        .expect_err("unknown quota");
    assert!(matches!(missing, StorageError::QuotaNotFound { .. }));
}

#[tokio::test]
async fn storage_lease_lifecycle_commit_release_expiry_and_deactivation() {
    let storage = InMemoryStorage::new();
    storage
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect("bootstrap");
    let id = seeded_quota(&storage, Some(100)).await;
    let ttl = Duration::from_mins(1);

    let token = storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 30),
            ttl,
            &idem(OperationType::Reserve, "r1", 1),
        )
        .await
        .expect("acquire");
    assert_eq!(storage.consumed(id), 30);
    assert_eq!(storage.lease_state(token), Some(LeaseState::Active));

    let over = storage
        .commit_lease(
            &ctx(),
            &scope(),
            token,
            Some(31),
            &idem(OperationType::Commit, "c0", 1),
            &[],
        )
        .await
        .expect_err("over-commit");
    assert_eq!(
        over,
        StorageError::OverCommitNotAuthorized {
            reserved: 30,
            actual: 31
        }
    );

    storage
        .commit_lease(
            &ctx(),
            &scope(),
            token,
            Some(20),
            &idem(OperationType::Commit, "c1", 1),
            &[],
        )
        .await
        .expect("commit less than reserved");
    assert_eq!(storage.consumed(id), 20, "unused reservation is returned");
    assert_eq!(storage.lease_state(token), Some(LeaseState::Committed));
    let again = storage
        .release_lease(
            &ctx(),
            &scope(),
            token,
            &idem(OperationType::Release, "x", 1),
            &[],
        )
        .await
        .expect_err("terminal lease");
    assert_eq!(again, StorageError::LeaseNotActive { token });

    let token2 = storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 10),
            ttl,
            &idem(OperationType::Reserve, "r2", 1),
        )
        .await
        .expect("second lease");
    storage.expire_leases();
    let expired = storage
        .commit_lease(
            &ctx(),
            &scope(),
            token2,
            None,
            &idem(OperationType::Commit, "c2", 1),
            &[],
        )
        .await
        .expect_err("expired leases are released lazily (I4)");
    assert_eq!(expired, StorageError::LeaseNotActive { token: token2 });
    let reclaimed = storage
        .reclaim_expired_leases(10, OffsetDateTime::now_utc())
        .await
        .expect("reclaim");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].token, token2);
    assert_eq!(storage.consumed(id), 20, "auto-release returned the hold");
    assert_eq!(storage.lease_state(token2), Some(LeaseState::AutoReleased));

    let token3 = storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 5),
            ttl,
            &idem(OperationType::Reserve, "r3", 1),
        )
        .await
        .expect("third lease");
    let outcome = storage
        .deactivate_quota(&ctx(), &scope(), id, &[])
        .await
        .expect("deactivate");
    assert_eq!(outcome.resolved_leases, vec![token3]);
    assert_eq!(
        storage.lease_state(token3),
        Some(LeaseState::ResolvedByDeactivation)
    );
    let blocked = storage
        .apply_debit_plan(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 1),
            &idem(OperationType::Debit, "z", 1),
            &[],
        )
        .await
        .expect_err("deactivated quota accepts no debit");
    assert_eq!(blocked, StorageError::QuotaDeactivated { id });
}

#[tokio::test]
async fn storage_active_lease_cap_is_enforced_from_the_seeded_defaults() {
    let storage = InMemoryStorage::new();
    let mut bundle = BootstrapBundle::foundation();
    bundle.config_defaults.max_active_leases = 1;
    storage.bootstrap(&bundle).await.expect("bootstrap");
    let id = seeded_quota(&storage, None).await;
    storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 1),
            Duration::from_secs(9),
            &idem(OperationType::Reserve, "a", 1),
        )
        .await
        .expect("first");
    let err = storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 1),
            Duration::from_secs(9),
            &idem(OperationType::Reserve, "b", 1),
        )
        .await
        .expect_err("cap reached");
    assert_eq!(err, StorageError::LeaseInflightLimitExceeded);
}

#[tokio::test]
async fn storage_snapshot_reads_reflect_scope_and_remaining_capacity() {
    let storage = InMemoryStorage::new();
    let id = seeded_quota(&storage, Some(10)).await;
    storage
        .apply_debit_plan(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 4),
            &idem(OperationType::Debit, "d", 1),
            &[],
        )
        .await
        .expect("debit");
    let snaps = storage
        .read_quota_snapshot(&ctx(), &scope(), &applicable())
        .await
        .expect("snapshot");
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].consumed, 4);
    assert_eq!(snaps[0].remaining, Some(6));
    assert!(snaps[0].currently_within_window);
    let other = ApplicableQuotas {
        subjects: vec![test_subject("someone-else")],
        ..applicable()
    };
    assert!(
        storage
            .read_quota_snapshot(&ctx(), &scope(), &other)
            .await
            .expect("snapshot")
            .is_empty(),
        "another subject sees no quota"
    );
}

// --- storage policies ------------------------------------------------------

#[tokio::test]
async fn storage_policy_versions_update_rollback_and_delete() {
    let storage = InMemoryStorage::new();
    let draft = PolicyDraft {
        scope: PolicyScope::Global,
        engine_id: "most-restrictive-wins".to_owned(),
        engine_config: empty_engine_config(),
        timeout_ms: None,
        description: None,
        comment: Some("seed".to_owned()),
        created_by: "op".to_owned(),
    };
    let v1 = storage
        .create_policy(&ctx(), draft.clone(), &[])
        .await
        .expect("v1");
    assert_eq!(v1.version, 1);
    let dup = storage
        .create_policy(&ctx(), draft, &[])
        .await
        .expect_err("scope taken");
    assert!(matches!(dup, StorageError::VersionConflict { .. }));

    let stale = PolicyUpdate {
        if_match_version: 7,
        engine_id: None,
        engine_config: None,
        timeout_ms: None,
        comment: None,
        created_by: "op".to_owned(),
    };
    let err = storage
        .update_policy(&ctx(), PolicyId::global(), stale, &[])
        .await
        .expect_err("lost update");
    assert_eq!(
        err,
        StorageError::VersionConflict {
            expected: 7,
            actual: 1
        }
    );

    let v2 = storage
        .update_policy(
            &ctx(),
            PolicyId::global(),
            PolicyUpdate {
                if_match_version: 1,
                engine_id: Some("cel".to_owned()),
                engine_config: None,
                timeout_ms: Some(5),
                comment: None,
                created_by: "op".to_owned(),
            },
            &[],
        )
        .await
        .expect("v2");
    assert_eq!(v2.version, 2);
    assert_eq!(v2.engine_id, "cel");
    let listed = storage
        .list_policy_versions(&PolicyId::global(), PageRequest::default())
        .await
        .expect("list");
    assert_eq!(
        listed.items.iter().map(|m| m.state).collect::<Vec<_>>(),
        vec![PolicyVersionState::Superseded, PolicyVersionState::Active]
    );

    let back = storage
        .rollback_policy(&ctx(), PolicyId::global(), 1, None, &[])
        .await
        .expect("rollback");
    assert_eq!(back.version, 1);
    assert_eq!(back.state, PolicyVersionState::Active);
    assert_eq!(
        storage
            .read_policy_version(&PolicyId::global(), 2)
            .await
            .expect("read")
            .map(|v| v.state),
        Some(PolicyVersionState::RolledBack)
    );
    let terminal = storage
        .rollback_policy(&ctx(), PolicyId::global(), 2, None, &[])
        .await
        .expect_err("rolled-back versions never re-activate");
    assert!(matches!(
        terminal,
        StorageError::VersionRolledBack { version: 2, .. }
    ));
    let unknown = storage
        .rollback_policy(&ctx(), PolicyId::global(), 9, None, &[])
        .await
        .expect_err("unknown version");
    assert!(matches!(
        unknown,
        StorageError::UnknownPolicyVersion { version: 9, .. }
    ));

    storage
        .delete_policy(&ctx(), PolicyId::global(), None, &[])
        .await
        .expect("delete");
    assert!(
        storage
            .read_policy(&PolicyScope::Global)
            .await
            .expect("read")
            .is_none()
    );
    storage
        .delete_policy(&ctx(), PolicyId::global(), None, &[])
        .await
        .expect("idempotent delete");
}

#[tokio::test]
async fn storage_reclaims_expired_idempotency_records_and_log_entries() {
    let storage = InMemoryStorage::new();
    let id = seeded_quota(&storage, None).await;
    storage
        .apply_debit_plan(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 1),
            &idem(OperationType::Debit, "d", 1),
            &[],
        )
        .await
        .expect("debit");
    let far_future = OffsetDateTime::now_utc() + time::Duration::days(10);
    assert_eq!(
        storage
            .reclaim_expired_idempotency(10, OffsetDateTime::now_utc())
            .await
            .expect("none yet"),
        0
    );
    assert_eq!(
        storage
            .reclaim_expired_idempotency(10, far_future)
            .await
            .expect("reclaim"),
        1
    );
    assert_eq!(
        storage
            .reclaim_operation_log(1, far_future)
            .await
            .expect("log"),
        1
    );
    assert_eq!(
        storage
            .reclaim_operation_log(10, far_future)
            .await
            .expect("log again"),
        1
    );
    assert_eq!(
        storage
            .reclaim_operation_log(10, far_future)
            .await
            .expect("empty"),
        0
    );
}
