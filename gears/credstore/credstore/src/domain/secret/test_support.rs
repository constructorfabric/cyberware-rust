//! Shared test infrastructure for domain-layer unit tests (ADR-0006).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use credstore_sdk::{
    CredStoreError, CredStorePluginClientV1, OwnerId, SecretRef, SecretValue, SharingMode,
    TenantId, ValueId,
};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::{AccessScope, PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;
pub use crate::domain::ports::metrics::NoopMetrics;
use crate::domain::ports::metrics::{
    CredStoreMetricsPort, Dep, DepOp, FenceVerify, Outcome, ReadOutcome,
};
use crate::domain::ports::plugin::PluginSelector;
use crate::domain::resolver::TenantDirectory;
use crate::domain::secret::model::{GcEntry, GcReason, NewSecret, SecretRow, SecretStatus};
use crate::domain::secret::repo::SecretRepo;
use crate::domain::secret::type_resolver::{ResolvedSecretType, SecretTypeResolver};
use crate::domain::secret::typing::reasons;
use time::OffsetDateTime;

// ── SecurityContext helpers ───────────────────────────────────────────────────

/// Build a minimal [`SecurityContext`] for unit tests with custom subject and tenant.
///
/// # Panics
///
/// Panics if the builder fails (only possible on missing fields which we always supply).
#[must_use]
pub fn make_ctx(subject_id: Uuid, tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .build()
        .expect("test ctx")
}

// ── mock PolicyEnforcer ───────────────────────────────────────────────────────

/// Permissive PDP fake: emits one `Eq` permit on the caller's
/// `subject_tenant_id` (the slot the PEP populates) — the flat, pre-expanded
/// shape a capability-less PEP receives (`AUTHZ_USAGE_SCENARIOS` S09–S11).
/// The repo fakes ignore scope contents, so this only has to compile to a
/// valid scope under `require_constraints(true)`.
struct MockAuthZResolver;

#[async_trait]
impl AuthZResolverApi for MockAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let root = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .expect("MockAuthZResolver: subject.properties[\"tenant_id\"]; build ctx via make_ctx");
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        root,
                    ))],
                }],
                ..Default::default()
            },
        })
    }
}

/// Permissive [`PolicyEnforcer`] for `Service` unit tests.
#[must_use]
pub fn mock_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(MockAuthZResolver))
}

/// PDP fake that always denies (`decision: false`).
struct DenyAuthZResolver;

#[async_trait]
impl AuthZResolverApi for DenyAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// Denying [`PolicyEnforcer`] — drives `scope_for` → `DomainError::AccessDenied`.
#[must_use]
pub fn deny_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(DenyAuthZResolver))
}

/// Type-aware PDP fake: permissive (like [`mock_enforcer`]) except for the
/// listed resource-type ids, which are denied. Records every evaluated
/// resource type so tests can assert what the PEP targeted.
pub struct TypeAwareAuthZResolver {
    denied_types: Vec<String>,
    pub seen_resource_types: Mutex<Vec<String>>,
}

#[async_trait]
impl AuthZResolverApi for TypeAwareAuthZResolver {
    async fn evaluate(
        &self,
        ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.seen_resource_types
            .lock()
            .expect("lock")
            .push(request.resource.resource_type.clone());
        if self.denied_types.contains(&request.resource.resource_type) {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        }
        MockAuthZResolver.evaluate(ctx, request).await
    }
}

/// Enforcer denying exactly the given resource-type ids; returns the
/// resolver too so tests can inspect the evaluated types.
#[must_use]
pub fn type_deny_enforcer(
    denied_types: Vec<String>,
) -> (PolicyEnforcer, Arc<TypeAwareAuthZResolver>) {
    let resolver = Arc::new(TypeAwareAuthZResolver {
        denied_types,
        seen_resource_types: Mutex::new(Vec::new()),
    });
    (PolicyEnforcer::new(resolver.clone()), resolver)
}

/// Permissive PDP fake recording every `(action)` evaluated — for asserting
/// ADR-0004's body-derived action selection (`write`/`write_secret`).
pub struct RecordingAuthZResolver {
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl AuthZResolverApi for RecordingAuthZResolver {
    async fn evaluate(
        &self,
        ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.seen
            .lock()
            .expect("lock")
            .push(request.action.name.clone());
        MockAuthZResolver.evaluate(ctx, request).await
    }
}

impl RecordingAuthZResolver {
    /// Every action name evaluated so far, in call order.
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn seen_actions(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }
}

/// Permissive enforcer recording every evaluated action name; returns the
/// resolver so tests can assert which of `write`/`write_secret` a body
/// actually required.
#[must_use]
pub fn type_recording_enforcer() -> (PolicyEnforcer, Arc<RecordingAuthZResolver>) {
    let resolver = Arc::new(RecordingAuthZResolver {
        seen: Mutex::new(Vec::new()),
    });
    (PolicyEnforcer::new(resolver.clone()), resolver)
}

/// PDP fake denying exactly one `(resource_type, action)` pair; permissive
/// otherwise. Models a policy that grants everything except one action on
/// one type — e.g. `write` without `write_secret`.
pub struct ActionDenyAuthZResolver {
    resource_type: String,
    action: String,
}

#[async_trait]
impl AuthZResolverApi for ActionDenyAuthZResolver {
    async fn evaluate(
        &self,
        ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        if request.resource.resource_type == self.resource_type
            && request.action.name == self.action
        {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        }
        MockAuthZResolver.evaluate(ctx, request).await
    }
}

/// Enforcer denying exactly `action` on `resource_type`, permissive for
/// every other `(type, action)` pair.
#[must_use]
pub fn action_deny_enforcer(
    resource_type: String,
    action: &str,
) -> (PolicyEnforcer, Arc<ActionDenyAuthZResolver>) {
    let resolver = Arc::new(ActionDenyAuthZResolver {
        resource_type,
        action: action.to_owned(),
    });
    (PolicyEnforcer::new(resolver.clone()), resolver)
}

/// PDP fake that always returns a transport failure.
struct FailAuthZResolver;

#[async_trait]
impl AuthZResolverApi for FailAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::service_unavailable()
            .with_detail("failing_enforcer: simulated PDP transport failure".to_owned())
            .create())
    }
}

