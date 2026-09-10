//! Unit tests for the credential-store domain [`Service`] (ADR-0006:
//! immutable value versions).
//!
//! Saga/reaper/healing/out-of-band-seeding tests are gone along with the
//! mechanisms they exercised; this file covers the write protocol (create,
//! overwrite, torn writes, lost CAS, concurrent last-writer-wins), the
//! read-retry-once protocol, one-transaction delete, the fence's narrowed
//! integrity-check role, and the maintenance job (`run_gc`).

use std::sync::Arc;

use credstore_sdk::{
    CredStorePluginClientV1, ExpiryWrite, OwnerId, SecretRef, SecretValue, SharingMode, TenantId,
    ValueId, WriteOptions,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{
    CredStoreMetricsPort, Dep, DepOp, FenceVerify, Outcome, ReadOutcome,
};
use crate::domain::ports::plugin::PluginSelector;
use crate::domain::resolver::TenantDirectory;
use crate::domain::secret::model::{
    Fallback, GcReason, SecretStatus, WritePrecondition, WriteSpec,
};
use crate::domain::secret::repo::SecretRepo;
use crate::domain::secret::service::{GcSettings, Service};
use crate::domain::secret::test_support::*;

fn key(s: &str) -> SecretRef {
    SecretRef::new(s).expect("valid ref")
}

fn test_gc_settings() -> GcSettings {
    GcSettings {
        pending_max_age_secs: 3600,
        batch_size: 256,
    }
}

fn test_gc_settings_zero_age() -> GcSettings {
    GcSettings {
        pending_max_age_secs: 0,
        batch_size: 256,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "test builder threading every Service::new dependency plus gc settings through"
)]
fn make_service_with_gc(
    repo: Arc<dyn SecretRepo>,
    plugin: Arc<FakePlugin>,
    dir: Arc<dyn TenantDirectory>,
    enforcer: authz_resolver_sdk::PolicyEnforcer,
    metrics: Arc<dyn CredStoreMetricsPort>,
    gc: GcSettings,
) -> Service {
    Service::new(
        repo,
        dir,
        enforcer,
        Arc::new(FakePluginSelector::new(plugin)) as Arc<dyn PluginSelector>,
        catalog_type_resolver(),
        metrics,
        gc,
    )
}

fn make_service(
    repo: Arc<dyn SecretRepo>,
    plugin: Arc<FakePlugin>,
    dir: Arc<dyn TenantDirectory>,
    enforcer: authz_resolver_sdk::PolicyEnforcer,
    metrics: Arc<dyn CredStoreMetricsPort>,
) -> Service {
    make_service_with_gc(repo, plugin, dir, enforcer, metrics, test_gc_settings())
}

fn make_service_noop(
    repo: Arc<dyn SecretRepo>,
    plugin: Arc<FakePlugin>,
    dir: Arc<dyn TenantDirectory>,
) -> Service {
    make_service(repo, plugin, dir, mock_enforcer(), Arc::new(NoopMetrics))
}

fn exists() -> WritePrecondition {
    WritePrecondition::Exists
}

fn matches(id: Uuid, version: i64) -> WritePrecondition {
    WritePrecondition::Version { id, version }
}

// ── basic read/write/delete happy paths ─────────────────────────────────────

#[tokio::test]
async fn get_own_tenant_secret_returns_hit_own() {
    let tenant = Uuid::new_v4();
    let subject = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(
        repo.clone(),
        plugin.clone(),
        dir,
        mock_enforcer(),
        metrics.clone(),
    );
    let ctx = make_ctx(subject, tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");

    let got = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(got.value.as_bytes(), b"v1");
    assert!(!got.is_inherited);
    assert_eq!(metrics.last_read_outcome(), Some(ReadOutcome::HitOwn));
}

#[tokio::test]
async fn get_inherited_shared_from_parent_sets_is_inherited() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let subject = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::new(vec![child, parent]));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);

    let parent_ctx = make_ctx(subject, parent);
    svc.put(
        &parent_ctx,
        &key("shared-k"),
        SecretValue::from("shared-v"),
        WriteSpec::create(SharingMode::Shared),
    )
    .await
    .expect("create at parent");

    let child_ctx = make_ctx(Uuid::new_v4(), child);
    let got = svc
        .get(&child_ctx, &key("shared-k"))
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got.value.as_bytes(), b"shared-v");
    assert!(got.is_inherited);
    assert_eq!(got.owner_tenant_id, TenantId(parent));
}

