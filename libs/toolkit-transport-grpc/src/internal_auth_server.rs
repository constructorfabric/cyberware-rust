//! Inbound (server-side) platform-plane authentication for gRPC.
//!
//! [`InternalAuthGrpcLayer`] is the gRPC counterpart of the HTTP
//! `internal_auth_middleware`: a Tower [`Layer`] installed on a tonic
//! `Server` that validates the `x-toolkit-internal-token` metadata on every
//! inbound RPC and, on success, inserts a
//! [`PlatformSecurityContext`] and a [`PeerAuthenticated`] marker into the
//! request extensions so downstream handlers can read them
//! (`cpt-cf-adr-platform-plane-auth`).
//!
//! Validation is **async** (the K8s `TokenReview` backend is an out-of-process
//! call), so this cannot use tonic's synchronous `Interceptor` trait — it is a
//! Tower service operating at the `http::Request`/`http::Response` layer, which
//! tonic propagates into the handler's [`tonic::Request`] extensions.
//!
//! # Enforcement
//!
//! - [`InternalAuthEnforcement::Required`] (default) — a non-exempt RPC without
//!   a valid token is rejected. This is the mode a platform-plane-only listener
//!   uses.
//! - [`InternalAuthEnforcement::Permissive`] — an **absent** token passes
//!   through unauthenticated (mirroring the HTTP middleware), for listeners that
//!   also serve tenant-plane / anonymous RPCs. A **present-but-invalid** token
//!   is always rejected regardless of mode.
//!
//! Whenever enforcement is active (either mode), the layer stamps a
//! [`PlatformAuthEnforced`] marker on every request it handles — before the
//! exempt check, so exempt methods and `Permissive` anonymous pass-throughs
//! carry it too. A handler uses it to fail closed on an enforcing listener; see
//! that type for the full semantics.
//!
//! Disabling enforcement entirely (Profile 1 / in-process: the process
//! boundary is the trust root) is a deliberate call —
//! [`InternalAuthGrpcLayer::disabled`] — distinct from "no authenticator was
//! supplied", so a caller cannot reach the fully-open configuration by
//! accident (e.g. by forgetting to wire a configured authenticator through).
//!
//! # Exempt methods
//!
//! Infrastructure RPCs (gRPC health checking, server reflection) are exempt by
//! path prefix — see [`DEFAULT_EXEMPT_PREFIXES`]. The allowlist is configurable
//! via [`InternalAuthGrpcLayer::with_exempt_prefixes`]. A prefix only matches on
//! a method-path segment boundary (see [`prefix_matches_boundary`]) so e.g. the
//! reflection prefix cannot be satisfied by an unrelated package that merely
//! shares the string prefix. Exemption skips *token validation* only — an
//! exempt request still carries the posture marker (see Enforcement above).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::future::Either;
use secrecy::{ExposeSecret, SecretString};
use tonic::Status;
use toolkit_security::constants::INTERNAL_TOKEN_HEADER;
use toolkit_security::{
    DynInternalAuthenticator, InternalAuthNError, InternalAuthenticator, PeerAuthenticated,
    PlatformAuthEnforced, PlatformSecurityContext,
};
use tower::{Layer, Service};

/// gRPC method path prefixes exempt from platform-plane enforcement by default.
///
/// These are the infrastructure services a client or load balancer probes
/// before (or independently of) authenticating: the standard health-checking
/// service and both versions of the reflection service, spelled out in full
/// (not a bare `grpc.reflection.` prefix) so an unrelated package that merely
/// shares the string prefix (e.g. `grpc.reflection.evil.Svc`) is never
/// accidentally exempted.
pub const DEFAULT_EXEMPT_PREFIXES: &[&str] = &[
    "/grpc.health.v1.Health/",
    "/grpc.reflection.v1.ServerReflection/",
    "/grpc.reflection.v1alpha.ServerReflection/",
];

