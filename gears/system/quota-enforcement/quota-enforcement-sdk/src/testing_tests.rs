//! The doubles must hold the contract semantics the gear's tests rely on.

use std::collections::BTreeMap;
use std::time::Duration;

use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{
    InMemoryStorage, empty_engine_config, quota_draft, test_metric, test_subject, test_tenant,
};
use crate::models::{
    ApplicableQuotas, BootstrapBundle, CapPatch, ConfigDefaults, Decision, DecisionResult, EventId,
    IdempotencyScope, IdempotencySubjectKey, IdempotencyWrite, LeaseState, NotificationEvent,
    NotificationEventKind, OperationType, PageRequest, PayloadHash, PolicyDraft, PolicyId,
    PolicyScope, PolicyUpdate, PolicyVersionState, QuotaDebitPlan, QuotaFilter, QuotaId,
    QuotaPatch, QuotaStatus,
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

#[tokio::test]
async fn active_projection_bindings_are_the_distinct_pairs_of_active_quotas() {
    let storage = InMemoryStorage::new();
    assert!(
        storage
            .read_active_projection_bindings()
            .await
            .expect("empty store")
            .is_empty()
    );

    let first = storage
        .create_quota(
            &ctx(),
            &scope(),
            quota_draft(test_subject("u1"), Some(10)),
            &[],
        )
        .await
        .expect("first");
    storage
        .create_quota(
            &ctx(),
            &scope(),
            quota_draft(test_subject("u2"), Some(10)),
            &[],
        )
        .await
        .expect("same pair, other subject id");
    let bindings = storage
        .read_active_projection_bindings()
        .await
        .expect("bindings");
    assert_eq!(
        bindings.len(),
        1,
        "distinct by (metric, projection_type): {bindings:?}"
    );
    let binding = bindings.iter().next().expect("one");
    assert_eq!(binding.metric, test_metric());
    assert_eq!(binding.projection_type, test_subject("u1").projection_type);

    storage
        .deactivate_quota(&ctx(), &scope(), first, &[])
        .await
        .expect("deactivate one");
    assert_eq!(
        storage
            .read_active_projection_bindings()
            .await
            .expect("bindings")
            .len(),
        1,
        "the other active Quota keeps the pair"
    );

    storage.fail_with(StorageError::Unavailable("db down".into()));
    assert!(matches!(
        storage.read_active_projection_bindings().await,
        Err(StorageError::Unavailable(_))
    ));
}

// --- quota lifecycle reference semantics -----------------------------------

fn quota_changed(quota_id: Option<QuotaId>) -> NotificationEvent {
    NotificationEvent {
        event_id: EventId::generate(),
        kind: NotificationEventKind::QuotaChanged,
        tenant_id: test_tenant(),
        quota_id,
        policy_id: None,
        subject: None,
        payload: serde_json::json!({ "change_kind": "created" }),
        emitted_at: OffsetDateTime::now_utc(),
    }
}

#[tokio::test]
async fn storage_create_fills_the_quota_id_on_events_that_lack_it() {
    let storage = InMemoryStorage::new();
    let id = storage
        .create_quota(
            &ctx(),
            &scope(),
            quota_draft(test_subject("u1"), Some(10)),
            &[quota_changed(None)],
        )
        .await
        .expect("create");
    let events = storage.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].quota_id, Some(id));
}

#[tokio::test]
async fn storage_update_rejects_thresholds_on_an_unbounded_merged_row() {
    let storage = InMemoryStorage::new();
    let mut draft = quota_draft(test_subject("u1"), Some(100));
    draft.notification_thresholds = vec![50];
    let id = storage
        .create_quota(&ctx(), &scope(), draft, &[])
        .await
        .expect("create");

    let unbound_only = storage
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
        .expect_err("thresholds stay, cap goes: I14");
    assert_eq!(unbound_only, StorageError::ThresholdsRequireBoundedCap);
    assert_eq!(
        storage.quota(id).expect("row").record_version,
        1,
        "a rejected patch writes nothing"
    );

    let unbound_and_clear = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                cap: Some(CapPatch::Unbounded),
                notification_thresholds: Some(Vec::new()),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect("both in one patch is consistent");
    assert_eq!(unbound_and_clear.cap, None);
    assert!(unbound_and_clear.notification_thresholds.is_empty());

    let thresholds_on_unbounded = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                notification_thresholds: Some(vec![80]),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect_err("thresholds on an unbounded row: I14");
    assert_eq!(
        thresholds_on_unbounded,
        StorageError::ThresholdsRequireBoundedCap
    );
}

