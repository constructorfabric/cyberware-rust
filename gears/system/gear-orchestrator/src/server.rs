//! gRPC server implementation for `DirectoryService`
//!
//! This gear provides the gRPC service implementation for Directory Service.

use std::sync::Arc;

use tonic::{Extensions, Request, Response, Status};

use cf_system_sdks::directory::labels::is_valid_label_segment;
use cf_system_sdks::directory::{
    DeregisterInstanceRequest, DirectoryClient, DirectoryInvalidArgument, DirectoryNotFound,
    DirectoryService, DirectoryServiceNameConflict, DirectoryServiceServer, GetOpenApiSpecRequest,
    GetOpenApiSpecResponse, HeartbeatRequest, InstanceInfo, InstanceState, LabelSelector,
    ListAllInstancesRequest, ListAllInstancesResponse, ListInstancesRequest, ListInstancesResponse,
    ProtoInstanceState, RegisterInstanceInfo, RegisterInstanceRequest, ResolveGrpcServiceRequest,
    ResolveGrpcServiceResponse, ResolveRestServiceRequest, ResolveRestServiceResponse,
    ServiceEndpoint, ServiceInstanceInfo,
};
use std::collections::BTreeMap;
use toolkit_security::{PlatformAuthEnforced, PlatformSecurityContext};

use crate::domain::authz::{RegistrationPolicy, registration_authorized};

/// Map a lookup failure onto a gRPC status, keeping "not registered" distinct
/// from "the lookup itself failed".
///
/// Clients rely on this distinction: `NotFound` is reconstructed into the
/// `DirectoryNotFound` sentinel and reported as "provider not up yet", which is
/// a routine startup condition rather than an error. Blanket-mapping every
/// failure to `NotFound` would make a genuine internal fault look like a
/// missing registration and leave a consumer waiting forever without a signal.
fn lookup_status(err: &anyhow::Error) -> Status {
    if err.downcast_ref::<DirectoryNotFound>().is_some() {
        Status::not_found(err.to_string())
    } else if err.downcast_ref::<DirectoryInvalidArgument>().is_some() {
        Status::invalid_argument(err.to_string())
    } else {
        Status::internal(err.to_string())
    }
}

/// gRPC service implementation of Directory Service.
///
/// Platform-plane (`x-toolkit-internal-token`) *authentication* is applied at
/// the gRPC server boundary by `grpc-hub`'s `InternalAuthGrpcLayer`
/// (`cpt-cf-adr-platform-plane-auth`): the token is validated and, on success, a
/// `PlatformSecurityContext` / `PeerAuthenticated` is placed in the request
/// extensions before the handler runs.
///
/// This type adds the *authorization* half for the registration-mutating RPCs
/// (`register_instance` / `deregister_instance` / `heartbeat`): each reads the
/// authenticated peer identity and rejects a caller that has no authority over
/// the `gear_name` it claims (see [`registration_authorized`]). Without this a
/// peer holding any valid internal token could tamper with another gear's
/// registration or inject a rogue instance under its name. A request with no
/// identity is rejected on an enforcing listener and allowed on a disabled one
/// (see [`Self::authorize_registration`]).
#[derive(Clone)]
pub struct DirectoryServiceImpl {
    api: Arc<dyn DirectoryClient>,
    policy: Arc<RegistrationPolicy>,
}

impl DirectoryServiceImpl {
    /// Create a `DirectoryService` backed by `api` with the default (empty)
    /// authorization policy.
    ///
    /// Every authenticated peer may then act only on its own gear; how a
    /// request with no identity is handled depends on the listener's posture
    /// (see [`Self::authorize_registration`]).
    pub fn new(api: Arc<dyn DirectoryClient>) -> Self {
        Self {
            api,
            policy: Arc::new(RegistrationPolicy::default()),
        }
    }

    /// Set the registration-authorization policy.
    #[must_use]
    pub fn with_policy(mut self, policy: RegistrationPolicy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Authorize a registration-mutating RPC against the authenticated peer.
    ///
    /// `op` names the attempted operation (`"register"` / `"deregister"` /
    /// `"heartbeat"`), used in the denial `Status` and log so both reflect what
    /// was attempted.
    ///
    /// Reads the [`PlatformSecurityContext`] the platform-plane layer stamped
    /// into the request extensions and applies [`registration_authorized`].
    ///
    /// When no context was stamped, the [`PlatformAuthEnforced`] marker decides
    /// (see that type for what present/absent mean):
    ///
    /// - **absent** (enforcement disabled, Profile 1 / in-process): authorization
    ///   is skipped — failing open matches the transport's disabled posture.
    /// - **present** (an enforcing listener let a context-less caller through — a
    ///   `Permissive` anonymous caller, or a method exempt from authentication):
    ///   rejected `unauthenticated`. Otherwise dropping the token or hitting an
    ///   exempt path would let a caller with no identity act on any gear while an
    ///   honest per-gear token holder is bound to its own.
    fn authorize_registration(
        &self,
        extensions: &Extensions,
        gear_name: &str,
        op: &str,
    ) -> Result<(), Status> {
        let Some(ctx) = extensions.get::<PlatformSecurityContext>() else {
            if extensions.get::<PlatformAuthEnforced>().is_some() {
                tracing::warn!(
                    gear_name,
                    op,
                    reason = "unauthenticated",
                    "directory mutation denied: no authenticated platform peer on an enforcing listener"
                );
                return Err(Status::unauthenticated(format!(
                    "{op} requires an authenticated platform peer"
                )));
            }
            return Ok(());
        };
        if registration_authorized(ctx.identity(), gear_name, &self.policy) {
            Ok(())
        } else {
            tracing::warn!(
                peer = ctx.identity().peer_name(),
                gear_name,
                op,
                reason = "unauthorized",
                "directory mutation denied: peer is not authorized for this gear"
            );
            Err(Status::permission_denied(format!(
                "peer is not authorized to {op} this gear"
            )))
        }
    }

    /// Map a `register_instance` store error onto a gRPC status.
    ///
    /// A gRPC-service-name ownership conflict is enforced atomically at the
    /// store (`GearManager::register_instance`), which resolves the
    /// check-then-write race the per-entry `DashMap` locks leave open — a name
    /// resolved across *every* gear (`resolve_grpc_service` /
    /// `GearManager::pick_service_round_robin` scan all instances) must have a
    /// single owner or part of its traffic could be routed to the wrong gear.
    /// The conflict surfaces as the typed [`DirectoryServiceNameConflict`],
    /// which is mapped to a *static* `permission_denied` (the caller-controlled
    /// service name and the owning gear are recorded in a server-side
    /// `tracing::warn!`, never reflected back). Every other store error falls
    /// through to [`lookup_status`].
    fn register_status(gear_name: &str, err: &anyhow::Error) -> Status {
        if let Some(conflict) = err.downcast_ref::<DirectoryServiceNameConflict>() {
            tracing::warn!(
                gear_name,
                service_name = %conflict.service_name,
                owner = %conflict.owner,
                reason = "grpc_service_name_conflict",
                "registration denied: gRPC service name already owned by another gear"
            );
            return Status::permission_denied(
                "a gRPC service name in this registration is already owned by another gear",
            );
        }
        lookup_status(err)
    }
}

#[tonic::async_trait]
impl DirectoryService for DirectoryServiceImpl {
    async fn resolve_grpc_service(
        &self,
        request: Request<ResolveGrpcServiceRequest>,
    ) -> Result<Response<ResolveGrpcServiceResponse>, Status> {
        let service_name = request.into_inner().service_name;
        validate_lookup_name("service_name", &service_name)?;

        let endpoint = self
            .api
            .resolve_grpc_service(&service_name)
            .await
            .map_err(|e| lookup_status(&e))?;

        Ok(Response::new(ResolveGrpcServiceResponse {
            endpoint_uri: endpoint.uri,
        }))
    }

    async fn resolve_rest_service(
        &self,
        request: Request<ResolveRestServiceRequest>,
    ) -> Result<Response<ResolveRestServiceResponse>, Status> {
        let gear_name = request.into_inner().gear_name;
        validate_lookup_name("gear_name", &gear_name)?;

        let endpoint = self
            .api
            .resolve_rest_service(&gear_name)
            .await
            .map_err(|e| lookup_status(&e))?;

        Ok(Response::new(ResolveRestServiceResponse {
            endpoint_uri: endpoint.uri,
        }))
    }

