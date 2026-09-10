//! SQLite-backed integration tests for [`SecretRepoImpl`] (ADR-0006:
//! immutable value versions).
//!
//! These exercise the real `SeaORM`/`SecureORM` read + write paths against an
//! in-memory SQLite database built from the module's own migrations
//! (`m0001_initial_schema` + `m0002_value_versions`). No raw SQL for the
//! schema; fixtures are seeded through the repository's own write methods
//! (a handful of tests probe the raw schema directly to pin the migration's
//! `CHECK` contracts).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

use std::sync::Arc;

use credstore_sdk::{OwnerId, SecretRef, SecretType, SharingMode, TenantId, ValueId};
use sea_orm::{ActiveValue, EntityTrait};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{ScopeError, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::secret::model::{GcReason, NewSecret, SecretStatus};
use crate::domain::secret::repo::SecretRepo;
use crate::infra::storage::entity;
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::repo_impl::SecretRepoImpl;

/// Build a repo backed by a fresh, isolated in-memory SQLite database.
async fn setup() -> SecretRepoImpl {
    let id = Uuid::new_v4();
    let dsn = format!("sqlite:file:credstore_repo_{id}?mode=memory&cache=shared");
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect sqlite");

    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrations");

    SecretRepoImpl::new(Arc::new(DBProvider::<DomainError>::new(db)))
}

fn sref(s: &str) -> SecretRef {
    SecretRef::new(s).expect("valid secret ref")
}

fn new_secret(
    tenant: Uuid,
    owner: Uuid,
    key: &str,
    sharing: SharingMode,
    value_id: ValueId,
) -> NewSecret {
    NewSecret {
        id: Uuid::new_v4(),
        tenant_id: TenantId(tenant),
        reference: sref(key),
        sharing,
        owner_id: OwnerId(owner),
        secret_type_uuid: SecretType::generic().uuid(),
        expires_at: None,
        value_id,
        value_fp: vec![7u8; 32],
        fp_key_id: 1,
    }
}

/// Insert an active row via the real write-protocol entrypoint, returning
/// `(row_id, value_id)`.
async fn seed_active(
    repo: &SecretRepoImpl,
    tenant: Uuid,
    owner: Uuid,
    key: &str,
    sharing: SharingMode,
) -> (Uuid, ValueId) {
    let value_id = ValueId::new_v4();
    let new = new_secret(tenant, owner, key, sharing, value_id);
    let id = new.id;
    repo.insert_active(&AccessScope::for_tenant(tenant), &new)
        .await
        .expect("insert_active");
    (id, value_id)
}

fn subtree_scope(root: Uuid) -> AccessScope {
    AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
        ScopeFilter::in_tenant_subtree(pep_properties::OWNER_TENANT_ID, root, true, Vec::new()),
    ])])
}

// ── migration schema contracts ───────────────────────────────────────────────

/// Insert a `credstore_secrets` row bypassing the domain layer entirely, so
/// tests can probe the raw migration `CHECK` contracts directly (an
/// out-of-domain status, or a pointer/fingerprint pairing the domain would
/// never construct).
async fn insert_raw_secret(
    repo: &SecretRepoImpl,
    status: i16,
    value_id: Option<Uuid>,
    value_fp: Option<Vec<u8>>,
    fp_key_id: Option<i16>,
) -> Result<(), ScopeError> {
    let conn = repo.db.conn().expect("conn");
    entity::secrets::Entity::insert(entity::secrets::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(Uuid::new_v4()),
        reference: ActiveValue::Set("x".to_owned()),
        sharing: ActiveValue::Set(2),
        owner_id: ActiveValue::Set(Uuid::new_v4()),
        status: ActiveValue::Set(status),
        created_at: ActiveValue::NotSet,
        updated_at: ActiveValue::NotSet,
        version: ActiveValue::NotSet,
        secret_type_uuid: ActiveValue::Set(SecretType::generic().uuid()),
        expires_at: ActiveValue::Set(None),
        value_id: ActiveValue::Set(value_id),
        value_fp: ActiveValue::Set(value_fp),
        fp_key_id: ActiveValue::Set(fp_key_id),
        fallback: ActiveValue::Set(1),
    })
    .secure()
    .scope_unchecked(&AccessScope::allow_all())?
    .exec(&conn)
    .await
    .map(|_| ())
}

