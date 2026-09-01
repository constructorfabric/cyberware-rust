//! Production [`PermissionCatalog`] backed by `dyn TypesRegistryClient`.
//!
//! Read-through with a short-TTL snapshot cache and a bounded
//! staleness budget:
//!
//! * **Within TTL** (`age < cache_ttl`, default 30s) — serve cached
//!   without touching the registry.
//! * **Beyond TTL, refresh succeeds** — atomically swap in the new
//!   snapshot, return it.
//! * **Beyond TTL, refresh fails, snapshot age < `stale_threshold`**
//!   (default `2 × cache_ttl` = 60s) — emit a `warn!` event and
//!   serve the stale snapshot. Graceful degradation across brief
//!   upstream blips.
//! * **Beyond TTL, refresh fails, snapshot age ≥ `stale_threshold`** —
//!   surface `PermissionCatalogError::Registry`. REST maps this to
//!   503 via `catalog_error_to_rbac`, so sustained registry outages
//!   become loud at the API surface instead of dragging stale
//!   authorisation metadata indefinitely.
//!
//! Caching is sound because RBAC permissions are declared at compile
//! time via `gts_instance!` and registered at process startup; the
//! catalog does not change at runtime within a single deployment.
//! Mirrors the policy on the account-management gear's
//! `infra::types_registry::metadata_schema_registry`
//! — "caching is allowed when mutations are infrequent and trait
//! updates can tolerate propagation delay."

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use serde::Deserialize;
use toolkit_gts::gts_id;
use types_registry_sdk::{GtsInstance, InstanceQuery, TypesRegistryClient};

use crate::domain::permission_catalog::{
    AuthzPermission, CatalogCursor, CatalogListPage, PermissionCatalog, PermissionCatalogError,
    PermissionCatalogFilter, paginate_catalog_page,
};

const PERMISSION_INSTANCE_PATTERN_WILDCARD: &str = gts_id!("cf.toolkit.authz.permission.v1~*");

/// Default snapshot freshness window. Permissions are stable across a
/// deployment (registered at startup via `gts_instance!`), so a 30s
/// staleness budget is well below any operator-visible signal. Tests
/// that need to assert per-call upstream behaviour pass
/// `Duration::ZERO` via [`TypesRegistryPermissionCatalog::with_cache_ttl`].
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(30);

/// Default multiplier for the staleness budget — the snapshot can be
/// served past TTL on registry failure for `cache_ttl * N` total
/// before refusing the read. `N = 2` mirrors the simplest "graceful
/// degradation by one TTL window" policy and is the value baked into
/// [`TypesRegistryPermissionCatalog::new`].
const STALE_THRESHOLD_MULTIPLIER: u32 = 2;

/// Wire-shape mirror of `toolkit_gts::AuthzPermissionV1` so the rbac crate
/// doesn't need a type-level dep on `toolkit_gts`.
#[derive(Debug, Deserialize)]
struct AuthzPermissionV1Wire {
    id: String,
    resource_type: String,
    action: String,
    display_name: String,
}

/// Cached whole-catalog snapshot. The hot path clones the inner `Arc`
/// (not the slice); on TTL expiry the caller refetches and atomically
/// swaps in a fresh snapshot. The payload is `Arc<[_]>` rather than
/// `Arc<Vec<_>>` so the snapshot costs one allocation, not two, and one
/// pointer hop instead of two on every read.
struct CachedSnapshot {
    fetched_at: Instant,
    permissions: Arc<[AuthzPermission]>,
}

pub struct TypesRegistryPermissionCatalog {
    client: Arc<dyn TypesRegistryClient>,
    cache_ttl: Duration,
    /// Upper bound on how stale the cached snapshot may get while the
    /// registry is unreachable. `age < cache_ttl` serves cached
    /// without an upstream call; `cache_ttl ≤ age < stale_threshold`
    /// serves stale on refresh failure (with a warn); `age ≥
    /// stale_threshold` refuses to serve and surfaces the registry
    /// error (which the REST layer maps to 503).
    stale_threshold: Duration,
    snapshot: ArcSwapOption<CachedSnapshot>,
}

impl TypesRegistryPermissionCatalog {
    /// Default-TTL constructor. Use this in production wiring; tests
    /// that want to disable the cache (or shorten the TTL for an
    /// expiry assertion) should use [`Self::with_cache_ttl`].
    pub(crate) fn new(client: Arc<dyn TypesRegistryClient>) -> Self {
        Self::with_cache_ttl(client, DEFAULT_CACHE_TTL)
    }

    /// Construct the catalog with an explicit TTL.
    ///
    /// `cache_ttl == Duration::ZERO` disables the cache — every call
    /// refetches from the registry. Used by tests that assert
    /// uncached per-invocation behaviour against the mock. The stale
    /// threshold is derived as `cache_ttl * STALE_THRESHOLD_MULTIPLIER`;
    /// tests that need a custom value use
    /// [`Self::with_cache_ttl_and_stale_threshold`].
    pub(crate) fn with_cache_ttl(
        client: Arc<dyn TypesRegistryClient>,
        cache_ttl: Duration,
    ) -> Self {
        let stale_threshold = cache_ttl
            .checked_mul(STALE_THRESHOLD_MULTIPLIER)
            .unwrap_or(cache_ttl);
        Self::with_cache_ttl_and_stale_threshold(client, cache_ttl, stale_threshold)
    }

