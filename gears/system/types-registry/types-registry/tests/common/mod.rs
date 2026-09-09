#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

//! Common test utilities for types-registry integration tests.
//!
//! `dead_code` is allowed because each test binary includes this module whole and
//! uses only the helpers it needs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gts::GtsConfig;
use types_registry::{
    config::TypesRegistryConfig, domain::service::TypesRegistryService,
    infra::InMemoryGtsRepository,
};

pub fn default_config() -> GtsConfig {
    TypesRegistryConfig::default().to_gts_config()
}

pub fn create_service() -> Arc<TypesRegistryService> {
    let repo = Arc::new(InMemoryGtsRepository::new(default_config()));
    Arc::new(TypesRegistryService::new(
        repo,
        TypesRegistryConfig::default(),
    ))
}

/// Per-test temporary directory removed during unwinding as well as on success.
///
/// Declaring the guard before a database provider makes Rust drop the provider
/// first, so `SQLite` has released the file by the time this cleanup runs.
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            eprintln!(
                "failed to clean up test directory {}: {error}",
                self.path.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Database harness
// ---------------------------------------------------------------------------

use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};

/// In-memory `SQLite` with the managed-state migration applied.
///
/// `max_conns = 1`: a bare `sqlite::memory:` gives every pooled connection its
/// own empty database, so a second connection would see no tables at all.
pub async fn test_db() -> Arc<DBProvider<DbError>> {
    provider_for("sqlite::memory:", 1).await
}

/// File-backed `SQLite` with a real pool, for the tests that need more than one
/// connection against the same database. The caller owns the temporary
/// directory and drops it to clean up.
pub async fn test_db_file(path: &std::path::Path) -> Arc<DBProvider<DbError>> {
    let dsn = format!("sqlite://{}?mode=rwc", path.display());
    provider_for(&dsn, 4).await
}

/// Any DSN with the managed-state migration applied. The `integration` suite
/// hands this a container DSN so the `PostgreSQL` and `MySQL` repository tests
/// exercise the same code path as the `SQLite` ones.
pub async fn provider_for(dsn: &str, max_conns: u32) -> Arc<DBProvider<DbError>> {
    let opts = ConnectOpts {
        max_conns: Some(max_conns),
        min_conns: Some(1),
        ..Default::default()
    };
    let dsn_scheme = dsn.split(':').next().unwrap_or("database");
    let db = connect_db(dsn, opts)
        .await
        .unwrap_or_else(|e| panic!("connect {dsn_scheme} test database: {e}"));
    run_migrations_for_testing(&db, migrations())
        .await
        .expect("run migrations");
    Arc::new(DBProvider::new(db))
}

fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    use sea_orm_migration::MigratorTrait;
    types_registry::infra::storage::Migrator::migrations()
}

/// The database-backed persistence ports, as the gear wires them. Tests that
/// drive `accept` / `run_operation` / `RegistryService` pass this: the domain names
/// only its ports, so the adapter is chosen here exactly as `init()` chooses it.
pub fn stores() -> Arc<dyn types_registry::domain::ports::Stores> {
    Arc::new(types_registry::infra::storage::Repos)
}

/// The scope every P0 read and write runs under. P0 has no PDP (ceiling C6) and
/// the entities are `#[secure(unrestricted)]`, so `allow_all` is the honest
/// value: a legitimate authorization outcome with no row-level filtering, not a
/// bypass. `AccessScope::default()` is deny-all, which is what an unset scope
/// would give.
pub fn allow_all() -> AccessScope {
    AccessScope::allow_all()
}

#[must_use]
pub fn metrics() -> std::sync::Arc<dyn types_registry::domain::ports::metrics::AdmissionMetrics> {
    std::sync::Arc::new(types_registry::domain::ports::metrics::NoopMetrics)
}

pub fn limits() -> types_registry::config::Limits {
    types_registry::config::Limits::default()
}

pub fn worker_settings() -> types_registry::config::WorkerSettings {
    types_registry::config::WorkerSettings::default()
}

// ---------------------------------------------------------------------------
// Managed-state fixtures
// ---------------------------------------------------------------------------
//
// `type_schema` and `type_schema_revision` have no repository writer: those writes
// belong to the admission worker, their first caller. Tests that need admitted rows
// write them through their `ActiveModel`s and share the operation → item → revision
// → current-pointer boilerplate here.

