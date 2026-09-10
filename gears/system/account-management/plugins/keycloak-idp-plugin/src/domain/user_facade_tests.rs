use super::*;
use crate::config::UserFacadeConfig;
use crate::domain::credstore::CredStoreReader;
use crate::domain::kc::transport::KcTransport;
use crate::domain::system_actor::build_system_ctx;
use crate::domain::test_support::{
    ConfigurableStubCS, StubCredStore, keycloak_cfg, make_metadata, noop_failure_metrics,
    noop_metadata_metrics, noop_user_metrics,
};
use crate::infra::kc_http::ReqwestKcTransport;
use account_management_sdk::idp_user::{
    IdpDeprovisionUserRequest, IdpListUsersRequest, IdpNewUser, IdpProvisionUserRequest,
    IdpTenantContext, IdpUserFilterField, IdpUserPagination,
};
use credstore_sdk::CredStoreClientV1;
use gts::GtsTypeId;
use toolkit_odata::filter::{FilterNode, FilterOp, ODataValue};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn user_facade_cfg() -> UserFacadeConfig {
    UserFacadeConfig {
        default_user_required_actions: vec!["VERIFY_EMAIL".into()],
        list_users_page_limit_default: 50,
        list_users_page_limit_max: 200,
        session_revoke_on_deprovision: true,
    }
}

fn build_facade_with_cs(server: &MockServer, cs: Arc<dyn CredStoreClientV1>) -> UserFacade {
    build_facade_with_cs_and_cfg(server, cs, user_facade_cfg())
}

fn build_facade_with_cs_and_cfg(
    server: &MockServer,
    cs: Arc<dyn CredStoreClientV1>,
    user_cfg: UserFacadeConfig,
) -> UserFacade {
    let kc_cfg = keycloak_cfg(&server.uri());
    let realm_admin_cfg = kc_cfg.realm_admin.clone();

    let cs_reader = CredStoreReader::new(cs, build_system_ctx(Uuid::nil()));
    let transport: Arc<dyn KcTransport> = Arc::new(ReqwestKcTransport::new(reqwest::Client::new()));
    let factory = Arc::new(KeycloakAdminClientFactory::new(
        kc_cfg,
        transport,
        cs_reader,
        Arc::new(crate::domain::test_support::NoopMetrics),
    ));
    UserFacade::new(
        user_cfg,
        realm_admin_cfg,
        factory,
        Arc::new(MetadataCodec),
        noop_user_metrics(),
        noop_metadata_metrics(),
        noop_failure_metrics(),
    )
}

fn build_facade(server: &MockServer) -> UserFacade {
    build_facade_with_cs(server, Arc::new(StubCredStore))
}

fn build_facade_with_required_actions(
    server: &MockServer,
    required_actions: Vec<String>,
) -> UserFacade {
    let cfg = UserFacadeConfig {
        default_user_required_actions: required_actions,
        ..user_facade_cfg()
    };
    build_facade_with_cs_and_cfg(server, Arc::new(StubCredStore), cfg)
}

fn provision_user_req(
    tenant_id: Uuid,
    metadata: Option<serde_json::Value>,
    username: &str,
    email: Option<&str>,
) -> IdpProvisionUserRequest {
    let mut payload = IdpNewUser::new(username);
    if let Some(e) = email {
        payload = payload.with_email(e);
    }
    IdpProvisionUserRequest::new(
        IdpTenantContext::new(
            tenant_id,
            "stub-tenant",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            metadata,
        ),
        payload,
    )
}

/// Mount the realm-admin token endpoint so the factory's
/// `realm_client(...)` call succeeds. Shared/default-realm tests only
/// need this realm's endpoint; Adopted tests can call it again with
/// a different `realm_under_test`.
async fn mount_token_endpoint(server: &MockServer, realm_under_test: &str) {
    Mock::given(method("POST"))
        .and(path(format!(
            "/realms/{realm_under_test}/protocol/openid-connect/token"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "t-realm",
            "expires_in": 600
        })))
        .mount(server)
        .await;
}

// -----------------------------------------------------------------
// Phase 12 — provision_user
// -----------------------------------------------------------------

#[tokio::test]
async fn provision_user_happy_path_default_shared_realm_returns_idp_user() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();

        // Step 4: POST /users → 201 (no body required — we re-discover
        // the UUID via the username probe).
        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        // Step 5: GET /users?username=...&exact=true → [{ id }].
        Mock::given(method("GET"))
            .and(path("/admin/realms/platform/users"))
            .and(query_param("username", "alice"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": user_uuid.to_string()}
            ])))
            .mount(&server)
            .await;

        // Step 6: PUT /users/{user}/groups/{tenant_group} → 204.
        Mock::given(method("PUT"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            tenant_uuid,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            "alice",
            Some("alice@example.com"),
        );

        let user = facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect("provision_user ok");
        assert_eq!(user.id, user_uuid);
        assert_eq!(user.username, "alice");
        assert_eq!(user.email.as_deref(), Some("alice@example.com"));
    })
    .await;
}

/// Regression for the `VERIFY_EMAIL`/`emailVerified` coupling bug.
///
/// When the operator opts users into `VERIFY_EMAIL` via
/// `default_user_required_actions`, the POST /users body MUST send
/// `emailVerified=false` — otherwise KC silently skips the action and the
/// operator-configured gate becomes a no-op. The two fields share a single
/// source of truth in [`UserFacade::create_user`].
#[tokio::test]
async fn provision_user_with_verify_email_sends_email_verified_false() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();

        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/admin/realms/platform/users"))
            .and(query_param("username", "alice"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": user_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let facade = build_facade_with_required_actions(&server, vec!["VERIFY_EMAIL".into()]);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            tenant_uuid,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            "alice",
            Some("alice@example.com"),
        );

        facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect("provision_user ok");

        let received = server.received_requests().await.unwrap();
        let post = received
            .iter()
            .find(|r| r.method.as_str() == "POST" && r.url.path() == "/admin/realms/platform/users")
            .expect("POST /users must be sent");
        let body: serde_json::Value =
            serde_json::from_slice(&post.body).expect("POST body is JSON");
        assert_eq!(
            body.get("emailVerified"),
            Some(&serde_json::Value::Bool(false)),
            "VERIFY_EMAIL in required_actions => emailVerified MUST be false",
        );
        assert_eq!(
            body.get("requiredActions"),
            Some(&serde_json::json!(["VERIFY_EMAIL"])),
            "required_actions must be forwarded verbatim",
        );
    })
    .await;
}

/// Counterpart to [`provision_user_with_verify_email_sends_email_verified_false`].
///
/// When the operator clears `default_user_required_actions` (dev/E2E
/// posture), AM is treated as authoritative for the email and the POST
/// /users body MUST send `emailVerified=true` so the password grant on
/// freshly-provisioned users works without a manual KC admin step.
#[tokio::test]
async fn provision_user_without_required_actions_sends_email_verified_true() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();

        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/admin/realms/platform/users"))
            .and(query_param("username", "alice"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": user_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let facade = build_facade_with_required_actions(&server, vec![]);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            tenant_uuid,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            "alice",
            Some("alice@example.com"),
        );

        facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect("provision_user ok");

        let received = server.received_requests().await.unwrap();
        let post = received
            .iter()
            .find(|r| r.method.as_str() == "POST" && r.url.path() == "/admin/realms/platform/users")
            .expect("POST /users must be sent");
        let body: serde_json::Value =
            serde_json::from_slice(&post.body).expect("POST body is JSON");
        assert_eq!(
            body.get("emailVerified"),
            Some(&serde_json::Value::Bool(true)),
            "empty required_actions => AM authoritative => emailVerified MUST be true",
        );
        assert_eq!(
            body.get("requiredActions"),
            Some(&serde_json::json!([])),
            "required_actions must be forwarded verbatim (empty array)",
        );
    })
    .await;
}

#[tokio::test]
async fn provision_user_in_adopted_realm_uses_openbao_ref() {
    let server = MockServer::start().await;
    mount_token_endpoint(&server, "partner-acme").await;

    let tenant_uuid = Uuid::new_v4();
    let group_uuid = Uuid::new_v4();
    let user_uuid = Uuid::new_v4();

    Mock::given(method("POST"))
        .and(path("/admin/realms/partner-acme/users"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/admin/realms/partner-acme/users"))
        .and(query_param("username", "bob"))
        .and(query_param("exact", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": user_uuid.to_string()}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "/admin/realms/partner-acme/users/{user_uuid}/groups/{group_uuid}"
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    // OpenBao stub returns the per-realm admin secret keyed by the
    // templated SecretRef.
    let cs = Arc::new(ConfigurableStubCS::with_entry(
        "keycloak-idp-realm-admin-partner-acme-secret",
        "per-realm-ra-secret",
    ));
    let facade = build_facade_with_cs(&server, cs);
    let ctx = build_system_ctx(Uuid::nil());
    let req = provision_user_req(
        tenant_uuid,
        Some(make_metadata(
            "partner-acme",
            RealmBinding::Adopted,
            group_uuid,
            Some("keycloak-idp-realm-admin-partner-acme-secret"),
        )),
        "bob",
        None,
    );

    let user = facade
        .provision_user_inner(&ctx, &req)
        .await
        .expect("provision_user ok");
    assert_eq!(user.id, user_uuid);
    assert_eq!(user.username, "bob");
    assert!(user.email.is_none());
}

#[tokio::test]
async fn provision_user_with_duplicate_username_returns_rejected() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        // POST /users → 409 Conflict (duplicate).
        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(
                ResponseTemplate::new(409).set_body_string("User exists with same username"),
            )
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            "alice",
            None,
        );

        let err = facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect_err("duplicate must error");
        // every 409 rides the typed duplicate lane. This mock
        // returns a plain-text (non-JSON) body, so the attribution is the
        // honest unattributed token, not a misread field.
        assert!(
            matches!(
                err,
                PluginError::UserOpDuplicate {
                    field: account_management_sdk::IdpUserDuplicateField::UsernameOrEmail,
                    ref detail,
                } if detail.contains("409")
            ),
            "expected UserOpDuplicate(UsernameOrEmail) with 409 detail, got {err:?}",
        );
    })
    .await;
}