/// Insert a `credstore_value_gc` row bypassing the domain layer, to probe the
/// raw `reason` `CHECK`.
async fn insert_raw_gc(repo: &SecretRepoImpl, reason: i16) -> Result<(), ScopeError> {
    let conn = repo.db.conn().expect("conn");
    entity::value_gc::Entity::insert(entity::value_gc::ActiveModel {
        value_id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: ActiveValue::Set(Uuid::new_v4()),
        reason: ActiveValue::Set(reason),
        enqueued_at: ActiveValue::NotSet,
    })
    .secure()
    .scope_unchecked(&AccessScope::allow_all())?
    .exec(&conn)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn migration_final_schema_rejects_retired_status_codes() {
    let repo = setup().await;
    for bad_status in [1_i16, 3_i16] {
        let err = insert_raw_secret(&repo, bad_status, None, None, None)
            .await
            .expect_err("retired status code must violate the narrowed CHECK");
        assert!(
            err.to_string().to_lowercase().contains("check"),
            "expected a CHECK violation, got: {err}"
        );
    }
}

#[tokio::test]
async fn migration_final_schema_enforces_fp_with_value_pairing() {
    let repo = setup().await;
    // A value_id with no fingerprint violates ck_credstore_fp_with_value.
    let err = insert_raw_secret(&repo, 2, Some(Uuid::new_v4()), None, None)
        .await
        .expect_err("value_id with no fingerprint must violate the pairing CHECK");
    assert!(err.to_string().to_lowercase().contains("check"));

    // A fingerprint with no value_id equally violates it.
    let err = insert_raw_secret(&repo, 2, None, Some(vec![1u8; 32]), Some(1))
        .await
        .expect_err("fingerprint with no value_id must violate the pairing CHECK");
    assert!(err.to_string().to_lowercase().contains("check"));
}

#[tokio::test]
async fn migration_creates_value_gc_table_with_reason_check() {
    let repo = setup().await;
    let err = insert_raw_gc(&repo, 9)
        .await
        .expect_err("out-of-domain reason must violate the CHECK");
    assert!(err.to_string().to_lowercase().contains("check"));
}

// ── write protocol: insert_active ────────────────────────────────────────────

#[tokio::test]
async fn insert_active_removes_pending_gc_row_atomically() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let value_id = ValueId::new_v4();

    repo.gc_insert_pending(value_id, TenantId(tenant))
        .await
        .expect("gc_insert_pending");
    assert!(
        repo.gc_list(100)
            .await
            .expect("gc_list")
            .iter()
            .any(|e| e.value_id == value_id),
        "pending entry must be visible before insert_active"
    );

    let new = new_secret(tenant, owner, "k", SharingMode::Tenant, value_id);
    repo.insert_active(&AccessScope::for_tenant(tenant), &new)
        .await
        .expect("insert_active");

    assert!(
        repo.gc_list(100)
            .await
            .expect("gc_list")
            .iter()
            .all(|e| e.value_id != value_id),
        "insert_active must remove the pending entry in the same transaction"
    );

    let row = repo
        .resolve_for_get(TenantId(tenant), OwnerId(owner), &sref("k"), &[tenant])
        .await
        .expect("resolve")
        .expect("row visible");
    assert_eq!(row.value_id, Some(value_id));
    assert_eq!(row.status, SecretStatus::Active);
    assert_eq!(row.version, 1);
}

#[tokio::test]
async fn duplicate_nonprivate_insert_conflicts() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    seed_active(&repo, tenant, owner, "dup", SharingMode::Tenant).await;

    let new = new_secret(tenant, owner, "dup", SharingMode::Tenant, ValueId::new_v4());
    let err = repo
        .insert_active(&AccessScope::for_tenant(tenant), &new)
        .await
        .expect_err("duplicate non-private insert violates unique index");
    assert!(matches!(err, DomainError::Conflict));
}

// ── write protocol: switch_value ─────────────────────────────────────────────

#[tokio::test]
async fn switch_value_bumps_version_and_enqueues_old_as_superseded() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let (id, old_value) = seed_active(&repo, tenant, owner, "k", SharingMode::Tenant).await;

    let new_value = ValueId::new_v4();
    let (row, old) = repo
        .switch_value(
            &scope,
            id,
            None,
            SharingMode::Tenant,
            None,
            new_value,
            vec![9u8; 32],
            1,
        )
        .await
        .expect("switch_value")
        .expect("row updated");
    assert_eq!(row.version, 2);
    assert_eq!(row.value_id, Some(new_value));
    assert_eq!(old, Some(old_value));

    // The old id's pending entry never existed for it (it was realized by the
    // seed's own insert_active), so switch_value's own gc bookkeeping is what
    // enqueues it now, as `superseded` — and the new id's own (nonexistent)
    // pending row removal is a harmless no-op.
    let gc = repo.gc_list(100).await.expect("gc_list");
    let entry = gc
        .iter()
        .find(|e| e.value_id == old_value)
        .expect("old value enqueued for gc");
    assert_eq!(entry.reason, GcReason::Superseded);
    assert!(gc.iter().all(|e| e.value_id != new_value));
}