use sea_orm::ActiveValue::Set;
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, secure_insert};
use types_registry::infra::storage::entity::enums::{
    OperationItemStatus, OperationKind, OperationStatus, Plane,
};
use types_registry::infra::storage::entity::{
    operation, operation_item, type_schema, type_schema_revision,
};
use uuid::Uuid;

/// A completed registration operation and its single item, which every revision
/// row needs: `type_schema_revision.operation_item_id` is a `RESTRICT` foreign
/// key pinning the admitting provenance. Returns the item id.
pub async fn seed_operation_item(
    runner: &impl DBRunner,
    gts_id: &str,
    revision_no: i32,
    now: OffsetDateTime,
) -> i64 {
    let scope = allow_all();
    let op_id = Uuid::new_v4();
    secure_insert::<operation::Entity>(
        operation::ActiveModel {
            id: Set(op_id),
            kind: Set(OperationKind::Registration),
            dry_run: Set(false),
            plane: Set(Plane::Platform),
            tenant_id: Set(None),
            principal_id: Set(Uuid::from_u128(0xB1)),
            idempotency_key: Set(format!("idem-{op_id}")),
            idempotency_scope_hash: Set(vec![0x01]),
            request_fingerprint: Set(vec![0x02]),
            status: Set(OperationStatus::Completed),
            created_at: Set(now),
            started_at: Set(Some(now)),
            completed_at: Set(Some(now)),
        },
        &scope,
        runner,
    )
    .await
    .expect("insert operation");

    let item = secure_insert::<operation_item::Entity>(
        operation_item::ActiveModel {
            operation_id: Set(op_id),
            item_no: Set(0),
            gts_id: Set(gts_id.to_owned()),
            dry_run: Set(false),
            kind: Set(OperationKind::Registration),
            expected_resource_version: Set(0),
            status: Set(OperationItemStatus::Succeeded),
            request_payload: Set(None),
            result_revision_no: Set(Some(revision_no)),
            result_resource_version: Set(Some(i64::from(revision_no))),
            error_payload: Set(None),
            created_at: Set(now),
            started_at: Set(Some(now)),
            completed_at: Set(Some(now)),
            ..Default::default()
        },
        &scope,
        runner,
    )
    .await
    .expect("insert operation item");
    item.id
}

/// A **pending** item naming a positive `expected_resource_version`: the input a
/// revision commit terminalizes, and the only shape `mark_item_unchanged` accepts
/// (`ck_tr_operation_item_state` requires `expected_resource_version >= 1` for
/// `unchanged`). Returns the item id.
pub async fn seed_pending_revision_item(
    runner: &impl DBRunner,
    gts_id: &str,
    expected_resource_version: i64,
    now: OffsetDateTime,
) -> i64 {
    let scope = allow_all();
    let op_id = Uuid::new_v4();
    secure_insert::<operation::Entity>(
        operation::ActiveModel {
            id: Set(op_id),
            kind: Set(OperationKind::Registration),
            dry_run: Set(false),
            plane: Set(Plane::Platform),
            tenant_id: Set(None),
            principal_id: Set(Uuid::from_u128(0xB1)),
            idempotency_key: Set(format!("idem-{op_id}")),
            idempotency_scope_hash: Set(vec![0x01]),
            request_fingerprint: Set(vec![0x02]),
            status: Set(OperationStatus::Running),
            created_at: Set(now),
            started_at: Set(Some(now)),
            completed_at: Set(None),
        },
        &scope,
        runner,
    )
    .await
    .expect("insert operation");

    let item = secure_insert::<operation_item::Entity>(
        operation_item::ActiveModel {
            operation_id: Set(op_id),
            item_no: Set(0),
            gts_id: Set(gts_id.to_owned()),
            dry_run: Set(false),
            kind: Set(OperationKind::Registration),
            expected_resource_version: Set(expected_resource_version),
            status: Set(OperationItemStatus::Pending),
            request_payload: Set(Some("{}".to_owned())),
            result_revision_no: Set(None),
            result_resource_version: Set(None),
            error_payload: Set(None),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        },
        &scope,
        runner,
    )
    .await
    .expect("insert pending operation item");
    item.id
}