/// Regression for B-H1. When KC echoes the request
/// payload back in a 4xx error body, secret-shaped strings
/// (`password=`, `client_secret=`, etc.) MUST be redacted before
/// landing in `UserOpRejected.detail` — `detail` rides downstream to
/// AM logs and may end up in user-facing error envelopes. The capture
/// path is `redact_secrets(&truncate_2kb(&body_text))`; missing
/// redaction was the round-4 finding.
#[tokio::test]
async fn provision_user_4xx_body_redacts_leaked_password() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(ResponseTemplate::new(409).set_body_string(
                r#"{"error":"Conflict","echo":{"password":"hunter2-leak-me-not"}}"#,
            ))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            "alice",
            None,
        );

        let err = facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect_err("409 must error");
        //  moved 409s to the typed duplicate lane; the B-H1
        // redaction guarantee must hold on its `detail` all the same.
        assert!(
            matches!(
                &err,
                PluginError::UserOpDuplicate { detail, .. }
                    if !detail.contains("hunter2-leak-me-not")
                        && detail.contains("<redacted>")
            ),
            "expected UserOpDuplicate with `hunter2-leak-me-not` masked as `<redacted>` (B-H1 regression), got {err:?}",
        );
    })
    .await;
}

#[tokio::test]
async fn provision_user_with_5xx_returns_unavailable() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        // POST /users → 503 Service Unavailable.
        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            "alice",
            None,
        );

        let err = facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect_err("5xx must error");
        assert!(
            matches!(err, PluginError::UserOpUnavailable { ref detail } if detail.contains("503")),
            "expected UserOpUnavailable{{ detail contains 503 }}, got {err:?}",
        );
    })
    .await;
}

/// `add_user_to_group` returns 404 (tenant group disappeared mid-saga)
/// AFTER `create_user` succeeded → the freshly-created KC user is an
/// orphan. The facade MUST issue a best-effort `delete_user` compensation
/// and propagate the original group-add error.
///
/// Wiremock `.expect(1)` on the DELETE pins the contract: compensation
/// fires exactly once. Without the compensation logic, the test catches
/// a regression that re-introduces dangling KC users.
#[tokio::test]
async fn provision_user_orphan_compensation_calls_delete_on_group_add_failure() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();

        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/admin/realms/platform/users"))
            .and(query_param("username", "alice"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": user_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        // PUT (group bind) → 404: tenant group disappeared.
        Mock::given(method("PUT"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(404).set_body_string("group missing"))
            .mount(&server)
            .await;
        // DELETE (compensation) → 204. `.expect(1)` is the contract:
        // compensation MUST fire exactly once on this code path.
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            tenant_uuid,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            "alice",
            None,
        );

        let err = facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect_err("group-add 404 must error after compensation");
        // Primary error propagated — not masked by the compensation call.
        assert!(
            matches!(err, PluginError::UserOpRejected { ref detail } if detail.contains("404")),
            "expected UserOpRejected{{ detail contains 404 }}, got {err:?}",
        );
        // wiremock's `.expect(1)` is verified on drop — the test passes
        // only if the DELETE fired exactly once.
    })
    .await;
}

/// Compensation `delete_user` itself fails → primary group-add error
/// still propagates without being masked. The "orphan-user
/// compensation FAILED" warn log path runs but doesn't change the
/// caller-visible failure (operators see the primary cause first).
#[tokio::test]
async fn provision_user_orphan_compensation_failure_does_not_mask_primary_error() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();

        Mock::given(method("POST"))
            .and(path("/admin/realms/platform/users"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/admin/realms/platform/users"))
            .and(query_param("username", "alice"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": user_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(404).set_body_string("group missing"))
            .mount(&server)
            .await;
        // DELETE compensation FAILS with 500 — drives the warn-level
        // "compensation FAILED" log path. The primary error must still
        // propagate verbatim. Don't `.expect(N)` — `delete_user` runs
        // through `execute_with_retry` and 5xx triggers the standard
        // retry policy, so the exact call count is policy-dependent;
        // what matters here is the propagated error.
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(500).set_body_string("kc boom"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = provision_user_req(
            tenant_uuid,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            "alice",
            None,
        );

        let err = facade
            .provision_user_inner(&ctx, &req)
            .await
            .expect_err("primary group-add error must propagate even when compensation fails");
        // The primary `UserOpRejected{tenant group disappeared}` from
        // `add_user_to_group`'s 404 path is what surfaces — the 500 on
        // DELETE is logged but not raised, so the caller sees the
        // original cause.
        assert!(
            matches!(err, PluginError::UserOpRejected { ref detail } if detail.contains("404")),
            "compensation failure masked the primary error: {err:?}",
        );
    })
    .await;
}

#[tokio::test]
async fn provision_user_with_malformed_metadata_returns_rejected() {
    let server = MockServer::start().await;
    // No HTTP mocks needed — failure short-circuits before any KC call.
    let facade = build_facade(&server);
    let ctx = build_system_ctx(Uuid::nil());
    let req = provision_user_req(
        Uuid::new_v4(),
        // Garbage metadata — missing required keys.
        Some(serde_json::json!({"version": "v1", "realm_name": "platform"})),
        "alice",
        None,
    );

    let err = facade
        .provision_user_inner(&ctx, &req)
        .await
        .expect_err("malformed metadata must error");
    // The inner method propagates `MetadataDecode` verbatim so the
    // metrics adapter can observe the `version_observed` label;
    // `translate_user_op_failure` (idp_impl.rs) is what converts it to
    // `UserOpRejected{ detail: "metadata decode failed: ..." }` at the
    // outer IDP boundary. Asserting on the variant keeps this test
    // focused on the facade layer's actual contract.
    assert!(
        matches!(err, PluginError::MetadataDecode(_)),
        "expected PluginError::MetadataDecode(_), got {err:?}",
    );
}

// -----------------------------------------------------------------
// Phase 13 — deprovision_user / list_users
// -----------------------------------------------------------------

/// Build a facade with a tweaked [`UserFacadeConfig`] — Phase 13
/// tests flip `session_revoke_on_deprovision` per scenario.
fn build_facade_with_user_cfg(server: &MockServer, user_cfg: UserFacadeConfig) -> UserFacade {
    let kc_cfg = keycloak_cfg(&server.uri());
    let realm_admin_cfg = kc_cfg.realm_admin.clone();

    let cs_reader = CredStoreReader::new(Arc::new(StubCredStore), build_system_ctx(Uuid::nil()));
    let transport: Arc<dyn KcTransport> = Arc::new(ReqwestKcTransport::new(reqwest::Client::new()));
    let factory = Arc::new(KeycloakAdminClientFactory::new(
        kc_cfg,
        transport,
        cs_reader,
        Arc::new(crate::domain::test_support::NoopMetrics),
    ));
    UserFacade::new(
        user_cfg,
        realm_admin_cfg,
        factory,
        Arc::new(MetadataCodec),
        noop_user_metrics(),
        noop_metadata_metrics(),
        noop_failure_metrics(),
    )
}

/// Mount the KC `GET /admin/realms/{realm}/users/{user_uuid}` mock that
/// the binding-check step (`read_user_owner_tenant`) consumes before any
/// destructive call. Returns a minimal user representation with
/// `attributes.tenant_id = [tenant_id]` so the check accepts the binding.
///
/// Tests that need to exercise the binding-MISMATCH branch should NOT call
/// this helper — they should mount their own response (e.g. 404, or a body
/// with a different `tenant_id`) so `read_user_owner_tenant` returns
/// `Ok(None)` / `Ok(Some(other_uuid))` and the saga skips the delete.
async fn mount_user_binding_returns(
    server: &MockServer,
    realm: &str,
    user_uuid: Uuid,
    tenant_id: Uuid,
) {
    Mock::given(method("GET"))
        .and(path(format!("/admin/realms/{realm}/users/{user_uuid}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": user_uuid.to_string(),
            "attributes": {
                "tenant_id": [tenant_id.to_string()],
                "user_type": ["user"],
            },
        })))
        .mount(server)
        .await;
}

fn deprovision_user_req(
    tenant_id: Uuid,
    metadata: Option<serde_json::Value>,
    user_id: Uuid,
) -> IdpDeprovisionUserRequest {
    IdpDeprovisionUserRequest::new(
        IdpTenantContext::new(
            tenant_id,
            "stub-tenant",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            metadata,
        ),
        user_id,
    )
}

fn list_users_req(
    tenant_id: Uuid,
    metadata: Option<serde_json::Value>,
    pagination: IdpUserPagination,
    user_id_filter: Option<Uuid>,
) -> IdpListUsersRequest {
    let mut req = IdpListUsersRequest::new(
        IdpTenantContext::new(
            tenant_id,
            "stub-tenant",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            metadata,
        ),
        pagination,
    );
    if let Some(filter) = user_id_filter {
        req = req.with_filter(FilterNode::binary(
            IdpUserFilterField::Id,
            FilterOp::Eq,
            ODataValue::Uuid(filter),
        ));
    }
    req
}

#[tokio::test]
async fn deprovision_user_happy_path_returns_ok() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_id = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();
        // Binding check: GET /users/{user_uuid} returns the user with the
        // matching tenant_id attribute, so deprovision proceeds.
        mount_user_binding_returns(&server, "platform", user_uuid, tenant_id).await;
        // Session revoke disabled → no /logout mock needed.
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let user_cfg = UserFacadeConfig {
            session_revoke_on_deprovision: false,
            ..user_facade_cfg()
        };
        let facade = build_facade_with_user_cfg(&server, user_cfg);
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_user_req(
            tenant_id,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            user_uuid,
        );

        facade
            .deprovision_user_inner(&ctx, &req)
            .await
            .expect("deprovision_user ok");
    })
    .await;
}