    /// Construct the catalog with an explicit TTL and staleness
    /// budget. Used internally by `new` for production wiring and by
    /// the companion test file to assert the stale-503 boundary
    /// without sleeping a full minute. `pub(crate)` (not `pub`) so
    /// callers outside the crate keep going through `new`.
    pub(crate) fn with_cache_ttl_and_stale_threshold(
        client: Arc<dyn TypesRegistryClient>,
        cache_ttl: Duration,
        stale_threshold: Duration,
    ) -> Self {
        Self {
            client,
            cache_ttl,
            stale_threshold,
            snapshot: ArcSwapOption::empty(),
        }
    }

    /// Return the whole catalog, serving the cached snapshot when
    /// fresh and refetching from the registry otherwise.
    ///
    /// Failure semantics:
    /// * Refresh succeeds → atomic swap, return fresh.
    /// * Refresh fails, snapshot age `< stale_threshold` → warn and
    ///   return the stale snapshot. Graceful degradation across
    ///   brief upstream blips.
    /// * Refresh fails, snapshot age `≥ stale_threshold` (or no
    ///   snapshot exists) → propagate
    ///   [`PermissionCatalogError::Registry`]; sustained outages
    ///   surface as 503 at the REST layer.
    async fn fetch_all(&self) -> Result<Arc<[AuthzPermission]>, PermissionCatalogError> {
        if !self.cache_ttl.is_zero()
            && let Some(snapshot) = self.snapshot.load_full()
            && snapshot.fetched_at.elapsed() < self.cache_ttl
        {
            return Ok(Arc::clone(&snapshot.permissions));
        }

        // Fetch outside any lock. Concurrent first-fetchers may race;
        // last writer wins. The fetch is idempotent, so the few extra
        // round-trips during cache warm-up are cheaper than a
        // single-flight gate.
        match self.fetch_from_registry().await {
            Ok(fresh) => {
                let permissions: Arc<[AuthzPermission]> = Arc::from(fresh);
                if !self.cache_ttl.is_zero() {
                    self.snapshot.store(Some(Arc::new(CachedSnapshot {
                        fetched_at: Instant::now(),
                        permissions: Arc::clone(&permissions),
                    })));
                }
                Ok(permissions)
            }
            Err(err) => {
                // Stale-503 boundary: keep serving the last known
                // snapshot for one extra TTL window after the
                // upstream went unreachable; refuse the read once we
                // cross the staleness budget so callers see the
                // outage at the API surface (mapped to 503 by
                // `catalog_error_to_rbac`).
                if !self.cache_ttl.is_zero()
                    && let Some(snapshot) = self.snapshot.load_full()
                    && snapshot.fetched_at.elapsed() < self.stale_threshold
                {
                    tracing::warn!(
                        target: "rbac.permission_catalog",
                        age_secs = snapshot.fetched_at.elapsed().as_secs(),
                        stale_threshold_secs = self.stale_threshold.as_secs(),
                        error = %err,
                        "types-registry refresh failed; serving stale snapshot within staleness budget"
                    );
                    return Ok(Arc::clone(&snapshot.permissions));
                }
                // Past the staleness budget (or no snapshot at all): this error
                // reaches the caller as an opaque 503, and `catalog_error_to_rbac`
                // drops the upstream diagnostic when it maps the variant. Log it
                // here — this is the last frame that still holds the cause.
                tracing::warn!(
                    target: "rbac.permission_catalog",
                    error = %err,
                    "types-registry read failed and no snapshot is within the staleness budget"
                );
                Err(err)
            }
        }
    }

    /// Raw registry fetch — used both for the first cache miss and for
    /// every call when caching is disabled. The output is sorted by
    /// `id` ascending so consumers can binary-search the snapshot
    /// without resorting per call.
    async fn fetch_from_registry(&self) -> Result<Vec<AuthzPermission>, PermissionCatalogError> {
        let instances: Vec<GtsInstance> = self
            .client
            .list_instances(InstanceQuery::new().with_pattern(PERMISSION_INSTANCE_PATTERN_WILDCARD))
            .await
            .map_err(|e| PermissionCatalogError::Registry(e.to_string()))?;

        let mut out = Vec::with_capacity(instances.len());
        for inst in instances {
            let id_for_err = inst.id.to_string();
            let wire: AuthzPermissionV1Wire = serde_json::from_value(inst.object).map_err(|e| {
                PermissionCatalogError::Deserialize {
                    id: id_for_err.clone(),
                    cause: e.to_string(),
                }
            })?;
            out.push(AuthzPermission {
                id: wire.id,
                resource_type: wire.resource_type,
                action: wire.action,
                display_name: wire.display_name,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

#[async_trait]
impl PermissionCatalog for TypesRegistryPermissionCatalog {
    async fn list_permissions(
        &self,
        filter: PermissionCatalogFilter,
        cursor: Option<CatalogCursor>,
        limit: u32,
    ) -> Result<CatalogListPage, PermissionCatalogError> {
        // `all` is sorted by id ascending (see `fetch_from_registry`).
        // The shared helper seeks via binary search and paginates the
        // in-memory slice in either direction, so forward and backward
        // cursor semantics stay identical to the in-memory fake.
        let all = self.fetch_all().await?;
        paginate_catalog_page(&all, &filter, cursor.as_ref(), limit)
    }

    async fn exists(
        &self,
        operation: &str,
        target_type: &str,
    ) -> Result<bool, PermissionCatalogError> {
        let all = self.fetch_all().await?;
        Ok(all
            .iter()
            .any(|p| p.action == operation && p.resource_type == target_type))
    }
}

#[cfg(test)]
#[path = "types_registry_permission_catalog_tests.rs"]
mod types_registry_permission_catalog_tests;
