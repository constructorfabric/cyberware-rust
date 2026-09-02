//! Test doubles for the plugin contracts. Enabled with the `test-util` feature.
//!
//! [`InMemoryStorage`] implements every method of
//! [`QuotaEnforcementStoragePluginV1`] with simple in-memory semantics, so the
//! gear's bootstrap and readiness paths run against a complete contract
//! (foundation `DoD`, "Workspace and Crate Skeletons"). [`InMemoryCoordination`]
//! implements [`CoordinationPluginV1`] with deterministic expiry control, so
//! tests never wait on a wall clock.
//!
//! Both doubles accept an injected failure through `fail_with`, which makes
//! every later call return that error until `clear_failure`.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    reason = "test support: fixtures are built from constant, well-formed inputs"
)]

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use gts::GtsTypeId;
use parking_lot::Mutex;
use serde_json::Value;
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::coordination_plugin::{CoordinationError, CoordinationPluginV1, Lock, LockScope};
use crate::models::{
    ApplicableQuotas, BatchDebitItem, BootstrapBundle, CapPatch, ConfigDefaults, ContractRef,
    DeactivateOutcome, DebitPlan, EnforcementMode, EventId, ExpiredLease, IdempotencyRecord,
    IdempotencyScope, IdempotencyWrite, LeaseHold, LeaseState, LeaseToken, MetricId,
    MutationResult, NotificationEvent, PageRequest, PageResult, PolicyDraft, PolicyId, PolicyScope,
    PolicyUpdate, PolicyVersion, PolicyVersionMeta, PolicyVersionState, Quota, QuotaDraft,
    QuotaFilter, QuotaId, QuotaPatch, QuotaSnapshot, QuotaSource, QuotaStatus, QuotaType,
    SubjectRef, TenantId, ValidityWindowPatch,
};
use crate::storage_plugin::{CONTRACT_MAJOR, QuotaEnforcementStoragePluginV1, StorageError};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A fixed tenant for tests.
#[must_use]
pub fn test_tenant() -> TenantId {
    TenantId::new(Uuid::from_u128(0x7e57_0000_0000_0000_0000_0000_0000_0001))
}

/// A registered-looking metric instance id.
#[must_use]
pub fn test_metric() -> MetricId {
    MetricId::parse("gts.cf.qe.metric.type.v1~cf.genai.llm_gateway.token.v1")
        .expect("well-formed metric id")
}

/// A user-scope subject under a test owner projection.
#[must_use]
pub fn test_subject(subject_id: &str) -> SubjectRef {
    SubjectRef {
        projection_type: GtsTypeId::new("gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~"),
        subject_id: subject_id.to_owned(),
    }
}

/// A consumption Quota draft with a bounded cap.
#[must_use]
pub fn quota_draft(subject: SubjectRef, cap: Option<u64>) -> QuotaDraft {
    QuotaDraft {
        tenant_id: test_tenant(),
        subject,
        metric: test_metric(),
        quota_type: QuotaType::Allocation,
        period: None,
        enforcement_mode: EnforcementMode::Hard,
        cap,
        notification_thresholds: Vec::new(),
        validity_window: None,
        fail_open_hint: false,
        metadata: serde_json::Map::new(),
        source: QuotaSource::Licensing,
        constraint_contract: ContractRef {
            type_id: GtsTypeId::new(
                "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~",
            ),
            version: 1,
        },
    }
}

// ---------------------------------------------------------------------------
// Coordination double
// ---------------------------------------------------------------------------

struct Hold {
    holder_id: Uuid,
    expires_at: OffsetDateTime,
}

#[derive(Default)]
struct CoordState {
    holds: HashMap<LockScope, Hold>,
    failure: Option<CoordinationError>,
    try_lock_calls: usize,
    renew_calls: usize,
    release_calls: usize,
}

/// In-memory [`CoordinationPluginV1`] with deterministic expiry.
#[derive(Default)]
pub struct InMemoryCoordination {
    state: Mutex<CoordState>,
}

impl InMemoryCoordination {
    /// An empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every later call fails with `err` until [`Self::clear_failure`].
    pub fn fail_with(&self, err: CoordinationError) {
        self.state.lock().failure = Some(err);
    }

    /// Stop the injected failure.
    pub fn clear_failure(&self) {
        self.state.lock().failure = None;
    }

    /// Move every hold's expiry into the past. Simulates a TTL elapse.
    pub fn expire_all(&self) {
        let past = OffsetDateTime::now_utc() - Duration::from_secs(1);
        for hold in self.state.lock().holds.values_mut() {
            hold.expires_at = past;
        }
    }

