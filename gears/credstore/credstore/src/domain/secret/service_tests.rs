//! Unit tests for the credential-store domain [`Service`] (ADR-0004: the
//! credential surface; ADR-0006: immutable value versions).
//!
//! Saga/reaper/healing/out-of-band-seeding tests are gone along with the
//! mechanisms they exercised; this file covers the credential/secret split
//! (`get`/`get_secret`), the merged write surface (`put`/`patch`), suppression
//! (`fallback`), the write protocol (create, overwrite, torn writes, lost CAS,
//! concurrent last-writer-wins), the read-retry-once protocol, one-transaction
//! delete, the fence's narrowed integrity-check role, and the maintenance job
//! (`run_gc`).

use std::sync::Arc;

use credstore_sdk::{
    CredStorePluginClientV1, Fallback as SdkFallback, InheritanceStatus, OwnerId, PatchField,
    SecretRef, SecretType, SecretValue, SharingMode, TenantId, ValueId,
};
use credstore_sdk::{CredentialPatch, CredentialStatus, CredentialWrite};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{
    CredStoreMetricsPort, Dep, DepOp, FenceVerify, Outcome, ReadOutcome,
};
use crate::domain::ports::plugin::PluginSelector;
use crate::domain::resolver::TenantDirectory;
use crate::domain::secret::model::{
    Fallback, GcReason, PutPrecondition, SecretStatus, WritePrecondition,
};
use crate::domain::secret::repo::SecretRepo;
use crate::domain::secret::service::{GcSettings, ListSettings, Service};
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

