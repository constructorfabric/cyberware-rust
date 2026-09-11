use super::*;
use crate::config::KeycloakConfig;
use crate::domain::credstore::CredStoreReader;
use crate::domain::kc::transport::KcTransport;
use crate::domain::ports::metrics::{
    EndpointClass, KcAdminMetricsPort, TokenRefreshOutcome, TokenTier,
};
use crate::domain::system_actor::build_system_ctx;
use crate::domain::test_support::{NoopMetrics, StubCredStore, keycloak_cfg};
use crate::infra::kc_http::ReqwestKcTransport;
use async_trait::async_trait;
use credstore_sdk::{
    CredStoreClientV1, CredStoreError, Credential, CredentialListItem, CredentialPatch,
    CredentialWrite, PutOutcome, PutPrecondition, Secret, SecretRef, SecretValue, Validator,
    WritePrecondition,
};
use parking_lot::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Stub that hands back one canned response per `get_secret` call. `Secret`
/// is not `Clone`, so we `take()` from a `Mutex<Option<_>>`.
#[domain_model]
#[derive(Default)]
struct StubOnce {
    response: StdMutex<Option<Secret>>,
}

#[async_trait]
impl CredStoreClientV1 for StubOnce {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<Credential>, CredStoreError> {
        Ok(None)
    }

