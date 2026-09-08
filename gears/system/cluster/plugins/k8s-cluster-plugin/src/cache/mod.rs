//! Native cache over the `ClusterCacheEntry` custom resource (DESIGN.md §6).
//!
//! [`K8sCache`] implements [`ClusterCacheBackend`] over one namespaced
//! `ClusterCacheEntry` per key. The version the contract wants lives in
//! `spec.version` — this plugin's own monotonic counter, never
//! `metadata.resourceVersion` (§2.7) — so it starts at 1 and resets to 1 on
//! delete-and-recreate. `metadata.resourceVersion` is used only as the guarded-write
//! precondition (§2.7). TTL is enforced on the **read path** (an entry past its
//! `expiresAt` reads as absent, §6.2) and reclaimed by the deadline-armed
//! [`sweeper`]; a single shared [`watch`]er fans every subscriber out in process
//! (§6.3); and `scan_prefix` is a paginated list (§6.4).
//!
//! The pure pieces carry the L1 coverage: version arithmetic and read-path expiry
//! here, the [`watch`] registry + event mapping, the [`sweeper`] heap, and the
//! [`scan`] filter. Real-server behaviour is Phase 6.

mod scan;
mod sweeper;
mod watch;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::Api;
use kube::runtime::watcher;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use cluster_sdk::ClusterError;
use cluster_sdk::cache::{
    CacheConsistency, CacheEntry, CacheFeatures, CacheWatch, ClusterCacheBackend, PutRequest, Ttl,
};

use crate::client::ResolvedClient;
use crate::config::{K8sCacheConfig, ReadMode};
use crate::crd::{ClusterCacheEntry, ClusterCacheEntrySpec};
use crate::guarded::{self, CallSite, Created, Replaced};
use crate::k8s_error;
use crate::naming::{
    self, ANNOTATION_NAME, LABEL_MANAGED_BY, LABEL_PRIMITIVE, MANAGED_BY_VALUE, Seg,
};

use self::scan::{LIST_PAGE, live_matching_key};
use self::watch::{CacheRegistry, key_of};

/// The in-flight event buffer for each [`CacheWatch`] (§6.3).
const WATCH_BUFFER: usize = 32;

/// Whether the entry's `expiresAt` deadline has passed as of `now` (§6.2).
///
/// Absent (`Ttl::Indefinite`) never expires. A malformed deadline is treated as
/// **not** expired — a corrupt object is not silently dropped from a read; the
/// canary and the CRD schema are what keep the field well-formed.
#[must_use]
pub fn is_expired(expires_at: Option<&str>, now: Timestamp) -> bool {
    expires_at
        .and_then(|s| s.parse::<Timestamp>().ok())
        .is_some_and(|deadline| deadline <= now)
}

/// The next version after `prev`: `prev + 1`, saturating (§2.7). A mutating write
/// always strictly increases the version, so an identical-value `put` still bumps it.
#[must_use]
pub fn next_version(prev: u64) -> u64 {
    prev.saturating_add(1)
}

/// The absolute `expiresAt` a write records for `ttl`, on the writer's clock (the
/// one documented clock-skew exception, §6.2). `None` for [`Ttl::Indefinite`].
#[must_use]
pub fn expiry_deadline(ttl: Ttl, now: Timestamp) -> Option<String> {
    match ttl {
        Ttl::Of(duration) => {
            let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
            let signed =
                SignedDuration::new(secs, i32::try_from(duration.subsec_nanos()).unwrap_or(0));
            Some(now.checked_add(signed).unwrap_or(now).to_string())
        }
        Ttl::Indefinite => None,
    }
}

/// Shared runtime for the cache backend.
struct CacheRuntime {
    client: kube::Client,
    namespace: String,
    lease_prefix: String,
    request_timeout: Duration,
    read_mode: ReadMode,
    max_value_bytes: usize,
    put_max_retries: u8,
    registry: Arc<CacheRegistry>,
    /// Commands to the sweeper task (arm/disarm), `None` when the watcher-driven
    /// sweeper is disabled (`cache_watch: false` uses a periodic scan instead).
    sweep_tx: Option<mpsc::UnboundedSender<SweepCmd>>,
    // The cache's contract signals (`cluster.cache.*`) are emitted by the
    // `InstrumentedCache` decorator the handle wraps this backend in (DESIGN.md §8),
    // so the backend itself carries no metrics sink — unlike the native lock/leader
    // backends, whose signals have no decorator equivalent.
}