/// One immutable authored revision.
pub async fn seed_type_schema_revision(
    runner: &impl DBRunner,
    entity_id: i64,
    revision_no: i32,
    operation_item_id: i64,
    raw_schema: &str,
    now: OffsetDateTime,
) {
    secure_insert::<type_schema_revision::Entity>(
        type_schema_revision::ActiveModel {
            entity_id: Set(entity_id),
            revision_no: Set(revision_no),
            raw_schema: Set(raw_schema.to_owned()),
            content_hash: Set(vec![u8::try_from(revision_no).expect("small revision")]),
            gts_spec_version: Set(gts::GTS_SPECIFICATION_VERSION.to_owned()),
            gts_impl_version: Set(gts::GTS_IMPLEMENTATION_VERSION.to_owned()),
            compat_forced: Set(false),
            operation_item_id: Set(operation_item_id),
            created_at: Set(now),
            updated_at: Set(now),
        },
        &allow_all(),
        runner,
    )
    .await
    .expect("insert type schema revision");
}

/// The current-state row pointing at a revision. The resolved artifacts are
/// placeholders: nothing under test reads them, because the transient store
/// resolves from the *authored* document (D3).
pub async fn seed_current_type_schema(
    runner: &impl DBRunner,
    entity_id: i64,
    revision_no: i32,
    resolved_schema: &str,
    now: OffsetDateTime,
) {
    secure_insert::<type_schema::Entity>(
        type_schema::ActiveModel {
            entity_id: Set(entity_id),
            revision_no: Set(revision_no),
            resolved_schema: Set(resolved_schema.to_owned()),
            effective_traits: Set("{}".to_owned()),
            effective_traits_schema: Set("{}".to_owned()),
            resolution_fingerprint: Set(vec![0x11]),
            created_at: Set(now),
            updated_at: Set(now),
        },
        &allow_all(),
        runner,
    )
    .await
    .expect("insert current type schema");
}

// ---------------------------------------------------------------------------
// A commit paused mid-transaction (shared by the concurrency suites)
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use toolkit_db::DbTx;
use toolkit_db::secure::ScopeError;
use types_registry::domain::admission::fingerprint::ScopeHash;
use types_registry::domain::enums::{DependencyKind, EntityKind, OwnershipScope};
use types_registry::domain::family::FamilyKey;
use types_registry::domain::ports::{
    CurrentDocument, CurrentInstanceRow, CurrentInstanceValue, CurrentSchemaCas,
    CurrentTypeSchemaRow, DependencyClosure, DependencyStore, EntityRow, EntityStore,
    EntityWriteOrderStore, InstanceStore, NewCurrentInstance, NewCurrentTypeSchema, NewEntity,
    NewInstanceRevision, NewOperation, NewOperationItem, NewRevision, OperationItemRow,
    OperationRow, OperationStore, ReverseImpact, TypeSchemaStore, VersionFamilyRow,
    VersionFamilyStore,
};

// ---------------------------------------------------------------------------
// A commit paused mid-transaction
// ---------------------------------------------------------------------------

/// Which statement of the commit transaction [`PausingStores`] holds the pass at.
///
/// Named points rather than a closure, because each one is a *place in the commit
/// protocol* a test is making a claim about, and the name is that claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausePoint {
    /// Immediately before the commit's first statement claims `entity_write_order`.
    BeforeEntityWriteOrderClaim,
    /// Between `commit_revision`'s entity read and both of its concurrency
    /// branches: the window the `unchanged` re-read and the compare-and-swap exist
    /// to close.
    CurrentDocuments,
    /// After creation takes the family row but before checking its rules.
    CreateOrGet,
    RevisionEntityRead,
}

