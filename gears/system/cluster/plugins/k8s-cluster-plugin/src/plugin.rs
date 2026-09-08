//! The builder/handle lifecycle surface the cluster wiring crate consumes
//! (DESIGN.md §3.2, §11), following the outbox-style builder/handle pattern
//! (ADR-006). None of these are `RunnableCapability`: the cluster gear
//! (`cf-gears-cluster`) owns each handle's lifecycle via `build_and_start`/`stop`.
//!
//! Five shapes live here:
//!
//! * [`K8sClusterPlugin`] — the **combined** plugin: all three primitives over one
//!   shared `kube::Client`, for a consumer that wants the whole cluster surface
//!   from one handle.
//! * [`K8sCachePlugin`], [`K8sLeaderElectionPlugin`], [`K8sLockPlugin`] — the three
//!   **standalone** per-primitive plugins the providers (`provider.rs`) build, so
//!   an operator can route `cache` / `leader_election` / `lock` to `provider: k8s`
//!   independently (§3.5).
//!
//! Each `build_and_start` runs the same startup order (§3.2, §3.4): expand config
//! vars → build the client + resolve identity/namespace once → preflight (RBAC for
//! the enabled primitives, plus the cache canary when a cache is built) → construct
//! the backend(s) over the one shared client → (cache only) return once the shared
//! watcher's initial list is complete. Each handle carries a `stopped` flag and an
//! ADR-006 `Drop` guard (see [`crate::shutdown`]).

use std::sync::Arc;

use cluster_sdk::observability::otel::OtelClusterMetrics;
use cluster_sdk::{
    ClusterCacheBackend, ClusterError, ClusterMetrics, DistributedLockBackend, InstrumentedCache,
    LeaderElectionBackend,
};
use kube::Client;
use toolkit::var_expand::ExpandVars;
use tracing::info;

use crate::cache::K8sCache;
use crate::client::{self, ResolvedClient};
use crate::config::{K8sCacheConfig, K8sClusterConfig, K8sLeaderElectionConfig, K8sLockConfig};
use crate::leader::K8sLeaderElection;
use crate::lock::K8sLock;
use crate::preflight::{self, Primitive};
use crate::provider::PROVIDER_NAME;
use crate::shutdown::{DropDiagnosis, diagnose_drop, join_backends};

/// The single ADR-004 metrics sink for one handle, labelled `k8s`.
///
/// Built over the process-global OpenTelemetry meter; with no meter provider
/// installed (the zero-infra dev path, tests) it is transparently a no-op. Shared by
/// the [`InstrumentedCache`] decorator and the native leader backend (§8).
fn metrics_sink() -> Arc<dyn ClusterMetrics> {
    Arc::new(OtelClusterMetrics::from_global_meter(PROVIDER_NAME))
}

/// Wraps a native cache in the SDK's [`InstrumentedCache`] decorator so its
/// operations emit the contracted `cluster.cache.*` signals (§8), mirroring the
/// postgres and standalone handles.
fn instrument(
    cache: &Arc<K8sCache>,
    metrics: Arc<dyn ClusterMetrics>,
) -> Arc<dyn ClusterCacheBackend> {
    Arc::new(InstrumentedCache::new(
        Arc::clone(cache) as Arc<dyn ClusterCacheBackend>,
        PROVIDER_NAME,
        metrics,
    ))
}

/// Logs the resolved startup identity and the API server version (§3.4). The
/// version is best-effort — an unreadable `/version` is a WARN inside
/// [`preflight::server_version`], never fatal.
async fn log_startup(resolved: &ResolvedClient, primitives: &[Primitive]) {
    let server_version = preflight::server_version(&resolved.client).await;
    info!(
        namespace = %resolved.namespace,
        identity = %resolved.identity,
        server_version = server_version.as_deref().unwrap_or("unknown"),
        primitives = ?primitives,
        "cluster.provider.k8s_started: resolved kubernetes cluster provider startup context"
    );
}

// ── Combined plugin ──────────────────────────────────────────────────────────

/// Entry point for constructing the combined Kubernetes cluster plugin (all three
/// primitives over one shared client).
///
/// ```no_run
/// # async fn doc(config: k8s_cluster_plugin::K8sClusterConfig) -> Result<(), cluster_sdk::ClusterError> {
/// use k8s_cluster_plugin::K8sClusterPlugin;
/// let handle = K8sClusterPlugin::builder(config).build_and_start().await?;
/// handle.stop().await;
/// # Ok(())
/// # }
/// ```
pub struct K8sClusterPlugin;