#[tokio::test]
async fn deprovision_user_with_session_revoke_calls_logout_then_delete() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_id = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();
        // Binding check passes (matching tenant_id) → revoke + delete proceed.
        mount_user_binding_returns(&server, "platform", user_uuid, tenant_id).await;
        // session_revoke enabled by default in `user_facade_cfg`.
        Mock::given(method("POST"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/logout"
            )))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_user_req(
            tenant_id,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            user_uuid,
        );

        facade
            .deprovision_user_inner(&ctx, &req)
            .await
            .expect("deprovision_user ok");
        // `.expect(1)` on each mock asserts call ordering implicitly:
        // both endpoints saw exactly one request.
    })
    .await;
}

#[tokio::test]
async fn deprovision_user_vendor_404_returns_ok() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_id = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();
        // Binding check passes — even if a concurrent deleter races us, the
        // user still has its tenant_id attribute at this exact moment.
        // The KC-side delete then 404s because the concurrent deleter
        // already cleared the row — that is the idempotent-absent path
        // this test pins.
        mount_user_binding_returns(&server, "platform", user_uuid, tenant_id).await;
        // Logout returns 404 too (user already gone) — best-effort,
        // continues to DELETE.
        Mock::given(method("POST"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/logout"
            )))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_user_req(
            tenant_id,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            user_uuid,
        );

        facade
            .deprovision_user_inner(&ctx, &req)
            .await
            .expect("404 must be idempotent ok");
    })
    .await;
}

#[tokio::test]
async fn deprovision_user_5xx_returns_unavailable() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_id = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();
        // Binding check passes so the failure surface is unambiguously
        // the DELETE step (not the new pre-check).
        mount_user_binding_returns(&server, "platform", user_uuid, tenant_id).await;
        // Disable session-revoke so the failure surface is unambiguously
        // the DELETE step.
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .mount(&server)
            .await;

        let user_cfg = UserFacadeConfig {
            session_revoke_on_deprovision: false,
            ..user_facade_cfg()
        };
        let facade = build_facade_with_user_cfg(&server, user_cfg);
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_user_req(
            tenant_id,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            user_uuid,
        );

        let err = facade
            .deprovision_user_inner(&ctx, &req)
            .await
            .expect_err("5xx must error");
        assert!(
            matches!(err, PluginError::UserOpUnavailable { ref detail } if detail.contains("503")),
            "expected UserOpUnavailable{{ detail contains 503 }}, got {err:?}",
        );
    })
    .await;
}

/// Regression: cross-tenant deprovision is silently skipped (no destructive
/// KC call) when the stored `tenant_id` attribute on the user does not match
/// the request's `tenant_context.tenant_id`.
///
/// This pins the security fix that closed
/// `tests/e2e/.../test_delete_user_in_wrong_tenant_rejected_for_scoped_user`
/// (e2e xfail strict). Before the fix, AM's User-resource PEP grants
/// `User.delete` on `OWNER_TENANT_ID` derived from the URL path; the IMPL
/// then forwarded `user_id` to KC without verifying the binding, so any
/// caller authorised for `User.delete` on their own tenant could delete a
/// user owned by ANY other tenant by routing through their own `tenant_id`
/// path. Users are KC-stored (`ADR-0005`) so the SQL constraint the `AuthZ`
/// resolver emits never applies — the binding check must be explicit.
///
/// What this test asserts:
///   * The KC `GET /users/{user_uuid}` mock returns a user with a `tenant_id`
///     attribute set to a UUID that DIFFERS from the request's `tenant_id`.
///   * `deprovision_user_inner` returns `Ok(())` — same shape as the genuine
///     idempotent-absent path, so AM surfaces HTTP 204 to the caller
///     (enumeration-resistant; caller cannot distinguish "wrong tenant"
///     from "already deleted").
///   * NO `DELETE /users/{user_uuid}` request is issued — the destructive
///     call is skipped entirely. `.expect(0)` on the DELETE mock fails the
///     test the moment the binding check is bypassed.
///   * NO `POST /users/{user_uuid}/logout` request either — same reason.
#[tokio::test]
async fn deprovision_user_skips_delete_when_binding_mismatches() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let user_uuid = Uuid::new_v4();
        let request_tenant_id = Uuid::new_v4();
        // Crucial: the user's stored tenant_id is DIFFERENT — this is the
        // cross-tenant scenario where alice (in request_tenant_id) is
        // trying to delete bob (owned by user_actual_tenant_id).
        let user_actual_tenant_id = Uuid::new_v4();
        assert_ne!(
            request_tenant_id, user_actual_tenant_id,
            "test setup invariant: tenants must differ"
        );
        mount_user_binding_returns(&server, "platform", user_uuid, user_actual_tenant_id).await;
        // Both destructive calls MUST NOT be issued — `.expect(0)` blows up
        // the test instantly if the binding-check skip ever regresses.
        Mock::given(method("POST"))
            .and(path(format!(
                "/admin/realms/platform/users/{user_uuid}/logout"
            )))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_user_req(
            request_tenant_id,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            user_uuid,
        );

        // Enumeration-resistant: Ok(()) shape — caller cannot distinguish
        // this from a real already-absent deprovision.
        facade
            .deprovision_user_inner(&ctx, &req)
            .await
            .expect("binding mismatch must return Ok (idempotent-absent shape)");
    })
    .await;
}

/// Regression companion: user absent from KC entirely (GET → 404) also
/// short-circuits to the idempotent-absent path, with no DELETE issued.
#[tokio::test]
async fn deprovision_user_skips_delete_when_user_absent_in_kc() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let user_uuid = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        // GET /users/{uuid} → 404 (user not in KC). Binding check collapses
        // to Ok(None) which short-circuits the delete.
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/admin/realms/platform/users/{user_uuid}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_user_req(
            tenant_id,
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
            user_uuid,
        );

        facade
            .deprovision_user_inner(&ctx, &req)
            .await
            .expect("user-absent must return Ok (idempotent-absent)");
    })
    .await;
}

#[tokio::test]
async fn list_users_happy_path_returns_page() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let group_uuid = Uuid::new_v4();
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let u3 = Uuid::new_v4();
        // `top = 2`, 3-member fixture (fits one page) → has_more=true after
        // the drain → next_cursor must be set.
        // Items are sorted by (createdTimestamp ASC, id ASC) client-side.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .and(query_param("first", "0"))
            // Plugin drains the full membership a page at a time, fetching
            // `list_users_page_limit_max` rows per KC call (test cfg sets it
            // to 200). Small fixtures fit in the first page, so a single
            // `first=0` call still serves the request.
            .and(query_param("max", "200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": u1.to_string(), "username": "alice", "email": "alice@example.com", "createdTimestamp": 1_716_400_000_000_i64},
                {"id": u2.to_string(), "username": "bob", "firstName": "Bob", "lastName": "Builder", "createdTimestamp": 1_716_400_001_000_i64},
                {"id": u3.to_string(), "username": "carol", "createdTimestamp": 1_716_400_002_000_i64},
            ])))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = list_users_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            IdpUserPagination::new(2, None).expect("valid pagination"),
            None,
        );

        let page = facade
            .list_users_inner(&ctx, &req)
            .await
            .expect("list_users ok");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, u1);
        assert_eq!(page.items[0].username, "alice");
        assert_eq!(page.items[0].email.as_deref(), Some("alice@example.com"));
        assert_eq!(page.items[1].id, u2);
        assert_eq!(page.items[1].display_name.as_deref(), Some("Bob Builder"));
        assert_eq!(page.page_info.limit, 2);
        assert!(
            page.page_info.next_cursor.is_some(),
            "has_more=true must produce a next_cursor",
        );
    })
    .await;
}