/// Every port forwarded to the real adapter, with one call held on a channel.
///
/// A decorator rather than a hand-written fake: everything the commit path reads
/// and writes must be the real thing, or a test would be pinning the fake's
/// behaviour instead of the transaction's. Only the *timing* is the test's.
pub struct PausingStores {
    inner: Arc<dyn types_registry::domain::ports::Stores>,
    at: PausePoint,
    /// Which call of `at` to hold on: 1 is the first.
    nth: usize,
    /// How many calls of `at` have been seen.
    seen: std::sync::atomic::AtomicUsize,
    /// Sent once, when the pass reaches the pause point.
    reached: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Awaited once, before that call returns.
    resume: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl PausingStores {
    /// The decorated ports, a receiver that fires when the pass reaches `at`, and
    /// the sender that lets it go.
    #[must_use]
    pub fn new(
        at: PausePoint,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        Self::new_at_occurrence(at, 1)
    }

    /// Like [`Self::new`], but pause at the `nth` matching call.
    #[must_use]
    pub fn new_at_occurrence(
        at: PausePoint,
        nth: usize,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let decorated = Arc::new(Self {
            inner: stores(),
            at,
            nth,
            seen: std::sync::atomic::AtomicUsize::new(0),
            reached: tokio::sync::Mutex::new(Some(reached_tx)),
            resume: tokio::sync::Mutex::new(Some(resume_rx)),
        });
        (decorated, reached_rx, resume_tx)
    }

    /// Signal the caller and wait to be let go. A no-op on any call after the
    /// first, so a commit path that grew a second call at the same point would not
    /// deadlock the suite.
    async fn pause(&self, at: PausePoint) {
        if at != self.at {
            return;
        }
        if self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 != self.nth {
            return;
        }
        let reached = self.reached.lock().await.take();
        let resume = self.resume.lock().await.take();
        if let Some(reached) = reached {
            // The receiver is dropped only if the test already gave up; that is the
            // test's own failure to report, not this helper's.
            reached.send(()).ok();
        }
        if let Some(resume) = resume {
            resume.await.expect("the test must always resume the pass");
        }
    }
}

#[async_trait]
impl EntityWriteOrderStore for PausingStores {
    async fn claim_entity_write_order(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        now: OffsetDateTime,
    ) -> Result<(), ScopeError> {
        self.pause(PausePoint::BeforeEntityWriteOrderClaim).await;
        self.inner.claim_entity_write_order(tx, scope, now).await
    }
}

#[async_trait]
impl VersionFamilyStore for PausingStores {
    async fn create_or_get(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_key: &FamilyKey,
        ownership_scope: OwnershipScope,
        owner_tenant_id: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<(VersionFamilyRow, bool), ScopeError> {
        let out = self
            .inner
            .create_or_get(tx, scope, family_key, ownership_scope, owner_tenant_id, now)
            .await?;
        self.pause(PausePoint::CreateOrGet).await;
        Ok(out)
    }
}

#[async_trait]
impl EntityStore for PausingStores {
    async fn find_by_gts_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_id: &str,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.pause(PausePoint::RevisionEntityRead).await;
        self.inner.find_by_gts_id(tx, scope, gts_id).await
    }

    async fn find_by_gts_ids(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_ids: &[String],
    ) -> Result<Vec<EntityRow>, ScopeError> {
        self.inner.find_by_gts_ids(tx, scope, gts_ids).await
    }

    async fn find_by_gts_uuid(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_uuid: Uuid,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.find_by_gts_uuid(tx, scope, gts_uuid).await
    }

    async fn kind_in_family(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_id: i64,
    ) -> Result<Option<EntityKind>, ScopeError> {
        self.inner.kind_in_family(tx, scope, family_id).await
    }

    async fn insert_entity(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewEntity,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.insert_entity(tx, scope, new).await
    }

    async fn compare_and_swap_version(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
        expected_resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<Option<i64>, ScopeError> {
        self.inner
            .compare_and_swap_version(tx, scope, entity_id, expected_resource_version, now)
            .await
    }
}

#[async_trait]
impl TypeSchemaStore for PausingStores {
    async fn current_documents(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentDocument>, ScopeError> {
        let out = self.inner.current_documents(tx, scope, entity_ids).await?;
        self.pause(PausePoint::CurrentDocuments).await;
        Ok(out)
    }

    async fn current_schemas(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentTypeSchemaRow>, ScopeError> {
        self.inner.current_schemas(tx, scope, entity_ids).await
    }

    async fn find_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentTypeSchemaRow>, ScopeError> {
        self.inner.find_current_schema(tx, scope, entity_id).await
    }

    async fn insert_schema_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewRevision,
    ) -> Result<(), ScopeError> {
        self.inner.insert_schema_revision(tx, scope, new).await
    }

    async fn insert_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
    ) -> Result<(), ScopeError> {
        self.inner.insert_current_schema(tx, scope, new).await
    }