impl K8sClusterPlugin {
    // No `#[must_use]`: `K8sClusterBuilder` already carries a `#[must_use]` message,
    // so a bare attribute here would be a `clippy::double_must_use` no-op.
    pub fn builder(config: K8sClusterConfig) -> K8sClusterBuilder {
        K8sClusterBuilder {
            config,
            client: None,
            metrics: None,
        }
    }
}

/// Fluent builder for [`K8sClusterPlugin`].
#[must_use = "a builder starts nothing until `.build_and_start()` is called"]
pub struct K8sClusterBuilder {
    config: K8sClusterConfig,
    /// A caller-supplied client to adopt instead of inferring one from the
    /// environment (§3.3, §14).
    client: Option<Client>,
    /// A caller-supplied metrics sink to emit the ADR-004 signals through, instead of
    /// the process-global OpenTelemetry meter (§8).
    metrics: Option<Arc<dyn ClusterMetrics>>,
}

impl K8sClusterBuilder {
    /// Routes the plugin's ADR-004 signals through `metrics` instead of the
    /// process-global OpenTelemetry meter (§8). A host that owns its own meter — or a
    /// test asserting on the emitted catalog — injects a sink here so emission is not
    /// coupled to global state.
    pub fn with_metrics(mut self, metrics: Arc<dyn ClusterMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Adopts an existing `kube::Client` rather than inferring one from the
    /// environment (DESIGN.md §3.3): a host gear (mini-chat, chat-engine) that
    /// already holds a client hands it in so the plugin does not authenticate twice.
    /// When unset, `build_and_start` builds a client via `Config::infer()`.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Builds and starts all three backends over one shared client (§3.2):
    /// expand vars → build client → preflight (RBAC for leader/lock/cache + the
    /// cache canary) → construct the three backends → await the cache watcher's
    /// initial list.
    ///
    /// # Errors
    ///
    /// [`ClusterError::InvalidConfig`] on an unresolvable `${VAR}`, an unresolved
    /// namespace/identity, a failed RBAC preflight, or a missing/mismatched CRD;
    /// [`ClusterError::Provider`] on a backend fault building the client.
    pub async fn build_and_start(self) -> Result<K8sClusterHandle, ClusterError> {
        let mut config = self.config;
        config
            .expand_vars()
            .map_err(|err| ClusterError::InvalidConfig {
                reason: format!("k8s: `namespace`/`identity` env-var expansion failed: {err}"),
            })?;

        let resolved = resolve_client(
            self.client,
            config.namespace.as_deref(),
            config.identity.as_deref(),
            config.request_timeout_ms,
        )
        .await?;

        // The combined plugin runs all three primitives, so every one is preflighted.
        let primitives = [Primitive::LeaderElection, Primitive::Lock, Primitive::Cache];
        log_startup(&resolved, &primitives).await;
        preflight::check_rbac(
            &resolved.client,
            &resolved.namespace,
            &primitives,
            config.skip_rbac_preflight,
        )
        .await?;
        preflight::cache_canary(
            &resolved.client,
            &resolved.namespace,
            &config.lease_prefix,
            &resolved.identity,
        )
        .await?;

        let metrics = self.metrics.unwrap_or_else(metrics_sink);
        let leader_config = leader_config_from(&config);
        let lock_config = lock_config_from(&config);
        let cache_config = cache_config_from(&config);

        let cache = Arc::new(K8sCache::new(&resolved, &cache_config)?);
        let leader = Arc::new(K8sLeaderElection::new(
            &resolved,
            &leader_config,
            Arc::clone(&metrics),
        )?);
        let lock = Arc::new(K8sLock::new(&resolved, &lock_config, Arc::clone(&metrics))?);

        // The cache's contract signals flow through the `InstrumentedCache`
        // decorator; the native leader and lock backends take the same shared sink
        // directly.
        let cache_dyn = instrument(&cache, metrics);

        // Return only once the shared watcher's initial list is complete (§3.2),
        // bounded so a watch that never lists cannot hold startup open forever.
        wait_cache_ready(&cache, config.request_timeout_ms).await;

        Ok(K8sClusterHandle {
            cache,
            cache_dyn,
            leader,
            lock,
            stopped: false,
        })
    }
}

/// The running combined plugin. Hands its cache/leader/lock backends to the wiring
/// crate for `ClientHub` registration. Call [`stop`](Self::stop) on graceful
/// shutdown (§11).
pub struct K8sClusterHandle {
    /// The concrete cache, retained so `stop`/`Drop` can drive its native teardown
    /// (the `dyn` [`cache`](Self::cache) exposes neither `stop` nor `cancel`).
    cache: Arc<K8sCache>,
    /// The same cache as an instrumented trait object handed to the wiring crate (§8).
    cache_dyn: Arc<dyn ClusterCacheBackend>,
    leader: Arc<K8sLeaderElection>,
    lock: Arc<K8sLock>,
    /// Set by `stop` so the `Drop` guard can tell a graceful shutdown apart from a
    /// forgotten one (ADR-006 Confirmation).
    stopped: bool,
}

impl K8sClusterHandle {
    /// The instrumented cache backend (§8).
    #[must_use]
    pub fn cache(&self) -> Arc<dyn ClusterCacheBackend> {
        Arc::clone(&self.cache_dyn)
    }

