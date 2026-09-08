//! Shared L2/L3 fixture: one k3s cluster per test binary, a namespace per
//! scenario, and the direct-API assertion helpers the `K8S-*` scenarios read the
//! real `Lease` / `ClusterCacheEntry` objects through (docs/TESTING.md 4.1).
//!
//! ## One container, shared
//!
//! k3s takes ~15-25s to become ready, so — unlike the postgres plugin, which
//! spins a fresh container per test — this fixture starts **one** k3s container per
//! test binary behind a [`tokio::sync::OnceCell`] and isolates every scenario by
//! its own Kubernetes namespace instead (the API server's own isolation boundary).
//! The container lives in a `static`, so it is torn down at process exit (the
//! testcontainers reaper reclaims it).
//!
//! ## Client injection rather than `KUBECONFIG`
//!
//! The plugin's inference path (`Config::infer()`) reads `KUBECONFIG`, but this
//! workspace is edition 2024 with `unsafe_code = "forbid"`, so a test cannot
//! `std::env::set_var("KUBECONFIG", ...)` (now `unsafe`) to point it at the
//! container. Instead the fixture builds one admin [`kube::Client`] against the
//! mapped host port and injects it through the builders' `with_client` (the same
//! adoption path a host gear uses, DESIGN.md 3.3). The few scenarios that must
//! exercise the inference path itself scope a written kubeconfig with the
//! `temp-env` crate around just that `build_and_start`.

#![cfg(feature = "integration")]
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test harness: a fixture-setup failure IS the test failure, and not \
              every helper here is used by every test binary that `mod common;`s it"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tower::{Layer, Service};

use k8s_cluster_plugin::{
    ClusterCacheEntry, K8sCacheConfig, K8sClusterConfig, K8sLeaderElectionConfig, K8sLockConfig,
};
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::client::ClientBuilder;
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::runtime::wait::{await_condition, conditions};
use kube::{Client, Config, ResourceExt};
use serde_json::{Value, json};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::k3s::{K3s, KUBE_SECURE_PORT};
use tokio::sync::OnceCell;

/// The CRD manifest shipped with the plugin (docs/TESTING.md 3): applied once per
/// container so a cache scenario never races the install.
const CRD_YAML: &str = include_str!("../../../deploy/crd.yaml");

/// A single shared k3s cluster. Only the container and the port-rewritten
/// kubeconfig are shared; **clients are built per scenario**, because a
/// `kube::Client` is bound to the tokio runtime that created it and each
/// `#[tokio::test]` runs on its own runtime (a client shared across them fails
/// later tests with `Service(Closed)`).
pub struct SharedCluster {
    /// Kept alive for the process lifetime; dropping it removes the container.
    _container: ContainerAsync<K3s>,
    /// The port-rewritten kubeconfig every per-scenario client is built from — also
    /// the base for the inference-path scenarios (scoped `KUBECONFIG` via `temp-env`)
    /// and for restricted-SA clients.
    pub kubeconfig: Kubeconfig,
}

impl SharedCluster {
    /// Builds a fresh admin client on the current runtime from the shared kubeconfig.
    pub async fn client(&self) -> Client {
        let config =
            Config::from_custom_kubeconfig(self.kubeconfig.clone(), &KubeConfigOptions::default())
                .await
                .expect("kube config builds from the shared kubeconfig");
        Client::try_from(config).expect("admin client builds")
    }
}

static CLUSTER: OnceCell<SharedCluster> = OnceCell::const_new();
/// Monotonic suffix so every scenario's namespace name is unique within a binary.
static NAMESPACE_SEQ: AtomicU64 = AtomicU64::new(0);

/// The Docker label key every fixture container carries.
const FIXTURE_LABEL_KEY: &str = "org.cf-gears.test-fixture";
/// The label value marking a container as this plugin's k3s fixture.
const FIXTURE_LABEL_VALUE: &str = "cf-k8s-cluster-plugin";

/// Removes any k3s container this fixture leaked in a previous run, before starting
/// a fresh one.
///
/// The shared container lives in a `static` (so scenarios can share it) and is never
/// `Drop`ped, and **testcontainers 0.27 ships no ryuk reaper** — so a container from
/// an earlier `cargo test` run (or a crashed one) would otherwise pile up, and each
/// k3s runs a full control plane plus etcd. Reaping the previous run's container here
/// bounds the leak to a single container at a time: self-healing regardless of how
/// the tests were invoked. Safe because `cargo test` runs test binaries sequentially,
/// so no *other* fixture container is live when this fires. Best-effort — a missing or
/// slow Docker is not this fixture's failure (the `start()` below will report it).
fn reap_stale_fixture_containers() {
    let _reaped = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "docker ps -aq --filter label={FIXTURE_LABEL_KEY}={FIXTURE_LABEL_VALUE} \
             | xargs -r docker rm -f >/dev/null 2>&1"
        ))
        .status();
}

