//! Native distributed lock over one `Lease` per lock name (DESIGN.md §5).
//!
//! [`K8sLock`] implements [`DistributedLockBackend`] over
//! `coordination.k8s.io/v1.Lease`. A held lock is a Lease carrying our per-acquisition
//! **holder token** (`<identity>#<uuid>`, §5.1); acquisition is create-or-guarded-
//! claim (§5.2); a blocking [`lock()`](DistributedLockBackend::lock) establishes a
//! watch on the one Lease *before* its first attempt and shares it with any
//! same-process waiter via [`waiters`] (§5.3); renew and release are token-fenced
//! guarded writes (§5.4); and release **clears** the holder rather than deleting the
//! object, with a background [`reaper`] pruning long-empty objects (§5.5).
//!
//! Per §3.3 a held lock runs no renewal loop — renewal is consumer-driven through
//! the [`LockGuard`]. Servicing that guard's command channel is one parked task per
//! held lock (the same shape the postgres plugin uses), which costs no connection
//! and no polling.
//!
//! The pure pieces carry the L1 coverage: the [`HolderToken`] round-trip, the
//! blocking-wait 3-outcome classifier ([`classify_wait`]), the [`waiters`] registry,
//! and the [`reaper`] eligibility predicate. Real-server behaviour is Phase 6.

mod reaper;
mod waiters;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use futures_util::StreamExt;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::Api;
use kube::api::ObjectMeta;
use kube::runtime::watcher;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use cluster_sdk::lock::{
    DistributedLockBackend, LockCommandReceiver, LockFeatures, LockGuard, LockRequest,
};
use cluster_sdk::observability::{self, ResourceId, result, spans};
use cluster_sdk::{ClusterError, ClusterMetrics};
use tracing::Instrument as _;

use crate::client::ResolvedClient;
use crate::config::K8sLockConfig;
use crate::guarded::{self, CallSite, Created, Replaced};
use crate::k8s_error;
use crate::lease::{lease_duration_seconds, ttl_ms};
use crate::naming::{
    self, ANNOTATION_NAME, ANNOTATION_TTL_MS, LABEL_MANAGED_BY, LABEL_PRIMITIVE, MANAGED_BY_VALUE,
    Seg,
};
use crate::observed::Observed;

use self::waiters::LockWaiters;

/// The in-flight command buffer for each [`LockGuard`] (§5.4).
const GUARD_COMMAND_BUFFER: usize = 4;

/// The `(holderIdentity, renewTime)` pair `Observed` tracks for expiry (§2.8).
type Record = (Option<String>, Option<String>);

/// A per-acquisition lock holder token: `<identity>#<uuid-v4>` (§5.1).
///
/// The identity prefix answers "which replica holds this?" in `kubectl` without a
/// lookup; the fresh UUID makes two acquisitions unconfusable, which is what makes
/// renew/release safe against a successor and forces two in-process acquisitions to
/// arbitrate through the API server exactly as two processes would (§5.1). Only this
/// plugin parses the `#`, and it splits on the **last** one so an identity may
/// itself contain `#`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderToken {
    identity: String,
    uuid: String,
}

impl HolderToken {
    /// A fresh token for `identity` with a new v4 UUID (§5.1).
    #[must_use]
    pub fn generate(identity: &str) -> Self {
        Self {
            identity: identity.to_owned(),
            uuid: Uuid::new_v4().to_string(),
        }
    }

    /// The wire form written to `holderIdentity`: `<identity>#<uuid>`.
    #[must_use]
    pub fn to_holder_string(&self) -> String {
        format!("{}#{}", self.identity, self.uuid)
    }