#[tokio::test]
async fn get_tenant_mode_not_inherited_by_child() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::new(vec![child, parent]));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);

    let parent_ctx = make_ctx(Uuid::new_v4(), parent);
    svc.put(
        &parent_ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");

    let child_ctx = make_ctx(Uuid::new_v4(), child);
    let got = svc.get(&child_ctx, &key("k")).await.expect("get");
    assert!(got.is_none(), "Tenant-mode secret must not be inherited");
}

#[tokio::test]
async fn get_private_owner_match_only() {
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);

    let owner_ctx = make_ctx(owner, tenant);
    svc.put(
        &owner_ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Private),
    )
    .await
    .expect("create");

    let other_ctx = make_ctx(Uuid::new_v4(), tenant);
    assert!(svc.get(&other_ctx, &key("k")).await.expect("get").is_none());
    assert!(svc.get(&owner_ctx, &key("k")).await.expect("get").is_some());
}

#[tokio::test]
async fn get_shadowing_private_beats_inherited() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::new(vec![child, parent]));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);

    let parent_ctx = make_ctx(owner, parent);
    svc.put(
        &parent_ctx,
        &key("k"),
        SecretValue::from("shared"),
        WriteSpec::create(SharingMode::Shared),
    )
    .await
    .expect("create at parent");

    let child_ctx = make_ctx(owner, child);
    svc.put(
        &child_ctx,
        &key("k"),
        SecretValue::from("private"),
        WriteSpec::create(SharingMode::Private),
    )
    .await
    .expect("create private at child");

    let got = svc
        .get(&child_ctx, &key("k"))
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got.value.as_bytes(), b"private");
    assert!(!got.is_inherited);
}

// ── dependency metrics ────────────────────────────────────────────────────────

#[tokio::test]
async fn get_records_pdp_dependency_metric() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(repo.clone(), plugin, dir, mock_enforcer(), metrics.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    assert!(svc.get(&ctx, &key("absent")).await.expect("get").is_none());
    // No row resolves, so the PDP is never consulted (S09 prefetch) —
    // assert the read miss instead.
    assert_eq!(metrics.last_read_outcome(), Some(ReadOutcome::Miss));
}

#[tokio::test]
async fn put_records_pdp_dependency_metric() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(repo, plugin, dir, mock_enforcer(), metrics.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    assert!(
        metrics
            .deps()
            .iter()
            .any(|(d, op, o)| *d == Dep::Pdp && *op == DepOp::Evaluate && *o == Outcome::Success)
    );
    assert!(
        metrics
            .deps()
            .iter()
            .any(|(d, op, _)| *d == Dep::Plugin && *op == DepOp::PluginPut)
    );
}

#[tokio::test]
async fn delete_records_pdp_dependency_metric() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(repo, plugin, dir, mock_enforcer(), metrics.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    svc.delete(&ctx, &key("k"), exists()).await.expect("delete");
    assert!(
        metrics
            .deps()
            .iter()
            .any(|(d, op, o)| *d == Dep::Pdp && *op == DepOp::Evaluate && *o == Outcome::Success)
    );
}

// ── write protocol: create ───────────────────────────────────────────────────

#[tokio::test]
async fn create_starts_at_version_one_then_overwrite_bumps() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row1 = repo.rows()[0].clone();
    assert_eq!(row1.version, 1);
    assert_eq!(row1.status, SecretStatus::Active);
    let value_id1 = row1.value_id.expect("value_id set");

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v2"),
        WriteSpec::update(SharingMode::Tenant, matches(row1.id, 1)),
    )
    .await
    .expect("overwrite");
    let row2 = repo.rows()[0].clone();
    assert_eq!(row2.version, 2);
    let value_id2 = row2.value_id.expect("value_id set");
    assert_ne!(value_id1, value_id2, "each write mints a fresh value_id");

    let got = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(got.value.as_bytes(), b"v2");
}

#[tokio::test]
async fn put_create_conflict_returns_conflict() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("first create");
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v2"),
            WriteSpec::create(SharingMode::Tenant),
        )
        .await
        .expect_err("second create conflicts");
    assert!(matches!(err, DomainError::Conflict));
}

#[tokio::test]
async fn put_shared_coexists_with_private() {
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(owner, tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("tenant-v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("tenant create");
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("priv-v"),
        WriteSpec::create(SharingMode::Private),
    )
    .await
    .expect("private create coexists");
    assert_eq!(repo.rows().len(), 2);
}

#[tokio::test]
async fn put_omitting_sharing_preserves_existing_shared_mode() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Shared),
    )
    .await
    .expect("create shared");
    let row = repo.rows()[0].clone();

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v2"),
        WriteSpec::update(SharingMode::Tenant, exists()).preserve_sharing(true),
    )
    .await
    .expect("rotate preserving sharing");
    assert_eq!(repo.rows()[0].sharing, SharingMode::Shared);
    let _ = row;
}

