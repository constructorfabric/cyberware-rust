use super::*;

use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    routing::get,
};
use http::{Request as HttpRequest, StatusCode, header};
use toolkit_security::{
    InternalAuthNError, InternalAuthenticator, PlatformIdentity, SecurityContext,
};
use tower::ServiceExt;

const GOOD_TOKEN: &str = "valid-token";
const UNAVAILABLE_TOKEN: &str = "unavailable-token";
const INTERNAL_TOKEN: &str = "internal-failure-token";
const INTERNAL_HEADER: &str = "x-toolkit-internal-token";
const SA_GOOD: &str = "good-sa-token";
const SA_UNAVAILABLE: &str = "unavailable-sa-token";
const SA_INTERNAL: &str = "internal-failure-sa-token";
const PROBLEM_JSON: &str = "application/problem+json";

/// Authenticator stand-in: accepts `GOOD_TOKEN`, signals a transient
/// backend outage for `UNAVAILABLE_TOKEN`, an unexpected infrastructure
/// failure for `INTERNAL_TOKEN`, and rejects everything else (a forged or
/// expired JWT).
struct StubAuthenticator;

impl BearerAuthenticator for StubAuthenticator {
    async fn authenticate(&self, token: &str) -> Result<SecurityContext, AuthNError> {
        match token {
            GOOD_TOKEN => Ok(SecurityContext::anonymous()),
            UNAVAILABLE_TOKEN => Err(AuthNError::Unavailable),
            INTERNAL_TOKEN => Err(AuthNError::Other("boom".to_owned())),
            _ => Err(AuthNError::InvalidToken),
        }
    }
}

fn app(is_anonymous: bool) -> Router {
    let authenticator = Arc::new(StubAuthenticator);

    // `security_context_middleware` runs as a `route_layer` (after routing); the
    // `AnonymousRoute` marker is added as an outer router `layer` so it is
    // present in the request extensions by the time the middleware reads it
    // (this mirrors how the bootstrap layer surfaces `!OperationSpec.authenticated` per-route).
    let secctx = axum::middleware::from_fn_with_state(
        authenticator,
        security_context_middleware::<StubAuthenticator>,
    );

    let router = Router::new()
        .route("/", get(|| async { StatusCode::OK }))
        .route_layer(secctx);

    if is_anonymous {
        router.layer(axum::Extension(AnonymousRoute))
    } else {
        router
    }
}

