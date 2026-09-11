//! Shared test fixtures (`DESIGN.md:614`) - real cluster-mode wiring for
//! dispatcher tests (`eb-dispatcher-routing`), not the comment-only shell
//! `gears-rust#4427` removed. `harness`/`api_v1`/`request`/`response`/`body`
//! (`eb-rest-handlers`) are the HTTP integration-test harness for this
//! crate's own ingest/delivery REST handlers - a separate concern from the
//! dispatcher fixtures below, file-for-file mirroring `oagw`'s
//! `test_support/` (design.md "Test harness follows oagw's two-layer
//! `test_support` pattern").

pub mod api_v1;
pub mod authz_doubles;
pub mod body;
// Not every harness convenience method (builder customization beyond
// tenant/subject id, additional response assertions like `assert_header`)
// is exercised by the specific handler tests that exist today (task group
// 11) - a general-purpose harness API surface, mirroring oagw's, is
// allowed to have unused corners rather than being trimmed to exactly what
// today's tests happened to need.
#[allow(
    dead_code,
    reason = "general-purpose harness API surface, not all of it exercised yet"
)]
pub mod harness;
#[allow(
    dead_code,
    reason = "general-purpose harness API surface, not all of it exercised yet"
)]
pub mod request;
#[allow(
    dead_code,
    reason = "general-purpose harness API surface, not all of it exercised yet"
)]
pub mod response;
pub mod type_registry;

pub use authz_doubles::DenyingAuthZ;
#[allow(
    unused_imports,
    reason = "general-purpose harness API surface, not all of it exercised yet"
)]
pub use body::IntoBody;
pub use body::Json;
#[allow(
    unused_imports,
    reason = "general-purpose harness API surface, not all of it exercised yet"
)]
pub use harness::{EventBrokerHarness, EventBrokerHarnessBuilder};
#[allow(
    unused_imports,
    reason = "general-purpose harness API surface, not all of it exercised yet"
)]
pub use request::RequestCase;
#[allow(
    unused_imports,
    reason = "general-purpose harness API surface, not all of it exercised yet"
)]
pub use response::TestResponse;
pub use type_registry::StaticTypesRegistry;

/// The GTS **type** identifier an event names, for a test that builds an
/// `Event` directly. A concrete event type is a derived type schema, so the
/// identifier ends in `~` and `GtsInstanceId` would refuse it.
///
/// # Panics
/// Panics if `raw` is not a well-formed GTS type identifier - a fixture
/// mistake, not a runtime condition.
#[must_use]
pub fn event_type_id(raw: &str) -> gts::GtsTypeId {
    gts::GtsTypeId::try_new(raw)
        .unwrap_or_else(|err| panic!("'{raw}' is not a valid GTS type id: {err}"))
}

use std::net::SocketAddr;
use std::sync::Arc;

use cluster::{ClusterConfig, ClusterHandle, ClusterWiring, ProviderRegistry};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit::directory::{DirectoryClient, RegisterInstanceInfo, ServiceEndpoint};
use toolkit::runtime::GearManager;
use uuid::Uuid;

use crate::domain::cluster::EventBrokerCluster;

/// `standalone_event_broker_cluster()` wires the toolkit's own real,
/// production `LocalDirectoryClient` (backed by a fresh `GearManager`, the
/// same in-memory store `gear-orchestrator` itself runs standalone mode on -
/// no DB, no network) into the same `ClientHub` production code paths
/// (`module::EventBrokerModule::register_self`, `infra::dispatcher::forward`)
/// resolve their `Arc<dyn DirectoryClient>` from, so dispatcher tests
/// exercise the real directory-client behavior (round-robin, instance
/// state) rather than a second, independently-maintained mock of it.
///
/// Fetches the `LocalDirectoryClient` `standalone_event_broker_cluster()`
/// registered into `hub`, typed as the trait object production code
/// resolves.
pub fn test_directory_client(hub: &ClientHub) -> Arc<dyn DirectoryClient> {
    hub.get::<dyn DirectoryClient>()
        .expect("standalone_event_broker_cluster() registers a LocalDirectoryClient")
}