#[tokio::test]
async fn list_users_with_cursor_continues_from_tiebreaker() {
    // DESIGN "Interactions & Sequences": both page requests hit KC with first=0; cursor carries
    // (last_created_at, last_user_id) and client-side filtering skips the
    // already-returned users.
    //
    // Both fetches are identical (first=0&max=3), so we mount a single
    // permissive mock that returns all 3 users on every call, and rely on
    // client-side skip to produce the correct second page.
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        // Use fixed UUIDs so we can assert on the second page items.
        let u1 = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();
        let u2 = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000002").unwrap();
        let u3 = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000003").unwrap();

        // Single mock for both pages: KC always returns all 3 users.
        // sorted order: u1(ts=1000) < u2(ts=2000) < u3(ts=3000).
        // Page 1: limit=2, no cursor → emit u1,u2; cursor encodes u2.
        // Page 2: limit=2, cursor=(ts=2000,u2) → skip u1,u2 → emit u3 only.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .and(query_param("first", "0"))
            // Plugin drains the full membership a page at a time, fetching
            // `list_users_page_limit_max` rows per KC call (test cfg sets it
            // to 200). Small fixtures fit in the first page, so a single
            // `first=0` call still serves the request.
            .and(query_param("max", "200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": u1.to_string(), "username": "a", "createdTimestamp": 1000_i64},
                {"id": u2.to_string(), "username": "b", "createdTimestamp": 2000_i64},
                {"id": u3.to_string(), "username": "c", "createdTimestamp": 3000_i64},
            ])))
            .expect(2) // page 1 + page 2 each call the same endpoint
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let metadata = Some(make_metadata(
            "platform",
            RealmBinding::Shared,
            group_uuid,
            None,
        ));

        // Page 1.
        let req1 = list_users_req(
            tenant_uuid,
            metadata.clone(),
            IdpUserPagination::new(2, None).expect("valid pagination"),
            None,
        );
        let page1 = facade
            .list_users_inner(&ctx, &req1)
            .await
            .expect("page 1 ok");
        assert_eq!(page1.items.len(), 2);
        assert_eq!(page1.items[0].id, u1);
        assert_eq!(page1.items[1].id, u2);
        let cursor = page1
            .page_info
            .next_cursor
            .clone()
            .expect("page 1 produced cursor");

        // Verify cursor encodes u2's tiebreaker.
        let decoded = UserFacade::decode_cursor(&cursor).expect("cursor decodes");
        assert_eq!(decoded.last_created_at, 2000_i64);
        assert_eq!(decoded.last_user_id, u2);

        // Page 2 — re-issue with cursor; client-side skip removes u1,u2.
        let req2 = list_users_req(
            tenant_uuid,
            metadata,
            IdpUserPagination::new(2, Some(cursor)).expect("valid pagination"),
            None,
        );
        let page2 = facade
            .list_users_inner(&ctx, &req2)
            .await
            .expect("page 2 ok");
        assert_eq!(page2.items.len(), 1);
        assert_eq!(page2.items[0].id, u3);
        assert!(
            page2.page_info.next_cursor.is_none(),
            "last page must not produce a next_cursor",
        );
    })
    .await;
}

#[tokio::test]
async fn list_users_cursor_filter_mismatch_returns_rejected() {
    let server = MockServer::start().await;
    // No KC mocks needed — failure short-circuits before fetching members.

    // Encode a cursor whose `filter_hash` was computed against
    // `user_id_filter = Some(filter_a)`; the request will then ask
    // with `user_id_filter = Some(filter_b)`.
    let tenant_uuid = Uuid::new_v4();
    let filter_a = Uuid::new_v4();
    let filter_b = Uuid::new_v4();
    let group_uuid = Uuid::new_v4();
    let stale_cursor = UserFacade::encode_cursor(&ListUsersCursor {
        filter_hash: UserFacade::filter_hash(
            tenant_uuid,
            "platform",
            Some(filter_a),
            &StringMatcher::Always,
        ),
        last_created_at: 1_716_400_001_000_i64,
        last_user_id: filter_a,
    });

    let facade = build_facade(&server);
    let ctx = build_system_ctx(Uuid::nil());
    let req = list_users_req(
        tenant_uuid,
        Some(make_metadata(
            "platform",
            RealmBinding::Shared,
            group_uuid,
            None,
        )),
        IdpUserPagination::new(2, Some(stale_cursor)).expect("valid pagination"),
        Some(filter_b),
    );

    let err = facade
        .list_users_inner(&ctx, &req)
        .await
        .expect_err("filter mismatch must error");
    assert!(
        matches!(err, PluginError::UserOpRejected { ref detail } if detail == "invalid cursor"),
        "expected UserOpRejected{{ detail == 'invalid cursor' }}, got {err:?}",
    );
}

#[tokio::test]
async fn list_users_5xx_returns_unavailable() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let group_uuid = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = list_users_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            IdpUserPagination::default(),
            None,
        );

        let err = facade
            .list_users_inner(&ctx, &req)
            .await
            .expect_err("5xx must error");
        assert!(
            matches!(err, PluginError::UserOpUnavailable { ref detail } if detail.contains("503")),
            "expected UserOpUnavailable{{ detail contains 503 }}, got {err:?}",
        );
    })
    .await;
}

// -----------------------------------------------------------------
// Tasks 8+9 — cursor codec round-trips and list_users tiebreaker
// -----------------------------------------------------------------

// `cursor_round_trip_first_page_has_null_tiebreaker` (pre-CursorV1)
// was retired with the CursorV1 migration: the new envelope always
// carries a concrete `(last_created_at, last_user_id)` pair (the
// plugin only emits a cursor when `has_more=true`, which means at
// least one row was emitted and its tiebreaker is known). The
// "first page" case is now "no cursor at all" — covered implicitly
// by every test that calls `list_users_inner` without a cursor.

#[test]
fn cursor_round_trip_with_tiebreaker() {
    use uuid::uuid;
    let c = ListUsersCursor {
        filter_hash: "abc123".into(),
        last_created_at: 1_716_400_000_000_i64,
        last_user_id: uuid!("9c4a6b2e-0000-0000-0000-000000000001"),
    };
    let encoded = UserFacade::encode_cursor(&c);
    let decoded = UserFacade::decode_cursor(&encoded).expect("decodes");
    assert_eq!(decoded.last_created_at, 1_716_400_000_000_i64);
    assert_eq!(
        decoded.last_user_id,
        uuid!("9c4a6b2e-0000-0000-0000-000000000001"),
    );
    assert_eq!(decoded.filter_hash, "abc123");
}

/// `CursorV1` envelope is wire-compatible with `toolkit_odata::CursorV1`:
/// the AM handler `lower_odata_to_list_users_query` MUST be able to
/// parse our cursor through the same `CursorV1::decode` path it uses
/// for tenant `list_children`. Verify the plugin's emitted cursor
/// decodes cleanly as `CursorV1` and carries the documented
/// `(o, s, d)` triple plus our `(k[0], k[1])` tiebreaker.
#[test]
fn cursor_wire_shape_is_cursor_v1_compatible() {
    use toolkit_odata::{CursorV1, SortDir};
    use uuid::uuid;
    let c = ListUsersCursor {
        filter_hash: "abc123".into(),
        last_created_at: 1_716_400_000_000_i64,
        last_user_id: uuid!("9c4a6b2e-0000-0000-0000-000000000001"),
    };
    let encoded = UserFacade::encode_cursor(&c);
    let parsed = CursorV1::decode(&encoded).expect("encoded cursor MUST be a valid CursorV1");
    assert_eq!(parsed.o, SortDir::Asc);
    assert_eq!(parsed.s, "+created_at,+id");
    assert_eq!(parsed.d, "fwd");
    assert_eq!(parsed.f.as_deref(), Some("abc123"));
    assert_eq!(parsed.k.len(), 2);
    assert_eq!(parsed.k[0], "1716400000000");
    assert_eq!(parsed.k[1], "9c4a6b2e-0000-0000-0000-000000000001");
}

/// Cursors emitted under one `(o, s)` order-pin MUST be rejected
/// when sent back with a different order shape. Forging a `CursorV1`
/// with the wrong `s` (e.g. another endpoint's cursor accidentally
/// reused) surfaces as `UserOpRejected("invalid cursor")`.
#[test]
fn cursor_decode_rejects_mismatched_order_pin() {
    use toolkit_odata::{CursorV1, SortDir};
    let forged = CursorV1 {
        k: vec!["1716400000000".into(), Uuid::nil().to_string()],
        o: SortDir::Desc,      // mismatched
        s: "+username".into(), // mismatched
        f: Some("abc123".into()),
        d: "fwd".into(),
    }
    .encode()
    .expect("forged cursor encodes");
    let err = UserFacade::decode_cursor(&forged).expect_err("mismatched order rejected");
    assert!(matches!(err, PluginError::UserOpRejected { .. }));
}

