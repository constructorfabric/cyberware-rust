//! gRPC client implementation of Directory API
//!
//! This client allows remote gears to discover and resolve services via gRPC.

use anyhow::Result;
use async_trait::async_trait;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::ProtoInstanceState;
use crate::api::{
    DirectoryClient, DirectoryInvalidArgument, DirectoryNotFound, DirectoryPermissionDenied,
    InstanceState, LabelSelector, RegisterInstanceInfo, ServiceEndpoint, ServiceInstanceInfo,
};
use std::collections::BTreeMap;
use toolkit_transport_grpc::InternalAuthInterceptor;
use toolkit_transport_grpc::client::{GrpcClientConfig, connect_lazy, connect_with_retry};

use crate::{
    DeregisterInstanceRequest, DirectoryServiceClient, GetOpenApiSpecRequest, GrpcServiceEndpoint,
    HeartbeatRequest, InstanceInfo, ListAllInstancesRequest, ListInstancesRequest,
    RegisterInstanceRequest, ResolveGrpcServiceRequest, ResolveRestServiceRequest,
};

/// The directory channel wrapped with the platform-plane
/// [`InternalAuthInterceptor`], which attaches the gear's internal token
/// (`x-toolkit-internal-token`) to every outbound system call. A
/// [`disabled`](InternalAuthInterceptor::disabled) interceptor attaches
/// nothing (Profile 1 / no platform-plane credential).
type AuthedChannel = InterceptedService<Channel, InternalAuthInterceptor>;

/// Map a lookup RPC's `tonic::Status` onto the directory's typed sentinels.
///
/// The status code is the only thing distinguishing "this name is not
/// registered" from "the directory is unreachable", and stringifying the status
/// throws it away. Callers downcast the result: `DirectoryEndpointResolver`
/// turns `DirectoryNotFound` into `Ok(None)` (a provider that has not come up
/// yet — routine during startup) and anything else into a real error.
fn lookup_error(resource: &str, status: &tonic::Status) -> anyhow::Error {
    match status.code() {
        tonic::Code::NotFound => DirectoryNotFound::new(resource.to_owned()).into(),
        tonic::Code::InvalidArgument => {
            DirectoryInvalidArgument::new(status.message().to_owned()).into()
        }
        code => anyhow::anyhow!(
            "directory lookup for {resource} failed: gRPC {code:?}: {}",
            status.message()
        ),
    }
}

/// Map a mutating RPC's `tonic::Status`, preserving the code in the message.
///
/// A bare `"gRPC call failed"` hides whether the directory was unreachable
/// (transient) or rejected the request permanently. The two permanent classes
/// are typed so a caller retrying a mutation (e.g. the presence loop) can stop
/// rather than spin forever:
/// - `InvalidArgument` → [`DirectoryInvalidArgument`] (malformed request);
/// - `PermissionDenied` → [`DirectoryPermissionDenied`] (an authorization
///   decision: peer not authorized, namespace / trust domain not allowlisted,
///   or a gRPC service name owned by another gear).
///
/// Every other code — including `Unauthenticated` — stays an opaque,
/// transient-by-default error (code kept in the message) and is retried. A
/// credential failure is not permanent: a stale or rotated token is valid again
/// on the next attempt, so a retry recovers what the permanent sentinel would
/// strand.
fn call_error(op: &str, status: &tonic::Status) -> anyhow::Error {
    match status.code() {
        tonic::Code::InvalidArgument => {
            DirectoryInvalidArgument::new(status.message().to_owned()).into()
        }
        tonic::Code::PermissionDenied => {
            DirectoryPermissionDenied::new(status.message().to_owned()).into()
        }
        code => anyhow::anyhow!("directory {op} failed: gRPC {code:?}: {}", status.message()),
    }
}