#[tokio::test]
async fn switch_value_version_mismatch_returns_none_without_touching_gc() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let (id, old_value) = seed_active(&repo, tenant, owner, "k", SharingMode::Tenant).await;

    let new_value = ValueId::new_v4();
    let result = repo
        .switch_value(
            &scope,
            id,
            Some(99), // stale
            SharingMode::Tenant,
            None,
            new_value,
            vec![9u8; 32],
            1,
        )
        .await
        .expect("switch_value");
    assert!(result.is_none());

    // Nothing moved: no gc entry for either id, and the row still points at
    // the original value, unchanged version.
    let gc = repo.gc_list(100).await.expect("gc_list");
    assert!(
        gc.iter()
            .all(|e| e.value_id != new_value && e.value_id != old_value)
    );
    let row = repo
        .resolve_for_get(TenantId(tenant), OwnerId(owner), &sref("k"), &[tenant])
        .await
        .expect("resolve")
        .expect("row");
    assert_eq!(row.value_id, Some(old_value));
    assert_eq!(row.version, 1);
}

#[tokio::test]
async fn switch_value_missing_row_returns_none() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let result = repo
        .switch_value(
            &scope,
            Uuid::new_v4(),
            None,
            SharingMode::Tenant,
            None,
            ValueId::new_v4(),
            vec![9u8; 32],
            1,
        )
        .await
        .expect("switch_value");
    assert!(result.is_none());
}

#[tokio::test]
async fn switch_value_with_matching_expected_version_bumps() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _old) = seed_active(&repo, tenant, owner, "ver", SharingMode::Tenant).await;

    let (row, _) = repo
        .switch_value(
            &scope,
            id,
            Some(1),
            SharingMode::Tenant,
            None,
            ValueId::new_v4(),
            vec![9u8; 32],
            1,
        )
        .await
        .expect("switch_value")
        .expect("row updated");
    assert_eq!(row.version, 2);
}

// ── write protocol: delete_by_id ─────────────────────────────────────────────

#[tokio::test]
async fn delete_by_id_enqueues_removed_and_then_not_found() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let (id, value_id) = seed_active(&repo, tenant, owner, "gone", SharingMode::Tenant).await;

    let old = repo.delete_by_id(&scope, id, None).await.expect("delete");
    assert_eq!(old, Some(value_id));

    let gc = repo.gc_list(100).await.expect("gc_list");
    let entry = gc
        .iter()
        .find(|e| e.value_id == value_id)
        .expect("value enqueued for gc");
    assert_eq!(entry.reason, GcReason::Removed);

    let err = repo
        .delete_by_id(&scope, id, None)
        .await
        .expect_err("second delete is NotFound");
    assert!(matches!(err, DomainError::NotFound));
}

#[tokio::test]
async fn delete_by_id_with_stale_expected_version_is_not_found() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let (id, _value_id) = seed_active(&repo, tenant, owner, "ver-del", SharingMode::Tenant).await;
    let scope = AccessScope::for_tenant(tenant);

    let err = repo
        .delete_by_id(&scope, id, Some(99))
        .await
        .expect_err("stale expected_version must match 0 rows");
    assert!(matches!(err, DomainError::NotFound));

    repo.delete_by_id(&scope, id, Some(1))
        .await
        .expect("matching expected_version deletes");
}

#[tokio::test]
async fn delete_then_create_only_put_under_the_same_reference_succeeds() {
    // No name retention (ADR-0006): a successor mints its own value_id, so it
    // can never collide with the predecessor's lagging gc entry.
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _old) = seed_active(&repo, tenant, owner, "reused", SharingMode::Tenant).await;

    repo.delete_by_id(&scope, id, None).await.expect("delete");

    let new_value = ValueId::new_v4();
    let new = new_secret(tenant, owner, "reused", SharingMode::Tenant, new_value);
    repo.insert_active(&scope, &new)
        .await
        .expect("recreate under the same reference immediately succeeds");

    let row = repo
        .resolve_for_get(TenantId(tenant), OwnerId(owner), &sref("reused"), &[tenant])
        .await
        .expect("resolve")
        .expect("row");
    assert_eq!(row.value_id, Some(new_value));
}

