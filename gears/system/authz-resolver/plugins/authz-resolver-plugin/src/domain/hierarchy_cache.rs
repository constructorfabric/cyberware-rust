//! LRU + TTL + singleflight cache for hierarchy reads.
//!
//! Backing structure: `lru::LruCache<CacheKey, CacheEntry>` for the store,
//! plus a `Mutex<HashMap<CacheKey, watch::Receiver<FlightState>>>` for
//! singleflight coordination. Concurrent `get_or_fetch` calls for the same
//! key share one fetch: the leader holds a `tokio::sync::watch::Sender` and
//! publishes the result; waiters subscribe to a `watch::Receiver`. Because
//! `watch` latches its value, a waiter that subscribes *after* the leader
//! has already published still observes the result — there is no
//! missed-signal race (the bug that `Notify` had). Errors propagate to
//! waiters but are NOT cached: a waiter that sees a failed
//! flight re-checks the cache and otherwise fetches once itself.
//!
//! Cache value type is a tagged `CacheValue` enum — one cache stores
//! heterogeneous hierarchy reads (tenant subtrees, group memberships, etc.).
//! Hierarchy operations downcast at the call site via a single match.

// Holds an `AuthZMetrics` infra handle to record cache hit/miss ratios — a
// runtime adapter, not business logic. Same sanctioned exception the
// keycloak-authn-plugin domain uses for its infra adapter handles.
#![allow(unknown_lints, de0301_no_infra_in_domain)]

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::domain::error::PluginError;
use lru::LruCache;
use tenant_resolver_sdk::models::TenantStatus;
use tokio::sync::watch;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::config::CacheConfig;
use crate::domain::clock::Clock;
use crate::domain::metrics_port::CacheKind;
use crate::infra::metrics::AuthZMetrics;
use toolkit_macros::domain_model;

/// Distinct cache keys for every hierarchy-read shape. The variants make
/// the cache type-safe — a `TenantSubtree` and a `TenantMeta` lookup for
/// the same tenant id are independent entries.
#[domain_model]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) enum CacheKey {
    /// Result of `TenantResolverClient::get_descendants` projected to IDs.
    /// `barriers_ignored` projects the request's `BarrierMode` to a bool
    /// (`true` for `Ignore`, `false` for `Respect` — the SDK enum doesn't
    /// derive `Hash`, so we project to a key-friendly shape).
    /// `status_hash` covers the `Option<Vec<TenantStatus>>` filter
    /// (sorted + hashed via `hash_status`) so two calls with the same
    /// effective filter share an entry.
    TenantSubtree {
        id: Uuid,
        barriers_ignored: bool,
        status_hash: u64,
    },
    /// Result of `TenantResolverClient::get_tenant` projected to
    /// `TenantMetadata`. Reserved: constructed only by the reserved
    /// `validate_tenant_root` path; no v1 evaluation uses it.
    #[allow(dead_code)]
    TenantMeta { id: Uuid },
    /// Result of `get_group_subtree_resource_ids(root_group_ids)`. The
    /// `ids_hash` is order-independent (sort + dedup + hash) so callers
    /// with the same set of root groups share the entry.
    GroupSubtree { ids_hash: u64 },
    /// Result of `get_group_member_resource_ids(group_ids)`. Reserved:
    /// constructed only by that reserved method (future `ExplicitGroups`).
    #[allow(dead_code)]
    GroupMembers { ids_hash: u64 },
}

/// Tagged cache values. Each hierarchy operation downcasts via a single
/// match on the known variant at its call site.
#[domain_model]
#[derive(Debug, Clone)]
pub(crate) enum CacheValue {
    TenantSubtree(Vec<Uuid>),
    /// Reserved — paired with `CacheKey::TenantMeta` (see above).
    #[allow(dead_code)]
    TenantMeta(TenantMetadata),
    /// Resolved group subtree: the flat resource-id list plus the distinct
    /// owning tenant(s) of the groups (used to AND-pair a tenant predicate
    /// into the group constraint — see `Materialization::GroupSubtree`). Both
    /// are a deterministic function of the group set, so the existing
    /// `CacheKey::GroupSubtree { ids_hash }` remains a correct key.
    GroupSubtree {
        resource_ids: Vec<Uuid>,
        owner_tenant_ids: Vec<Uuid>,
    },
    /// Reserved — paired with `CacheKey::GroupMembers` (see above).
    #[allow(dead_code)]
    GroupMembers(Vec<Uuid>),
}

