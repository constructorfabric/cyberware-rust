//! The public `ClusterCacheV1` facade — a thin, cloneable handle delegating to
//! the resolved `Arc<dyn ClusterCacheBackend>`.

use std::sync::Arc;

use toolkit::client_hub::ClientHub;

use crate::cache::backend::ClusterCacheBackend;
use crate::cache::resolver::CacheResolverBuilder;
use crate::cache::types::{CacheConsistency, CacheEntry, CacheFeatures, PutRequest, Ttl};
use crate::cache::watch::CacheWatch;
use crate::error::ClusterError;
use crate::restart::ResubscribeFuture;

/// The public cache facade. Construct via [`ClusterCacheV1::resolver`]; cloning
/// is cheap (an `Arc` bump).
///
/// Use [`scoped`](Self::scoped) to carve a composable sub-namespace: every key
/// (and watch/scan prefix) is auto-prefixed on the write path and stripped on
/// the read path (DESIGN §3.8).
#[derive(Clone)]
pub struct ClusterCacheV1 {
    inner: Arc<dyn ClusterCacheBackend>,
}

impl ClusterCacheV1 {
    /// Wraps a resolved backend. Crate-internal: consumers obtain a facade
    /// through the resolver.
    pub(crate) fn from_backend(inner: Arc<dyn ClusterCacheBackend>) -> Self {
        Self { inner }
    }