    /// True when a live, unexpired hold exists for `scope`.
    #[must_use]
    pub fn is_held(&self, scope: LockScope) -> bool {
        let now = OffsetDateTime::now_utc();
        self.state
            .lock()
            .holds
            .get(&scope)
            .is_some_and(|h| h.expires_at > now)
    }

    /// Number of `try_lock` calls, failures included.
    #[must_use]
    pub fn try_lock_calls(&self) -> usize {
        self.state.lock().try_lock_calls
    }

    /// Number of `renew` calls, failures included.
    #[must_use]
    pub fn renew_calls(&self) -> usize {
        self.state.lock().renew_calls
    }

    /// Number of `release` calls, failures included.
    #[must_use]
    pub fn release_calls(&self) -> usize {
        self.state.lock().release_calls
    }
}

#[async_trait]
impl CoordinationPluginV1 for InMemoryCoordination {
    async fn try_lock(&self, scope: LockScope, ttl: Duration) -> Result<Lock, CoordinationError> {
        let mut st = self.state.lock();
        st.try_lock_calls += 1;
        if let Some(err) = &st.failure {
            return Err(err.clone());
        }
        let now = OffsetDateTime::now_utc();
        if st.holds.get(&scope).is_some_and(|h| h.expires_at > now) {
            return Err(CoordinationError::Conflict { scope });
        }
        let holder_id = Uuid::now_v7();
        st.holds.insert(
            scope,
            Hold {
                holder_id,
                expires_at: now + ttl,
            },
        );
        Ok(Lock::new(scope, holder_id, ttl, now))
    }

    async fn renew(&self, lock: &Lock) -> Result<(), CoordinationError> {
        let mut st = self.state.lock();
        st.renew_calls += 1;
        if let Some(err) = &st.failure {
            return Err(err.clone());
        }
        let now = OffsetDateTime::now_utc();
        let scope = lock.scope();
        match st.holds.get_mut(&scope) {
            Some(hold) if hold.holder_id == lock.holder_id() && hold.expires_at > now => {
                hold.expires_at = now + lock.ttl();
                Ok(())
            }
            _ => Err(CoordinationError::LockExpired { scope }),
        }
    }

    async fn release(&self, lock: Lock) -> Result<(), CoordinationError> {
        let mut st = self.state.lock();
        st.release_calls += 1;
        if let Some(err) = &st.failure {
            return Err(err.clone());
        }
        let scope = lock.scope();
        if st
            .holds
            .get(&scope)
            .is_some_and(|h| h.holder_id == lock.holder_id())
        {
            st.holds.remove(&scope);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Storage double
// ---------------------------------------------------------------------------

struct LeaseRow {
    tenant_id: TenantId,
    metric: MetricId,
    subject_key: crate::models::IdempotencySubjectKey,
    holds: Vec<LeaseHold>,
    state: LeaseState,
    expires_at: OffsetDateTime,
}

struct LogEntry {
    at: OffsetDateTime,
}

#[derive(Default)]
struct StorageState {
    installed_major: Option<u32>,
    bootstrapped: Option<BootstrapBundle>,
    bootstrap_calls: usize,
    defaults: Option<ConfigDefaults>,
    quotas: BTreeMap<QuotaId, Quota>,
    consumed: BTreeMap<QuotaId, u64>,
    leases: BTreeMap<LeaseToken, LeaseRow>,
    idempotency: HashMap<IdempotencyScope, IdempotencyRecord>,
    policies: BTreeMap<PolicyId, Vec<PolicyVersion>>,
    events: Vec<NotificationEvent>,
    log: Vec<LogEntry>,
    failure: Option<StorageError>,
}

/// Complete in-memory [`QuotaEnforcementStoragePluginV1`].
pub struct InMemoryStorage {
    state: Mutex<StorageState>,
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStorage {
    /// A backend whose installed schema major equals [`CONTRACT_MAJOR`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(StorageState {
                installed_major: Some(CONTRACT_MAJOR),
                ..StorageState::default()
            }),
        }
    }

    /// A backend that reports another installed schema major (I12 tests).
    #[must_use]
    pub fn with_installed_schema_major(major: u32) -> Self {
        let this = Self::new();
        this.state.lock().installed_major = Some(major);
        this
    }

    /// Every later call fails with `err` until [`Self::clear_failure`].
    pub fn fail_with(&self, err: StorageError) {
        self.state.lock().failure = Some(err);
    }

    /// Stop the injected failure.
    pub fn clear_failure(&self) {
        self.state.lock().failure = None;
    }