// ── maintenance job support ──────────────────────────────────────────────────

#[tokio::test]
async fn list_expired_matches_only_active_rows_past_expiry() {
    use time::Duration as TimeDuration;
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);

    let expired_id = Uuid::new_v4();
    let value_id = ValueId::new_v4();
    let mut new = new_secret(tenant, owner, "expired", SharingMode::Tenant, value_id);
    new.id = expired_id;
    new.expires_at = Some(time::OffsetDateTime::now_utc() - TimeDuration::seconds(5));
    repo.insert_active(&scope, &new).await.expect("insert");

    // A live (unexpired) row is untouched by the listing.
    let mut live = new_secret(
        tenant,
        owner,
        "living",
        SharingMode::Tenant,
        ValueId::new_v4(),
    );
    live.expires_at = Some(time::OffsetDateTime::now_utc() + TimeDuration::hours(1));
    repo.insert_active(&scope, &live)
        .await
        .expect("insert live");

    let expired = repo.list_expired(100).await.expect("list_expired");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].id, expired_id);
}

#[tokio::test]
async fn delete_expired_row_enqueues_its_version_and_is_idempotent() {
    use time::Duration as TimeDuration;
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);

    let value_id = ValueId::new_v4();
    let mut new = new_secret(tenant, owner, "expired", SharingMode::Tenant, value_id);
    new.expires_at = Some(time::OffsetDateTime::now_utc() - TimeDuration::seconds(5));
    let id = new.id;
    repo.insert_active(&scope, &new).await.expect("insert");

    let removed = repo
        .delete_expired_row(id)
        .await
        .expect("delete_expired_row");
    assert_eq!(removed, Some(value_id));
    assert_eq!(
        repo.gc_list(100)
            .await
            .expect("gc_list")
            .iter()
            .find(|e| e.value_id == value_id)
            .expect("enqueued")
            .reason,
        GcReason::Removed
    );

    // Idempotent: a row already gone is nothing to do, not an error.
    let again = repo
        .delete_expired_row(id)
        .await
        .expect("delete_expired_row again");
    assert_eq!(again, None);
}

// ── gc bookkeeping ────────────────────────────────────────────────────────────

#[tokio::test]
async fn gc_list_orders_by_enqueued_at_and_respects_limit() {
    let repo = setup().await;
    let a = ValueId::new_v4();
    let b = ValueId::new_v4();
    repo.gc_insert_pending(a, TenantId(Uuid::new_v4()))
        .await
        .expect("insert a");
    repo.gc_insert_pending(b, TenantId(Uuid::new_v4()))
        .await
        .expect("insert b");

    let all = repo.gc_list(100).await.expect("gc_list");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].value_id, a, "oldest first");
    assert_eq!(all[1].value_id, b);

    let limited = repo.gc_list(1).await.expect("gc_list limited");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].value_id, a);
}

#[tokio::test]
async fn gc_mark_updates_reason_gc_delete_removes_it() {
    let repo = setup().await;
    let v = ValueId::new_v4();
    repo.gc_insert_pending(v, TenantId(Uuid::new_v4()))
        .await
        .expect("insert");

    assert!(repo.gc_mark(v, GcReason::Aborted).await.expect("gc_mark"));
    assert_eq!(
        repo.gc_list(100).await.expect("list")[0].reason,
        GcReason::Aborted
    );

    assert!(repo.gc_delete(v).await.expect("gc_delete"));
    assert!(repo.gc_list(100).await.expect("list").is_empty());
    // Deleting again is a benign no-op.
    assert!(!repo.gc_delete(v).await.expect("gc_delete again"));
}

#[tokio::test]
async fn gc_mark_missing_entry_returns_false() {
    let repo = setup().await;
    assert!(
        !repo
            .gc_mark(ValueId::new_v4(), GcReason::Aborted)
            .await
            .expect("gc_mark")
    );
}

#[tokio::test]
async fn is_value_referenced_reflects_the_current_pointer() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let (_id, value_id) = seed_active(&repo, tenant, owner, "k", SharingMode::Tenant).await;

    assert!(
        repo.is_value_referenced(value_id)
            .await
            .expect("is_value_referenced")
    );
    assert!(
        !repo
            .is_value_referenced(ValueId::new_v4())
            .await
            .expect("unreferenced id")
    );
}

// ── read path (unchanged logic, adapted fixtures) ────────────────────────────