    async fn get_secret(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<Secret>, CredStoreError> {
        Ok(self.response.lock().take())
    }

    async fn put(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _write: CredentialWrite,
        _precondition: PutPrecondition,
    ) -> Result<PutOutcome, CredStoreError> {
        Ok(PutOutcome {
            created: true,
            validator: Validator {
                id: Uuid::nil(),
                version: 1,
            },
        })
    }

    async fn patch(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _patch: CredentialPatch,
        _precondition: WritePrecondition,
    ) -> Result<Validator, CredStoreError> {
        Ok(Validator {
            id: Uuid::nil(),
            version: 1,
        })
    }

    async fn delete(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _precondition: WritePrecondition,
    ) -> Result<(), CredStoreError> {
        Ok(())
    }

    async fn list(
        &self,
        _ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<CredentialListItem>, CredStoreError> {
        Ok(Page::empty(query.limit.unwrap_or(0)))
    }
}

/// Generate a self-signed CA PEM at runtime via `rcgen`. The cert is
/// never used over the wire — we only need reqwest's
/// `ClientBuilder::add_root_certificate` to accept the PEM at
/// `.build()` time (parsing is deferred when the `rustls` backend is
/// active).
fn make_test_ca_pem() -> String {
    let cert =
        rcgen::generate_simple_self_signed(vec!["test-ca".to_owned()]).expect("self-signed cert");
    cert.cert.pem()
}

/// Build a default transport (no TLS bundle, system trust store).
fn default_transport() -> Arc<dyn KcTransport> {
    Arc::new(ReqwestKcTransport::new(reqwest::Client::new()))
}

fn factory_for_cfg(cfg: KeycloakConfig) -> KeycloakAdminClientFactory {
    let stub: Arc<dyn CredStoreClientV1> = Arc::new(StubCredStore);
    let reader = CredStoreReader::new(Arc::clone(&stub), build_system_ctx(Uuid::nil()));
    KeycloakAdminClientFactory::new(cfg, default_transport(), reader, Arc::new(NoopMetrics))
}

fn factory_for_uri(base: &str) -> KeycloakAdminClientFactory {
    factory_for_cfg(keycloak_cfg(base))
}

fn factory_for(server: &MockServer) -> KeycloakAdminClientFactory {
    factory_for_uri(&server.uri())
}

#[test]
fn token_response_deserializes_access_token_into_redacting_wrapper() {
    let response: TokenResponse =
        serde_json::from_str(r#"{"access_token":"must-not-debug","expires_in":600}"#)
            .expect("valid token response");

    assert_eq!(response.access_token.expose_secret(), "must-not-debug");
    assert!(!format!("{:?}", response.access_token).contains("must-not-debug"));
}

#[test]
fn new_normalises_base_url_to_trailing_slash() {
    // No trailing slash on the configured base — common operator typo.
    let factory = factory_for_uri("https://kc.example/auth");
    assert!(
        factory.base_url.as_str().ends_with('/'),
        "base_url must end with / after normalization: {}",
        factory.base_url
    );
    // Path prefix preserved.
    assert_eq!(factory.base_url.path(), "/auth/");

    // Already-trailing case is a no-op.
    let factory2 = factory_for_uri("https://kc.example/auth/");
    assert_eq!(factory2.base_url.path(), "/auth/");
}

#[tokio::test]
async fn factory_with_tls_ca_bundle_ref_loads_bundle() {
    // CredStore stub returns a fresh self-signed PEM under any ref id.
    let pem = make_test_ca_pem();
    let stub = Arc::new(StubOnce {
        response: StdMutex::new(Some(Secret {
            reference: SecretRef::new("platform-ca-bundle").expect("valid SecretRef"),
            secret_type: String::new(),
            expires_at: None,
            value: SecretValue::from(pem.as_str()),
            validator: Validator {
                id: Uuid::nil(),
                version: 1,
            },
        })),
    });
    let reader = CredStoreReader::new(stub, build_system_ctx(Uuid::nil()));
    let mut cfg = keycloak_cfg("https://kc.example/auth");
    cfg.tls_ca_bundle_ref = Some("platform-ca-bundle".into());
    // Transport construction is now where TLS bundle loading happens.
    let transport_result = ReqwestKcTransport::from_config(&cfg, &reader).await;
    assert!(
        transport_result.is_ok(),
        "transport must build with a valid PEM CA bundle, got {:?}",
        transport_result.err(),
    );
}

#[tokio::test]
async fn factory_with_missing_tls_ca_bundle_ref_fails_init() {
    // CredStore returns Ok(None) — the ref is absent.
    let stub: Arc<dyn CredStoreClientV1> = Arc::new(StubCredStore);
    let reader = CredStoreReader::new(stub, build_system_ctx(Uuid::nil()));
    let mut cfg = keycloak_cfg("https://kc.example/auth");
    cfg.tls_ca_bundle_ref = Some("platform-ca-bundle".into());
    let result = ReqwestKcTransport::from_config(&cfg, &reader)
        .await
        .map(|_| ());
    let err = result.expect_err("must fail when ref is absent");
    assert!(
        matches!(err, PluginError::CredStoreRead { ref detail } if detail.contains("not found")),
        "expected CredStoreRead 'not found', got {err:?}",
    );
}

#[tokio::test]
async fn factory_with_invalid_tls_ca_bundle_fails_init() {
    // PEM markers with garbage base64 content → reqwest's `build()`
    // path surfaces a builder error after the rustls pemfile iterator
    // rejects the cert encoding. Note: reqwest 0.13 + rustls defers PEM
    // parsing from `Certificate::from_pem` to `ClientBuilder::build`,
    // so the failure surfaces with the `Config { detail: "reqwest
    // client build failed: ..." }` shape (not the `Config { detail:
    // ".. is not a valid PEM .." }` shape that the native-tls backend
    // would produce).
    let bad_pem =
        "-----BEGIN CERTIFICATE-----\nnot-valid-base64-content!!!\n-----END CERTIFICATE-----\n";
    let stub = Arc::new(StubOnce {
        response: StdMutex::new(Some(Secret {
            reference: SecretRef::new("platform-ca-bundle").expect("valid SecretRef"),
            secret_type: String::new(),
            expires_at: None,
            value: SecretValue::from(bad_pem),
            validator: Validator {
                id: Uuid::nil(),
                version: 1,
            },
        })),
    });
    let reader = CredStoreReader::new(stub, build_system_ctx(Uuid::nil()));
    let mut cfg = keycloak_cfg("https://kc.example/auth");
    cfg.tls_ca_bundle_ref = Some("platform-ca-bundle".into());
    let result = ReqwestKcTransport::from_config(&cfg, &reader)
        .await
        .map(|_| ());
    let err = result.expect_err("must reject malformed PEM");
    assert!(
        matches!(err, PluginError::Config { ref detail } if detail.contains("reqwest")),
        "expected Config 'reqwest' error, got {err:?}",
    );
}

#[tokio::test]
async fn bootstrap_client_caches_after_first_acquire() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(move |_req: &wiremock::Request| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "abcdef",
                "expires_in": 600
            }))
        })
        .mount(&server)
        .await;

    let calls_assert = Arc::clone(&calls);
    let factory = factory_for(&server);
    let c1 = factory.bootstrap_client().await.expect("first acquire");
    let c2 = factory
        .bootstrap_client()
        .await
        .expect("second acquire (cached)");

    assert_eq!(c1.access_token(), "abcdef");
    assert_eq!(c2.access_token(), "abcdef");
    assert_eq!(
        calls_assert.load(Ordering::SeqCst),
        1,
        "token endpoint hit exactly once",
    );
}