/// Starts (once) and returns the shared k3s cluster, with the CRD applied and
/// `Established`.
pub async fn shared_cluster() -> &'static SharedCluster {
    CLUSTER.get_or_init(start_k3s).await
}

/// Boots a k3s container, builds the admin client against the mapped host port,
/// applies `deploy/crd.yaml`, and waits for the CRD to become `Established`.
///
/// A broken manifest fails here — with a clear message — rather than surfacing as
/// every cache scenario failing at construction (docs/TESTING.md 3, 4.1).
async fn start_k3s() -> SharedCluster {
    reap_stale_fixture_containers();

    let conf_dir = std::env::temp_dir().join(format!(
        "k3s-cluster-plugin-{}-{}",
        std::process::id(),
        NAMESPACE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&conf_dir).expect("create the k3s kubeconfig mount dir");

    let container = K3s::default()
        .with_conf_mount(&conf_dir)
        .with_privileged(true)
        .with_userns_mode("host")
        // Tag the container so a leaked one can be reaped by the next run (see
        // `reap_stale_fixture_containers`).
        .with_label(FIXTURE_LABEL_KEY, FIXTURE_LABEL_VALUE)
        .start()
        .await
        .expect("k3s container starts (needs a privileged Docker daemon)");

    let port = container
        .get_host_port_ipv4(KUBE_SECURE_PORT)
        .await
        .expect("k3s secure port is mapped");

    // The in-container kubeconfig points at :6443; rewrite it to the mapped host
    // port so a client on the host can reach the API server.
    let raw = container
        .image()
        .read_kube_config()
        .expect("k3s writes a kubeconfig into the mount dir");
    let mut kubeconfig = Kubeconfig::from_yaml(&raw).expect("k3s kubeconfig parses");
    for cluster in &mut kubeconfig.clusters {
        if let Some(server) = cluster.cluster.as_mut().and_then(|c| c.server.as_mut()) {
            *server = format!("https://127.0.0.1:{port}");
        }
    }

    let config = Config::from_custom_kubeconfig(kubeconfig.clone(), &KubeConfigOptions::default())
        .await
        .expect("kube config builds from the rewritten kubeconfig");
    let client = Client::try_from(config).expect("admin client builds");

    apply_crd(&client).await;

    SharedCluster {
        _container: container,
        kubeconfig,
    }
}

/// Applies the shipped CRD and waits for it to report `Established` (bounded).
async fn apply_crd(client: &Client) {
    let crd: CustomResourceDefinition = serde_yaml_ng::from_str(CRD_YAML)
        .expect("deploy/crd.yaml parses as a CustomResourceDefinition");
    let name = crd.name_any();
    let api: Api<CustomResourceDefinition> = Api::all(client.clone());
    // Idempotent: a shared container applies it once, but be tolerant of a re-run.
    if api.get_opt(&name).await.expect("CRD get").is_none() {
        api.create(&PostParams::default(), &crd)
            .await
            .expect("CRD applies cleanly (a broken manifest fails the fixture here)");
    }
    let established = await_condition(api, &name, conditions::is_crd_established());
    tokio::time::timeout(Duration::from_secs(30), established)
        .await
        .expect("the CRD becomes Established within 30s")
        .expect("await_condition on the CRD");
}

/// A per-scenario namespace, deleted (best-effort) on drop, which cascades to every
/// object in it. Carries the admin client to inject into plugins and to assert with.
pub struct NamespaceGuard {
    pub namespace: String,
    pub client: Client,
}

/// Creates a fresh namespace against the shared cluster and returns a guard for it.
///
/// Unlike the sketch in docs/TESTING.md 4.1, this creates **no** Role/RoleBinding:
/// scenarios inject the admin client (see the module docs), so the plugin already
/// has every verb. The restricted-verb scenarios build their own SA client via
/// [`restricted_client`].
pub async fn fresh_namespace(base_name: &str) -> NamespaceGuard {
    let cluster = shared_cluster().await;
    let seq = NAMESPACE_SEQ.fetch_add(1, Ordering::Relaxed);
    // RFC 1123: lowercase, <= 63 chars. `base_name` arrives already-legal from the
    // call sites; the suffix keeps it unique.
    let namespace = format!("{}-{seq}", sanitize_label(base_name));
    let ns = Namespace {
        metadata: kube::api::ObjectMeta {
            name: Some(namespace.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let client = cluster.client().await;
    let api: Api<Namespace> = Api::all(client.clone());
    api.create(&PostParams::default(), &ns)
        .await
        .expect("namespace creates");
    NamespaceGuard { namespace, client }
}

/// A process-unique identity string. The test host has no `HOSTNAME`/`POD_NAME`, so
/// `identity` cannot resolve from the environment; every config carries an explicit
/// one, and a fresh value per builder call gives each plugin instance in a
/// multi-candidate scenario its own `holderIdentity`.
pub fn fresh_identity(base: &str) -> String {
    format!("{base}-{}", NAMESPACE_SEQ.fetch_add(1, Ordering::Relaxed))
}

impl NamespaceGuard {
    /// A combined config pointed at this namespace, all other fields defaulted.
    pub fn cluster_config(&self) -> K8sClusterConfig {
        self.cluster_config_with(json!({}))
    }

    /// A combined config pointed at this namespace, with `overrides` merged over the
    /// defaults (mirrors the postgres `cluster_config_json` idiom).
    pub fn cluster_config_with(&self, overrides: Value) -> K8sClusterConfig {
        from_merged(self.base_json(), overrides)
    }

    /// A standalone cache config pointed at this namespace.
    pub fn cache_config_with(&self, overrides: Value) -> K8sCacheConfig {
        from_merged(self.base_json(), overrides)
    }

    /// A standalone leader-election config pointed at this namespace.
    pub fn leader_config_with(&self, overrides: Value) -> K8sLeaderElectionConfig {
        from_merged(self.base_json(), overrides)
    }

    /// A standalone lock config pointed at this namespace.
    pub fn lock_config_with(&self, overrides: Value) -> K8sLockConfig {
        from_merged(self.base_json(), overrides)
    }

    fn base_json(&self) -> Value {
        json!({ "namespace": self.namespace, "identity": fresh_identity("test") })
    }

    /// The `Lease` API scoped to this namespace, for reading holder / renewTime /
    /// leaseDurationSeconds off the real object.
    pub fn leases(&self) -> Api<Lease> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// The `ClusterCacheEntry` API scoped to this namespace.
    pub fn cache_entries(&self) -> Api<ClusterCacheEntry> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Lists the `Lease` objects in this namespace.
    pub async fn list_leases(&self) -> Vec<Lease> {
        self.leases()
            .list(&ListParams::default())
            .await
            .expect("list leases")
            .items
    }

    /// Lists the `ClusterCacheEntry` objects in this namespace.
    pub async fn list_cache_entries(&self) -> Vec<ClusterCacheEntry> {
        self.cache_entries()
            .list(&ListParams::default())
            .await
            .expect("list cache entries")
            .items
    }
}

impl Drop for NamespaceGuard {
    fn drop(&mut self) {
        // Namespace teardown is asynchronous and best-effort: fire a detached delete
        // and let the cascade run. A leftover namespace (crashed test) is reclaimed
        // when the container dies, so a failed delete is not itself a test failure.
        let client = self.client.clone();
        let namespace = self.namespace.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let api: Api<Namespace> = Api::all(client);
                let _deleted = api.delete(&namespace, &DeleteParams::background()).await;
            });
        }
    }
}

/// Deserializes a `base` JSON object with `overrides` shallow-merged over it into
/// any of the four config shapes (mirrors the postgres `cluster_config_json`).
fn from_merged<T: serde::de::DeserializeOwned>(mut base: Value, overrides: Value) -> T {
    merge(&mut base, overrides);
    serde_json::from_value(base).expect("config JSON deserializes")
}

/// Shallow object merge — sufficient for the flat config shapes here.
fn merge(base: &mut Value, overrides: Value) {
    let (Value::Object(base_map), Value::Object(override_map)) = (base, overrides) else {
        return;
    };
    for (key, value) in override_map {
        base_map.insert(key, value);
    }
}

/// Lowercases and replaces every illegal character so an arbitrary `base_name`
/// yields a legal RFC 1123 label prefix.
fn sanitize_label(base: &str) -> String {
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(40);
    out
}

/// Per-verb API request tallies, incremented by [`CountingLayer`] at the transport
/// so a scenario can assert *how many* requests (and of which verb) the plugin made
/// — the plugin ships no `cluster_k8s_api_requests` counter, so this stands in.
#[derive(Default)]
pub struct ApiCounts {
    /// `GET`s that are not watches.
    reads: AtomicU64,
    /// `GET`s carrying `watch=true` (a streaming watch connection).
    watches: AtomicU64,
    /// `POST`s (object creation).
    creates: AtomicU64,
    /// `PUT`s (guarded replace).
    updates: AtomicU64,
    /// `PATCH`es (should be zero — this plugin never patches).
    patches: AtomicU64,
    /// `DELETE`s.
    deletes: AtomicU64,
}

impl ApiCounts {
    fn record(&self, method: &http::Method, uri: &http::Uri) {
        let is_watch = uri
            .query()
            .is_some_and(|q| q.split('&').any(|kv| kv == "watch=true" || kv == "watch=1"));
        let counter = match *method {
            http::Method::GET if is_watch => &self.watches,
            http::Method::GET => &self.reads,
            http::Method::POST => &self.creates,
            http::Method::PUT => &self.updates,
            http::Method::PATCH => &self.patches,
            http::Method::DELETE => &self.deletes,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Non-watch `GET`s so far.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Watch (`GET ?watch=true`) connections opened so far.
    pub fn watches(&self) -> u64 {
        self.watches.load(Ordering::Relaxed)
    }

    /// `DELETE`s so far.
    pub fn deletes(&self) -> u64 {
        self.deletes.load(Ordering::Relaxed)
    }

    /// Mutating requests so far — `POST` + `PUT` + `PATCH` + `DELETE`. The
    /// "a follower issues no writes" property (K8S-LEAD-007, K8S-SPEC-010) is this
    /// count being zero.
    pub fn mutating(&self) -> u64 {
        self.creates.load(Ordering::Relaxed)
            + self.updates.load(Ordering::Relaxed)
            + self.patches.load(Ordering::Relaxed)
            + self.deletes.load(Ordering::Relaxed)
    }

    /// Every counted request so far.
    pub fn total(&self) -> u64 {
        self.reads() + self.watches() + self.mutating()
    }
}

/// A `tower` layer that tallies each request into a shared [`ApiCounts`] before
/// forwarding it, wrapping the outermost point of `kube`'s service stack so it sees
/// the real verb and path.
#[derive(Clone)]
pub struct CountingLayer {
    counts: Arc<ApiCounts>,
}

impl<S> Layer<S> for CountingLayer {
    type Service = CountingService<S>;

    fn layer(&self, inner: S) -> CountingService<S> {
        CountingService {
            inner,
            counts: Arc::clone(&self.counts),
        }
    }
}

/// The [`CountingLayer`]'s service: increments the tally, then delegates unchanged
/// (the response future passes straight through, so no boilerplate future wrapper).
#[derive(Clone)]
pub struct CountingService<S> {
    inner: S,
    counts: Arc<ApiCounts>,
}

impl<S, B> Service<http::Request<B>> for CountingService<S>
where
    S: Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> S::Future {
        self.counts.record(req.method(), req.uri());
        self.inner.call(req)
    }
}

/// Builds a client whose transport tallies every request into the returned
/// [`ApiCounts`]. Inject the client into a plugin via `with_client` and read the
/// counts to assert request volume.
pub async fn counted_client() -> (Client, Arc<ApiCounts>) {
    let cluster = shared_cluster().await;
    let counts = Arc::new(ApiCounts::default());
    let config =
        Config::from_custom_kubeconfig(cluster.kubeconfig.clone(), &KubeConfigOptions::default())
            .await
            .expect("kube config builds from the shared kubeconfig");
    let client = ClientBuilder::try_from(config)
        .expect("client builder from config")
        .with_layer(&CountingLayer {
            counts: Arc::clone(&counts),
        })
        .build();
    (client, counts)
}

/// Polls `condition` until it returns `true` or `timeout` elapses; returns whether
/// it became true. The standard "assert eventual server state" primitive — used
/// instead of a fixed sleep so a fast cluster does not pay a worst-case wait.
pub async fn wait_until<F, Fut>(timeout: Duration, interval: Duration, mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(timeout, async {
        loop {
            if condition().await {
                return true;
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .unwrap_or(false)
}