    /// Static entry point: returns a fluent resolver bound to `hub`.
    pub fn resolver(hub: &ClientHub) -> CacheResolverBuilder<'_> {
        CacheResolverBuilder::new(hub)
    }

    /// Returns a sub-namespaced view of this cache: every key (and the prefix of
    /// `watch_prefix`/`scan_prefix`) is auto-prefixed with `prefix + "/"` on the
    /// write path and stripped on the read path (DESIGN §3.8). Scoping composes —
    /// `cache.scoped("a")?.scoped("b")?` makes the backend observe `"a/b/<key>"`.
    ///
    /// # Errors
    /// Returns [`ClusterError::InvalidName`] if `prefix` violates the scope-prefix
    /// rule: slash-separated segments of `[a-zA-Z0-9_-]` with no leading, trailing,
    /// or empty (doubled-slash) segments, max 255 chars.
    pub fn scoped(&self, prefix: &str) -> Result<Self, ClusterError> {
        let prefix = crate::scope::validated_prefix(prefix)?;
        Ok(Self::from_backend(Arc::new(
            crate::cache::ScopedCacheBackend::new(Arc::clone(&self.inner), prefix),
        )))
    }

    /// The bound backend's declared consistency class.
    #[must_use]
    pub fn consistency(&self) -> CacheConsistency {
        self.inner.consistency()
    }

    /// The bound backend's native capability flags.
    #[must_use]
    pub fn features(&self) -> CacheFeatures {
        self.inner.features()
    }

    /// Returns the versioned entry for `key`, or `None` if absent.
    ///
    /// # Errors
    /// Propagates any [`ClusterError`] from the backend.
    pub async fn get(&self, key: &str) -> Result<Option<CacheEntry>, ClusterError> {
        crate::scope::validate_cache_key(key)?;
        self.inner.get(key).await
    }

    /// Stores `req.value` under `req.key`, incrementing the version; overwrites if
    /// present.
    ///
    /// # Errors
    /// Propagates any [`ClusterError`] from the backend.
    pub async fn put(&self, req: PutRequest<'_>) -> Result<(), ClusterError> {
        crate::scope::validate_cache_key(req.key)?;
        self.inner.put(req).await
    }

    /// Removes `key`, returning whether it existed.
    ///
    /// # Errors
    /// Propagates any [`ClusterError`] from the backend.
    pub async fn delete(&self, key: &str) -> Result<bool, ClusterError> {
        crate::scope::validate_cache_key(key)?;
        self.inner.delete(key).await
    }

    /// Existence check for `key`.
    ///
    /// # Errors
    /// Propagates any [`ClusterError`] from the backend.
    pub async fn contains(&self, key: &str) -> Result<bool, ClusterError> {
        crate::scope::validate_cache_key(key)?;
        self.inner.contains(key).await
    }

    /// Atomically creates `req.key` only if absent.
    ///
    /// # Errors
    /// Propagates any [`ClusterError`] from the backend.
    pub async fn put_if_absent(
        &self,
        req: PutRequest<'_>,
    ) -> Result<Option<CacheEntry>, ClusterError> {
        crate::scope::validate_cache_key(req.key)?;
        self.inner.put_if_absent(req).await
    }

    /// Atomic version-based compare-and-swap.
    ///
    /// # Errors
    /// Returns [`ClusterError::CasConflict`] on version mismatch, or another
    /// [`ClusterError`] from the backend.
    pub async fn compare_and_swap(
        &self,
        key: &str,
        expected_version: u64,
        new_value: &[u8],
        ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        crate::scope::validate_cache_key(key)?;
        self.inner
            .compare_and_swap(key, expected_version, new_value, ttl)
            .await
    }

    /// Watches an exact key.
    ///
    /// The returned watch carries a resubscribe seam, so
    /// [`CacheWatch::auto_restart`] can transparently re-`watch` this key on a
    /// retryable terminal close.
    ///
    /// # Errors
    /// Returns [`ClusterError::Unsupported`] with `feature: "watch"` when the
    /// backend declares no exact-watch support, or propagates any other
    /// [`ClusterError`] from the backend.
    pub async fn watch(&self, key: &str) -> Result<CacheWatch, ClusterError> {
        crate::scope::validate_cache_key(key)?;
        let mut watch = self.inner.watch(key).await?;
        install_exact_watch_seam(Arc::clone(&self.inner), key.to_owned(), &mut watch);
        Ok(watch)
    }

    /// Watches a key prefix.
    ///
    /// The returned watch carries a resubscribe seam (see [`watch`](Self::watch)).
    ///
    /// # Errors
    /// Returns [`ClusterError::InvalidName`] if `prefix` opens a reserved
    /// keyspace (see `reject_reserved_prefix`),
    /// [`ClusterError::Unsupported`] when the backend lacks native
    /// prefix-watch support, or another [`ClusterError`] from the backend.
    pub async fn watch_prefix(&self, prefix: &str) -> Result<CacheWatch, ClusterError> {
        reject_reserved_prefix(prefix)?;
        let mut watch = self.inner.watch_prefix(prefix).await?;
        install_prefix_watch_seam(Arc::clone(&self.inner), prefix.to_owned(), &mut watch);
        Ok(watch)
    }

    /// Lists the keys currently present under `prefix`.
    ///
    /// # Errors
    /// Returns [`ClusterError::InvalidName`] if `prefix` opens a reserved
    /// keyspace (see `reject_reserved_prefix`),
    /// [`ClusterError::Unsupported`] when the backend lacks scan support,
    /// or another [`ClusterError`] from the backend.
    pub async fn scan_prefix(&self, prefix: &str) -> Result<Vec<String>, ClusterError> {
        reject_reserved_prefix(prefix)?;
        self.inner.scan_prefix(prefix).await
    }

    /// Opt-in polling prefix watch: synthesizes `watch_prefix` semantics on a
    /// backend that declares no native support
    /// (`features().prefix_watch == false`), by polling
    /// [`scan_prefix`](Self::scan_prefix) + `get` every `interval` (DESIGN §3.12).
    ///
    /// This is **not** free — see [`PollingPrefixWatch`] for the cost and the
    /// recommendation to prefer a native-prefix-watch backend at scale. Dropping
    /// the returned [`CacheWatch`] stops the polling task. Pair with
    /// [`watch_prefix`](Self::watch_prefix) (native) when the backend supports it.
    ///
    /// A zero `interval` does not panic: the returned watch yields a single
    /// terminal [`CacheWatchEvent::Closed`](crate::cache::CacheWatchEvent::Closed)
    /// carrying [`ClusterError::InvalidConfig`] (non-retryable) — see
    /// [`PollingPrefixWatch::spawn`]. Disappeared keys are reported as
    /// [`CacheEvent::Deleted`](crate::cache::CacheEvent::Deleted), never `Expired`.
    ///
    /// A prefix opening a reserved keyspace (see `reject_reserved_prefix`) is
    /// refused the same way, since this method has no `Result` to refuse
    /// through: the returned watch yields one terminal `Closed` carrying
    /// [`ClusterError::InvalidName`] and never polls.
    #[must_use]
    pub fn watch_prefix_polling(&self, prefix: &str, interval: std::time::Duration) -> CacheWatch {
        if let Err(refusal) = reject_reserved_prefix(prefix) {
            return closed_watch(refusal);
        }
        let mut watch =
            crate::cache::PollingPrefixWatch::spawn(Arc::clone(&self.inner), prefix, interval);
        install_polling_watch_seam(
            Arc::clone(&self.inner),
            prefix.to_owned(),
            interval,
            &mut watch,
        );
        watch
    }
}