fn test_list_settings() -> ListSettings {
    ListSettings {
        max_limit: 200,
        value_mode_cap: 25,
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
        test_list_settings(),
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

// ── WritePrecondition (patch/delete) helpers ────────────────────────────────

fn exists() -> WritePrecondition {
    WritePrecondition::Exists
}

fn matches(id: Uuid, version: i64) -> WritePrecondition {
    WritePrecondition::Version { id, version }
}

// ── PutPrecondition (put) helpers ───────────────────────────────────────────

fn create_only() -> PutPrecondition {
    PutPrecondition::CreateOnly
}

fn put_exists() -> PutPrecondition {
    PutPrecondition::Exists
}

fn put_matches(id: Uuid, version: i64) -> PutPrecondition {
    PutPrecondition::Version { id, version }
}

// ── CredentialWrite / CredentialPatch builders ──────────────────────────────

/// A `CredentialWrite` for a create (`secret_type` required by ADR-0004),
/// generic type, `fallback: inherit`, no expiry.
fn write_create(sharing: SharingMode, value: &str) -> CredentialWrite {
    write_create_typed(sharing, value, "generic")
}

fn write_create_typed(sharing: SharingMode, value: &str, type_name: &str) -> CredentialWrite {
    CredentialWrite {
        secret_type: Some(SecretType::from_name(type_name).expect("known type").into()),
        sharing,
        fallback: SdkFallback::Inherit,
        expires_at: None,
        value: SecretValue::from(value),
    }
}

/// A `CredentialWrite` for a replace (`secret_type: None` — must equal
/// stored), generic defaults otherwise.
fn write_replace(sharing: SharingMode, value: &str) -> CredentialWrite {
    CredentialWrite {
        secret_type: None,
        sharing,
        fallback: SdkFallback::Inherit,
        expires_at: None,
        value: SecretValue::from(value),
    }
}

fn empty_patch() -> CredentialPatch {
    CredentialPatch {
        secret_type: None,
        sharing: None,
        fallback: None,
        expires_at: PatchField::Absent,
        value: PatchField::Absent,
    }
}

fn patch_value(value: &str) -> CredentialPatch {
    CredentialPatch {
        value: PatchField::Set(SecretValue::from(value)),
        ..empty_patch()
    }
}

fn patch_sharing(sharing: SharingMode) -> CredentialPatch {
    CredentialPatch {
        sharing: Some(sharing),
        ..empty_patch()
    }
}

fn patch_suppress() -> CredentialPatch {
    CredentialPatch {
        fallback: Some(SdkFallback::None),
        value: PatchField::Null,
        ..empty_patch()
    }
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
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("create");

    let got = svc
        .get_secret(&ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
    assert_eq!(got.value.as_bytes(), b"v1");
    assert_eq!(metrics.last_read_outcome(), Some(ReadOutcome::HitOwn));

    let cred = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(cred.inheritance, InheritanceStatus::Own);
    assert_eq!(cred.status, CredentialStatus::Active);
}

#[tokio::test]
async fn get_inherited_shared_from_parent_sets_inherited_status() {
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
        write_create(SharingMode::Shared, "shared-v"),
        create_only(),
    )
    .await
    .expect("create at parent");

    let child_ctx = make_ctx(Uuid::new_v4(), child);
    let got = svc
        .get_secret(&child_ctx, &key("shared-k"))
        .await
        .expect("get_secret")
        .expect("some");
    assert_eq!(got.value.as_bytes(), b"shared-v");

    let cred = svc
        .get(&child_ctx, &key("shared-k"))
        .await
        .expect("get")
        .expect("some");
    assert_eq!(cred.inheritance, InheritanceStatus::Inherited);
    assert_eq!(cred.status, CredentialStatus::None);
    assert!(
        cred.validator.is_none(),
        "no own row => no strong validator"
    );
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");

    let child_ctx = make_ctx(Uuid::new_v4(), child);
    let got = svc
        .get_secret(&child_ctx, &key("k"))
        .await
        .expect("get_secret");
    assert!(got.is_none(), "Tenant-mode secret must not be inherited");
    assert!(svc.get(&child_ctx, &key("k")).await.expect("get").is_none());
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
        write_create(SharingMode::Private, "v"),
        create_only(),
    )
    .await
    .expect("create");

    let other_ctx = make_ctx(Uuid::new_v4(), tenant);
    assert!(
        svc.get_secret(&other_ctx, &key("k"))
            .await
            .expect("get_secret")
            .is_none()
    );
    assert!(
        svc.get_secret(&owner_ctx, &key("k"))
            .await
            .expect("get_secret")
            .is_some()
    );
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
        write_create(SharingMode::Shared, "shared"),
        create_only(),
    )
    .await
    .expect("create at parent");

    let child_ctx = make_ctx(owner, child);
    svc.put(
        &child_ctx,
        &key("k"),
        write_create(SharingMode::Private, "private"),
        create_only(),
    )
    .await
    .expect("create private at child");

    let got = svc
        .get_secret(&child_ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
    assert_eq!(got.value.as_bytes(), b"private");
    let cred = svc
        .get(&child_ctx, &key("k"))
        .await
        .expect("get")
        .expect("some");
    // The child's own private row wins resolution, but the parent's shared
    // row under the same reference is a resolvable ancestor candidate, so
    // the own row *shadows* it rather than simply "being the only option":
    // per ADR-0004 that is `Overridden`, not `Own`.
    assert_eq!(cred.inheritance, InheritanceStatus::Overridden);
}

// ── dependency metrics ────────────────────────────────────────────────────────

#[tokio::test]
async fn get_secret_records_pdp_dependency_metric() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let metrics = FakeMetrics::new();
    let svc = make_service(repo.clone(), plugin, dir, mock_enforcer(), metrics.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    assert!(
        svc.get_secret(&ctx, &key("absent"))
            .await
            .expect("get_secret")
            .is_none()
    );
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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

    let outcome = svc
        .put(
            &ctx,
            &key("k"),
            write_create(SharingMode::Tenant, "v1"),
            create_only(),
        )
        .await
        .expect("create");
    assert!(outcome.created);
    assert_eq!(outcome.validator.version, 1);
    let row1 = repo.rows()[0].clone();
    assert_eq!(row1.version, 1);
    assert_eq!(row1.status, SecretStatus::Active);
    let value_id1 = row1.value_id.expect("value_id set");

    let outcome2 = svc
        .put(
            &ctx,
            &key("k"),
            write_replace(SharingMode::Tenant, "v2"),
            put_matches(row1.id, 1),
        )
        .await
        .expect("overwrite");
    assert!(!outcome2.created);
    assert_eq!(outcome2.validator.version, 2);
    let row2 = repo.rows()[0].clone();
    assert_eq!(row2.version, 2);
    let value_id2 = row2.value_id.expect("value_id set");
    assert_ne!(value_id1, value_id2, "each write mints a fresh value_id");

    let got = svc
        .get_secret(&ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
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
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("first create");
    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_create(SharingMode::Tenant, "v2"),
            create_only(),
        )
        .await
        .expect_err("second create conflicts");
    assert!(matches!(err, DomainError::Conflict));
}

#[tokio::test]
async fn put_create_without_type_is_rejected() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let write = CredentialWrite {
        secret_type: None,
        sharing: SharingMode::Tenant,
        fallback: SdkFallback::Inherit,
        expires_at: None,
        value: SecretValue::from("v"),
    };
    let err = svc
        .put(&ctx, &key("k"), write, create_only())
        .await
        .expect_err("type is required on create");
    assert!(matches!(
        err,
        DomainError::InvalidRequest {
            reason: crate::domain::secret::typing::reasons::TYPE_REQUIRED,
            ..
        }
    ));
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
        write_create(SharingMode::Tenant, "tenant-v"),
        create_only(),
    )
    .await
    .expect("tenant create");
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Private, "priv-v"),
        create_only(),
    )
    .await
    .expect("private create coexists");
    assert_eq!(repo.rows().len(), 2);
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
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    svc.put(
        &ctx,
        &key("k"),
        write_replace(SharingMode::Tenant, "v2"),
        put_matches(row.id, row.version),
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
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_replace(SharingMode::Tenant, "v2"),
            put_matches(row.id, 99),
        )
        .await
        .expect_err("stale version conflicts");
    assert!(matches!(err, DomainError::VersionConflict));
    // precheck rejects before any backend write.
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
            write_replace(SharingMode::Tenant, "v"),
            put_exists(),
        )
        .await
        .expect_err("update of a missing secret conflicts");
    assert!(matches!(err, DomainError::VersionConflict));
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
        write_create(SharingMode::Tenant, "old"),
        create_only(),
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
            write_replace(SharingMode::Tenant, "torn"),
            put_matches(row.id, row.version),
        )
        .await
        .expect_err("CAS failure propagates");
    assert!(matches!(err, DomainError::Internal { .. }));

    // Old value still serves; row untouched.
    let got = svc
        .get_secret(&ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
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
        write_create(SharingMode::Tenant, "winner"),
        create_only(),
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
            write_replace(SharingMode::Tenant, "loser"),
            put_matches(row.id, row.version),
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
    let got = svc
        .get_secret(&ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
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
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("create");
    let value_id1 = repo.rows()[0].value_id.expect("value_id");

    svc.put(
        &ctx,
        &key("k"),
        write_replace(SharingMode::Tenant, "v2"),
        put_exists(),
    )
    .await
    .expect("first Exists overwrite");
    let value_id2 = repo.rows()[0].value_id.expect("value_id");

    svc.put(
        &ctx,
        &key("k"),
        write_replace(SharingMode::Tenant, "v3"),
        put_exists(),
    )
    .await
    .expect("second Exists overwrite");
    let value_id3 = repo.rows()[0].value_id.expect("value_id");

    assert_ne!(value_id1, value_id2);
    assert_ne!(value_id2, value_id3);

    let got = svc
        .get_secret(&ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
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
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("create");
    let old_value_id = repo.rows()[0].value_id.expect("value_id");

    // The overwrite's own best-effort cleanup of the superseded version fails.
    plugin.fail_next_deletes(1);

    svc.put(
        &ctx,
        &key("k"),
        write_replace(SharingMode::Tenant, "v2"),
        put_exists(),
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
        write_create(SharingMode::Tenant, "old"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();
    let old_value_id = row.value_id.expect("value_id");

    // Read the (already-bootstrapped) fence key back out of the plugin so
    // the simulated concurrent write's fingerprint is one `get_secret`'s
    // fence verification will actually accept — a fabricated fp would just
    // look like corruption and fail closed, which is not what this test is
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

    let got = svc
        .get_secret(&ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
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
        write_create(SharingMode::Tenant, "old"),
        create_only(),
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
        write_create(SharingMode::Tenant, "new"),
        create_only(),
    )
    .await
    .expect("recreate under the same reference succeeds immediately");

    let got = svc
        .get_secret(&ctx, &key("reused"))
        .await
        .expect("get_secret")
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
        write_create(SharingMode::Private, "v"),
        create_only(),
    )
    .await
    .expect("create");
    svc.delete(&ctx, &key("k"), exists()).await.expect("delete");
    assert!(repo.rows().is_empty());
    assert!(
        svc.get_secret(&ctx, &key("k"))
            .await
            .expect("get_secret")
            .is_none()
    );
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
        write_create(SharingMode::Shared, "v"),
        create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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
        test_list_settings(),
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
    let got = svc.get_secret(&ctx, &key("k")).await.expect("get_secret");
    assert!(got.is_none());
    assert_eq!(metrics.cross_tenant_denied_count(), 1);
}

#[tokio::test]
async fn get_secret_returns_not_found_when_pdp_denies() {
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
    assert!(
        svc.get_secret(&ctx, &key("k"))
            .await
            .expect("get_secret")
            .is_none()
    );
}

#[tokio::test]
async fn get_secret_returns_service_unavailable_when_pdp_fails() {
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
    let err = svc
        .get_secret(&ctx, &key("k"))
        .await
        .expect_err("pdp outage");
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
        test_list_settings(),
    );
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_create(SharingMode::Tenant, "v"),
            create_only(),
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
            write_create(SharingMode::Tenant, "v"),
            create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_create(SharingMode::Tenant, "v2"),
            create_only(),
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
#[allow(
    clippy::cognitive_complexity,
    reason = "a flat enumeration of every PluginError variant's DomainError mapping; splitting \
              it would only scatter the one-to-one correspondence this test is checking"
)]
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
        map_plugin_err(E::InvalidRequest {
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
async fn get_secret_folds_plugin_access_denied_into_anti_enumeration_miss() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::with_get_denied();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    repo.seed(seeded_row(tenant, Uuid::new_v4(), "k", SharingMode::Tenant));
    assert!(
        svc.get_secret(&ctx, &key("k"))
            .await
            .expect("get_secret")
            .is_none()
    );
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
            write_create(SharingMode::Tenant, "v"),
            create_only()
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

    // personal-token only allows Private sharing.
    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_create_typed(SharingMode::Tenant, "v", "personal-token"),
            create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create generic");
    let row = repo.rows()[0].clone();

    let write = CredentialWrite {
        secret_type: Some(SecretType::from_name("api-key").expect("known").into()),
        ..write_replace(SharingMode::Tenant, "v2")
    };
    let err = svc
        .put(&ctx, &key("k"), write, put_matches(row.id, row.version))
        .await
        .expect_err("type change rejected");
    assert!(matches!(
        err,
        DomainError::TypeViolation {
            reason: crate::domain::secret::typing::reasons::TYPE_IMMUTABLE,
            ..
        }
    ));
}

#[tokio::test]
async fn expiry_rejected_for_non_expirable_type() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let write = CredentialWrite {
        expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
        ..write_create(SharingMode::Tenant, "v") // generic: not expirable
    };
    let err = svc
        .put(&ctx, &key("k"), write, create_only())
        .await
        .expect_err("expiry on non-expirable type");
    assert!(matches!(err, DomainError::TypeViolation { .. }));
}

#[tokio::test]
async fn per_type_pdp_denial_hides_reads_and_forbids_writes() {
    let tenant = Uuid::new_v4();
    let api_key_gts = SecretType::from_name("api-key")
        .unwrap()
        .gts_id()
        .to_owned();
    let (enforcer, _resolver) = type_deny_enforcer(vec![api_key_gts]);
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service(repo, plugin, dir, enforcer, Arc::new(NoopMetrics));
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_create_typed(SharingMode::Tenant, "v", "api-key"),
            create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    svc.get_secret(&ctx, &key("k"))
        .await
        .expect("get_secret")
        .expect("some");
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
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("create");
    let fp1 = repo.rows()[0].value_fp.clone();
    svc.put(
        &ctx,
        &key("k"),
        write_replace(SharingMode::Tenant, "v2"),
        put_exists(),
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
        write_create(SharingMode::Tenant, "gen1"),
        create_only(),
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
        write_create(SharingMode::Tenant, "gen2"),
        create_only(),
    )
    .await
    .expect("create gen2");

    // A validator minted for gen1 must never match gen2, even if version
    // counters happen to coincide (both start at 1).
    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_replace(SharingMode::Tenant, "v3"),
            put_matches(gen1.id, gen1.version),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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

    let write = CredentialWrite {
        expires_at: Some(OffsetDateTime::now_utc() + time::Duration::seconds(1)),
        ..write_create_typed(SharingMode::Tenant, "v", "bearer-token")
    };
    svc.put(&ctx, &key("k"), write, create_only())
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
        write_create(SharingMode::Tenant, "v"),
        create_only(),
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

// ── ADR-0004: walkthrough scenario (T1 shared, T2 overrides/rotates/
//    suppresses/deletes, T3 inherits from T1/T2) ────────────────────────────

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "a single end-to-end narrative walkthrough of override/rotate/suppress/delete \
              across three tenants; splitting it would break the story the test is telling"
)]
async fn walkthrough_t1_t2_t3_override_rotate_suppress_delete() {
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();
    let t3 = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    // t3's chain is [t3, t2, t1]; t2's chain is [t2, t1]; t1's is [t1].
    let dir_t1 = Arc::new(FakeDir::single(t1));
    let dir_t2 = Arc::new(FakeDir::new(vec![t2, t1]));
    let dir_t3 = Arc::new(FakeDir::new(vec![t3, t2, t1]));

    let svc_t1 = make_service_noop(repo.clone(), plugin.clone(), dir_t1);
    let svc_t2 = make_service_noop(repo.clone(), plugin.clone(), dir_t2);
    let svc_t3 = make_service_noop(repo.clone(), plugin.clone(), dir_t3);

    let owner1 = Uuid::new_v4();
    let ctx1 = make_ctx(owner1, t1);
    let owner2 = Uuid::new_v4();
    let ctx2 = make_ctx(owner2, t2);
    let owner3 = Uuid::new_v4();
    let ctx3 = make_ctx(owner3, t3);
    let name = key("smtp-default");

    // Step 0: T1 publishes shared V1. T2/T3 inherit it.
    svc_t1
        .put(
            &ctx1,
            &name,
            write_create(SharingMode::Shared, "V1"),
            create_only(),
        )
        .await
        .expect("T1 publishes");
    assert_eq!(
        svc_t2
            .get_secret(&ctx2, &name)
            .await
            .expect("t2 get_secret")
            .expect("v1")
            .value
            .as_bytes(),
        b"V1"
    );
    assert_eq!(
        svc_t3
            .get_secret(&ctx3, &name)
            .await
            .expect("t3 get_secret")
            .expect("v1")
            .value
            .as_bytes(),
        b"V1"
    );
    assert_eq!(
        svc_t3
            .get(&ctx3, &name)
            .await
            .expect("t3 get")
            .expect("cred")
            .inheritance,
        InheritanceStatus::Inherited
    );

    // Step 1: T2 overrides with V2 (create-only).
    let outcome = svc_t2
        .put(
            &ctx2,
            &name,
            write_create(SharingMode::Shared, "V2"),
            create_only(),
        )
        .await
        .expect("T2 overrides");
    assert!(outcome.created);
    let t2_cred = svc_t2
        .get(&ctx2, &name)
        .await
        .expect("t2 get")
        .expect("cred");
    assert_eq!(t2_cred.inheritance, InheritanceStatus::Overridden);
    assert_eq!(t2_cred.status, CredentialStatus::Active);
    assert!(t2_cred.validator.is_some(), "own row => strong validator");
    assert_eq!(
        svc_t3
            .get_secret(&ctx3, &name)
            .await
            .expect("t3 get_secret")
            .expect("v2")
            .value
            .as_bytes(),
        b"V2"
    );

    // Step 2: T2 rotates via PATCH {value}. Only write_secret is required —
    // assert via the fake PDP's recorded actions.
    let (rotate_enforcer, rotate_resolver) = type_recording_enforcer();
    let svc_t2_recording = make_service(
        repo.clone(),
        plugin.clone(),
        Arc::new(FakeDir::new(vec![t2, t1])),
        rotate_enforcer,
        Arc::new(NoopMetrics),
    );
    let validator = svc_t2_recording
        .patch(
            &ctx2,
            &name,
            patch_value("V3"),
            matches(
                t2_cred.validator.expect("some").id,
                t2_cred.validator.expect("some").version,
            ),
        )
        .await
        .expect("T2 rotates");
    assert_eq!(
        validator.version,
        t2_cred.validator.expect("some").version + 1
    );
    assert_eq!(
        svc_t3
            .get_secret(&ctx3, &name)
            .await
            .expect("t3 get_secret")
            .expect("v3")
            .value
            .as_bytes(),
        b"V3"
    );
    let seen = rotate_resolver.seen_actions();
    assert!(seen.contains(&crate::domain::authz::actions::WRITE_SECRET.to_owned()));
    assert!(
        !seen.contains(&crate::domain::authz::actions::WRITE.to_owned()),
        "a value-only PATCH must not evaluate `write`: {seen:?}"
    );

    // Step 3: T2 suppresses {"fallback": "none", "value": null} — one
    // transaction. T2's row becomes declared/none; T2 and T3 now get None;
    // T2's record reads suppressed/declared; T1 untouched.
    let t2_after_rotate = svc_t2
        .get(&ctx2, &name)
        .await
        .expect("t2 get")
        .expect("cred");
    svc_t2
        .patch(
            &ctx2,
            &name,
            patch_suppress(),
            matches(
                t2_after_rotate.validator.expect("some").id,
                t2_after_rotate.validator.expect("some").version,
            ),
        )
        .await
        .expect("T2 suppresses");
    assert!(
        svc_t2
            .get_secret(&ctx2, &name)
            .await
            .expect("t2 get_secret")
            .is_none()
    );
    assert!(
        svc_t3
            .get_secret(&ctx3, &name)
            .await
            .expect("t3 get_secret")
            .is_none()
    );
    let t2_suppressed = svc_t2
        .get(&ctx2, &name)
        .await
        .expect("t2 get")
        .expect("cred");
    assert_eq!(t2_suppressed.status, CredentialStatus::Declared);
    assert_eq!(t2_suppressed.inheritance, InheritanceStatus::Suppressed);
    assert_eq!(
        svc_t1
            .get_secret(&ctx1, &name)
            .await
            .expect("t1 get_secret")
            .expect("still v1")
            .value
            .as_bytes(),
        b"V1",
        "T1's own record and value are untouched"
    );

    // Step 4: T2 deletes — T2/T3 inherit T1 (V1) again, status: none.
    svc_t2
        .delete(
            &ctx2,
            &name,
            matches(
                t2_suppressed.validator.expect("some").id,
                t2_suppressed.validator.expect("some").version,
            ),
        )
        .await
        .expect("T2 deletes");
    assert_eq!(
        svc_t2
            .get_secret(&ctx2, &name)
            .await
            .expect("t2 get_secret")
            .expect("inherits v1 again")
            .value
            .as_bytes(),
        b"V1"
    );
    assert_eq!(
        svc_t3
            .get_secret(&ctx3, &name)
            .await
            .expect("t3 get_secret")
            .expect("inherits v1 again")
            .value
            .as_bytes(),
        b"V1"
    );
    let t2_final = svc_t2
        .get(&ctx2, &name)
        .await
        .expect("t2 get")
        .expect("cred");
    assert_eq!(t2_final.status, CredentialStatus::None);
    assert!(t2_final.validator.is_none());
    assert_eq!(t2_final.inheritance, InheritanceStatus::Inherited);
}