impl CacheValue {
    /// Variant name for diagnostics. Used in the (should-never-happen)
    /// cache-shape-mismatch errors so they don't log the full value, which
    /// carries tenant/resource IDs.
    pub(crate) fn variant_name(&self) -> &'static str {
        match self {
            CacheValue::TenantSubtree(_) => "TenantSubtree",
            CacheValue::TenantMeta(_) => "TenantMeta",
            CacheValue::GroupSubtree { .. } => "GroupSubtree",
            CacheValue::GroupMembers(_) => "GroupMembers",
        }
    }
}

/// Narrow projection of `TenantInfo`. The plugin only needs these four
/// fields for constraint generation + ownership validation; storing the
/// full `TenantInfo` (with `name` and `tenant_type`) would waste memory.
// Reserved: only populated/consumed by the reserved `validate_tenant_root`
// path. Kept so the projected tenant shape stays documented.
#[domain_model]
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct TenantMetadata {
    pub(crate) id: Uuid,
    pub(crate) status: TenantStatus,
    pub(crate) self_managed: bool,
    pub(crate) parent_id: Option<Uuid>,
}

#[domain_model]
#[derive(Debug, Clone)]
struct CacheEntry {
    /// `Arc` so a cache hit copies a refcount rather than the whole value
    /// while the store mutex is held. A `TenantSubtree`/`GroupSubtree`
    /// payload can hold up to `MAX_PAGINATED_ITEMS` (`100_000`) ids, and this
    /// is the hot allow path — the memcpy used to sit inside the critical
    /// section, lengthening it for every waiter on the same lock.
    value: Arc<CacheValue>,
    inserted_at: Instant,
}

#[domain_model]
pub(crate) struct HierarchyCache {
    store: Mutex<LruCache<CacheKey, CacheEntry>>,
    in_flight: Mutex<HashMap<CacheKey, watch::Receiver<FlightState>>>,
    clock: Arc<dyn Clock>,
    ttl: Duration,
    singleflight_enabled: bool,
    metrics: Arc<AuthZMetrics>,
}

impl HierarchyCache {
    pub(crate) fn new(
        config: &CacheConfig,
        clock: Arc<dyn Clock>,
        metrics: Arc<AuthZMetrics>,
    ) -> Self {
        // No clamp: `CacheConfig::max_entries` is `NonZeroUsize`, so a
        // `max_entries: 0` is rejected when the config is loaded rather than
        // silently becoming a 10 000-entry cache nobody configured.
        let capacity = config.max_entries;

        if config.event_invalidation.enabled {
            warn!(
                "HierarchyCache: event_invalidation.enabled = true is reserved for v2; \
                 v1 ships TTL-only invalidation and ignores this flag"
            );
        }

        Self {
            store: Mutex::new(LruCache::new(capacity)),
            in_flight: Mutex::new(HashMap::new()),
            clock,
            ttl: Duration::from_secs(config.ttl_seconds),
            singleflight_enabled: config.singleflight_enabled,
            metrics,
        }
    }