/// Refuses a `prefix` that opens a keyspace the cluster gear reserves for its
/// own records — the lease store the default lock and leader-election backends
/// write to (see [`reserved_lease_cache`](crate::reserved_lease_cache)).
///
/// The three prefix-taking methods need their own check because they cannot use
/// the one every key-taking method uses. `validate_cache_key` would be wrong
/// here twice over: it rejects `""`, which is the legitimate and common way to
/// say "everything in my scope", and a prefix is not a key, so the rest of the
/// key rule does not apply to it either. What *does* apply is the one thing the
/// key rule expresses only incidentally — a reserved prefix names a keyspace
/// this API does not serve — so that is tested directly, on the sigil, which
/// covers every reserved space rather than one spelling of one prefix.
///
/// Without this a `scan_prefix("$lease/")` enumerated every held lock and
/// election name, and a `watch_prefix("$lease/")` streamed every lease mutation
/// as it happened. Neither could read a lease *value* — `get` validates — so
/// what leaked was names and timing rather than forgeable state, which is
/// disclosure, not a broken mutual exclusion. Still not this API's to serve.
///
/// # Errors
/// Returns [`ClusterError::InvalidName`] if `prefix` opens a reserved keyspace.
fn reject_reserved_prefix(prefix: &str) -> Result<(), ClusterError> {
    if crate::scope::is_reserved_key(prefix) {
        return Err(ClusterError::InvalidName {
            name: prefix.to_owned(),
            reason: crate::scope::RESERVED_KEY_RULE,
        });
    }
    Ok(())
}

/// A watch that is already over: it carries `error` as its single terminal
/// event and nothing else.
///
/// The refusal path for [`watch_prefix_polling`](ClusterCacheV1::watch_prefix_polling),
/// which returns a `CacheWatch` rather than a `Result` and so has to report a
/// rejected argument in-band. The same shape the zero-interval rejection in
/// [`PollingPrefixWatch::spawn`](crate::cache::PollingPrefixWatch::spawn)
/// already uses, minus its task: a capacity-1 channel takes the event
/// synchronously, and dropping the sender ends the stream behind it.
fn closed_watch(error: ClusterError) -> CacheWatch {
    let (sender, watch) = CacheWatch::channel(1);
    // Infallible: the receiver is alive and the buffer is empty.
    sender
        .try_send(crate::cache::CacheWatchEvent::Closed(error))
        .ok();
    watch
}

/// Installs a self-reinstalling resubscribe seam that re-runs `watch(key)` on
/// the bound backend. Each reconnected watch is re-seamed, so
/// [`CacheWatch::auto_restart`] reconnects *repeatedly* on successive retryable
/// closes, not just once. Capturing the backend (whose `async_trait` methods
/// return a concretely-`Send` boxed future) rather than the facade avoids a
/// `Send` auto-trait inference cycle.
fn install_exact_watch_seam(
    backend: Arc<dyn ClusterCacheBackend>,
    key: String,
    watch: &mut CacheWatch,
) {
    watch.set_resubscribe(move || -> ResubscribeFuture<CacheWatch> {
        let backend = Arc::clone(&backend);
        let key = key.clone();
        Box::pin(async move {
            let mut fresh = backend.watch(&key).await?;
            install_exact_watch_seam(Arc::clone(&backend), key, &mut fresh);
            Ok(fresh)
        })
    });
}

/// As [`install_exact_watch_seam`], but re-runs `watch_prefix(prefix)`.
fn install_prefix_watch_seam(
    backend: Arc<dyn ClusterCacheBackend>,
    prefix: String,
    watch: &mut CacheWatch,
) {
    watch.set_resubscribe(move || -> ResubscribeFuture<CacheWatch> {
        let backend = Arc::clone(&backend);
        let prefix = prefix.clone();
        Box::pin(async move {
            let mut fresh = backend.watch_prefix(&prefix).await?;
            install_prefix_watch_seam(Arc::clone(&backend), prefix, &mut fresh);
            Ok(fresh)
        })
    });
}