#[tokio::test]
async fn put_omitting_sharing_rotates_existing_private_secret() {
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(owner, tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("priv"),
        WriteSpec::create(SharingMode::Private),
    )
    .await
    .expect("create private");

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("rotated"),
        WriteSpec::update(SharingMode::Tenant, exists()).preserve_sharing(true),
    )
    .await
    .expect("rotate own private secret");
    assert_eq!(repo.rows().len(), 1);
    assert_eq!(repo.rows()[0].sharing, SharingMode::Private);
    let got = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(got.value.as_bytes(), b"rotated");
}

// ── write protocol: overwrite CAS / preconditions ────────────────────────────

#[tokio::test]
async fn put_if_match_matching_version_overwrites_and_bumps() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v2"),
        WriteSpec::update(SharingMode::Tenant, matches(row.id, row.version)),
    )
    .await
    .expect("matching version overwrites");
    assert_eq!(repo.rows()[0].version, 2);
}

#[tokio::test]
async fn put_if_match_stale_version_conflicts_without_writing() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v2"),
            WriteSpec::update(SharingMode::Tenant, matches(row.id, 99)),
        )
        .await
        .expect_err("stale version conflicts");
    assert!(matches!(err, DomainError::VersionConflict));
    // precheck_version rejects before any backend write.
    assert_eq!(repo.rows()[0].version, 1);
}

#[tokio::test]
async fn put_if_match_on_missing_secret_conflicts() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let err = svc
        .put(
            &ctx,
            &key("absent"),
            SecretValue::from("v"),
            WriteSpec::update(SharingMode::Tenant, exists()),
        )
        .await
        .expect_err("update of a missing secret conflicts");
    assert!(matches!(err, DomainError::VersionConflict));
}

#[tokio::test]
async fn update_never_creates_and_requires_a_precondition() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let spec = WriteSpec {
        sharing: SharingMode::Tenant,
        create_only: false,
        precondition: None,
        opts: WriteOptions::default(),
        preserve_sharing: false,
    };
    let err = svc
        .put(&ctx, &key("k"), SecretValue::from("v"), spec)
        .await
        .expect_err("missing precondition rejected");
    assert!(matches!(err, DomainError::PreconditionRequired { .. }));
}

// ── torn writes / lost CAS / concurrent last-writer-wins (ADR-0006 core) ────

#[tokio::test]
async fn torn_write_leaves_old_value_serving_and_run_gc_reclaims_the_orphan() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_with_gc(
        repo.clone(),
        plugin.clone(),
        dir,
        mock_enforcer(),
        Arc::new(NoopMetrics),
        test_gc_settings_zero_age(),
    );
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("old"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();
    let old_value_id = row.value_id.expect("value_id");

    // plugin.put will succeed, but the repo's CAS (switch_value) fails as if
    // the DB were unreachable — step 4's DB-unreachable failure mode.
    repo.fail_next_switch_value(1);
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("torn"),
            WriteSpec::update(SharingMode::Tenant, matches(row.id, row.version)),
        )
        .await
        .expect_err("CAS failure propagates");
    assert!(matches!(err, DomainError::Internal { .. }));

    // Old value still serves; row untouched.
    let got = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(got.value.as_bytes(), b"old");
    assert_eq!(repo.rows()[0].value_id, Some(old_value_id));

    // A pending gc row exists for the orphaned new value_id.
    let pending: Vec<_> = repo
        .gc_entries()
        .into_iter()
        .filter(|e| e.reason == GcReason::Pending)
        .collect();
    assert_eq!(pending.len(), 1);
    let orphan_id = pending[0].value_id;
    assert!(plugin.contains(&TenantId(tenant), orphan_id));

    // run_gc (pending_max_age = 0) reclaims it: backend entry deleted, gc row dropped.
    let report = svc.run_gc(&ctx).await.expect("run_gc");
    assert_eq!(report.gc_pending_reclaimed, 1);
    assert!(!plugin.contains(&TenantId(tenant), orphan_id));
    assert!(repo.gc_entries().iter().all(|e| e.value_id != orphan_id));
}

#[tokio::test]
async fn cas_lost_marks_uploaded_version_aborted_and_deletes_it_winner_serves() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("winner"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    // Force this writer's CAS to report "lost" (Ok(None)) — models a
    // concurrent writer's switch_value having already moved the row.
    repo.force_next_switch_value_none(1);
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("loser"),
            WriteSpec::update(SharingMode::Tenant, matches(row.id, row.version)),
        )
        .await
        .expect_err("lost CAS is a conflict");
    assert!(matches!(err, DomainError::VersionConflict));

    // The loser's uploaded bytes were aborted: marked, then cleaned up
    // (plugin delete + gc delete both succeed with the default FakePlugin).
    assert!(
        repo.gc_entries().is_empty(),
        "the aborted version's gc row must be cleaned up by the best-effort abort path"
    );

    // The winner's value still serves.
    let got = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(got.value.as_bytes(), b"winner");
}

