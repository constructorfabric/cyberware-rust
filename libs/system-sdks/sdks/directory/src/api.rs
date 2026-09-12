//! Directory API - contract for service discovery and instance resolution
//!
//! This gear defines the core traits and types for the directory service API.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeMap;

/// Represents an endpoint where a service can be reached
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ServiceEndpoint {
    pub uri: String,
}

impl ServiceEndpoint {
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }

    #[must_use]
    pub fn http(host: &str, port: u16) -> Self {
        Self {
            uri: format!("{}://{}:{}", "http", host, port),
        }
    }

    #[must_use]
    pub fn https(host: &str, port: u16) -> Self {
        Self {
            uri: format!("https://{host}:{port}"),
        }
    }

    pub fn uds(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            uri: format!("unix://{}", path.as_ref().display()),
        }
    }
}

/// Label-equality selector over instance labels (k8s `matchLabels` style).
///
/// Matching is equality-AND: an instance matches iff it carries **every**
/// `(key, value)` pair in `match_labels`. An empty selector matches every
/// instance. This is the selector consumed by
/// [`DirectoryClient::resolve_by_labels`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelSelector {
    /// Required label equalities; all must match.
    pub match_labels: BTreeMap<String, String>,
}

impl LabelSelector {
    /// An empty selector (matches every instance).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a selector from an existing `matchLabels` map.
    #[must_use]
    pub fn from_match_labels(match_labels: BTreeMap<String, String>) -> Self {
        Self { match_labels }
    }

    /// Add one required label equality, returning `self` for chaining.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.match_labels.insert(key.into(), value.into());
        self
    }

    /// Whether this selector constrains nothing (matches every instance).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.match_labels.is_empty()
    }

    /// Whether `labels` satisfies every required equality in this selector.
    #[must_use]
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        self.match_labels
            .iter()
            .all(|(k, v)| labels.get(k) == Some(v))
    }
}

/// Serving state of a directory instance, projected from the directory's live
/// registration/heartbeat tracking.
///
/// Only [`Ready`](Self::Ready) / [`Healthy`](Self::Healthy) are **serving**;
/// [`Draining`](Self::Draining) is a graceful shutdown (callers should avoid
/// sending new work) and [`Quarantined`](Self::Quarantined) is stale (missed
/// heartbeats). [`Registered`](Self::Registered) is the pre-serving baseline.
/// Exposed on [`ServiceInstanceInfo`] so [`DirectoryClient::resolve_by_labels`]
/// callers can apply their own health policy from a single resolve.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstanceState {
    /// Registered but not yet serving (no heartbeat / readiness signal).
    #[default]
    Registered,
    /// Dependencies resolved and marked ready.
    Ready,
    /// Serving and heartbeating.
    Healthy,
    /// Missed its heartbeat deadline; not serving, pending eviction.
    Quarantined,
    /// Gracefully shutting down; upstreams should stop routing new work.
    Draining,
    /// State could not be determined.
    Unknown,
}

impl InstanceState {
    /// Whether an instance in this state should receive new traffic
    /// (`Ready` / `Healthy`). All other states are non-serving.
    #[must_use]
    pub fn is_serving(self) -> bool {
        matches!(self, InstanceState::Ready | InstanceState::Healthy)
    }
}