/// Failing [`PolicyEnforcer`] — drives `scope_for` → `DomainError::ServiceUnavailable`.
#[must_use]
pub fn failing_enforcer() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(FailAuthZResolver))
}

// ── Fake type resolvers ───────────────────────────────────────────────────────

/// Catalog-backed [`SecretTypeResolver`]: resolves the built-in catalog
/// types by their deterministic UUID, mirroring what the production
/// resolver returns for the registry-seeded schemas. Unknown UUIDs map to
/// `UNKNOWN_SECRET_TYPE`, like an unregistered type.
pub struct CatalogTypeResolver;

#[async_trait]
impl SecretTypeResolver for CatalogTypeResolver {
    async fn resolve(&self, type_uuid: Uuid) -> Result<ResolvedSecretType, DomainError> {
        credstore_sdk::SECRET_TYPE_CATALOG
            .iter()
            .find(|d| credstore_sdk::types::type_uuid(d.gts_id) == Some(type_uuid))
            .map(|d| ResolvedSecretType {
                gts_id: d.gts_id.to_owned(),
                traits: d.traits(),
            })
            .ok_or_else(|| DomainError::TypeViolation {
                field: "type",
                reason: reasons::UNKNOWN_SECRET_TYPE,
                detail: format!("secret type {type_uuid} is not registered"),
            })
    }
}

/// Catalog-backed resolver as an `Arc<dyn SecretTypeResolver>` — the
/// default for `Service` unit tests.
#[must_use]
pub fn catalog_type_resolver() -> Arc<dyn SecretTypeResolver> {
    Arc::new(CatalogTypeResolver)
}

/// [`SecretTypeResolver`] that always fails with `ServiceUnavailable` —
/// drives the registry-outage (503) paths.
pub struct FailingTypeResolver;

#[async_trait]
impl SecretTypeResolver for FailingTypeResolver {
    async fn resolve(&self, _type_uuid: Uuid) -> Result<ResolvedSecretType, DomainError> {
        Err(DomainError::ServiceUnavailable {
            detail: "types-registry unavailable".to_owned(),
            retry_after: None,
            cause: None,
        })
    }
}

// ── FakeDir ───────────────────────────────────────────────────────────────────

/// Returns a preset ancestor chain (self first, root last).
pub struct FakeDir {
    chain: Vec<Uuid>,
}

impl FakeDir {
    #[must_use]
    pub fn new(chain: Vec<Uuid>) -> Self {
        Self { chain }
    }

    /// Single-tenant chain (only self).
    #[must_use]
    pub fn single(id: Uuid) -> Self {
        Self { chain: vec![id] }
    }
}

#[async_trait]
impl TenantDirectory for FakeDir {
    async fn ancestor_chain(
        &self,
        _ctx: &SecurityContext,
        _req: TenantId,
    ) -> Result<Vec<Uuid>, DomainError> {
        Ok(self.chain.clone())
    }
}

// ── FakePlugin ────────────────────────────────────────────────────────────────

/// Key: `(tenant_id, value_id)` (ADR-0006).
type PluginKey = (Uuid, Uuid);

/// In-memory plugin store keyed by `(tenant_id, value_id)`, with the same
/// immutability guard the static plugin enforces (`put` on an existing key
/// is `Conflict`) and fault-injection hooks for the write-protocol tests.
pub struct FakePlugin {
    store: Mutex<HashMap<PluginKey, Vec<u8>>>,
    /// Number of upcoming `put` calls that fail before the plugin recovers,
    /// simulating a transient backend outage mid-write.
    put_failures: Mutex<usize>,
    /// Number of upcoming `delete` calls that fail before the plugin
    /// recovers, simulating a transient backend outage mid-cleanup.
    delete_failures: Mutex<usize>,
    /// Number of upcoming `get` calls that report `Ok(None)` regardless of
    /// the store's actual contents — models a read racing a concurrent
    /// pointer switch (ADR-0006's retry-once case).
    not_found_gets: Mutex<usize>,
    /// When set, every `get` returns [`CredStoreError::AccessDenied`],
    /// modelling a backend whose own ACLs reject a read the gear's PDP has
    /// already allowed.
    get_denied: bool,
    /// Count of backend reads of the reserved fence-key entry — lets tests
    /// assert the mismatch-triggered refresh does not re-read the key on every
    /// poisoned get (the cache-thrash guard).
    fence_key_gets: AtomicUsize,
}