#[tokio::test]
async fn find_own_matches_private_owner_and_tenant_shared() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);

    seed_active(&repo, tenant, owner, "priv", SharingMode::Private).await;
    let own = repo
        .find_own(&scope, TenantId(tenant), OwnerId(owner), &sref("priv"))
        .await
        .expect("find_own")
        .expect("private row found by its owner");
    assert_eq!(own.sharing, SharingMode::Private);

    let other = repo
        .find_own(
            &scope,
            TenantId(tenant),
            OwnerId(Uuid::new_v4()),
            &sref("priv"),
        )
        .await
        .expect("find_own");
    assert!(other.is_none());

    seed_active(&repo, tenant, owner, "team", SharingMode::Tenant).await;
    let team = repo
        .find_own(
            &scope,
            TenantId(tenant),
            OwnerId(Uuid::new_v4()),
            &sref("team"),
        )
        .await
        .expect("find_own")
        .expect("tenant-shared row visible");
    assert_eq!(team.sharing, SharingMode::Tenant);
}

#[tokio::test]
async fn find_for_write_addresses_sharing_class_for_coexistence() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);

    seed_active(&repo, tenant, owner, "dup", SharingMode::Tenant).await;
    seed_active(&repo, tenant, owner, "dup", SharingMode::Private).await;

    let private = repo
        .find_for_write(
            &scope,
            TenantId(tenant),
            OwnerId(owner),
            &sref("dup"),
            SharingMode::Private,
        )
        .await
        .expect("find_for_write")
        .expect("private row addressed");
    assert_eq!(private.sharing, SharingMode::Private);

    for write_sharing in [SharingMode::Tenant, SharingMode::Shared] {
        let nonprivate = repo
            .find_for_write(
                &scope,
                TenantId(tenant),
                OwnerId(owner),
                &sref("dup"),
                write_sharing,
            )
            .await
            .expect("find_for_write")
            .expect("non-private row addressed");
        assert_eq!(nonprivate.sharing, SharingMode::Tenant);
    }

    let other = repo
        .find_for_write(
            &scope,
            TenantId(tenant),
            OwnerId(Uuid::new_v4()),
            &sref("dup"),
            SharingMode::Private,
        )
        .await
        .expect("find_for_write");
    assert!(other.is_none());
}

#[tokio::test]
async fn resolve_for_get_prefers_closest_tenant_then_private() {
    let repo = setup().await;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let owner = Uuid::new_v4();

    seed_active(&repo, parent, owner, "cfg", SharingMode::Shared).await;
    seed_active(&repo, child, owner, "cfg", SharingMode::Private).await;

    let row = repo
        .resolve_for_get(
            TenantId(child),
            OwnerId(owner),
            &sref("cfg"),
            &[child, parent],
        )
        .await
        .expect("resolve")
        .expect("row resolved");
    assert_eq!(row.tenant_id, TenantId(child));
    assert_eq!(row.sharing, SharingMode::Private);

    let none = repo
        .resolve_for_get(
            TenantId(child),
            OwnerId(owner),
            &sref("absent"),
            &[child, parent],
        )
        .await
        .expect("resolve");
    assert!(none.is_none());
}

#[tokio::test]
async fn resolve_for_get_excludes_parent_tenant_mode_but_inherits_shared() {
    let repo = setup().await;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let owner = Uuid::new_v4();

    seed_active(&repo, parent, owner, "tenant-only", SharingMode::Tenant).await;
    seed_active(&repo, parent, owner, "shared-cfg", SharingMode::Shared).await;

    let tenant_only = repo
        .resolve_for_get(
            TenantId(child),
            OwnerId(owner),
            &sref("tenant-only"),
            &[child, parent],
        )
        .await
        .expect("resolve");
    assert!(
        tenant_only.is_none(),
        "parent Tenant-mode secret must not be inherited by a child"
    );

    let shared = repo
        .resolve_for_get(
            TenantId(child),
            OwnerId(owner),
            &sref("shared-cfg"),
            &[child, parent],
        )
        .await
        .expect("resolve")
        .expect("shared row must be inherited");
    assert_eq!(shared.sharing, SharingMode::Shared);
    assert_eq!(shared.tenant_id, TenantId(parent));
    assert_ne!(shared.tenant_id, TenantId(child));
}