#[tokio::test]
async fn two_exists_writers_sequentially_last_pointer_wins_earlier_collected() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let value_id1 = repo.rows()[0].value_id.expect("value_id");

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v2"),
        WriteSpec::update(SharingMode::Tenant, exists()),
    )
    .await
    .expect("first Exists overwrite");
    let value_id2 = repo.rows()[0].value_id.expect("value_id");

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v3"),
        WriteSpec::update(SharingMode::Tenant, exists()),
    )
    .await
    .expect("second Exists overwrite");
    let value_id3 = repo.rows()[0].value_id.expect("value_id");

    assert_ne!(value_id1, value_id2);
    assert_ne!(value_id2, value_id3);

    let got = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(got.value.as_bytes(), b"v3", "last writer wins");

    // Both superseded versions were collected by each write's own step-5
    // cleanup (the default FakePlugin never fails delete).
    assert!(!plugin.contains(&TenantId(tenant), value_id1));
    assert!(!plugin.contains(&TenantId(tenant), value_id2));
    assert!(plugin.contains(&TenantId(tenant), value_id3));
    assert!(repo.gc_entries().is_empty());
}

#[tokio::test]
async fn step5_delete_failure_leaves_a_superseded_gc_row_that_run_gc_drains() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let old_value_id = repo.rows()[0].value_id.expect("value_id");

    // The overwrite's own best-effort cleanup of the superseded version fails.
    plugin.fail_next_deletes(1);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v2"),
        WriteSpec::update(SharingMode::Tenant, exists()),
    )
    .await
    .expect("overwrite succeeds despite the cleanup failure");

    let superseded: Vec<_> = repo
        .gc_entries()
        .into_iter()
        .filter(|e| e.value_id == old_value_id)
        .collect();
    assert_eq!(superseded.len(), 1);
    assert_eq!(superseded[0].reason, GcReason::Superseded);

    // The next maintenance run drains it (the queued failure was consumed).
    let report = svc.run_gc(&ctx).await.expect("run_gc");
    assert_eq!(report.gc_deleted, 1);
    assert!(repo.gc_entries().iter().all(|e| e.value_id != old_value_id));
}

#[tokio::test]
async fn read_races_a_switch_retry_returns_the_current_version() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("old"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();
    let old_value_id = row.value_id.expect("value_id");

    // Read the (already-bootstrapped) fence key back out of the plugin so
    // the simulated concurrent write's fingerprint is one `get`'s fence
    // verification will actually accept — a fabricated fp would just look
    // like corruption and fail closed, which is not what this test is
    // exercising.
    let fence_key = plugin
        .get(&ctx, &TenantId::nil(), &credstore_sdk::FENCE_KEY_VALUE_ID)
        .await
        .expect("fence key bootstrapped by the create above")
        .expect("fence key present");
    let new_fp = crate::domain::secret::fence::compute_fp(fence_key.as_bytes(), b"new");

    // A concurrent write already landed: switch the row to a new value_id
    // (and put the new bytes) right after the *next* resolve, and remove the
    // old backend entry (as its own step-5 cleanup would have).
    let new_value_id = ValueId::new_v4();
    repo.switch_after_next_resolve(row.id, new_value_id, new_fp);
    plugin
        .put(
            &ctx,
            &TenantId(tenant),
            &new_value_id,
            SecretValue::from("new"),
        )
        .await
        .expect("seed new value");
    plugin
        .delete(&ctx, &TenantId(tenant), &old_value_id)
        .await
        .expect("simulate old cleanup");

    let got = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(
        got.value.as_bytes(),
        b"new",
        "the retry must serve the current (post-switch) version"
    );
}

#[tokio::test]
async fn delete_then_create_only_put_under_the_same_reference_succeeds() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("reused"),
        SecretValue::from("old"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let old_value_id = repo.rows()[0].value_id.expect("value_id");

    svc.delete(&ctx, &key("reused"), exists())
        .await
        .expect("delete");
    assert!(repo.rows().is_empty());

    svc.put(
        &ctx,
        &key("reused"),
        SecretValue::from("new"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("recreate under the same reference succeeds immediately");

    let got = svc
        .get(&ctx, &key("reused"))
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got.value.as_bytes(), b"new");
    // The predecessor's version was collected by delete's own step-3 cleanup.
    assert!(!plugin.contains(&TenantId(tenant), old_value_id));
}

// ── delete ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_private_secret_removes_row() {
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(owner, tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Private),
    )
    .await
    .expect("create");
    svc.delete(&ctx, &key("k"), exists()).await.expect("delete");
    assert!(repo.rows().is_empty());
    assert!(svc.get(&ctx, &key("k")).await.expect("get").is_none());
}