#[tokio::test]
async fn storage_deactivation_is_terminal_returns_held_capacity_and_skips_expired_leases() {
    let storage = InMemoryStorage::new();
    storage
        .bootstrap(&BootstrapBundle::foundation())
        .await
        .expect("bootstrap");
    let id = seeded_quota(&storage, Some(100)).await;
    let ttl = Duration::from_mins(1);

    let expired = storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 30),
            ttl,
            &idem(OperationType::Reserve, "e", 1),
        )
        .await
        .expect("lease that will expire");
    storage.expire_leases();
    let live = storage
        .acquire_lease(
            &ctx(),
            &scope(),
            &applicable(),
            &plan(id, 5),
            ttl,
            &idem(OperationType::Reserve, "l", 1),
        )
        .await
        .expect("live lease");
    assert_eq!(storage.consumed(id), 35);

    let outcome = storage
        .deactivate_quota(&ctx(), &scope(), id, &[])
        .await
        .expect("deactivate");
    assert_eq!(
        outcome.resolved_leases,
        vec![live],
        "expired leases are not resolved (I4)"
    );
    assert_eq!(storage.lease_state(expired), Some(LeaseState::Active));
    assert_eq!(storage.consumed(id), 30, "the live hold was returned");
    let row = storage.quota(id).expect("row");
    assert_eq!(row.status, QuotaStatus::Deactivated);
    assert_eq!(row.record_version, 2);

    let again = storage
        .deactivate_quota(&ctx(), &scope(), id, &[])
        .await
        .expect_err("no second cascade");
    assert_eq!(again, StorageError::QuotaDeactivated { id });
    assert_eq!(storage.quota(id).expect("row").record_version, 2);

    let patched = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                fail_open_hint: Some(true),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect_err("a deactivated Quota accepts no patch");
    assert_eq!(patched, StorageError::QuotaDeactivated { id });
}

#[tokio::test]
async fn storage_active_quota_counts_cover_active_quotas_only() {
    let storage = InMemoryStorage::new();
    let zero = seeded_quota(&storage, Some(0)).await;
    seeded_quota(&storage, None).await;
    seeded_quota(&storage, Some(10)).await;
    let counts = storage.read_active_quota_counts().await.expect("counts");
    assert_eq!((counts.cap_zero, counts.cap_unbounded), (1, 1));
    assert_eq!(counts.by_metric.get(&test_metric()), Some(&3));

    storage
        .deactivate_quota(&ctx(), &scope(), zero, &[])
        .await
        .expect("deactivate");
    let counts = storage.read_active_quota_counts().await.expect("counts");
    assert_eq!((counts.cap_zero, counts.cap_unbounded), (0, 1));
    assert_eq!(counts.by_metric.get(&test_metric()), Some(&2));

    storage.fail_with(StorageError::Unavailable("db down".into()));
    assert!(matches!(
        storage.read_active_quota_counts().await,
        Err(StorageError::Unavailable(_))
    ));
}