    async fn get_open_api_spec(
        &self,
        request: Request<GetOpenApiSpecRequest>,
    ) -> Result<Response<GetOpenApiSpecResponse>, Status> {
        let gear_name = request.into_inner().gear_name;
        validate_lookup_name("gear_name", &gear_name)?;

        let openapi_spec = self
            .api
            .get_openapi_spec(&gear_name)
            .await
            .map_err(|e| lookup_status(&e))?;

        Ok(Response::new(GetOpenApiSpecResponse { openapi_spec }))
    }

    async fn list_instances(
        &self,
        request: Request<ListInstancesRequest>,
    ) -> Result<Response<ListInstancesResponse>, Status> {
        let req = request.into_inner();
        let gear_name = req.gear_name;
        validate_lookup_name("gear_name", &gear_name)?;
        let selector = LabelSelector::from_match_labels(
            req.match_labels.into_iter().collect::<BTreeMap<_, _>>(),
        );
        // Validate the caller-supplied selector against the same rules stored
        // labels obey: a malformed selector can never match a valid label, and
        // echoing it back is unsafe for the same reason register-time labels
        // are screened.
        cf_system_sdks::directory::validate_selector(&selector)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        // Enumerate once and filter by the selector (an empty selector matches
        // all). The response is always spec-free: only the `openapi_spec_hash`
        // rides along, and the full document is fetched out-of-band via
        // `GetOpenApiSpec`. This bounds every multi-instance response regardless
        // of spec size, matching `resolve_by_labels` / `list_all_instances`
        // (ADR-0009); the hash still lets a consumer detect a spec change.
        let mut instances = self
            .api
            .list_instances(&gear_name)
            .await
            .map_err(|e| lookup_status(&e))?;
        instances.retain(|inst| selector.matches(&inst.labels));

        let resp = ListInstancesResponse {
            instances: instances
                .into_iter()
                .map(domain_instance_to_proto)
                .collect(),
        };

        Ok(Response::new(resp))
    }

    async fn list_all_instances(
        &self,
        _request: Request<ListAllInstancesRequest>,
    ) -> Result<Response<ListAllInstancesResponse>, Status> {
        let instances = self
            .api
            .list_all_instances()
            .await
            .map_err(|e| lookup_status(&e))?;

        let resp = ListAllInstancesResponse {
            instances: instances
                .into_iter()
                // `labels` are omitted from the broad cross-gear snapshot;
                // `without_labels` is the shared transform (see the
                // `list_all_instances` trait doc). The full OpenAPI document is
                // never inlined into any enumeration — `InstanceInfo` carries
                // only the `openapi_spec_hash`.
                .map(|inst| domain_instance_to_proto(inst.without_labels()))
                .collect(),
        };

        Ok(Response::new(resp))
    }

    async fn register_instance(
        &self,
        request: Request<RegisterInstanceRequest>,
    ) -> Result<Response<()>, Status> {
        let (_metadata, extensions, req) = request.into_parts();

        validate_identity(&req.gear_name, &req.instance_id)?;
        // Authorize the peer over the gear it claims *after* the name is screened
        // (so the comparison is against a well-formed name) but *before* any
        // state is written.
        self.authorize_registration(&extensions, &req.gear_name, "register")?;
        validate_labels(&req.labels)?;
        for (idx, svc) in req.grpc_services.iter().enumerate() {
            // Identify the service by index, never by interpolating the
            // caller-controlled `service_name` into the (reflected) status
            // context. The name is validated as a label segment first so an
            // invalid one is rejected before storage or discovery.
            if !is_valid_label_segment(&svc.service_name) {
                return Err(Status::invalid_argument(format!(
                    "grpc service #{idx}: service name contains invalid characters"
                )));
            }
            validate_endpoint_uri(&format!("grpc service #{idx}"), &svc.endpoint_uri)?;
        }
        if let Some(uri) = &req.rest_endpoint_uri {
            validate_endpoint_uri("rest endpoint", uri)?;
        }
        if let Some(spec) = &req.openapi_spec {
            validate_openapi_spec(spec)?;
        }

        // Parse endpoints from GrpcServiceEndpoint messages
        let grpc_services = req
            .grpc_services
            .into_iter()
            .map(|svc| (svc.service_name, ServiceEndpoint::new(svc.endpoint_uri)))
            .collect();

        // Retained for the conflict log below, since `gear_name` is moved into
        // `info`.
        let gear_name = req.gear_name.clone();
        let mut info = RegisterInstanceInfo::new(req.gear_name, req.instance_id)
            .with_grpc_services(grpc_services)
            .with_labels(req.labels.into_iter().collect());
        if !req.version.is_empty() {
            info = info.with_version(req.version);
        }
        if let Some(uri) = req.rest_endpoint_uri {
            info = info.with_rest_endpoint(ServiceEndpoint::new(uri));
        }
        if let Some(spec) = req.openapi_spec {
            info = info.with_openapi_spec(spec);
        }

        // The store enforces single-gear gRPC service-name ownership atomically;
        // a conflict maps to a static `permission_denied` (see `register_status`).
        self.api
            .register_instance(info)
            .await
            .map_err(|e| Self::register_status(&gear_name, &e))?;

        Ok(Response::new(()))
    }

    async fn deregister_instance(
        &self,
        request: Request<DeregisterInstanceRequest>,
    ) -> Result<Response<()>, Status> {
        let (_metadata, extensions, req) = request.into_parts();

        validate_identity(&req.gear_name, &req.instance_id)?;
        self.authorize_registration(&extensions, &req.gear_name, "deregister")?;
        self.api
            .deregister_instance(&req.gear_name, &req.instance_id)
            .await
            .map_err(|e| lookup_status(&e))?;

        Ok(Response::new(()))
    }

    async fn heartbeat(&self, request: Request<HeartbeatRequest>) -> Result<Response<()>, Status> {
        let (_metadata, extensions, req) = request.into_parts();

        validate_identity(&req.gear_name, &req.instance_id)?;
        self.authorize_registration(&extensions, &req.gear_name, "heartbeat")?;
        self.api
            .send_heartbeat(&req.gear_name, &req.instance_id)
            .await
            .map_err(|e| lookup_status(&e))?;

        Ok(Response::new(()))
    }
}

/// Convert a domain [`ServiceInstanceInfo`] into the proto `InstanceInfo`.
fn domain_instance_to_proto(i: ServiceInstanceInfo) -> InstanceInfo {
    InstanceInfo {
        gear_name: i.gear,
        instance_id: i.instance_id,
        endpoint_uri: i.endpoint.map(|ep| ep.uri).unwrap_or_default(),
        version: i.version.unwrap_or_default(),
        rest_endpoint_uri: i.rest_endpoint.map(|ep| ep.uri),
        openapi_spec_hash: i.openapi_spec_hash,
        labels: i.labels.into_iter().collect(),
        state: domain_state_to_proto(i.state) as i32,
    }
}

/// Map the domain [`InstanceState`] onto the proto `InstanceState` enum so a
/// label-targeted caller can apply its own health policy from one resolve.
fn domain_state_to_proto(state: InstanceState) -> ProtoInstanceState {
    match state {
        InstanceState::Registered => ProtoInstanceState::Registered,
        InstanceState::Ready => ProtoInstanceState::Ready,
        InstanceState::Healthy => ProtoInstanceState::Healthy,
        InstanceState::Quarantined => ProtoInstanceState::Quarantined,
        InstanceState::Draining => ProtoInstanceState::Draining,
        InstanceState::Unknown => ProtoInstanceState::Unspecified,
    }
}

/// Upper bound on a single registrant-supplied endpoint URI. Endpoint URIs are
/// echoed verbatim to every `resolve_*` / `list_*` consumer, so they are
/// bounded and screened for control characters at the trust boundary.
const MAX_ENDPOINT_URI_LEN: usize = 2048;

/// Upper bound on a registrant-supplied `OpenAPI` document. Like endpoint URIs and
/// labels, the spec is echoed to consumers - returned verbatim by
/// `GetOpenApiSpec`, inlined into `list_instances` (when requested), and turned
/// into a public route table by the edge - so it is bounded at the trust
/// boundary rather than stored unbounded. This is a size guard only; structural
/// `OpenAPI`/JSON validation is tracked separately by
/// `cpt-cf-binding-fr-openapi-validation`. The cap sits well under the default
/// 4 MiB gRPC message size so several specs can still be inlined in one
/// `list_instances` response.
const MAX_OPENAPI_SPEC_LEN: usize = 1024 * 1024;

/// Schemes accepted for a registered endpoint URI.
///
/// Both endpoint consumers speak HTTP transports only: gRPC service endpoints
/// are dialed via tonic's `Endpoint::from_shared` (default TCP connector) and
/// REST endpoints are reverse-proxied by the `api-gateway` forwarder, which
/// parses them as an `http::Uri` and forwards over `http`/`https`. No consumer
/// connects over a Unix domain socket, so `unix` (and every other scheme) is
/// rejected here rather than stored and handed back to a consumer that cannot
/// dial it.
const SUPPORTED_ENDPOINT_SCHEMES: [&str; 2] = ["http", "https"];

/// Reject an endpoint URI that is empty, over [`MAX_ENDPOINT_URI_LEN`], carries
/// ASCII control/whitespace characters, is syntactically malformed, is missing
/// its scheme or authority, or uses a scheme no endpoint consumer can dial (see
/// [`SUPPORTED_ENDPOINT_SCHEMES`]).
///
/// The URI originates from an untrusted registrant and is later handed back to
/// every discovery consumer, so screening it here keeps a malformed,
/// undialable, or injection-style value from being stored and propagated. The
/// URI is parsed with the same [`http::Uri`] parser the gRPC transport and REST
/// proxy use, so a value accepted here is one those consumers can also parse.
fn validate_endpoint_uri(context: &str, uri: &str) -> Result<(), Status> {
    if uri.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{context}: empty endpoint URI"
        )));
    }
    if uri.len() > MAX_ENDPOINT_URI_LEN {
        return Err(Status::invalid_argument(format!(
            "{context}: endpoint URI too long: {} (max {MAX_ENDPOINT_URI_LEN})",
            uri.len()
        )));
    }
    if uri
        .chars()
        .any(|c| c.is_ascii_control() || c.is_whitespace())
    {
        return Err(Status::invalid_argument(format!(
            "{context}: endpoint URI contains control or whitespace characters"
        )));
    }
    let parsed = uri.parse::<http::Uri>().map_err(|err| {
        Status::invalid_argument(format!("{context}: malformed endpoint URI: {err}"))
    })?;
    let scheme = parsed.scheme_str().ok_or_else(|| {
        Status::invalid_argument(format!("{context}: endpoint URI is missing a scheme"))
    })?;
    if !SUPPORTED_ENDPOINT_SCHEMES.contains(&scheme) {
        return Err(Status::invalid_argument(format!(
            "{context}: unsupported endpoint scheme '{scheme}' (expected one of \
             {SUPPORTED_ENDPOINT_SCHEMES:?})"
        )));
    }
    let authority = parsed.authority().ok_or_else(|| {
        Status::invalid_argument(format!("{context}: endpoint URI missing authority (host)"))
    })?;
    if authority.as_str().contains('@') {
        return Err(Status::invalid_argument(format!(
            "{context}: endpoint URI must not contain userinfo ('@')"
        )));
    }
    if parsed.host().is_none_or(str::is_empty) {
        return Err(Status::invalid_argument(format!(
            "{context}: endpoint URI missing host"
        )));
    }
    Ok(())
}