/// `CursorV1` with absent `f` (no filter context) is structurally
/// invalid for our envelope — the plugin always sets a filter hash
/// (even for "no filter" requests, it hashes the `tenant_id` +
/// `realm_name` + empty filter set). Reject so a hand-rolled or
/// foreign cursor doesn't silently bypass filter-pinning.
#[test]
fn cursor_decode_rejects_missing_filter_hash() {
    use toolkit_odata::{CursorV1, SortDir};
    let forged = CursorV1 {
        k: vec!["1716400000000".into(), Uuid::nil().to_string()],
        o: SortDir::Asc,
        s: "+created_at,+id".into(),
        f: None, // missing
        d: "fwd".into(),
    }
    .encode()
    .expect("forged cursor encodes");
    let err = UserFacade::decode_cursor(&forged).expect_err("missing f rejected");
    assert!(matches!(err, PluginError::UserOpRejected { .. }));
}

#[tokio::test]
async fn list_users_emits_cursor_when_more_results_exist() {
    use uuid::uuid;
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let group_uuid = Uuid::new_v4();
        // limit=2, 3-member fixture (fits one page) → has_more=true.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .and(query_param("first", "0"))
            // Plugin drains the full membership a page at a time, fetching
            // `list_users_page_limit_max` rows per KC call (test cfg sets it
            // to 200). Small fixtures fit in the first page, so a single
            // `first=0` call still serves the request.
            .and(query_param("max", "200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "9c4a6b2e-0000-0000-0000-000000000001", "username": "u1", "createdTimestamp": 1_716_400_000_000_i64},
                {"id": "9c4a6b2e-0000-0000-0000-000000000002", "username": "u2", "createdTimestamp": 1_716_400_001_000_i64},
                {"id": "9c4a6b2e-0000-0000-0000-000000000003", "username": "u3", "createdTimestamp": 1_716_400_002_000_i64},
            ])))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = list_users_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            IdpUserPagination::new(2, None).expect("valid pagination"),
            None,
        );

        let page = facade.list_users_inner(&ctx, &req).await.expect("ok");
        assert_eq!(page.items.len(), 2, "must emit exactly limit=2 items");
        assert!(
            page.page_info.next_cursor.is_some(),
            "next_cursor must be set when more results exist",
        );
        let next =
            UserFacade::decode_cursor(page.page_info.next_cursor.as_ref().unwrap())
                .expect("decodes");
        assert_eq!(next.last_created_at, 1_716_400_001_000_i64);
        assert_eq!(
            next.last_user_id,
            uuid!("9c4a6b2e-0000-0000-0000-000000000002"),
        );
    })
    .await;
}

#[tokio::test]
async fn list_users_no_cursor_on_last_page() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let group_uuid = Uuid::new_v4();
        // limit=10, 2-member fixture (one short page) → has_more=false.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "9c4a6b2e-0000-0000-0000-000000000001", "username": "u1", "createdTimestamp": 1_716_400_000_000_i64},
                {"id": "9c4a6b2e-0000-0000-0000-000000000002", "username": "u2", "createdTimestamp": 1_716_400_001_000_i64},
            ])))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = list_users_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            IdpUserPagination::new(10, None).expect("valid pagination"),
            None,
        );

        let page = facade.list_users_inner(&ctx, &req).await.expect("ok");
        assert_eq!(page.items.len(), 2);
        assert!(
            page.page_info.next_cursor.is_none(),
            "next_cursor must be None on last page",
        );
    })
    .await;
}

#[tokio::test]
async fn list_users_drains_members_past_first_page() {
    // regression: a member KC orders past the first members page
    // must still be reachable via pagination. With
    // `list_users_page_limit_max = 2` the facade pages the group in windows
    // of 2; the third member lives in the SECOND KC window (first=2). The
    // pre-fix single-window fetch (first=0 only) dropped it from every page
    // — create-then-list returned 201 yet the user never appeared.
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let u1 = Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000001").unwrap();
        let u2 = Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000002").unwrap();
        let u3 = Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000003").unwrap();

        // First window (first=0, full page of 2) → drain continues.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .and(query_param("first", "0"))
            .and(query_param("max", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": u1.to_string(), "username": "u1", "createdTimestamp": 1000_i64},
                {"id": u2.to_string(), "username": "u2", "createdTimestamp": 2000_i64},
            ])))
            .mount(&server)
            .await;
        // Second window (first=2, short page of 1) → drain stops here.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .and(query_param("first", "2"))
            .and(query_param("max", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": u3.to_string(), "username": "u3", "createdTimestamp": 3000_i64},
            ])))
            .mount(&server)
            .await;

        let user_cfg = UserFacadeConfig {
            list_users_page_limit_max: 2,
            ..user_facade_cfg()
        };
        let facade = build_facade_with_user_cfg(&server, user_cfg);
        let ctx = build_system_ctx(Uuid::nil());
        let metadata = Some(make_metadata(
            "platform",
            RealmBinding::Shared,
            group_uuid,
            None,
        ));

        // Page 1: limit clamped to 2 → [u1, u2], cursor at u2, has_more.
        let req1 = list_users_req(
            tenant_uuid,
            metadata.clone(),
            IdpUserPagination::new(10, None).expect("valid pagination"),
            None,
        );
        let page1 = facade
            .list_users_inner(&ctx, &req1)
            .await
            .expect("page 1 ok");
        assert_eq!(
            page1.items.iter().map(|u| u.id).collect::<Vec<_>>(),
            vec![u1, u2],
        );
        let cursor = page1
            .page_info
            .next_cursor
            .clone()
            .expect("page 1 produced a cursor");

        // Page 2: the second-window member MUST appear (the bug: it never did).
        let req2 = list_users_req(
            tenant_uuid,
            metadata,
            IdpUserPagination::new(10, Some(cursor)).expect("valid pagination"),
            None,
        );
        let page2 = facade
            .list_users_inner(&ctx, &req2)
            .await
            .expect("page 2 ok");
        assert_eq!(
            page2.items.iter().map(|u| u.id).collect::<Vec<_>>(),
            vec![u3],
            "member in the second KC window must be reachable via pagination",
        );
        assert!(
            page2.page_info.next_cursor.is_none(),
            "u3 is the last row - no further cursor",
        );
    })
    .await;
}

#[tokio::test]
async fn list_users_filter_finds_member_past_first_page() {
    // regression: `$filter=username eq '<name>'` must find a user
    // even when KC orders it past the first members page. The filter is
    // applied client-side over the DRAINED set, so the drain must pull the
    // second window first; the pre-fix single-window fetch returned an
    // empty result for a user that plainly exists in the group.
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;

        let group_uuid = Uuid::new_v4();
        let target = Uuid::parse_str("cccccccc-0000-0000-0000-000000000003").unwrap();

        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .and(query_param("first", "0"))
            .and(query_param("max", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "cccccccc-0000-0000-0000-000000000001", "username": "alice", "createdTimestamp": 1000_i64},
                {"id": "cccccccc-0000-0000-0000-000000000002", "username": "bob", "createdTimestamp": 2000_i64},
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group_uuid}/members"
            )))
            .and(query_param("first", "2"))
            .and(query_param("max", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": target.to_string(), "username": "rty.fgh", "createdTimestamp": 3000_i64},
            ])))
            .mount(&server)
            .await;

        let user_cfg = UserFacadeConfig {
            list_users_page_limit_max: 2,
            ..user_facade_cfg()
        };
        let facade = build_facade_with_user_cfg(&server, user_cfg);
        let ctx = build_system_ctx(Uuid::nil());
        let req = list_users_req(
            Uuid::new_v4(),
            Some(make_metadata(
                "platform",
                RealmBinding::Shared,
                group_uuid,
                None,
            )),
            IdpUserPagination::new(10, None).expect("valid pagination"),
            None,
        )
        .with_filter(FilterNode::binary(
            IdpUserFilterField::Username,
            FilterOp::Eq,
            ODataValue::String("rty.fgh".into()),
        ));

        let page = facade
            .list_users_inner(&ctx, &req)
            .await
            .expect("list ok");
        assert_eq!(
            page.items.iter().map(|u| u.id).collect::<Vec<_>>(),
            vec![target],
            "username filter must find a member living past the first KC window",
        );
    })
    .await;
}

// =============================================================================
// build_string_matcher — $filter lowering tests
//
// Pins the FilterNode → StringMatcher lowering: the supported operator
// subset (`eq` / `contains` / `and` / `or` on string fields; `id eq`
// folded to `Always`) plus the UnsupportedOperation (501) surface for
// shapes the /users contract does not support. Helper-level tests (no
// wiremock) — symmetric with the `extract_id_filter` tests.
// =============================================================================

fn lit_string(v: &str) -> ODataValue {
    ODataValue::String(v.into())
}

#[test]
fn build_string_matcher_none_is_always() {
    let got = UserFacade::build_string_matcher(None).expect("no filter is valid");
    assert_eq!(got, StringMatcher::Always);
}