impl FakePlugin {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(HashMap::new()),
            put_failures: Mutex::new(0),
            delete_failures: Mutex::new(0),
            not_found_gets: Mutex::new(0),
            get_denied: false,
            fence_key_gets: AtomicUsize::new(0),
        })
    }

    /// Plugin that fails the next `n` `put` calls, then behaves normally.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn with_put_failures(n: usize) -> Arc<Self> {
        let p = Self::new();
        *p.put_failures.lock().expect("lock") = n;
        p
    }

    /// Plugin that fails the next `n` `delete` calls, then behaves normally.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn with_delete_failures(n: usize) -> Arc<Self> {
        let p = Self::new();
        *p.delete_failures.lock().expect("lock") = n;
        p
    }

    /// Plugin whose every `get` denies — models a backend ACL rejecting a read
    /// the gear's PDP already allowed.
    #[must_use]
    pub fn with_get_denied() -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(HashMap::new()),
            put_failures: Mutex::new(0),
            delete_failures: Mutex::new(0),
            not_found_gets: Mutex::new(0),
            get_denied: true,
            fence_key_gets: AtomicUsize::new(0),
        })
    }

    /// Arrange for the next `n` `get` calls to report `Ok(None)` regardless
    /// of what is actually stored.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn fail_next_gets_with_not_found(&self, n: usize) {
        *self.not_found_gets.lock().expect("lock") += n;
    }

    /// Arrange for the next `n` `delete` calls on this (already-populated)
    /// instance to fail — for tests that need to inject a cleanup failure
    /// after values have already been written through the normal API.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn fail_next_deletes(&self, n: usize) {
        *self.delete_failures.lock().expect("lock") += n;
    }

    /// Arrange for the next `n` `put` calls on this (already-populated)
    /// instance to fail — for tests that need the *value* write (not the
    /// fence-key bootstrap put, which typically runs first on a fresh
    /// instance) to fail. Bootstrap the fence key with an unrelated write
    /// first, then call this before the write under test.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn fail_next_puts(&self, n: usize) {
        *self.put_failures.lock().expect("lock") += n;
    }

    /// Number of backend reads of the reserved fence-key entry so far.
    ///
    /// # Panics
    ///
    /// Never panics.
    #[must_use]
    pub fn fence_key_gets(&self) -> usize {
        self.fence_key_gets.load(Ordering::Relaxed)
    }

    /// True when the store holds a value for `(tenant, value_id)`.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn contains(&self, tenant_id: &TenantId, value_id: ValueId) -> bool {
        self.store
            .lock()
            .expect("lock")
            .contains_key(&(tenant_id.0, value_id.0))
    }

    fn key(tenant_id: &TenantId, value_id: &ValueId) -> PluginKey {
        (tenant_id.0, value_id.0)
    }
}

impl Default for FakePlugin {
    fn default() -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
            put_failures: Mutex::new(0),
            delete_failures: Mutex::new(0),
            not_found_gets: Mutex::new(0),
            get_denied: false,
            fence_key_gets: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CredStorePluginClientV1 for FakePlugin {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
    ) -> Result<Option<SecretValue>, CredStoreError> {
        if *value_id == credstore_sdk::FENCE_KEY_VALUE_ID {
            self.fence_key_gets.fetch_add(1, Ordering::Relaxed);
        }
        if self.get_denied {
            return Err(CredStoreError::AccessDenied);
        }
        {
            let mut remaining = self.not_found_gets.lock().expect("lock");
            if *remaining > 0 {
                *remaining -= 1;
                return Ok(None);
            }
        }
        let k = Self::key(tenant_id, value_id);
        let guard = self.store.lock().expect("lock");
        Ok(guard.get(&k).map(|v| SecretValue::new(v.clone())))
    }

    async fn put(
        &self,
        _ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
        value: SecretValue,
    ) -> Result<(), CredStoreError> {
        {
            let mut remaining = self.put_failures.lock().expect("lock");
            if *remaining > 0 {
                *remaining -= 1;
                return Err(CredStoreError::Internal(
                    "simulated backend put failure".to_owned(),
                ));
            }
        }
        let k = Self::key(tenant_id, value_id);
        let mut store = self.store.lock().expect("lock");
        // Immutability guard, matching the static plugin: a put to an id
        // already present is a contract violation the gear never issues.
        if store.contains_key(&k) {
            return Err(CredStoreError::Conflict);
        }
        store.insert(k, value.as_bytes().to_vec());
        Ok(())
    }

    async fn delete(
        &self,
        _ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
    ) -> Result<(), CredStoreError> {
        {
            let mut remaining = self.delete_failures.lock().expect("lock");
            if *remaining > 0 {
                *remaining -= 1;
                return Err(CredStoreError::Internal(
                    "simulated backend delete failure".to_owned(),
                ));
            }
        }
        let k = Self::key(tenant_id, value_id);
        self.store.lock().expect("lock").remove(&k);
        Ok(())
    }
}

// ── FakePluginSelector ────────────────────────────────────────────────────────

pub struct FakePluginSelector {
    plugin: Arc<FakePlugin>,
}

impl FakePluginSelector {
    #[must_use]
    pub fn new(plugin: Arc<FakePlugin>) -> Self {
        Self { plugin }
    }
}

#[async_trait]
impl PluginSelector for FakePluginSelector {
    async fn resolve(&self) -> Result<Arc<dyn CredStorePluginClientV1>, DomainError> {
        Ok(self.plugin.clone())
    }
}