/// A command to the sweeper task (§6.2).
enum SweepCmd {
    /// Arm `key` to be reclaimed at `at`.
    Arm { key: String, at: Timestamp },
    /// Cancel any pending sweep of `key` (deleted, or written indefinite).
    Disarm { key: String },
}

impl CacheRuntime {
    fn api(&self) -> Api<ClusterCacheEntry> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn object_name(&self, key: &str) -> String {
        naming::lease_name(&self.lease_prefix, Seg::Cache, key)
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

    /// Reads the raw object for `key`, `None` on 404.
    async fn read_raw(&self, key: &str) -> Result<Option<ClusterCacheEntry>, ClusterError> {
        let api = self.api();
        let name = self.object_name(key);
        self.timed("get cache entry", guarded::read(&api, &name))
            .await
    }

    /// Reads the live entry for `key`: `None` when absent **or** past its
    /// `expiresAt` (read-path expiry is authoritative, §6.2).
    async fn read_live(&self, key: &str) -> Result<Option<ClusterCacheEntry>, ClusterError> {
        Ok(self
            .read_raw(key)
            .await?
            .filter(|entry| !is_expired(entry.spec.expires_at.as_deref(), Timestamp::now())))
    }

    /// Builds a fresh object for `key` (create path, no `resourceVersion`).
    fn new_object(&self, key: &str, value: &[u8], version: u64, ttl: Ttl) -> ClusterCacheEntry {
        let spec =
            ClusterCacheEntrySpec::new(value, version, expiry_deadline(ttl, Timestamp::now()));
        let mut object = ClusterCacheEntry::new(&self.object_name(key), spec);
        object.metadata.namespace = Some(self.namespace.clone());
        object.metadata.labels = Some(BTreeMap::from([
            (LABEL_MANAGED_BY.to_owned(), MANAGED_BY_VALUE.to_owned()),
            (
                LABEL_PRIMITIVE.to_owned(),
                Seg::Cache.primitive_label().to_owned(),
            ),
        ]));
        object.metadata.annotations = Some(BTreeMap::from([(
            ANNOTATION_NAME.to_owned(),
            key.to_owned(),
        )]));
        object
    }

    /// Rejects a value over the configured size cap **before** any request (§6.6).
    fn check_value_size(&self, value: &[u8]) -> Result<(), ClusterError> {
        if value.len() > self.max_value_bytes {
            return Err(ClusterError::InvalidConfig {
                reason: format!(
                    "cache value of {} bytes exceeds max_value_bytes ({}); this cache is not a \
                     blob store (DESIGN.md 6.6)",
                    value.len(),
                    self.max_value_bytes
                ),
            });
        }
        Ok(())
    }

    /// Notifies the sweeper of a write's deadline (or clears it), best-effort.
    fn arm_sweeper(&self, key: &str, ttl: Ttl) {
        let Some(tx) = &self.sweep_tx else { return };
        let cmd = match expiry_deadline(ttl, Timestamp::now())
            .and_then(|s| s.parse::<Timestamp>().ok())
        {
            Some(at) => SweepCmd::Arm {
                key: key.to_owned(),
                at,
            },
            None => SweepCmd::Disarm {
                key: key.to_owned(),
            },
        };
        let _sent = tx.send(cmd);
    }
}

/// The native Kubernetes cache backend (§6).
pub struct K8sCache {
    runtime: Arc<CacheRuntime>,
    shutdown: CancellationToken,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Flips to `true` once the shared watcher's initial list completes (its first
    /// `InitDone`), so the handle's `build_and_start` can return only after the
    /// watch cache is populated (DESIGN.md §3.2). Starts `true` when `cache_watch`
    /// is off — there is no watcher to wait for.
    ready_rx: tokio::sync::watch::Receiver<bool>,
}

impl K8sCache {
    /// Builds a cache backend from a resolved client and the cache config (§3.5),
    /// spawning the shared watcher and sweeper when `cache_watch` is enabled.
    ///
    /// # Errors
    ///
    /// [`ClusterError::InvalidConfig`] when `lease_prefix` is not a legal RFC 1123
    /// label (§2.2).
    pub fn new(resolved: &ResolvedClient, config: &K8sCacheConfig) -> Result<Self, ClusterError> {
        naming::validate_lease_prefix(&config.lease_prefix)?;
        let shutdown = CancellationToken::new();
        let registry = Arc::new(CacheRegistry::new());

        let (sweep_tx, sweep_rx) = if config.cache_watch {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let runtime = Arc::new(CacheRuntime {
            client: resolved.client.clone(),
            namespace: resolved.namespace.clone(),
            lease_prefix: config.lease_prefix.clone(),
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            read_mode: config.cache_reads,
            max_value_bytes: config.max_value_bytes,
            put_max_retries: config.put_max_retries,
            registry,
            sweep_tx,
        });

        // Ready immediately when there is no watcher to wait for; otherwise the
        // watcher flips this on its first `InitDone`.
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(!config.cache_watch);

        let backend = Self {
            runtime,
            shutdown,
            tasks: Arc::new(Mutex::new(Vec::new())),
            ready_rx,
        };
        if config.cache_watch {
            backend.spawn_watcher(ready_tx);
            if let Some(rx) = sweep_rx {
                backend.spawn_sweeper(rx);
            }
        }
        Ok(backend)
    }

    /// Resolves once the shared watcher's initial list has completed (§3.2), so a
    /// handle's `build_and_start` returns only after the watch cache is populated.
    /// Returns immediately when `cache_watch` is off, or if the watcher has already
    /// exited (its sender dropped) — a caller must not block startup on a watcher
    /// that is gone.
    pub async fn wait_ready(&self) {
        let mut rx = self.ready_rx.clone();
        if *rx.borrow() {
            return;
        }
        let _ready_or_sender_gone = rx.wait_for(|&ready| ready).await;
    }

    /// Cancels the shared tasks and awaits them (§11).
    pub async fn stop(&self) {
        // Deliver a terminal `Closed(Shutdown)` to every active watch *before* the
        // watcher task is torn down (§11 step 3): the watcher returns on `shutdown`
        // without notifying its subscribers, so this is the only place they learn
        // the plugin is going away rather than a silent end-of-stream.
        self.runtime
            .registry
            .broadcast_closed(&ClusterError::Shutdown);
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

    /// Cancels the shared tasks synchronously, without awaiting them or delivering
    /// the terminal `Closed` — the teardown the handle's `Drop` uses when `stop()`
    /// was never called and cannot `.await` (§11).
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }

    /// The label selector matching every cache object this plugin owns.
    fn cache_selector() -> String {
        format!(
            "{LABEL_MANAGED_BY}={MANAGED_BY_VALUE},{LABEL_PRIMITIVE}={}",
            Seg::Cache.primitive_label()
        )
    }

    /// Spawns the single shared cache watcher feeding the registry and sweeper (§6.3).
    ///
    /// `ready_tx` is flipped to `true` on the first `InitDone` — the marker that the
    /// watcher's initial list is complete — so `wait_ready` (and through it a
    /// handle's `build_and_start`) can gate on a populated watch cache (§3.2).
    fn spawn_watcher(&self, ready_tx: tokio::sync::watch::Sender<bool>) {
        let runtime = Arc::clone(&self.runtime);
        let shutdown = self.shutdown.clone();
        let handle = tokio::spawn(async move {
            let api = runtime.api();
            let wc = watcher::Config::default().labels(&Self::cache_selector());
            let stream = watcher(api, wc);
            tokio::pin!(stream);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    event = stream.next() => {
                        match event {
                            Some(Ok(event)) => {
                                // The initial list is complete at the first
                                // `InitDone`; flip readiness before dispatching it
                                // (a re-list's later `InitDone`s are harmless no-ops).
                                if matches!(event, watcher::Event::InitDone) {
                                    let _sent = ready_tx.send(true);
                                }
                                dispatch_watch_event(&runtime, event);
                            }
                            // `kube`'s watcher retries and re-lists internally (a
                            // relist surfaces to subscribers as `Reset`), so an error
                            // item here is transient — but log it so a *persistent*
                            // failure (e.g. RBAC revoked mid-flight) is diagnosable
                            // rather than silently starving the watch cache.
                            Some(Err(err)) => tracing::warn!(
                                error = %err,
                                "cluster.provider.cache_watch_error: the cache watcher stream \
                                 returned an error; kube retries internally"
                            ),
                            None => return,
                        }
                    }
                }
            }
        });
        self.track(handle);
    }

