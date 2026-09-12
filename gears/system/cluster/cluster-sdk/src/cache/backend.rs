//! The pluggable cache backend trait every cache provider implements.

use async_trait::async_trait;

use crate::assert_dyn_compatible;
use crate::cache::types::{CacheConsistency, CacheEntry, CacheFeatures, PutRequest, Ttl};
use crate::cache::watch::CacheWatch;
use crate::error::ClusterError;

/// The plugin contract a cache backend implements.
///
/// The facade holds an `Arc<dyn ClusterCacheBackend>`, so the trait must be
/// dyn-compatible (asserted at the bottom of this module). Every fallible
/// method returns [`ClusterError`].
#[async_trait]
pub trait ClusterCacheBackend: Send + Sync {
    /// The backend's declared consistency class.
    #[must_use]
    fn consistency(&self) -> CacheConsistency;

    /// The backend's native capability flags.
    #[must_use]
    fn features(&self) -> CacheFeatures;

    /// The concrete provider type name, used for diagnostics — for example the
    /// `provider` field of
    /// [`ClusterError::CapabilityNotMet`](crate::error::ClusterError::CapabilityNotMet).
    ///
    /// The default returns the implementing type's name via
    /// [`std::any::type_name`]. Resolving the name *through the trait object*
    /// this way is deliberate: `std::any::type_name_of_val` applied to a
    /// `&dyn ClusterCacheBackend` only ever yields the trait-object name, never
    /// the concrete backend, because it is monomorphized on the static type. A
    /// provided method is monomorphized per implementer, so this body reports
    /// the real backend through the vtable.
    #[must_use]
    fn provider_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Returns the versioned entry for `key`, or `None` if absent.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the backend operation fails. Never errors
    /// for a missing key (`Ok(None)`).
    async fn get(&self, key: &str) -> Result<Option<CacheEntry>, ClusterError>;