/// gRPC client for Directory API
///
/// This client connects to a remote `DirectoryService` via gRPC and provides
/// typed access to service discovery functionality. It includes:
/// - Configurable timeouts and retries via transport stack
/// - Automatic proto ↔ domain type conversions
/// - Distributed tracing and metrics
/// - Platform-plane credential attachment via an [`InternalAuthInterceptor`]
///   (defaults to attaching nothing; supply one via the `*_with_interceptor`
///   constructors for Profile-3 / shared-secret deployments)
pub struct DirectoryGrpcClient {
    inner: DirectoryServiceClient<AuthedChannel>,
}

impl DirectoryGrpcClient {
    /// Connect to a directory service using default configuration with retries.
    ///
    /// Uses exponential backoff retry logic for reliable connection establishment.
    /// This is the recommended method for `OoP` gears connecting to the master host.
    ///
    /// # Errors
    /// It will return an error when it fails
    pub async fn connect(uri: impl Into<String>) -> Result<Self> {
        let cfg = GrpcClientConfig::new("directory");
        Self::connect_with_retry(uri, &cfg).await
    }

    /// Connect with default configuration + retries, attaching `interceptor`'s
    /// platform-plane credential to every outbound call.
    ///
    /// This is the Profile-3 / shared-secret entry point: the interceptor is
    /// typically built from a
    /// [`ServiceAccountTokenReader`](toolkit_transport_grpc::ServiceAccountTokenReader)
    /// (rotating SA token) or
    /// [`InternalAuthInterceptor::from_token`] (static shared secret).
    ///
    /// # Errors
    /// It will return an error when it fails
    pub async fn connect_with_interceptor(
        uri: impl Into<String>,
        interceptor: InternalAuthInterceptor,
    ) -> Result<Self> {
        let cfg = GrpcClientConfig::new("directory");
        let channel: Channel = connect_with_retry(uri, &cfg).await?;
        Ok(Self::from_channel_with_interceptor(channel, interceptor))
    }

    /// Create a directory client with a **lazily-connecting** channel.
    ///
    /// Performs **no** eager connection: the channel connects on the first RPC
    /// and transparently reconnects on failure. This is the eventual-readiness
    /// entry point (`cpt-cf-adr-eventual-readiness`) for `OoP` bootstrap — the
    /// process starts even when the `DirectoryService` is not yet reachable, and
    /// the presence loop's backoff retry absorbs the startup window instead of
    /// the process crashing (which would offload retries onto a k8s
    /// `CrashLoopBackOff`).
    ///
    /// # Runtime context
    /// Must be called from within a Tokio runtime context: building the lazy
    /// channel initialises the hyper reactor. Calling it outside a runtime
    /// returns an error rather than panicking (it still does not connect).
    ///
    /// # Errors
    /// Returns an error if called outside a Tokio runtime context, or if `uri`
    /// is malformed — never for an unreachable peer.
    pub fn connect_lazy(uri: impl Into<String>) -> Result<Self> {
        let cfg = GrpcClientConfig::new("directory");
        let channel: Channel = connect_lazy(uri, &cfg)?;
        Ok(Self::from_channel(channel))
    }

    /// Create a directory client with a **lazily-connecting** channel, attaching
    /// `interceptor`'s platform-plane credential to every outbound call.
    ///
    /// The lazy counterpart of [`connect_with_interceptor`](Self::connect_with_interceptor);
    /// see [`connect_lazy`](Self::connect_lazy) for the connection semantics.
    /// The URI is validated before `interceptor` is consumed, so the credential
    /// is only moved into the client on success.
    ///
    /// # Errors
    /// Returns an error only if `uri` is malformed — never for an unreachable
    /// peer.
    pub fn connect_lazy_with_interceptor(
        uri: impl Into<String>,
        interceptor: InternalAuthInterceptor,
    ) -> Result<Self> {
        let cfg = GrpcClientConfig::new("directory");
        // Validate the URI (build the channel) before consuming `interceptor`.
        let channel: Channel = connect_lazy(uri, &cfg)?;
        Ok(Self::from_channel_with_interceptor(channel, interceptor))
    }

