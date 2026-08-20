//! Per-`gts_id` exclusive coordination lock backed by the cluster gear's
//! [`DistributedLockV1`] / [`DistributedLockBackend`].
//!
//! **Component**: `cpt-cf-uc-ch-plugin-component-lock-manager`
//!
//! # Overview
//!
//! Replaces the former `ClickHouse` Keeper / `ZooKeeper` reader/writer recipe with
//! a single exclusive mutex per `gts_id`, resolved from the `usage-collector`
//! cluster profile. Both create (ingest) and delete (catalog) paths acquire the
//! same lock name, so they are mutually exclusive and concurrent creates for
//! the same `gts_id` serialize.
//!
//! # Lock name convention
//!
//! Cluster lock names must satisfy `[a-zA-Z0-9_-]{1,255}`. Raw `gts_id` values
//! contain `.` and `~`, so every `gts_id` is mapped to a stable hashed leaf:
//!
//! ```text
//! gts-{xxh3_64 hex}
//! ```
//!
//! under a [`DistributedLockV1::scoped`](`usage-collector`) prefix.
//!
//! # Lifecycle / lazy resolve
//!
//! The cluster gear registers lock backends in its `start()` phase, while this
//! plugin constructs stores during `init()`. [`LockManager`] therefore holds an
//! [`Arc<ClientHub>`] and resolves [`DistributedLockV1`] lazily on first
//! acquire (after cluster `start`). If the profile is unbound, acquire fails
//! closed with [`UsageCollectorPluginError::Transient`].
//!
//! # Release semantics
//!
//! Cluster [`LockGuard`] drop is a **no-op** — locks lapse only via explicit
//! [`LockGuard::release`] or TTL expiry. [`ClusterLockGuard`] therefore:
//! - exposes async [`ClusterLockGuard::release`] for every exit path, and
//! - best-effort spawns a release on [`Drop`] if the caller forgot.
//!
//! # ADR-002 deviation
//!
//! Cluster ADR-002 forbids remote I/O while holding a lock. This plugin must
//! hold the lock across `ClickHouse` SQL (that is the referential-integrity
//! mechanism). Call sites invoke [`ClusterLockGuard::ensure_still_held`]
//! (`renew`) immediately before the mutating write and abort with `Transient`
//! on [`ClusterError::LockExpired`]. Size `lock_ttl_secs` above worst-case
//! critical-section latency.
//!
//! # Fail-closed rule (DESIGN.md §3.6 step 7)
//!
//! If the cluster lock cannot be granted within `lock_timeout_secs`, both
//! create and delete return [`UsageCollectorPluginError::Transient`] rather
//! than proceeding unlocked.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cluster_sdk::error::ClusterError;
use cluster_sdk::lock::{DistributedLockV1, LockCapability, LockGuard};
use cluster_sdk::profile::ClusterProfile;
use tokio::sync::Mutex;
use toolkit::client_hub::ClientHub;
use tracing::instrument;
use usage_collector_sdk::UsageCollectorPluginError;
use xxhash_rust::xxh3::xxh3_64;

use crate::infra::metrics::{LockMode, Metrics, OpDurationGuard, TimedOp};

/// Fixed cluster profile marker. Operators MUST provision
/// `cluster.profiles.usage-collector` (typically `cache: { provider: standalone }`
/// with omit-default lock).
#[derive(Debug, Clone, Copy)]
pub struct UsageCollectorProfile;

impl ClusterProfile for UsageCollectorProfile {
    const NAME: &'static str = "usage-collector";
}

/// Scope prefix applied to every lock name under this plugin.
const LOCK_SCOPE_PREFIX: &str = "usage-collector";

/// Exclusive per-`gts_id` coordination lock manager over cluster
/// [`DistributedLockV1`].
///
/// Wrap in [`Arc`] so it is cheaply cloneable across the gear's async tasks.
pub struct LockManager {
    hub: Arc<ClientHub>,
    /// Lazily resolved after cluster `start` registers the backend.
    lock: OnceLock<DistributedLockV1>,
    /// Serializes the first resolve attempt (`OnceLock` alone is not async-safe
    /// for the fallible resolve path).
    resolve_gate: Mutex<()>,
    ttl: Duration,
    timeout: Duration,
    metrics: Arc<Metrics>,
}