#[tokio::test]
async fn invalidate_forces_token_refetch() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(move |_req: &wiremock::Request| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok",
                "expires_in": 600
            }))
        })
        .mount(&server)
        .await;

    let calls_assert = Arc::clone(&calls);
    let factory = factory_for(&server);
    factory.bootstrap_client().await.expect("first acquire");
    factory.invalidate("master", "keycloak-idp-plugin-bootstrap");
    factory
        .bootstrap_client()
        .await
        .expect("refetch after invalidate");
    assert_eq!(calls_assert.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn token_endpoint_201_decode_failure_preserves_real_status() {
    // KC misconfig: returns 2xx (here 201, not 200) with a body that
    // doesn't deserialise as TokenResponse. The error should carry the
    // ACTUAL status (201), not a hard-coded 200, so post-mortems see
    // what the server really returned.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(201).set_body_string("not-json"))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let err = factory.bootstrap_client().await.unwrap_err();
    let PluginError::KcRest { status, .. } = &err else {
        unreachable!("decode failure must surface as KcRest; got {err:?}");
    };
    assert!(
        matches!(status, KcStatusKind::Http(201)),
        "decode failure must preserve the real 2xx status; got {status:?}",
    );
}

/// `serde_json` reports parse failures with a byte offset into the FULL
/// response body. The error envelope only carries the first 2KB, so a
/// raw offset past the cap is unactionable — the operator hunts for
/// "column N" in a slice that does not contain column N. Guard the
/// annotation that flags this case.
#[tokio::test]
async fn token_endpoint_2xx_decode_failure_annotates_position_beyond_2kb() {
    let server = MockServer::start().await;
    // Pad an extra (ignored) field so the parse error on
    // `expires_in` lands past the 2KB envelope. `"a" * 3000` keeps the
    // value cheap to construct while pushing serde's reported column
    // well past 2048.
    let padding = "a".repeat(3000);
    let body = format!(r#"{{"padding":"{padding}","expires_in":"not-a-number"}}"#);
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let err = factory.bootstrap_client().await.unwrap_err();
    let PluginError::KcRest { body_first_2kb, .. } = &err else {
        unreachable!("decode failure must surface as KcRest; got {err:?}");
    };
    assert!(
        body_first_2kb.contains("position beyond first 2048B of body"),
        "expected beyond-capture annotation, got: {body_first_2kb}",
    );
}

/// Sanity counterpart to
/// [`token_endpoint_2xx_decode_failure_annotates_position_beyond_2kb`]:
/// a short body that fails to parse should NOT carry the beyond-capture
/// annotation — the position is meaningful as-is.
#[tokio::test]
async fn token_endpoint_2xx_decode_failure_within_2kb_omits_annotation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let err = factory.bootstrap_client().await.unwrap_err();
    let PluginError::KcRest { body_first_2kb, .. } = &err else {
        unreachable!("decode failure must surface as KcRest; got {err:?}");
    };
    assert!(
        !body_first_2kb.contains("position beyond"),
        "short-body parse failure must NOT carry beyond-capture annotation, got: {body_first_2kb}",
    );
}

#[tokio::test]
async fn token_endpoint_short_expires_in_clamps_to_min_ttl() {
    // KC returns `expires_in: 1` against a 30s safety margin: the raw
    // math (`lifetime.saturating_sub(safety)`) lands at zero. Without
    // the clamp every `bootstrap_client()` call would re-fetch in a
    // tight loop. The clamp pins the effective TTL to MIN_TOKEN_TTL,
    // observable here as "two acquires hit the token endpoint exactly
    // once" (cache holds across calls inside the 5s floor).
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(move |_req: &wiremock::Request| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok",
                "expires_in": 1
            }))
        })
        .mount(&server)
        .await;

    let calls_assert = Arc::clone(&calls);
    let factory = factory_for(&server);
    factory.bootstrap_client().await.expect("first acquire");
    factory.bootstrap_client().await.expect("cached acquire");
    assert_eq!(
        calls_assert.load(Ordering::SeqCst),
        1,
        "MIN_TOKEN_TTL clamp must hold the cache across back-to-back acquires",
    );
}