    /// The native leader-election backend.
    #[must_use]
    pub fn leader_election(&self) -> Arc<dyn LeaderElectionBackend> {
        Arc::clone(&self.leader) as Arc<dyn LeaderElectionBackend>
    }

    /// The native lock backend.
    #[must_use]
    pub fn lock(&self) -> Arc<dyn DistributedLockBackend> {
        Arc::clone(&self.lock) as Arc<dyn DistributedLockBackend>
    }

    /// Shuts the plugin down (§11). Each backend's `stop()` delivers its own
    /// primitive's terminal watch events — leader `Status(Lost)` + `Closed(Shutdown)`,
    /// cache `Closed(Shutdown)`, blocked `lock()` waiters `Shutdown` — then cancels
    /// its token and awaits its tasks; the three run concurrently under one
    /// [`TASK_JOIN_TIMEOUT`](crate::shutdown::TASK_JOIN_TIMEOUT). Dropping the handle
    /// afterward drops the last client clones.
    pub async fn stop(mut self) {
        let cache = Arc::clone(&self.cache);
        let leader = Arc::clone(&self.leader);
        let lock = Arc::clone(&self.lock);
        join_backends(async move {
            tokio::join!(cache.stop(), leader.stop(), lock.stop());
        })
        .await;
        self.stopped = true;
    }
}

impl Drop for K8sClusterHandle {
    fn drop(&mut self) {
        // Cancel every backend's token *before* diagnosing (see
        // `shutdown::diagnose_drop`), so a forgotten `stop()` — or a `stop()` future
        // dropped by a supervisor's `timeout(D, handle.stop())` — still tears down
        // the watcher/sweeper/reaper/election tasks.
        let diagnosis = diagnose_drop(self.stopped, || {
            self.cache.cancel();
            self.leader.cancel();
            self.lock.cancel();
        });
        report_drop("K8sClusterHandle", &diagnosis);
    }
}

// ── Standalone cache plugin ──────────────────────────────────────────────────

/// Entry point for the standalone Kubernetes cache plugin (§3.5) — the cache
/// primitive only, built by [`K8sCacheProvider`](crate::provider::K8sCacheProvider).
pub struct K8sCachePlugin;

impl K8sCachePlugin {
    pub fn builder(config: K8sCacheConfig) -> K8sCacheBuilder {
        K8sCacheBuilder {
            config,
            client: None,
        }
    }
}

/// Fluent builder for [`K8sCachePlugin`].
#[must_use = "a builder starts nothing until `.build_and_start()` is called"]
pub struct K8sCacheBuilder {
    config: K8sCacheConfig,
    /// A caller-supplied client to adopt instead of inferring one (§3.3).
    client: Option<Client>,
}

impl K8sCacheBuilder {
    /// Adopts an existing `kube::Client` rather than inferring one (DESIGN.md §3.3).
    /// See [`K8sClusterBuilder::with_client`].
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Builds and starts the cache backend (§3.2): expand vars → build client →
    /// preflight (cache RBAC + canary) → construct the cache → await the watcher's
    /// initial list.
    ///
    /// # Errors
    ///
    /// [`ClusterError::InvalidConfig`] on an unresolvable `${VAR}`, an unresolved
    /// namespace/identity, a failed RBAC preflight, or a missing/mismatched CRD.
    pub async fn build_and_start(self) -> Result<K8sCacheHandle, ClusterError> {
        let mut config = self.config;
        config
            .expand_vars()
            .map_err(|err| ClusterError::InvalidConfig {
                reason: format!("k8s: `namespace`/`identity` env-var expansion failed: {err}"),
            })?;

        let resolved = resolve_client(
            self.client,
            config.namespace.as_deref(),
            config.identity.as_deref(),
            config.request_timeout_ms,
        )
        .await?;

        let primitives = [Primitive::Cache];
        log_startup(&resolved, &primitives).await;
        preflight::check_rbac(
            &resolved.client,
            &resolved.namespace,
            &primitives,
            config.skip_rbac_preflight,
        )
        .await?;
        preflight::cache_canary(
            &resolved.client,
            &resolved.namespace,
            &config.lease_prefix,
            &resolved.identity,
        )
        .await?;

        let cache = Arc::new(K8sCache::new(&resolved, &config)?);
        let cache_dyn = instrument(&cache, metrics_sink());
        wait_cache_ready(&cache, config.request_timeout_ms).await;

        Ok(K8sCacheHandle {
            cache,
            cache_dyn,
            stopped: false,
        })
    }
}

/// The running standalone cache plugin.
pub struct K8sCacheHandle {
    cache: Arc<K8sCache>,
    cache_dyn: Arc<dyn ClusterCacheBackend>,
    stopped: bool,
}

impl K8sCacheHandle {
    /// The instrumented cache backend (§8).
    #[must_use]
    pub fn cache(&self) -> Arc<dyn ClusterCacheBackend> {
        Arc::clone(&self.cache_dyn)
    }