    async fn update_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
        expected: CurrentSchemaCas,
    ) -> Result<bool, ScopeError> {
        self.inner
            .update_current_schema(tx, scope, new, expected)
            .await
    }
}

#[async_trait]
impl InstanceStore for PausingStores {
    async fn current_values(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentInstanceValue>, ScopeError> {
        self.inner.current_values(tx, scope, entity_ids).await
    }

    async fn find_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentInstanceRow>, ScopeError> {
        self.inner.find_current_instance(tx, scope, entity_id).await
    }

    async fn insert_instance_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewInstanceRevision,
    ) -> Result<(), ScopeError> {
        self.inner.insert_instance_revision(tx, scope, new).await
    }

    async fn insert_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<(), ScopeError> {
        self.inner.insert_current_instance(tx, scope, new).await
    }

    async fn update_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<bool, ScopeError> {
        self.inner.update_current_instance(tx, scope, new).await
    }
}

#[async_trait]
impl OperationStore for PausingStores {
    async fn find_by_idempotency(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        idempotency_scope_hash: &ScopeHash,
        idempotency_key: &str,
    ) -> Result<Option<OperationRow>, ScopeError> {
        self.inner
            .find_by_idempotency(tx, scope, idempotency_scope_hash, idempotency_key)
            .await
    }

    async fn find_by_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<OperationRow>, ScopeError> {
        self.inner.find_by_id(tx, scope, id).await
    }

    async fn insert_operation(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewOperation,
    ) -> Result<OperationRow, ScopeError> {
        self.inner.insert_operation(tx, scope, new).await
    }

    async fn insert_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        parent: &OperationRow,
        items: &[NewOperationItem],
    ) -> Result<(), ScopeError> {
        self.inner.insert_items(tx, scope, parent, items).await
    }

    async fn find_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        operation_id: Uuid,
    ) -> Result<Vec<OperationItemRow>, ScopeError> {
        self.inner.find_items(tx, scope, operation_id).await
    }

    async fn mark_running(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner.mark_running(tx, scope, id, now).await
    }

    async fn mark_completed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner.mark_completed(tx, scope, id, now).await
    }

    async fn mark_item_succeeded(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        revision_no: i32,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_succeeded(tx, scope, item_id, revision_no, resource_version, now)
            .await
    }

    async fn mark_item_unchanged(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_unchanged(tx, scope, item_id, resource_version, now)
            .await
    }

    async fn mark_item_failed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        error_payload: String,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_failed(tx, scope, item_id, error_payload, now)
            .await
    }
}

#[async_trait]
impl DependencyStore for PausingStores {
    async fn closure(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[String],
    ) -> Result<DependencyClosure, ScopeError> {
        self.inner.closure(tx, scope, roots).await
    }

    async fn reverse_impact(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[i64],
        write_set_bound: usize,
    ) -> Result<ReverseImpact, ScopeError> {
        self.inner
            .reverse_impact(tx, scope, roots, write_set_bound)
            .await
    }

    async fn replace_outgoing(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        from_entity_id: i64,
        edges: &[(DependencyKind, i64)],
    ) -> Result<(), ScopeError> {
        self.inner
            .replace_outgoing(tx, scope, from_entity_id, edges)
            .await
    }
}

// ---------------------------------------------------------------------------
// A commit that announces when it reaches the serialization point
// ---------------------------------------------------------------------------

/// Real adapter wrapper that signals entry into `claim_entity_write_order`.
pub struct ClaimSignallingStores {
    inner: Arc<dyn types_registry::domain::ports::Stores>,
    entered: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl ClaimSignallingStores {
    /// The decorated ports and a receiver that fires when the claim is entered.
    #[must_use]
    pub fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                inner: stores(),
                entered: tokio::sync::Mutex::new(Some(tx)),
            }),
            rx,
        )
    }
}