    /// Stores `req.value` under `req.key`, incrementing the version; overwrites if
    /// present.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the backend operation fails.
    async fn put(&self, req: PutRequest<'_>) -> Result<(), ClusterError>;

    /// Removes `key`, returning whether it existed (best-effort `true` when the
    /// backend cannot determine prior existence).
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the backend operation fails.
    async fn delete(&self, key: &str) -> Result<bool, ClusterError>;

    /// Existence check.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the backend operation fails.
    async fn contains(&self, key: &str) -> Result<bool, ClusterError>;

    /// Atomically creates `req.key` only if absent: `Some(entry)` when created,
    /// `None` when the key already existed.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the backend operation fails.
    async fn put_if_absent(&self, req: PutRequest<'_>) -> Result<Option<CacheEntry>, ClusterError>;

    /// Atomic version-based compare-and-swap.
    ///
    /// # Errors
    /// Returns [`ClusterError::CasConflict`] (carrying the current entry when
    /// cheaply obtainable) when `expected_version` no longer matches, or
    /// another [`ClusterError`] if the backend operation fails.
    async fn compare_and_swap(
        &self,
        key: &str,
        expected_version: u64,
        new_value: &[u8],
        ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError>;

    /// Atomic value-guarded compare-and-swap: replaces `key` with `new_value`
    /// only if its current value equals `expected_value`.
    ///
    /// The value-guarded counterpart of [`compare_and_swap`](Self::compare_and_swap),
    /// and the swap-shaped sibling of
    /// [`compare_and_delete`](Self::compare_and_delete). It exists for the same
    /// reason the value-guarded delete does: a key that is deleted and re-created
    /// resets its version (a fresh `put_if_absent` is version 1), so a version
    /// guard can alias a successor's fresh claim, whereas a guard on the exact
    /// bytes that were read cannot. A lease steal is CAS'd *in place* — never
    /// deleted and reinserted, which would reset the fence — so it needs a
    /// value-guarded swap, not a value-guarded delete.
    ///
    /// The default implementation is a non-atomic [`get`](Self::get)-then-
    /// [`compare_and_swap`](Self::compare_and_swap) and is therefore only
    /// best-effort: it narrows but does not close the read-to-swap window. A
    /// backend with an atomic store **should override** this with a genuinely
    /// atomic compare-and-swap-on-value so the guard holds under contention.
    ///
    /// # Errors
    /// Returns [`ClusterError::CasConflict`] (carrying the current entry when
    /// cheaply obtainable) when `expected_value` no longer matches, or another
    /// [`ClusterError`] if the backend operation fails.
    async fn compare_and_swap_value(
        &self,
        key: &str,
        expected_value: &[u8],
        new_value: &[u8],
        ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        match self.get(key).await? {
            Some(entry) if entry.value.as_slice() == expected_value => {
                self.compare_and_swap(key, entry.version, new_value, ttl)
                    .await
            }
            current => Err(ClusterError::CasConflict {
                key: key.to_owned(),
                current,
            }),
        }
    }

    /// Atomically deletes `key` only if its current value equals
    /// `expected_value`, returning whether it was deleted.
    ///
    /// The value-guarded counterpart of [`delete`](Self::delete): a holder that
    /// stored a unique owner token under `key` can release *its own* claim
    /// without racing a successor that has since taken the key over (after the
    /// holder's TTL lapsed) — the successor wrote a different value, so this is a
    /// no-op against it. A value mismatch or an absent key returns `Ok(false)`,
    /// never an error, mirroring a guarded release that finds someone else has
    /// already moved on (cf. the k8s elector's `holderIdentity`-guarded release).
    ///
    /// Guarding on the value rather than the version is deliberate: a key that is
    /// deleted and re-created resets its version (a fresh `put_if_absent` is
    /// version 1), so a version guard could alias a successor's fresh claim,
    /// whereas a unique owner token cannot.
    ///
    /// The default implementation is a non-atomic [`get`](Self::get)-then-
    /// [`delete`](Self::delete) and is therefore only best-effort: it narrows but
    /// does not close the read-to-delete window. A backend with an atomic store
    /// (such as the linearizable default) **should override** this with a
    /// genuinely atomic compare-and-delete so the guard holds under contention.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the backend operation fails.
    async fn compare_and_delete(
        &self,
        key: &str,
        expected_value: &[u8],
    ) -> Result<bool, ClusterError> {
        match self.get(key).await? {
            Some(entry) if entry.value.as_slice() == expected_value => self.delete(key).await,
            _ => Ok(false),
        }
    }

    /// Watches an exact key.
    ///
    /// # Errors
    /// Returns [`ClusterError::Unsupported`] with `feature: "watch"` when the
    /// backend declares no exact-watch support (`features().watch == false`) —
    /// a backend that cannot serve a watch must say so rather than return a
    /// channel that never fires — or another [`ClusterError`] if the watch
    /// cannot be established.
    async fn watch(&self, key: &str) -> Result<CacheWatch, ClusterError>;

    /// Watches a key prefix.
    ///
    /// # Errors
    /// Returns [`ClusterError::Unsupported`] with `feature: "prefix_watch"` when
    /// the backend declares no native prefix-watch support (callers may polyfill
    /// via the scoping feature), or another [`ClusterError`] on failure.
    async fn watch_prefix(&self, prefix: &str) -> Result<CacheWatch, ClusterError>;

    /// Lists the keys currently present under `prefix`.
    ///
    /// Used by the polling prefix-watch polyfill
    /// ([`PollingPrefixWatch`](crate::cache::PollingPrefixWatch)) to enumerate the
    /// keyspace it diffs, since the watch-union contract carries no value and the
    /// cache exposes no value-bearing scan. The default returns
    /// [`ClusterError::Unsupported`] with `feature: "scan_prefix"` so the contract
    /// extension is additive — a backend opts in only by overriding this method.
    ///
    /// # Errors
    /// Returns [`ClusterError::Unsupported`] with `feature: "scan_prefix"` by
    /// default, or another [`ClusterError`] on failure.
    async fn scan_prefix(&self, prefix: &str) -> Result<Vec<String>, ClusterError> {
        let _ = prefix;
        Err(ClusterError::Unsupported {
            feature: "scan_prefix",
        })
    }

    /// A cheap, non-mutating liveness check on the backend's own resources — its
    /// connection pool, its socket, the store's reachability.
    ///
    /// Read by the gear's composite readiness healthcheck (DESIGN.md
    /// §4.4), which turns a failing probe into
    /// [`ProfileHealth::Degraded`](crate::dto::ProfileHealth::Degraded) on the
    /// profile's descriptor — pulling *that* profile's consumers out of rotation
    /// without touching consumers of healthy profiles.
    ///
    /// The default returns `Ok(())`, so the extension is additive and dyn-safe: a
    /// plugin that does not implement it still compiles. That default is
    /// deliberately `Ok(())` rather than [`ClusterError::Unsupported`], unlike
    /// [`scan_prefix`](Self::scan_prefix): `scan_prefix` is a capability a caller
    /// branches on, whereas this is advisory liveness, and "cannot be probed"
    /// must not read as "is failing". A backend with no remote resource to check
    /// — an in-process map — is genuinely always serving.
    ///
    /// Implementations must be cheap, non-mutating (`SELECT 1`, never a write)
    /// and prompt: the composite healthcheck bounds every probe by its own
    /// budget and reports the profile `Degraded` on timeout, and the framework in
    /// turn bounds the whole check by `oop_http.healthcheck_timeout_ms` (500 ms
    /// by default).
    ///
    /// A **delegating** backend must forward this method. The default's `Ok(())`
    /// would otherwise mask the wrapped backend's real state, which is not a
    /// theoretical concern: every plugin wraps its cache in
    /// [`InstrumentedCache`](crate::observability::InstrumentedCache), so an
    /// unforwarded `probe` would report `Ok` for every deployed profile.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the backend cannot currently serve.
    async fn probe(&self) -> Result<(), ClusterError> {
        Ok(())
    }
}

assert_dyn_compatible!(ClusterCacheBackend);