    /// Shuts the cache down (§11): deliver `Closed(Shutdown)` to active watches,
    /// cancel the watcher/sweeper, and await them under
    /// [`TASK_JOIN_TIMEOUT`](crate::shutdown::TASK_JOIN_TIMEOUT).
    pub async fn stop(mut self) {
        let cache = Arc::clone(&self.cache);
        join_backends(cache.stop()).await;
        self.stopped = true;
    }
}

impl Drop for K8sCacheHandle {
    fn drop(&mut self) {
        let diagnosis = diagnose_drop(self.stopped, || self.cache.cancel());
        report_drop("K8sCacheHandle", &diagnosis);
    }
}

// ── Standalone leader-election plugin ────────────────────────────────────────

/// Entry point for the standalone Kubernetes leader-election plugin (§3.5) — built
/// by [`K8sLeaderElectionProvider`](crate::provider::K8sLeaderElectionProvider).
pub struct K8sLeaderElectionPlugin;

impl K8sLeaderElectionPlugin {
    pub fn builder(config: K8sLeaderElectionConfig) -> K8sLeaderElectionBuilder {
        K8sLeaderElectionBuilder {
            config,
            client: None,
        }
    }
}

/// Fluent builder for [`K8sLeaderElectionPlugin`].
#[must_use = "a builder starts nothing until `.build_and_start()` is called"]
pub struct K8sLeaderElectionBuilder {
    config: K8sLeaderElectionConfig,
    /// A caller-supplied client to adopt instead of inferring one (§3.3).
    client: Option<Client>,
}

impl K8sLeaderElectionBuilder {
    /// Adopts an existing `kube::Client` rather than inferring one (DESIGN.md §3.3).
    /// See [`K8sClusterBuilder::with_client`].
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Builds and starts the leader-election backend (§3.2): expand vars → build
    /// client → preflight (leader RBAC; **no cache canary**) → construct the backend.
    ///
    /// # Errors
    ///
    /// [`ClusterError::InvalidConfig`] on an unresolvable `${VAR}`, an unresolved
    /// namespace/identity, a bad `lease_prefix`, or a failed RBAC preflight.
    pub async fn build_and_start(self) -> Result<K8sLeaderElectionHandle, ClusterError> {
        let mut config = self.config;
        config
            .expand_vars()
            .map_err(|err| ClusterError::InvalidConfig {
                reason: format!("k8s: `namespace`/`identity` env-var expansion failed: {err}"),
            })?;

        let resolved = resolve_client(
            self.client,
            config.namespace.as_deref(),
            config.identity.as_deref(),
            config.request_timeout_ms,
        )
        .await?;

        let primitives = [Primitive::LeaderElection];
        log_startup(&resolved, &primitives).await;
        preflight::check_rbac(
            &resolved.client,
            &resolved.namespace,
            &primitives,
            config.skip_rbac_preflight,
        )
        .await?;

        let metrics = metrics_sink();
        let leader = Arc::new(K8sLeaderElection::new(&resolved, &config, metrics)?);

        Ok(K8sLeaderElectionHandle {
            leader,
            stopped: false,
        })
    }
}

/// The running standalone leader-election plugin.
pub struct K8sLeaderElectionHandle {
    leader: Arc<K8sLeaderElection>,
    stopped: bool,
}

impl K8sLeaderElectionHandle {
    /// The native leader-election backend.
    #[must_use]
    pub fn leader_election(&self) -> Arc<dyn LeaderElectionBackend> {
        Arc::clone(&self.leader) as Arc<dyn LeaderElectionBackend>
    }