    /// Parses a `holderIdentity`, splitting on the **last** `#` (§5.1). Returns
    /// `None` for a holder with no `#` — a foreign/legacy holder this plugin did not
    /// write.
    ///
    /// The exact inverse of [`to_holder_string`](Self::to_holder_string). The acquire
    /// path compares the raw `holderIdentity` string rather than a parsed token, so
    /// this is retained as the token codec's other half — exercised by the unit tests
    /// and used by `kubectl`-side diagnostics — rather than consumed on a hot path.
    #[allow(dead_code)]
    #[must_use]
    pub fn parse(holder: &str) -> Option<Self> {
        let (identity, uuid) = holder.rsplit_once('#')?;
        if uuid.is_empty() {
            return None;
        }
        Some(Self {
            identity: identity.to_owned(),
            uuid: uuid.to_owned(),
        })
    }
}

/// Why a blocking `lock()` attempt stops waiting, decided from the loop's terminal
/// conditions (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitDecision {
    /// The plugin is shutting down — the caller must not retry (`ClusterError::Shutdown`).
    Shutdown,
    /// The caller's budget elapsed with the lock genuinely held (`LockTimeout`).
    Timeout,
    /// Neither: keep waiting for a release or the holder's expiry.
    Keep,
}

/// Classifies a blocked `lock()`'s next step from `(shutdown cancelled, deadline
/// passed)` (§5.3), a pure function so the three-way distinction — the point of the
/// section — is unit-tested as one.
///
/// Shutdown is checked first: a plugin going down must return `Shutdown`, not a
/// `LockTimeout` the caller might retry. A backend (`Provider`) error is not one of
/// these outcomes — it propagates immediately from the attempt and never reaches
/// this decision.
#[must_use]
pub fn classify_wait(cancelled: bool, deadline_passed: bool) -> WaitDecision {
    if cancelled {
        WaitDecision::Shutdown
    } else if deadline_passed {
        WaitDecision::Timeout
    } else {
        WaitDecision::Keep
    }
}

/// Shared runtime for every lock this backend serves.
struct LockRuntime {
    client: kube::Client,
    namespace: String,
    identity: String,
    lease_prefix: String,
    /// The ADR-004 metrics sink; emits `cluster_lock_ops_total` /
    /// `cluster_lock_op_duration_seconds` / `cluster_provider_errors_total` (§8).
    metrics: Arc<dyn ClusterMetrics>,
    /// The bounded `provider` label attached to every emitted signal.
    provider: &'static str,
    request_timeout: Duration,
    reaper_enabled: bool,
    reaper_interval: Duration,
    lock_object_retention: Duration,
    lock_name_cardinality_warn: u64,
    /// Per-name incumbent observations, refreshed on each acquire attempt so a
    /// lapsed foreign holder can be stolen only after a full TTL of observation
    /// (§2.8, §5.2) — never on first sight.
    observed: DashMap<String, Observed<Record>>,
    /// In-process release-waiter registry shared by blocking `lock()` calls (§5.3).
    waiters: Arc<LockWaiters>,
}

impl LockRuntime {
    fn api(&self) -> Api<Lease> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn lease_name(&self, coordination_name: &str) -> String {
        naming::lease_name(&self.lease_prefix, Seg::Lock, coordination_name)
    }

    async fn read(&self, name: &str) -> Result<Option<Lease>, ClusterError> {
        let api = self.api();
        self.timed("get lock lease", guarded::read(&api, name))
            .await
    }

    /// Records the ADR-004 metric side of a finished lock op — the duration
    /// histogram, the bounded-`result` counter, and (for a `Provider` error) the
    /// shared provider-error signals — mirroring the postgres native lock's
    /// `record_lock` so both natives emit the identical signal set (§8). Called by
    /// `try_lock`/`lock` and by the per-guard task's `renew`/`release`.
    fn record_lock<T>(
        &self,
        op: &'static str,
        lock: &str,
        started: std::time::Instant,
        outcome: &Result<T, ClusterError>,
    ) {
        self.metrics
            .lock_op_duration(op, started.elapsed().as_secs_f64());
        self.metrics.lock_op(op, result::label(outcome));
        if let Err(err) = outcome {
            observability::emit_provider_error(
                &*self.metrics,
                self.provider,
                op,
                ResourceId::Lock(lock),
                err,
            );
        }
    }