// ── ADR-0004: body-derived actions ───────────────────────────────────────────

#[tokio::test]
async fn put_evaluates_both_write_and_write_secret() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let (enforcer, resolver) = type_recording_enforcer();
    let svc = make_service(repo, plugin, dir, enforcer, Arc::new(NoopMetrics));
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let seen = resolver.seen_actions();
    assert!(seen.contains(&crate::domain::authz::actions::WRITE.to_owned()));
    assert!(seen.contains(&crate::domain::authz::actions::WRITE_SECRET.to_owned()));
}

#[tokio::test]
async fn patch_metadata_only_evaluates_write_alone() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    let (enforcer, resolver) = type_recording_enforcer();
    let svc_recording = make_service(repo.clone(), plugin, dir, enforcer, Arc::new(NoopMetrics));
    svc_recording
        .patch(
            &ctx,
            &key("k"),
            patch_sharing(SharingMode::Shared),
            matches(row.id, row.version),
        )
        .await
        .expect("metadata-only patch");
    let seen = resolver.seen_actions();
    assert!(seen.contains(&crate::domain::authz::actions::WRITE.to_owned()));
    assert!(!seen.contains(&crate::domain::authz::actions::WRITE_SECRET.to_owned()));
}

#[tokio::test]
async fn patch_value_only_evaluates_write_secret_alone() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    let (enforcer, resolver) = type_recording_enforcer();
    let svc_recording = make_service(repo.clone(), plugin, dir, enforcer, Arc::new(NoopMetrics));
    svc_recording
        .patch(
            &ctx,
            &key("k"),
            patch_value("v2"),
            matches(row.id, row.version),
        )
        .await
        .expect("value-only patch");
    let seen = resolver.seen_actions();
    assert!(!seen.contains(&crate::domain::authz::actions::WRITE.to_owned()));
    assert!(seen.contains(&crate::domain::authz::actions::WRITE_SECRET.to_owned()));
}