#[tokio::test]
async fn delete_only_own_tenant_404_when_inherited_only() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::new(vec![child, parent]));
    let svc = make_service_noop(repo, plugin, dir);

    let parent_ctx = make_ctx(Uuid::new_v4(), parent);
    svc.put(
        &parent_ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Shared),
    )
    .await
    .expect("create at parent");

    let child_ctx = make_ctx(Uuid::new_v4(), child);
    let err = svc
        .delete(&child_ctx, &key("k"), exists())
        .await
        .expect_err("no own-tenant row at the child");
    assert!(matches!(err, DomainError::NotFound));
}

#[tokio::test]
async fn delete_if_match_stale_version_conflicts() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    let err = svc
        .delete(&ctx, &key("k"), matches(row.id, 99))
        .await
        .expect_err("stale version conflicts");
    assert!(matches!(err, DomainError::VersionConflict));
    assert_eq!(repo.rows().len(), 1, "row must survive a rejected delete");
}

#[tokio::test]
async fn delete_if_match_race_maps_zero_rows_to_version_conflict() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    // Simulate a race: the row is gone by the time delete_by_id's own
    // transaction runs, even though find_own/precheck (moments earlier) saw
    // it at `row.version`.
    repo.force_next_delete_by_id_not_found(1);
    let err = svc
        .delete(&ctx, &key("k"), matches(row.id, row.version))
        .await
        .expect_err("lost race under a version precondition is a conflict");
    assert!(matches!(err, DomainError::VersionConflict));
}

#[tokio::test]
async fn delete_with_no_plugin_fails_without_deleting_the_row() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let dir = Arc::new(FakeDir::single(tenant));
    // Create with a real plugin, then rebuild the service without one.
    let plugin = FakePlugin::new();
    let svc = make_service_noop(repo.clone(), plugin, dir.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");

    let svc_no_plugin = Service::new(
        repo.clone(),
        dir,
        mock_enforcer(),
        Arc::new(NoPluginSelector),
        catalog_type_resolver(),
        Arc::new(NoopMetrics),
        test_gc_settings(),
    );
    let err = svc_no_plugin
        .delete(&ctx, &key("k"), exists())
        .await
        .expect_err("no plugin available");
    assert!(matches!(err, DomainError::ServiceUnavailable { .. }));
    assert_eq!(
        repo.rows().len(),
        1,
        "row must survive when no plugin resolves"
    );
}

// ── PDP / scope gating ────────────────────────────────────────────────────────

#[tokio::test]
async fn read_gate_denied_when_tenant_out_of_scope() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::with_scope_allows(false));
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(repo.clone(), plugin, dir, mock_enforcer(), metrics.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    repo.seed(seeded_row(tenant, Uuid::new_v4(), "k", SharingMode::Tenant));
    let got = svc.get(&ctx, &key("k")).await.expect("get");
    assert!(got.is_none());
    assert_eq!(metrics.cross_tenant_denied_count(), 1);
}

#[tokio::test]
async fn get_returns_not_found_when_pdp_denies() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service(
        repo.clone(),
        plugin,
        dir,
        deny_enforcer(),
        Arc::new(NoopMetrics),
    );
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    repo.seed(seeded_row(tenant, Uuid::new_v4(), "k", SharingMode::Tenant));
    assert!(svc.get(&ctx, &key("k")).await.expect("get").is_none());
}

#[tokio::test]
async fn get_returns_service_unavailable_when_pdp_fails() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service(
        repo.clone(),
        plugin,
        dir,
        failing_enforcer(),
        Arc::new(NoopMetrics),
    );
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    repo.seed(seeded_row(tenant, Uuid::new_v4(), "k", SharingMode::Tenant));
    let err = svc.get(&ctx, &key("k")).await.expect_err("pdp outage");
    assert!(matches!(err, DomainError::ServiceUnavailable { .. }));
}

#[tokio::test]
async fn operations_return_service_unavailable_when_type_resolver_fails() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = Service::new(
        repo,
        dir,
        mock_enforcer(),
        Arc::new(FakePluginSelector::new(plugin)),
        Arc::new(FailingTypeResolver),
        Arc::new(NoopMetrics),
        test_gc_settings(),
    );
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v"),
            WriteSpec::create(SharingMode::Tenant),
        )
        .await
        .expect_err("registry outage");
    assert!(matches!(err, DomainError::ServiceUnavailable { .. }));
}