    /// Connect to a directory service with custom configuration and retry logic.
    ///
    /// Uses exponential backoff based on `cfg.max_retries`, `cfg.base_backoff`,
    /// and `cfg.max_backoff` settings.
    ///
    /// # Errors
    /// It will return an error when it fails
    pub async fn connect_with_retry(
        uri: impl Into<String>,
        cfg: &GrpcClientConfig,
    ) -> Result<Self> {
        let channel: Channel = connect_with_retry(uri, cfg).await?;
        Ok(Self::from_channel(channel))
    }

    /// Connect to a directory service without retry logic.
    ///
    /// This method attempts a single connection. Use `connect` or `connect_with_retry`
    /// for production scenarios where the directory service may not be immediately available.
    ///
    /// # Errors
    /// It will return an error when it fails
    pub async fn connect_no_retry(uri: impl Into<String>, cfg: &GrpcClientConfig) -> Result<Self> {
        let uri_string = uri.into();

        // Create endpoint with timeouts from config
        let endpoint = tonic::transport::Endpoint::from_shared(uri_string)?
            .connect_timeout(cfg.connect_timeout)
            .timeout(cfg.rpc_timeout);

        // Connect to the service
        let channel = endpoint.connect().await?;

        if cfg.enable_tracing {
            tracing::debug!(
                service_name = cfg.service_name,
                connect_timeout_ms = cfg.connect_timeout.as_millis(),
                rpc_timeout_ms = cfg.rpc_timeout.as_millis(),
                "directory gRPC client connected"
            );
        }

        Ok(Self::from_channel(channel))
    }

    /// Create from an existing channel (useful for testing or custom setup).
    ///
    /// Attaches no platform-plane credential; use
    /// [`from_channel_with_interceptor`](Self::from_channel_with_interceptor)
    /// to attach one.
    #[must_use]
    pub fn from_channel(channel: Channel) -> Self {
        Self::from_channel_with_interceptor(channel, InternalAuthInterceptor::disabled())
    }

    /// Create from an existing channel, attaching `interceptor`'s platform-plane
    /// credential to every outbound call.
    #[must_use]
    pub fn from_channel_with_interceptor(
        channel: Channel,
        interceptor: InternalAuthInterceptor,
    ) -> Self {
        Self {
            inner: DirectoryServiceClient::with_interceptor(channel, interceptor),
        }
    }
}

#[async_trait]
impl DirectoryClient for DirectoryGrpcClient {
    async fn resolve_grpc_service(&self, service_name: &str) -> Result<ServiceEndpoint> {
        let mut client = self.inner.clone();
        let request = tonic::Request::new(ResolveGrpcServiceRequest {
            service_name: service_name.to_owned(),
        });

        let response = client
            .resolve_grpc_service(request)
            .await
            .map_err(|e| lookup_error(&format!("service {service_name}"), &e))?;

        let proto_response = response.into_inner();
        Ok(ServiceEndpoint::new(proto_response.endpoint_uri))
    }

    async fn resolve_rest_service(&self, gear_name: &str) -> Result<ServiceEndpoint> {
        let mut client = self.inner.clone();
        let request = tonic::Request::new(ResolveRestServiceRequest {
            gear_name: gear_name.to_owned(),
        });

        let response = client
            .resolve_rest_service(request)
            .await
            .map_err(|e| lookup_error(&format!("gear {gear_name}"), &e))?;

        let proto_response = response.into_inner();
        Ok(ServiceEndpoint::new(proto_response.endpoint_uri))
    }

    async fn get_openapi_spec(&self, gear_name: &str) -> Result<String> {
        let mut client = self.inner.clone();
        let request = tonic::Request::new(GetOpenApiSpecRequest {
            gear_name: gear_name.to_owned(),
        });

        let response = client
            .get_open_api_spec(request)
            .await
            .map_err(|e| lookup_error(&format!("openapi spec for gear {gear_name}"), &e))?;

        Ok(response.into_inner().openapi_spec)
    }

    async fn list_instances(&self, gear: &str) -> Result<Vec<ServiceInstanceInfo>> {
        let mut client = self.inner.clone();
        let request = tonic::Request::new(ListInstancesRequest {
            gear_name: gear.to_owned(),
            match_labels: std::collections::HashMap::new(),
        });

        let response = client
            .list_instances(request)
            .await
            .map_err(|e| lookup_error(&format!("instances of gear {gear}"), &e))?;

        let instances = response
            .into_inner()
            .instances
            .into_iter()
            .map(proto_instance_to_domain)
            .collect();

        Ok(instances)
    }