/// Whether an **absent** platform-plane credential is rejected or allowed.
///
/// A present-but-invalid credential is always rejected, in either mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalAuthEnforcement {
    /// Reject a non-exempt RPC that does not present a valid token. The mode a
    /// dedicated platform-plane listener uses.
    #[default]
    Required,
    /// Let an RPC that presents **no** token through unauthenticated (the tenant
    /// plane, if any, is enforced separately). Mirrors the HTTP middleware.
    Permissive,
}

/// Whether the layer validates inbound requests or passes every request
/// through untouched.
///
/// Kept as its own type (rather than `Option<DynInternalAuthenticator>`
/// inline) so "authentication deliberately disabled" is a distinct,
/// explicitly-constructed state from "no authenticator value was passed"
/// (`cpt-cf-adr-platform-plane-auth`).
#[derive(Clone)]
enum AuthMode {
    /// Profile 1 / in-process: every request passes through untouched.
    Disabled,
    /// Every non-exempt request is validated against this authenticator.
    Enforced(DynInternalAuthenticator),
}

/// Immutable, shared configuration behind a [`InternalAuthGrpcLayer`].
#[derive(Clone)]
struct Config {
    mode: AuthMode,
    /// How an absent credential is treated.
    enforcement: InternalAuthEnforcement,
    /// gRPC method path prefixes exempt from enforcement.
    exempt_prefixes: Vec<String>,
}

/// Tower [`Layer`] that enforces the platform plane on inbound gRPC requests.
///
/// Install it on a tonic server with `Server::builder().layer(layer)`. The
/// layer is server-wide: it applies to every service mounted on that server
/// (tonic cannot layer an async middleware onto a single service without losing
/// `NamedService`).
#[derive(Clone)]
pub struct InternalAuthGrpcLayer {
    config: Arc<Config>,
}

impl std::fmt::Debug for InternalAuthGrpcLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalAuthGrpcLayer")
            .field(
                "enforced",
                &matches!(self.config.mode, AuthMode::Enforced(_)),
            )
            .field("enforcement", &self.config.enforcement)
            .field("exempt_prefixes", &self.config.exempt_prefixes)
            .finish()
    }
}

impl InternalAuthGrpcLayer {
    fn with_mode(mode: AuthMode) -> Self {
        Self {
            config: Arc::new(Config {
                mode,
                enforcement: InternalAuthEnforcement::Required,
                exempt_prefixes: DEFAULT_EXEMPT_PREFIXES
                    .iter()
                    .map(|p| (*p).to_owned())
                    .collect(),
            }),
        }
    }

    /// Build a layer that validates every non-exempt request against
    /// `authenticator`.
    ///
    /// Enforcement defaults to [`InternalAuthEnforcement::Required`] with the
    /// [`DEFAULT_EXEMPT_PREFIXES`] allowlist.
    #[must_use]
    pub fn new(authenticator: DynInternalAuthenticator) -> Self {
        Self::with_mode(AuthMode::Enforced(authenticator))
    }

    /// Build a layer that passes every request through untouched.
    ///
    /// This is the explicit, deliberate way to disable platform-plane
    /// enforcement (Profile 1 / in-process, where the process boundary is the
    /// trust root) — distinct from "no authenticator was configured", which
    /// this type cannot represent.
    #[must_use]
    pub fn disabled() -> Self {
        Self::with_mode(AuthMode::Disabled)
    }

    /// Override how an absent credential is treated (default:
    /// [`InternalAuthEnforcement::Required`]).
    #[must_use]
    pub fn with_enforcement(mut self, enforcement: InternalAuthEnforcement) -> Self {
        Arc::make_mut(&mut self.config).enforcement = enforcement;
        self
    }

