//! The CAS-based default distributed-lock backend over `Arc<dyn ClusterCacheBackend>`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::defaults::lease::{Acquisition, CacheLeaseStore};
use crate::defaults::{LOCK_KEY_PREFIX, ShutdownRevoke, guard, identity};
use cluster_sdk::cache::{CacheWatch, CacheWatchEvent, ClusterCacheBackend};
use cluster_sdk::error::{ClusterError, ProviderErrorKind};
use cluster_sdk::lease::LeaseToken;
use cluster_sdk::lock::{
    DistributedLockBackend, LockCommandReceiver, LockFeatures, LockGuard, LockRequest,
};
use cluster_sdk::observability::{self, ClusterMetrics, NoopMetrics, result, spans};

/// Records the metric side of a finished lock op (duration + bounded-`result`
/// counter) and the shared provider-error signals. Used by both the backend
/// (`try_lock`/`lock`) and the per-guard task (`renew`/`release`).
fn record_lock<T>(
    metrics: &dyn ClusterMetrics,
    provider: &'static str,
    op: &'static str,
    lock: &str,
    started: std::time::Instant,
    outcome: &Result<T, ClusterError>,
) {
    metrics.lock_op_duration(op, started.elapsed().as_secs_f64());
    metrics.lock_op(op, result::label(outcome));
    if let Err(err) = outcome {
        observability::emit_provider_error(
            metrics,
            provider,
            op,
            observability::ResourceId::Lock(lock),
            err,
        );
    }
}

/// The in-flight command buffer for each [`LockGuard`].
const COMMAND_BUFFER: usize = 4;

/// The maximum number of consecutive *immediately*-ended watch re-subscribes a
/// blocking [`lock`](CasBasedDistributedLockBackend::lock) tolerates before
/// treating the backend watch as structurally unusable. Bounds a busy-spin
/// against a backend that hands back a watch yielding `None` at once.
const MAX_CONSECUTIVE_WATCH_RESETS: u32 = 8;

/// Minimal backoff applied before re-subscribing to a watch that ended
/// immediately, so a busy-spin cannot burn CPU before the cap (or the caller's
/// timeout) fires. It doubles as the "immediate" threshold: a watch that lived
/// at least this long before ending is treated as a legitimate stream rotation
/// rather than a busy-spin, so the acquisition keeps waiting.
///
/// This boundary is a **heuristic**, not a guarantee: a backend that legitimately
/// rotates its watch faster than this is classified as a busy-spin (the backoff
/// still prevents CPU spin, but the unusable-watch cap may eventually fire).
const WATCH_RESUBSCRIBE_BACKOFF: Duration = Duration::from_millis(50);

/// The mean poll cadence a blocking acquisition falls back to when the cache
/// backend serves no exact watch (`features().watch == false`, e.g. redis
/// `watch_mode: disabled`): with no event to wake on, it re-attempts the claim
/// around this interval (jittered by [`watchless_poll_interval`]) instead. Capped
/// by the caller's remaining budget at the call site, so a tight `timeout` still
/// fires on time (plan D3).
const WATCHLESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A jittered watchless poll interval drawn uniformly from
/// `[WATCHLESS_POLL_INTERVAL, 2 * WATCHLESS_POLL_INTERVAL)` — a floor of one full
/// interval (so a poll never degenerates into a spin) plus up to one interval of
/// jitter, so multiple waiters contending the same lock over a watchless cache
/// desynchronize their claim attempts rather than retrying in lockstep and
/// hammering the backend with synchronized CAS bursts. Mirrors the leader
/// backend's `reclaim_jitter`; `ThreadRng` is entropy-seeded, so concurrent
/// participants diverge with no explicit seeding. The caller clamps the result to
/// its remaining `wait` budget.
fn watchless_poll_interval() -> Duration {
    use rand::RngExt as _;
    let base_nanos = u64::try_from(WATCHLESS_POLL_INTERVAL.as_nanos()).unwrap_or(u64::MAX);
    // `base + U(0, base)` == `U(base, 2*base)`. `.max(1)` keeps the range
    // non-empty for a hypothetical zero base.
    let jitter = rand::rng().random_range(0..base_nanos.max(1));
    Duration::from_nanos(base_nanos.saturating_add(jitter))
}