    async fn resolve_by_labels(
        &self,
        gear: &str,
        selector: &LabelSelector,
    ) -> Result<Vec<ServiceInstanceInfo>> {
        // Push the selector server-side so the directory returns only matching
        // instances. Every `list_instances` response is spec-free (only the
        // `openapi_spec_hash` rides along; the document is fetched via
        // `GetOpenApiSpec`), so an empty (match-all) selector never leaks a full
        // OpenAPI document over the wire.
        //
        // cancel-safe: the single await is the unary `list_instances` RPC, which
        // precedes any local state change; cancelling it just drops the in-flight
        // response and leaves nothing partially applied.
        let mut client = self.inner.clone();
        let match_labels = selector
            .match_labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let request = tonic::Request::new(ListInstancesRequest {
            gear_name: gear.to_owned(),
            match_labels,
        });

        let response = client
            .list_instances(request)
            .await
            .map_err(|e| lookup_error(&format!("instances of gear {gear}"), &e))?;

        let instances = response
            .into_inner()
            .instances
            .into_iter()
            .map(proto_instance_to_domain)
            .filter(|i| selector.matches(&i.labels))
            .collect();

        Ok(instances)
    }

    async fn list_all_instances(&self) -> Result<Vec<ServiceInstanceInfo>> {
        let mut client = self.inner.clone();
        let response = client
            .list_all_instances(tonic::Request::new(ListAllInstancesRequest {}))
            .await
            .map_err(|e| lookup_error("all instances", &e))?;

        let instances = response
            .into_inner()
            .instances
            .into_iter()
            .map(|proto| proto_instance_to_domain(proto).without_labels())
            .collect();

        Ok(instances)
    }

    async fn register_instance(&self, info: RegisterInstanceInfo) -> Result<()> {
        let mut client = self.inner.clone();

        // Convert gRPC service endpoints
        let grpc_services = info
            .grpc_services
            .into_iter()
            .map(|(name, ep)| GrpcServiceEndpoint {
                service_name: name,
                endpoint_uri: ep.uri,
            })
            .collect();

        let req = RegisterInstanceRequest {
            gear_name: info.gear,
            instance_id: info.instance_id,
            grpc_services,
            version: info.version.unwrap_or_default(),
            rest_endpoint_uri: info.rest_endpoint.map(|ep| ep.uri),
            openapi_spec: info.openapi_spec,
            labels: info.labels.into_iter().collect(),
        };

        client
            .register_instance(tonic::Request::new(req))
            .await
            .map_err(|e| call_error("register_instance", &e))?;

        Ok(())
    }

    async fn deregister_instance(&self, gear: &str, instance_id: &str) -> Result<()> {
        let mut client = self.inner.clone();

        let req = DeregisterInstanceRequest {
            gear_name: gear.to_owned(),
            instance_id: instance_id.to_owned(),
        };

        client
            .deregister_instance(tonic::Request::new(req))
            .await
            .map_err(|e| call_error("deregister_instance", &e))?;

        Ok(())
    }

    async fn send_heartbeat(&self, gear: &str, instance_id: &str) -> Result<()> {
        let mut client = self.inner.clone();

        let req = HeartbeatRequest {
            gear_name: gear.to_owned(),
            instance_id: instance_id.to_owned(),
        };

        client
            .heartbeat(tonic::Request::new(req))
            .await
            .map_err(|e| call_error("heartbeat", &e))?;

        Ok(())
    }
}