impl LockManager {
    /// Build a lock manager that will resolve [`DistributedLockV1`] lazily from
    /// `hub` on first acquire.
    ///
    /// Does **not** talk to the cluster backend yet — safe to call during gear
    /// `init` before cluster `start`.
    #[must_use]
    pub fn new(
        hub: Arc<ClientHub>,
        ttl: Duration,
        timeout: Duration,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            hub,
            lock: OnceLock::new(),
            resolve_gate: Mutex::new(()),
            ttl,
            timeout,
            metrics,
        }
    }

    /// Map a raw `gts_id` to a cluster-valid lock leaf name.
    #[must_use]
    pub(crate) fn lock_leaf(gts_id: &str) -> String {
        format!("gts-{:016x}", xxh3_64(gts_id.as_bytes()))
    }

    /// Resolve (or return cached) [`DistributedLockV1`] for
    /// [`UsageCollectorProfile`], requiring linearizable locks.
    fn resolve_lock(&self) -> Result<&DistributedLockV1, UsageCollectorPluginError> {
        if let Some(lock) = self.lock.get() {
            return Ok(lock);
        }
        // Sync resolve under a brief async gate — caller must hold resolve_gate
        // when racing; see `ensure_resolved`.
        let facade = DistributedLockV1::resolver(&self.hub)
            .profile(UsageCollectorProfile)
            .require(LockCapability::Linearizable)
            .resolve()
            .map_err(map_resolve_error)?
            .scoped(LOCK_SCOPE_PREFIX)
            .map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "failed to scope usage-collector distributed lock: {e}"
                ))
            })?;
        // A racing resolve may have installed its facade first; either winner is
        // equivalent, and the `get` below returns whichever landed.
        let _installed = self.lock.set(facade);
        self.lock.get().ok_or_else(|| {
            UsageCollectorPluginError::internal(
                "distributed lock OnceLock empty after successful set",
            )
        })
    }

    async fn ensure_resolved(&self) -> Result<&DistributedLockV1, UsageCollectorPluginError> {
        if let Some(lock) = self.lock.get() {
            return Ok(lock);
        }
        let _gate = self.resolve_gate.lock().await;
        self.resolve_lock()
    }

    /// Acquire the exclusive per-`gts_id` coordination lock.
    ///
    /// `mode` is an observability label only (create vs delete call site); both
    /// paths acquire the same exclusive mutex.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Transient`] if the lock cannot be
    /// acquired within `lock_timeout_secs`, or if the cluster profile is unbound
    /// (fail-closed — DESIGN.md §3.6 step 7).
    #[instrument(skip(self), fields(gts_id = %gts_id, ?mode))]
    pub async fn acquire(
        &self,
        gts_id: &str,
        mode: LockMode,
    ) -> Result<ClusterLockGuard, UsageCollectorPluginError> {
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::LockAcquire(mode));

        let lock = self.ensure_resolved().await.inspect_err(|_e| {
            self.metrics.inc_lock_manager_unavailable(mode);
        })?;

        let name = Self::lock_leaf(gts_id);
        let started = Instant::now();
        let result = lock.lock(&name, self.ttl, self.timeout).await;
        match result {
            Ok(inner) => {
                // Contended acquires wait; treat any wait as contention signal.
                if started.elapsed() > Duration::from_millis(5) {
                    self.metrics.inc_lock_contention(mode);
                }
                Ok(ClusterLockGuard {
                    inner: Some(inner),
                    ttl: self.ttl,
                    mode,
                    metrics: Arc::clone(&self.metrics),
                    released: false,
                })
            }
            Err(e) => {
                self.metrics.inc_lock_manager_unavailable(mode);
                Err(map_acquire_error(e))
            }
        }
    }

    /// Convenience: exclusive lock for the create / ingest path.
    pub async fn acquire_for_create(
        &self,
        gts_id: &str,
    ) -> Result<ClusterLockGuard, UsageCollectorPluginError> {
        self.acquire(gts_id, LockMode::Create).await
    }

    /// Convenience: exclusive lock for the catalog-delete path.
    pub async fn acquire_for_delete(
        &self,
        gts_id: &str,
    ) -> Result<ClusterLockGuard, UsageCollectorPluginError> {
        self.acquire(gts_id, LockMode::Delete).await
    }
}