    async fn timed<T, F>(&self, ctx: &'static str, fut: F) -> Result<T, ClusterError>
    where
        F: std::future::Future<Output = Result<T, ClusterError>>,
    {
        match tokio::time::timeout(self.request_timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(k8s_error::timeout(ctx)),
        }
    }

    /// A fresh claim Lease for `coordination_name`, holder set to `token` (create
    /// path, no `resourceVersion`).
    fn new_claim(
        &self,
        coordination_name: &str,
        token: &HolderToken,
        ttl: Duration,
    ) -> Result<Lease, ClusterError> {
        let mut lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.lease_name(coordination_name)),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([
                    (LABEL_MANAGED_BY.to_owned(), MANAGED_BY_VALUE.to_owned()),
                    (
                        LABEL_PRIMITIVE.to_owned(),
                        Seg::Lock.primitive_label().to_owned(),
                    ),
                ])),
                annotations: Some(BTreeMap::from([(
                    ANNOTATION_NAME.to_owned(),
                    coordination_name.to_owned(),
                )])),
                ..ObjectMeta::default()
            },
            spec: Some(LeaseSpec::default()),
        };
        set_holder(&mut lease, token, ttl)?;
        Ok(lease)
    }
}

/// Stamps `lease` with the identifying labels the reaper's label-selector lists on
/// (§5.5) and the `name` annotation `lock_name_of` reads back for error messages
/// (§2.2). Idempotent; applied on both the create path (via `new_claim`) and the
/// claim/adopt path (via `guarded_claim`) so a Lease this plugin did not create still
/// becomes visible and correctly named once we claim it. Free-standing for the same
/// reason as `set_holder`.
fn set_identity(lease: &mut Lease, coordination_name: &str) {
    let labels = lease.metadata.labels.get_or_insert_with(BTreeMap::new);
    labels.insert(LABEL_MANAGED_BY.to_owned(), MANAGED_BY_VALUE.to_owned());
    labels.insert(
        LABEL_PRIMITIVE.to_owned(),
        Seg::Lock.primitive_label().to_owned(),
    );
    lease
        .metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(ANNOTATION_NAME.to_owned(), coordination_name.to_owned());
}

/// Stamps `lease` with `token` as holder, a fresh `renewTime`, the rounded-up
/// `leaseDurationSeconds`, and the exact `ttl-ms` annotation (§2.9). Free-standing
/// because it needs nothing from the runtime — the holder is the token, not the
/// backend's identity.
fn set_holder(lease: &mut Lease, token: &HolderToken, ttl: Duration) -> Result<(), ClusterError> {
    let ttl_millis = ttl_ms(ttl)?;
    lease
        .metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(ANNOTATION_TTL_MS.to_owned(), ttl_millis.to_string());
    let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
    spec.holder_identity = Some(token.to_holder_string());
    spec.lease_duration_seconds = Some(lease_duration_seconds(ttl)?);
    spec.renew_time = Some(now_micro());
    spec.acquire_time = Some(now_micro());
    Ok(())
}

/// The native Kubernetes distributed-lock backend (§5).
pub struct K8sLock {
    runtime: Arc<LockRuntime>,
    shutdown: CancellationToken,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl K8sLock {
    /// Builds a lock backend from a resolved client and the lock config (§3.5).
    ///
    /// # Errors
    ///
    /// [`ClusterError::InvalidConfig`] when `lease_prefix` is not a legal RFC 1123
    /// label (§2.2).
    pub fn new(
        resolved: &ResolvedClient,
        config: &K8sLockConfig,
        metrics: Arc<dyn ClusterMetrics>,
    ) -> Result<Self, ClusterError> {
        naming::validate_lease_prefix(&config.lease_prefix)?;
        let runtime = LockRuntime {
            client: resolved.client.clone(),
            namespace: resolved.namespace.clone(),
            identity: resolved.identity.clone(),
            lease_prefix: config.lease_prefix.clone(),
            metrics,
            provider: crate::provider::PROVIDER_NAME,
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            reaper_enabled: config.reaper,
            reaper_interval: Duration::from_millis(config.reaper_interval_ms),
            lock_object_retention: Duration::from_millis(config.lock_object_retention_ms),
            lock_name_cardinality_warn: config.lock_name_cardinality_warn_threshold,
            observed: DashMap::new(),
            waiters: Arc::new(LockWaiters::new()),
        };
        let backend = Self {
            runtime: Arc::new(runtime),
            shutdown: CancellationToken::new(),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        backend.spawn_reaper();
        Ok(backend)
    }

    /// Cancels the shutdown token and awaits the guard/reaper tasks (§11).
    pub async fn stop(&self) {
        self.shutdown.cancel();
        let handles = {
            let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *tasks)
        };
        for handle in handles {
            let _joined = handle.await;
        }
    }

    fn track(&self, handle: JoinHandle<()>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|h| !h.is_finished());
        tasks.push(handle);
    }