#[tokio::test]
async fn patch_both_metadata_and_value_evaluates_both_actions() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    let (enforcer, resolver) = type_recording_enforcer();
    let svc_recording = make_service(repo.clone(), plugin, dir, enforcer, Arc::new(NoopMetrics));
    let patch = CredentialPatch {
        sharing: Some(SharingMode::Shared),
        ..patch_value("v2")
    };
    svc_recording
        .patch(&ctx, &key("k"), patch, matches(row.id, row.version))
        .await
        .expect("both-fields patch");
    let seen = resolver.seen_actions();
    assert!(seen.contains(&crate::domain::authz::actions::WRITE.to_owned()));
    assert!(seen.contains(&crate::domain::authz::actions::WRITE_SECRET.to_owned()));
}

#[tokio::test]
async fn patch_denied_action_writes_nothing() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir.clone());
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    // Deny write_secret specifically for the generic type.
    let generic_gts = SecretType::generic().gts_id().to_owned();
    let (enforcer, _resolver) =
        action_deny_enforcer(generic_gts, crate::domain::authz::actions::WRITE_SECRET);
    let svc_denied = make_service(repo.clone(), plugin, dir, enforcer, Arc::new(NoopMetrics));

    let patch = CredentialPatch {
        sharing: Some(SharingMode::Shared),
        ..patch_value("v2")
    };
    let err = svc_denied
        .patch(&ctx, &key("k"), patch, matches(row.id, row.version))
        .await
        .expect_err("write_secret denied");
    assert!(matches!(err, DomainError::AccessDenied { .. }));
    // Nothing was written: sharing and value both unchanged.
    assert_eq!(repo.rows()[0].sharing, SharingMode::Tenant);
    assert_eq!(repo.rows()[0].version, row.version);
}