    /// Number of `bootstrap` calls, failures included.
    #[must_use]
    pub fn bootstrap_calls(&self) -> usize {
        self.state.lock().bootstrap_calls
    }

    /// The bundle of the last successful `bootstrap`.
    #[must_use]
    pub fn bootstrapped_bundle(&self) -> Option<BootstrapBundle> {
        self.state.lock().bootstrapped.clone()
    }

    /// The seeded default rows, once `bootstrap` succeeded.
    #[must_use]
    pub fn seeded_defaults(&self) -> Option<ConfigDefaults> {
        self.state.lock().defaults
    }

    /// Every event enqueued so far, in order.
    #[must_use]
    pub fn events(&self) -> Vec<NotificationEvent> {
        self.state.lock().events.clone()
    }

    /// A stored Quota.
    #[must_use]
    pub fn quota(&self, id: QuotaId) -> Option<Quota> {
        self.state.lock().quotas.get(&id).cloned()
    }

    /// Consumed or in-flight amount of a Quota.
    #[must_use]
    pub fn consumed(&self, id: QuotaId) -> u64 {
        self.state.lock().consumed.get(&id).copied().unwrap_or(0)
    }

    /// State of a lease, if it exists.
    #[must_use]
    pub fn lease_state(&self, token: LeaseToken) -> Option<LeaseState> {
        self.state.lock().leases.get(&token).map(|l| l.state)
    }

    /// Move every active lease's expiry into the past (I4 tests).
    pub fn expire_leases(&self) {
        let past = OffsetDateTime::now_utc() - Duration::from_secs(1);
        for lease in self.state.lock().leases.values_mut() {
            lease.expires_at = past;
        }
    }

    fn check(st: &StorageState) -> Result<(), StorageError> {
        match &st.failure {
            Some(err) => Err(err.clone()),
            None => Ok(()),
        }
    }

    fn push_events(st: &mut StorageState, events: &[NotificationEvent]) -> Vec<EventId> {
        st.events.extend_from_slice(events);
        st.log.push(LogEntry {
            at: OffsetDateTime::now_utc(),
        });
        events.iter().map(|e| e.event_id).collect()
    }

    fn record(st: &mut StorageState, write: &IdempotencyWrite) -> Result<bool, StorageError> {
        if let Some(existing) = st.idempotency.get(&write.scope) {
            if existing.payload_hash == write.payload_hash {
                return Ok(true);
            }
            return Err(StorageError::IdempotencyPayloadMismatch);
        }
        let retention = st.defaults.map_or(86_400, |d| d.idempotency_retention_secs);
        let now = OffsetDateTime::now_utc();
        st.idempotency.insert(
            write.scope.clone(),
            IdempotencyRecord {
                scope: write.scope.clone(),
                payload_hash: write.payload_hash,
                decision_blob: serde_json::to_value(&write.decision)
                    .map_err(|e| StorageError::Internal(e.to_string()))?,
                engine_id: write.engine_id.clone(),
                policy_id: write.policy_id.clone(),
                policy_version: write.policy_version,
                created_at: now,
                expires_at: now + Duration::from_secs(retention),
            },
        );
        Ok(false)
    }

    fn snapshot_counters(st: &StorageState, plan: &DebitPlan) -> MutationResult {
        MutationResult {
            counters: plan
                .keys()
                .map(|id| crate::models::CounterSnapshot {
                    quota_id: *id,
                    period_id: None,
                    value: st.consumed.get(id).copied().unwrap_or(0),
                })
                .collect(),
            threshold_crossings: Vec::new(),
            event_ids: Vec::new(),
        }
    }

    fn active_quota(st: &StorageState, id: QuotaId) -> Result<&Quota, StorageError> {
        let quota = st
            .quotas
            .get(&id)
            .ok_or(StorageError::QuotaNotFound { id })?;
        if quota.status == QuotaStatus::Deactivated {
            return Err(StorageError::QuotaDeactivated { id });
        }
        Ok(quota)
    }

    fn paginate<T: Clone>(items: &[T], page: &PageRequest) -> PageResult<T> {
        let start: usize = page
            .cursor
            .as_deref()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let limit = page.limit.max(1) as usize;
        let end = start.saturating_add(limit).min(items.len());
        let next_cursor = (end < items.len()).then(|| end.to_string());
        PageResult {
            items: items.get(start..end).unwrap_or_default().to_vec(),
            next_cursor,
        }
    }