    /// Cancels the guard/reaper tasks synchronously, without awaiting them — the
    /// teardown the handle's `Drop` uses when `stop()` was never called and cannot
    /// `.await` (§11). A held lock's Lease is left to lapse on its own deadline.
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }

    /// Spawns the stale lock-object reaper if enabled (§5.5).
    fn spawn_reaper(&self) {
        if !self.runtime.reaper_enabled {
            return;
        }
        let handle = tokio::spawn(reaper::run_reaper(
            Arc::clone(&self.runtime),
            self.shutdown.clone(),
        ));
        self.track(handle);
    }

    /// One acquire attempt for `name` under `token` (§5.2). `Ok(Some(lease))` on a
    /// won claim (carrying the written object), `Ok(None)` on contention, `Err` on a
    /// backend fault.
    async fn try_acquire(
        &self,
        object_name: &str,
        coordination_name: &str,
        token: &HolderToken,
        ttl: Duration,
    ) -> Result<Option<Lease>, ClusterError> {
        let existing = self.runtime.read(object_name).await?;
        let Some(lease) = existing else {
            return self.create_claim(coordination_name, token, ttl).await;
        };

        let holder = lease
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.clone())
            .filter(|h| !h.is_empty());

        if holder.is_none() {
            // Free (cleared) object: claim it with a guarded replace.
            self.runtime.observed.remove(object_name);
            return self
                .guarded_claim(object_name, coordination_name, lease, token, ttl)
                .await;
        }

        // Held by someone: steal only once our own observation has aged past a full
        // TTL (§2.8). A single sighting is never enough. The relevant TTL is the
        // *current holder's* claim duration — read from the observed Lease's ttl-ms
        // annotation — not our own requested `ttl`: whether their claim has lapsed
        // depends on how long *they* held it for, and a long-TTL acquirer must not be
        // forced to wait out its own TTL to reclaim a short-lived lapsed claim.
        let holder_ttl = observed_ttl(&lease).unwrap_or(ttl);
        let record: Record = claim_record(&lease);
        let now = std::time::Instant::now();
        let expired = {
            let mut observed = self
                .runtime
                .observed
                .entry(object_name.to_owned())
                .or_insert_with(|| Observed::new(record.clone(), now));
            observed.observe(record, now);
            observed.is_expired(now, holder_ttl)
        };
        if expired {
            self.runtime.observed.remove(object_name);
            self.guarded_claim(object_name, coordination_name, lease, token, ttl)
                .await
        } else {
            Ok(None)
        }
    }

    /// Creates the Lease as ours; a `409 AlreadyExists` is contention this tick.
    async fn create_claim(
        &self,
        coordination_name: &str,
        token: &HolderToken,
        ttl: Duration,
    ) -> Result<Option<Lease>, ClusterError> {
        let lease = self.runtime.new_claim(coordination_name, token, ttl)?;
        let api = self.runtime.api();
        let created = self
            .runtime
            .timed(
                "create lock lease",
                guarded::create(&api, &lease, CallSite::LockAcquire),
            )
            .await?;
        Ok(match created {
            Created::Created(applied) => Some(*applied),
            Created::Exists => None,
        })
    }

    /// Guarded replace claiming a free/lapsed Lease; a `409` is contention (§5.2).
    async fn guarded_claim(
        &self,
        object_name: &str,
        coordination_name: &str,
        mut lease: Lease,
        token: &HolderToken,
        ttl: Duration,
    ) -> Result<Option<Lease>, ClusterError> {
        // Adopt-path parity with `new_claim`: an object this plugin did not create
        // (a foreign/legacy Lease occupying the same computed name) must still carry
        // the managed-by/primitive labels and the `name` annotation once we claim it,
        // or the reaper's label-selector list never sees it (a released object then
        // leaks) and `lock_name_of` reports the object name instead of the
        // coordination name in `LockExpired`/`LockTimeout` (§2.2, §5.5).
        set_identity(&mut lease, coordination_name);
        set_holder(&mut lease, token, ttl)?;
        let api = self.runtime.api();
        let replaced = self
            .runtime
            .timed(
                "claim lock lease",
                guarded::replace(&api, object_name, &lease, CallSite::LockAcquire),
            )
            .await?;
        Ok(match replaced {
            Replaced::Applied(applied) => Some(*applied),
            Replaced::Conflict(_) => None,
        })
    }

    /// Builds the guard for a won claim and spawns the task that services its
    /// renew/release commands (§5.4).
    fn spawn_guard(
        &self,
        name: &str,
        token: HolderToken,
        lease: Lease,
        ttl: Duration,
    ) -> LockGuard {
        let (commands, guard) = LockGuard::channel(name.to_owned(), GUARD_COMMAND_BUFFER);
        let task = GuardTask {
            runtime: Arc::clone(&self.runtime),
            object_name: self.runtime.lease_name(name),
            token,
            held: lease,
            deadline: Instant::now() + ttl,
            shutdown: self.shutdown.clone(),
        };
        self.track(tokio::spawn(task.run(commands)));
        guard
    }
}