#[tokio::test]
async fn patch_empty_is_rejected() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    let err = svc
        .patch(&ctx, &key("k"), empty_patch(), matches(row.id, row.version))
        .await
        .expect_err("empty patch rejected");
    assert!(matches!(
        err,
        DomainError::InvalidRequest {
            reason: crate::domain::secret::typing::reasons::EMPTY_PATCH,
            ..
        }
    ));
}

#[tokio::test]
async fn patch_metadata_no_op_keeps_version_and_returns_current_validator() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    // Same sharing as already stored: a genuine no-op.
    let validator = svc
        .patch(
            &ctx,
            &key("k"),
            patch_sharing(SharingMode::Tenant),
            matches(row.id, row.version),
        )
        .await
        .expect("no-op patch");
    assert_eq!(validator.id, row.id);
    assert_eq!(
        validator.version, row.version,
        "no-op must not bump the version"
    );
    assert_eq!(repo.rows()[0].version, row.version);
}

#[tokio::test]
async fn patch_type_immutable() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create generic");
    let row = repo.rows()[0].clone();

    let patch = CredentialPatch {
        secret_type: Some(SecretType::from_name("api-key").expect("known").into()),
        ..empty_patch()
    };
    let err = svc
        .patch(&ctx, &key("k"), patch, matches(row.id, row.version))
        .await
        .expect_err("type change rejected");
    assert!(matches!(
        err,
        DomainError::TypeViolation {
            reason: crate::domain::secret::typing::reasons::TYPE_IMMUTABLE,
            ..
        }
    ));
}