#[tokio::test]
async fn resolve_for_get_never_inherits_ancestor_private_even_for_same_owner() {
    let repo = setup().await;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let owner = Uuid::new_v4();

    seed_active(&repo, parent, owner, "priv-key", SharingMode::Private).await;

    let resolved = repo
        .resolve_for_get(
            TenantId(child),
            OwnerId(owner),
            &sref("priv-key"),
            &[child, parent],
        )
        .await
        .expect("resolve");
    assert!(
        resolved.is_none(),
        "an ancestor's private secret must not resolve for a descendant, even with a matching owner id"
    );
}

#[tokio::test]
async fn new_secret_defaults_to_version_one() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    seed_active(&repo, tenant, owner, "v1", SharingMode::Tenant).await;

    let row = repo
        .resolve_for_get(TenantId(tenant), OwnerId(owner), &sref("v1"), &[tenant])
        .await
        .expect("resolve")
        .expect("active row");
    assert_eq!(
        row.version, 1,
        "a freshly inserted secret starts at version 1"
    );
}

#[tokio::test]
async fn secret_type_round_trips_through_storage() {
    let repo = setup().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let mut new = new_secret(
        tenant,
        owner,
        "typed",
        SharingMode::Private,
        ValueId::new_v4(),
    );
    new.secret_type_uuid = SecretType::from_name("personal-token")
        .expect("known")
        .uuid();
    repo.insert_active(&scope, &new).await.expect("insert");

    let row = repo
        .resolve_for_get(TenantId(tenant), OwnerId(owner), &sref("typed"), &[tenant])
        .await
        .expect("resolve")
        .expect("row");
    assert_eq!(
        row.secret_type_uuid,
        SecretType::from_name("personal-token")
            .expect("known")
            .uuid()
    );
}

// ── scope_includes_tenant ────────────────────────────────────────────────────

#[tokio::test]
async fn scope_includes_tenant_unconstrained_and_deny() {
    let repo = setup().await;
    let t = Uuid::new_v4();
    assert!(
        repo.scope_includes_tenant(&AccessScope::allow_all(), t)
            .await
            .expect("allow_all")
    );
    assert!(
        !repo
            .scope_includes_tenant(&AccessScope::deny_all(), t)
            .await
            .expect("deny_all")
    );
}

#[tokio::test]
async fn scope_includes_tenant_direct_uuid_match() {
    let repo = setup().await;
    let t = Uuid::new_v4();
    assert!(
        repo.scope_includes_tenant(&AccessScope::for_tenant(t), t)
            .await
            .expect("direct match")
    );
    assert!(
        !repo
            .scope_includes_tenant(&AccessScope::for_tenant(t), Uuid::new_v4())
            .await
            .expect("direct miss")
    );
}

#[tokio::test]
async fn scope_includes_tenant_structured_subtree_fails_closed() {
    let repo = setup().await;
    let root = Uuid::new_v4();
    let scope = subtree_scope(root);
    assert!(
        !repo
            .scope_includes_tenant(&scope, root)
            .await
            .expect("structured subtree predicate must fail closed")
    );
    assert!(
        !repo
            .scope_includes_tenant(&scope, Uuid::new_v4())
            .await
            .expect("structured subtree predicate must fail closed for any tenant")
    );
}

#[tokio::test]
async fn scope_includes_tenant_sibling_owner_filter_fails_closed() {
    let repo = setup().await;
    let t = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
        ScopeFilter::eq(pep_properties::OWNER_ID, owner),
    ])]);
    assert!(
        !repo
            .scope_includes_tenant(&scope, t)
            .await
            .expect("sibling owner filter must fail closed"),
        "a sub-tenant scope must not be widened to the whole tenant"
    );
}

#[tokio::test]
async fn scope_includes_tenant_unknown_property_fails_closed() {
    let repo = setup().await;
    let t = Uuid::new_v4();
    let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![ScopeFilter::eq(
        pep_properties::RESOURCE_ID,
        Uuid::new_v4(),
    )])]);
    assert!(
        !repo
            .scope_includes_tenant(&scope, t)
            .await
            .expect("non-tenant property must fail closed")
    );
}

#[tokio::test]
async fn scope_includes_tenant_or_of_constraints_admits_on_broad_alternative() {
    let repo = setup().await;
    let t = Uuid::new_v4();
    let scope = AccessScope::from_constraints(vec![
        ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t),
            ScopeFilter::eq(pep_properties::OWNER_ID, Uuid::new_v4()),
        ]),
        ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, t)]),
    ]);
    assert!(
        repo.scope_includes_tenant(&scope, t)
            .await
            .expect("broad OR alternative admits"),
        "a whole-tenant alternative must still grant despite a narrower sibling"
    );
}