#[async_trait]
impl DistributedLockBackend for K8sLock {
    /// Unconditionally linearizable (§3.7): a Lease guarded replace is Raft-arbitrated.
    fn features(&self) -> LockFeatures {
        LockFeatures::new(true)
    }

    fn provider_name(&self) -> &'static str {
        crate::provider::PROVIDER_NAME
    }

    async fn try_lock(&self, name: &str, ttl: Duration) -> Result<LockGuard, ClusterError> {
        let span = tracing::info_span!(spans::LOCK_TRY_LOCK, provider = %self.runtime.provider, lock = %name);
        let started = std::time::Instant::now();
        let out = async {
            if self.shutdown.is_cancelled() {
                return Err(ClusterError::Shutdown);
            }
            let object_name = self.runtime.lease_name(name);
            let token = HolderToken::generate(&self.runtime.identity);
            // Note: a contended `try_lock` deliberately leaves its `Observed` record in
            // place. Reclamation of a lapsed holder is driven by that record ageing
            // across repeated attempts (SC-LOCK-003 reclaims by polling `try_lock`), and
            // a stable lapsed holder keeps `seen_at` fixed by design — so an abandoned
            // record cannot be told apart from an actively-polled one, and evicting here
            // would break reclamation. The map is bounded by lock-name cardinality,
            // which the reaper already warns about (§5.5).
            match self.try_acquire(&object_name, name, &token, ttl).await? {
                Some(lease) => Ok(self.spawn_guard(name, token, lease, ttl)),
                None => Err(ClusterError::LockContended {
                    name: name.to_owned(),
                }),
            }
        }
        .instrument(span)
        .await;
        self.runtime.record_lock("try_lock", name, started, &out);
        out
    }

    async fn lock(
        &self,
        name: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        let span =
            tracing::info_span!(spans::LOCK_LOCK, provider = %self.runtime.provider, lock = %name);
        let started = std::time::Instant::now();
        let out = self.lock_inner(name, ttl, timeout).instrument(span).await;
        self.runtime.record_lock("lock", name, started, &out);
        out
    }
}