    /// Spawns the deadline-armed sweeper task (§6.2).
    fn spawn_sweeper(&self, rx: mpsc::UnboundedReceiver<SweepCmd>) {
        let handle = tokio::spawn(sweeper::run_sweeper(
            Arc::clone(&self.runtime),
            rx,
            self.shutdown.clone(),
        ));
        self.track(handle);
    }
}

/// Applies one watcher event to the registry and the sweeper (§6.3).
fn dispatch_watch_event(runtime: &CacheRuntime, event: watcher::Event<ClusterCacheEntry>) {
    use watcher::Event;
    // Extract the sweeper hint before `classify_event` consumes the event.
    let sweep = match &event {
        Event::Apply(entry) | Event::InitApply(entry) => {
            key_of(entry).map(|key| (key, entry.spec.expires_at.clone()))
        }
        _ => None,
    };
    match watch::classify_event(event) {
        watch::CacheSignal::Event(cache_event) => runtime.registry.dispatch(&cache_event),
        watch::CacheSignal::Relisted => runtime.registry.broadcast_reset(),
        watch::CacheSignal::Quiet => {}
    }
    if let (Some(tx), Some((key, expires_at))) = (&runtime.sweep_tx, sweep) {
        let cmd = match expires_at.and_then(|s| s.parse::<Timestamp>().ok()) {
            Some(at) => SweepCmd::Arm { key, at },
            None => SweepCmd::Disarm { key },
        };
        let _sent = tx.send(cmd);
    }
}