    fn snapshot(st: &StorageState, quota: &Quota) -> QuotaSnapshot {
        let consumed = st.consumed.get(&quota.id).copied().unwrap_or(0);
        let now = OffsetDateTime::now_utc();
        QuotaSnapshot {
            quota_id: quota.id,
            subject: quota.subject.clone(),
            metric: quota.metric.clone(),
            quota_type: quota.quota_type,
            enforcement_mode: quota.enforcement_mode,
            cap: quota.cap,
            consumed,
            remaining: quota.cap.map(|cap| cap.saturating_sub(consumed)),
            period: None,
            metadata: quota.metadata.clone(),
            validity_window: quota.validity_window,
            currently_within_window: quota.validity_window.is_none_or(|w| w.contains(now)),
        }
    }

    fn matches(quota: &Quota, applicable: &ApplicableQuotas) -> bool {
        quota.status == QuotaStatus::Active
            && quota.tenant_id == applicable.tenant_id
            && quota.metric == applicable.metric
            && applicable.subjects.contains(&quota.subject)
    }

    fn active_version(versions: &[PolicyVersion]) -> Option<&PolicyVersion> {
        versions
            .iter()
            .find(|v| v.state == PolicyVersionState::Active)
    }
}

#[async_trait]
impl QuotaEnforcementStoragePluginV1 for InMemoryStorage {
    async fn bootstrap(&self, bundle: &BootstrapBundle) -> Result<(), StorageError> {
        let mut st = self.state.lock();
        st.bootstrap_calls += 1;
        Self::check(&st)?;
        let installed = st.installed_major.unwrap_or(bundle.contract_major);
        if installed != bundle.contract_major {
            return Err(StorageError::SchemaVersionMismatch {
                installed,
                expected: bundle.contract_major,
            });
        }
        st.installed_major = Some(installed);
        if st.defaults.is_none() {
            st.defaults = Some(bundle.config_defaults);
        }
        if let Some(draft) = &bundle.global_policy {
            st.policies
                .entry(PolicyId::global())
                .or_insert_with(|| vec![new_version(PolicyId::global(), 1, draft.clone())]);
        }
        st.bootstrapped = Some(bundle.clone());
        Ok(())
    }