impl K8sLock {
    /// The uninstrumented blocking-acquire loop that [`lock`](Self::lock) spans and
    /// measures (§5.3).
    async fn lock_inner(
        &self,
        name: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        if self.shutdown.is_cancelled() {
            return Err(ClusterError::Shutdown);
        }
        let object_name = self.runtime.lease_name(name);
        let deadline = Instant::now() + timeout;

        // Subscribe to the shared waiter registry *before* the first attempt so a
        // release landing between "we saw it held" and "we subscribed" cannot be
        // missed (§5.3). The first subscriber spawns the shared watch and hands its
        // handle to the registry, which aborts it when the last waiter leaves.
        let (notify, first) = self.runtime.waiters.subscribe(name);
        if first {
            let handle = self.spawn_lock_watch(name, object_name.clone());
            self.runtime.waiters.attach_watch(name, handle);
        }
        let _guard = WaiterGuard {
            waiters: Arc::clone(&self.runtime.waiters),
            name: name.to_owned(),
        };

        // Arm the release notification once, before the first attempt, and keep it
        // armed across the loop: `wake()` uses `notify_waiters` (which stores no
        // permit), so a release landing between an attempt and the await is lost
        // unless the `Notified` is already registered. `enable()` at the top of each
        // iteration registers it before `try_acquire` reads the lock's state.
        let notified = notify.notified();
        tokio::pin!(notified);

        loop {
            // Re-check shutdown *before* the acquire: the `select!` below can wake on
            // `shutdown.cancelled()`, and without this a re-entered loop would issue
            // another `try_acquire` first — which could return `Ok(guard)` during
            // shutdown (the contract requires `Shutdown`) and spawn a `GuardTask` on
            // an already-cancelled token, leaving the consumer a guard whose commands
            // have no receiver and a Lease held until its TTL lapses (§5.4).
            if self.shutdown.is_cancelled() {
                return Err(ClusterError::Shutdown);
            }
            notified.as_mut().enable();
            let token = HolderToken::generate(&self.runtime.identity);
            if let Some(lease) = self.try_acquire(&object_name, name, &token, ttl).await? {
                return Ok(self.spawn_guard(name, token, lease, ttl));
            }
            let now = Instant::now();
            match classify_wait(self.shutdown.is_cancelled(), now >= deadline) {
                WaitDecision::Shutdown => return Err(ClusterError::Shutdown),
                WaitDecision::Timeout => {
                    return Err(ClusterError::LockTimeout {
                        name: name.to_owned(),
                        waited: timeout,
                    });
                }
                WaitDecision::Keep => {}
            }
            // Wait for a release/change, our budget, or shutdown — whichever first.
            let remaining = deadline.saturating_duration_since(now);
            tokio::select! {
                () = &mut notified => notified.set(notify.notified()),
                () = tokio::time::sleep(remaining) => {}
                () = self.shutdown.cancelled() => {}
            }
        }
    }
}

impl K8sLock {
    /// Spawns the shared watch feeding [`LockWaiters::wake`] for `name` (§5.3).
    fn spawn_lock_watch(&self, name: &str, object_name: String) -> JoinHandle<()> {
        let runtime = Arc::clone(&self.runtime);
        let name = name.to_owned();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let api = runtime.api();
            let wc = watcher::Config::default().fields(&format!("metadata.name={object_name}"));
            let stream = watcher(api, wc);
            tokio::pin!(stream);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    event = stream.next() => {
                        match event {
                            // Any observed change (holder cleared/changed, deletion)
                            // may mean the lock became takeable: wake the waiters.
                            Some(Ok(_)) => runtime.waiters.wake(&name),
                            // `kube` retries internally, so an error item is transient
                            // (a blocked `lock()` still wakes via its budget or the next
                            // event); log it so a persistent failure is diagnosable.
                            Some(Err(err)) => tracing::warn!(
                                error = %err, lock = %name,
                                "cluster.provider.lock_watch_error: the lock-release watcher \
                                 stream returned an error; kube retries internally"
                            ),
                            None => return,
                        }
                        if !runtime.waiters.has_waiters(&name) {
                            return;
                        }
                    }
                }
            }
        })
    }
}

