//! End-to-end proof of the directory-resolving REST client (eventual
//! readiness and churn), exercised against the real `GearManager` directory
//! and a live `Axum` server serving the `PaymentApi` routes.
//!
//! Covers the three consumer-side requirements:
//! 1. **Provider registers its REST endpoint** in the directory (via
//!    `GearManager::register_instance` with a `rest_endpoint`).
//! 2. **Not-ready tolerance** — calling before the provider is registered
//!    yields `CanonicalError::ServiceUnavailable`, never a panic.
//! 3. **Runtime churn** — provider deregistered (pod vanished) → calls fail
//!    cleanly; a new instance on a different endpoint → the resolving client
//!    re-resolves, rebuilds, and recovers automatically.

#![allow(clippy::unwrap_used)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use uuid::Uuid;

use api_contracts_sdk::contract::PaymentApi;
use api_contracts_sdk::models::{ChargeRequest, PaymentStatus};
use api_contracts_sdk::rest::PaymentApiRestResolvingClient;

use toolkit::api::OpenApiRegistryImpl;
use toolkit::discovery::DirectoryEndpointResolver;
use toolkit::{DirectoryClient, Endpoint, GearInstance, GearManager, LocalDirectoryClient};
use toolkit_canonical_errors::CanonicalError;
use toolkit_contract::policy::PolicyStack;
use toolkit_contract::runtime::resolving::EndpointResolver;
use toolkit_contract::wiring::ClientTuning;
use toolkit_security::SecurityContext;

use cf_api_contracts::api::rest::routes::register_routes;
use cf_api_contracts::domain::local_client::PaymentLocalClient;
use cf_api_contracts::domain::service::PaymentDomainService;

const PROVIDER_GEAR: &str = "api-contracts";

/// Spin up a live `PaymentApi` HTTP server on an ephemeral port.
///
/// Returns the address plus a shutdown handle: dropping the sender stops the
/// server, which is how a test models a pod actually going away rather than
/// merely being deregistered from the directory.
async fn start_server() -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let svc = Arc::new(PaymentDomainService::new());
    let local: Arc<dyn PaymentApi> =
        Arc::new(PaymentLocalClient::new(svc, Arc::new(PolicyStack::new())));

    let secctx_layer = axum::middleware::from_fn(
        |mut req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(SecurityContext::anonymous());
            next.run(req).await
        },
    );

    let openapi = OpenApiRegistryImpl::new();
    let app = register_routes(Router::new(), &openapi, local).layer(secctx_layer);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        drop(
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    // Err means the sender was dropped, which is also a stop.
                    drop(rx.await);
                })
                .await,
        );
    });
    (addr, tx)
}

/// Register `addr` as a healthy REST instance of `PROVIDER_GEAR` and return its id.
fn register_provider(mgr: &GearManager, addr: SocketAddr) -> Uuid {
    let id = Uuid::new_v4();
    let inst = GearInstance::new(PROVIDER_GEAR, id)
        .with_rest_endpoint(Endpoint::http(&addr.ip().to_string(), addr.port()));
    mgr.register_instance(Arc::new(inst)).unwrap();
    // Promote Registered -> Healthy so round-robin prefers it.
    mgr.update_heartbeat(PROVIDER_GEAR, id, Instant::now());
    id
}

fn resolving_client(mgr: &Arc<GearManager>) -> PaymentApiRestResolvingClient {
    // The production adapter, not a test stand-in. An earlier version of this
    // test wrapped the directory in a resolver that mapped *every* failure to
    // `Ok(None)`, which is the opposite of what production did at the time —
    // so the tests passed while the shipped path classified every unregistered
    // provider as a directory outage.
    let dir: Arc<dyn DirectoryClient> = Arc::new(LocalDirectoryClient::new(Arc::clone(mgr)));
    let resolver: Arc<dyn EndpointResolver> = Arc::new(DirectoryEndpointResolver::new(dir));
    PaymentApiRestResolvingClient::new(resolver, PROVIDER_GEAR, ClientTuning::default())
}

fn charge_req() -> ChargeRequest {
    ChargeRequest::new(1000, "USD", "resolving-client test")
}

#[tokio::test]
async fn unresolved_when_provider_not_registered() {
    let mgr = Arc::new(GearManager::new());
    let client = resolving_client(&mgr);

    // No provider registered yet → directory resolves nothing.
    let err = client
        .charge(SecurityContext::anonymous(), charge_req())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CanonicalError::ServiceUnavailable { .. }),
        "expected ServiceUnavailable, got {err:?}"
    );
}

#[tokio::test]
async fn resolves_and_calls_live_provider() {
    let mgr = Arc::new(GearManager::new());
    let (addr, _server) = start_server().await;
    register_provider(&mgr, addr);

    let client = resolving_client(&mgr);
    let resp = client
        .charge(SecurityContext::anonymous(), charge_req())
        .await
        .unwrap();
    assert_eq!(resp.status, PaymentStatus::Pending);
    assert!(!resp.payment_id.is_nil());
}

#[tokio::test]
async fn recovers_after_provider_vanishes_and_returns() {
    let mgr = Arc::new(GearManager::new());

    // Provider A is up and registered.
    let (addr_a, server_a) = start_server().await;
    let id_a = register_provider(&mgr, addr_a);

    let client = resolving_client(&mgr);
    assert_eq!(
        client
            .charge(SecurityContext::anonymous(), charge_req())
            .await
            .unwrap()
            .status,
        PaymentStatus::Pending,
    );

    // Provider A's pod vanishes: deregistered *and* the socket goes away.
    // Stopping the server matters — `DirectoryEndpointResolver` memoizes a
    // successful resolution for its TTL, so for a short window the client still
    // holds A's address. If A were still listening it would keep answering, and
    // this test would be asserting on a scenario that cannot happen in
    // production.
    mgr.deregister(PROVIDER_GEAR, id_a);
    drop(server_a);

    let err = client
        .charge(SecurityContext::anonymous(), charge_req())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CanonicalError::ServiceUnavailable { .. }),
        "expected ServiceUnavailable after provider vanished, got {err:?}"
    );

    // A fresh instance appears on a different endpoint. Recovery is bounded by
    // the resolver's memoization window rather than being instant: the cache
    // holds only successful resolutions and is not invalidated by a failed
    // call, so A's dead address is handed out until the entry ages out. That is
    // the documented self-heal behaviour (`DirectoryEndpointResolver::TTL`).
    let (addr_b, _server_b) = start_server().await;
    register_provider(&mgr, addr_b);
    tokio::time::sleep(DirectoryEndpointResolver::TTL + Duration::from_millis(100)).await;

    assert_eq!(
        client
            .charge(SecurityContext::anonymous(), charge_req())
            .await
            .unwrap()
            .status,
        PaymentStatus::Pending,
    );
}