/// Convert a proto `InstanceInfo` into the domain [`ServiceInstanceInfo`].
fn proto_instance_to_domain(proto: InstanceInfo) -> ServiceInstanceInfo {
    ServiceInstanceInfo {
        gear: proto.gear_name,
        instance_id: proto.instance_id,
        endpoint: if proto.endpoint_uri.is_empty() {
            None
        } else {
            Some(ServiceEndpoint::new(proto.endpoint_uri))
        },
        version: if proto.version.is_empty() {
            None
        } else {
            Some(proto.version)
        },
        rest_endpoint: proto.rest_endpoint_uri.map(ServiceEndpoint::new),
        openapi_spec_hash: proto.openapi_spec_hash,
        // The `InstanceInfo` proto message carries no per-service gRPC
        // breakdown, so nothing to reconstruct over the OoP directory transport;
        // only the in-process `LocalDirectoryClient` populates this (from the
        // live `GearInstance`). No consumer reads `grpc_services` off a
        // gRPC-obtained instance today — a single gRPC endpoint is available via
        // the primary `endpoint`. When a label-targeted gRPC client needs the
        // full per-service map remotely (the TopologyView work), add a
        // `repeated GrpcServiceEndpoint grpc_services` field to `InstanceInfo`.
        grpc_services: Vec::new(),
        // Stable addressing labels cross the wire (ordering is normalized into a
        // BTreeMap for deterministic selector matching).
        labels: proto.labels.into_iter().collect::<BTreeMap<_, _>>(),
        // Live serving state so label-targeted callers can filter on health.
        state: proto_state_to_domain(proto.state),
    }
}