    /// Read-through fetch with TTL + LRU + singleflight semantics. The
    /// fetch closure is invoked at most once per (key, leader) when
    /// singleflight is enabled. Errors are propagated to all waiters but
    /// never stored.
    // Leader/waiter singleflight state machine; the branches are sequential
    // phases of one protocol and read worse split apart.
    #[allow(clippy::cognitive_complexity)]
    pub(crate) async fn get_or_fetch<F, Fut>(
        &self,
        key: CacheKey,
        fetch: F,
    ) -> Result<Arc<CacheValue>, PluginError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<CacheValue, PluginError>> + Send,
    {
        // Fast path: live cache hit. Record hit/miss for the per-cache-type
        // hit-ratio gauge (§3.13 authz_evaluation_cache_hit_ratio).
        if let Some(value) = self.try_get_live(&key) {
            debug!(?key, result = "hit", "hierarchy cache");
            self.metrics
                .record_cache_access(cache_metric_kind(&key), true);
            return Ok(value);
        }
        self.metrics
            .record_cache_access(cache_metric_kind(&key), false);

        if !self.singleflight_enabled {
            debug!(?key, result = "miss_no_singleflight", "hierarchy cache");
            return self.fetch_and_store(key, fetch).await;
        }

        // Singleflight: register as leader or as waiter. A waiter whose leader
        // is cancelled or panics before publishing re-coalesces by looping here
        // rather than by re-entering `get_or_fetch`: iteration keeps the stack
        // flat, so a run of abandoned leaders on one key cannot grow the
        // waiter's frame without bound. The hit/miss counters above are
        // deliberately outside this loop — one cache access is one miss,
        // however many abandoned leaders it has to outlive.
        loop {
            match self.acquire_lease(key) {
                Lease::Leader(mut lease) => {
                    debug!(?key, result = "miss_leader", "hierarchy cache");
                    let result = self.fetch_and_store(key, fetch).await;
                    // Publish the full outcome (success OR error) so waiters share
                    // it instead of each re-fetching — no thundering herd when the
                    // downstream is failing. Errors are shared but never written to
                    // the LRU (fetch_and_store only stores on Ok), so "errors are
                    // not cached" still holds. The lease guard removes the
                    // in_flight entry on drop (incl. on cancel/panic).
                    lease.publish(FlightState::Done(clone_result(&result)));
                    return result;
                }
                Lease::Waiter(mut rx) => {
                    debug!(?key, result = "miss_waiter", "hierarchy cache");
                    // Wait for the leader to publish or drop. `changed()` errors
                    // once the channel closes, but the latest value stays latched
                    // and readable via `borrow()` — so ignore the result and read
                    // the latched state directly (robust to publish-then-drop).
                    _ = rx.changed().await;
                    // Scope the borrow so it is dropped before the next iteration
                    // re-borrows the cache via acquire_lease.
                    {
                        if let FlightState::Done(result) = &*rx.borrow() {
                            // Share the leader's outcome (value or cloned error).
                            return clone_result(result);
                        }
                    }
                    // `Pending`/`Abandoned`: the leader was cancelled/panicked
                    // before publishing. Re-check the live cache first (a
                    // *different* leader may have just stored it), then RE-COALESCE
                    // on the next iteration rather than every waiter fetching
                    // independently. The abandoned leader's Drop already removed the
                    // in_flight entry, so exactly one of the woken waiters wins
                    // acquire_lease and becomes the new leader while the rest wait
                    // on it — preserving singleflight (no thundering herd) even
                    // under a cancellation storm.
                    if let Some(value) = self.try_get_live(&key) {
                        return Ok(value);
                    }
                }
            }
        }
    }

    fn try_get_live(&self, key: &CacheKey) -> Option<Arc<CacheValue>> {
        let now = self.clock.now();
        let mut store = match self.store.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match store.get(key) {
            // `Arc::clone` — a refcount bump, not a deep copy of the payload.
            Some(entry) if now.duration_since(entry.inserted_at) < self.ttl => {
                Some(Arc::clone(&entry.value))
            }
            Some(_) => {
                // Expired — remove so the LRU's "least-recently-used"
                // bookkeeping doesn't hold on to dead entries.
                store.pop(key);
                None
            }
            None => None,
        }
    }

    async fn fetch_and_store<F, Fut>(
        &self,
        key: CacheKey,
        fetch: F,
    ) -> Result<Arc<CacheValue>, PluginError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<CacheValue, PluginError>> + Send,
    {
        let value = Arc::new(fetch().await?);
        let now = self.clock.now();
        let mut store = match self.store.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        store.put(
            key,
            CacheEntry {
                value: Arc::clone(&value),
                inserted_at: now,
            },
        );
        Ok(value)
    }

    fn acquire_lease(&self, key: CacheKey) -> Lease<'_> {
        let mut in_flight = match self.in_flight.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(rx) = in_flight.get(&key) {
            return Lease::Waiter(rx.clone());
        }
        let (tx, rx) = watch::channel(FlightState::Pending);
        in_flight.insert(key, rx);
        Lease::Leader(LeaderLease {
            in_flight: &self.in_flight,
            key,
            sender: tx,
            published: false,
        })
    }
}