#[tokio::test]
async fn token_endpoint_unbounded_expires_in_cannot_overflow_instant() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(move |_req: &wiremock::Request| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok",
                "expires_in": u64::MAX
            }))
        })
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    factory
        .bootstrap_client()
        .await
        .expect("untrusted expires_in must not panic");
    factory
        .bootstrap_client()
        .await
        .expect("bounded token remains cached");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// KC token endpoint can echo the rejected `client_secret` in the
/// error body (some misconfigured proxies / older KC versions); the
/// factory's `body_first_2kb` MUST run that through `redact_secrets +
/// truncate_2kb` so the secret never leaves the helper as plaintext.
#[tokio::test]
async fn token_endpoint_error_body_redacts_client_secret_leak() {
    let server = MockServer::start().await;
    let leak_body =
        r#"{"error":"invalid_client","error_description":"client_secret=leak-me-not is rejected"}"#;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string(leak_body))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let err = factory.bootstrap_client().await.unwrap_err();
    let PluginError::KcRest { body_first_2kb, .. } = &err else {
        unreachable!("expected KcRest, got {err:?}");
    };
    assert!(
        !body_first_2kb.contains("leak-me-not"),
        "client_secret leaked unredacted into body_first_2kb: {body_first_2kb}"
    );
    assert!(
        body_first_2kb.contains("<redacted>"),
        "expected redaction marker in body_first_2kb: {body_first_2kb}"
    );
}

#[tokio::test]
async fn token_endpoint_5xx_surfaces_as_kcrest_http() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let err = factory.bootstrap_client().await.unwrap_err();
    assert!(
        matches!(
            err,
            PluginError::KcRest {
                status: KcStatusKind::Http(503),
                ..
            }
        ),
        "expected KcRest{{ status: Http(503), .. }}, got {err:?}",
    );
}

#[tokio::test]
async fn health_probe_ok_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/realms/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    factory.health_probe().await.expect("probe must succeed");
}

#[tokio::test]
async fn health_probe_surfaces_503_as_kcrest_http() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/realms/master"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let err = factory.health_probe().await.expect_err("must fail");
    assert!(
        matches!(
            err,
            PluginError::KcRest {
                method: "GET",
                status: KcStatusKind::Http(503),
                ..
            }
        ),
        "expected GET /realms/master 503, got {err:?}",
    );
}

#[tokio::test]
async fn health_probe_surfaces_unreachable_as_transport() {
    // 127.0.0.1:1 is the canonical "almost certainly unbound" port —
    // connection refused immediately, surfacing as `Transport`. We
    // don't drop a real wiremock server because the OS may reuse the
    // released port for a different listener, returning unexpected
    // status codes.
    let factory = factory_for_uri("http://127.0.0.1:1");
    let err = factory.health_probe().await.expect_err("must fail");
    assert!(
        matches!(
            err,
            PluginError::KcRest {
                method: "GET",
                status: KcStatusKind::Transport | KcStatusKind::Timeout,
                ..
            }
        ),
        "expected Transport/Timeout, got {err:?}",
    );
}