#[test]
fn build_string_matcher_id_only_is_always() {
    // `id eq <uuid>` is captured by `extract_id_filter`; at the top
    // level it lowers to `Always` so the string predicate is a no-op.
    let node = FilterNode::binary(
        IdpUserFilterField::Id,
        FilterOp::Eq,
        ODataValue::Uuid(Uuid::new_v4()),
    );
    let got = UserFacade::build_string_matcher(Some(&node))
        .expect("id-eq alone is supported via extract_id_filter");
    assert_eq!(got, StringMatcher::Always);
}

#[test]
fn build_string_matcher_single_eq_on_username() {
    let node = FilterNode::binary(
        IdpUserFilterField::Username,
        FilterOp::Eq,
        lit_string("alice"),
    );
    let got = UserFacade::build_string_matcher(Some(&node)).expect("supported");
    assert_eq!(
        got,
        StringMatcher::Field {
            field: IdpUserFilterField::Username,
            op: StringMatchOp::Eq,
            needle: "alice".into(),
        }
    );
}

#[test]
fn build_string_matcher_contains_on_username() {
    // `contains` is the user-search operator — previously 501.
    let node = FilterNode::binary(
        IdpUserFilterField::Username,
        FilterOp::Contains,
        lit_string("ali"),
    );
    let got = UserFacade::build_string_matcher(Some(&node)).expect("contains is supported");
    assert_eq!(
        got,
        StringMatcher::Field {
            field: IdpUserFilterField::Username,
            op: StringMatchOp::Contains,
            needle: "ali".into(),
        }
    );
}

#[test]
#[allow(clippy::panic)] // test-only assertion: panic on unsupported field reveals codegen drift
fn build_string_matcher_each_supported_string_field() {
    for field in [
        IdpUserFilterField::Username,
        IdpUserFilterField::Email,
        IdpUserFilterField::FirstName,
        IdpUserFilterField::LastName,
        IdpUserFilterField::DisplayName,
    ] {
        let node = FilterNode::binary(field, FilterOp::Eq, lit_string("v"));
        let got = UserFacade::build_string_matcher(Some(&node))
            .unwrap_or_else(|e| panic!("field {field:?} should be supported, got {e:?}"));
        assert_eq!(
            got,
            StringMatcher::Field {
                field,
                op: StringMatchOp::Eq,
                needle: "v".into(),
            },
            "field {field:?} lowered to the wrong clause"
        );
    }
}

#[test]
fn build_string_matcher_and_is_conjunction() {
    let node = FilterNode::and(vec![
        FilterNode::binary(
            IdpUserFilterField::Username,
            FilterOp::Eq,
            lit_string("alice"),
        ),
        FilterNode::binary(IdpUserFilterField::Email, FilterOp::Eq, lit_string("a@b")),
    ]);
    let got = UserFacade::build_string_matcher(Some(&node)).expect("supported");
    assert_eq!(
        got,
        StringMatcher::All(vec![
            StringMatcher::Field {
                field: IdpUserFilterField::Username,
                op: StringMatchOp::Eq,
                needle: "alice".into(),
            },
            StringMatcher::Field {
                field: IdpUserFilterField::Email,
                op: StringMatchOp::Eq,
                needle: "a@b".into(),
            },
        ])
    );
}

#[test]
fn build_string_matcher_or_of_contains_is_disjunction() {
    // The exact shape the Users-list search screen sends:
    // `contains(username,'a') or contains(email,'a') or contains(display_name,'a')`.
    let node = FilterNode::or(vec![
        FilterNode::binary(
            IdpUserFilterField::Username,
            FilterOp::Contains,
            lit_string("a"),
        ),
        FilterNode::binary(
            IdpUserFilterField::Email,
            FilterOp::Contains,
            lit_string("a"),
        ),
        FilterNode::binary(
            IdpUserFilterField::DisplayName,
            FilterOp::Contains,
            lit_string("a"),
        ),
    ]);
    let got = UserFacade::build_string_matcher(Some(&node)).expect("or of contains is supported");
    assert_eq!(
        got,
        StringMatcher::Any(vec![
            StringMatcher::Field {
                field: IdpUserFilterField::Username,
                op: StringMatchOp::Contains,
                needle: "a".into(),
            },
            StringMatcher::Field {
                field: IdpUserFilterField::Email,
                op: StringMatchOp::Contains,
                needle: "a".into(),
            },
            StringMatcher::Field {
                field: IdpUserFilterField::DisplayName,
                op: StringMatchOp::Contains,
                needle: "a".into(),
            },
        ])
    );
}

#[test]
fn build_string_matcher_id_combined_with_username_under_and() {
    // `id eq <uuid> and username eq 'alice'` → All[Always, Field(username)].
    let node = FilterNode::and(vec![
        FilterNode::binary(
            IdpUserFilterField::Id,
            FilterOp::Eq,
            ODataValue::Uuid(Uuid::new_v4()),
        ),
        FilterNode::binary(
            IdpUserFilterField::Username,
            FilterOp::Eq,
            lit_string("alice"),
        ),
    ]);
    let got = UserFacade::build_string_matcher(Some(&node)).expect("supported combo");
    assert_eq!(
        got,
        StringMatcher::All(vec![
            StringMatcher::Always,
            StringMatcher::Field {
                field: IdpUserFilterField::Username,
                op: StringMatchOp::Eq,
                needle: "alice".into(),
            },
        ])
    );
}

#[test]
fn build_string_matcher_rejects_id_eq_inside_or() {
    // `id` cannot be hoisted out of a disjunction → explicit 501.
    let node = FilterNode::or(vec![
        FilterNode::binary(
            IdpUserFilterField::Id,
            FilterOp::Eq,
            ODataValue::Uuid(Uuid::new_v4()),
        ),
        FilterNode::binary(
            IdpUserFilterField::Username,
            FilterOp::Eq,
            lit_string("alice"),
        ),
    ]);
    let err = UserFacade::build_string_matcher(Some(&node)).expect_err("id-in-or unsupported");
    assert!(matches!(err, PluginError::UserOpUnsupported { .. }));
}

#[test]
fn build_string_matcher_rejects_ne() {
    let node = FilterNode::binary(
        IdpUserFilterField::Username,
        FilterOp::Ne,
        lit_string("alice"),
    );
    let err = UserFacade::build_string_matcher(Some(&node)).expect_err("ne is unsupported");
    assert!(matches!(err, PluginError::UserOpUnsupported { .. }));
    assert!(
        err.to_string().contains("ne"),
        "detail should name op: {err}"
    );
}

#[test]
#[allow(clippy::panic)] // test-only assertion: panic on unexpected Ok signals contract drift
fn build_string_matcher_rejects_startswith_and_endswith() {
    // `contains` is now supported; the other substring ops are not.
    for op in [FilterOp::StartsWith, FilterOp::EndsWith] {
        let node = FilterNode::binary(IdpUserFilterField::Username, op, lit_string("a"));
        let Err(err) = UserFacade::build_string_matcher(Some(&node)) else {
            panic!("op {op:?} must be UnsupportedOperation")
        };
        assert!(matches!(err, PluginError::UserOpUnsupported { .. }));
    }
}

#[test]
fn build_string_matcher_rejects_in_list() {
    let node = FilterNode::InList {
        field: IdpUserFilterField::Username,
        values: vec![lit_string("alice"), lit_string("bob")],
    };
    let err = UserFacade::build_string_matcher(Some(&node)).expect_err("in unsupported");
    assert!(matches!(err, PluginError::UserOpUnsupported { .. }));
}

#[test]
fn id_set_matcher_is_exact_and_hashes_the_normalized_set() {
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let node = FilterNode::InList {
        field: IdpUserFilterField::Id,
        values: vec![
            ODataValue::Uuid(second),
            ODataValue::Uuid(first),
            ODataValue::Uuid(second),
        ],
    };
    let matcher = UserFacade::build_string_matcher(Some(&node)).expect("UUID set supported");
    assert_eq!(
        matcher,
        StringMatcher::IdSet([first, second].into_iter().collect())
    );
    let user: UserRep =
        serde_json::from_value(serde_json::json!({"id": first, "username": "alice"}))
            .expect("user");
    assert!(matcher.matches(&user));
    let unrelated: UserRep =
        serde_json::from_value(serde_json::json!({"id": Uuid::from_u128(3), "username": "alice"}))
            .expect("user");
    assert!(!matcher.matches(&unrelated));
    let reordered = StringMatcher::IdSet([second, first].into_iter().collect());
    let hash = UserFacade::filter_hash(Uuid::nil(), "platform", None, &matcher);
    assert_eq!(
        hash,
        UserFacade::filter_hash(Uuid::nil(), "platform", None, &reordered)
    );
    assert_ne!(
        hash,
        UserFacade::filter_hash(
            Uuid::nil(),
            "platform",
            None,
            &StringMatcher::IdSet([first].into_iter().collect())
        )
    );
    assert!(!StringMatcher::IdSet(std::collections::BTreeSet::new()).matches(&user));
    let and = FilterNode::and(vec![
        node.clone(),
        FilterNode::binary(
            IdpUserFilterField::Username,
            FilterOp::Eq,
            lit_string("bob"),
        ),
    ]);
    assert!(
        !UserFacade::build_string_matcher(Some(&and))
            .expect("AND")
            .matches(&user)
    );
    let or = FilterNode::or(vec![
        node,
        FilterNode::binary(
            IdpUserFilterField::Username,
            FilterOp::Eq,
            lit_string("bob"),
        ),
    ]);
    assert!(
        UserFacade::build_string_matcher(Some(&or))
            .expect("OR")
            .matches(&user)
    );
}