/// As [`install_exact_watch_seam`], but re-spawns the polling polyfill (which
/// can also surface a retryable backend error as `Closed`).
fn install_polling_watch_seam(
    backend: Arc<dyn ClusterCacheBackend>,
    prefix: String,
    interval: std::time::Duration,
    watch: &mut CacheWatch,
) {
    watch.set_resubscribe(move || -> ResubscribeFuture<CacheWatch> {
        let backend = Arc::clone(&backend);
        let prefix = prefix.clone();
        Box::pin(async move {
            let mut fresh =
                crate::cache::PollingPrefixWatch::spawn(Arc::clone(&backend), &prefix, interval);
            install_polling_watch_seam(Arc::clone(&backend), prefix, interval, &mut fresh);
            Ok(fresh)
        })
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;

    use super::ClusterCacheV1;
    use crate::cache::backend::ClusterCacheBackend;
    use crate::cache::types::{
        CacheConsistency, CacheEntry, CacheEvent, CacheFeatures, PutRequest,
    };
    use crate::cache::watch::{CacheWatch, CacheWatchEvent};
    use crate::error::ClusterError;
    use crate::scope::RESERVED_LEASE_PREFIX;

    /// The lease key a reserved prefix would reach if the facade let one
    /// through — a real one, spelled as the lock default writes it.
    const LEASE_KEY: &str = "$lease/lock/ledger";

    /// A backend that serves one lease key and one consumer key, and records
    /// every prefix it is asked for. Both halves matter: the recording is how a
    /// refusal is told apart from a call that reached the backend and found
    /// nothing, and the seeded lease key is what makes "nothing leaked" an
    /// answer from a populated store rather than an empty one agreeing with
    /// anything.
    struct RecordingBackend {
        prefixes: Mutex<Vec<String>>,
    }

    impl RecordingBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                prefixes: Mutex::new(Vec::new()),
            })
        }

        fn prefixes(&self) -> Vec<String> {
            self.prefixes.lock().expect("lock").clone()
        }

        fn record(&self, prefix: &str) {
            self.prefixes.lock().expect("lock").push(prefix.to_owned());
        }
    }

    #[async_trait]
    impl ClusterCacheBackend for RecordingBackend {
        fn consistency(&self) -> CacheConsistency {
            CacheConsistency::Linearizable
        }

        fn features(&self) -> CacheFeatures {
            CacheFeatures::new(true)
        }

        async fn get(&self, _key: &str) -> Result<Option<CacheEntry>, ClusterError> {
            Ok(Some(CacheEntry {
                value: b"v".to_vec(),
                version: 1,
            }))
        }

        async fn put(&self, _req: PutRequest<'_>) -> Result<(), ClusterError> {
            Ok(())
        }

        async fn delete(&self, _key: &str) -> Result<bool, ClusterError> {
            Ok(true)
        }

        async fn contains(&self, _key: &str) -> Result<bool, ClusterError> {
            Ok(true)
        }

        async fn put_if_absent(
            &self,
            _req: PutRequest<'_>,
        ) -> Result<Option<CacheEntry>, ClusterError> {
            Ok(None)
        }

        async fn compare_and_swap(
            &self,
            _key: &str,
            _expected_version: u64,
            _new_value: &[u8],
            _ttl: crate::cache::types::Ttl,
        ) -> Result<CacheEntry, ClusterError> {
            Err(ClusterError::Unsupported {
                feature: "compare_and_swap",
            })
        }

        async fn watch(&self, _key: &str) -> Result<CacheWatch, ClusterError> {
            Ok(CacheWatch::channel(4).1)
        }

        async fn watch_prefix(&self, prefix: &str) -> Result<CacheWatch, ClusterError> {
            self.record(prefix);
            Ok(CacheWatch::channel(4).1)
        }

        async fn scan_prefix(&self, prefix: &str) -> Result<Vec<String>, ClusterError> {
            self.record(prefix);
            Ok([LEASE_KEY, "ledger"]
                .into_iter()
                .filter(|key| key.starts_with(prefix))
                .map(str::to_owned)
                .collect())
        }
    }

    fn facade(backend: &Arc<RecordingBackend>) -> ClusterCacheV1 {
        ClusterCacheV1::from_backend(Arc::clone(backend) as Arc<dyn ClusterCacheBackend>)
    }

    /// The three prefix-taking methods are the ones `validate_cache_key` never
    /// covered, and a reserved prefix is exactly what they used to forward: a
    /// Profile 1 consumer could enumerate every held lock and election name
    /// through `scan_prefix`, or stream every lease mutation through either
    /// `watch_prefix`. The refusal has to reach the backend not at all — asserted
    /// on the recorded prefixes, since an empty result is also what a backend
    /// with nothing under the prefix would return.
    #[tokio::test]
    async fn the_prefix_methods_refuse_a_reserved_prefix() {
        let backend = RecordingBackend::new();
        let cache = facade(&backend);

        let scanned = cache
            .scan_prefix(RESERVED_LEASE_PREFIX)
            .await
            .expect_err("`scan_prefix` must refuse the reserved keyspace");
        let watched = cache
            .watch_prefix(RESERVED_LEASE_PREFIX)
            .await
            .expect_err("`watch_prefix` must refuse the reserved keyspace");

        for (method, error) in [("scan_prefix", scanned), ("watch_prefix", watched)] {
            match error {
                ClusterError::InvalidName { name, reason } => {
                    assert_eq!(name, RESERVED_LEASE_PREFIX);
                    assert!(
                        reason.contains("reserved keyspace"),
                        "`{method}` must say why: {reason}"
                    );
                }
                other => panic!("`{method}` refused with the wrong error: {other:?}"),
            }
        }

        assert!(
            backend.prefixes().is_empty(),
            "a refused prefix must never reach the backend, got {:?}",
            backend.prefixes()
        );
    }

    /// The same refusal on the polling polyfill, which returns a `CacheWatch`
    /// rather than a `Result` and so has to report it in-band. Terminal and
    /// silent: one `Closed`, then end of stream, and no poll ever runs.
    #[tokio::test]
    async fn the_polling_prefix_watch_refuses_a_reserved_prefix_terminally() {
        let backend = RecordingBackend::new();
        let cache = facade(&backend);

        let mut watch = cache.watch_prefix_polling(RESERVED_LEASE_PREFIX, Duration::from_millis(1));

        match watch.recv().await {
            Some(CacheWatchEvent::Closed(ClusterError::InvalidName { name, reason })) => {
                assert_eq!(name, RESERVED_LEASE_PREFIX);
                assert!(reason.contains("reserved keyspace"), "{reason}");
            }
            other => panic!("a reserved polling watch must close on the refusal: {other:?}"),
        }
        assert!(
            watch.recv().await.is_none(),
            "`Closed` is terminal: nothing follows it"
        );
        assert!(
            backend.prefixes().is_empty(),
            "the poll loop must never have started, got {:?}",
            backend.prefixes()
        );
    }

    /// **The regression guard on the fix itself.** `""` is the ordinary way to
    /// say "everything in my scope" — every scoped consumer's whole keyspace —
    /// and `validate_cache_key("")` rejects it, so reusing the key validator here
    /// would have broken every prefix consumer in the codebase while looking like
    /// a security fix. The check tests the sigil instead, and this pins that
    /// choice on all three methods.
    #[tokio::test]
    async fn the_empty_prefix_still_reaches_the_backend_on_every_prefix_method() {
        let backend = RecordingBackend::new();
        let cache = facade(&backend);

        let keys = cache
            .scan_prefix("")
            .await
            .expect("`\"\"` is a legal prefix");
        assert!(
            keys.contains(&"ledger".to_owned()),
            "`scan_prefix(\"\")` must still enumerate the consumer keyspace, got {keys:?}"
        );
        cache
            .watch_prefix("")
            .await
            .expect("`\"\"` is a legal prefix");

        // The polling variant proves it *polled* rather than closing: the poll
        // loop's first tick scans, and the seeded key surfaces as a `Changed`.
        let mut polling = cache.watch_prefix_polling("", Duration::from_millis(5));
        let first = tokio::time::timeout(Duration::from_secs(5), polling.recv())
            .await
            .expect("the first poll lands well inside the timeout");
        assert!(
            matches!(
                first,
                Some(CacheWatchEvent::Event(CacheEvent::Changed { .. }))
            ),
            "a polling watch on `\"\"` must poll, not close: {first:?}"
        );

        assert_eq!(
            backend.prefixes(),
            ["", "", ""],
            "every prefix method must have forwarded `\"\"` to the backend verbatim"
        );
    }
}