#[tokio::test]
async fn reactive_401_triggers_one_retry_after_invalidation() {
    let server = MockServer::start().await;
    let token_calls = Arc::new(AtomicUsize::new(0));
    let token_calls_clone = Arc::clone(&token_calls);

    // Two consecutive token fetches return two distinct tokens.
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(move |_req: &wiremock::Request| {
            let n = token_calls_clone.fetch_add(1, Ordering::SeqCst);
            let access = if n == 0 { "stale" } else { "fresh" };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": access,
                "expires_in": 600
            }))
        })
        .mount(&server)
        .await;

    // The protected endpoint: 401 for the stale bearer, 200 for the fresh one.
    Mock::given(method("GET"))
        .and(path("/admin/realms/master/probe"))
        .and(header("authorization", "Bearer stale"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid_client"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/admin/realms/master/probe"))
        .and(header("authorization", "Bearer fresh"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let probe_url = format!("{}/admin/realms/master/probe", server.uri());
    let result = factory
        .with_reactive_401(
            "master",
            "keycloak-idp-plugin-bootstrap",
            &SecretSource::Inline(SecretString::from("topsecret")),
            |client| {
                let url = probe_url.clone();
                async move {
                    let (status, body) = client
                        .raw_request("GET", &url, None, "/admin/realms/{realm}/probe")
                        .await?;
                    if status != 200 {
                        return Err(PluginError::KcRest {
                            method: "GET",
                            path_template: "/admin/realms/{realm}/probe".into(),
                            status: KcStatusKind::Http(status),
                            body_first_2kb: body,
                        });
                    }
                    Ok(())
                }
            },
        )
        .await;
    result.expect("reactive-401 retry must succeed");
    assert_eq!(
        token_calls.load(Ordering::SeqCst),
        2,
        "token endpoint hit twice (initial + post-invalidation refetch)",
    );
}

#[tokio::test]
async fn reactive_401_propagates_second_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "always-stale",
            "expires_in": 600
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/admin/realms/master/probe"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid_client"))
        .mount(&server)
        .await;

    let factory = factory_for(&server);
    let probe_url = format!("{}/admin/realms/master/probe", server.uri());
    let err = factory
        .with_reactive_401(
            "master",
            "keycloak-idp-plugin-bootstrap",
            &SecretSource::Inline(SecretString::from("topsecret")),
            |client| {
                let url = probe_url.clone();
                async move {
                    let (status, body) = client
                        .raw_request("GET", &url, None, "/admin/realms/{realm}/probe")
                        .await?;
                    if status != 200 {
                        return Err(PluginError::KcRest {
                            method: "GET",
                            path_template: "/admin/realms/{realm}/probe".into(),
                            status: KcStatusKind::Http(status),
                            body_first_2kb: body,
                        });
                    }
                    Ok::<(), PluginError>(())
                }
            },
        )
        .await
        .expect_err("second 401 must propagate");
    assert!(
        matches!(
            err,
            PluginError::KcRest {
                status: KcStatusKind::Http(401),
                ..
            }
        ),
        "expected propagated 401, got {err:?}",
    );
}

#[tokio::test]
async fn concurrent_acquire_on_cold_cache_hits_token_endpoint_once() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    // Add a tiny delay so concurrent callers actually queue on the
    // per-key inflight mutex (otherwise the first caller could finish
    // before the others arrive at the slow path).
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(move |_req: &wiremock::Request| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(50))
                .set_body_json(serde_json::json!({
                    "access_token": "single-flight",
                    "expires_in": 600
                }))
        })
        .mount(&server)
        .await;

    let calls_assert = Arc::clone(&calls);
    let factory = Arc::new(factory_for(&server));
    let mut handles = Vec::with_capacity(10);
    for _ in 0..10 {
        let f = Arc::clone(&factory);
        handles.push(tokio::spawn(async move { f.bootstrap_client().await }));
    }
    for h in handles {
        let client = h.await.expect("task joined").expect("acquire ok");
        assert_eq!(client.access_token(), "single-flight");
    }
    assert_eq!(
        calls_assert.load(Ordering::SeqCst),
        1,
        "single-flight: token endpoint hit exactly once for N concurrent misses",
    );
}

// =====================================================================
// `credential_refresh` / `kc_admin_token_refresh` emission wiring
// (DESIGN "Observability"). Both counters tick on every cache-miss path in
// `acquire_client`: `credential_refresh` after `resolve_secret`
// returns, `kc_admin_token_refresh` after `fetch_token` returns.
// =====================================================================

/// `(outcome, tier, realm)` triple captured by [`RecordingKcMetrics`].
type RecordedRefresh = (TokenRefreshOutcome, TokenTier, String);

/// Recording [`KcAdminMetricsPort`] for assertions on refresh-event
/// shape (outcome + tier + realm). The two counters are stored
/// separately so a test can assert that, say, `credential_refresh`
/// fires twice (initial + post-401 retry) while
/// `kc_admin_token_refresh` fires twice as well — distinguishing
/// `OpenBao` read failures from KC token-endpoint failures requires
/// keeping the two emission sites visible.
#[domain_model]
#[derive(Default)]
struct RecordingKcMetrics {
    credential: StdMutex<Vec<RecordedRefresh>>,
    token: StdMutex<Vec<RecordedRefresh>>,
}