pub(crate) fn map_resolve_error(err: ClusterError) -> UsageCollectorPluginError {
    match err {
        ClusterError::ProfileNotBound { .. }
        | ClusterError::CapabilityNotMet { .. }
        | ClusterError::ProfileNotSpecified => UsageCollectorPluginError::transient(format!(
            "cluster distributed lock unavailable for profile `{}`: {err}",
            UsageCollectorProfile::NAME
        )),
        other => UsageCollectorPluginError::internal(format!(
            "failed to resolve cluster distributed lock: {other}"
        )),
    }
}

pub(crate) fn map_acquire_error(err: ClusterError) -> UsageCollectorPluginError {
    match err {
        ClusterError::LockTimeout { .. }
        | ClusterError::LockContended { .. }
        | ClusterError::ProfileNotBound { .. }
        | ClusterError::Shutdown
        | ClusterError::Provider { .. } => {
            UsageCollectorPluginError::transient(format!("cluster lock acquire failed: {err}"))
        }
        ClusterError::InvalidName { .. } => {
            UsageCollectorPluginError::internal(format!("invalid cluster lock name: {err}"))
        }
        other => {
            UsageCollectorPluginError::transient(format!("cluster lock acquire failed: {other}"))
        }
    }
}

pub(crate) fn map_renew_error(err: ClusterError) -> UsageCollectorPluginError {
    match err {
        ClusterError::LockExpired { .. }
        | ClusterError::Provider { .. }
        | ClusterError::Shutdown => UsageCollectorPluginError::transient(format!(
            "cluster lock no longer held (renew failed): {err}"
        )),
        other => {
            UsageCollectorPluginError::transient(format!("cluster lock renew failed: {other}"))
        }
    }
}

/// Exclusive lock acquisition port shared by the catalog and record stores.
///
/// Abstracts the cluster-backed lock manager so `ChCatalogStore` and
/// `ChRecordStore` can be exercised in unit tests with stub implementations.
///
/// The cluster lock is a single exclusive per-`gts_id` mutex: catalog create,
/// catalog delete, and record create all contend on the same name, which is
/// what makes the reference-count probe under `delete` authoritative.
///
/// The returned guard is a boxed [`LockGuardPort`] that must be explicitly
/// [`LockGuardPort::release`]d on every exit path (cluster lock drop is a
/// no-op) and exposes `ensure_still_held` (lease renew) before the critical
/// write. Callers must hold it across the entire critical section (e.g. for
/// delete: existence check → reference probe → renew → `DELETE`).
#[async_trait]
pub trait CatalogLockPort: Send + Sync + 'static {
    /// Acquire an exclusive per-`gts_id` lock for a catalog-delete operation.
    ///
    /// Returns [`UsageCollectorPluginError::Transient`] when the cluster lock
    /// cannot be granted (fail-closed rule — DESIGN.md §3.6 step 7).
    async fn acquire_exclusive_for_delete(
        &self,
        gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError>;

    /// Acquire the same exclusive per-`gts_id` lock for a create operation
    /// (catalog create or record ingest).
    ///
    /// Defaults to [`Self::acquire_exclusive_for_delete`] because both call
    /// sites resolve to one exclusive mutex per `gts_id`; the split exists so
    /// an implementation that labels its lock metrics per call site can
    /// override it with a `mode = "create"` acquire.
    async fn acquire_exclusive_for_create(
        &self,
        gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        self.acquire_exclusive_for_delete(gts_id).await
    }
}