/// Releases a blocking `lock()`'s waiter subscription on every exit path. The shared
/// watch is owned by [`LockWaiters`] and aborted there on the last unsubscribe, so
/// this guard only needs to deregister.
struct WaiterGuard {
    waiters: Arc<LockWaiters>,
    name: String,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        // `unsubscribe` aborts the shared watch when this is the last waiter.
        let _last = self.waiters.unsubscribe(&self.name);
    }
}

/// The task servicing one held lock's [`LockGuard`] commands (§5.4). Parked on
/// `recv` — no renewal loop, no connection (§3.3) — until a release or the consumer
/// drops the guard.
struct GuardTask {
    runtime: Arc<LockRuntime>,
    object_name: String,
    token: HolderToken,
    /// The last-written Lease (its `resourceVersion` for the next guarded write).
    held: Lease,
    /// The monotonic deadline our claim is valid until (§2.8).
    deadline: Instant,
    shutdown: CancellationToken,
}

impl GuardTask {
    async fn run(mut self, mut commands: LockCommandReceiver) {
        loop {
            tokio::select! {
                // Shutdown leaves the Lease exactly as it is — it lapses on its own
                // deadline, so a restart under a held lock revokes nothing (§11).
                () = self.shutdown.cancelled() => return,
                request = commands.recv() => {
                    match request {
                        Some(LockRequest::Renew { new_ttl, responder }) => {
                            let name = lock_name_of(&self.held);
                            let span = tracing::info_span!(
                                spans::LOCK_RENEW, provider = %self.runtime.provider, lock = %name
                            );
                            let started = std::time::Instant::now();
                            let out = self.renew(new_ttl).instrument(span).await;
                            self.runtime.record_lock("renew", &name, started, &out);
                            responder.respond(out);
                        }
                        Some(LockRequest::Release { responder }) => {
                            let name = lock_name_of(&self.held);
                            let span = tracing::info_span!(
                                spans::LOCK_RELEASE, provider = %self.runtime.provider, lock = %name
                            );
                            let started = std::time::Instant::now();
                            let out = self.release().instrument(span).await;
                            self.runtime.record_lock("release", &name, started, &out);
                            responder.respond(out);
                            return;
                        }
                        // Guard dropped without releasing: exit, leave the Lease to
                        // lapse via TTL (§5.2).
                        None => return,
                    }
                }
            }
        }
    }

    /// Token-fenced renew (§5.4): reset `renewTime`/TTL if we still hold and our
    /// deadline has not passed, else [`ClusterError::LockExpired`].
    async fn renew(&mut self, new_ttl: Duration) -> Result<(), ClusterError> {
        if self.deadline <= Instant::now() {
            return Err(ClusterError::LockExpired {
                name: lock_name_of(&self.held),
            });
        }
        let mut lease = self.held.clone();
        set_holder(&mut lease, &self.token, new_ttl)?;
        let api = self.runtime.api();
        let replaced = self
            .runtime
            .timed(
                "renew lock lease",
                guarded::replace(&api, &self.object_name, &lease, CallSite::LockRenew),
            )
            .await?;
        match replaced {
            Replaced::Applied(applied) => {
                self.held = *applied;
                self.deadline = Instant::now() + new_ttl;
                Ok(())
            }
            // A 409 means our lease moved on — a successor stole it after our lapse.
            Replaced::Conflict(_) => Err(ClusterError::LockExpired {
                name: lock_name_of(&self.held),
            }),
        }
    }