/// Reject a registrant-supplied `OpenAPI` document over [`MAX_OPENAPI_SPEC_LEN`].
///
/// The spec originates from an untrusted registrant and is later echoed to
/// discovery consumers - returned verbatim by `GetOpenApiSpec`, inlined into
/// `list_instances`, and rendered into the edge's public route table - so an
/// unbounded value would let a single registrant inflate every such response.
/// This bounds size only; structural `OpenAPI`/JSON validation is the separate
/// `cpt-cf-binding-fr-openapi-validation`.
fn validate_openapi_spec(spec: &str) -> Result<(), Status> {
    if spec.len() > MAX_OPENAPI_SPEC_LEN {
        return Err(Status::invalid_argument(format!(
            "openapi spec too long: {} bytes (max {MAX_OPENAPI_SPEC_LEN})",
            spec.len()
        )));
    }
    Ok(())
}

/// Reject a registrant whose labels violate the directory's label rules.
///
/// The rules themselves live in the directory SDK
/// ([`cf_system_sdks::directory::labels`]) so every boundary a label can enter
/// through — this gRPC front-end, the in-process store, and a gear's own
/// start-up — enforces them identically. Here we only adapt the shared
/// [`LabelValidationError`] onto a gRPC `Status`. The registrant is remote and
/// untrusted, so this runs before the map is stored and echoed on every
/// `list_instances` / `resolve_by_labels` response; the error message never
/// interpolates the offending (caller-controlled) key/value.
fn validate_labels(labels: &std::collections::HashMap<String, String>) -> Result<(), Status> {
    cf_system_sdks::directory::labels::validate_label_pairs(
        labels.len(),
        labels.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    )
    .map_err(|e| Status::invalid_argument(e.to_string()))
}

/// Reject a lookup key (gear or service name) that is blank or violates the
/// segment charset.
///
/// This is the **same** screen `register_instance` applies on write, run here on
/// the read RPCs so both ends of the contract agree on what a valid name is: a
/// malformed name is `InvalidArgument` on lookup and register alike, while a
/// valid-but-unknown name stays a clean not-found — never conflated with a
/// caller bug. `context` is a static field label, so the offending
/// (caller-controlled) value is never interpolated into the message.
fn validate_lookup_name(context: &str, name: &str) -> Result<(), Status> {
    if name.trim().is_empty() {
        return Err(Status::invalid_argument(format!(
            "{context} must not be empty"
        )));
    }
    if !is_valid_label_segment(name) {
        return Err(Status::invalid_argument(format!(
            "{context} contains invalid characters"
        )));
    }
    Ok(())
}

/// Reject a registrant whose `(gear_name, instance_id)` identity is malformed.
///
/// A blank/malformed gear name or an `instance_id` that is not a UUID is a
/// client error, not a server fault: validating it at the trust boundary returns
/// `InvalidArgument` (so the caller's presence loop stops retrying a
/// permanently-invalid request) instead of surfacing as a bare
/// `anyhow!("Invalid instance_id ...")` mislabelled `Internal` from deeper in
/// the store. The gear-name charset screen ([`validate_lookup_name`]) also
/// rejects control / whitespace / reflected characters before storage, since the
/// name is echoed on every discovery response.
fn validate_identity(gear_name: &str, instance_id: &str) -> Result<(), Status> {
    validate_lookup_name("gear_name", gear_name)?;
    if uuid::Uuid::parse_str(instance_id).is_err() {
        return Err(Status::invalid_argument("instance_id must be a valid UUID"));
    }
    Ok(())
}