#[test]
fn id_set_matcher_refuses_non_uuid_values_without_widening_the_filter() {
    let node = FilterNode::InList {
        field: IdpUserFilterField::Id,
        values: vec![
            ODataValue::Uuid(Uuid::from_u128(1)),
            lit_string("not-a-uuid"),
        ],
    };
    assert!(matches!(
        UserFacade::build_string_matcher(Some(&node)),
        Err(PluginError::UserOpUnsupported { .. })
    ));
}

#[tokio::test]
async fn batch_user_ids_share_one_membership_read_and_exclude_unrequested_users() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;
        let group = Uuid::new_v4();
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let absent = Uuid::from_u128(4);
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/platform/groups/{group}/members")))
            .and(query_param("first", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": first, "username": "alice", "createdTimestamp": 1000},
                {"id": second, "username": "bob", "firstName": "Bob", "lastName": "Builder", "createdTimestamp": 2000},
                {"id": Uuid::from_u128(3), "username": "unrequested", "createdTimestamp": 3000}
            ])))
            .expect(1)
            .mount(&server).await;
        let query = account_management_sdk::ListUsersQuery::with_ids([second, first, absent]).expect("batch");
        let req = list_users_req(Uuid::new_v4(), Some(make_metadata("platform", RealmBinding::Shared, group, None)), query.pagination, None)
            .with_filter(query.filter.expect("ID set"));
        let page = build_facade(&server).list_users_inner(&build_system_ctx(Uuid::nil()), &req).await.expect("batch lookup");
        assert_eq!(page.items.iter().map(|user| user.id).collect::<Vec<_>>(), [first, second]);
        assert_eq!(page.items[1].display_name.as_deref(), Some("Bob Builder"));
        assert!(page.page_info.next_cursor.is_none());
        server.verify().await;
    }).await;
}

#[tokio::test]
async fn membership_safety_cap_returns_unavailable_not_false_absence() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;
        let group = Uuid::new_v4();
        let batch: Vec<_> = (1..=200).map(|id| serde_json::json!({"id": Uuid::from_u128(id), "username": "member", "createdTimestamp": 1000})).collect();
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/platform/groups/{group}/members")))
            .respond_with(ResponseTemplate::new(200).set_body_json(batch))
            .expect(u64::try_from(MEMBERS_DRAIN_HARD_CAP.div_ceil(200)).expect("request count"))
            .mount(&server).await;
        let query = account_management_sdk::ListUsersQuery::with_ids([Uuid::from_u128(201)]).expect("batch");
        let req = list_users_req(Uuid::new_v4(), Some(make_metadata("platform", RealmBinding::Shared, group, None)), query.pagination, None)
            .with_filter(query.filter.expect("ID set"));
        let result = build_facade(&server).list_users_inner(&build_system_ctx(Uuid::nil()), &req).await;
        assert!(matches!(result, Err(PluginError::UserOpUnavailable { .. })));
        server.verify().await;
    }).await;
}

/// ID-set cursors resume the same normalized set and refuse a changed set
/// before any provider call, even when both sets have the same cardinality.
#[tokio::test]
async fn batch_user_ids_paginate_and_pin_the_normalized_filter() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_REALM_ADMIN_SECRET", Some("ra"))], async move {
        mount_token_endpoint(&server, "platform").await;
        let tenant = Uuid::new_v4();
        let group = Uuid::new_v4();
        let ids = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/platform/groups/{group}/members"
            )))
            .and(query_param("first", "0"))
            .and(query_param("max", "200"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": ids[2], "username": "unrequested", "createdTimestamp": 500},
                {"id": ids[0], "username": "alice", "createdTimestamp": 1000},
                {"id": ids[1], "username": "bob", "createdTimestamp": 1000}
            ])))
            .expect(2)
            .mount(&server)
            .await;
        let facade = build_facade(&server);
        let ctx = build_system_ctx(tenant);
        let metadata = Some(make_metadata("platform", RealmBinding::Shared, group, None));
        let query =
            account_management_sdk::ListUsersQuery::with_ids([ids[0], ids[1]]).expect("batch");
        let first_request = list_users_req(
            tenant,
            metadata.clone(),
            IdpUserPagination::new(1, None).expect("page"),
            None,
        )
        .with_filter(query.filter.expect("ID filter"));
        let first = facade
            .list_users_inner(&ctx, &first_request)
            .await
            .expect("first page");
        assert_eq!(
            first.items.iter().map(|user| user.id).collect::<Vec<_>>(),
            [ids[0]]
        );
        let cursor = first.page_info.next_cursor.expect("continuation");

        let reordered = FilterNode::InList {
            field: IdpUserFilterField::Id,
            values: vec![
                ODataValue::Uuid(ids[1]),
                ODataValue::Uuid(ids[0]),
                ODataValue::Uuid(ids[1]),
            ],
        };
        let second_request = list_users_req(
            tenant,
            metadata.clone(),
            IdpUserPagination::new(1, Some(cursor.clone())).expect("page"),
            None,
        )
        .with_filter(reordered);
        let second = facade
            .list_users_inner(&ctx, &second_request)
            .await
            .expect("second page");
        assert_eq!(
            second.items.iter().map(|user| user.id).collect::<Vec<_>>(),
            [ids[1]]
        );
        assert!(second.page_info.next_cursor.is_none());

        let changed =
            account_management_sdk::ListUsersQuery::with_ids([ids[0], ids[2]]).expect("batch");
        let changed_request = list_users_req(
            tenant,
            metadata,
            IdpUserPagination::new(1, Some(cursor)).expect("page"),
            None,
        )
        .with_filter(changed.filter.expect("ID filter"));
        assert!(matches!(
            facade.list_users_inner(&ctx, &changed_request).await,
            Err(PluginError::UserOpRejected { .. })
        ));
    })
    .await;
}

#[test]
fn build_string_matcher_rejects_not() {
    let inner = FilterNode::binary(
        IdpUserFilterField::Username,
        FilterOp::Eq,
        lit_string("alice"),
    );
    let node = FilterNode::not(inner);
    let err = UserFacade::build_string_matcher(Some(&node)).expect_err("not unsupported");
    assert!(matches!(err, PluginError::UserOpUnsupported { .. }));
}

#[test]
fn build_string_matcher_rejects_non_string_literal_on_string_field() {
    // Username is a string field; a Uuid literal is a type mismatch the
    // AM-side validator usually catches first, but the plugin's
    // belt-and-braces check rejects it explicitly too.
    let node = FilterNode::binary(
        IdpUserFilterField::Username,
        FilterOp::Eq,
        ODataValue::Uuid(Uuid::new_v4()),
    );
    let err = UserFacade::build_string_matcher(Some(&node)).expect_err("type mismatch");
    assert!(matches!(err, PluginError::UserOpUnsupported { .. }));
}

// =============================================================================
// StringMatcher::matches — predicate evaluation tests
// =============================================================================

fn make_rep(
    id: Uuid,
    username: &str,
    email: Option<&str>,
    first: Option<&str>,
    last: Option<&str>,
) -> UserRep {
    UserRep {
        id,
        created_timestamp: 0,
        username: Some(username.into()),
        email: email.map(str::to_owned),
        first_name: first.map(str::to_owned),
        last_name: last.map(str::to_owned),
    }
}

fn eq(field: IdpUserFilterField, needle: &str) -> StringMatcher {
    StringMatcher::Field {
        field,
        op: StringMatchOp::Eq,
        needle: needle.into(),
    }
}

fn contains(field: IdpUserFilterField, needle: &str) -> StringMatcher {
    StringMatcher::Field {
        field,
        op: StringMatchOp::Contains,
        needle: needle.into(),
    }
}

#[test]
fn matches_always_matches_every_rep() {
    let rep = make_rep(Uuid::new_v4(), "alice", Some("a@b"), Some("A"), Some("B"));
    assert!(StringMatcher::Always.matches(&rep));
}

#[test]
fn matches_eq_is_case_sensitive_exact() {
    let rep = make_rep(Uuid::new_v4(), "alice", None, None, None);
    assert!(eq(IdpUserFilterField::Username, "alice").matches(&rep));
    assert!(
        !eq(IdpUserFilterField::Username, "Alice").matches(&rep),
        "eq is case-sensitive"
    );
    assert!(!eq(IdpUserFilterField::Username, "bob").matches(&rep));
}

#[test]
fn matches_contains_is_case_insensitive_substring() {
    let rep = make_rep(
        Uuid::new_v4(),
        "alice",
        Some("Alice@Example.COM"),
        None,
        None,
    );
    assert!(
        contains(IdpUserFilterField::Username, "LIC").matches(&rep),
        "case-insensitive substring on username"
    );
    assert!(
        contains(IdpUserFilterField::Email, "example").matches(&rep),
        "case-insensitive substring on email"
    );
    assert!(!contains(IdpUserFilterField::Username, "xyz").matches(&rep));
}