    /// Shuts leader election down (§11): each election task delivers its watch the
    /// `Status(Lost)` then `Closed(Shutdown)` terminal pair, then the tasks are
    /// cancelled and awaited under
    /// [`TASK_JOIN_TIMEOUT`](crate::shutdown::TASK_JOIN_TIMEOUT).
    pub async fn stop(mut self) {
        let leader = Arc::clone(&self.leader);
        join_backends(leader.stop()).await;
        self.stopped = true;
    }
}

impl Drop for K8sLeaderElectionHandle {
    fn drop(&mut self) {
        let diagnosis = diagnose_drop(self.stopped, || self.leader.cancel());
        report_drop("K8sLeaderElectionHandle", &diagnosis);
    }
}

// ── Standalone lock plugin ───────────────────────────────────────────────────

/// Entry point for the standalone Kubernetes lock plugin (§3.5) — built by
/// [`K8sLockProvider`](crate::provider::K8sLockProvider).
pub struct K8sLockPlugin;

impl K8sLockPlugin {
    pub fn builder(config: K8sLockConfig) -> K8sLockBuilder {
        K8sLockBuilder {
            config,
            client: None,
        }
    }
}

/// Fluent builder for [`K8sLockPlugin`].
#[must_use = "a builder starts nothing until `.build_and_start()` is called"]
pub struct K8sLockBuilder {
    config: K8sLockConfig,
    /// A caller-supplied client to adopt instead of inferring one (§3.3).
    client: Option<Client>,
}

impl K8sLockBuilder {
    /// Adopts an existing `kube::Client` rather than inferring one (DESIGN.md §3.3).
    /// See [`K8sClusterBuilder::with_client`].
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = Some(client);
        self
    }

    /// Builds and starts the lock backend (§3.2): expand vars → build client →
    /// preflight (lock RBAC; **no cache canary**) → construct the backend (which
    /// spawns the stale-object reaper).
    ///
    /// # Errors
    ///
    /// [`ClusterError::InvalidConfig`] on an unresolvable `${VAR}`, an unresolved
    /// namespace/identity, a bad `lease_prefix`, or a failed RBAC preflight.
    pub async fn build_and_start(self) -> Result<K8sLockHandle, ClusterError> {
        let mut config = self.config;
        config
            .expand_vars()
            .map_err(|err| ClusterError::InvalidConfig {
                reason: format!("k8s: `namespace`/`identity` env-var expansion failed: {err}"),
            })?;

        let resolved = resolve_client(
            self.client,
            config.namespace.as_deref(),
            config.identity.as_deref(),
            config.request_timeout_ms,
        )
        .await?;

        let primitives = [Primitive::Lock];
        log_startup(&resolved, &primitives).await;
        preflight::check_rbac(
            &resolved.client,
            &resolved.namespace,
            &primitives,
            config.skip_rbac_preflight,
        )
        .await?;

        let lock = Arc::new(K8sLock::new(&resolved, &config, metrics_sink())?);

        Ok(K8sLockHandle {
            lock,
            stopped: false,
        })
    }
}

/// The running standalone lock plugin.
pub struct K8sLockHandle {
    lock: Arc<K8sLock>,
    stopped: bool,
}

impl K8sLockHandle {
    /// The native lock backend.
    #[must_use]
    pub fn lock(&self) -> Arc<dyn DistributedLockBackend> {
        Arc::clone(&self.lock) as Arc<dyn DistributedLockBackend>
    }

    /// Shuts the lock down (§11): cancel the guard/reaper tasks and await them under
    /// [`TASK_JOIN_TIMEOUT`](crate::shutdown::TASK_JOIN_TIMEOUT). Held Leases are
    /// left to lapse on their own deadlines — a restart is not a lease event (§5.5).
    pub async fn stop(mut self) {
        let lock = Arc::clone(&self.lock);
        join_backends(lock.stop()).await;
        self.stopped = true;
    }
}

