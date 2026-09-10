//! Consumer-side discovery wiring for eventual readiness.
//!
//! Bridges the service [`DirectoryClient`](crate::DirectoryClient) into the
//! contract layer's [`EndpointResolver`], and carries the
//! [`ConsumerRegistration`]s emitted by `#[toolkit::consumes]` that the
//! runtime's proxy-wiring phase replays at startup.
//!
//! Unconditional: consuming another gear over a discovered REST transport is
//! the normal case, and `#[toolkit::consumes]` emits its registration
//! unconditionally too. Putting this behind a feature only created a way to
//! build a gear whose declared dependencies were silently never wired.

use std::sync::Arc;

use async_trait::async_trait;

use cf_system_sdks::directory::{DirectoryInvalidArgument, DirectoryNotFound};

use crate::DirectoryClient;
use crate::client_hub::ClientHub;

/// Re-export so the `#[toolkit::consumes]`-generated wire fn can name the
/// resolver type as `toolkit::discovery::EndpointResolver` without the consuming
/// gear crate needing a direct `toolkit-contract` dependency.
pub use toolkit_contract::runtime::resolving::{EndpointResolver, ResolveError};

/// Adapts the service [`DirectoryClient`] into the contract-layer
/// [`EndpointResolver`] consumed by `DirectoryResolvingClient`.
///
/// Keeps `toolkit-contract` free of a dependency on the directory SDK: the
/// adapter lives here in `toolkit`, which already owns `DirectoryClient`.
///
/// Successful resolutions are memoized for a short TTL ([`Self::TTL`]) so a
/// per-call resolving client does not incur a directory round-trip on every
/// business call (the `OoP` directory is a network hop). Only `Ok(Some(uri))` is
/// cached; `Ok(None)` / `Err` always re-resolve. The TTL is deliberately short
/// so endpoint churn self-heals within ~TTL.
pub struct DirectoryEndpointResolver {
    client: Arc<dyn DirectoryClient>,
    cache: parking_lot::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
}

impl DirectoryEndpointResolver {
    /// Memoization window for successful resolutions.
    ///
    /// Public because it bounds observable behaviour: a provider that moves is
    /// followed within roughly this window, not instantly, since the cache
    /// holds only successes and is not invalidated by a failed call.
    pub const TTL: std::time::Duration = std::time::Duration::from_millis(1500);