#[test]
fn matches_display_name_derived_from_first_and_last() {
    // `display_name` is not on UserRep — derived as `"first last"`
    // when both are present (mirrors `user_rep_to_idp_user`).
    let rep = make_rep(Uuid::new_v4(), "u", None, Some("Alice"), Some("Liddell"));
    assert!(eq(IdpUserFilterField::DisplayName, "Alice Liddell").matches(&rep));
    assert!(contains(IdpUserFilterField::DisplayName, "lid").matches(&rep));
}

#[test]
fn matches_absent_field_never_satisfies_a_clause() {
    let rep = make_rep(Uuid::new_v4(), "u", None, None, None);
    assert!(!eq(IdpUserFilterField::Email, "a@b").matches(&rep));
    assert!(!contains(IdpUserFilterField::Email, "a").matches(&rep));
}

#[test]
fn matches_all_is_conjunction() {
    let rep = make_rep(Uuid::new_v4(), "alice", Some("a@b"), None, None);
    let all_ok = StringMatcher::All(vec![
        eq(IdpUserFilterField::Username, "alice"),
        eq(IdpUserFilterField::Email, "a@b"),
    ]);
    assert!(all_ok.matches(&rep));
    let one_bad = StringMatcher::All(vec![
        eq(IdpUserFilterField::Username, "alice"),
        eq(IdpUserFilterField::Email, "other"),
    ]);
    assert!(!one_bad.matches(&rep));
}

#[test]
fn matches_any_is_disjunction_frontend_search_shape() {
    // `contains(username,'a') or contains(email,'a') or contains(display_name,'a')`.
    let search = StringMatcher::Any(vec![
        contains(IdpUserFilterField::Username, "a"),
        contains(IdpUserFilterField::Email, "a"),
        contains(IdpUserFilterField::DisplayName, "a"),
    ]);
    // Matches via email even though username/display-name lack 'a'.
    let rep = make_rep(Uuid::new_v4(), "zzz", Some("qa@x"), None, None);
    assert!(search.matches(&rep));
    // No 'a' anywhere → no match.
    let rep2 = make_rep(Uuid::new_v4(), "zzz", Some("q@x"), Some("Bob"), Some("Lee"));
    assert!(!search.matches(&rep2));
}

// =============================================================================
// filter_hash — cursor pinning extension tests
// =============================================================================

#[test]
fn filter_hash_changes_when_string_filter_added() {
    let tenant = Uuid::new_v4();
    let h0 = UserFacade::filter_hash(tenant, "platform", None, &StringMatcher::Always);
    let h1 = UserFacade::filter_hash(
        tenant,
        "platform",
        None,
        &eq(IdpUserFilterField::Username, "alice"),
    );
    assert_ne!(
        h0, h1,
        "adding a string filter MUST change the hash so the previous \
         cursor is rejected as `invalid cursor`",
    );
}

#[test]
fn filter_hash_changes_when_string_filter_value_changes() {
    let tenant = Uuid::new_v4();
    let h1 = UserFacade::filter_hash(
        tenant,
        "platform",
        None,
        &eq(IdpUserFilterField::Username, "alice"),
    );
    let h2 = UserFacade::filter_hash(
        tenant,
        "platform",
        None,
        &eq(IdpUserFilterField::Username, "bob"),
    );
    assert_ne!(h1, h2, "changing the filter value MUST change the hash");
}

#[test]
fn filter_hash_changes_when_operator_changes() {
    // A cursor minted under `username eq 'a'` must be rejected under
    // `username contains 'a'` — the op is part of the filter identity.
    let tenant = Uuid::new_v4();
    let h_eq = UserFacade::filter_hash(
        tenant,
        "platform",
        None,
        &eq(IdpUserFilterField::Username, "a"),
    );
    let h_contains = UserFacade::filter_hash(
        tenant,
        "platform",
        None,
        &contains(IdpUserFilterField::Username, "a"),
    );
    assert_ne!(h_eq, h_contains, "eq vs contains MUST change the hash");
}

#[test]
fn filter_hash_changes_between_and_and_or() {
    let tenant = Uuid::new_v4();
    let clauses = || {
        vec![
            contains(IdpUserFilterField::Username, "a"),
            contains(IdpUserFilterField::Email, "a"),
        ]
    };
    let h_and = UserFacade::filter_hash(tenant, "platform", None, &StringMatcher::All(clauses()));
    let h_or = UserFacade::filter_hash(tenant, "platform", None, &StringMatcher::Any(clauses()));
    assert_ne!(h_and, h_or, "and vs or over the same clauses MUST differ");
}

#[test]
fn filter_hash_distinguishes_some_empty_string_from_none() {
    let tenant = Uuid::new_v4();
    let h_none = UserFacade::filter_hash(tenant, "platform", None, &StringMatcher::Always);
    let h_empty = UserFacade::filter_hash(
        tenant,
        "platform",
        None,
        &eq(IdpUserFilterField::Username, ""),
    );
    assert_ne!(
        h_none, h_empty,
        "`username eq ''` and no filter MUST hash differently",
    );
}

/// `realm_name` is folded into the hash so a tenant whose realm
/// rebinds mid-pagination produces a different cursor signature —
/// `invalid cursor` reject on the continuation request, same path
/// as a filter/tenant change. Pre-CursorV1 this was an explicit
/// `decoded.realm_name != metadata.realm_name` check on the plugin
/// side; the hash fold replaces it.
#[test]
fn filter_hash_changes_when_realm_name_changes() {
    let tenant = Uuid::new_v4();
    let h_a = UserFacade::filter_hash(tenant, "platform", None, &StringMatcher::Always);
    let h_b = UserFacade::filter_hash(tenant, "partner-acme", None, &StringMatcher::Always);
    assert_ne!(
        h_a, h_b,
        "realm rebind MUST change the hash (replaces the pre-CursorV1 \
         explicit realm-name equality check)",
    );
}

// ---------------------------------------------------------------------------
// KC response classification for user-create rejections.
// ---------------------------------------------------------------------------

#[test]
fn kc_conflict_classifies_duplicate_username() {
    let body = r#"{"errorMessage":"User exists with same username"}"#;
    assert_eq!(
        classify_kc_conflict(body),
        account_management_sdk::IdpUserDuplicateField::Username
    );
}

#[test]
fn kc_conflict_classifies_duplicate_email() {
    let body = r#"{"errorMessage":"User exists with same email"}"#;
    assert_eq!(
        classify_kc_conflict(body),
        account_management_sdk::IdpUserDuplicateField::Email
    );
}

// KC's ModelDuplicateException path emits the combined constant — on
// KC 26 it is the ONLY 409 `createUser` produces directly. A message
// naming both nouns must NOT be misattributed to username (the
// contains("username") check running first).
#[test]
fn kc_conflict_combined_message_is_username_or_email() {
    let body = r#"{"errorMessage":"User exists with same username or email"}"#;
    assert_eq!(
        classify_kc_conflict(body),
        account_management_sdk::IdpUserDuplicateField::UsernameOrEmail
    );
}

// The 409 status alone is the uniqueness signal: unknown wording,
// non-JSON proxy bodies, and JSON without `errorMessage` all stay in
// the duplicate lane with the honest unattributed field — wording
// drift can degrade the field token, never the 409 status.
#[test]
fn kc_conflict_unrecognised_body_is_unattributed_duplicate() {
    use account_management_sdk::IdpUserDuplicateField as F;
    assert_eq!(
        classify_kc_conflict(r#"{"errorMessage":"Some vendor surprise"}"#),
        F::UsernameOrEmail
    );
    assert_eq!(
        classify_kc_conflict("<html>proxy error</html>"),
        F::UsernameOrEmail
    );
    assert_eq!(
        classify_kc_conflict(r#"{"error":"conflict"}"#),
        F::UsernameOrEmail
    );
}

#[test]
fn kc_password_policy_reject_both_shapes_detected() {
    // Admin user-create with embedded credential (KC errorMessage shape).
    assert!(is_kc_password_policy_reject(
        r#"{"errorMessage":"Password policy not met"}"#
    ));
    // Credential-endpoint shape (`error` = invalidPassword* constant).
    assert!(is_kc_password_policy_reject(
        r#"{"error":"invalidPasswordMinLengthMessage","error_description":"Invalid password: minimum length 12"}"#
    ));
    // errorMessage carrying the invalidPassword* constant directly.
    assert!(is_kc_password_policy_reject(
        r#"{"errorMessage":"invalidPasswordMinDigitsMessage"}"#
    ));
}

#[test]
fn kc_password_policy_ignores_other_400s() {
    assert!(!is_kc_password_policy_reject(
        r#"{"errorMessage":"unrecognized field"}"#
    ));
    assert!(!is_kc_password_policy_reject("not json"));
    assert!(!is_kc_password_policy_reject(
        r#"{"error":"unknown_error"}"#
    ));
}