/// The outcome of one wait step in [`wait_for_lease`](CasBasedDistributedLockBackend::wait_for_lease):
/// either the watch resolved, or — with no watch — a bounded poll timer did.
enum WaitStep {
    /// The `wait` budget elapsed with no actionable event (a watch timeout, or a
    /// watchless poll that consumed the whole budget).
    Elapsed,
    /// A terminal watch close carrying the backend error.
    Closed(ClusterError),
    /// A watch event, or a watchless poll tick → re-attempt the claim.
    Retry,
    /// The watch stream ended without a terminal close → re-subscribe.
    StreamEnded,
}

/// A distributed-lock backend that derives TTL-bounded mutual exclusion from
/// cache compare-and-swap operations (DESIGN §3.11, ADR-001).
///
/// # A lock is a store-owned lease (§5.8.1)
///
/// Acquisition writes a [`LeaseRecord`](cluster_sdk::lease::LeaseRecord) —
/// `{ owner, deadline, fence }` — under `lock/{name}` and returns the
/// [`LeaseToken`] that is the whole authority over it. `renew` and `release` are
/// conditional writes predicated on that token and on nothing this process
/// remembers, which lets a lease be renewed through a *different* backend
/// handle than the one that acquired it, and therefore through a different cluster
/// replica (invariant I7).
///
/// Both halves of the trait are served from one lease. The token half
/// ([`acquire`](DistributedLockBackend::acquire) and friends) is the primitive;
/// [`try_lock`](DistributedLockBackend::try_lock) and
/// [`lock`](DistributedLockBackend::lock) acquire the same lease and hand back a
/// [`LockGuard`] whose task holds the token, because the guard's fields are private
/// and cannot carry one (§6.5).
///
/// A blocking [`lock`](DistributedLockBackend::lock) subscribes to a `watch` on the
/// key and retries on each event until it acquires or the timeout elapses. It also
/// wakes itself at the incumbent's `deadline`: a lease lapsing writes nothing, so
/// unlike physical TTL expiry it produces no watch event (see
/// [`lease`](crate::defaults::lease)).
///
/// A crashed holder's lock lapses at its `deadline` and is then *stolen* by the
/// next acquirer at `fence + 1` — there is no auto-renewal, and the fence stays
/// internal rather than becoming a consumer-facing fencing token (§5.8.1, ADR-002).
/// A long critical section refreshes its lease via [`LockGuard::renew`].
///
/// # Consistency safety (ADR-009)
///
/// Correctness-grade exclusion holds only over a **linearizable** cache.
/// Construct with [`new`](Self::new) (default-safe) or
/// [`new_allow_weak_consistency`](Self::new_allow_weak_consistency) to accept
/// the split-brain risk. [`features`](DistributedLockBackend::features) derives
/// `linearizable` from the underlying cache's consistency.
pub struct CasBasedDistributedLockBackend {
    leases: Arc<CacheLeaseStore>,
    /// Cancelled by [`ShutdownRevoke::revoke`] to signal an in-flight blocking
    /// [`lock`](Self::lock) waiter to return [`ClusterError::Shutdown`] promptly
    /// on graceful shutdown (DESIGN §3.13). The waiter runs in the caller's
    /// future (not a spawned task), so there is no task set to await.
    shutdown: CancellationToken,
    /// The bounded `provider` label for emitted signals (default `"unknown"`
    /// until set via [`with_observability`](Self::with_observability)).
    provider: &'static str,
    /// The metrics sink (default [`NoopMetrics`]).
    metrics: Arc<dyn ClusterMetrics>,
}

impl CasBasedDistributedLockBackend {
    const NAME: &'static str = "CasBasedDistributedLockBackend";