    /// Replace the exempt method-path allowlist (default:
    /// [`DEFAULT_EXEMPT_PREFIXES`]).
    ///
    /// Each entry is matched against the gRPC method path
    /// (`/<package>.<Service>/<Method>`) on a segment boundary (see
    /// [`prefix_matches_boundary`]) — an empty string or a prefix lacking a
    /// leading `/` can never match, so a config typo cannot silently exempt
    /// every method. Pass an empty vector to enforce on every method.
    #[must_use]
    pub fn with_exempt_prefixes(mut self, prefixes: Vec<String>) -> Self {
        let prefixes = prefixes
            .into_iter()
            .filter(|p| !p.is_empty() && p.starts_with('/'))
            .collect();
        Arc::make_mut(&mut self.config).exempt_prefixes = prefixes;
        self
    }
}

impl<S> Layer<S> for InternalAuthGrpcLayer {
    type Service = InternalAuthGrpcService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        InternalAuthGrpcService {
            inner,
            config: Arc::clone(&self.config),
        }
    }
}

/// The [`Service`] produced by [`InternalAuthGrpcLayer`].
#[derive(Clone)]
pub struct InternalAuthGrpcService<S> {
    inner: S,
    config: Arc<Config>,
}

/// Outcome of reading the internal-token header off an inbound request.
enum TokenOutcome {
    /// A single, non-empty token was present.
    Present(SecretString),
    /// No internal-token header was present.
    Missing,
    /// The header was present but malformed (non-ASCII, empty, or repeated).
    Invalid,
}

/// Read (and, on a definitive outcome, consume) the
/// `x-toolkit-internal-token` header from an inbound request.
///
/// A header repeated more than once is rejected outright rather than silently
/// validating the first occurrence and forwarding the rest untouched.
fn read_token(headers: &mut http::HeaderMap) -> TokenOutcome {
    let mut values = headers.get_all(INTERNAL_TOKEN_HEADER).iter();
    let Some(first) = values.next() else {
        return TokenOutcome::Missing;
    };
    if values.next().is_some() {
        return TokenOutcome::Invalid;
    }
    let Ok(raw) = first.to_str() else {
        return TokenOutcome::Invalid;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        TokenOutcome::Invalid
    } else {
        TokenOutcome::Present(SecretString::from(trimmed))
    }
}

/// Map a neutral [`InternalAuthNError`] onto a gRPC [`Status`].
///
/// The token and any provider-specific detail are never surfaced on the wire.
fn authn_error_to_status(err: &InternalAuthNError) -> Status {
    match err {
        InternalAuthNError::InvalidToken => Status::unauthenticated("invalid internal token"),
        InternalAuthNError::Unavailable => Status::unavailable("internal-auth backend unavailable"),
        // `Other` (and, defensively, any future neutral variant) is an
        // unexpected infrastructure failure. `InternalAuthNError` is
        // `#[non_exhaustive]`, so the wildcard is required.
        _ => Status::internal("internal authentication failure"),
    }
}