#[async_trait]
impl EntityWriteOrderStore for ClaimSignallingStores {
    async fn claim_entity_write_order(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        now: OffsetDateTime,
    ) -> Result<(), ScopeError> {
        if let Some(entered) = self.entered.lock().await.take() {
            // Dropped only if the test gave up, which is its own failure to report.
            entered.send(()).ok();
        }
        self.inner.claim_entity_write_order(tx, scope, now).await
    }
}

#[async_trait]
impl VersionFamilyStore for ClaimSignallingStores {
    async fn create_or_get(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_key: &FamilyKey,
        ownership_scope: OwnershipScope,
        owner_tenant_id: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<(VersionFamilyRow, bool), ScopeError> {
        let out = self
            .inner
            .create_or_get(tx, scope, family_key, ownership_scope, owner_tenant_id, now)
            .await?;
        Ok(out)
    }
}

#[async_trait]
impl EntityStore for ClaimSignallingStores {
    async fn find_by_gts_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_id: &str,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.find_by_gts_id(tx, scope, gts_id).await
    }

    async fn find_by_gts_ids(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_ids: &[String],
    ) -> Result<Vec<EntityRow>, ScopeError> {
        self.inner.find_by_gts_ids(tx, scope, gts_ids).await
    }

    async fn find_by_gts_uuid(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_uuid: Uuid,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.find_by_gts_uuid(tx, scope, gts_uuid).await
    }

    async fn kind_in_family(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_id: i64,
    ) -> Result<Option<EntityKind>, ScopeError> {
        self.inner.kind_in_family(tx, scope, family_id).await
    }

    async fn insert_entity(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewEntity,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.insert_entity(tx, scope, new).await
    }

    async fn compare_and_swap_version(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
        expected_resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<Option<i64>, ScopeError> {
        self.inner
            .compare_and_swap_version(tx, scope, entity_id, expected_resource_version, now)
            .await
    }
}

#[async_trait]
impl TypeSchemaStore for ClaimSignallingStores {
    async fn current_documents(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentDocument>, ScopeError> {
        let out = self.inner.current_documents(tx, scope, entity_ids).await?;
        Ok(out)
    }

    async fn current_schemas(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentTypeSchemaRow>, ScopeError> {
        self.inner.current_schemas(tx, scope, entity_ids).await
    }

    async fn find_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentTypeSchemaRow>, ScopeError> {
        self.inner.find_current_schema(tx, scope, entity_id).await
    }

    async fn insert_schema_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewRevision,
    ) -> Result<(), ScopeError> {
        self.inner.insert_schema_revision(tx, scope, new).await
    }

    async fn insert_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
    ) -> Result<(), ScopeError> {
        self.inner.insert_current_schema(tx, scope, new).await
    }

    async fn update_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
        expected: CurrentSchemaCas,
    ) -> Result<bool, ScopeError> {
        self.inner
            .update_current_schema(tx, scope, new, expected)
            .await
    }
}

#[async_trait]
impl InstanceStore for ClaimSignallingStores {
    async fn current_values(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentInstanceValue>, ScopeError> {
        self.inner.current_values(tx, scope, entity_ids).await
    }

    async fn find_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentInstanceRow>, ScopeError> {
        self.inner.find_current_instance(tx, scope, entity_id).await
    }

    async fn insert_instance_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewInstanceRevision,
    ) -> Result<(), ScopeError> {
        self.inner.insert_instance_revision(tx, scope, new).await
    }

    async fn insert_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<(), ScopeError> {
        self.inner.insert_current_instance(tx, scope, new).await
    }

    async fn update_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<bool, ScopeError> {
        self.inner.update_current_instance(tx, scope, new).await
    }
}

#[async_trait]
impl OperationStore for ClaimSignallingStores {
    async fn find_by_idempotency(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        idempotency_scope_hash: &ScopeHash,
        idempotency_key: &str,
    ) -> Result<Option<OperationRow>, ScopeError> {
        self.inner
            .find_by_idempotency(tx, scope, idempotency_scope_hash, idempotency_key)
            .await
    }

    async fn find_by_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<OperationRow>, ScopeError> {
        self.inner.find_by_id(tx, scope, id).await
    }