/// Information about a service instance
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ServiceInstanceInfo {
    /// Gear name this instance belongs to
    pub gear: String,
    /// Unique instance identifier
    pub instance_id: String,
    /// Primary endpoint for the instance
    pub endpoint: Option<ServiceEndpoint>,
    /// Optional version string
    pub version: Option<String>,
    /// REST endpoint (HTTP base URL) for this instance, or `None` if the gear
    /// exposes no REST API. This is the address the edge reverse-proxy actually
    /// dials for REST routing.
    pub rest_endpoint: Option<ServiceEndpoint>,
    /// Stable content token for the published `OpenAPI` spec, if any.
    ///
    /// Enumeration is deliberately spec-free: only this hash rides along, never
    /// the full document (fetched out-of-band via
    /// [`DirectoryClient::get_openapi_spec`]). The hash lets a consumer detect a
    /// spec change without inlining N copies of the document.
    pub openapi_spec_hash: Option<String>,
    /// Published gRPC services as `(service name, endpoint)` pairs.
    ///
    /// Kept as an ordered list rather than a map so it round-trips a repeated
    /// proto field without reordering; service names are unique per instance.
    /// Carried back by `list_instances` so a subsequent `register_instance`
    /// (which replaces the entry wholesale) can augment — rather than clobber —
    /// the previously-registered gRPC services when adding a REST endpoint.
    pub grpc_services: Vec<(String, ServiceEndpoint)>,
    /// Stable addressing labels published by this instance (k8s `matchLabels`
    /// style). Used by [`DirectoryClient::resolve_by_labels`] to select shards
    /// or peers within a directory name.
    pub labels: BTreeMap<String, String>,
    /// Live serving state, so a `resolve_by_labels` caller can apply its own
    /// health policy without a second call. Defaults to
    /// [`InstanceState::Registered`].
    pub state: InstanceState,
}

impl ServiceInstanceInfo {
    /// Start a projection for `gear` / `instance_id` with no primary endpoint
    /// and no optional metadata. Chain the `with_*` builders to populate it.
    /// `#[non_exhaustive]` means callers must use this builder rather than a
    /// struct literal, so future fields stay additive.
    #[must_use]
    pub fn new(gear: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self {
            gear: gear.into(),
            instance_id: instance_id.into(),
            ..Self::default()
        }
    }

    /// Set the primary endpoint (`None` for an instance with no primary
    /// endpoint).
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: Option<ServiceEndpoint>) -> Self {
        self.endpoint = endpoint;
        self
    }

    /// Set the optional version string.
    #[must_use]
    pub fn with_version(mut self, version: Option<String>) -> Self {
        self.version = version;
        self
    }

    /// Set the optional REST endpoint.
    #[must_use]
    pub fn with_rest_endpoint(mut self, rest_endpoint: Option<ServiceEndpoint>) -> Self {
        self.rest_endpoint = rest_endpoint;
        self
    }

    /// Set the optional `OpenAPI` spec content token.
    #[must_use]
    pub fn with_openapi_spec_hash(mut self, openapi_spec_hash: Option<String>) -> Self {
        self.openapi_spec_hash = openapi_spec_hash;
        self
    }

    /// Set the published gRPC services.
    #[must_use]
    pub fn with_grpc_services(mut self, grpc_services: Vec<(String, ServiceEndpoint)>) -> Self {
        self.grpc_services = grpc_services;
        self
    }

    /// Set the stable addressing labels.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// Set the live serving state.
    #[must_use]
    pub fn with_state(mut self, state: InstanceState) -> Self {
        self.state = state;
        self
    }

    /// Drop the stable addressing labels, returning `self`.
    ///
    /// The shared transform every [`DirectoryClient::list_all_instances`]
    /// implementation applies so the cross-gear snapshot omits labels
    /// identically across transports — see that method's doc for why.
    #[must_use]
    pub fn without_labels(mut self) -> Self {
        self.labels.clear();
        self
    }
}

/// Information for registering a new gear instance
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RegisterInstanceInfo {
    /// Gear name
    pub gear: String,
    /// Unique instance identifier
    pub instance_id: String,
    /// Published gRPC services as `(service name, endpoint)` pairs (service
    /// names are unique per instance; ordered to mirror the proto repeated
    /// field rather than reordered into a map).
    pub grpc_services: Vec<(String, ServiceEndpoint)>,
    /// Optional version string
    pub version: Option<String>,
    /// Optional REST endpoint (HTTP base URL) exposed by the gear.
    pub rest_endpoint: Option<ServiceEndpoint>,
    /// Optional `OpenAPI` spec (JSON) published by the gear.
    pub openapi_spec: Option<String>,
    /// Stable addressing labels for this instance (k8s `matchLabels` style).
    pub labels: BTreeMap<String, String>,
}

impl RegisterInstanceInfo {
    /// Start a registration for `gear` / `instance_id` with no endpoints,
    /// version, spec, or labels. Chain the `with_*` builders to populate it.
    #[must_use]
    pub fn new(gear: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self {
            gear: gear.into(),
            instance_id: instance_id.into(),
            ..Self::default()
        }
    }