/// Whether `path` matches `prefix` on a method-path segment boundary: either
/// `prefix` already ends with `/`, or `path` continues past `prefix` with a
/// `/` (or ends exactly at `prefix`).
///
/// Prevents a prefix like `/pkg.Svc/List` from also matching
/// `/pkg.Svc/ListSecrets`, and (for the built-in defaults) a bare
/// `grpc.reflection.` prefix from matching an unrelated
/// `grpc.reflection.evil.Svc` package.
fn prefix_matches_boundary(path: &str, prefix: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/')
}

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for InternalAuthGrpcService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Default + Send + 'static,
{
    type Response = http::Response<ResBody>;
    type Error = S::Error;
    // `Either::Left` (pass-through/exempt) skips the `Box::pin` allocation;
    // only `Right` (enforced) needs it.
    type Future = Either<
        S::Future,
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    // cancel-safe: dropping this future before it resolves simply drops the
    // in-flight authentication (or inner-service) call; no state is mutated
    // by this layer outside the request/response it is handling, so a
    // cancellation leaves nothing partially applied.
    fn call(&mut self, mut req: http::Request<ReqBody>) -> Self::Future {
        // Tower readiness contract: `poll_ready` was called on `self.inner`, so
        // the readiness reservation belongs to it. Move that instance into the
        // future and leave a fresh clone behind for the next call.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        // Disabled layer (Profile 1 / in-process): straight pass-through with no
        // posture marker — there is no platform-plane boundary to signal.
        let AuthMode::Enforced(authenticator) = &self.config.mode else {
            return Either::Left(inner.call(req));
        };

        // Enforcement is active. Stamp the posture marker before the exempt
        // check so exempt methods carry it too and cannot be mistaken for a
        // disabled listener downstream (see `PlatformAuthEnforced`).
        req.extensions_mut().insert(PlatformAuthEnforced);

        // Infrastructure methods (health, reflection) bypass token validation;
        // they still carry the posture marker stamped above.
        let path = req.uri().path();
        if self
            .config
            .exempt_prefixes
            .iter()
            .any(|prefix| prefix_matches_boundary(path, prefix))
        {
            return Either::Left(inner.call(req));
        }

        let config = Arc::clone(&self.config);
        let authenticator = authenticator.clone();
        let path = path.to_owned();

        Either::Right(Box::pin(async move {
            match read_token(req.headers_mut()) {
                TokenOutcome::Present(token) => {
                    match authenticator.authenticate(token.expose_secret()).await {
                        Ok(identity) => {
                            // The layer has consumed the credential; never
                            // forward it to the handler.
                            req.headers_mut().remove(INTERNAL_TOKEN_HEADER);
                            let name = identity.peer_name().to_owned();
                            tracing::debug!(
                                peer = %name,
                                method = %path,
                                "platform-plane gRPC call authenticated"
                            );
                            req.extensions_mut().insert(PeerAuthenticated { name });
                            req.extensions_mut()
                                .insert(PlatformSecurityContext::new(identity));
                            inner.call(req).await
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                method = %path,
                                "platform-plane gRPC authentication failed"
                            );
                            Ok(authn_error_to_status(&err).into_http())
                        }
                    }
                }
                // A malformed / empty / repeated credential is rejected in
                // either mode.
                TokenOutcome::Invalid => {
                    tracing::warn!(
                        method = %path,
                        "platform-plane gRPC call rejected: malformed internal token"
                    );
                    Ok(Status::unauthenticated("invalid internal token").into_http())
                }
                TokenOutcome::Missing => match config.enforcement {
                    InternalAuthEnforcement::Permissive => inner.call(req).await,
                    InternalAuthEnforcement::Required => {
                        tracing::warn!(
                            method = %path,
                            "platform-plane gRPC call rejected: missing internal token"
                        );
                        Ok(Status::unauthenticated("missing internal token").into_http())
                    }
                },
            }
        }))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::future::{Ready, ready};

    use toolkit_security::PlatformIdentity;

    /// Header the [`Echo`] inner service sets to report whether the request
    /// carried a validated [`PlatformSecurityContext`] extension by the time it
    /// was called.
    const HAD_CTX_HEADER: &str = "x-test-had-ctx";
    /// Header reporting the [`PeerAuthenticated`] name the inner service saw.
    const PEER_HEADER: &str = "x-test-peer";
    /// Header reporting whether the inner service still saw the raw internal
    /// token header (it must not, once the layer has consumed it).
    const SAW_TOKEN_HEADER: &str = "x-test-saw-token";
    /// Header reporting whether the request carried the [`PlatformAuthEnforced`]
    /// posture marker by the time the inner service was called.
    const MARKER_HEADER: &str = "x-test-marker";

    /// Terminal inner service: records what extensions the request carried into
    /// response headers so tests can assert the middleware populated them.
    #[derive(Clone)]
    struct Echo;

    impl Service<http::Request<()>> for Echo {
        type Response = http::Response<()>;
        type Error = Infallible;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<()>) -> Self::Future {
            let had_ctx = req.extensions().get::<PlatformSecurityContext>().is_some();
            let peer = req
                .extensions()
                .get::<PeerAuthenticated>()
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let saw_token = req.headers().get(INTERNAL_TOKEN_HEADER).is_some();
            let marker = req.extensions().get::<PlatformAuthEnforced>().is_some();
            let mut resp = http::Response::new(());
            resp.headers_mut().insert(
                HAD_CTX_HEADER,
                if had_ctx { "1" } else { "0" }.parse().unwrap(),
            );
            resp.headers_mut().insert(
                PEER_HEADER,
                peer.parse().unwrap_or_else(|_| "".parse().unwrap()),
            );
            resp.headers_mut().insert(
                SAW_TOKEN_HEADER,
                if saw_token { "1" } else { "0" }.parse().unwrap(),
            );
            resp.headers_mut().insert(
                MARKER_HEADER,
                if marker { "1" } else { "0" }.parse().unwrap(),
            );
            ready(Ok(resp))
        }
    }

    /// A fake platform-plane validator: `"good"` authenticates as `peer-x`,
    /// `"down"` is a backend outage, `"broken"` is an unexpected
    /// infrastructure failure, anything else is an invalid token.
    struct FakeAuth;

    impl InternalAuthenticator for FakeAuth {
        async fn authenticate(&self, token: &str) -> Result<PlatformIdentity, InternalAuthNError> {
            match token {
                "good" => Ok(PlatformIdentity::Shared {
                    name: "peer-x".to_owned(),
                }),
                "down" => Err(InternalAuthNError::Unavailable),
                "broken" => Err(InternalAuthNError::Other("boom".to_owned())),
                _ => Err(InternalAuthNError::InvalidToken),
            }
        }
    }

    fn authed_layer() -> InternalAuthGrpcLayer {
        InternalAuthGrpcLayer::new(DynInternalAuthenticator::new(FakeAuth))
    }

    fn request(path: &str, token: Option<&str>) -> http::Request<()> {
        let mut builder = http::Request::builder().uri(path);
        if let Some(token) = token {
            builder = builder.header(INTERNAL_TOKEN_HEADER, token);
        }
        builder.body(()).unwrap()
    }

    /// Drive one request through the layered service, going through the
    /// Tower `poll_ready`/`call` contract (rather than calling `call`
    /// directly) to also exercise `InternalAuthGrpcService::poll_ready` and
    /// the inner `Echo::poll_ready`.
    async fn call(layer: &InternalAuthGrpcLayer, req: http::Request<()>) -> http::Response<()> {
        let mut svc = layer.clone().layer(Echo);
        tower::ServiceExt::ready(&mut svc).await.unwrap();
        svc.call(req).await.unwrap()
    }

    fn grpc_status(resp: &http::Response<()>) -> Option<i32> {
        resp.headers()
            .get("grpc-status")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
    }

    #[tokio::test]
    async fn disabled_layer_passes_through() {
        let layer = InternalAuthGrpcLayer::disabled();
        // No token, Required-by-default — but disabled is a no-op.
        let resp = call(&layer, request("/pkg.Svc/Method", None)).await;
        assert!(grpc_status(&resp).is_none(), "must not reject");
        assert_eq!(resp.headers().get(HAD_CTX_HEADER).unwrap(), "0");
        assert_eq!(
            resp.headers().get(MARKER_HEADER).unwrap(),
            "0",
            "a disabled layer enforces nothing, so it must not stamp the posture marker"
        );
    }

    #[tokio::test]
    async fn required_rejects_missing_token() {
        let resp = call(&authed_layer(), request("/pkg.Svc/Method", None)).await;
        // gRPC Unauthenticated == 16.
        assert_eq!(grpc_status(&resp), Some(16));
    }

    #[tokio::test]
    async fn valid_token_populates_extensions_and_is_stripped() {
        let resp = call(&authed_layer(), request("/pkg.Svc/Method", Some("good"))).await;
        assert!(grpc_status(&resp).is_none(), "valid token must not reject");
        assert_eq!(resp.headers().get(HAD_CTX_HEADER).unwrap(), "1");
        assert_eq!(resp.headers().get(PEER_HEADER).unwrap(), "peer-x");
        assert_eq!(
            resp.headers().get(SAW_TOKEN_HEADER).unwrap(),
            "0",
            "the handler must never see the raw internal token"
        );
        assert_eq!(
            resp.headers().get(MARKER_HEADER).unwrap(),
            "1",
            "an enforced request carries the posture marker alongside the identity"
        );
    }

    #[tokio::test]
    async fn invalid_token_is_rejected() {
        let resp = call(&authed_layer(), request("/pkg.Svc/Method", Some("nope"))).await;
        assert_eq!(grpc_status(&resp), Some(16));
    }

    #[tokio::test]
    async fn empty_token_is_rejected_even_when_permissive() {
        let layer = authed_layer().with_enforcement(InternalAuthEnforcement::Permissive);
        let resp = call(&layer, request("/pkg.Svc/Method", Some("   "))).await;
        assert_eq!(grpc_status(&resp), Some(16));
    }

    #[tokio::test]
    async fn permissive_still_rejects_invalid_token() {
        // The module doc's central claim: a present-but-invalid token is
        // rejected regardless of mode, not just an empty/malformed one.
        let layer = authed_layer().with_enforcement(InternalAuthEnforcement::Permissive);
        let resp = call(&layer, request("/pkg.Svc/Method", Some("nope"))).await;
        assert_eq!(grpc_status(&resp), Some(16));
    }

    #[tokio::test]
    async fn permissive_still_maps_backend_outage_to_unavailable() {
        let layer = authed_layer().with_enforcement(InternalAuthEnforcement::Permissive);
        let resp = call(&layer, request("/pkg.Svc/Method", Some("down"))).await;
        // gRPC Unavailable == 14.
        assert_eq!(grpc_status(&resp), Some(14));
    }

    #[tokio::test]
    async fn duplicate_token_header_is_rejected() {
        let mut req = request("/pkg.Svc/Method", Some("good"));
        req.headers_mut()
            .append(INTERNAL_TOKEN_HEADER, "good".parse().unwrap());
        let resp = call(&authed_layer(), req).await;
        assert_eq!(
            grpc_status(&resp),
            Some(16),
            "a repeated internal-token header must be rejected, not validated on the first value"
        );
    }

    #[tokio::test]
    async fn backend_unavailable_maps_to_unavailable() {
        let resp = call(&authed_layer(), request("/pkg.Svc/Method", Some("down"))).await;
        // gRPC Unavailable == 14.
        assert_eq!(grpc_status(&resp), Some(14));
    }

    #[tokio::test]
    async fn permissive_allows_missing_token() {
        let layer = authed_layer().with_enforcement(InternalAuthEnforcement::Permissive);
        let resp = call(&layer, request("/pkg.Svc/Method", None)).await;
        assert!(
            grpc_status(&resp).is_none(),
            "permissive must allow anonymous"
        );
        assert_eq!(resp.headers().get(HAD_CTX_HEADER).unwrap(), "0");
        assert_eq!(
            resp.headers().get(MARKER_HEADER).unwrap(),
            "1",
            "an enforcing listener stamps the posture marker even for an anonymous \
             pass-through, so a downstream handler can fail closed on it"
        );
    }

    #[tokio::test]
    async fn exempt_path_bypasses_authentication_but_still_marks_posture() {
        // Health check with no token is allowed even under Required.
        let resp = call(
            &authed_layer(),
            request("/grpc.health.v1.Health/Check", None),
        )
        .await;
        assert!(
            grpc_status(&resp).is_none(),
            "exempt method must pass through without a token"
        );
        assert_eq!(resp.headers().get(HAD_CTX_HEADER).unwrap(), "0");
        // Exemption skips authentication, not enforcement posture: the marker is
        // still stamped so a handler can't mistake an exempt request on an
        // enforcing listener for a disabled-listener (fail-open) one and grant
        // it authority (e.g. over another gear's registration).
        assert_eq!(
            resp.headers().get(MARKER_HEADER).unwrap(),
            "1",
            "an exempt method on an enforcing listener must still carry the posture marker"
        );
    }

    #[tokio::test]
    async fn reflection_boundary_does_not_leak_to_unrelated_package() {
        // A package that merely shares the `grpc.reflection.` string prefix
        // (but is neither of the two real reflection services) must not be
        // exempted.
        let resp = call(
            &authed_layer(),
            request("/grpc.reflection.evil.Svc/Method", None),
        )
        .await;
        assert_eq!(
            grpc_status(&resp),
            Some(16),
            "a look-alike package must not inherit the reflection exemption"
        );
    }

    #[tokio::test]
    async fn custom_exempt_prefixes_replace_defaults() {
        let layer = authed_layer().with_exempt_prefixes(vec!["/my.Svc/".to_owned()]);
        // The custom prefix is now exempt.
        let resp = call(&layer, request("/my.Svc/Ping", None)).await;
        assert!(grpc_status(&resp).is_none());
        // The former default (health) is no longer exempt.
        let resp = call(&layer, request("/grpc.health.v1.Health/Check", None)).await;
        assert_eq!(grpc_status(&resp), Some(16));
    }

    #[tokio::test]
    async fn malformed_exempt_prefixes_are_dropped() {
        // An empty string or a prefix without a leading '/' can never match a
        // gRPC method path, so keeping it in the allowlist would either do
        // nothing or (for an empty string) match every path via `starts_with`.
        // `with_exempt_prefixes` filters both out rather than accepting a
        // config typo that silently disables enforcement server-wide.
        let layer = authed_layer().with_exempt_prefixes(vec![
            String::new(),
            "no-leading-slash".to_owned(),
            "/my.Svc/".to_owned(),
        ]);
        let resp = call(&layer, request("/pkg.Svc/Method", None)).await;
        assert_eq!(
            grpc_status(&resp),
            Some(16),
            "an empty exempt entry must not exempt every path"
        );
        let resp = call(&layer, request("/my.Svc/Ping", None)).await;
        assert!(
            grpc_status(&resp).is_none(),
            "the valid entry still applies"
        );
    }

    #[tokio::test]
    async fn non_ascii_token_header_is_rejected() {
        let mut req = request("/pkg.Svc/Method", None);
        req.headers_mut().insert(
            INTERNAL_TOKEN_HEADER,
            http::HeaderValue::from_bytes(b"\xff\xfe").unwrap(),
        );
        let resp = call(&authed_layer(), req).await;
        assert_eq!(
            grpc_status(&resp),
            Some(16),
            "a non-UTF-8 token header must be rejected, not silently ignored"
        );
    }

    #[tokio::test]
    async fn unexpected_backend_error_maps_to_internal() {
        // `InternalAuthNError::Other` (and, defensively, any future
        // non-exhaustive neutral variant) must never leak provider detail on
        // the wire; it maps to gRPC `Internal`, not a token-shaped rejection.
        let resp = call(&authed_layer(), request("/pkg.Svc/Method", Some("broken"))).await;
        // gRPC Internal == 13.
        assert_eq!(grpc_status(&resp), Some(13));
    }

    #[test]
    fn prefix_boundary_examples() {
        assert!(prefix_matches_boundary(
            "/grpc.health.v1.Health/Check",
            "/grpc.health.v1.Health/"
        ));
        assert!(!prefix_matches_boundary(
            "/pkg.Svc/ListSecrets",
            "/pkg.Svc/List"
        ));
        assert!(prefix_matches_boundary("/pkg.Svc/List", "/pkg.Svc/List"));
        assert!(prefix_matches_boundary(
            "/pkg.Svc/List/sub",
            "/pkg.Svc/List"
        ));
    }
}