/// Drive a request through `router` and return `(status, content_type)`.
async fn send(router: Router, auth: Option<&str>) -> (StatusCode, Option<String>) {
    let mut builder = HttpRequest::builder().uri("/");
    if let Some(value) = auth {
        builder = builder.header(header::AUTHORIZATION, value);
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = router.oneshot(request).await.unwrap();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (response.status(), content_type)
}

/// Internal-auth stand-in: accepts `SA_GOOD` (as `flight-control`), signals
/// outage for `SA_UNAVAILABLE`, an unexpected failure for `SA_INTERNAL`, and
/// rejects everything else.
struct StubInternalAuthenticator;

impl InternalAuthenticator for StubInternalAuthenticator {
    async fn authenticate(&self, token: &str) -> Result<PlatformIdentity, InternalAuthNError> {
        match token {
            SA_GOOD => Ok(PlatformIdentity::KubernetesServiceAccount {
                namespace: "toolkit".to_owned(),
                service_account: "flight-control".to_owned(),
                pod: None,
            }),
            SA_UNAVAILABLE => Err(InternalAuthNError::Unavailable),
            SA_INTERNAL => Err(InternalAuthNError::Other("boom".to_owned())),
            _ => Err(InternalAuthNError::InvalidToken),
        }
    }
}

/// Handler that echoes the authenticated peer gear, but only when **both**
/// the [`PeerAuthenticated`] marker and the [`PlatformSecurityContext`] are
/// present in the request extensions (otherwise `"none"`).
async fn peer_echo(
    peer: Option<Extension<PeerAuthenticated>>,
    platform: Option<Extension<PlatformSecurityContext>>,
) -> String {
    match (peer, platform) {
        (Some(Extension(peer)), Some(_)) => peer.name,
        _ => "none".to_owned(),
    }
}

fn platform_app() -> Router {
    let authenticator = Arc::new(StubInternalAuthenticator);
    let layer = axum::middleware::from_fn_with_state(
        authenticator,
        internal_auth_middleware::<StubInternalAuthenticator>,
    );
    Router::new().route("/", get(peer_echo)).route_layer(layer)
}

/// Handler reporting whether the [`PlatformAuthEnforced`] posture marker is
/// present (`"1"`) or absent (`"0"`) in the request extensions.
async fn marker_echo(marker: Option<Extension<PlatformAuthEnforced>>) -> String {
    if marker.is_some() { "1" } else { "0" }.to_owned()
}

fn marker_app() -> Router {
    let authenticator = Arc::new(StubInternalAuthenticator);
    let layer = axum::middleware::from_fn_with_state(
        authenticator,
        internal_auth_middleware::<StubInternalAuthenticator>,
    );
    Router::new()
        .route("/", get(marker_echo))
        .route_layer(layer)
}

/// Stacked app: `internal_auth_middleware` (outermost, runs first) then
/// `security_context_middleware`, mirroring the DESIGN § 3.2 middleware order.
fn stacked_app() -> Router {
    let bearer = Arc::new(StubAuthenticator);
    let internal = Arc::new(StubInternalAuthenticator);
    let secctx = axum::middleware::from_fn_with_state(
        bearer,
        security_context_middleware::<StubAuthenticator>,
    );
    let internal_layer = axum::middleware::from_fn_with_state(
        internal,
        internal_auth_middleware::<StubInternalAuthenticator>,
    );
    Router::new()
        .route("/", get(|| async { StatusCode::OK }))
        .route_layer(secctx)
        .route_layer(internal_layer)
}

fn stacked_public_app() -> Router {
    let bearer = Arc::new(StubAuthenticator);
    let internal = Arc::new(StubInternalAuthenticator);
    let secctx = axum::middleware::from_fn_with_state(
        bearer,
        security_context_middleware::<StubAuthenticator>,
    );
    let internal_layer = axum::middleware::from_fn_with_state(
        internal,
        internal_auth_middleware::<StubInternalAuthenticator>,
    );
    Router::new()
        .route("/", get(|| async { StatusCode::OK }))
        .route_layer(secctx)
        .route_layer(internal_layer)
        .layer(axum::Extension(AnonymousRoute))
}

/// Drive a request with arbitrary headers through `router`, returning
/// `(status, content_type, body)`.
async fn send_headers(
    router: Router,
    headers: &[(&str, &str)],
) -> (StatusCode, Option<String>, String) {
    let mut builder = HttpRequest::builder().uri("/");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn protected_route_without_auth_is_401_problem() {
    let (status, content_type) = send(app(false), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn protected_route_with_valid_token_passes() {
    let (status, _) = send(app(false), Some("Bearer valid-token")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn forged_token_is_rejected_as_401_problem() {
    let (status, content_type) = send(app(false), Some("Bearer forged-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn invalid_auth_header_is_401_problem() {
    let (status, content_type) = send(app(false), Some("Basic dXNlcjpwYXNz")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn backend_unavailable_is_503_problem() {
    let (status, content_type) = send(app(false), Some("Bearer unavailable-token")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn unexpected_authn_failure_is_500_problem() {
    let (status, content_type) = send(app(false), Some("Bearer internal-failure-token")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn public_route_without_auth_passes_through() {
    let (status, _) = send(app(true), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn public_route_revalidates_present_token() {
    let (status, _) = send(app(true), Some("Bearer forged-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_no_header_passes_through_permissive() {
    let (status, _, body) = send_headers(platform_app(), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "none");
}

#[tokio::test]
async fn internal_stamps_posture_marker_for_authenticated_request() {
    let (status, _, body) = send_headers(marker_app(), &[(INTERNAL_HEADER, SA_GOOD)]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body, "1",
        "an enforcing HTTP listener must stamp the posture marker"
    );
}

#[tokio::test]
async fn internal_stamps_posture_marker_for_anonymous_passthrough() {
    // The inversion this closes: an anonymous caller on an enforcing HTTP
    // listener (no token, permissive pass-through) must still carry the marker
    // so a downstream handler can fail closed rather than treat it as a listener
    // with no platform plane.
    let (status, _, body) = send_headers(marker_app(), &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body, "1",
        "anonymous pass-through on an enforcing listener must still carry the marker"
    );
}

#[tokio::test]
async fn internal_valid_token_sets_peer_and_platform_context() {
    let (status, _, body) = send_headers(platform_app(), &[(INTERNAL_HEADER, SA_GOOD)]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "flight-control");
}

#[tokio::test]
async fn internal_invalid_token_is_401_problem() {
    let (status, content_type, _) =
        send_headers(platform_app(), &[(INTERNAL_HEADER, "forged")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn internal_backend_unavailable_is_503_problem() {
    let (status, content_type, _) =
        send_headers(platform_app(), &[(INTERNAL_HEADER, SA_UNAVAILABLE)]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn internal_unexpected_failure_is_500_problem() {
    let (status, content_type, _) =
        send_headers(platform_app(), &[(INTERNAL_HEADER, SA_INTERNAL)]).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn internal_empty_header_is_401_problem() {
    let (status, content_type, _) = send_headers(platform_app(), &[(INTERNAL_HEADER, "   ")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(content_type.as_deref(), Some(PROBLEM_JSON));
}

#[tokio::test]
async fn peer_authenticated_does_not_skip_jwt_validation() {
    // Valid SA token (peer authenticated) but a forged user JWT: the tenant
    // plane must still reject — peer trust is not a JWT fast path.
    let (status, _, _) = send_headers(
        stacked_app(),
        &[
            (INTERNAL_HEADER, SA_GOOD),
            (header::AUTHORIZATION.as_str(), "Bearer forged-token"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn valid_peer_and_valid_jwt_passes() {
    let (status, _, _) = send_headers(
        stacked_app(),
        &[
            (INTERNAL_HEADER, SA_GOOD),
            (header::AUTHORIZATION.as_str(), "Bearer valid-token"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn invalid_internal_token_rejected_before_tenant_plane() {
    // A bad SA token must be turned away by internal_auth_middleware before
    // security_context_middleware runs — even though the user JWT here is valid.
    let (status, _, _) = send_headers(
        stacked_app(),
        &[
            (INTERNAL_HEADER, "forged"),
            (header::AUTHORIZATION.as_str(), "Bearer valid-token"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn system_call_to_public_endpoint_passes() {
    // Valid SA token, no JWT, route is AnonymousRoute — the normal probe/platform path.
    let (status, _, _) = send_headers(stacked_public_app(), &[(INTERNAL_HEADER, SA_GOOD)]).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn public_endpoint_with_no_credentials_passes() {
    // No SA token and no JWT on an AnonymousRoute: passes (health probe).
    let (status, _, _) = send_headers(stacked_public_app(), &[]).await;
    assert_eq!(status, StatusCode::OK);
}