    async fn insert_operation(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewOperation,
    ) -> Result<OperationRow, ScopeError> {
        self.inner.insert_operation(tx, scope, new).await
    }

    async fn insert_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        parent: &OperationRow,
        items: &[NewOperationItem],
    ) -> Result<(), ScopeError> {
        self.inner.insert_items(tx, scope, parent, items).await
    }

    async fn find_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        operation_id: Uuid,
    ) -> Result<Vec<OperationItemRow>, ScopeError> {
        self.inner.find_items(tx, scope, operation_id).await
    }

    async fn mark_running(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner.mark_running(tx, scope, id, now).await
    }

    async fn mark_completed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner.mark_completed(tx, scope, id, now).await
    }

    async fn mark_item_succeeded(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        revision_no: i32,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_succeeded(tx, scope, item_id, revision_no, resource_version, now)
            .await
    }

    async fn mark_item_unchanged(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_unchanged(tx, scope, item_id, resource_version, now)
            .await
    }

    async fn mark_item_failed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        error_payload: String,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_failed(tx, scope, item_id, error_payload, now)
            .await
    }
}

#[async_trait]
impl DependencyStore for ClaimSignallingStores {
    async fn closure(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[String],
    ) -> Result<DependencyClosure, ScopeError> {
        let out = self.inner.closure(tx, scope, roots).await?;
        Ok(out)
    }

    async fn reverse_impact(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[i64],
        write_set_bound: usize,
    ) -> Result<ReverseImpact, ScopeError> {
        self.inner
            .reverse_impact(tx, scope, roots, write_set_bound)
            .await
    }

    async fn replace_outgoing(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        from_entity_id: i64,
        edges: &[(DependencyKind, i64)],
    ) -> Result<(), ScopeError> {
        self.inner
            .replace_outgoing(tx, scope, from_entity_id, edges)
            .await
    }
}

// ---------------------------------------------------------------------------
// A current-state write that loses its compare-and-swap
// ---------------------------------------------------------------------------

/// Real adapter wrapper that injects one current-schema CAS miss.
pub struct CasMissStores {
    inner: Arc<dyn types_registry::domain::ports::Stores>,
    refuse_for_entity_id: i64,
}

impl CasMissStores {
    /// Inject a CAS miss for `refuse_for_entity_id`.
    #[must_use]
    pub fn new(refuse_for_entity_id: i64) -> Arc<Self> {
        Arc::new(Self {
            inner: stores(),
            refuse_for_entity_id,
        })
    }
}

#[async_trait]
impl EntityWriteOrderStore for CasMissStores {
    async fn claim_entity_write_order(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        now: OffsetDateTime,
    ) -> Result<(), ScopeError> {
        self.inner.claim_entity_write_order(tx, scope, now).await
    }
}

#[async_trait]
impl VersionFamilyStore for CasMissStores {
    async fn create_or_get(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_key: &FamilyKey,
        ownership_scope: OwnershipScope,
        owner_tenant_id: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<(VersionFamilyRow, bool), ScopeError> {
        self.inner
            .create_or_get(tx, scope, family_key, ownership_scope, owner_tenant_id, now)
            .await
    }
}

#[async_trait]
impl EntityStore for CasMissStores {
    async fn find_by_gts_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_id: &str,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.find_by_gts_id(tx, scope, gts_id).await
    }

    async fn find_by_gts_ids(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_ids: &[String],
    ) -> Result<Vec<EntityRow>, ScopeError> {
        self.inner.find_by_gts_ids(tx, scope, gts_ids).await
    }

    async fn find_by_gts_uuid(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_uuid: Uuid,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.find_by_gts_uuid(tx, scope, gts_uuid).await
    }

    async fn kind_in_family(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_id: i64,
    ) -> Result<Option<EntityKind>, ScopeError> {
        self.inner.kind_in_family(tx, scope, family_id).await
    }

    async fn insert_entity(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewEntity,
    ) -> Result<Option<EntityRow>, ScopeError> {
        self.inner.insert_entity(tx, scope, new).await
    }