    /// Set the published gRPC services.
    #[must_use]
    pub fn with_grpc_services(mut self, grpc_services: Vec<(String, ServiceEndpoint)>) -> Self {
        self.grpc_services = grpc_services;
        self
    }

    /// Set the version string.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Set the REST endpoint (HTTP base URL).
    #[must_use]
    pub fn with_rest_endpoint(mut self, rest_endpoint: ServiceEndpoint) -> Self {
        self.rest_endpoint = Some(rest_endpoint);
        self
    }

    /// Set the published `OpenAPI` spec (JSON).
    #[must_use]
    pub fn with_openapi_spec(mut self, openapi_spec: impl Into<String>) -> Self {
        self.openapi_spec = Some(openapi_spec.into());
        self
    }

    /// Set the stable addressing labels.
    ///
    /// Re-registration replaces the directory entry wholesale, but an **empty**
    /// label set is treated as "preserve the stored labels", not "clear them"
    /// (see `GearManager::with_metadata_of`): the periodic self-heal and REST
    /// augmentation re-register without labels and must not erase a shard's
    /// identity. A non-empty set replaces the stored one. Clearing labels
    /// outright is therefore intentionally not expressible — an instance's
    /// labels are its static shard/peer identity for its lifetime; changing
    /// them means restarting with new configuration.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }
}

/// A resolved gRPC service and the endpoint it is reachable at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GrpcServiceInfo {
    /// Fully-qualified gRPC service name (e.g. `payment.v1.PaymentApi`).
    pub service_name: String,
    /// Endpoint the service is reachable at.
    pub endpoint: ServiceEndpoint,
}

impl GrpcServiceInfo {
    pub fn new(service_name: impl Into<String>, endpoint: ServiceEndpoint) -> Self {
        Self {
            service_name: service_name.into(),
            endpoint,
        }
    }
}

/// Sentinel error wrapped via `anyhow::Error` to signal "the requested gear or
/// service is not registered (or has no live instance)" through the
/// [`DirectoryClient`] trait. Consumers downcast to this type to distinguish a
/// not-ready provider (eventual readiness) from a directory-backend failure —
/// see `toolkit::discovery::DirectoryEndpointResolver`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryNotFound {
    /// What was being looked up — e.g. `"gear foo"` or `"service foo.Bar"`.
    pub resource: String,
}

impl DirectoryNotFound {
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
        }
    }
}

impl std::fmt::Display for DirectoryNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "directory: not found: {}", self.resource)
    }
}

impl std::error::Error for DirectoryNotFound {}

/// Sentinel error wrapped via `anyhow::Error` to signal "client-supplied
/// argument is malformed" (e.g. invalid UUID) through the [`DirectoryClient`]
/// trait. Allows the gRPC server boundary to return `Status::invalid_argument`
/// instead of mislabeling a client bug as an internal failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryInvalidArgument {
    /// Human-readable description of what was invalid.
    pub message: String,
}

impl DirectoryInvalidArgument {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DirectoryInvalidArgument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "directory: invalid argument: {}", self.message)
    }
}

impl std::error::Error for DirectoryInvalidArgument {}

/// Sentinel error wrapped via `anyhow::Error` to signal "the directory refused
/// this call on authorization grounds".
///
/// It carries the gRPC `PermissionDenied` and `Unauthenticated` codes across the
/// [`DirectoryClient`] trait boundary (the code is otherwise lost when a
/// `tonic::Status` is stringified). Like [`DirectoryInvalidArgument`], such a
/// rejection is **permanent**: retrying the identical request can never turn a
/// "no" into a "yes". A registrant reaches this because it is not an authorized
/// peer for the gear it claims, because its `ServiceAccount` namespace / SPIFFE
/// trust domain is not allowlisted, because an enforcing listener saw no
/// authenticated peer, or because a gRPC service name in the registration is
/// already owned by another gear (a server-side
/// [`DirectoryServiceNameConflict`] mapped to `PermissionDenied`). The presence
/// loop downcasts to this type to stop retrying and log loudly rather than
/// spinning at `warn!` forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPermissionDenied {
    /// Human-readable description of why the call was refused.
    pub message: String,
}