#[tokio::test]
async fn put_create_type_mismatch_with_inherited() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir_parent = Arc::new(FakeDir::single(parent));
    let dir_child = Arc::new(FakeDir::new(vec![child, parent]));
    let svc_parent = make_service_noop(repo.clone(), plugin.clone(), dir_parent);
    let svc_child = make_service_noop(repo.clone(), plugin.clone(), dir_child);

    let parent_ctx = make_ctx(owner, parent);
    svc_parent
        .put(
            &parent_ctx,
            &key("k"),
            write_create_typed(
                SharingMode::Shared,
                r#"{"username":"u","password":"p"}"#,
                "basic-auth",
            ),
            create_only(),
        )
        .await
        .expect("parent creates basic-auth");

    let child_ctx = make_ctx(Uuid::new_v4(), child);
    let err = svc_child
        .put(
            &child_ctx,
            &key("k"),
            write_create_typed(SharingMode::Shared, "{\"a\":1}", "generic"),
            create_only(),
        )
        .await
        .expect_err("type mismatch with inherited");
    assert!(matches!(
        err,
        DomainError::TypeViolation {
            reason: crate::domain::secret::typing::reasons::TYPE_MISMATCH_WITH_INHERITED,
            ..
        }
    ));
}

#[tokio::test]
async fn declared_inherit_own_row_reports_declared_status_inherited_inheritance() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir_parent = Arc::new(FakeDir::single(parent));
    let dir_child = Arc::new(FakeDir::new(vec![child, parent]));
    let svc_parent = make_service_noop(repo.clone(), plugin.clone(), dir_parent);
    let svc_child = make_service_noop(repo.clone(), plugin.clone(), dir_child);

    let parent_ctx = make_ctx(owner, parent);
    svc_parent
        .put(
            &parent_ctx,
            &key("k"),
            write_create(SharingMode::Shared, "V1"),
            create_only(),
        )
        .await
        .expect("parent publishes shared");

    let child_ctx = make_ctx(owner, child);
    svc_child
        .put(
            &child_ctx,
            &key("k"),
            write_create(SharingMode::Shared, "V2"),
            create_only(),
        )
        .await
        .expect("child overrides");
    let cred_before = svc_child
        .get(&child_ctx, &key("k"))
        .await
        .expect("get")
        .expect("cred");
    svc_child
        .patch(
            &child_ctx,
            &key("k"),
            patch_value_null_keep_inherit(),
            matches(
                cred_before.validator.expect("some").id,
                cred_before.validator.expect("some").version,
            ),
        )
        .await
        .expect("remove value, keep fallback: inherit");

    let cred = svc_child
        .get(&child_ctx, &key("k"))
        .await
        .expect("get")
        .expect("cred");
    assert_eq!(cred.status, CredentialStatus::Declared);
    assert_eq!(cred.inheritance, InheritanceStatus::Inherited);
    // The record's own row is `declared` (no value of its own) even though
    // the *value* resolves from the parent, so `owner_id` still reports the
    // child's own creator, not the parent's.
    assert_eq!(cred.owner_id, Some(OwnerId(owner)));
    assert_eq!(
        svc_child
            .get_secret(&child_ctx, &key("k"))
            .await
            .expect("get_secret")
            .expect("v1")
            .value
            .as_bytes(),
        b"V1"
    );
}