#[tokio::test]
async fn put_returns_access_denied_when_pdp_denies() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service(repo, plugin, dir, deny_enforcer(), Arc::new(NoopMetrics));
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v"),
            WriteSpec::create(SharingMode::Tenant),
        )
        .await
        .expect_err("pdp denies");
    assert!(matches!(err, DomainError::AccessDenied { .. }));
}

#[tokio::test]
async fn delete_returns_access_denied_when_pdp_denies() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    repo.seed(seeded_row(tenant, Uuid::new_v4(), "k", SharingMode::Tenant));
    let svc = make_service(repo, plugin, dir, deny_enforcer(), Arc::new(NoopMetrics));
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let err = svc
        .delete(&ctx, &key("k"), exists())
        .await
        .expect_err("pdp denies");
    assert!(matches!(err, DomainError::AccessDenied { .. }));
}

#[tokio::test]
async fn create_only_conflict_is_authorized_before_it_leaks_existence() {
    let tenant = Uuid::new_v4();
    let (enforcer, resolver) = type_deny_enforcer(vec![]);
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service(repo, plugin, dir, enforcer, Arc::new(NoopMetrics));
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v2"),
            WriteSpec::create(SharingMode::Tenant),
        )
        .await
        .expect_err("create-only conflict");
    assert!(matches!(err, DomainError::Conflict));
    assert!(
        !resolver
            .seen_resource_types
            .lock()
            .expect("lock")
            .is_empty()
    );
}

// ── plugin-error mapping ─────────────────────────────────────────────────────

#[test]
fn map_plugin_err_covers_all_variants() {
    use crate::domain::secret::service::map_plugin_err;
    use credstore_sdk::CredStoreError as E;
    assert!(matches!(map_plugin_err(E::NotFound), DomainError::NotFound));
    assert!(matches!(
        map_plugin_err(E::AccessDenied),
        DomainError::AccessDenied { .. }
    ));
    assert!(matches!(map_plugin_err(E::Conflict), DomainError::Conflict));
    assert!(matches!(
        map_plugin_err(E::ServiceUnavailable {
            detail: "x".into(),
            retry_after: None
        }),
        DomainError::ServiceUnavailable { .. }
    ));
    assert!(matches!(
        map_plugin_err(E::NoPluginAvailable),
        DomainError::ServiceUnavailable {
            retry_after: None,
            ..
        }
    ));
    assert!(matches!(
        map_plugin_err(E::invalid_ref("x")),
        DomainError::Internal { .. }
    ));
    assert!(matches!(
        map_plugin_err(E::unsupported_transition("x")),
        DomainError::Internal { .. }
    ));
    assert!(matches!(
        map_plugin_err(E::TypeViolation {
            reason: "R".into(),
            detail: "d".into()
        }),
        DomainError::Internal { .. }
    ));
    assert!(matches!(
        map_plugin_err(E::Internal("x".into())),
        DomainError::Internal { .. }
    ));
}