/// [`PluginSelector`] that always fails to resolve a plugin — models the
/// `NoPluginAvailable` (misconfigured / unregistered backend) 503 path.
pub struct NoPluginSelector;

#[async_trait]
impl PluginSelector for NoPluginSelector {
    async fn resolve(&self) -> Result<Arc<dyn CredStorePluginClientV1>, DomainError> {
        Err(DomainError::ServiceUnavailable {
            detail: "no storage plugin registered".to_owned(),
            retry_after: None,
            cause: None,
        })
    }
}

// ── FakeSecretRepo ────────────────────────────────────────────────────────────

/// `(row_id, new_value_id, new_value_fp)` — see `pending_switch`'s field docs.
type PendingSwitch = (Uuid, ValueId, Vec<u8>);

/// In-memory [`SecretRepo`] replicating the real (transactional) semantics of
/// each method, for domain-service unit tests.
///
/// `scope_allows` controls the result of [`SecretRepo::scope_includes_tenant`].
pub struct FakeSecretRepo {
    rows: Mutex<Vec<SecretRow>>,
    gc: Mutex<Vec<GcEntry>>,
    pub scope_allows: bool,
    /// When `> 0`, the next `insert_active` call fails with a simulated
    /// internal error (before touching rows/gc) and decrements; consumed
    /// once per call.
    insert_active_failures: Mutex<usize>,
    /// When `> 0`, the next `switch_value` call fails with a simulated
    /// internal error (before touching rows/gc) rather than returning
    /// `Ok(None)`/`Ok(Some(_))` — models a step-4 DB-unreachable failure,
    /// distinct from an ordinary lost CAS.
    switch_value_failures: Mutex<usize>,
    /// When `> 0`, the next `switch_value` call returns `Ok(None)` (a lost
    /// CAS) without touching rows/gc, regardless of the actual id/version —
    /// models "another writer's CAS committed first", which a purely
    /// sequential test cannot otherwise reproduce.
    force_switch_value_none: Mutex<usize>,
    /// One-shot hook for the read-races-a-switch scenario: after the *next*
    /// `resolve_for_get` call whose result matches `row_id` returns (with the
    /// pre-switch snapshot), atomically flips the stored row to
    /// `new_value_id`/`new_fp` (bumping its version) — so a second,
    /// subsequently-issued `resolve_for_get` observes the post-switch row,
    /// exactly like a concurrent writer's pointer switch landing between two
    /// reads, without needing real concurrency.
    pending_switch: Mutex<Option<PendingSwitch>>,
    /// When set, `delete_by_id` returns an error — simulating a DB failure.
    fail_delete: bool,
    /// When `> 0`, the next `delete_by_id` call reports `NotFound`
    /// regardless of actual row state — models a row vanishing concurrently
    /// between the caller's precheck and this call.
    force_delete_by_id_not_found: Mutex<usize>,
}

impl FakeSecretRepo {
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
            gc: Mutex::new(Vec::new()),
            scope_allows: true,
            insert_active_failures: Mutex::new(0),
            switch_value_failures: Mutex::new(0),
            force_switch_value_none: Mutex::new(0),
            pending_switch: Mutex::new(None),
            fail_delete: false,
            force_delete_by_id_not_found: Mutex::new(0),
        }
    }

    #[must_use]
    pub fn with_scope_allows(scope_allows: bool) -> Self {
        Self {
            scope_allows,
            ..Self::new()
        }
    }

    /// Repo whose `delete_by_id` always fails — exercises the delete-failure
    /// path.
    #[must_use]
    pub fn with_delete_failure() -> Self {
        Self {
            fail_delete: true,
            ..Self::new()
        }
    }

    /// Arrange for the next `n` `insert_active` calls to fail with a
    /// simulated internal (DB-unreachable-like) error.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn fail_next_insert_active(&self, n: usize) {
        *self.insert_active_failures.lock().expect("lock") += n;
    }

    /// Arrange for the next `n` `switch_value` calls to fail with a
    /// simulated internal (DB-unreachable-like) error, instead of running
    /// the ordinary CAS.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn fail_next_switch_value(&self, n: usize) {
        *self.switch_value_failures.lock().expect("lock") += n;
    }

    /// Arrange for the next `n` `switch_value` calls to report a lost CAS
    /// (`Ok(None)`) without touching rows/gc — models a concurrent writer
    /// having already moved the row by the time this call's CAS ran.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn force_next_switch_value_none(&self, n: usize) {
        *self.force_switch_value_none.lock().expect("lock") += n;
    }

    /// One-shot: once a `resolve_for_get` call resolves to `row_id`, flip
    /// that stored row to `new_value_id`/`new_fp` (version bumped) right
    /// after computing *that* call's (pre-switch) result — so the next
    /// `resolve_for_get` call sees the post-switch row. See the field docs
    /// on `pending_switch`.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn switch_after_next_resolve(&self, row_id: Uuid, new_value_id: ValueId, new_fp: Vec<u8>) {
        *self.pending_switch.lock().expect("lock") = Some((row_id, new_value_id, new_fp));
    }

    /// Arrange for the next `n` `delete_by_id` calls to report `NotFound`
    /// regardless of actual row state — models a row vanishing concurrently
    /// between the caller's precheck and the delete transaction.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn force_next_delete_by_id_not_found(&self, n: usize) {
        *self.force_delete_by_id_not_found.lock().expect("lock") += n;
    }

    /// Force `row_id`'s `expires_at` into the past, for maintenance-job
    /// (`run_gc`) tests that need an already-expired row without waiting.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned or no row matches `row_id`.
    pub fn force_expire(&self, row_id: Uuid) {
        let mut rows = self.rows.lock().expect("lock");
        let row = rows
            .iter_mut()
            .find(|r| r.id == row_id)
            .expect("row_id must exist");
        row.expires_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(5));
    }

    /// Seed rows directly (for pre-seeding parent/inherited state).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn seed(&self, row: SecretRow) {
        self.rows.lock().expect("lock").push(row);
    }

    /// Snapshot of all rows (for asserting write-protocol state).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn rows(&self) -> Vec<SecretRow> {
        self.rows.lock().expect("lock").clone()
    }

    /// Snapshot of all gc entries (for asserting garbage-collection
    /// bookkeeping).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn gc_entries(&self) -> Vec<GcEntry> {
        self.gc.lock().expect("lock").clone()
    }

    /// Resolution-eligible (ADR-0004, Suppression): `active` and not expired,
    /// or `declared` with `fallback: none` (a suppressing row that competes
    /// and blocks). Mirrors the production predicate in
    /// `infra::storage::repo_impl::reads::resolution_eligible_condition`.
    fn resolution_eligible(r: &SecretRow) -> bool {
        match r.status {
            SecretStatus::Active => r.expires_at.is_none_or(|at| at > OffsetDateTime::now_utc()),
            SecretStatus::Declared => r.fallback == crate::domain::secret::model::Fallback::None,
        }
    }

    /// Collection-read visibility predicate (ADR-0005): own tenant, every
    /// sharing-visible row of any status; ancestor tenants, resolution-
    /// eligible `shared` rows only. Mirrors
    /// `resolve_candidates`'s per-row predicate above (minus the reference
    /// match) and
    /// `infra::storage::repo_impl::reads::chain_visibility_condition`.
    fn is_candidate_visible(
        r: &SecretRow,
        req_tenant: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
    ) -> bool {
        chain.contains(&r.tenant_id.0)
            && if r.tenant_id == req_tenant {
                match r.sharing {
                    SharingMode::Private => r.owner_id.0 == subject.0,
                    SharingMode::Tenant | SharingMode::Shared => true,
                }
            } else {
                r.sharing == SharingMode::Shared && Self::resolution_eligible(r)
            }
    }
}