/// Shared state of an in-flight singleflight fetch, published by the leader
/// over a `watch` channel.
/// - `Done(result)` — leader finished; waiters share this outcome (success or
///   a cloned error) instead of re-fetching. Errors are shared but never
///   written to the LRU, so "errors are not cached" still holds.
/// - `Abandoned` — leader was cancelled/panicked before publishing; the lease
///   guard sends this so waiters fall back to their own fetch.
#[domain_model]
#[derive(Debug)]
enum FlightState {
    Pending,
    Done(Result<Arc<CacheValue>, PluginError>),
    Abandoned,
}

#[domain_model]
enum Lease<'a> {
    Leader(LeaderLease<'a>),
    Waiter(watch::Receiver<FlightState>),
}

/// RAII guard for the singleflight leader. Guarantees the `in_flight` entry is
/// removed and waiters are unblocked even if the leader's fetch future is
/// cancelled or panics — otherwise the key would be wedged (coalescing lost,
/// map entry leaked, waiters blocked on a dropped sender).
#[domain_model]
struct LeaderLease<'a> {
    in_flight: &'a Mutex<HashMap<CacheKey, watch::Receiver<FlightState>>>,
    key: CacheKey,
    sender: watch::Sender<FlightState>,
    published: bool,
}

impl LeaderLease<'_> {
    /// Publish the fetch outcome to waiters; afterwards `Drop` only cleans up.
    fn publish(&mut self, state: FlightState) {
        _ = self.sender.send(state);
        self.published = true;
    }
}

impl Drop for LeaderLease<'_> {
    fn drop(&mut self) {
        if !self.published {
            // Cancelled/panicked before publishing — wake waiters so none
            // block on a dropped sender; they fall back to their own fetch.
            _ = self.sender.send(FlightState::Abandoned);
        }
        let mut in_flight = match self.in_flight.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        in_flight.remove(&self.key);
    }
}

/// Clone a fetch result for sharing with singleflight waiters.
///
/// [`PluginError`] is `Clone` (the SDK error type is not, which is one reason
/// the pipeline carries its own), so this is a refcount bump on the payload and
/// a shallow clone on the error — no variant-by-variant rebuild.
fn clone_result(
    result: &Result<Arc<CacheValue>, PluginError>,
) -> Result<Arc<CacheValue>, PluginError> {
    match result {
        // Refcount bump — waiters share the leader's payload rather than
        // each receiving a deep copy of it.
        Ok(value) => Ok(Arc::clone(value)),
        Err(err) => Err(err.clone()),
    }
}

/// Map a `CacheKey` to its metric `cache_type` kind for the hit-ratio gauge.
fn cache_metric_kind(key: &CacheKey) -> CacheKind {
    match key {
        CacheKey::TenantSubtree { .. } => CacheKind::TenantSubtree,
        CacheKey::TenantMeta { .. } => CacheKind::TenantMeta,
        CacheKey::GroupSubtree { .. } => CacheKind::GroupSubtree,
        CacheKey::GroupMembers { .. } => CacheKind::GroupMembers,
    }
}

/// Hash the resolved `tenant_status` filter for use as part of a cache
/// key. Sorts the variants before hashing so `[Active, Suspended]` and
/// `[Suspended, Active]` produce the same key. Uses the pinned `as_smallint`
/// encoding (Active=1, Suspended=2, Deleted=3) rather than `Debug`
/// formatting, so the key is stable and never depends on a derived `Debug`
/// impl that could change without notice.
pub(crate) fn hash_status(status: &[TenantStatus]) -> u64 {
    let mut codes: Vec<i16> = status.iter().map(|s| s.as_smallint()).collect();
    codes.sort_unstable();
    codes.dedup();
    let mut hasher = DefaultHasher::new();
    for code in &codes {
        code.hash(&mut hasher);
    }
    hasher.finish()
}

/// Hash a `Vec<Uuid>` order-independently. Sorts + dedups before hashing
/// so callers passing the same set in different orders share a cache entry.
pub(crate) fn hash_ids(ids: &[Uuid]) -> u64 {
    let mut sorted: Vec<Uuid> = ids.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut hasher = DefaultHasher::new();
    for id in &sorted {
        id.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
#[path = "hierarchy_cache_tests.rs"]
mod tests;