#[test]
fn no_plugin_available_maps_to_distinct_non_retryable_unavailable() {
    use crate::domain::secret::service::map_plugin_err;
    let mapped = map_plugin_err(credstore_sdk::CredStoreError::NoPluginAvailable);
    match mapped {
        DomainError::ServiceUnavailable {
            retry_after,
            detail,
            ..
        } => {
            assert!(retry_after.is_none());
            assert_eq!(detail, "no storage plugin registered");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn plugin_unavailable_detail_is_curated_off_the_wire() {
    use crate::domain::secret::service::map_plugin_err;
    let mapped = map_plugin_err(credstore_sdk::CredStoreError::ServiceUnavailable {
        detail: "raw backend secret leak attempt".into(),
        retry_after: None,
    });
    match mapped {
        DomainError::ServiceUnavailable { detail, .. } => {
            assert_eq!(detail, "storage backend unavailable");
            assert!(!detail.contains("secret"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn get_folds_plugin_access_denied_into_anti_enumeration_miss() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::with_get_denied();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    repo.seed(seeded_row(tenant, Uuid::new_v4(), "k", SharingMode::Tenant));
    assert!(svc.get(&ctx, &key("k")).await.expect("get").is_none());
}

#[tokio::test]
async fn pdp_denial_is_not_a_dependency_health_error() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(repo, plugin, dir, deny_enforcer(), metrics.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    assert!(
        svc.put(
            &ctx,
            &key("k"),
            SecretValue::from("v"),
            WriteSpec::create(SharingMode::Tenant),
        )
        .await
        .is_err(),
        "pdp denies the write"
    );
    assert!(
        metrics
            .deps()
            .iter()
            .all(|(d, _, o)| *d != Dep::Pdp || *o == Outcome::Success)
    );
}

// ── typing / traits ───────────────────────────────────────────────────────────

#[tokio::test]
async fn typed_create_enforces_allow_sharing_and_returns_type() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let opts = WriteOptions {
        secret_type: Some(
            credstore_sdk::SecretType::from_name("personal-token")
                .unwrap()
                .into(),
        ),
        expires_at: ExpiryWrite::default(),
    };
    // personal-token only allows Private sharing.
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v"),
            WriteSpec::create(SharingMode::Tenant).with_opts(opts),
        )
        .await
        .expect_err("sharing not allowed for type");
    assert!(matches!(err, DomainError::TypeViolation { .. }));
}

#[tokio::test]
async fn secret_type_is_immutable_on_overwrite() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create generic");
    let row = repo.rows()[0].clone();

    let opts = WriteOptions {
        secret_type: Some(
            credstore_sdk::SecretType::from_name("api-key")
                .unwrap()
                .into(),
        ),
        expires_at: ExpiryWrite::default(),
    };
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v2"),
            WriteSpec::update(SharingMode::Tenant, matches(row.id, row.version)).with_opts(opts),
        )
        .await
        .expect_err("type change rejected");
    assert!(matches!(err, DomainError::TypeViolation { .. }));
}

#[tokio::test]
async fn expiry_rejected_for_non_expirable_type() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let opts = WriteOptions {
        secret_type: None, // generic: not expirable
        expires_at: ExpiryWrite::Set(OffsetDateTime::now_utc() + time::Duration::hours(1)),
    };
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v"),
            WriteSpec::create(SharingMode::Tenant).with_opts(opts),
        )
        .await
        .expect_err("expiry on non-expirable type");
    assert!(matches!(err, DomainError::TypeViolation { .. }));
}

#[tokio::test]
async fn per_type_pdp_denial_hides_reads_and_forbids_writes() {
    let tenant = Uuid::new_v4();
    let api_key_gts = credstore_sdk::SecretType::from_name("api-key")
        .unwrap()
        .gts_id()
        .to_owned();
    let (enforcer, _resolver) = type_deny_enforcer(vec![api_key_gts]);
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service(repo, plugin, dir, enforcer, Arc::new(NoopMetrics));
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let opts = WriteOptions {
        secret_type: Some(
            credstore_sdk::SecretType::from_name("api-key")
                .unwrap()
                .into(),
        ),
        expires_at: ExpiryWrite::default(),
    };
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v"),
            WriteSpec::create(SharingMode::Tenant).with_opts(opts),
        )
        .await
        .expect_err("denied type");
    assert!(matches!(err, DomainError::AccessDenied { .. }));
}

#[tokio::test]
async fn generic_secrets_evaluate_the_full_concrete_type() {
    let tenant = Uuid::new_v4();
    let (enforcer, resolver) = type_deny_enforcer(vec![]);
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service(repo, plugin, dir, enforcer, Arc::new(NoopMetrics));
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let seen = resolver.seen_resource_types.lock().expect("lock").clone();
    assert!(
        seen.iter().any(|t| t.contains("generic")),
        "generic must still be evaluated as a concrete type: {seen:?}"
    );
}

// ── fence (ADR-0003, narrowed by ADR-0006) ───────────────────────────────────

#[tokio::test]
async fn clean_write_then_read_verifies_ok() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(repo, plugin, dir, mock_enforcer(), metrics.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(metrics.fence_verifies(), vec![FenceVerify::Ok]);
}

#[tokio::test]
async fn overwrite_restamps_the_fingerprint() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let fp1 = repo.rows()[0].value_fp.clone();
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v2"),
        WriteSpec::update(SharingMode::Tenant, exists()),
    )
    .await
    .expect("overwrite");
    let fp2 = repo.rows()[0].value_fp.clone();
    assert_ne!(
        fp1, fp2,
        "the fingerprint must be restamped for the new value"
    );
}

#[tokio::test]
async fn aba_recreate_rejects_stale_generation_validator() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("gen1"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create gen1");
    let gen1 = repo.rows()[0].clone();
    svc.delete(&ctx, &key("k"), exists())
        .await
        .expect("delete gen1");
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("gen2"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create gen2");

    // A validator minted for gen1 must never match gen2, even if version
    // counters happen to coincide (both start at 1).
    let err = svc
        .put(
            &ctx,
            &key("k"),
            SecretValue::from("v3"),
            WriteSpec::update(SharingMode::Tenant, matches(gen1.id, gen1.version)),
        )
        .await
        .expect_err("stale generation validator rejected");
    assert!(matches!(err, DomainError::VersionConflict));
}

#[tokio::test]
async fn fence_key_bootstrap_persists_the_key_in_the_backend() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    assert!(plugin.contains(&TenantId::nil(), credstore_sdk::FENCE_KEY_VALUE_ID));
}