impl DirectoryPermissionDenied {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DirectoryPermissionDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "directory: permission denied: {}", self.message)
    }
}

impl std::error::Error for DirectoryPermissionDenied {}

/// Sentinel error wrapped via `anyhow::Error` to signal "a gRPC service name in
/// this registration is already owned by a *different* gear".
///
/// Registration enforces single-gear ownership of every advertised gRPC service
/// name atomically at the store (see `GearManager::register_instance`),
/// so a name resolved across all gears cannot be split between two owners. The
/// gRPC server boundary downcasts to this type to return
/// `Status::permission_denied` (with a static message) and to log the
/// conflicting `service_name` / `owner` server-side, rather than mislabeling an
/// ownership conflict as an internal fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryServiceNameConflict {
    /// The gRPC service name that is already owned.
    pub service_name: String,
    /// The gear that currently owns `service_name`.
    pub owner: String,
}

impl DirectoryServiceNameConflict {
    pub fn new(service_name: impl Into<String>, owner: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            owner: owner.into(),
        }
    }
}

impl std::fmt::Display for DirectoryServiceNameConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "directory: gRPC service name '{}' already owned by gear '{}'",
            self.service_name, self.owner
        )
    }
}

impl std::error::Error for DirectoryServiceNameConflict {}

/// Directory API trait for service discovery and instance management
///
/// This trait defines the contract for interacting with the gear directory.
/// It can be implemented by:
/// - A local implementation that delegates to `GearManager`
/// - A gRPC client for out-of-process gears
#[async_trait]
pub trait DirectoryClient: Send + Sync {
    /// Resolve a gRPC service by its logical name to an endpoint
    async fn resolve_grpc_service(&self, service_name: &str) -> Result<ServiceEndpoint>;

    /// Resolve a REST endpoint (HTTP base URL) for a gear by its name.
    ///
    /// Returns the base URL (e.g. `http://billing:8080`) that callers use to
    /// make REST requests to the resolved gear.
    async fn resolve_rest_service(&self, gear_name: &str) -> Result<ServiceEndpoint>;

    /// Retrieve the `OpenAPI` spec (JSON) published by a gear.
    async fn get_openapi_spec(&self, gear_name: &str) -> Result<String>;

    /// List all service instances for a given gear.
    ///
    /// Entries are **spec-free**: only the `openapi_spec_hash` is carried, never
    /// the full `OpenAPI` document, so a multi-instance response stays
    /// bounded regardless of spec size. Callers fetch the document out-of-band
    /// via [`get_openapi_spec`](Self::get_openapi_spec); the hash lets a consumer
    /// detect a spec change without inlining N copies of the document.
    ///
    /// An endpoint-less instance is still returned (`endpoint`/`rest_endpoint` =
    /// `None`, never an empty-URI sentinel): *no-endpoint, not an error*. Only
    /// [`list_all_instances`](Self::list_all_instances), which needs a dialable
    /// URI, drops such entries.
    async fn list_instances(&self, gear: &str) -> Result<Vec<ServiceInstanceInfo>>;