fn patch_value_null_keep_inherit() -> CredentialPatch {
    CredentialPatch {
        value: PatchField::Null,
        ..empty_patch()
    }
}

#[tokio::test]
async fn patch_value_on_declared_row_switches_it_to_active() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin, dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);
    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v1"),
        create_only(),
    )
    .await
    .expect("create");
    let row = repo.rows()[0].clone();

    svc.patch(
        &ctx,
        &key("k"),
        patch_value_null_keep_inherit(),
        matches(row.id, row.version),
    )
    .await
    .expect("remove value");
    assert_eq!(repo.rows()[0].status, SecretStatus::Declared);

    let declared = repo.rows()[0].clone();
    svc.patch(
        &ctx,
        &key("k"),
        patch_value("v2"),
        matches(declared.id, declared.version),
    )
    .await
    .expect("write value onto a declared row");
    assert_eq!(repo.rows()[0].status, SecretStatus::Active);
    assert_eq!(
        svc.get_secret(&ctx, &key("k"))
            .await
            .expect("get_secret")
            .expect("v2")
            .value
            .as_bytes(),
        b"v2"
    );
}

// ── ADDENDUM 3: Credential.owner_id ─────────────────────────────────────────

#[tokio::test]
async fn get_reports_the_creators_subject_id_as_owner_id_for_an_own_row() {
    let tenant = Uuid::new_v4();
    let subject = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo, plugin, dir);
    let ctx = make_ctx(subject, tenant);

    svc.put(
        &ctx,
        &key("k"),
        write_create(SharingMode::Tenant, "v"),
        create_only(),
    )
    .await
    .expect("create");

    let cred = svc.get(&ctx, &key("k")).await.expect("get").expect("some");
    assert_eq!(cred.owner_id, Some(OwnerId(subject)));
}