/// Seam trait for testing the lock-guard validity check in isolation.
///
/// `ChCatalogStore::delete` calls `ensure_still_held` / `release` through this
/// trait so the delete path can be exercised offline with stub guards.
#[async_trait]
pub trait LockGuardPort: Send + Sync + 'static {
    /// Renew the lease (replacement for Keeper session validity). Returns
    /// [`UsageCollectorPluginError::Transient`] if the lock is no longer held.
    async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError>;

    /// Explicitly release the lock. Must be called on every exit path; drop is
    /// only a best-effort fallback.
    async fn release(self: Box<Self>) -> Result<(), UsageCollectorPluginError>;
}

/// RAII-ish guard for an exclusive cluster lock.
///
/// Call [`release`](Self::release) on every exit path. [`Drop`] best-effort
/// spawns a release if the caller forgot (cluster [`LockGuard`] drop is a
/// no-op).
pub struct ClusterLockGuard {
    inner: Option<LockGuard>,
    ttl: Duration,
    mode: LockMode,
    metrics: Arc<Metrics>,
    released: bool,
}

impl ClusterLockGuard {
    /// Renew the lock TTL to confirm the lease is still held before a critical
    /// `ClickHouse` write.
    pub async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError> {
        let Some(inner) = self.inner.as_ref() else {
            return Err(UsageCollectorPluginError::transient(
                "cluster lock guard already released",
            ));
        };
        inner.renew(self.ttl).await.map_err(|e| {
            self.metrics.inc_lock_manager_unavailable(self.mode);
            map_renew_error(e)
        })
    }

    /// Explicitly release the lock.
    pub async fn release(mut self) -> Result<(), UsageCollectorPluginError> {
        self.released = true;
        if let Some(inner) = self.inner.take() {
            inner.release().await.map_err(|e| {
                self.metrics.inc_lock_manager_unavailable(self.mode);
                UsageCollectorPluginError::transient(format!("cluster lock release failed: {e}"))
            })?;
        }
        Ok(())
    }
}

impl Drop for ClusterLockGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let Some(inner) = self.inner.take() else {
            return;
        };
        let mode = self.mode;
        // Cluster LockGuard drop is a no-op; spawn best-effort release so a
        // forgotten explicit release still frees the name before TTL. Without a
        // runtime there is nothing to spawn onto, so the release cannot be
        // attempted at all — say so rather than dropping the guard silently.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                ?mode,
                "cluster lock guard dropped outside a Tokio runtime; no release could be \
                 attempted and the lock name stays held until its TTL expires"
            );
            return;
        };
        handle.spawn(async move {
            // Best effort: if the release fails the lease still lapses on
            // its own TTL, and there is no caller left to inform — but the
            // cluster degradation that caused it must stay visible.
            if let Err(e) = inner.release().await {
                tracing::warn!(
                    error = %e,
                    ?mode,
                    "cluster lock release on guard drop failed; the lock name stays held \
                     until its TTL expires"
                );
            }
        });
    }
}

#[async_trait]
impl LockGuardPort for ClusterLockGuard {
    async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError> {
        ClusterLockGuard::ensure_still_held(self).await
    }

    async fn release(self: Box<Self>) -> Result<(), UsageCollectorPluginError> {
        (*self).release().await
    }
}

/// Wire the real [`LockManager`] as a [`CatalogLockPort`].
#[async_trait]
impl CatalogLockPort for LockManager {
    async fn acquire_exclusive_for_delete(
        &self,
        gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        let guard = self.acquire_for_delete(gts_id).await?;
        Ok(Box::new(guard))
    }

    /// Overrides the trait default so the create call site keeps its
    /// `mode = "create"` lock metric labels; both methods take the same
    /// exclusive per-`gts_id` mutex.
    async fn acquire_exclusive_for_create(
        &self,
        gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        let guard = self.acquire_for_create(gts_id).await?;
        Ok(Box::new(guard))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "lock_manager_tests.rs"]
mod lock_manager_tests;