    /// Creates a default-safe backend over `cache`.
    ///
    /// # Errors
    /// Returns [`ClusterError::InvalidConfig`] when `cache` declares
    /// [`CacheConsistency::EventuallyConsistent`](cluster_sdk::cache::CacheConsistency),
    /// because correctness-grade exclusion requires linearizable CAS.
    pub fn new(cache: Arc<dyn ClusterCacheBackend>) -> Result<Self, ClusterError> {
        guard::reject_weak_consistency(cache.consistency(), Self::NAME)?;
        Ok(Self::with_cache(cache))
    }

    /// Creates a backend over `cache`, bypassing the consistency guard.
    ///
    /// Always succeeds and emits a `tracing::warn!` acknowledging the
    /// split-brain risk (two holders may transiently acquire the same lock under
    /// partition). Use only when the cache is intentionally eventually
    /// consistent and the consumer accepts that risk (ADR-009).
    #[must_use]
    pub fn new_allow_weak_consistency(cache: Arc<dyn ClusterCacheBackend>) -> Self {
        guard::warn_weak_consistency(cache.consistency(), Self::NAME);
        Self::with_cache(cache)
    }

    fn with_cache(cache: Arc<dyn ClusterCacheBackend>) -> Self {
        // Tell the operator once, at construction, if this cache cannot serve
        // exact watch: a blocking `lock()` then waits by bounded polling rather
        // than reactively (see `wait_for_lease`). Not fatal.
        guard::warn_without_watch(cache.features(), Self::NAME);
        Self {
            leases: Arc::new(CacheLeaseStore::new(cache)),
            shutdown: CancellationToken::new(),
            provider: "unknown",
            metrics: Arc::new(NoopMetrics),
        }
    }

    /// The cache the lease records live in, for the watch a blocking acquisition
    /// waits on.
    fn cache(&self) -> &Arc<dyn ClusterCacheBackend> {
        self.leases.cache()
    }