// ── ADR-0004: resolve_credential's weak-validator source ────────────────────

#[tokio::test]
async fn resolve_credential_carries_the_winner_identity_when_no_own_row() {
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir_parent = Arc::new(FakeDir::single(parent));
    let dir_child = Arc::new(FakeDir::new(vec![child, parent]));
    let svc_parent = make_service_noop(repo.clone(), plugin.clone(), dir_parent);
    let svc_child = make_service_noop(repo.clone(), plugin.clone(), dir_child);

    let parent_ctx = make_ctx(owner, parent);
    svc_parent
        .put(
            &parent_ctx,
            &key("k"),
            write_create(SharingMode::Shared, "V1"),
            create_only(),
        )
        .await
        .expect("parent publishes");
    let parent_row = repo.rows()[0].clone();

    let child_ctx = make_ctx(Uuid::new_v4(), child);
    let (cred, weak_source) = svc_child
        .resolve_credential(&child_ctx, &key("k"))
        .await
        .expect("resolve_credential")
        .expect("some");
    assert!(cred.validator.is_none());
    // No own row in play: the ancestor's owner id must never leak across
    // the tenant boundary (ADDENDUM 3) — reads it in its own tenant's
    // context instead.
    assert!(cred.owner_id.is_none());
    assert_eq!(weak_source, Some((parent_row.id, parent_row.version)));
}

// ── hardening (lifecycle review addendum) ────────────────────────────────────

#[tokio::test]
async fn abandon_pending_marks_aborted_rather_than_deleting_the_intent() {
    let tenant = Uuid::new_v4();
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let dir = Arc::new(FakeDir::single(tenant));
    let svc = make_service_noop(repo.clone(), plugin.clone(), dir);
    let ctx = make_ctx(Uuid::new_v4(), tenant);

    // Bootstrap the fence key with an unrelated write first, so the failure
    // armed below lands on the *value's* plugin.put (create_new's step 3),
    // not on the fence-key bootstrap put.
    svc.put(
        &ctx,
        &key("bootstrap"),
        write_create(SharingMode::Tenant, "x"),
        create_only(),
    )
    .await
    .expect("bootstrap the fence key");
    plugin.fail_next_puts(1);

    let err = svc
        .put(
            &ctx,
            &key("k"),
            write_create(SharingMode::Tenant, "v"),
            create_only(),
        )
        .await
        .expect_err("plugin.put failure propagates");
    assert!(matches!(err, DomainError::Internal { .. }));

    let entries = repo.gc_entries();
    assert_eq!(
        entries.len(),
        1,
        "the intent row must survive, not be deleted"
    );
    assert_eq!(
        entries[0].reason,
        GcReason::Aborted,
        "abandon_pending must mark Aborted, never delete the intent outright"
    );
}

#[tokio::test]
async fn run_gc_pending_reclaim_claims_before_deleting_from_the_backend() {
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

    // An unreferenced pending entry whose backend delete will fail.
    let orphan = ValueId::new_v4();
    plugin
        .put(
            &ctx,
            &TenantId(tenant),
            &orphan,
            SecretValue::from("orphan-bytes"),
        )
        .await
        .expect("seed orphan bytes");
    repo.gc_insert_pending(orphan, TenantId(tenant))
        .await
        .expect("insert pending");
    plugin.fail_next_deletes(1);

    let report = svc.run_gc(&ctx).await.expect("run_gc");
    // The claim (gc_delete) must have succeeded and counted even though the
    // backend delete failed — "claim first" means the gc row is gone
    // regardless of the backend outcome.
    assert_eq!(report.gc_pending_reclaimed, 1);
    assert!(
        repo.gc_entries().iter().all(|e| e.value_id != orphan),
        "the gc row must be claimed (removed) even when the backend delete fails"
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
        updated_at: OffsetDateTime::now_utc(),
        secret_type_uuid: SecretType::generic().uuid(),
        expires_at: None,
        value_id: Some(ValueId::new_v4()),
        value_fp: Some(vec![7u8; 32]),
        fp_key_id: Some(1),
        fallback: Fallback::Inherit,
    }
}