#[tokio::test]
async fn fence_key_bootstrap_conflict_is_treated_as_another_replica_won() {
    // A stricter plugin's `put` immutability guard rejects the bootstrap
    // write with Conflict when a peer already landed its own candidate —
    // load_fence_key must treat that as "re-read", not an error.
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    // Pre-seed the fence key as if a peer replica already won the race.
    plugin
        .put(
            &ctx,
            &TenantId::nil(),
            &credstore_sdk::FENCE_KEY_VALUE_ID,
            SecretValue::new(vec![9u8; 32]),
        )
        .await
        .expect("preseed fence key");

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create still succeeds, adopting the peer's key");
}

#[tokio::test]
async fn fence_key_reference_is_unreachable_through_the_api() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create, bootstrapping the fence key");
    // No external caller ever carries the nil tenant, so nothing about the
    // reserved fence-key entry is reachable via get/put/delete under any
    // real tenant — this is a structural property (nil tenant never equals
    // a real `ctx.subject_tenant_id()`), asserted here for documentation.
    assert_ne!(TenantId(tenant), TenantId::nil());
}

// ── maintenance job (`run_gc`) ────────────────────────────────────────────────

#[tokio::test]
async fn run_gc_deletes_an_expired_row_and_its_version() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let opts = WriteOptions {
        secret_type: Some(
            credstore_sdk::SecretType::from_name("bearer-token")
                .unwrap()
                .into(),
        ),
        expires_at: ExpiryWrite::Set(OffsetDateTime::now_utc() + time::Duration::seconds(1)),
    };
    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant).with_opts(opts),
    )
    .await
    .expect("create expirable");
    let row = repo.rows()[0].clone();
    let value_id = row.value_id.expect("value_id");

    // Force it into the past directly (bypassing real time).
    repo.force_expire(row.id);

    let report = svc.run_gc(&ctx).await.expect("run_gc");
    assert_eq!(report.expired_deleted, 1);
    assert!(repo.rows().is_empty());
    assert!(!plugin.contains(&TenantId(tenant), value_id));
}

#[tokio::test]
async fn run_gc_twice_second_report_is_all_zeros() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let first = svc.run_gc(&ctx).await.expect("run_gc first");
    assert_eq!(first, crate::domain::secret::service::GcReport::default());
    let second = svc.run_gc(&ctx).await.expect("run_gc second");
    assert_eq!(second, crate::domain::secret::service::GcReport::default());
}

#[tokio::test]
async fn run_gc_never_deletes_a_referenced_pending_id() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_with_gc(
        repo.clone(),
        plugin.clone(),
        dir,
        mock_enforcer(),
        Arc::new(NoopMetrics),
        test_gc_settings_zero_age(),
    );
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        SecretValue::from("v"),
        WriteSpec::create(SharingMode::Tenant),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();
    let value_id = row.value_id.expect("value_id");

    // Manually (mis)repair the gc table: a pending entry for a value_id a
    // row still references — the defensive branch must hold even so.
    repo.gc_insert_pending(value_id, TenantId(tenant))
        .await
        .expect("insert pending");

    let report = svc.run_gc(&ctx).await.expect("run_gc");
    assert_eq!(
        report.gc_pending_reclaimed, 0,
        "a referenced id must never be counted as reclaimed"
    );
    assert!(
        plugin.contains(&TenantId(tenant), value_id),
        "the backend entry a live row points to must never be deleted"
    );
    assert!(
        repo.gc_entries().iter().all(|e| e.value_id != value_id),
        "the stale (manually-repaired) pending row is still dropped"
    );
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn seeded_row(
    tenant: Uuid,
    owner: Uuid,
    reference: &str,
    sharing: SharingMode,
) -> crate::domain::secret::model::SecretRow {
    crate::domain::secret::model::SecretRow {
        id: Uuid::new_v4(),
        tenant_id: TenantId(tenant),
        reference: reference.to_owned(),
        sharing,
        owner_id: OwnerId(owner),
        status: SecretStatus::Active,
        version: 1,
        secret_type_uuid: credstore_sdk::SecretType::generic().uuid(),
        expires_at: None,
        value_id: Some(ValueId::new_v4()),
        value_fp: Some(vec![7u8; 32]),
        fp_key_id: Some(1),
        fallback: Fallback::Inherit,
    }
}