/// Map the proto `InstanceState` (an open enum carried as `i32`) onto the
/// domain [`InstanceState`].
///
/// `UNSPECIFIED` (a peer that never set the field, e.g. an older server) and
/// any discriminant this build does not recognise map to the non-serving
/// [`InstanceState::Unknown`] — kept distinct from
/// [`InstanceState::Registered`] so "the state is unknown" is not silently read
/// as a known pre-serving baseline. An unrecognised discriminant is logged so
/// the version skew is observable rather than swallowed.
fn proto_state_to_domain(state: i32) -> InstanceState {
    match ProtoInstanceState::try_from(state) {
        Ok(ProtoInstanceState::Ready) => InstanceState::Ready,
        Ok(ProtoInstanceState::Healthy) => InstanceState::Healthy,
        Ok(ProtoInstanceState::Quarantined) => InstanceState::Quarantined,
        Ok(ProtoInstanceState::Draining) => InstanceState::Draining,
        Ok(ProtoInstanceState::Registered) => InstanceState::Registered,
        Ok(ProtoInstanceState::Unspecified) => InstanceState::Unknown,
        Err(_) => {
            tracing::warn!(
                raw_state = state,
                "directory returned an unrecognised InstanceState discriminant; \
                 treating as Unknown (non-serving)"
            );
            InstanceState::Unknown
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_grpc_client_can_be_constructed() {
        // Smoke test to ensure types compile and connect
        let endpoint = tonic::transport::Endpoint::from_static("http://[::1]:50051");

        // We can't actually connect without a server, but we can construct the client type
        // This ensures the API is correct
        let channel_result = endpoint.connect().await;

        // It's expected to fail since there's no server, but if it does somehow succeed:
        if let Ok(channel) = channel_result {
            let _client = DirectoryGrpcClient::from_channel(channel);
        }
    }

    #[tokio::test]
    async fn from_channel_constructs_without_connecting() {
        // `connect_lazy` yields a Channel without a live server, so both the
        // default (no-credential) and interceptor-bearing constructors can be
        // exercised offline.
        let channel = Channel::from_static("http://[::1]:50051").connect_lazy();
        let _default = DirectoryGrpcClient::from_channel(channel.clone());
        let _authed = DirectoryGrpcClient::from_channel_with_interceptor(
            channel,
            InternalAuthInterceptor::disabled(),
        );
    }

    #[tokio::test]
    async fn connect_lazy_succeeds_against_unreachable_peer() {
        // The lazy constructor performs no eager connect, so an OoP gear can
        // build its directory client before the `DirectoryService` is up
        // (`cpt-cf-adr-eventual-readiness`). Nothing is listening on port 1, yet
        // both the plain and interceptor-bearing constructors return `Ok`.
        let plain = DirectoryGrpcClient::connect_lazy("http://127.0.0.1:1");
        assert!(
            plain.is_ok(),
            "connect_lazy must not eagerly connect (unreachable peer -> Ok)"
        );

        let authed = DirectoryGrpcClient::connect_lazy_with_interceptor(
            "http://127.0.0.1:1",
            InternalAuthInterceptor::disabled(),
        );
        assert!(
            authed.is_ok(),
            "connect_lazy_with_interceptor must not eagerly connect (unreachable peer -> Ok)"
        );
    }

    #[tokio::test]
    async fn connect_lazy_rejects_malformed_uri() {
        // A malformed endpoint is a static misconfiguration worth failing fast
        // on — the only error path of the lazy constructors.
        assert!(
            DirectoryGrpcClient::connect_lazy(String::new()).is_err(),
            "connect_lazy should fail on a malformed URI"
        );
        assert!(
            DirectoryGrpcClient::connect_lazy_with_interceptor(
                String::new(),
                InternalAuthInterceptor::disabled(),
            )
            .is_err(),
            "connect_lazy_with_interceptor should fail on a malformed URI"
        );
    }

    #[tokio::test]
    async fn resolve_grpc_service_through_lazy_client_errors_not_hangs() {
        // A lazy client builds against an unreachable directory; the first RPC
        // returns a lookup/call error rather than hanging (outer timeout proves
        // non-hang; nothing is listening on port 1).
        let client =
            DirectoryGrpcClient::connect_lazy("http://127.0.0.1:1").expect("lazy build ok");
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.resolve_grpc_service("cf.directory.v1.DirectoryService"),
        )
        .await;
        assert!(
            outcome.is_ok(),
            "resolve_grpc_service through a lazy client must not hang against an unreachable peer"
        );
        assert!(
            outcome.unwrap().is_err(),
            "resolve_grpc_service against an unreachable directory must return Err"
        );
    }

    #[test]
    fn proto_instance_maps_all_fields_to_domain() {
        let proto = InstanceInfo {
            gear_name: "calc".to_owned(),
            instance_id: "calc-1".to_owned(),
            endpoint_uri: "http://calc:8080".to_owned(),
            version: "1.2.3".to_owned(),
            rest_endpoint_uri: Some("http://calc:8080".to_owned()),
            openapi_spec_hash: Some("1a2b3c4d5e6f7a8b".to_owned()),
            labels: [("shard".to_owned(), "7".to_owned())].into_iter().collect(),
            state: ProtoInstanceState::Healthy as i32,
        };
        let domain = proto_instance_to_domain(proto);
        assert_eq!(domain.gear, "calc");
        assert_eq!(domain.state, InstanceState::Healthy);
        assert_eq!(domain.instance_id, "calc-1");
        assert_eq!(
            domain.endpoint.as_ref().map(|e| e.uri.as_str()),
            Some("http://calc:8080")
        );
        assert_eq!(domain.version.as_deref(), Some("1.2.3"));
        assert_eq!(
            domain.rest_endpoint.map(|e| e.uri),
            Some("http://calc:8080".to_owned())
        );
        // Enumeration is spec-free: only the hash crosses the wire.
        assert_eq!(
            domain.openapi_spec_hash.as_deref(),
            Some("1a2b3c4d5e6f7a8b")
        );
        // Labels cross the wire and land in a BTreeMap for deterministic matching.
        assert_eq!(domain.labels.get("shard"), Some(&"7".to_owned()));
    }

    #[test]
    fn proto_instance_maps_empty_version_to_none() {
        let proto = InstanceInfo {
            gear_name: "worker".to_owned(),
            instance_id: "worker-1".to_owned(),
            endpoint_uri: "http://worker:7000".to_owned(),
            version: String::new(),
            rest_endpoint_uri: None,
            openapi_spec_hash: None,
            labels: std::collections::HashMap::new(),
            state: ProtoInstanceState::Unspecified as i32,
        };
        let domain = proto_instance_to_domain(proto);
        // A non-empty proto endpoint_uri maps to `Some`.
        assert_eq!(
            domain.endpoint.as_ref().map(|e| e.uri.as_str()),
            Some("http://worker:7000")
        );
        // An empty proto version string maps to `None` rather than an empty string.
        assert!(domain.version.is_none());
        assert!(domain.rest_endpoint.is_none());
        assert!(domain.openapi_spec_hash.is_none());
        assert!(domain.labels.is_empty());
        // An unset proto state (`UNSPECIFIED`) maps to the non-serving Unknown
        // sentinel — distinct from the pre-serving Registered baseline.
        assert_eq!(domain.state, InstanceState::Unknown);
        assert!(!domain.state.is_serving());
    }

    #[test]
    fn proto_instance_maps_empty_endpoint_to_none() {
        // proto3 carries an absent primary endpoint as an empty string; it must
        // map back to `None`, not an empty-URI sentinel a dialer could mistake
        // for a real address.
        let proto = InstanceInfo {
            gear_name: "grpc-only".to_owned(),
            instance_id: "g-1".to_owned(),
            endpoint_uri: String::new(),
            version: String::new(),
            rest_endpoint_uri: None,
            openapi_spec_hash: None,
            labels: std::collections::HashMap::new(),
            state: ProtoInstanceState::Ready as i32,
        };
        let domain = proto_instance_to_domain(proto);
        assert!(
            domain.endpoint.is_none(),
            "an empty proto endpoint_uri must map to None"
        );
    }

    #[test]
    fn proto_state_unspecified_and_unrecognised_map_to_unknown() {
        // `UNSPECIFIED` (peer never set the field) and any discriminant this
        // build does not know (e.g. a newer server) both collapse to the
        // non-serving Unknown sentinel rather than being read as Registered.
        assert_eq!(
            proto_state_to_domain(ProtoInstanceState::Unspecified as i32),
            InstanceState::Unknown
        );
        assert_eq!(proto_state_to_domain(9999), InstanceState::Unknown);
        assert!(!proto_state_to_domain(9999).is_serving());

        // Known discriminants still map through unchanged.
        assert_eq!(
            proto_state_to_domain(ProtoInstanceState::Registered as i32),
            InstanceState::Registered
        );
        assert_eq!(
            proto_state_to_domain(ProtoInstanceState::Healthy as i32),
            InstanceState::Healthy
        );
    }

    #[test]
    fn call_error_types_invalid_argument_as_permanent() {
        let err = call_error(
            "register_instance",
            &tonic::Status::invalid_argument("bad label"),
        );
        assert!(
            err.downcast_ref::<DirectoryInvalidArgument>().is_some(),
            "InvalidArgument must map to the typed permanent DirectoryInvalidArgument"
        );
    }

    #[test]
    fn call_error_types_permission_denied_as_permanent() {
        // A true authorization decision (peer not authorized, namespace not
        // allowlisted, or a server-side service-name conflict) is permanent, so
        // the presence loop stops retrying.
        let err = call_error(
            "register_instance",
            &tonic::Status::permission_denied("peer not authorized"),
        );
        assert!(
            err.downcast_ref::<DirectoryPermissionDenied>().is_some(),
            "PermissionDenied must map to the typed permanent DirectoryPermissionDenied"
        );
    }

    #[test]
    fn call_error_keeps_transient_codes_opaque() {
        // Transient codes stay untyped (code preserved in the message) so the
        // presence loop keeps retrying. `Unauthenticated` is among them — a
        // stale/rotated credential must not collapse to the permanent sentinel.
        for status in [
            tonic::Status::unavailable("connection reset"),
            tonic::Status::unauthenticated("invalid internal token"),
        ] {
            let err = call_error("heartbeat", &status);
            assert!(err.downcast_ref::<DirectoryInvalidArgument>().is_none());
            assert!(
                err.downcast_ref::<DirectoryPermissionDenied>().is_none(),
                "{:?} must stay transient/retryable, not map to DirectoryPermissionDenied",
                status.code()
            );
            assert!(err.to_string().contains(&format!("{:?}", status.code())));
        }
    }
}