impl Drop for K8sLockHandle {
    fn drop(&mut self) {
        let diagnosis = diagnose_drop(self.stopped, || self.lock.cancel());
        report_drop("K8sLockHandle", &diagnosis);
    }
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Resolves the connection either by adopting a caller-supplied client (§3.3, the
/// `with_client` path) or by inferring one from the environment (`Config::infer()`).
///
/// # Errors
///
/// Propagates [`client::adopt`]/[`client::build`]'s errors — [`ClusterError::InvalidConfig`]
/// on an unresolvable environment or namespace/identity, [`ClusterError::Provider`] on a
/// client-construction fault.
async fn resolve_client(
    client: Option<Client>,
    config_namespace: Option<&str>,
    config_identity: Option<&str>,
    request_timeout_ms: u64,
) -> Result<ResolvedClient, ClusterError> {
    match client {
        Some(client) => client::adopt(client, config_namespace, config_identity).await,
        None => client::build(config_namespace, config_identity, request_timeout_ms).await,
    }
}

/// Awaits the cache watcher's initial list under a startup budget (§3.2), so a
/// watch that never lists cannot hold `build_and_start` open forever.
async fn wait_cache_ready(cache: &Arc<K8sCache>, request_timeout_ms: u64) {
    let budget = std::time::Duration::from_millis(request_timeout_ms);
    if tokio::time::timeout(budget, cache.wait_ready())
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_ms = request_timeout_ms,
            "cluster.provider.cache_watch_not_ready: the cache watcher's initial list did not \
             complete within the startup budget; proceeding (reads stay authoritative and the \
             watch cache fills as events arrive)"
        );
    }
}

/// Emits the ADR-006 `Drop` diagnosis for `handle`: silent on a clean stop, a WARN
/// during a panic unwind, and a debug-`panic!` / release-`warn!` for a forgotten
/// `stop()`.
fn report_drop(handle: &str, diagnosis: &DropDiagnosis) {
    match diagnosis {
        DropDiagnosis::StoppedCleanly => {}
        DropDiagnosis::DuringPanic => tracing::warn!(
            "{handle} dropped during panic unwind without stop(); skipping debug panic to avoid \
             double-panic abort"
        ),
        DropDiagnosis::Unstopped => {
            #[cfg(debug_assertions)]
            panic!("{handle} dropped without stop() - programming error");
            #[cfg(not(debug_assertions))]
            tracing::warn!(
                "{handle} dropped without stop() - programming error; background tasks may leak"
            );
        }
    }
}

/// Projects the combined config's leader-election subset (§3.5). The combined
/// plugin runs all three primitives from one config, so each backend is handed the
/// slice of fields it reads.
fn leader_config_from(config: &K8sClusterConfig) -> K8sLeaderElectionConfig {
    K8sLeaderElectionConfig {
        namespace: config.namespace.clone(),
        identity: config.identity.clone(),
        lease_prefix: config.lease_prefix.clone(),
        request_timeout_ms: config.request_timeout_ms,
        max_acquire_backoff_ms: config.max_acquire_backoff_ms,
        skip_rbac_preflight: config.skip_rbac_preflight,
        min_election_ttl_ms: config.min_election_ttl_ms,
        election_lease_names: config.election_lease_names.clone(),
    }
}

/// Projects the combined config's lock subset (§3.5).
fn lock_config_from(config: &K8sClusterConfig) -> K8sLockConfig {
    K8sLockConfig {
        namespace: config.namespace.clone(),
        identity: config.identity.clone(),
        lease_prefix: config.lease_prefix.clone(),
        request_timeout_ms: config.request_timeout_ms,
        max_acquire_backoff_ms: config.max_acquire_backoff_ms,
        skip_rbac_preflight: config.skip_rbac_preflight,
        reaper: config.reaper,
        reaper_interval_ms: config.reaper_interval_ms,
        lock_object_retention_ms: config.lock_object_retention_ms,
        lock_name_cardinality_warn_threshold: config.lock_name_cardinality_warn_threshold,
    }
}

/// Projects the combined config's cache subset (§3.5).
fn cache_config_from(config: &K8sClusterConfig) -> K8sCacheConfig {
    K8sCacheConfig {
        namespace: config.namespace.clone(),
        identity: config.identity.clone(),
        lease_prefix: config.lease_prefix.clone(),
        request_timeout_ms: config.request_timeout_ms,
        max_acquire_backoff_ms: config.max_acquire_backoff_ms,
        skip_rbac_preflight: config.skip_rbac_preflight,
        cache_reads: config.cache_reads,
        cache_watch: config.cache_watch,
        cache_sweep_interval_ms: config.cache_sweep_interval_ms,
        max_value_bytes: config.max_value_bytes,
        put_max_retries: config.put_max_retries,
    }
}