#[tokio::test]
async fn storage_read_quotas_filters_and_pages_in_creation_order() {
    let storage = InMemoryStorage::new();
    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(
            storage
                .create_quota(
                    &ctx(),
                    &scope(),
                    quota_draft(test_subject(&format!("u{n}")), Some(n)),
                    &[],
                )
                .await
                .expect("create"),
        );
    }
    storage
        .deactivate_quota(&ctx(), &scope(), ids[4], &[])
        .await
        .expect("deactivate the last one");

    let first = storage
        .read_quotas(
            &ctx(),
            &scope(),
            QuotaFilter::default(),
            PageRequest::first(2),
        )
        .await
        .expect("page 1");
    assert_eq!(
        first.items.iter().map(|q| q.id).collect::<Vec<_>>(),
        ids[..2]
    );
    let second = storage
        .read_quotas(
            &ctx(),
            &scope(),
            QuotaFilter::default(),
            PageRequest {
                limit: 2,
                cursor: first.next_cursor.clone(),
            },
        )
        .await
        .expect("page 2");
    assert_eq!(
        second.items.iter().map(|q| q.id).collect::<Vec<_>>(),
        ids[2..4]
    );
    let third = storage
        .read_quotas(
            &ctx(),
            &scope(),
            QuotaFilter::default(),
            PageRequest {
                limit: 2,
                cursor: second.next_cursor.clone(),
            },
        )
        .await
        .expect("page 3");
    assert_eq!(
        third.items.iter().map(|q| q.id).collect::<Vec<_>>(),
        ids[4..]
    );
    assert!(third.next_cursor.is_none());

    let active = storage
        .read_quotas(
            &ctx(),
            &scope(),
            QuotaFilter {
                status: Some(QuotaStatus::Active),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("active only");
    assert_eq!(active.items.len(), 4);
    let by_subject = storage
        .read_quotas(
            &ctx(),
            &scope(),
            QuotaFilter {
                subject: Some(test_subject("u3")),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("by subject");
    assert_eq!(
        by_subject.items.iter().map(|q| q.id).collect::<Vec<_>>(),
        vec![ids[3]]
    );
    let by_ids = storage
        .read_quotas(
            &ctx(),
            &scope(),
            QuotaFilter {
                ids: vec![ids[0], ids[4]],
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("by ids");
    assert_eq!(by_ids.items.len(), 2, "deactivated rows stay readable");
}

#[tokio::test]
async fn storage_read_quotas_refuses_a_cursor_it_did_not_issue() {
    let storage = InMemoryStorage::new();
    storage
        .create_quota(
            &ctx(),
            &scope(),
            quota_draft(test_subject("u"), Some(1)),
            &[],
        )
        .await
        .expect("created");
    for cursor in ["not-a-cursor", "", "-1", "1.5"] {
        let err = storage
            .read_quotas(
                &ctx(),
                &scope(),
                QuotaFilter::default(),
                PageRequest {
                    limit: 10,
                    cursor: Some(cursor.to_owned()),
                },
            )
            .await
            .expect_err(cursor);
        assert_eq!(err, StorageError::InvalidCursor, "{cursor:?}");
    }
}

#[tokio::test]
async fn storage_update_quota_moves_the_contract_reference_with_the_metadata() {
    let storage = InMemoryStorage::new();
    let id = storage
        .create_quota(
            &ctx(),
            &scope(),
            quota_draft(test_subject("u"), Some(10)),
            &[],
        )
        .await
        .expect("created");
    let before = storage.quota(id).expect("stored").constraint_contract;
    let v2 = crate::models::ContractRef {
        type_id: gts::GtsTypeId::new(
            "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v2~",
        ),
        version: 2,
    };
    let mut metadata = serde_json::Map::new();
    metadata.insert("weight".to_owned(), serde_json::json!(3));

    let err = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                metadata: Some(metadata.clone()),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect_err("metadata without its contract");
    assert!(matches!(err, StorageError::Internal(_)), "{err:?}");
    assert_eq!(storage.quota(id).expect("stored").record_version, 1);

    let updated = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                metadata: Some(metadata),
                constraint_contract: Some(v2.clone()),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect("metadata with its contract");
    assert_ne!(before, v2);
    assert_eq!(updated.constraint_contract, v2);
    assert_eq!(updated.metadata["weight"], serde_json::json!(3));
    let unrelated = storage
        .update_quota(
            &ctx(),
            &scope(),
            id,
            QuotaPatch {
                fail_open_hint: Some(true),
                ..QuotaPatch::default()
            },
            &[],
        )
        .await
        .expect("non-metadata patch");
    assert_eq!(
        unrelated.constraint_contract, v2,
        "other patches leave it alone"
    );
}