impl KcAdminMetricsPort for RecordingKcMetrics {
    fn kc_admin_request_duration(&self, _endpoint_class: EndpointClass, _secs: f64) {}
    fn kc_admin_token_refresh(&self, outcome: TokenRefreshOutcome, tier: TokenTier, realm: &str) {
        self.token.lock().push((outcome, tier, realm.to_owned()));
    }
    fn credential_refresh(&self, outcome: TokenRefreshOutcome, tier: TokenTier, realm: &str) {
        self.credential
            .lock()
            .push((outcome, tier, realm.to_owned()));
    }
}

fn factory_with_metrics(
    server: &MockServer,
    metrics: Arc<dyn KcAdminMetricsPort>,
) -> KeycloakAdminClientFactory {
    let stub: Arc<dyn CredStoreClientV1> = Arc::new(StubCredStore);
    let reader = CredStoreReader::new(stub, build_system_ctx(Uuid::nil()));
    KeycloakAdminClientFactory::new(
        keycloak_cfg(&server.uri()),
        default_transport(),
        reader,
        metrics,
    )
}

#[tokio::test]
async fn acquire_client_emits_credential_refresh_and_token_refresh_on_cache_miss() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "t",
            "expires_in": 600
        })))
        .mount(&server)
        .await;

    let metrics = Arc::new(RecordingKcMetrics::default());
    let factory =
        factory_with_metrics(&server, Arc::clone(&metrics) as Arc<dyn KcAdminMetricsPort>);

    factory
        .bootstrap_client()
        .await
        .expect("bootstrap acquire must succeed");

    let cred = metrics.credential.lock().clone();
    let tok = metrics.token.lock().clone();
    assert_eq!(
        cred,
        vec![(
            TokenRefreshOutcome::Success,
            TokenTier::StaticEnv,
            "master".to_owned()
        )],
        "credential_refresh must tick once with Success/StaticEnv/master"
    );
    assert_eq!(
        tok,
        vec![(
            TokenRefreshOutcome::Success,
            TokenTier::StaticEnv,
            "master".to_owned()
        )],
        "kc_admin_token_refresh must tick once with Success/StaticEnv/master"
    );
}

#[tokio::test]
async fn acquire_client_does_not_emit_refresh_metrics_on_cache_hit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "t",
            "expires_in": 600
        })))
        .mount(&server)
        .await;

    let metrics = Arc::new(RecordingKcMetrics::default());
    let factory =
        factory_with_metrics(&server, Arc::clone(&metrics) as Arc<dyn KcAdminMetricsPort>);

    // Two acquires: first is cache-miss (emits), second is cache-hit
    // (must NOT emit — the rotation-observability invariant is "every
    // counter tick = a real resolution from source").
    factory
        .bootstrap_client()
        .await
        .expect("first acquire must succeed");
    factory
        .bootstrap_client()
        .await
        .expect("second acquire (cache hit) must succeed");

    let cred_len = metrics.credential.lock().len();
    let tok_len = metrics.token.lock().len();
    assert_eq!(cred_len, 1, "credential_refresh must NOT tick on cache hit");
    assert_eq!(
        tok_len, 1,
        "kc_admin_token_refresh must NOT tick on cache hit"
    );
}

#[tokio::test]
async fn acquire_client_emits_error_outcome_when_token_endpoint_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("kc down"))
        .mount(&server)
        .await;

    let metrics = Arc::new(RecordingKcMetrics::default());
    let factory =
        factory_with_metrics(&server, Arc::clone(&metrics) as Arc<dyn KcAdminMetricsPort>);

    factory
        .bootstrap_client()
        .await
        .expect_err("token endpoint 500 must fail acquire");

    // credential_refresh ticked Success (resolve_secret is infallible
    // for static-inline) BEFORE the token POST failed; the failure
    // surfaces on `kc_admin_token_refresh` with Error/StaticEnv/master.
    let cred = metrics.credential.lock().clone();
    let tok = metrics.token.lock().clone();
    assert_eq!(
        cred,
        vec![(
            TokenRefreshOutcome::Success,
            TokenTier::StaticEnv,
            "master".to_owned()
        )],
    );
    assert_eq!(
        tok,
        vec![(
            TokenRefreshOutcome::Error,
            TokenTier::StaticEnv,
            "master".to_owned()
        )],
    );
}