impl Default for FakeSecretRepo {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretRepo for FakeSecretRepo {
    async fn resolve_for_get(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        chain: &[Uuid],
    ) -> Result<Option<SecretRow>, DomainError> {
        let rows = self.rows.lock().expect("lock");
        let key_str = key.as_ref();

        let pos = |t: Uuid| chain.iter().position(|c| *c == t).unwrap_or(usize::MAX);
        let best = rows
            .iter()
            .filter(|r| {
                Self::resolution_eligible(r)
                    && r.reference == key_str
                    && chain.contains(&r.tenant_id.0)
                    && match r.sharing {
                        SharingMode::Private => r.owner_id.0 == subject.0,
                        SharingMode::Tenant => r.tenant_id == req_tenant,
                        SharingMode::Shared => true,
                    }
            })
            .min_by(|a, b| {
                pos(a.tenant_id.0).cmp(&pos(b.tenant_id.0)).then(
                    (a.sharing != SharingMode::Private).cmp(&(b.sharing != SharingMode::Private)),
                )
            });
        let result = best.cloned();
        drop(rows);

        // Apply a one-shot pending switch (read-races-a-switch test support):
        // this call's result stays the pre-switch snapshot, but the stored
        // row is mutated now, so the *next* resolve_for_get sees the switch.
        if let Some(r) = &result {
            let mut pending = self.pending_switch.lock().expect("lock");
            if pending.as_ref().is_some_and(|(row_id, ..)| *row_id == r.id) {
                let (_, new_value_id, new_fp) = pending.take().expect("checked Some above");
                drop(pending);
                let mut rows = self.rows.lock().expect("lock");
                if let Some(stored) = rows.iter_mut().find(|x| x.id == r.id) {
                    stored.value_id = Some(new_value_id);
                    stored.value_fp = Some(new_fp);
                    stored.version += 1;
                }
            }
        }
        Ok(result)
    }

    async fn resolve_candidates(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        chain: &[Uuid],
    ) -> Result<Vec<SecretRow>, DomainError> {
        let rows = self.rows.lock().expect("lock");
        let key_str = key.as_ref();
        let candidates = rows
            .iter()
            .filter(|r| {
                r.reference == key_str
                    && chain.contains(&r.tenant_id.0)
                    && if r.tenant_id == req_tenant {
                        match r.sharing {
                            SharingMode::Private => r.owner_id.0 == subject.0,
                            SharingMode::Tenant | SharingMode::Shared => true,
                        }
                    } else {
                        r.sharing == SharingMode::Shared && Self::resolution_eligible(r)
                    }
            })
            .cloned()
            .collect();
        Ok(candidates)
    }