    /// Resolve instances of `gear` whose labels satisfy `selector`.
    ///
    /// Matching is equality-AND (k8s `matchLabels`): an instance matches iff
    /// it carries **every** `(key, value)` pair in the selector; an empty
    /// selector matches every instance of the name.
    ///
    /// The returned set is **not** health-filtered — matched instances are
    /// returned regardless of readiness/heartbeat state, each carrying its
    /// `instance_id`, `labels`, endpoints, and live
    /// [`state`](ServiceInstanceInfo::state). Applying liveness policy (e.g.
    /// keeping only [`InstanceState::is_serving`]) is the caller's
    /// responsibility.
    ///
    /// Nor is it **endpoint-filtered**: a matched instance that advertises no
    /// endpoint yet is still returned (`endpoint`/`rest_endpoint` = `None`) —
    /// *no-endpoint, not an error* — so the caller can fall back rather than
    /// have the match silently hidden.
    ///
    /// Returned entries are **spec-free** on every implementation — the full
    /// `OpenAPI` document is omitted (only its hash is carried), so the
    /// polled resolve payload stays bounded; the caller fetches documents via
    /// [`get_openapi_spec`](Self::get_openapi_spec). This holds regardless of
    /// whether the selector is empty. The default body enumerates via
    /// [`list_instances`](Self::list_instances) and filters in-process;
    /// transport-backed clients (e.g. the gRPC client) override it to push the
    /// selector server-side.
    ///
    /// Label-based selection is effectively **out-of-process only**. Labels are
    /// published from the `OoP` serve path's configuration (`oop_http.labels`);
    /// the in-process registration path (grpc-hub start phase /
    /// `run_directory_register_phase`) has no label source, so an in-process
    /// gear carries no labels and a non-empty selector never matches it. This
    /// matches the deployment model: shards/peers are distinct instances
    /// (separate pods/processes), which is inherently the `OoP` topology.
    async fn resolve_by_labels(
        &self,
        gear: &str,
        selector: &LabelSelector,
    ) -> Result<Vec<ServiceInstanceInfo>> {
        // cancel-safe: the single await precedes any mutation; cancelling here
        // just drops the in-flight list and leaves no partial state.
        let instances = self.list_instances(gear).await?;
        // Entries are spec-free by construction — `ServiceInstanceInfo` carries
        // only the `openapi_spec_hash`, never the full OpenAPI document.
        Ok(instances
            .into_iter()
            .filter(|i| selector.matches(&i.labels))
            .collect())
    }

    /// List every service instance across all registered gears.
    ///
    /// Used by the edge gateway to discover which gears (and their REST
    /// endpoints) to reverse-proxy. This is a lightweight discovery snapshot.
    /// Like every enumeration path, it is **spec-free**: entries carry only the
    /// `openapi_spec_hash`, never the full `OpenAPI` document, even when the
    /// backing store holds a stored specification. The edge fetches a gear's
    /// document once, on first discovery, via
    /// [`get_openapi_spec`](Self::get_openapi_spec).
    ///
    /// The returned instances also do **not** carry `labels`: every
    /// implementation applies [`ServiceInstanceInfo::without_labels`] so the
    /// snapshot omits them identically regardless of transport. Labels drive
    /// the targeted [`resolve_by_labels`](Self::resolve_by_labels) path, not
    /// this snapshot.
    async fn list_all_instances(&self) -> Result<Vec<ServiceInstanceInfo>>;

    /// Register a new gear instance with the directory
    async fn register_instance(&self, info: RegisterInstanceInfo) -> Result<()>;

    /// Deregister a gear instance (for graceful shutdown)
    async fn deregister_instance(&self, gear: &str, instance_id: &str) -> Result<()>;

    /// Send a heartbeat for a gear instance to indicate it's still alive
    async fn send_heartbeat(&self, gear: &str, instance_id: &str) -> Result<()>;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_service_endpoint_creation() {
        let http_ep = ServiceEndpoint::http("localhost", 8080);
        assert_eq!(http_ep.uri, concat!("http", "://localhost:8080"));

        let https_endpoint = ServiceEndpoint::https("localhost", 8443);
        assert_eq!(https_endpoint.uri, "https://localhost:8443");

        let uds_ep = ServiceEndpoint::uds("/tmp/socket.sock");
        assert!(uds_ep.uri.starts_with("unix://"));
        assert!(uds_ep.uri.contains("socket.sock"));

        let custom_ep = ServiceEndpoint::new(concat!("http", "://example.com"));
        assert_eq!(custom_ep.uri, concat!("http", "://example.com"));
    }

    #[test]
    fn test_register_instance_info() {
        let info = RegisterInstanceInfo::new("test_gear", "instance1")
            .with_grpc_services(vec![(
                "test.Service".to_owned(),
                ServiceEndpoint::http("127.0.0.1", 8001),
            )])
            .with_version("1.0.0");

        assert_eq!(info.gear, "test_gear");
        assert_eq!(info.instance_id, "instance1");
        assert_eq!(info.grpc_services.len(), 1);
        assert!(info.rest_endpoint.is_none());
        assert!(info.openapi_spec.is_none());
        assert!(info.labels.is_empty());
    }