    async fn compare_and_swap_version(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
        expected_resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<Option<i64>, ScopeError> {
        self.inner
            .compare_and_swap_version(tx, scope, entity_id, expected_resource_version, now)
            .await
    }
}

#[async_trait]
impl TypeSchemaStore for CasMissStores {
    async fn current_documents(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentDocument>, ScopeError> {
        self.inner.current_documents(tx, scope, entity_ids).await
    }

    async fn current_schemas(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentTypeSchemaRow>, ScopeError> {
        self.inner.current_schemas(tx, scope, entity_ids).await
    }

    async fn find_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentTypeSchemaRow>, ScopeError> {
        self.inner.find_current_schema(tx, scope, entity_id).await
    }

    async fn insert_schema_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewRevision,
    ) -> Result<(), ScopeError> {
        self.inner.insert_schema_revision(tx, scope, new).await
    }

    async fn insert_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
    ) -> Result<(), ScopeError> {
        self.inner.insert_current_schema(tx, scope, new).await
    }

    async fn update_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
        expected: CurrentSchemaCas,
    ) -> Result<bool, ScopeError> {
        if new.entity_id == self.refuse_for_entity_id {
            // Simulate the projection moving after its token was captured.
            return Ok(false);
        }
        self.inner
            .update_current_schema(tx, scope, new, expected)
            .await
    }
}

#[async_trait]
impl InstanceStore for CasMissStores {
    async fn current_values(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentInstanceValue>, ScopeError> {
        self.inner.current_values(tx, scope, entity_ids).await
    }

    async fn find_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentInstanceRow>, ScopeError> {
        self.inner.find_current_instance(tx, scope, entity_id).await
    }

    async fn insert_instance_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewInstanceRevision,
    ) -> Result<(), ScopeError> {
        self.inner.insert_instance_revision(tx, scope, new).await
    }

    async fn insert_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<(), ScopeError> {
        self.inner.insert_current_instance(tx, scope, new).await
    }

    async fn update_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<bool, ScopeError> {
        self.inner.update_current_instance(tx, scope, new).await
    }
}

#[async_trait]
impl OperationStore for CasMissStores {
    async fn find_by_idempotency(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        idempotency_scope_hash: &ScopeHash,
        idempotency_key: &str,
    ) -> Result<Option<OperationRow>, ScopeError> {
        self.inner
            .find_by_idempotency(tx, scope, idempotency_scope_hash, idempotency_key)
            .await
    }

    async fn find_by_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<OperationRow>, ScopeError> {
        self.inner.find_by_id(tx, scope, id).await
    }

    async fn insert_operation(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewOperation,
    ) -> Result<OperationRow, ScopeError> {
        self.inner.insert_operation(tx, scope, new).await
    }

    async fn insert_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        parent: &OperationRow,
        items: &[NewOperationItem],
    ) -> Result<(), ScopeError> {
        self.inner.insert_items(tx, scope, parent, items).await
    }

    async fn find_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        operation_id: Uuid,
    ) -> Result<Vec<OperationItemRow>, ScopeError> {
        self.inner.find_items(tx, scope, operation_id).await
    }

    async fn mark_running(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner.mark_running(tx, scope, id, now).await
    }

    async fn mark_completed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner.mark_completed(tx, scope, id, now).await
    }

    async fn mark_item_succeeded(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        revision_no: i32,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_succeeded(tx, scope, item_id, revision_no, resource_version, now)
            .await
    }

    async fn mark_item_unchanged(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_unchanged(tx, scope, item_id, resource_version, now)
            .await
    }

    async fn mark_item_failed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        error_payload: String,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        self.inner
            .mark_item_failed(tx, scope, item_id, error_payload, now)
            .await
    }
}

#[async_trait]
impl DependencyStore for CasMissStores {
    async fn closure(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[String],
    ) -> Result<DependencyClosure, ScopeError> {
        self.inner.closure(tx, scope, roots).await
    }

    async fn reverse_impact(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[i64],
        write_set_bound: usize,
    ) -> Result<ReverseImpact, ScopeError> {
        self.inner
            .reverse_impact(tx, scope, roots, write_set_bound)
            .await
    }

    async fn replace_outgoing(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        from_entity_id: i64,
        edges: &[(DependencyKind, i64)],
    ) -> Result<(), ScopeError> {
        self.inner
            .replace_outgoing(tx, scope, from_entity_id, edges)
            .await
    }
}