    async fn find_own(
        &self,
        _scope: &AccessScope,
        tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
    ) -> Result<Option<SecretRow>, DomainError> {
        let rows = self.rows.lock().expect("lock");
        let key_str = key.as_ref();
        let best = rows
            .iter()
            .filter(|r| {
                r.tenant_id == tenant
                    && r.reference == key_str
                    && matches!(r.status, SecretStatus::Active | SecretStatus::Declared)
                    && match r.sharing {
                        SharingMode::Private => r.owner_id.0 == subject.0,
                        _ => true,
                    }
            })
            .min_by_key(|r| i32::from(r.sharing != SharingMode::Private));
        Ok(best.cloned())
    }

    async fn find_for_write(
        &self,
        _scope: &AccessScope,
        tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        sharing: SharingMode,
    ) -> Result<Option<SecretRow>, DomainError> {
        let rows = self.rows.lock().expect("lock");
        let key_str = key.as_ref();
        let row = rows.iter().find(|r| {
            r.tenant_id == tenant
                && r.reference == key_str
                && matches!(r.status, SecretStatus::Active | SecretStatus::Declared)
                && match sharing {
                    SharingMode::Private => {
                        r.sharing == SharingMode::Private && r.owner_id == subject
                    }
                    _ => r.sharing != SharingMode::Private,
                }
        });
        Ok(row.cloned())
    }

    async fn scope_includes_tenant(
        &self,
        _scope: &AccessScope,
        _tenant: Uuid,
    ) -> Result<bool, DomainError> {
        Ok(self.scope_allows)
    }