    #[test]
    fn test_register_instance_info_with_rest() {
        let info = RegisterInstanceInfo {
            gear: "billing".to_owned(),
            instance_id: "instance1".to_owned(),
            grpc_services: vec![],
            version: Some("2.0.0".to_owned()),
            labels: BTreeMap::new(),
            rest_endpoint: Some(ServiceEndpoint::http("billing", 8080)),
            openapi_spec: Some("{\"openapi\":\"3.1.0\"}".to_owned()),
        };

        assert_eq!(info.gear, "billing");
        assert_eq!(
            info.rest_endpoint.as_ref().unwrap().uri,
            concat!("http", "://billing:8080")
        );
        assert!(info.openapi_spec.is_some());
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn label_selector_equality_and_matches() {
        let selector = LabelSelector::new()
            .with("shard", "7")
            .with("role", "ingest");

        // Superset of the required labels matches.
        assert!(selector.matches(&labels(&[
            ("shard", "7"),
            ("role", "ingest"),
            ("extra", "x"),
        ])));
        // Missing one required key fails.
        assert!(!selector.matches(&labels(&[("shard", "7")])));
        // Wrong value on a required key fails.
        assert!(!selector.matches(&labels(&[("shard", "8"), ("role", "ingest")])));
    }

    #[test]
    fn empty_label_selector_matches_everything() {
        let selector = LabelSelector::new();
        assert!(selector.is_empty());
        assert!(selector.matches(&BTreeMap::new()));
        assert!(selector.matches(&labels(&[("shard", "7")])));
    }

    struct StaticDirectory {
        instances: Vec<ServiceInstanceInfo>,
    }

    #[async_trait]
    impl DirectoryClient for StaticDirectory {
        async fn resolve_grpc_service(&self, service_name: &str) -> Result<ServiceEndpoint> {
            Err(DirectoryNotFound::new(format!("service {service_name}")).into())
        }
        async fn resolve_rest_service(&self, gear_name: &str) -> Result<ServiceEndpoint> {
            Err(DirectoryNotFound::new(format!("gear {gear_name}")).into())
        }
        async fn get_openapi_spec(&self, gear_name: &str) -> Result<String> {
            Err(DirectoryNotFound::new(format!("spec {gear_name}")).into())
        }
        async fn list_instances(&self, gear: &str) -> Result<Vec<ServiceInstanceInfo>> {
            Ok(self
                .instances
                .iter()
                .filter(|i| i.gear == gear)
                .cloned()
                .collect())
        }
        async fn list_all_instances(&self) -> Result<Vec<ServiceInstanceInfo>> {
            Ok(self.instances.clone())
        }
        async fn register_instance(&self, _info: RegisterInstanceInfo) -> Result<()> {
            Ok(())
        }
        async fn deregister_instance(&self, _gear: &str, _instance_id: &str) -> Result<()> {
            Ok(())
        }
        async fn send_heartbeat(&self, _gear: &str, _instance_id: &str) -> Result<()> {
            Ok(())
        }
    }

    fn instance(id: &str, labels_pairs: &[(&str, &str)]) -> ServiceInstanceInfo {
        ServiceInstanceInfo {
            gear: "worker".to_owned(),
            instance_id: id.to_owned(),
            labels: labels(labels_pairs),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn resolve_by_labels_default_filters_by_equality_and() {
        let dir = StaticDirectory {
            instances: vec![
                instance("a", &[("shard", "7")]),
                instance("b", &[("shard", "8")]),
                instance("c", &[("shard", "7"), ("role", "ingest")]),
            ],
        };

        let selector = LabelSelector::new().with("shard", "7");
        let mut matched = dir
            .resolve_by_labels("worker", &selector)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.instance_id)
            .collect::<Vec<_>>();
        matched.sort();
        assert_eq!(matched, vec!["a".to_owned(), "c".to_owned()]);

        // An empty selector returns every instance of the name.
        let all = dir
            .resolve_by_labels("worker", &LabelSelector::new())
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
    }
}