    /// Token-fenced release (§5.4, §5.5): clear the holder if we still hold within
    /// our deadline; a lapsed/foreign claim is left untouched. Never deletes.
    async fn release(&mut self) -> Result<(), ClusterError> {
        // Our deadline passed: the fleet may treat the lock as free, so do not write
        // — a successor may already hold it (§5.4).
        if self.deadline <= Instant::now() {
            return Ok(());
        }
        let mut lease = self.held.clone();
        if let Some(spec) = lease.spec.as_mut() {
            spec.holder_identity = None;
            spec.renew_time = Some(now_micro());
        }
        let api = self.runtime.api();
        let replaced = self
            .runtime
            .timed(
                "release lock lease",
                guarded::replace(&api, &self.object_name, &lease, CallSite::Release),
            )
            .await?;
        match replaced {
            // Applied, or a 409 (a successor took over): the postcondition holds.
            Replaced::Applied(_) | Replaced::Conflict(_) => Ok(()),
        }
    }
}

/// The current wall-clock as a `MicroTime` (output for readers, never input to
/// expiry — §2.8).
fn now_micro() -> MicroTime {
    MicroTime(k8s_openapi::jiff::Timestamp::now())
}

/// The current holder's claim duration, read from the Lease's ttl-ms annotation
/// (the exact millisecond TTL, §2.9). `None` when the annotation is absent or
/// unparseable — a foreign/legacy holder — so the caller falls back to its own TTL.
fn observed_ttl(lease: &Lease) -> Option<Duration> {
    lease
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_TTL_MS))
        .and_then(|ms| ms.parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// The `(holderIdentity, renewTime)` record for `Observed` equality (§2.8).
fn claim_record(lease: &Lease) -> Record {
    let holder = lease.spec.as_ref().and_then(|s| s.holder_identity.clone());
    let renew = lease
        .spec
        .as_ref()
        .and_then(|spec| spec.renew_time.as_ref())
        .map(|t| t.0.to_string());
    (holder, renew)
}

/// The unmapped coordination name from a Lease's annotation, for error messages
/// (falls back to the object name when the annotation is absent).
fn lock_name_of(lease: &Lease) -> String {
    lease
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_NAME))
        .cloned()
        .or_else(|| lease.metadata.name.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{HolderToken, WaitDecision, classify_wait};

    #[test]
    fn holder_token_round_trips_through_the_wire_form() {
        let token = HolderToken::generate("broker-7");
        let wire = token.to_holder_string();
        assert!(wire.starts_with("broker-7#"));
        assert_eq!(HolderToken::parse(&wire), Some(token));
    }

    #[test]
    fn holder_token_splits_on_the_last_hash() {
        // An identity that itself contains '#' round-trips, because parse splits on
        // the final '#' (the UUID never contains one).
        let token = HolderToken {
            identity: "team#broker-7".to_owned(),
            uuid: "abc-123".to_owned(),
        };
        let parsed = HolderToken::parse(&token.to_holder_string()).unwrap();
        assert_eq!(parsed.identity, "team#broker-7");
        assert_eq!(parsed.uuid, "abc-123");
    }

    #[test]
    fn a_holder_without_a_hash_is_not_our_token() {
        assert_eq!(HolderToken::parse("plain-identity"), None);
        // A trailing '#' with no uuid is also rejected.
        assert_eq!(HolderToken::parse("id#"), None);
    }

    #[test]
    fn two_tokens_for_one_identity_are_distinct() {
        // The fence: two acquisitions never share a token (§5.1).
        assert_ne!(HolderToken::generate("me"), HolderToken::generate("me"));
    }

    #[test]
    fn wait_prefers_shutdown_then_timeout_then_keep() {
        // Shutdown wins even past the deadline — a going-down plugin must not hand
        // back a retryable timeout.
        assert_eq!(classify_wait(true, true), WaitDecision::Shutdown);
        assert_eq!(classify_wait(true, false), WaitDecision::Shutdown);
        // Not shutting down, budget elapsed → timeout.
        assert_eq!(classify_wait(false, true), WaitDecision::Timeout);
        // Still within budget → keep waiting.
        assert_eq!(classify_wait(false, false), WaitDecision::Keep);
    }
}