    /// Subscribes the blocking-acquisition watch, degrading to poll-only
    /// (`None`) when the backend serves no exact watch.
    ///
    /// `Unsupported` is a *permanent* capability answer, not a retryable close,
    /// so it is not surfaced as an error: the caller falls back to bounded
    /// polling. Any other error propagates.
    ///
    /// # Errors
    /// Returns a non-`Unsupported` [`ClusterError`] if the subscribe fails.
    async fn resubscribe(&self, key: &str) -> Result<Option<CacheWatch>, ClusterError> {
        match self.cache().watch(key).await {
            Ok(watch) => Ok(Some(watch)),
            Err(ClusterError::Unsupported { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Sets the `provider` label and metrics sink the backend emits through.
    ///
    /// Called by the wrapping plugin so emitted signals carry the deployment's
    /// provider name (ADR-004). Without it, signals use `provider = "unknown"`
    /// and a no-op sink.
    #[must_use]
    pub fn with_observability(
        mut self,
        provider: &'static str,
        metrics: Arc<dyn ClusterMetrics>,
    ) -> Self {
        self.provider = provider;
        self.metrics = metrics;
        self
    }

    /// Sets how long a lease record outlives the lease it fenced (§5.8.1).
    ///
    /// Additive rather than a third constructor: ADR-009's constructor *pair* is
    /// the consistency guard, and adding retention arguments to both halves would
    /// double a surface whose whole point is that there are exactly two ways in.
    /// The wiring calls this with the cluster gear's `fence_retention`; without
    /// it the backend keeps
    /// [`FENCE_RETENTION_DEFAULT`](cluster_sdk::lease::FENCE_RETENTION_DEFAULT).
    ///
    /// Safe to call after construction because neither constructor spawns
    /// anything: the guard tasks start per acquisition, so no task can be holding
    /// the store this replaces.
    #[must_use]
    pub fn with_fence_retention(mut self, retention: Duration) -> Self {
        self.leases = Arc::new(CacheLeaseStore::with_retention(
            Arc::clone(self.leases.cache()),
            retention,
        ));
        self
    }

    /// **Test-only.** Rebuilds the lease store on a virtual clock that tracks
    /// [`tokio::time::pause`]/`advance`, so a test (including the gear's
    /// `TimeControl::Virtual` conformance run) can lapse a lock's TTL under virtual
    /// time (§5.8.1).
    ///
    /// Production must never call this: [`new`](Self::new) and
    /// [`new_allow_weak_consistency`](Self::new_allow_weak_consistency) build the
    /// pure wall clock, which cannot diverge across a wall step. Named and
    /// documented as test-support, in the same spirit as
    /// [`reserved_lease_cache`](cluster_sdk::reserved_lease_cache) — a `pub` seam
    /// the conformance harness reaches from outside the crate's unit-test cfg.
    /// Call before any acquisition (the guard tasks start per acquisition, so no
    /// task can be holding the store this replaces).
    #[must_use]
    pub fn with_virtual_clock(mut self) -> Self {
        use rand::RngExt as _;
        self.leases = Arc::new(CacheLeaseStore::with_virtual_clock(
            Arc::clone(self.leases.cache()),
            self.leases.retention(),
            rand::rng().random::<u64>(),
        ));
        self
    }

    /// The cache key a named lock claims. Prefixed so a lock does not collide
    /// with a same-named election when both defaults share one cache.
    fn lock_key(name: &str) -> String {
        format!("{LOCK_KEY_PREFIX}{name}")
    }

    /// Spawns the guard's command task and returns the consumer-facing guard.
    ///
    /// The spawned [`GuardTask`] is **intentionally** tied to the lifetime of the
    /// consumer-held [`LockGuard`], not to backend [`revoke`](ShutdownRevoke::revoke):
    /// it self-terminates when the consumer drops the guard (its command channel
    /// closes) and is deliberately not cancelled on graceful shutdown. A
    /// `revoke`-driven cancellation would yank a lease out from under a consumer
    /// still inside its critical section; instead the held lease is the safety net
    /// and lapses via TTL (`cpt-cf-clst-fr-shutdown-ttl-cleanup`). The task is
    /// bounded — at most one per held guard.
    fn spawn_guard(&self, key: String, token: LeaseToken) -> LockGuard {
        let (receiver, guard) = LockGuard::channel(token.name.clone(), COMMAND_BUFFER);
        let task = GuardTask {
            leases: Arc::clone(&self.leases),
            key,
            token,
            provider: self.provider,
            metrics: Arc::clone(&self.metrics),
        };
        tokio::spawn(task.run(receiver));
        guard
    }

    /// The acquisition both [`try_lock`](DistributedLockBackend::try_lock) and
    /// [`acquire`](DistributedLockBackend::acquire) run: one insert-or-steal
    /// attempt, contention reported as [`ClusterError::LockContended`].
    ///
    /// Instrumented here rather than at each caller so the guard-returning and
    /// token-returning halves share one span and one metric series — they are the
    /// same operation, and `op` is a bounded label (invariant I15).
    async fn acquire_lease(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
    ) -> Result<LeaseToken, ClusterError> {
        let span =
            tracing::info_span!(spans::LOCK_TRY_LOCK, provider = %self.provider, lock = %name);
        let op_started = std::time::Instant::now();
        let out = async {
            let key = Self::lock_key(name);
            match self.leases.try_acquire(&key, name, owner, ttl).await? {
                Acquisition::Acquired(token) => Ok(token),
                Acquisition::Contended { .. } => Err(ClusterError::LockContended {
                    name: name.to_owned(),
                }),
            }
        }
        .instrument(span)
        .await;
        record_lock(
            &*self.metrics,
            self.provider,
            "try_lock",
            name,
            op_started,
            &out,
        );
        out
    }

    /// The acquisition both [`lock`](DistributedLockBackend::lock) and
    /// [`acquire_waiting`](DistributedLockBackend::acquire_waiting) run: retry the
    /// steal until it lands or `timeout` elapses.
    ///
    /// Each wait ends on whichever comes first — a watch event, the incumbent
    /// lease's `deadline`, the caller's `timeout`, or graceful shutdown. The
    /// deadline arm is what a physical-TTL lock did not need: a lapsing lease writes
    /// nothing, so no watch event announces it.
    async fn acquire_lease_waiting(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LeaseToken, ClusterError> {
        let span = tracing::info_span!(spans::LOCK_LOCK, provider = %self.provider, lock = %name);
        let op_started = std::time::Instant::now();
        let out = self
            .wait_for_lease(name, owner, ttl, timeout)
            .instrument(span)
            .await;
        record_lock(
            &*self.metrics,
            self.provider,
            "lock",
            name,
            op_started,
            &out,
        );
        out
    }

    /// The uninstrumented wait loop [`acquire_lease_waiting`](Self::acquire_lease_waiting)
    /// spans and measures.
    async fn wait_for_lease(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LeaseToken, ClusterError> {
        let key = Self::lock_key(name);
        let started = tokio::time::Instant::now();
        // Subscribe before the first attempt so a release between a failed claim
        // and the wait cannot be missed. A backend with no exact-watch support
        // (`features().watch == false`) yields `None`, and the wait falls back to
        // bounded polling — so a blocking `lock()` still completes over e.g. redis
        // `watch_mode: disabled` instead of failing at the first wait (plan D3).
        let mut watch = self.resubscribe(&key).await?;
        // Distinguish a busy-spin from a legitimate stream rotation. A watch that
        // ends (`recv` → `None`) *immediately* on every re-subscribe would spin
        // this loop hot (claim → watch ends → re-subscribe → claim …); a watch
        // that lived for a meaningful interval before ending is a normal
        // end-of-stream and the acquisition should keep waiting, bounded only by
        // `timeout`. Only consecutive *immediate* re-ends count toward the cap.
        let mut consecutive_immediate_resets: u32 = 0;
        // Cloned to a local so the `cancelled()` future in the wait `select!`
        // below does not borrow `self`.
        let shutdown = self.shutdown.clone();
        loop {
            // Graceful cluster shutdown observed before the next claim attempt:
            // abandon the wait promptly with a terminal `Shutdown` rather than
            // racing another claim against a backend that is tearing down.
            if shutdown.is_cancelled() {
                return Err(ClusterError::Shutdown);
            }
            let lapse_in = match self.leases.try_acquire(&key, name, owner, ttl).await? {
                Acquisition::Acquired(token) => return Ok(token),
                Acquisition::Contended { lapse_in } => lapse_in,
            };
            // Treat an exhausted *or zero* budget as a timeout: a zero remaining
            // would otherwise let an always-ready (e.g. closed) watch spin the
            // loop at no time cost until the cap, reporting an unusable-watch
            // error where the caller's deadline is the real binding constraint.
            let Some(remaining) = timeout
                .checked_sub(started.elapsed())
                .filter(|r| !r.is_zero())
            else {
                return Err(ClusterError::LockTimeout {
                    name: name.to_owned(),
                    waited: started.elapsed(),
                });
            };
            // Never wait past the point where the lease becomes stealable. Capped
            // by `remaining` so a lease outliving the caller's patience still
            // times out on time.
            let wait = lapse_in.map_or(remaining, |lapse| remaining.min(lapse));
            let recv_started = tokio::time::Instant::now();
            // With a watch, wait up to `wait` for its next event. Without one
            // (the backend serves no exact watch), poll the claim on a bounded,
            // jittered interval, still capped by `wait` so a tight `timeout`
            // fires on time.
            let step = tokio::select! {
                // Graceful cluster shutdown: abandon the wait promptly with a
                // terminal `Shutdown` (`cpt-cf-clst-fr-shutdown-revoke`). Held
                // locks lapse via their deadline; this only resolves an in-flight
                // wait.
                () = shutdown.cancelled() => return Err(ClusterError::Shutdown),
                step = async {
                    if let Some(w) = watch.as_mut() {
                        match tokio::time::timeout(wait, w.recv()).await {
                            Err(_elapsed) => WaitStep::Elapsed,
                            Ok(Some(CacheWatchEvent::Closed(err))) => WaitStep::Closed(err),
                            Ok(Some(_)) => WaitStep::Retry,
                            Ok(None) => WaitStep::StreamEnded,
                        }
                    } else {
                        // Watchless: sleep a jittered, bounded poll interval so
                        // contending waiters on the same lock desynchronize their
                        // claims. Clamped to `wait`; if that clamp consumed the
                        // whole `wait` budget it is an elapse (a real timeout or
                        // the incumbent's deadline), otherwise it is just a poll
                        // tick → retry the claim.
                        let poll = watchless_poll_interval().min(wait);
                        tokio::time::sleep(poll).await;
                        if poll == wait { WaitStep::Elapsed } else { WaitStep::Retry }
                    }
                } => step,
            };
            match step {
                // The wait budget ran out. If that was the caller's `timeout` it is
                // a real timeout; if it was the incumbent's deadline, loop and steal.
                WaitStep::Elapsed => {
                    if wait == remaining {
                        return Err(ClusterError::LockTimeout {
                            name: name.to_owned(),
                            waited: started.elapsed(),
                        });
                    }
                    consecutive_immediate_resets = 0;
                }
                WaitStep::Closed(err) => return Err(err),
                // Any event (release / expiry / lag / reset) or a poll tick →
                // retry the claim.
                WaitStep::Retry => consecutive_immediate_resets = 0,
                // End-of-stream (sender dropped without a terminal `Closed`).
                // Re-subscribe to keep waiting within the remaining timeout.
                WaitStep::StreamEnded if recv_started.elapsed() >= WATCH_RESUBSCRIBE_BACKOFF => {
                    // The watch lived a meaningful interval: a legitimate
                    // rotation, not a busy-spin. Keep waiting.
                    consecutive_immediate_resets = 0;
                    watch = self.resubscribe(&key).await?;
                }
                WaitStep::StreamEnded => {
                    // Ended immediately: a busy-spin symptom. Cap consecutive
                    // immediate re-ends so a structurally unusable watch surfaces
                    // instead of spinning, and back off so it cannot burn CPU
                    // before the cap (or the timeout) fires.
                    consecutive_immediate_resets += 1;
                    if consecutive_immediate_resets >= MAX_CONSECUTIVE_WATCH_RESETS {
                        tracing::warn!(
                            lock = name,
                            immediate_resubscribes = MAX_CONSECUTIVE_WATCH_RESETS,
                            "distributed-lock backend watch ended immediately on every \
                             re-subscribe; treating it as structurally unusable for blocking \
                             acquisition and aborting the wait"
                        );
                        return Err(ClusterError::Provider {
                            kind: ProviderErrorKind::Other,
                            message: format!(
                                "distributed-lock backend watch for `{name}` ended immediately \
                                 {MAX_CONSECUTIVE_WATCH_RESETS} times in a row; the watch is \
                                 unusable for blocking acquisition"
                            ),
                        });
                    }
                    // Clamp to the remaining wait so a tight `timeout` is not
                    // overshot by a full backoff interval.
                    tokio::time::sleep(WATCH_RESUBSCRIBE_BACKOFF.min(remaining)).await;
                    watch = self.resubscribe(&key).await?;
                }
            }
        }
    }
}

#[async_trait]
impl DistributedLockBackend for CasBasedDistributedLockBackend {
    fn features(&self) -> LockFeatures {
        LockFeatures::new(
            self.cache().consistency() == cluster_sdk::cache::CacheConsistency::Linearizable,
        )
    }

    async fn try_lock(&self, name: &str, ttl: Duration) -> Result<LockGuard, ClusterError> {
        // A fresh owner id per acquisition rather than one per process: two guards
        // held concurrently in this process are then distinct owners, so neither
        // can renew or release the other's lease. Remotely the owner is the
        // caller's `ClientId` (§5.4), supplied by the serving gear.
        let owner = identity::fresh_id();
        let token = self.acquire_lease(name, &owner, ttl).await?;
        Ok(self.spawn_guard(Self::lock_key(name), token))
    }

    async fn lock(
        &self,
        name: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        let owner = identity::fresh_id();
        let token = self
            .acquire_lease_waiting(name, &owner, ttl, timeout)
            .await?;
        Ok(self.spawn_guard(Self::lock_key(name), token))
    }

    async fn acquire(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
    ) -> Result<LeaseToken, ClusterError> {
        self.acquire_lease(name, owner, ttl).await
    }

    async fn acquire_waiting(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LeaseToken, ClusterError> {
        self.acquire_lease_waiting(name, owner, ttl, timeout).await
    }

    async fn renew(&self, token: &LeaseToken, ttl: Duration) -> Result<(), ClusterError> {
        let key = Self::lock_key(&token.name);
        let span = tracing::info_span!(
            spans::LOCK_RENEW,
            provider = %self.provider,
            lock = %token.name
        );
        let op_started = std::time::Instant::now();
        let out = self.leases.renew(&key, token, ttl).instrument(span).await;
        record_lock(
            &*self.metrics,
            self.provider,
            "renew",
            &token.name,
            op_started,
            &out,
        );
        out
    }

    async fn release(&self, token: &LeaseToken) -> Result<(), ClusterError> {
        let key = Self::lock_key(&token.name);
        let span = tracing::info_span!(
            spans::LOCK_RELEASE,
            provider = %self.provider,
            lock = %token.name
        );
        let op_started = std::time::Instant::now();
        let out = self.leases.release(&key, token).instrument(span).await;
        record_lock(
            &*self.metrics,
            self.provider,
            "release",
            &token.name,
            op_started,
            &out,
        );
        out
    }
}

#[async_trait]
impl ShutdownRevoke for CasBasedDistributedLockBackend {
    /// Revokes in-flight blocking acquisition on graceful shutdown
    /// (`cpt-cf-clst-fr-shutdown-revoke`): cancels the shared token so every
    /// waiting [`lock`](Self::lock) call returns [`ClusterError::Shutdown`]
    /// promptly. No task set is awaited — a waiter runs in the caller's own
    /// future, not a spawned task — and no release is issued: a held lock is a
    /// record that outlives this process and lapses at its own deadline
    /// (`cpt-cf-clst-fr-shutdown-ttl-cleanup`, §5.8.2).
    async fn revoke(&self) {
        self.shutdown.cancel();
    }
}

/// The background task that completes a held lock's `renew`/`release` commands
/// and self-terminates on channel closure (the consumer dropping its guard).
///
/// It exists because [`LockGuard`] cannot carry the [`LeaseToken`] — private
/// fields, one constructor — so the token lives here, in the task's own state
/// (§6.5). Everything it does is a lease operation on the shared store, which is
/// what makes the in-process guard path and the remote token path the same code.
struct GuardTask {
    leases: Arc<CacheLeaseStore>,
    /// The prefixed cache key. Derivable from `token.name`, kept resolved so the
    /// hot path does not re-`format!` it per command.
    key: String,
    /// The whole authority over this lock's lease.
    token: LeaseToken,
    provider: &'static str,
    metrics: Arc<dyn ClusterMetrics>,
}

impl GuardTask {
    async fn run(self, mut receiver: LockCommandReceiver) {
        while let Some(request) = receiver.recv().await {
            match request {
                LockRequest::Renew { new_ttl, responder } => {
                    let span = tracing::info_span!(
                        spans::LOCK_RENEW,
                        provider = %self.provider,
                        lock = %self.token.name
                    );
                    let op_started = std::time::Instant::now();
                    let out = self
                        .leases
                        .renew(&self.key, &self.token, new_ttl)
                        .instrument(span)
                        .await;
                    record_lock(
                        &*self.metrics,
                        self.provider,
                        "renew",
                        &self.token.name,
                        op_started,
                        &out,
                    );
                    responder.respond(out);
                }
                LockRequest::Release { responder } => {
                    let span = tracing::info_span!(
                        spans::LOCK_RELEASE,
                        provider = %self.provider,
                        lock = %self.token.name
                    );
                    let op_started = std::time::Instant::now();
                    let out = self
                        .leases
                        .release(&self.key, &self.token)
                        .instrument(span)
                        .await;
                    record_lock(
                        &*self.metrics,
                        self.provider,
                        "release",
                        &self.token.name,
                        op_started,
                        &out,
                    );
                    responder.respond(out);
                    // Release consumes the guard — the task is done.
                    return;
                }
            }
        }
        // The consumer dropped the guard without releasing: no I/O, the lease
        // lapses at its stored deadline (the safety net).
    }
}

#[cfg(test)]
#[path = "lock_tests.rs"]
mod lock_tests;