    async fn list_candidate_references(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
        reference_in: Option<&[String]>,
        type_uuid_in: Option<&[Uuid]>,
        cursor: Option<&str>,
        desc: bool,
        limit: u64,
    ) -> Result<Vec<String>, DomainError> {
        let rows = self.rows.lock().expect("lock");
        let mut refs: Vec<String> = rows
            .iter()
            .filter(|r| Self::is_candidate_visible(r, req_tenant, subject, chain))
            .filter(|r| reference_in.is_none_or(|refs| refs.contains(&r.reference)))
            .filter(|r| type_uuid_in.is_none_or(|types| types.contains(&r.secret_type_uuid)))
            .map(|r| r.reference.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if desc {
            refs.reverse();
        }
        if let Some(after) = cursor {
            refs.retain(|r| {
                if desc {
                    r.as_str() < after
                } else {
                    r.as_str() > after
                }
            });
        }
        refs.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(refs)
    }

    async fn list_candidate_types(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
        references: &[String],
        type_uuid_in: Option<&[Uuid]>,
    ) -> Result<Vec<Uuid>, DomainError> {
        let rows = self.rows.lock().expect("lock");
        let types: std::collections::BTreeSet<Uuid> = rows
            .iter()
            .filter(|r| references.contains(&r.reference))
            .filter(|r| Self::is_candidate_visible(r, req_tenant, subject, chain))
            .filter(|r| type_uuid_in.is_none_or(|types| types.contains(&r.secret_type_uuid)))
            .map(|r| r.secret_type_uuid)
            .collect();
        Ok(types.into_iter().collect())
    }

    async fn list_candidates_for_references(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
        references: &[String],
    ) -> Result<Vec<SecretRow>, DomainError> {
        let rows = self.rows.lock().expect("lock");
        Ok(rows
            .iter()
            .filter(|r| references.contains(&r.reference))
            .filter(|r| Self::is_candidate_visible(r, req_tenant, subject, chain))
            .cloned()
            .collect())
    }

    async fn gc_insert_pending(
        &self,
        value_id: ValueId,
        tenant_id: TenantId,
    ) -> Result<(), DomainError> {
        self.gc.lock().expect("lock").push(GcEntry {
            value_id,
            tenant_id,
            reason: GcReason::Pending,
            enqueued_at: OffsetDateTime::now_utc(),
        });
        Ok(())
    }

    async fn gc_delete(&self, value_id: ValueId) -> Result<bool, DomainError> {
        let mut gc = self.gc.lock().expect("lock");
        let before = gc.len();
        gc.retain(|e| e.value_id != value_id);
        Ok(gc.len() != before)
    }

    async fn gc_mark(&self, value_id: ValueId, reason: GcReason) -> Result<bool, DomainError> {
        let mut gc = self.gc.lock().expect("lock");
        match gc.iter_mut().find(|e| e.value_id == value_id) {
            Some(e) => {
                e.reason = reason;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn gc_list(&self, limit: u64) -> Result<Vec<GcEntry>, DomainError> {
        let mut gc = self.gc.lock().expect("lock").clone();
        gc.sort_by_key(|e| e.enqueued_at);
        gc.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(gc)
    }

    async fn is_value_referenced(&self, value_id: ValueId) -> Result<bool, DomainError> {
        Ok(self
            .rows
            .lock()
            .expect("lock")
            .iter()
            .any(|r| r.value_id == Some(value_id)))
    }

    async fn insert_active(
        &self,
        _scope: &AccessScope,
        new: &NewSecret,
    ) -> Result<(), DomainError> {
        {
            let mut remaining = self.insert_active_failures.lock().expect("lock");
            if *remaining > 0 {
                *remaining -= 1;
                return Err(DomainError::internal("simulated insert_active failure"));
            }
        }
        let mut rows = self.rows.lock().expect("lock");
        let conflict = rows.iter().any(|r| {
            r.tenant_id == new.tenant_id
                && r.reference == new.reference.as_ref()
                && match new.sharing {
                    SharingMode::Private => {
                        r.sharing == SharingMode::Private && r.owner_id == new.owner_id
                    }
                    _ => r.sharing != SharingMode::Private,
                }
        });
        if conflict {
            return Err(DomainError::Conflict);
        }
        rows.push(SecretRow {
            id: new.id,
            tenant_id: new.tenant_id,
            reference: new.reference.as_ref().to_owned(),
            sharing: new.sharing,
            owner_id: new.owner_id,
            status: SecretStatus::Active,
            version: 1,
            updated_at: OffsetDateTime::now_utc(),
            secret_type_uuid: new.secret_type_uuid,
            expires_at: new.expires_at,
            value_id: Some(new.value_id),
            value_fp: Some(new.value_fp.clone()),
            fp_key_id: Some(new.fp_key_id),
            fallback: new.fallback,
        });
        drop(rows);
        self.gc
            .lock()
            .expect("lock")
            .retain(|e| e.value_id != new.value_id);
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors SecretRepo::switch_value's one-CAS-with-every-field-it-may-update shape"
    )]
    async fn switch_value(
        &self,
        _scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
        sharing: SharingMode,
        fallback: crate::domain::secret::model::Fallback,
        expires_at: Option<OffsetDateTime>,
        new_value_id: ValueId,
        value_fp: Vec<u8>,
        fp_key_id: i16,
    ) -> Result<Option<(SecretRow, Option<ValueId>)>, DomainError> {
        {
            let mut remaining = self.switch_value_failures.lock().expect("lock");
            if *remaining > 0 {
                *remaining -= 1;
                return Err(DomainError::internal("simulated switch_value failure"));
            }
        }
        {
            let mut remaining = self.force_switch_value_none.lock().expect("lock");
            if *remaining > 0 {
                *remaining -= 1;
                return Ok(None);
            }
        }
        let mut rows = self.rows.lock().expect("lock");
        let row = rows.iter_mut().find(|r| {
            r.id == id
                && matches!(r.status, SecretStatus::Active | SecretStatus::Declared)
                && expected_version.is_none_or(|v| r.version == v)
        });
        let Some(row) = row else {
            return Ok(None);
        };
        let old_value_id = row.value_id;
        row.value_id = Some(new_value_id);
        row.value_fp = Some(value_fp);
        row.fp_key_id = Some(fp_key_id);
        row.sharing = sharing;
        row.fallback = fallback;
        row.expires_at = expires_at;
        row.status = SecretStatus::Active;
        row.version += 1;
        row.updated_at = OffsetDateTime::now_utc();
        let updated = row.clone();
        drop(rows);

        self.gc
            .lock()
            .expect("lock")
            .retain(|e| e.value_id != new_value_id);
        if let Some(old_id) = old_value_id {
            self.gc.lock().expect("lock").push(GcEntry {
                value_id: old_id,
                tenant_id: updated.tenant_id,
                reason: GcReason::Superseded,
                enqueued_at: OffsetDateTime::now_utc(),
            });
        }
        Ok(Some((updated, old_value_id)))
    }

    async fn update_metadata(
        &self,
        _scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
        sharing: SharingMode,
        fallback: crate::domain::secret::model::Fallback,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<Option<SecretRow>, DomainError> {
        let mut rows = self.rows.lock().expect("lock");
        let row = rows
            .iter_mut()
            .find(|r| r.id == id && expected_version.is_none_or(|v| r.version == v));
        let Some(row) = row else {
            return Ok(None);
        };
        row.sharing = sharing;
        row.fallback = fallback;
        row.expires_at = expires_at;
        row.version += 1;
        row.updated_at = OffsetDateTime::now_utc();
        Ok(Some(row.clone()))
    }

    async fn remove_value(
        &self,
        _scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
        sharing: SharingMode,
        fallback: crate::domain::secret::model::Fallback,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<Option<(SecretRow, Option<ValueId>)>, DomainError> {
        let mut rows = self.rows.lock().expect("lock");
        let row = rows
            .iter_mut()
            .find(|r| r.id == id && expected_version.is_none_or(|v| r.version == v));
        let Some(row) = row else {
            return Ok(None);
        };
        let old_value_id = row.value_id.take();
        row.value_fp = None;
        row.fp_key_id = None;
        row.status = SecretStatus::Declared;
        row.sharing = sharing;
        row.fallback = fallback;
        row.expires_at = expires_at;
        row.version += 1;
        row.updated_at = OffsetDateTime::now_utc();
        let updated = row.clone();
        drop(rows);

        if let Some(old_id) = old_value_id {
            self.gc.lock().expect("lock").push(GcEntry {
                value_id: old_id,
                tenant_id: updated.tenant_id,
                reason: GcReason::Removed,
                enqueued_at: OffsetDateTime::now_utc(),
            });
        }
        Ok(Some((updated, old_value_id)))
    }

    async fn delete_by_id(
        &self,
        _scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
    ) -> Result<Option<ValueId>, DomainError> {
        if self.fail_delete {
            return Err(DomainError::internal("simulated delete failure"));
        }
        {
            let mut remaining = self.force_delete_by_id_not_found.lock().expect("lock");
            if *remaining > 0 {
                *remaining -= 1;
                return Err(DomainError::NotFound);
            }
        }
        let mut rows = self.rows.lock().expect("lock");
        let idx = rows
            .iter()
            .position(|r| r.id == id && expected_version.is_none_or(|v| r.version == v));
        let Some(idx) = idx else {
            return Err(DomainError::NotFound);
        };
        let removed = rows.remove(idx);
        drop(rows);
        if let Some(value_id) = removed.value_id {
            self.gc.lock().expect("lock").push(GcEntry {
                value_id,
                tenant_id: removed.tenant_id,
                reason: GcReason::Removed,
                enqueued_at: OffsetDateTime::now_utc(),
            });
        }
        Ok(removed.value_id)
    }

    async fn list_expired(&self, limit: u64) -> Result<Vec<SecretRow>, DomainError> {
        let now = OffsetDateTime::now_utc();
        let rows = self.rows.lock().expect("lock");
        Ok(rows
            .iter()
            .filter(|r| {
                r.status == SecretStatus::Active && r.expires_at.is_some_and(|at| at <= now)
            })
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .cloned()
            .collect())
    }

    async fn delete_expired_row(&self, id: Uuid) -> Result<Option<ValueId>, DomainError> {
        let mut rows = self.rows.lock().expect("lock");
        let idx = rows.iter().position(|r| r.id == id);
        let Some(idx) = idx else {
            return Ok(None);
        };
        let removed = rows.remove(idx);
        drop(rows);
        if let Some(value_id) = removed.value_id {
            self.gc.lock().expect("lock").push(GcEntry {
                value_id,
                tenant_id: removed.tenant_id,
                reason: GcReason::Removed,
                enqueued_at: OffsetDateTime::now_utc(),
            });
        }
        Ok(removed.value_id)
    }
}

// ── FakeMetrics ───────────────────────────────────────────────────────────────

/// Recording metrics fake for assertions in tests.
pub struct FakeMetrics {
    pub cross_tenant_denied_count: Mutex<u64>,
    pub read_outcomes: Mutex<Vec<ReadOutcome>>,
    pub deps: Mutex<Vec<(Dep, DepOp, Outcome)>>,
    pub fence_verifies: Mutex<Vec<FenceVerify>>,
    pub gc_deleted_total: Mutex<u64>,
    pub gc_pending_reclaimed_total: Mutex<u64>,
    pub expired_deleted_total: Mutex<u64>,
    pub list_type_invariant_violation_total: Mutex<u64>,
}

impl FakeMetrics {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns all recorded dependency `(dep, op, outcome)` tuples.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn deps(&self) -> Vec<(Dep, DepOp, Outcome)> {
        self.deps.lock().expect("lock").clone()
    }

    /// Returns the number of times [`CredStoreMetricsPort::cross_tenant_denied`] was called.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (only possible after a panic in another thread).
    pub fn cross_tenant_denied_count(&self) -> u64 {
        *self.cross_tenant_denied_count.lock().expect("lock")
    }

    /// Returns the last recorded [`ReadOutcome`], if any.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn last_read_outcome(&self) -> Option<ReadOutcome> {
        self.read_outcomes.lock().expect("lock").last().copied()
    }

    /// Returns all recorded fence-verify verdicts.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn fence_verifies(&self) -> Vec<FenceVerify> {
        self.fence_verifies.lock().expect("lock").clone()
    }

    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn gc_deleted_total(&self) -> u64 {
        *self.gc_deleted_total.lock().expect("lock")
    }

    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn gc_pending_reclaimed_total(&self) -> u64 {
        *self.gc_pending_reclaimed_total.lock().expect("lock")
    }

    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn expired_deleted_total(&self) -> u64 {
        *self.expired_deleted_total.lock().expect("lock")
    }

    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn list_type_invariant_violation_total(&self) -> u64 {
        *self
            .list_type_invariant_violation_total
            .lock()
            .expect("lock")
    }
}

impl Default for FakeMetrics {
    fn default() -> Self {
        Self {
            cross_tenant_denied_count: Mutex::new(0),
            read_outcomes: Mutex::new(Vec::new()),
            deps: Mutex::new(Vec::new()),
            fence_verifies: Mutex::new(Vec::new()),
            gc_deleted_total: Mutex::new(0),
            gc_pending_reclaimed_total: Mutex::new(0),
            expired_deleted_total: Mutex::new(0),
            list_type_invariant_violation_total: Mutex::new(0),
        }
    }
}

impl CredStoreMetricsPort for FakeMetrics {
    fn read_outcome(&self, outcome: ReadOutcome) {
        self.read_outcomes.lock().expect("lock").push(outcome);
    }
    fn walkup_depth(&self, _depth: u64) {}
    fn dependency(&self, dep: Dep, op: DepOp, outcome: Outcome, _secs: f64) {
        self.deps.lock().expect("lock").push((dep, op, outcome));
    }
    fn cross_tenant_denied(&self) {
        *self.cross_tenant_denied_count.lock().expect("lock") += 1;
    }
    fn fence_verify(&self, outcome: FenceVerify) {
        self.fence_verifies.lock().expect("lock").push(outcome);
    }
    fn gc_deleted(&self, n: u64) {
        *self.gc_deleted_total.lock().expect("lock") += n;
    }
    fn gc_pending_reclaimed(&self, n: u64) {
        *self.gc_pending_reclaimed_total.lock().expect("lock") += n;
    }
    fn expired_deleted(&self, n: u64) {
        *self.expired_deleted_total.lock().expect("lock") += n;
    }
    fn list_type_invariant_violation(&self) {
        *self
            .list_type_invariant_violation_total
            .lock()
            .expect("lock") += 1;
    }
}