    /// Wrap a [`DirectoryClient`] with a short-TTL resolution cache.
    #[must_use]
    pub fn new(client: Arc<dyn DirectoryClient>) -> Self {
        Self {
            client,
            cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn cached(&self, gear: &str) -> Option<String> {
        self.cache
            .lock()
            .get(gear)
            .filter(|(_, at)| at.elapsed() < Self::TTL)
            .map(|(uri, _)| uri.clone())
    }

    fn store(&self, gear: &str, uri: &str) {
        self.cache
            .lock()
            .insert(gear.to_owned(), (uri.to_owned(), std::time::Instant::now()));
    }
}

#[async_trait]
impl EndpointResolver for DirectoryEndpointResolver {
    async fn resolve_endpoint(&self, gear: &str) -> Result<Option<String>, ResolveError> {
        // Fresh cache hit avoids the directory round-trip.
        if let Some(uri) = self.cached(gear) {
            return Ok(Some(uri));
        }
        // Preserve the trait's `Ok(None)` (no live instance / not ready) vs
        // `Err` (directory backend unreachable) distinction so a real directory
        // outage surfaces at `warn` rather than being silently treated as
        // "provider not ready". The directory returns the typed
        // `DirectoryNotFound` sentinel for the not-found case; anything else is
        // a genuine backend/transport failure (relevant for the OoP gRPC
        // directory client).
        match self.client.resolve_rest_service(gear).await {
            Ok(ep) => {
                self.store(gear, &ep.uri);
                Ok(Some(ep.uri))
            }
            Err(e) if e.downcast_ref::<DirectoryNotFound>().is_some() => Ok(None),
            Err(e) if e.downcast_ref::<DirectoryInvalidArgument>().is_some() => {
                // A malformed provider name is a permanent configuration error,
                // not a transient outage: re-resolving can never succeed. Log
                // it distinctly from the generic backend-failure path (at
                // `error`, not the caller's transient `warn`) so the actionable
                // "fix your config" message is not mistaken for a directory
                // outage, and still return `Err` so the call fails.
                tracing::error!(
                    gear,
                    error = %e,
                    "provider name permanently rejected by the directory (invalid \
                     argument); this is a static configuration error, not a directory \
                     outage, and will never resolve. Check the consumed gear name"
                );
                Err(ResolveError::new(gear, e))
            }
            Err(e) => Err(ResolveError::new(gear, e)),
        }
    }
}

/// An [`EndpointResolver`] that never resolves any gear (`Ok(None)` for every
/// lookup). Used by the proxy-wiring phase when no [`DirectoryClient`] is present
/// in the [`ClientHub`]: co-located (local) consumers still short-circuit and
/// wire successfully, while remote consumers register as *unresolved* readiness
/// gates (so `/readyz` correctly stays 503) rather than being silently skipped.
pub struct NullEndpointResolver;

#[async_trait]
impl EndpointResolver for NullEndpointResolver {
    async fn resolve_endpoint(&self, _gear: &str) -> Result<Option<String>, ResolveError> {
        Ok(None)
    }
}

/// An [`EndpointResolver`] that always resolves to a single, fixed endpoint —
/// the ADR-0004 static-endpoint override escape hatch for local development and
/// integration testing. It bypasses the service directory: a consumer configured
/// with `gears.<owner>.config.consumer_wiring.<dep> = "http://host:port"` wires a
/// resolving client whose endpoint is this constant. Not for production use.
pub struct StaticEndpointResolver {
    endpoint: String,
}

impl StaticEndpointResolver {
    /// Wrap a fixed base endpoint URI.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[async_trait]
impl EndpointResolver for StaticEndpointResolver {
    async fn resolve_endpoint(&self, _gear: &str) -> Result<Option<String>, ResolveError> {
        Ok(Some(self.endpoint.clone()))
    }
}

/// Outcome of a [`ConsumerRegistration::wire`] call: whether the dependency was
/// satisfied by a co-located compile-time impl (no directory dependency) or by
/// a directory-resolving REST client that still needs endpoint resolution.
///
/// The runtime uses this to gate readiness: `Local` deps are readiness-resolved
/// immediately (no `/readyz` gating, no probe loop), while `Remote` deps gate
/// readiness until the directory resolves them (ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireOutcome {
    /// A compile-time (in-process) impl was already registered — it won the
    /// `hub.try_get_local` short-circuit; no remote binding was created.
    Local,
    /// A directory-resolving REST client was registered; its endpoint is
    /// resolved lazily/at readiness-probe time.
    Remote,
}

/// A consumer→provider contract wiring, registered by `#[toolkit::consumes]`
/// via `inventory::submit!` and replayed by the runtime's proxy-wiring phase.
///
/// `wire` is a non-capturing `fn` (the provider gear name is inlined into the
/// generated body). It must:
///   1. short-circuit returning [`WireOutcome::Local`] if a compile-time (local)
///      impl is already registered (`hub.try_get_local::<dyn Trait>().is_some()`),
///      so in-process providers win;
///   2. otherwise register a directory-resolving REST client under the contract
///      trait via [`ClientHub::register_remote_proxy`] (using the supplied
///      [`EndpointResolver`]) and return [`WireOutcome::Remote`].
///
/// Step 1 must use `try_get_local`, not `try_get`. The hub is keyed by type, so
/// when two consumers in one process depend on the same contract the second to
/// wire would find the first one's proxy under `try_get`, report `Local`, and
/// have its dependency marked readiness-resolved — flipping `/readyz` to 200
/// while the provider is still absent. `register_remote_proxy` is what makes
/// that distinction visible to the hub.
pub struct ConsumerRegistration {
    /// Gear that declares the dependency (diagnostics).
    pub owner_gear: &'static str,
    /// Provider gear name the contract resolves against (diagnostics).
    pub dep_gear: &'static str,
    /// Wiring action: register the client into the hub; reports whether the
    /// dependency was satisfied locally or bound to a remote client. See
    /// [`WireFn`] for the argument roles.
    pub wire: WireFn,
}

/// The wiring function pointer emitted by `#[toolkit::consumes]`. Registers the
/// client into the `ClientHub`, given the endpoint resolver and the process's
/// platform-plane credential source (threaded onto a directory-resolving remote
/// client). Aliased to keep the fn-pointer type readable.
pub type WireFn = fn(
    &ClientHub,
    Arc<dyn EndpointResolver>,
    Option<&toolkit_contract::runtime::config::InternalTokenProvider>,
) -> anyhow::Result<WireOutcome>;

inventory::collect!(ConsumerRegistration);

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use cf_system_sdks::directory::{RegisterInstanceInfo, ServiceEndpoint, ServiceInstanceInfo};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts `resolve_rest_service` calls; all other methods are unused here.
    struct CountingDirectory {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl DirectoryClient for CountingDirectory {
        async fn resolve_grpc_service(&self, _: &str) -> anyhow::Result<ServiceEndpoint> {
            unimplemented!()
        }
        async fn resolve_rest_service(&self, gear: &str) -> anyhow::Result<ServiceEndpoint> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ServiceEndpoint::new(format!("http://{gear}.local")))
        }
        async fn get_openapi_spec(&self, _: &str) -> anyhow::Result<String> {
            unimplemented!()
        }
        async fn list_instances(&self, _: &str) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
            unimplemented!()
        }
        async fn list_all_instances(&self) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
            unimplemented!()
        }
        async fn register_instance(&self, _: RegisterInstanceInfo) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn deregister_instance(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn send_heartbeat(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn memoizes_successful_resolution_within_ttl() {
        let dir = Arc::new(CountingDirectory {
            calls: AtomicUsize::new(0),
        });
        let resolver = DirectoryEndpointResolver::new(dir.clone());

        for _ in 0..3 {
            let ep = resolver.resolve_endpoint("billing").await.unwrap();
            assert_eq!(ep.as_deref(), Some("http://billing.local"));
        }

        assert_eq!(
            dir.calls.load(Ordering::SeqCst),
            1,
            "successful resolution must be memoized within the TTL (one directory lookup)"
        );
    }

    #[tokio::test]
    async fn static_resolver_always_returns_fixed_endpoint() {
        let resolver = StaticEndpointResolver::new("http://localhost:8081");
        assert_eq!(
            resolver
                .resolve_endpoint("anything")
                .await
                .unwrap()
                .as_deref(),
            Some("http://localhost:8081")
        );
        assert_eq!(
            resolver.resolve_endpoint("other").await.unwrap().as_deref(),
            Some("http://localhost:8081")
        );
    }

    #[tokio::test]
    async fn null_resolver_never_resolves() {
        assert_eq!(
            NullEndpointResolver
                .resolve_endpoint("billing")
                .await
                .unwrap(),
            None
        );
    }

    /// Answers every lookup with a caller-chosen failure, so the resolver's
    /// two error branches can be told apart.
    struct FailingDirectory {
        make_error: fn(&str) -> anyhow::Error,
    }

    #[async_trait]
    impl DirectoryClient for FailingDirectory {
        async fn resolve_grpc_service(&self, _: &str) -> anyhow::Result<ServiceEndpoint> {
            unimplemented!()
        }
        async fn resolve_rest_service(&self, gear: &str) -> anyhow::Result<ServiceEndpoint> {
            Err((self.make_error)(gear))
        }
        async fn get_openapi_spec(&self, _: &str) -> anyhow::Result<String> {
            unimplemented!()
        }
        async fn list_instances(&self, _: &str) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
            unimplemented!()
        }
        async fn list_all_instances(&self) -> anyhow::Result<Vec<ServiceInstanceInfo>> {
            unimplemented!()
        }
        async fn register_instance(&self, _: RegisterInstanceInfo) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn deregister_instance(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn send_heartbeat(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn not_found_sentinel_resolves_to_ok_none() {
        // "The provider has not registered yet" is a routine startup condition,
        // not a failure: it must come back as `Ok(None)` so the readiness probe
        // logs it at `debug` and the resolving client reports `Unresolved`
        // rather than warning on every call.
        let dir = Arc::new(FailingDirectory {
            make_error: |gear| DirectoryNotFound::new(format!("gear {gear}")).into(),
        });
        let resolver = DirectoryEndpointResolver::new(dir);

        assert_eq!(resolver.resolve_endpoint("billing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn backend_failure_resolves_to_err() {
        // Anything that is not the sentinel is a genuine directory outage and
        // must stay an error, so a stuck 503 has a diagnostic trail.
        let dir = Arc::new(FailingDirectory {
            make_error: |_| anyhow::anyhow!("connection refused"),
        });
        let resolver = DirectoryEndpointResolver::new(dir);

        assert!(resolver.resolve_endpoint("billing").await.is_err());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn invalid_argument_resolves_to_err_and_logs_config_error() {
        // A malformed provider name is a permanent configuration error, handled
        // by the dedicated arm. Unlike the not-found sentinel (`Ok(None)`), it
        // must stay an `Err` so the call fails rather than being treated as a
        // provider that is merely not ready yet. It must also be logged
        // distinctly at `error` (a static configuration error) so it is not
        // folded into the generic backend-failure path, which emits no log of
        // its own at the resolver.
        let dir = Arc::new(FailingDirectory {
            make_error: |gear| DirectoryInvalidArgument::new(format!("bad name {gear}")).into(),
        });
        let resolver = DirectoryEndpointResolver::new(dir);

        assert!(resolver.resolve_endpoint("Bad Name").await.is_err());

        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .find(|line| line.contains("permanently rejected by the directory"))
            {
                Some(line) if line.contains("ERROR") => Ok(()),
                Some(_) => Err("invalid-argument arm logged at the wrong level".to_owned()),
                None => {
                    Err("invalid-argument arm must emit a distinct config-error log".to_owned())
                }
            }
        });
    }

    /// Mirrors the body `#[toolkit::consumes]` generates, so the wiring
    /// contract is exercised without going through `inventory` (which is
    /// link-time global and cannot hold two registrations built by a test).
    #[allow(
        clippy::unnecessary_wraps,
        reason = "mirrors the fallible signature of ConsumerRegistration::wire so the test \
                  exercises the same shape the macro emits"
    )]
    fn generated_wire_body<C>(hub: &ClientHub, proxy: Arc<C>) -> anyhow::Result<WireOutcome>
    where
        C: ?Sized + Send + Sync + 'static,
    {
        if hub.try_get_local::<C>().is_some() {
            return Ok(WireOutcome::Local);
        }
        if hub.has_remote_proxy::<C>() {
            return Ok(WireOutcome::Remote);
        }
        hub.register_remote_proxy::<C>(proxy);
        Ok(WireOutcome::Remote)
    }

    trait Payments: Send + Sync {}
    struct PaymentsProxy;
    impl Payments for PaymentsProxy {}
    struct PaymentsLocal;
    impl Payments for PaymentsLocal {}

    #[test]
    fn two_consumers_of_one_contract_both_report_remote() {
        // Two gears in one process consuming the same contract from the same
        // provider. Whichever wires second used to find the first one's proxy,
        // conclude the dependency was local, and get it marked
        // readiness-resolved — with both registrations sharing a `dep_gear`,
        // that flipped the very entry the unresolved remote dep was gating on,
        // and `/readyz` went green with no provider up.
        let hub = ClientHub::new();

        let first = generated_wire_body::<dyn Payments>(&hub, Arc::new(PaymentsProxy)).unwrap();
        let second = generated_wire_body::<dyn Payments>(&hub, Arc::new(PaymentsProxy)).unwrap();

        assert_eq!(first, WireOutcome::Remote);
        assert_eq!(
            second,
            WireOutcome::Remote,
            "the second consumer must still gate readiness, not short-circuit to Local"
        );
    }

    #[test]
    fn a_genuine_local_impl_still_wins() {
        // The short-circuit must keep working for its actual purpose: a
        // co-located provider registered during init means no HTTP hop and no
        // readiness gate.
        let hub = ClientHub::new();
        hub.register::<dyn Payments>(Arc::new(PaymentsLocal));

        let outcome = generated_wire_body::<dyn Payments>(&hub, Arc::new(PaymentsProxy)).unwrap();

        assert_eq!(outcome, WireOutcome::Local);
    }

    #[tokio::test]
    async fn local_directory_client_not_found_reaches_the_resolver_as_ok_none() {
        // End-to-end over the real in-process client rather than a stub: this
        // is the pairing that was broken (the client returned a bare `anyhow`,
        // so the resolver could never produce `Ok(None)`).
        let dir = Arc::new(crate::directory::LocalDirectoryClient::new(Arc::new(
            crate::runtime::GearManager::new(),
        )));
        let resolver = DirectoryEndpointResolver::new(dir);

        assert_eq!(
            resolver.resolve_endpoint("never-registered").await.unwrap(),
            None
        );
    }
}