/// Wires a `standalone` cache provider under the `event-broker` cluster
/// profile (matching `domain::cluster::EventBrokerCluster::resolve()`'s
/// profile name) and resolves an `EventBrokerCluster` against it.
///
/// The returned `ClientHub` is the same one `EventBrokerCluster` resolved
/// against, so a test's own `register()`/`discover()` calls observe the
/// same underlying state. The `ClusterHandle` is intentionally leaked
/// (`ClusterHandle::drop` without `stop()` panics in debug builds by
/// design, as a production safety net - tests don't need graceful
/// shutdown, the process ends shortly after).
pub async fn standalone_event_broker_cluster() -> (Arc<ClientHub>, EventBrokerCluster) {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn DirectoryClient>(Arc::new(toolkit::directory::LocalDirectoryClient::new(
        Arc::new(GearManager::new()),
    )));
    let config: ClusterConfig = serde_json::from_value(serde_json::json!({
        "profiles": { "event-broker": { "cache": { "provider": "standalone" } } }
    }))
    .expect("valid test cluster config");
    let providers = ProviderRegistry::new()
        .with_cache_provider(Arc::new(standalone_cluster_plugin::StandaloneCacheProvider));

    // `from_config` returns `(ClusterHandle, Vec<Arc<BoundProfile>>)` and
    // wiring alone does not make a profile resolvable: the bound set must be
    // published into a `ProfileRegistry` and a local cluster client registered,
    // which is the step the cluster gear's own `start` performs. A harness
    // standing in for the gear has to do the same.
    //
    // Skipping it does not fail here - `resolve` returns an *unbound stub* by
    // design, deferring the error to first use - so the symptom is a `503 "no
    // backend bound for profile"` on an unrelated request much later.
    let (mut handle, bound) = ClusterWiring::from_config(Arc::clone(&hub), &config, &providers)
        .await
        .expect("standalone provider wires cleanly");
    let profiles = Arc::new(cluster::ProfileRegistry::new());
    handle.publish(&profiles, bound);
    // Leaked with the handle: clearing the registry is the other half of the
    // gear's shutdown job, and a test process ends before it matters.
    std::mem::forget(profiles);
    forget_handle(handle);

    let cluster = EventBrokerCluster::resolve(&hub)
        .await
        .expect("event-broker profile is bound");
    (hub, cluster)
}

fn forget_handle(handle: ClusterHandle) {
    std::mem::forget(handle);
}

/// A running mock ingest/delivery instance: a real axum server on an
/// ephemeral local port, registered with `DirectoryService` under
/// `gear_name`. Dropping this stops the server; the directory entry is not
/// explicitly deregistered (each test builds a fresh `ClientHub`/directory,
/// so there's no cross-test leakage, and tests don't need graceful
/// shutdown).
pub struct MockInstance {
    #[allow(
        dead_code,
        reason = "public fixture field - not every test needs to inspect the addr"
    )]
    pub addr: SocketAddr,
    server: JoinHandle<()>,
    shutdown: CancellationToken,
}

impl Drop for MockInstance {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.server.abort();
    }
}

/// Binds `router` to an ephemeral local port, serves it in the background,
/// and registers the bound address with `directory` under `gear_name`
/// (`"http://127.0.0.1:{port}"` - the same `ServiceInstanceInfo` shape
/// `forward()` resolves against, design.md D5).
pub async fn mock_instance(
    directory: &Arc<dyn DirectoryClient>,
    gear_name: &str,
    router: axum::Router,
) -> MockInstance {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding an ephemeral local port must not fail");
    let addr = listener
        .local_addr()
        .expect("a just-bound listener has a local address");

    let shutdown = CancellationToken::new();
    let shutdown_signal = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown_signal.cancelled().await })
            .await
            .expect("mock instance server must not error");
    });

    directory
        .register_instance(
            RegisterInstanceInfo::new(gear_name.to_owned(), Uuid::new_v4().to_string())
                .with_rest_endpoint(ServiceEndpoint::new(format!("http://{addr}"))),
        )
        .await
        .expect("registering the mock instance must not fail");

    MockInstance {
        addr,
        server,
        shutdown,
    }
}