#[async_trait]
impl ClusterCacheBackend for K8sCache {
    /// Follows the read mode (§6.5): a quorum read is linearizable; a watch-cache
    /// read is eventually consistent.
    fn consistency(&self) -> CacheConsistency {
        match self.runtime.read_mode {
            ReadMode::Quorum => CacheConsistency::Linearizable,
            ReadMode::Cached => CacheConsistency::EventuallyConsistent,
        }
    }

    /// Native prefix watch via the shared watcher's prefix registry (§6.3).
    fn features(&self) -> CacheFeatures {
        CacheFeatures::new(true)
    }

    fn provider_name(&self) -> &'static str {
        crate::provider::PROVIDER_NAME
    }

    async fn get(&self, key: &str) -> Result<Option<CacheEntry>, ClusterError> {
        match self.runtime.read_live(key).await? {
            Some(entry) => Ok(Some(entry.spec.to_cache_entry()?)),
            None => Ok(None),
        }
    }

    async fn contains(&self, key: &str) -> Result<bool, ClusterError> {
        Ok(self.runtime.read_live(key).await?.is_some())
    }

    async fn put(&self, req: PutRequest<'_>) -> Result<(), ClusterError> {
        self.runtime.check_value_size(req.value)?;
        let api = self.runtime.api();
        let name = self.runtime.object_name(req.key);

        // Bounded create-then-overwrite (§6.1): create at v1, else read + guarded
        // replace at prev+1, retrying a lost race within the budget.
        let created = self
            .runtime
            .timed(
                "create cache entry",
                guarded::create(
                    &api,
                    &self.runtime.new_object(req.key, req.value, 1, req.ttl),
                    CallSite::CachePut,
                ),
            )
            .await?;
        if matches!(created, Created::Created(_)) {
            self.runtime.arm_sweeper(req.key, req.ttl);
            return Ok(());
        }

        for _attempt in 0..=self.runtime.put_max_retries {
            let Some(mut current) = self.runtime.read_raw(req.key).await? else {
                // Vanished between create and read (a concurrent delete/sweep):
                // create again at v1 rather than spinning the budget on re-reads.
                let created = self
                    .runtime
                    .timed(
                        "create cache entry",
                        guarded::create(
                            &api,
                            &self.runtime.new_object(req.key, req.value, 1, req.ttl),
                            CallSite::CachePut,
                        ),
                    )
                    .await?;
                if matches!(created, Created::Created(_)) {
                    self.runtime.arm_sweeper(req.key, req.ttl);
                    return Ok(());
                }
                // Recreated by someone else in the meantime: re-read + guarded replace.
                continue;
            };
            let version = next_version(u64::try_from(current.spec.version).unwrap_or(0));
            current.spec = ClusterCacheEntrySpec::new(
                req.value,
                version,
                expiry_deadline(req.ttl, Timestamp::now()),
            );
            let replaced = self
                .runtime
                .timed(
                    "put cache entry",
                    guarded::replace(&api, &name, &current, CallSite::CachePut),
                )
                .await?;
            if matches!(replaced, Replaced::Applied(_)) {
                self.runtime.arm_sweeper(req.key, req.ttl);
                return Ok(());
            }
            // A 409: someone else wrote; re-read and retry within the budget.
        }
        Err(ClusterError::Provider {
            kind: cluster_sdk::ProviderErrorKind::ResourceExhausted,
            message: format!(
                "put lost {} races on key `{}`",
                self.runtime.put_max_retries, req.key
            ),
        })
    }

    async fn put_if_absent(&self, req: PutRequest<'_>) -> Result<Option<CacheEntry>, ClusterError> {
        self.runtime.check_value_size(req.value)?;
        let api = self.runtime.api();
        let object = self.runtime.new_object(req.key, req.value, 1, req.ttl);
        let created = self
            .runtime
            .timed(
                "put_if_absent cache entry",
                guarded::create(&api, &object, CallSite::PutIfAbsent),
            )
            .await?;
        match created {
            Created::Created(applied) => {
                self.runtime.arm_sweeper(req.key, req.ttl);
                Ok(Some(applied.spec.to_cache_entry()?))
            }
            Created::Exists => Ok(None),
        }
    }

    async fn delete(&self, key: &str) -> Result<bool, ClusterError> {
        let api = self.runtime.api();
        let name = self.runtime.object_name(key);
        // Unconditional delete; 404 → false (§6.1). Bounded by `request_timeout`
        // like every other request path, so a stalled API server can't hang the caller.
        self.runtime
            .timed("delete cache entry", async {
                match api.delete(&name, &kube::api::DeleteParams::default()).await {
                    Ok(_) => Ok(true),
                    Err(kube::Error::Api(status)) if status.code == 404 => Ok(false),
                    Err(err) => Err(k8s_error::map_kube_error(&err)),
                }
            })
            .await
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_version: u64,
        new_value: &[u8],
        ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        self.runtime.check_value_size(new_value)?;
        let api = self.runtime.api();
        let name = self.runtime.object_name(key);

        let Some(mut current) = self.runtime.read_live(key).await? else {
            // Absent (or expired): the expected version cannot match.
            return Err(ClusterError::CasConflict {
                key: key.to_owned(),
                current: None,
            });
        };
        let current_version = u64::try_from(current.spec.version).unwrap_or(0);
        if current_version != expected_version {
            // Mismatch on the read: the live entry is in hand, no write (§6.1).
            return Err(ClusterError::CasConflict {
                key: key.to_owned(),
                current: Some(current.spec.to_cache_entry()?),
            });
        }
        let version = next_version(expected_version);
        current.spec =
            ClusterCacheEntrySpec::new(new_value, version, expiry_deadline(ttl, Timestamp::now()));
        let replaced = self
            .runtime
            .timed(
                "compare_and_swap cache entry",
                guarded::replace(&api, &name, &current, CallSite::CompareAndSwap),
            )
            .await?;
        match replaced {
            Replaced::Applied(applied) => {
                self.runtime.arm_sweeper(key, ttl);
                Ok(applied.spec.to_cache_entry()?)
            }
            // Matched at the read but lost the guarded write: re-read once to carry
            // the live version in the conflict (§6.1).
            Replaced::Conflict(_) => {
                let current = match self.runtime.read_live(key).await? {
                    Some(entry) => Some(entry.spec.to_cache_entry()?),
                    None => None,
                };
                Err(ClusterError::CasConflict {
                    key: key.to_owned(),
                    current,
                })
            }
        }
    }

    async fn compare_and_delete(
        &self,
        key: &str,
        expected_value: &[u8],
    ) -> Result<bool, ClusterError> {
        // Override the SDK's best-effort get-then-delete with an atomic guarded
        // delete: get, and if the value matches, delete on rV+uid (§6.1).
        let Some(current) = self.runtime.read_live(key).await? else {
            return Ok(false);
        };
        if current.spec.to_cache_entry()?.value.as_slice() != expected_value {
            return Ok(false);
        }
        let api = self.runtime.api();
        let name = self.runtime.object_name(key);
        self.runtime
            .timed(
                "compare_and_delete cache entry",
                guarded::delete(&api, &name, &current.metadata),
            )
            .await
    }

    async fn watch(&self, key: &str) -> Result<CacheWatch, ClusterError> {
        let (sender, watch) = CacheWatch::channel(WATCH_BUFFER);
        self.runtime.registry.subscribe_key(key, sender);
        Ok(watch)
    }

    async fn watch_prefix(&self, prefix: &str) -> Result<CacheWatch, ClusterError> {
        let (sender, watch) = CacheWatch::channel(WATCH_BUFFER);
        self.runtime.registry.subscribe_prefix(prefix, sender);
        Ok(watch)
    }

    async fn scan_prefix(&self, prefix: &str) -> Result<Vec<String>, ClusterError> {
        let api = self.runtime.api();
        let mut continue_token: Option<String> = None;
        let mut keys = Vec::new();
        loop {
            let mut params = kube::api::ListParams::default()
                .labels(&Self::cache_selector())
                .limit(LIST_PAGE);
            if let Some(token) = &continue_token {
                params = params.continue_token(token);
            }
            let list = self
                .runtime
                .timed("scan cache", async {
                    api.list(&params)
                        .await
                        .map_err(|e| k8s_error::map_kube_error(&e))
                })
                .await?;
            let now = Timestamp::now();
            keys.extend(
                list.items
                    .iter()
                    .filter_map(|e| live_matching_key(e, prefix, now)),
            );
            continue_token = list.metadata.continue_.filter(|t| !t.is_empty());
            if continue_token.is_none() {
                break;
            }
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::{expiry_deadline, is_expired, next_version};
    use cluster_sdk::cache::Ttl;
    use k8s_openapi::jiff::{SignedDuration, Timestamp};
    use std::time::Duration;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    #[test]
    fn read_path_expiry_is_authoritative() {
        let now = ts(1_000);
        // Indefinite never expires.
        assert!(!is_expired(None, now));
        // A deadline in the past → expired (reads as absent).
        let past = (now - SignedDuration::from_secs(1)).to_string();
        assert!(is_expired(Some(&past), now));
        // Exactly now → expired (deadline reached).
        assert!(is_expired(Some(&now.to_string()), now));
        // A future deadline → live.
        let future = (now + SignedDuration::from_secs(1)).to_string();
        assert!(!is_expired(Some(&future), now));
        // A malformed deadline is not silently treated as expired.
        assert!(!is_expired(Some("not-a-timestamp"), now));
    }

    #[test]
    fn version_strictly_increases_and_saturates() {
        assert_eq!(next_version(0), 1);
        assert_eq!(next_version(1), 2);
        assert_eq!(next_version(41), 42);
        assert_eq!(next_version(u64::MAX), u64::MAX); // saturates, never wraps
    }

    #[test]
    fn expiry_deadline_is_now_plus_ttl_or_absent() {
        let now = ts(1_000);
        assert_eq!(expiry_deadline(Ttl::Indefinite, now), None);
        let at = expiry_deadline(Ttl::Of(Duration::from_secs(50)), now).unwrap();
        assert_eq!(at.parse::<Timestamp>().unwrap(), ts(1_050));
        // Sub-second TTLs are representable exactly (§2.9) — 50ms lands off a whole
        // second, unlike the Lease's integer-seconds field.
        let ms = expiry_deadline(Ttl::Of(Duration::from_millis(50)), now).unwrap();
        assert!(ms.parse::<Timestamp>().unwrap() > now);
    }
}