/// Create a `DirectoryService` server backed by `api`.
///
/// Platform-plane *authentication* is applied at the gRPC server boundary by
/// `grpc-hub`'s `InternalAuthGrpcLayer`; the registration RPCs additionally
/// *authorize* the authenticated peer against the gear it claims (see
/// [`RegistrationPolicy`] / [`registration_authorized`]).
pub fn make_directory_service(
    api: Arc<dyn DirectoryClient>,
    policy: RegistrationPolicy,
) -> DirectoryServiceServer<DirectoryServiceImpl> {
    DirectoryServiceServer::new(DirectoryServiceImpl::new(api).with_policy(policy))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use cf_system_sdks::directory::GrpcServiceEndpoint;
    use cf_system_sdks::directory::labels::{
        MAX_LABEL_KEY_LEN, MAX_LABEL_VALUE_LEN, MAX_LABELS, is_valid_label_segment,
    };
    use toolkit::directory::LocalDirectoryClient;
    use toolkit::runtime::GearManager;
    use toolkit_security::PlatformIdentity;
    use uuid::Uuid;

    fn service() -> DirectoryServiceImpl {
        let manager = Arc::new(GearManager::new());
        let api: Arc<dyn DirectoryClient> = Arc::new(LocalDirectoryClient::new(manager));
        DirectoryServiceImpl::new(api)
    }

    /// A [`RegistrationPolicy`] from `&str` slices.
    fn policy(trusted: &[&str], namespaces: &[&str], domains: &[&str]) -> RegistrationPolicy {
        let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect();
        RegistrationPolicy {
            trusted_registrars: set(trusted),
            platform_namespaces: set(namespaces),
            trust_domains: set(domains),
        }
    }

    /// A `DirectoryService` with the given authorization policy.
    fn service_with_policy(policy: RegistrationPolicy) -> DirectoryServiceImpl {
        let manager = Arc::new(GearManager::new());
        let api: Arc<dyn DirectoryClient> = Arc::new(LocalDirectoryClient::new(manager));
        DirectoryServiceImpl::new(api).with_policy(policy)
    }

    /// A `DirectoryService` whose `trusted_registrars` are `registrars`.
    fn service_with_registrars(registrars: &[&str]) -> DirectoryServiceImpl {
        service_with_policy(policy(registrars, &[], &[]))
    }

    /// A `DirectoryService` that only accepts `ServiceAccount` peers from
    /// `namespaces`.
    fn service_with_namespaces(namespaces: &[&str]) -> DirectoryServiceImpl {
        service_with_policy(policy(&[], namespaces, &[]))
    }

    /// A per-gear (`ServiceAccount`) platform identity named `name`.
    fn sa_identity(name: &str) -> PlatformIdentity {
        PlatformIdentity::KubernetesServiceAccount {
            namespace: "toolkit".to_owned(),
            service_account: name.to_owned(),
            pod: None,
        }
    }

    /// Attach a validated [`PlatformSecurityContext`] to a request, as
    /// `grpc-hub`'s platform-plane layer does on a real inbound call.
    fn with_identity<T>(mut request: Request<T>, identity: PlatformIdentity) -> Request<T> {
        request
            .extensions_mut()
            .insert(PlatformSecurityContext::new(identity));
        request
    }

    /// Stamp only the [`PlatformAuthEnforced`] marker — no identity — as the
    /// platform-plane layer does for an anonymous caller on an enforcing
    /// (`Permissive`) listener.
    fn with_auth_enforced<T>(mut request: Request<T>) -> Request<T> {
        request.extensions_mut().insert(PlatformAuthEnforced);
        request
    }

    fn register_req(gear: &str) -> RegisterInstanceRequest {
        RegisterInstanceRequest {
            gear_name: gear.to_owned(),
            instance_id: Uuid::new_v4().to_string(),
            rest_endpoint_uri: Some(format!("http://{gear}:8080")),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn register_rejects_oversized_labels() {
        let svc = service();

        // Too many entries.
        let many: std::collections::HashMap<String, String> = (0..=MAX_LABELS)
            .map(|i| (format!("k{i}"), "v".to_owned()))
            .collect();
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                labels: many,
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Over-long value.
        let long: std::collections::HashMap<String, String> =
            [("shard".to_owned(), "x".repeat(MAX_LABEL_VALUE_LEN + 1))].into();
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                labels: long,
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // A request exactly at the limits still registers.
        let at_limit: std::collections::HashMap<String, String> = [(
            "k".repeat(MAX_LABEL_KEY_LEN),
            "v".repeat(MAX_LABEL_VALUE_LEN),
        )]
        .into();
        svc.register_instance(Request::new(RegisterInstanceRequest {
            gear_name: "billing".to_owned(),
            instance_id: Uuid::new_v4().to_string(),
            labels: at_limit,
            ..Default::default()
        }))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn register_rejects_malformed_endpoint_uri() {
        let svc = service();

        // Control/whitespace character in a gRPC endpoint URI.
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                grpc_services: vec![GrpcServiceEndpoint {
                    service_name: "billing.Service".to_owned(),
                    endpoint_uri: "http://billing:9000\n".to_owned(),
                }],
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Empty REST endpoint URI.
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                rest_endpoint_uri: Some(String::new()),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn validate_endpoint_uri_accepts_supported_schemes() {
        // The transports every endpoint consumer can actually dial.
        validate_endpoint_uri("grpc", "http://billing:9000").unwrap();
        validate_endpoint_uri("rest", "https://billing.svc:8443").unwrap();
        // Path/query are tolerated (consumers keep only scheme + authority).
        validate_endpoint_uri("rest", "https://billing.svc/ignored?x=1").unwrap();
    }

    #[test]
    fn validate_endpoint_uri_rejects_malformed_syntax() {
        // Not parseable as a URI at all.
        let err = validate_endpoint_uri("grpc", "http://[::1").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn validate_endpoint_uri_rejects_missing_components() {
        // No scheme (bare origin-form path).
        let err = validate_endpoint_uri("grpc", "/billing/v1").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        // Supported scheme but no authority (host).
        let err = validate_endpoint_uri("rest", "http:///path").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn validate_endpoint_uri_rejects_userinfo() {
        // A `user@host` authority parses, but every consumer dials the host
        // after the `@` while the stored value reads as the userinfo prefix, so
        // a trusted-looking `http://billing@evil.com` would redirect traffic.
        for uri in [
            "http://billing@evil.com:9000",
            "https://billing@evil.com",
            "http://user:pass@evil.com",
        ] {
            let err = validate_endpoint_uri("grpc", uri).unwrap_err();
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "expected {uri} to be rejected"
            );
        }
    }

    #[test]
    fn validate_endpoint_uri_rejects_hostless_authority() {
        // Authority present but no host (`http://:8080`) is undialable.
        let err = validate_endpoint_uri("rest", "http://:8080").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn validate_endpoint_uri_rejects_unsupported_schemes() {
        // No endpoint consumer speaks these transports.
        for uri in [
            "ftp://billing:21",
            "ws://billing:9000",
            "file:///etc/passwd",
        ] {
            let err = validate_endpoint_uri("grpc", uri).unwrap_err();
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "expected {uri} to be rejected"
            );
        }
    }

    #[test]
    fn validate_endpoint_uri_rejects_unix_until_consumers_support_uds() {
        // Neither the tonic gRPC channel (default TCP connector) nor the
        // api-gateway forwarder can dial a Unix domain socket, so a `unix://`
        // endpoint is rejected at the trust boundary. Flip this to an accept
        // (and extend SUPPORTED_ENDPOINT_SCHEMES) once every consumer supports
        // UDS.
        let err = validate_endpoint_uri("rest", "unix:///run/billing.sock").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn register_rejects_unsupported_endpoint_scheme() {
        let svc = service();
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                rest_endpoint_uri: Some("ftp://billing:21".to_owned()),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn validate_openapi_spec_bounds_length() {
        // At the cap passes; one byte over is rejected. The spec is echoed to
        // every consumer, so an oversized value is refused at registration.
        validate_openapi_spec(&"x".repeat(MAX_OPENAPI_SPEC_LEN)).unwrap();
        let err = validate_openapi_spec(&"x".repeat(MAX_OPENAPI_SPEC_LEN + 1)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn register_rejects_oversized_openapi_spec() {
        let svc = service();
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                rest_endpoint_uri: Some("http://billing:8080".to_owned()),
                openapi_spec: Some("x".repeat(MAX_OPENAPI_SPEC_LEN + 1)),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn list_all_instances_omits_labels() {
        let svc = service();
        svc.register_instance(Request::new(RegisterInstanceRequest {
            gear_name: "billing".to_owned(),
            instance_id: Uuid::new_v4().to_string(),
            rest_endpoint_uri: Some("http://billing:8080".to_owned()),
            labels: [("shard".to_owned(), "a".to_owned())].into(),
            ..Default::default()
        }))
        .await
        .unwrap();

        let all = svc
            .list_all_instances(Request::new(ListAllInstancesRequest {}))
            .await
            .unwrap()
            .into_inner()
            .instances;
        assert_eq!(all.len(), 1);
        assert!(
            all[0].labels.is_empty(),
            "cross-gear list_all snapshot must not carry labels"
        );
    }

    #[tokio::test]
    async fn register_then_resolve_rest_and_openapi() {
        let svc = service();

        // Register a gear with a REST endpoint and OpenAPI spec.
        svc.register_instance(Request::new(RegisterInstanceRequest {
            gear_name: "billing".to_owned(),
            instance_id: Uuid::new_v4().to_string(),
            grpc_services: vec![GrpcServiceEndpoint {
                service_name: "billing.Service".to_owned(),
                endpoint_uri: "http://billing:9000".to_owned(),
            }],
            version: "1.0.0".to_owned(),
            rest_endpoint_uri: Some("http://billing:8080".to_owned()),
            openapi_spec: Some("{\"openapi\":\"3.1.0\"}".to_owned()),
            ..Default::default()
        }))
        .await
        .unwrap();

        // Resolve the REST endpoint.
        let rest = svc
            .resolve_rest_service(Request::new(ResolveRestServiceRequest {
                gear_name: "billing".to_owned(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(rest.endpoint_uri, "http://billing:8080");

        // Retrieve the OpenAPI spec.
        let spec = svc
            .get_open_api_spec(Request::new(GetOpenApiSpecRequest {
                gear_name: "billing".to_owned(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(spec.openapi_spec.contains("openapi"));

        // list_instances carries the REST endpoint back.
        let listed = svc
            .list_instances(Request::new(ListInstancesRequest {
                gear_name: "billing".to_owned(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.instances.len(), 1);
        assert_eq!(
            listed.instances[0].rest_endpoint_uri.as_deref(),
            Some("http://billing:8080")
        );
    }

    #[tokio::test]
    async fn resolve_rest_missing_returns_not_found() {
        let svc = service();

        let status = svc
            .resolve_rest_service(Request::new(ResolveRestServiceRequest {
                gear_name: "missing".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);

        let status = svc
            .get_open_api_spec(Request::new(GetOpenApiSpecRequest {
                gear_name: "missing".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    /// Acceptance criteria: a gear registers its REST endpoint + `OpenAPI` spec
    /// and another gear resolves both — end-to-end over gRPC via `DirectoryClient`.
    #[tokio::test]
    async fn grpc_round_trip_register_and_resolve_via_directory_client() {
        use cf_system_sdks::directory::DirectoryGrpcClient;
        use tonic::transport::Server;

        // Directory service backed by an in-memory GearManager.
        let manager = Arc::new(GearManager::new());
        let api: Arc<dyn DirectoryClient> = Arc::new(LocalDirectoryClient::new(manager));
        let grpc_service = make_directory_service(api, RegistrationPolicy::default());

        // Reserve a free port, then let the tonic server bind it.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();

        tokio::spawn(async move {
            Server::builder()
                .add_service(grpc_service)
                .serve(addr)
                .await
                .unwrap();
        });

        // A remote gear talks to the directory purely through DirectoryClient.
        let client: Arc<dyn DirectoryClient> = Arc::new(
            DirectoryGrpcClient::connect(format!("http://{addr}"))
                .await
                .unwrap(),
        );

        // Register a REST endpoint + OpenAPI spec.
        client
            .register_instance(
                RegisterInstanceInfo::new("billing", Uuid::new_v4().to_string())
                    .with_version("1.0.0")
                    .with_rest_endpoint(ServiceEndpoint::http("billing", 8080))
                    .with_openapi_spec("{\"openapi\":\"3.1.0\"}"),
            )
            .await
            .unwrap();

        // Resolve both back over the wire.
        let rest = client.resolve_rest_service("billing").await.unwrap();
        assert_eq!(rest.uri, "http://billing:8080");

        let spec = client.get_openapi_spec("billing").await.unwrap();
        assert!(spec.contains("openapi"));
    }

    /// `list_all_instances` returns every registered gear (across gears) with
    /// its REST endpoint — the edge-gateway discovery path. The full `OpenAPI`
    /// document is intentionally omitted from this snapshot (edge fetches it per
    /// gear via `GetOpenApiSpec`).
    #[tokio::test]
    async fn list_all_instances_returns_all_gears_without_specs() {
        let svc = service();

        for (gear, port) in [("billing", 8080u16), ("catalog", 8081u16)] {
            svc.register_instance(Request::new(RegisterInstanceRequest {
                gear_name: gear.to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                grpc_services: vec![],
                version: "1.0.0".to_owned(),
                rest_endpoint_uri: Some(format!("http://{gear}:{port}")),
                openapi_spec: Some(format!("{{\"openapi\":\"3.1.0\",\"x\":\"{gear}\"}}")),
                ..Default::default()
            }))
            .await
            .unwrap();
        }

        let all = svc
            .list_all_instances(Request::new(ListAllInstancesRequest {}))
            .await
            .unwrap()
            .into_inner()
            .instances;

        assert_eq!(all.len(), 2);
        for inst in &all {
            assert!(inst.rest_endpoint_uri.is_some());
            assert!(
                inst.openapi_spec_hash.is_some(),
                "discovery snapshot carries only the spec hash, never the inline document"
            );
        }
        let gears: Vec<_> = all.iter().map(|i| i.gear_name.as_str()).collect();
        assert!(gears.contains(&"billing"));
        assert!(gears.contains(&"catalog"));
    }

    /// `cpt-cf-adr-platform-plane-auth` acceptance: with the platform-plane
    /// [`InternalAuthGrpcLayer`] on the server (as `grpc-hub` wires it), the
    /// `DirectoryService` gRPC RPCs reject callers lacking a valid internal
    /// token and accept those attaching the matching shared secret via an
    /// [`InternalAuthInterceptor`] — the full outbound→inbound loop. The
    /// `probe_layer` (a [`tower::util::MapRequestLayer`] mounted just below
    /// `InternalAuthGrpcLayer`) additionally proves the layer populates the
    /// `PeerAuthenticated` extension on the request it forwards downstream.
    #[tokio::test]
    async fn grpc_enforces_internal_token_end_to_end() {
        use cf_system_sdks::directory::DirectoryGrpcClient;
        use secrecy::SecretString;
        use tonic::transport::Server;
        use toolkit_security::{DynInternalAuthenticator, SharedSecretInternalAuthenticator};
        use toolkit_transport_grpc::{InternalAuthGrpcLayer, InternalAuthInterceptor};

        const SECRET: &str = "dev-internal-token";

        let manager = Arc::new(GearManager::new());
        let api: Arc<dyn DirectoryClient> = Arc::new(LocalDirectoryClient::new(manager));
        let grpc_service = make_directory_service(api, RegistrationPolicy::default());

        // Required mode: an absent token is rejected.
        let authenticator = DynInternalAuthenticator::new(SharedSecretInternalAuthenticator::new(
            SecretString::from(SECRET),
            "peer".to_owned(),
        ));
        let auth_layer = InternalAuthGrpcLayer::new(authenticator);
        let saw_expected_peer = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_layer = tower::util::MapRequestLayer::new({
            let saw_expected_peer = Arc::clone(&saw_expected_peer);
            move |req: http::Request<_>| {
                if req
                    .extensions()
                    .get::<toolkit_security::PeerAuthenticated>()
                    .is_some_and(|peer| peer.name == "peer")
                {
                    saw_expected_peer.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                req
            }
        });

        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        // Abort the server on test exit so it does not outlive the test with the
        // ephemeral port still bound.
        let server = tokio::spawn(async move {
            Server::builder()
                .layer(auth_layer)
                .layer(probe_layer)
                .add_service(grpc_service)
                .serve(addr)
                .await
                .unwrap();
        });
        let uri = format!("http://{addr}");

        // (1) No credential -> rejected.
        let anon = DirectoryGrpcClient::connect(uri.clone()).await.unwrap();
        assert!(
            anon.list_all_instances().await.is_err(),
            "call without an internal token must be rejected"
        );

        // (2) Wrong credential -> rejected.
        let bad = DirectoryGrpcClient::connect_with_interceptor(
            uri.clone(),
            InternalAuthInterceptor::from_token(SecretString::from("wrong")),
        )
        .await
        .unwrap();
        assert!(
            bad.list_all_instances().await.is_err(),
            "call with an invalid internal token must be rejected"
        );

        // (3) Matching credential -> accepted; register then read back.
        let authed = DirectoryGrpcClient::connect_with_interceptor(
            uri,
            InternalAuthInterceptor::from_token(SecretString::from(SECRET)),
        )
        .await
        .unwrap();
        authed
            .register_instance(
                RegisterInstanceInfo::new("billing", Uuid::new_v4().to_string())
                    .with_version("1.0.0")
                    .with_rest_endpoint(ServiceEndpoint::http("billing", 8080))
                    .with_openapi_spec("{\"openapi\":\"3.1.0\"}"),
            )
            .await
            .expect("authenticated register should succeed");
        let all = authed
            .list_all_instances()
            .await
            .expect("authenticated list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].gear, "billing");

        assert!(
            saw_expected_peer.load(std::sync::atomic::Ordering::SeqCst),
            "InternalAuthGrpcLayer must populate the PeerAuthenticated extension on the request \
             it forwards to the layers below it"
        );

        server.abort();
    }

    /// Exempting a method skips *authentication*; it must not grant authority
    /// over any gear's registration. With the whole `DirectoryService` on the
    /// exempt allowlist of an enforcing listener, an anonymous (token-less)
    /// `RegisterInstance` still reaches the handler carrying the
    /// `PlatformAuthEnforced` posture marker (stamped before the exempt check),
    /// so `authorize_registration` fails closed with `unauthenticated` rather
    /// than failing open. Exercises the layer + handler together.
    #[tokio::test]
    async fn exempt_registration_fails_closed_end_to_end() {
        use cf_system_sdks::directory::{DIRECTORY_SERVICE_NAME, DirectoryGrpcClient};
        use secrecy::SecretString;
        use tonic::transport::Server;
        use toolkit_security::{DynInternalAuthenticator, SharedSecretInternalAuthenticator};
        use toolkit_transport_grpc::InternalAuthGrpcLayer;

        let manager = Arc::new(GearManager::new());
        let api: Arc<dyn DirectoryClient> = Arc::new(LocalDirectoryClient::new(manager));
        let grpc_service = make_directory_service(api, RegistrationPolicy::default());

        // Enforcing listener, but the entire DirectoryService is exempted — a
        // misconfiguration that must not become an "act on any gear" backdoor.
        let authenticator = DynInternalAuthenticator::new(SharedSecretInternalAuthenticator::new(
            SecretString::from("dev-internal-token"),
            "peer".to_owned(),
        ));
        let auth_layer = InternalAuthGrpcLayer::new(authenticator)
            .with_exempt_prefixes(vec![format!("/{DIRECTORY_SERVICE_NAME}/")]);

        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let server = tokio::spawn(async move {
            Server::builder()
                .layer(auth_layer)
                .add_service(grpc_service)
                .serve(addr)
                .await
                .unwrap();
        });
        let uri = format!("http://{addr}");

        // No token on an exempted method: authentication is skipped, but the
        // registration must still be denied — never silently accepted.
        let anon = DirectoryGrpcClient::connect(uri).await.unwrap();
        anon.register_instance(RegisterInstanceInfo::new(
            "billing",
            Uuid::new_v4().to_string(),
        ))
        .await
        .expect_err("an exempted, unauthenticated registration must be denied, not fail open");

        server.abort();
    }

    #[test]
    fn domain_state_maps_to_proto_over_all_variants() {
        // Each domain state maps to its proto counterpart; Unknown collapses to
        // the UNSPECIFIED wire sentinel. Guards against a silent all-Registered
        // or swapped-variant regression.
        for (domain, proto) in [
            (InstanceState::Registered, ProtoInstanceState::Registered),
            (InstanceState::Ready, ProtoInstanceState::Ready),
            (InstanceState::Healthy, ProtoInstanceState::Healthy),
            (InstanceState::Quarantined, ProtoInstanceState::Quarantined),
            (InstanceState::Draining, ProtoInstanceState::Draining),
            (InstanceState::Unknown, ProtoInstanceState::Unspecified),
        ] {
            assert_eq!(domain_state_to_proto(domain), proto);
        }
    }

    /// The projected `state` field is real: a freshly registered instance reads
    /// `Registered`, and after a heartbeat it reads `Healthy` in the
    /// `list_instances` response.
    #[tokio::test]
    async fn list_instances_projects_live_state() {
        let svc = service();
        let instance_id = Uuid::new_v4().to_string();

        svc.register_instance(Request::new(RegisterInstanceRequest {
            gear_name: "billing".to_owned(),
            instance_id: instance_id.clone(),
            rest_endpoint_uri: Some("http://billing:8080".to_owned()),
            ..Default::default()
        }))
        .await
        .unwrap();

        let state_of = |svc: &DirectoryServiceImpl| {
            let svc = svc.clone();
            async move {
                svc.list_instances(Request::new(ListInstancesRequest {
                    gear_name: "billing".to_owned(),
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner()
                .instances[0]
                    .state
            }
        };

        assert_eq!(
            state_of(&svc).await,
            ProtoInstanceState::Registered as i32,
            "a freshly registered instance is not yet serving"
        );

        svc.heartbeat(Request::new(HeartbeatRequest {
            gear_name: "billing".to_owned(),
            instance_id,
        }))
        .await
        .unwrap();

        assert_eq!(
            state_of(&svc).await,
            ProtoInstanceState::Healthy as i32,
            "a heartbeat transitions the instance to Healthy"
        );
    }

    #[tokio::test]
    async fn register_rejects_malformed_identity() {
        let svc = service();

        // Non-UUID instance_id.
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: "not-a-uuid".to_owned(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Empty gear name.
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: String::new(),
                instance_id: Uuid::new_v4().to_string(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn register_rejects_labels_with_disallowed_charset() {
        let svc = service();
        // A control character in a label value must be rejected (and never
        // reflected into the returned Status).
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                labels: [("shard".to_owned(), "1\r\n2".to_owned())].into(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            !err.message().contains('\n'),
            "raw label value must not be echoed into the status message"
        );
    }

    #[tokio::test]
    async fn register_rejects_gear_name_with_disallowed_charset() {
        let svc = service();
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "bad\r\nname".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            !err.message().contains('\n'),
            "raw gear name must not be echoed into the status message"
        );
    }

    #[tokio::test]
    async fn register_rejects_grpc_service_name_with_disallowed_charset() {
        let svc = service();
        let err = svc
            .register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                grpc_services: vec![GrpcServiceEndpoint {
                    service_name: "bad\r\nservice".to_owned(),
                    endpoint_uri: "http://127.0.0.1:50051".to_owned(),
                }],
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            !err.message().contains('\n'),
            "raw service name must not be echoed into the status message"
        );
    }

    #[tokio::test]
    async fn lookup_rpcs_reject_malformed_name_with_invalid_argument() {
        let svc = service();

        // Both ends of the contract agree: a malformed name is InvalidArgument
        // on the read RPCs, not a silent not-found the caller reads as "not
        // started yet". The raw (caller-controlled) name is never reflected.
        let rest = svc
            .resolve_rest_service(Request::new(ResolveRestServiceRequest {
                gear_name: "bad\r\nname".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(rest.code(), tonic::Code::InvalidArgument);
        assert!(!rest.message().contains('\n'));

        let grpc = svc
            .resolve_grpc_service(Request::new(ResolveGrpcServiceRequest {
                service_name: "bad\r\nservice".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(grpc.code(), tonic::Code::InvalidArgument);

        let spec = svc
            .get_open_api_spec(Request::new(GetOpenApiSpecRequest {
                gear_name: "bad name".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(spec.code(), tonic::Code::InvalidArgument);

        let list = svc
            .list_instances(Request::new(ListInstancesRequest {
                gear_name: String::new(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(list.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn lookup_rpcs_report_valid_unknown_name_as_not_found() {
        let svc = service();

        // A valid-but-unregistered name stays a clean not-found — distinct from
        // the InvalidArgument a malformed name earns above.
        let rest = svc
            .resolve_rest_service(Request::new(ResolveRestServiceRequest {
                gear_name: "billing".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(rest.code(), tonic::Code::NotFound);

        // A valid unknown name still lists cleanly (empty), not an error.
        let list = svc
            .list_instances(Request::new(ListInstancesRequest {
                gear_name: "billing".to_owned(),
                ..Default::default()
            }))
            .await
            .unwrap();
        assert!(list.into_inner().instances.is_empty());
    }

    #[test]
    // The `naïve` literal is intentional: it exercises rejection of a non-ASCII
    // segment, which is exactly what this charset test asserts.
    #[allow(clippy::non_ascii_literal)]
    fn label_segment_charset_rules() {
        for ok in ["shard", "1", "role-a", "a.b_c", "v1.2.3"] {
            assert!(is_valid_label_segment(ok), "{ok} should be valid");
        }
        for bad in ["", "-lead", "trail_", ".dot", "a=b", "a b", "a\tb", "naïve"] {
            assert!(!is_valid_label_segment(bad), "{bad} should be invalid");
        }
    }

    /// The `list_instances` label-selector branch: a non-empty `match_labels`
    /// returns only matching instances, spec-free; a selector matching nothing
    /// returns an empty list.
    #[tokio::test]
    async fn list_instances_with_match_labels_filters_and_strips_spec() {
        let svc = service();

        // Two instances of the same gear on different shards, both with specs.
        for shard in ["1", "2"] {
            svc.register_instance(Request::new(RegisterInstanceRequest {
                gear_name: "ingest".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                version: "1.0.0".to_owned(),
                rest_endpoint_uri: Some(format!("http://ingest-{shard}:8080")),
                openapi_spec: Some("{\"openapi\":\"3.1.0\"}".to_owned()),
                labels: [("shard".to_owned(), shard.to_owned())].into(),
                ..Default::default()
            }))
            .await
            .unwrap();
        }

        // A selector pins exactly one shard, returned spec-free.
        let matched = svc
            .list_instances(Request::new(ListInstancesRequest {
                gear_name: "ingest".to_owned(),
                match_labels: [("shard".to_owned(), "1".to_owned())].into(),
            }))
            .await
            .unwrap()
            .into_inner()
            .instances;
        assert_eq!(
            matched.len(),
            1,
            "selector must return only the matching shard"
        );
        assert_eq!(
            matched[0].labels.get("shard").map(String::as_str),
            Some("1")
        );
        assert!(
            matched[0].openapi_spec_hash.is_some(),
            "label-resolve path carries only the spec hash, never the inline document"
        );

        // A selector matching nothing returns an empty list (not an error).
        let none = svc
            .list_instances(Request::new(ListInstancesRequest {
                gear_name: "ingest".to_owned(),
                match_labels: [("shard".to_owned(), "99".to_owned())].into(),
            }))
            .await
            .unwrap()
            .into_inner()
            .instances;
        assert!(none.is_empty(), "no instance carries shard=99");

        // Every `list_instances` response is spec-free (empty selector matches
        // all): the document is never inlined, but the hash still rides along so
        // a consumer can detect a spec change and fetch it via GetOpenApiSpec.
        let all_spec_free = svc
            .list_instances(Request::new(ListInstancesRequest {
                gear_name: "ingest".to_owned(),
                match_labels: std::collections::HashMap::new(),
            }))
            .await
            .unwrap()
            .into_inner()
            .instances;
        assert_eq!(all_spec_free.len(), 2, "empty selector matches every shard");
        assert!(
            all_spec_free.iter().all(|i| i.openapi_spec_hash.is_some()),
            "list_instances must carry the spec hash, never the inline document"
        );
    }

    /// End-to-end over gRPC via `DirectoryClient`: `resolve_by_labels` returns
    /// only the instances whose labels satisfy the selector, and each carries
    /// no inline `OpenAPI` document.
    #[tokio::test]
    async fn grpc_resolve_by_labels_returns_only_matching_spec_free() {
        use cf_system_sdks::directory::{DirectoryGrpcClient, LabelSelector};
        use tonic::transport::Server;

        let manager = Arc::new(GearManager::new());
        let api: Arc<dyn DirectoryClient> = Arc::new(LocalDirectoryClient::new(manager));
        let grpc_service = make_directory_service(api, RegistrationPolicy::default());

        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(grpc_service)
                .serve(addr)
                .await
                .unwrap();
        });

        let client: Arc<dyn DirectoryClient> = Arc::new(
            DirectoryGrpcClient::connect(format!("http://{addr}"))
                .await
                .unwrap(),
        );

        // Two shards of the same gear; each publishes an OpenAPI document.
        for shard in ["1", "2"] {
            client
                .register_instance(
                    RegisterInstanceInfo::new("ingest", Uuid::new_v4().to_string())
                        .with_rest_endpoint(ServiceEndpoint::http(&format!("ingest-{shard}"), 8080))
                        .with_openapi_spec("{\"openapi\":\"3.1.0\"}")
                        .with_labels([("shard".to_owned(), shard.to_owned())].into()),
                )
                .await
                .unwrap();
        }

        // Selector pins shard 1: exactly one match, spec-free over the wire.
        let matched = client
            .resolve_by_labels("ingest", &LabelSelector::new().with("shard", "1"))
            .await
            .unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(
            matched[0].labels.get("shard").map(String::as_str),
            Some("1")
        );
        assert!(
            matched[0].openapi_spec_hash.is_some(),
            "resolve_by_labels carries only the spec hash, never the inline document"
        );

        // A selector matching nothing yields an empty set (not an error).
        let none = client
            .resolve_by_labels("ingest", &LabelSelector::new().with("shard", "99"))
            .await
            .unwrap();
        assert!(none.is_empty());

        let all = client
            .resolve_by_labels("ingest", &LabelSelector::new())
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "empty selector matches every shard");
        assert!(
            all.iter().all(|i| i.openapi_spec_hash.is_some()),
            "empty-selector resolve_by_labels carries only the spec hash"
        );

        server.abort();
    }

    // ---- Registration authorization ----
    //
    // The pure allow/deny predicate ([`registration_authorized`]) and
    // [`RegistrationPolicy`] are unit-tested in `crate::domain::authz`; the
    // tests here exercise the gRPC adapter (`DirectoryServiceImpl`) that reads
    // the peer off the request and calls into that policy.

    /// SA identity whose `peer_name` equals the gear it registers -> allowed.
    #[tokio::test]
    async fn register_allows_sa_acting_on_its_own_gear() {
        let svc = service();
        svc.register_instance(with_identity(
            Request::new(register_req("billing")),
            sa_identity("billing"),
        ))
        .await
        .expect("a gear registering under its own name must be allowed");
    }

    /// End-to-end: with a namespace allowlist configured, an SA with the right
    /// name but from a namespace outside the allowlist is denied, while the same
    /// name from an allowed namespace is accepted.
    #[tokio::test]
    async fn register_binds_sa_to_platform_namespace() {
        let svc = service_with_namespaces(&["toolkit"]);

        // `sa_identity` lives in namespace "toolkit" (allowed) -> accepted.
        svc.register_instance(with_identity(
            Request::new(register_req("billing")),
            sa_identity("billing"),
        ))
        .await
        .expect("an SA named after its gear from an allowed namespace must be accepted");

        // Same SA name, foreign namespace -> denied even though the name matches.
        let err = svc
            .register_instance(with_identity(
                Request::new(register_req("billing")),
                PlatformIdentity::KubernetesServiceAccount {
                    namespace: "tenant-x".to_owned(),
                    service_account: "billing".to_owned(),
                    pod: None,
                },
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            !err.message().contains("billing") && !err.message().contains("tenant-x"),
            "the status must not reflect the gear name or peer namespace"
        );
    }

    /// SA identity acting on another gear, not a trusted registrar -> denied
    /// across all three RPCs, and the owner's existing registration survives
    /// each attempt unchanged. Covers takeover (overwriting the owner's own
    /// instance) as well as denied deregister / heartbeat against a live entry.
    #[tokio::test]
    async fn register_deregister_heartbeat_deny_cross_gear_sa() {
        let svc = service();
        let owner_instance = Uuid::new_v4().to_string();
        let owner_endpoint = "http://billing:8080";

        // The legitimate owner registers first, so the cross-gear attempts below
        // run against a live entry rather than an empty directory.
        svc.register_instance(with_identity(
            Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: owner_instance.clone(),
                rest_endpoint_uri: Some(owner_endpoint.to_owned()),
                ..Default::default()
            }),
            sa_identity("billing"),
        ))
        .await
        .expect("the gear registering under its own name must be allowed");

        // Takeover: `catalog` tries to overwrite billing's own instance.
        let reg = svc
            .register_instance(with_identity(
                Request::new(RegisterInstanceRequest {
                    gear_name: "billing".to_owned(),
                    instance_id: owner_instance.clone(),
                    rest_endpoint_uri: Some("http://attacker:8080".to_owned()),
                    ..Default::default()
                }),
                sa_identity("catalog"),
            ))
            .await
            .unwrap_err();
        assert_eq!(reg.code(), tonic::Code::PermissionDenied);
        assert!(
            !reg.message().contains("billing") && !reg.message().contains("catalog"),
            "the status must not reflect the gear name or peer identity"
        );

        let dereg = svc
            .deregister_instance(with_identity(
                Request::new(DeregisterInstanceRequest {
                    gear_name: "billing".to_owned(),
                    instance_id: owner_instance.clone(),
                }),
                sa_identity("catalog"),
            ))
            .await
            .unwrap_err();
        assert_eq!(dereg.code(), tonic::Code::PermissionDenied);

        let hb = svc
            .heartbeat(with_identity(
                Request::new(HeartbeatRequest {
                    gear_name: "billing".to_owned(),
                    instance_id: owner_instance.clone(),
                }),
                sa_identity("catalog"),
            ))
            .await
            .unwrap_err();
        assert_eq!(hb.code(), tonic::Code::PermissionDenied);

        // The owner's instance is untouched: still the only one, same endpoint,
        // and still `Registered` — the denied deregister did not remove it, the
        // denied takeover did not overwrite its endpoint, and the denied
        // heartbeat did not promote it to `Healthy`.
        let listed = svc
            .list_instances(Request::new(ListInstancesRequest {
                gear_name: "billing".to_owned(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner()
            .instances;
        assert_eq!(
            listed.len(),
            1,
            "the cross-gear attempts must neither add nor remove instances"
        );
        assert_eq!(listed[0].instance_id, owner_instance);
        assert_eq!(
            listed[0].rest_endpoint_uri.as_deref(),
            Some(owner_endpoint),
            "the denied takeover must not overwrite the owner's endpoint"
        );
        assert_eq!(
            listed[0].state,
            ProtoInstanceState::Registered as i32,
            "the denied heartbeat must not promote the instance to Healthy"
        );
    }

    /// A denied registration must write nothing: authorization runs *before*
    /// any state mutation, so a rogue-injection attempt leaves the directory
    /// empty. Inspecting the backing store (not just the status code) is what
    /// would catch `authorize_registration` being moved after the write.
    #[tokio::test]
    async fn denied_register_writes_nothing() {
        let svc = service();

        let err = svc
            .register_instance(with_identity(
                Request::new(RegisterInstanceRequest {
                    gear_name: "billing".to_owned(),
                    instance_id: Uuid::new_v4().to_string(),
                    rest_endpoint_uri: Some("http://attacker:8080".to_owned()),
                    ..Default::default()
                }),
                sa_identity("catalog"),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        // The rogue instance was never stored: billing is unresolvable.
        let resolved = svc
            .resolve_rest_service(Request::new(ResolveRestServiceRequest {
                gear_name: "billing".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(
            resolved.code(),
            tonic::Code::NotFound,
            "a denied register must not leave a resolvable instance behind"
        );
    }

    /// The *allow* path of `deregister_instance` / `heartbeat` for a gear acting
    /// on its own name — `register_instance` alone would not catch a wrong
    /// `gear_name` passed to `authorize_registration` in the other two handlers.
    #[tokio::test]
    async fn deregister_and_heartbeat_allow_own_gear() {
        let svc = service();
        let instance_id = Uuid::new_v4().to_string();

        svc.register_instance(with_identity(
            Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: instance_id.clone(),
                rest_endpoint_uri: Some("http://billing:8080".to_owned()),
                ..Default::default()
            }),
            sa_identity("billing"),
        ))
        .await
        .expect("own-gear register must be allowed");

        svc.heartbeat(with_identity(
            Request::new(HeartbeatRequest {
                gear_name: "billing".to_owned(),
                instance_id: instance_id.clone(),
            }),
            sa_identity("billing"),
        ))
        .await
        .expect("own-gear heartbeat must be allowed");

        svc.deregister_instance(with_identity(
            Request::new(DeregisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id,
            }),
            sa_identity("billing"),
        ))
        .await
        .expect("own-gear deregister must be allowed");

        // The allowed deregister actually removed the instance.
        let resolved = svc
            .resolve_rest_service(Request::new(ResolveRestServiceRequest {
                gear_name: "billing".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(resolved.code(), tonic::Code::NotFound);
    }

    /// The *allow* path of `deregister_instance` / `heartbeat` for a trusted
    /// registrar acting on another gear's behalf.
    #[tokio::test]
    async fn deregister_and_heartbeat_allow_trusted_registrar() {
        let svc = service_with_registrars(&["flight-control"]);
        let instance_id = Uuid::new_v4().to_string();

        svc.register_instance(with_identity(
            Request::new(RegisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id: instance_id.clone(),
                rest_endpoint_uri: Some("http://billing:8080".to_owned()),
                ..Default::default()
            }),
            sa_identity("flight-control"),
        ))
        .await
        .expect("a trusted registrar may register any gear");

        svc.heartbeat(with_identity(
            Request::new(HeartbeatRequest {
                gear_name: "billing".to_owned(),
                instance_id: instance_id.clone(),
            }),
            sa_identity("flight-control"),
        ))
        .await
        .expect("a trusted registrar may heartbeat any gear");

        svc.deregister_instance(with_identity(
            Request::new(DeregisterInstanceRequest {
                gear_name: "billing".to_owned(),
                instance_id,
            }),
            sa_identity("flight-control"),
        ))
        .await
        .expect("a trusted registrar may deregister any gear");
    }

    /// An unrecognised ([`PlatformIdentity::Unknown`]) identity fails closed at
    /// the RPC boundary — not just against the private `registration_authorized`
    /// predicate — and writes nothing. Guards the fail-closed promise in the
    /// type docs against a future variant slipping through as authorized.
    #[tokio::test]
    async fn register_denies_unknown_identity_and_writes_nothing() {
        let svc = service();

        let err = svc
            .register_instance(with_identity(
                Request::new(register_req("billing")),
                PlatformIdentity::Unknown,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        let resolved = svc
            .resolve_rest_service(Request::new(ResolveRestServiceRequest {
                gear_name: "billing".to_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(
            resolved.code(),
            tonic::Code::NotFound,
            "a denied unknown-identity register must not leave a resolvable instance"
        );
    }

    /// A peer listed in `trusted_registrars` may act on any gear.
    #[tokio::test]
    async fn register_allows_trusted_registrar_for_any_gear() {
        let svc = service_with_registrars(&["flight-control"]);
        svc.register_instance(with_identity(
            Request::new(register_req("billing")),
            sa_identity("flight-control"),
        ))
        .await
        .expect("a trusted registrar must be allowed to register any gear");
    }

    /// A shared-secret identity is always allowed (documented single trust
    /// boundary), even acting on a gear whose name it does not match.
    #[tokio::test]
    async fn register_allows_shared_identity() {
        let svc = service();
        svc.register_instance(with_identity(
            Request::new(register_req("billing")),
            PlatformIdentity::Shared {
                name: "toolkit-internal".to_owned(),
            },
        ))
        .await
        .expect("a shared-secret peer must be allowed (documented trust boundary)");
    }

    /// Profile 1 / in-process (enforcement disabled, so no
    /// [`PlatformAuthEnforced`] marker): no identity was stamped, authorization
    /// is skipped, and behavior is unchanged.
    #[tokio::test]
    async fn register_allows_when_no_identity_stamped() {
        let svc = service();
        svc.register_instance(Request::new(register_req("billing")))
            .await
            .expect("with no platform identity stamped, authorization is a no-op");
    }

    /// On an enforcing listener (the [`PlatformAuthEnforced`] marker is stamped)
    /// a request with no stamped identity is rejected rather than failing open —
    /// closing the `Permissive`-listener inversion where a token-less caller
    /// could otherwise act on any gear. A properly stamped per-gear identity
    /// still succeeds.
    #[tokio::test]
    async fn register_requires_stamped_identity_when_auth_enforced() {
        let svc = service();

        let err = svc
            .register_instance(with_auth_enforced(Request::new(register_req("billing"))))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        svc.register_instance(with_identity(
            with_auth_enforced(Request::new(register_req("billing"))),
            sa_identity("billing"),
        ))
        .await
        .expect("a stamped per-gear identity must still be allowed");
    }

    /// A registration that advertises a gRPC service name is bound: a second
    /// gear cannot claim a name already owned by a different gear, but the
    /// owning gear may re-register (or add another instance under) that name.
    #[tokio::test]
    async fn register_binds_grpc_service_name_to_owning_gear() {
        let svc = service();

        let grpc = |name: &str, uri: &str| {
            vec![GrpcServiceEndpoint {
                service_name: name.to_owned(),
                endpoint_uri: uri.to_owned(),
            }]
        };

        // authz-resolver claims its service name first (unowned -> allowed).
        svc.register_instance(with_identity(
            Request::new(RegisterInstanceRequest {
                gear_name: "authz-resolver".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                grpc_services: grpc("cf.authz.v1.AuthzService", "http://authz-resolver:9000"),
                ..Default::default()
            }),
            sa_identity("authz-resolver"),
        ))
        .await
        .expect("a gear claiming an unowned service name must be allowed");

        // A different gear (authorized only for itself) advertising that same
        // service name is rejected -- otherwise resolve_grpc_service could route
        // authz traffic to it.
        let err = svc
            .register_instance(with_identity(
                Request::new(RegisterInstanceRequest {
                    gear_name: "evil".to_owned(),
                    instance_id: Uuid::new_v4().to_string(),
                    grpc_services: grpc("cf.authz.v1.AuthzService", "http://evil:9000"),
                    ..Default::default()
                }),
                sa_identity("evil"),
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            !err.message().contains("authz-resolver")
                && !err.message().contains("evil")
                && !err.message().contains("cf.authz.v1.AuthzService"),
            "the status must not reflect the service name or gear identity"
        );

        // The owning gear may register another instance under the same name.
        svc.register_instance(with_identity(
            Request::new(RegisterInstanceRequest {
                gear_name: "authz-resolver".to_owned(),
                instance_id: Uuid::new_v4().to_string(),
                grpc_services: grpc("cf.authz.v1.AuthzService", "http://authz-resolver-2:9000"),
                ..Default::default()
            }),
            sa_identity("authz-resolver"),
        ))
        .await
        .expect("the owning gear must be able to add another instance for its own service");
    }
}