    async fn create_quota(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        draft: QuotaDraft,
        events: &[NotificationEvent],
    ) -> Result<QuotaId, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let now = OffsetDateTime::now_utc();
        let id = QuotaId::generate();
        st.quotas.insert(
            id,
            Quota {
                id,
                tenant_id: draft.tenant_id,
                subject: draft.subject,
                metric: draft.metric,
                quota_type: draft.quota_type,
                period: draft.period,
                enforcement_mode: draft.enforcement_mode,
                cap: draft.cap,
                notification_thresholds: draft.notification_thresholds,
                validity_window: draft.validity_window,
                fail_open_hint: draft.fail_open_hint,
                metadata: draft.metadata,
                source: draft.source,
                status: QuotaStatus::Active,
                constraint_contract: draft.constraint_contract,
                record_version: 1,
                created_at: now,
                updated_at: now,
            },
        );
        Self::push_events(&mut st, events);
        Ok(id)
    }

    async fn update_quota(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        quota_id: QuotaId,
        patch: QuotaPatch,
        events: &[NotificationEvent],
    ) -> Result<Quota, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let consumed = st.consumed.get(&quota_id).copied().unwrap_or(0);
        let quota = st
            .quotas
            .get_mut(&quota_id)
            .ok_or(StorageError::QuotaNotFound { id: quota_id })?;
        if let Some(cap) = patch.cap {
            match cap {
                CapPatch::Bounded(new_cap) if new_cap < consumed => {
                    return Err(StorageError::CapBelowConsumed { new_cap, consumed });
                }
                CapPatch::Bounded(new_cap) => quota.cap = Some(new_cap),
                CapPatch::Unbounded => quota.cap = None,
            }
        }
        if let Some(thresholds) = patch.notification_thresholds {
            quota.notification_thresholds = thresholds;
        }
        if let Some(window) = patch.validity_window {
            quota.validity_window = match window {
                ValidityWindowPatch::Clear => None,
                ValidityWindowPatch::Set(w) => Some(w),
            };
        }
        if let Some(metadata) = patch.metadata {
            quota.metadata = metadata;
        }
        if let Some(mode) = patch.enforcement_mode {
            quota.enforcement_mode = mode;
        }
        if let Some(hint) = patch.fail_open_hint {
            quota.fail_open_hint = hint;
        }
        quota.record_version += 1;
        quota.updated_at = OffsetDateTime::now_utc();
        let updated = quota.clone();
        Self::push_events(&mut st, events);
        Ok(updated)
    }

    async fn deactivate_quota(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        quota_id: QuotaId,
        events: &[NotificationEvent],
    ) -> Result<DeactivateOutcome, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let quota = st
            .quotas
            .get_mut(&quota_id)
            .ok_or(StorageError::QuotaNotFound { id: quota_id })?;
        quota.status = QuotaStatus::Deactivated;
        quota.record_version += 1;
        let mut resolved = Vec::new();
        for (token, lease) in &mut st.leases {
            if lease.state == LeaseState::Active
                && lease.holds.iter().any(|h| h.quota_id == quota_id)
            {
                lease.state = LeaseState::ResolvedByDeactivation;
                resolved.push(*token);
            }
        }
        Self::push_events(&mut st, events);
        Ok(DeactivateOutcome {
            resolved_leases: resolved,
        })
    }

    async fn read_quotas(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        filter: QuotaFilter,
        page: PageRequest,
    ) -> Result<PageResult<Quota>, StorageError> {
        let st = self.state.lock();
        Self::check(&st)?;
        let items: Vec<Quota> = st
            .quotas
            .values()
            .filter(|q| filter.tenant_id.is_none_or(|t| q.tenant_id == t))
            .filter(|q| filter.subject.as_ref().is_none_or(|s| &q.subject == s))
            .filter(|q| filter.metric.as_ref().is_none_or(|m| &q.metric == m))
            .filter(|q| filter.status.is_none_or(|s| q.status == s))
            .filter(|q| filter.ids.is_empty() || filter.ids.contains(&q.id))
            .cloned()
            .collect();
        Ok(Self::paginate(&items, &page))
    }

    async fn apply_debit_plan(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        _applicable: &ApplicableQuotas,
        plan: &DebitPlan,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        if Self::record(&mut st, idempotency)? {
            return Ok(Self::snapshot_counters(&st, plan));
        }
        for id in plan.keys() {
            Self::active_quota(&st, *id)?;
        }
        for (id, entry) in plan {
            let counter = st.consumed.entry(*id).or_insert(0);
            *counter = counter.saturating_add(entry.amount);
        }
        let mut result = Self::snapshot_counters(&st, plan);
        result.event_ids = Self::push_events(&mut st, events);
        Ok(result)
    }

    async fn apply_batch_debit(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        envelope: &IdempotencyWrite,
        items: &[BatchDebitItem],
        events: &[NotificationEvent],
    ) -> Result<Vec<MutationResult>, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        if Self::record(&mut st, envelope)? {
            return Ok(items
                .iter()
                .map(|i| Self::snapshot_counters(&st, &i.plan))
                .collect());
        }
        for item in items {
            for id in item.plan.keys() {
                Self::active_quota(&st, *id)?;
            }
        }
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            for (id, entry) in &item.plan {
                let counter = st.consumed.entry(*id).or_insert(0);
                *counter = counter.saturating_add(entry.amount);
            }
            results.push(Self::snapshot_counters(&st, &item.plan));
        }
        Self::push_events(&mut st, events);
        Ok(results)
    }

    async fn apply_credit(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        quota_id: QuotaId,
        amount: u64,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let plan: DebitPlan =
            BTreeMap::from([(quota_id, crate::models::QuotaDebitPlan { amount })]);
        if Self::record(&mut st, idempotency)? {
            return Ok(Self::snapshot_counters(&st, &plan));
        }
        Self::active_quota(&st, quota_id)?;
        let counter = st.consumed.entry(quota_id).or_insert(0);
        *counter = counter.saturating_sub(amount);
        let mut result = Self::snapshot_counters(&st, &plan);
        result.event_ids = Self::push_events(&mut st, events);
        Ok(result)
    }

    async fn apply_rollback(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        original: &IdempotencyScope,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let plan: DebitPlan = {
            let record = st
                .idempotency
                .get(original)
                .ok_or_else(|| StorageError::Internal("original operation unknown".to_owned()))?;
            let decision: crate::models::Decision =
                serde_json::from_value(record.decision_blob.clone())
                    .map_err(|e| StorageError::Internal(e.to_string()))?;
            decision.debit_plan
        };
        if Self::record(&mut st, idempotency)? {
            return Ok(Self::snapshot_counters(&st, &plan));
        }
        for (id, entry) in &plan {
            let counter = st.consumed.entry(*id).or_insert(0);
            *counter = counter.saturating_sub(entry.amount);
        }
        let mut result = Self::snapshot_counters(&st, &plan);
        result.event_ids = Self::push_events(&mut st, events);
        Ok(result)
    }

    async fn acquire_lease(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        applicable: &ApplicableQuotas,
        plan: &DebitPlan,
        ttl: Duration,
        idempotency: &IdempotencyWrite,
    ) -> Result<LeaseToken, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        if Self::record(&mut st, idempotency)? {
            return st
                .leases
                .iter()
                .find(|(_, l)| l.subject_key == idempotency.scope.subject_key)
                .map(|(token, _)| *token)
                .ok_or_else(|| StorageError::Internal("replayed lease not found".to_owned()));
        }
        let now = OffsetDateTime::now_utc();
        let cap = st.defaults.map_or(1000, |d| d.max_active_leases) as usize;
        let live = st
            .leases
            .values()
            .filter(|l| {
                l.state == LeaseState::Active
                    && l.expires_at > now
                    && l.tenant_id == applicable.tenant_id
                    && l.metric == applicable.metric
            })
            .count();
        if live >= cap {
            return Err(StorageError::LeaseInflightLimitExceeded);
        }
        for id in plan.keys() {
            Self::active_quota(&st, *id)?;
        }
        for (id, entry) in plan {
            let counter = st.consumed.entry(*id).or_insert(0);
            *counter = counter.saturating_add(entry.amount);
        }
        let token = LeaseToken::generate();
        st.leases.insert(
            token,
            LeaseRow {
                tenant_id: applicable.tenant_id,
                metric: applicable.metric.clone(),
                subject_key: idempotency.scope.subject_key,
                holds: plan
                    .iter()
                    .map(|(id, e)| LeaseHold {
                        quota_id: *id,
                        held_amount: e.amount,
                        period_id: None,
                    })
                    .collect(),
                state: LeaseState::Active,
                expires_at: now + ttl,
            },
        );
        Ok(token)
    }

    async fn commit_lease(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        token: LeaseToken,
        actual_amount: Option<u64>,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        if Self::record(&mut st, idempotency)? {
            return Ok(MutationResult::default());
        }
        let now = OffsetDateTime::now_utc();
        let holds = {
            let lease = st
                .leases
                .get_mut(&token)
                .filter(|l| l.state == LeaseState::Active && l.expires_at > now)
                .ok_or(StorageError::LeaseNotActive { token })?;
            let reserved: u64 = lease.holds.iter().map(|h| h.held_amount).sum();
            let actual = actual_amount.unwrap_or(reserved);
            if actual > reserved {
                return Err(StorageError::OverCommitNotAuthorized { reserved, actual });
            }
            lease.state = LeaseState::Committed;
            let unused = reserved - actual;
            let holds = lease.holds.clone();
            (holds, unused)
        };
        let (holds, mut unused) = holds;
        for hold in &holds {
            let give_back = unused.min(hold.held_amount);
            unused -= give_back;
            let counter = st.consumed.entry(hold.quota_id).or_insert(0);
            *counter = counter.saturating_sub(give_back);
        }
        let plan: DebitPlan = holds
            .iter()
            .map(|h| {
                (
                    h.quota_id,
                    crate::models::QuotaDebitPlan {
                        amount: h.held_amount,
                    },
                )
            })
            .collect();
        let mut result = Self::snapshot_counters(&st, &plan);
        result.event_ids = Self::push_events(&mut st, events);
        Ok(result)
    }

    async fn release_lease(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        token: LeaseToken,
        idempotency: &IdempotencyWrite,
        events: &[NotificationEvent],
    ) -> Result<MutationResult, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        if Self::record(&mut st, idempotency)? {
            return Ok(MutationResult::default());
        }
        let now = OffsetDateTime::now_utc();
        let holds = {
            let lease = st
                .leases
                .get_mut(&token)
                .filter(|l| l.state == LeaseState::Active && l.expires_at > now)
                .ok_or(StorageError::LeaseNotActive { token })?;
            lease.state = LeaseState::Released;
            lease.holds.clone()
        };
        for hold in &holds {
            let counter = st.consumed.entry(hold.quota_id).or_insert(0);
            *counter = counter.saturating_sub(hold.held_amount);
        }
        let plan: DebitPlan = holds
            .iter()
            .map(|h| {
                (
                    h.quota_id,
                    crate::models::QuotaDebitPlan {
                        amount: h.held_amount,
                    },
                )
            })
            .collect();
        let mut result = Self::snapshot_counters(&st, &plan);
        result.event_ids = Self::push_events(&mut st, events);
        Ok(result)
    }

    async fn read_quota_snapshot(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        applicable: &ApplicableQuotas,
    ) -> Result<Vec<QuotaSnapshot>, StorageError> {
        let st = self.state.lock();
        Self::check(&st)?;
        Ok(st
            .quotas
            .values()
            .filter(|q| Self::matches(q, applicable))
            .map(|q| Self::snapshot(&st, q))
            .collect())
    }

    async fn bulk_read_quota_snapshot(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        pairs: &[ApplicableQuotas],
        page: PageRequest,
    ) -> Result<PageResult<QuotaSnapshot>, StorageError> {
        let st = self.state.lock();
        Self::check(&st)?;
        let items: Vec<QuotaSnapshot> = st
            .quotas
            .values()
            .filter(|q| pairs.iter().any(|a| Self::matches(q, a)))
            .map(|q| Self::snapshot(&st, q))
            .collect();
        Ok(Self::paginate(&items, &page))
    }

    async fn create_policy(
        &self,
        _ctx: &SecurityContext,
        draft: PolicyDraft,
        events: &[NotificationEvent],
    ) -> Result<PolicyVersion, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        if let Some(existing) = st
            .policies
            .values()
            .filter_map(|v| Self::active_version(v))
            .find(|v| v.scope == draft.scope)
        {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: existing.version,
            });
        }
        let policy_id = match &draft.scope {
            PolicyScope::Global => PolicyId::global(),
            PolicyScope::Metric { metric } => PolicyId::new(format!("metric={metric}")),
        };
        let version = new_version(policy_id.clone(), 1, draft);
        st.policies.insert(policy_id, vec![version.clone()]);
        Self::push_events(&mut st, events);
        Ok(version)
    }

    async fn update_policy(
        &self,
        _ctx: &SecurityContext,
        policy_id: PolicyId,
        update: PolicyUpdate,
        events: &[NotificationEvent],
    ) -> Result<PolicyVersion, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let versions =
            st.policies
                .get_mut(&policy_id)
                .ok_or_else(|| StorageError::UnknownPolicyVersion {
                    policy_id: policy_id.clone(),
                    version: update.if_match_version,
                })?;
        let current = versions
            .iter()
            .position(|v| v.state == PolicyVersionState::Active)
            .ok_or_else(|| StorageError::UnknownPolicyVersion {
                policy_id: policy_id.clone(),
                version: update.if_match_version,
            })?;
        if versions[current].version != update.if_match_version {
            return Err(StorageError::VersionConflict {
                expected: update.if_match_version,
                actual: versions[current].version,
            });
        }
        let latest = versions.iter().map(|v| v.version).max().unwrap_or(0);
        let mut next = versions[current].clone();
        next.version = latest + 1;
        next.state = PolicyVersionState::Active;
        next.created_at = OffsetDateTime::now_utc();
        next.created_by = update.created_by;
        next.comment = update.comment;
        if let Some(engine_id) = update.engine_id {
            next.engine_id = engine_id;
        }
        if let Some(config) = update.engine_config {
            next.engine_config = config;
        }
        if update.timeout_ms.is_some() {
            next.timeout_ms = update.timeout_ms;
        }
        versions[current].state = PolicyVersionState::Superseded;
        versions.push(next.clone());
        Self::push_events(&mut st, events);
        Ok(next)
    }

    async fn rollback_policy(
        &self,
        _ctx: &SecurityContext,
        policy_id: PolicyId,
        target_version: u32,
        comment: Option<String>,
        events: &[NotificationEvent],
    ) -> Result<PolicyVersion, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let versions =
            st.policies
                .get_mut(&policy_id)
                .ok_or_else(|| StorageError::UnknownPolicyVersion {
                    policy_id: policy_id.clone(),
                    version: target_version,
                })?;
        let target = versions
            .iter()
            .position(|v| v.version == target_version)
            .ok_or_else(|| StorageError::UnknownPolicyVersion {
                policy_id: policy_id.clone(),
                version: target_version,
            })?;
        if versions[target].state == PolicyVersionState::RolledBack {
            return Err(StorageError::VersionRolledBack {
                policy_id,
                version: target_version,
            });
        }
        if let Some(active) = versions
            .iter_mut()
            .find(|v| v.state == PolicyVersionState::Active && v.version != target_version)
        {
            active.state = PolicyVersionState::RolledBack;
        }
        versions[target].state = PolicyVersionState::Active;
        if comment.is_some() {
            versions[target].comment = comment;
        }
        let result = versions[target].clone();
        Self::push_events(&mut st, events);
        Ok(result)
    }

    async fn delete_policy(
        &self,
        _ctx: &SecurityContext,
        policy_id: PolicyId,
        _comment: Option<String>,
        events: &[NotificationEvent],
    ) -> Result<(), StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        if let Some(active) = st.policies.get_mut(&policy_id).and_then(|versions| {
            versions
                .iter_mut()
                .find(|v| v.state == PolicyVersionState::Active)
        }) {
            active.state = PolicyVersionState::Deleted;
        }
        Self::push_events(&mut st, events);
        Ok(())
    }

    async fn read_policy(
        &self,
        scope: &PolicyScope,
    ) -> Result<Option<PolicyVersion>, StorageError> {
        let st = self.state.lock();
        Self::check(&st)?;
        Ok(st
            .policies
            .values()
            .filter_map(|v| Self::active_version(v))
            .find(|v| &v.scope == scope)
            .cloned())
    }

    async fn read_policy_version(
        &self,
        policy_id: &PolicyId,
        version: u32,
    ) -> Result<Option<PolicyVersion>, StorageError> {
        let st = self.state.lock();
        Self::check(&st)?;
        Ok(st
            .policies
            .get(policy_id)
            .and_then(|v| v.iter().find(|p| p.version == version))
            .cloned())
    }

    async fn list_policy_versions(
        &self,
        policy_id: &PolicyId,
        page: PageRequest,
    ) -> Result<PageResult<PolicyVersionMeta>, StorageError> {
        let st = self.state.lock();
        Self::check(&st)?;
        let items: Vec<PolicyVersionMeta> = st
            .policies
            .get(policy_id)
            .map(|versions| {
                versions
                    .iter()
                    .map(|v| PolicyVersionMeta {
                        version: v.version,
                        state: v.state,
                        created_at: v.created_at,
                        created_by: v.created_by.clone(),
                        comment: v.comment.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self::paginate(&items, &page))
    }

    async fn lookup_idempotency(
        &self,
        scope: &IdempotencyScope,
    ) -> Result<Option<IdempotencyRecord>, StorageError> {
        let st = self.state.lock();
        Self::check(&st)?;
        Ok(st.idempotency.get(scope).cloned())
    }

    async fn reclaim_expired_leases(
        &self,
        batch_size: u32,
        before: OffsetDateTime,
    ) -> Result<Vec<ExpiredLease>, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let mut reclaimed = Vec::new();
        for (token, lease) in &mut st.leases {
            if reclaimed.len() >= batch_size as usize {
                break;
            }
            if lease.state == LeaseState::Active && lease.expires_at <= before {
                lease.state = LeaseState::AutoReleased;
                reclaimed.push(ExpiredLease {
                    token: *token,
                    tenant_id: lease.tenant_id,
                    subject_key: lease.subject_key,
                    holds: lease.holds.clone(),
                    expired_at: lease.expires_at,
                });
            }
        }
        for lease in &reclaimed {
            for hold in &lease.holds {
                let counter = st.consumed.entry(hold.quota_id).or_insert(0);
                *counter = counter.saturating_sub(hold.held_amount);
            }
        }
        Ok(reclaimed)
    }

    async fn reclaim_expired_idempotency(
        &self,
        batch_size: u32,
        before: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let victims: Vec<IdempotencyScope> = st
            .idempotency
            .iter()
            .filter(|(_, r)| r.expires_at <= before)
            .take(batch_size as usize)
            .map(|(k, _)| k.clone())
            .collect();
        for scope in &victims {
            st.idempotency.remove(scope);
        }
        Ok(victims.len() as u64)
    }

    async fn reclaim_operation_log(
        &self,
        batch_size: u32,
        before: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        let mut st = self.state.lock();
        Self::check(&st)?;
        let before_len = st.log.len();
        let mut removed = 0_usize;
        st.log.retain(|entry| {
            if removed < batch_size as usize && entry.at <= before {
                removed += 1;
                false
            } else {
                true
            }
        });
        Ok((before_len - st.log.len()) as u64)
    }
}

fn new_version(policy_id: PolicyId, version: u32, draft: PolicyDraft) -> PolicyVersion {
    PolicyVersion {
        policy_id,
        version,
        scope: draft.scope,
        engine_id: draft.engine_id,
        engine_config: draft.engine_config,
        timeout_ms: draft.timeout_ms,
        description: draft.description,
        state: PolicyVersionState::Active,
        created_at: OffsetDateTime::now_utc(),
        created_by: draft.created_by,
        comment: draft.comment,
    }
}

/// Convenience: a JSON `null` engine config for policy drafts in tests.
#[must_use]
pub fn empty_engine_config() -> Value {
    Value::Object(serde_json::Map::new())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "testing_tests.rs"]
mod testing_tests;
