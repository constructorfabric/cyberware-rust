use super::*;
use crate::domain::credstore::{CredStoreReader, CredStoreWriter};
use crate::domain::kc::transport::KcTransport;
use crate::domain::system_actor::build_system_ctx;
use crate::domain::test_support::{
    ConfigurableStubCS, StubCredStore, keycloak_cfg, make_metadata, noop_credstore_metrics,
    noop_failure_metrics, noop_metadata_metrics, noop_tenant_metrics,
};
use crate::infra::kc_http::ReqwestKcTransport;
use account_management_sdk::idp::IdpProvisionTenantRequest;
use account_management_sdk::idp_user::IdpTenantContext;
use async_trait::async_trait;
use credstore_sdk::{
    CredStoreClientV1, CredStoreError, Credential, CredentialListItem, CredentialPatch,
    CredentialWrite, PutOutcome, PutPrecondition, Secret, SecretRef, SecretValue, SharingMode,
    Validator, WritePrecondition,
};
use gts::GtsTypeId;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Recorded `put` invocation — `(secret_ref_str, value_bytes, sharing)`.
/// Used by Created-mode tests to assert the realm-admin secret landed
/// under the templated key with `SharingMode::Tenant`.
type RecordedPut = (String, Vec<u8>, SharingMode);

/// Recorded `delete` invocation — `secret_ref_str`.
/// Used by deprovision-tests to assert the realm-admin secret was
/// purged under the templated key.
type RecordedDelete = String;

/// Tenant-deprovision purge spy. With no configured outcome it succeeds and
/// reports zero deletions, making it the explicit no-op dependency for tests
/// unrelated to service accounts. A configured one-shot failure proves tenant
/// teardown stops rather than running past an unsuccessful credential purge.
#[derive(Default)]
struct RecordingTenantAccountPurgeHook {
    calls: Mutex<Vec<Uuid>>,
    next_outcome: Mutex<Option<Result<usize, PluginError>>>,
}

impl RecordingTenantAccountPurgeHook {
    fn with_outcome(outcome: Result<usize, PluginError>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next_outcome: Mutex::new(Some(outcome)),
        }
    }

    fn calls(&self) -> Vec<Uuid> {
        self.calls.lock().clone()
    }
}

#[async_trait]
impl TenantAccountPurgeHook for RecordingTenantAccountPurgeHook {
    async fn purge_tenant_accounts(&self, tenant_id: Uuid) -> Result<usize, PluginError> {
        self.calls.lock().push(tenant_id);
        self.next_outcome.lock().take().unwrap_or(Ok(0))
    }
}

fn noop_purge_hook() -> Arc<dyn TenantAccountPurgeHook> {
    Arc::new(RecordingTenantAccountPurgeHook::default())
}

/// Configurable unified stub. Created-mode tests inject one of these to
/// (a) record `put` calls (happy path asserts the templated key) and
/// (b) optionally fail the next `put` (ambiguous `OpenBao` failure
/// arm). Mirrors the `ConfigurableStubCS` pattern above. Deprovision
/// tests additionally read [`Self::delete_calls`] to assert the
/// Created last-tenant teardown purged the realm-admin secret.
///
/// `get_responses` is the read seam (mirrors `ConfigurableStubCS`):
/// tests that need the reader to resolve a per-realm secret seed it via
/// [`Self::with_get_entry`] (or by merging a `ConfigurableStubCS` into
/// `build_facade_with_cs_and_mutator`).
#[domain_model]
#[derive(Default)]
struct ConfigurableStubOpenBao {
    put_calls: Mutex<Vec<RecordedPut>>,
    delete_calls: Mutex<Vec<RecordedDelete>>,
    /// One-shot override for the next `put` call's response. `None` →
    /// default `Ok(())`. Consumed on read.
    next_put_response: Mutex<Option<Result<(), CredStoreError>>>,
    /// Keyed read responses (consumed on first match — same semantics as
    /// [`ConfigurableStubCS`]).
    get_responses: Mutex<std::collections::HashMap<String, Secret>>,
}

impl ConfigurableStubOpenBao {
    fn with_put_failure(err: CredStoreError) -> Self {
        Self {
            put_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            next_put_response: Mutex::new(Some(Err(err))),
            get_responses: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn seed_get(&self, key: &str, value: &str) {
        self.get_responses.lock().insert(
            key.to_owned(),
            Secret {
                reference: SecretRef::new(key).expect("valid SecretRef"),
                secret_type: String::new(),
                expires_at: None,
                value: SecretValue::from(value),
                validator: Validator {
                    id: uuid::Uuid::nil(),
                    version: 1,
                },
            },
        );
    }
}

#[async_trait]
impl CredStoreClientV1 for ConfigurableStubOpenBao {
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
        key: &SecretRef,
    ) -> Result<Option<Secret>, CredStoreError> {
        Ok(self.get_responses.lock().remove(key.as_ref()))
    }

    async fn put(
        &self,
        _ctx: &SecurityContext,
        key: &SecretRef,
        write: CredentialWrite,
        _precondition: PutPrecondition,
    ) -> Result<PutOutcome, CredStoreError> {
        self.put_calls.lock().push((
            key.as_ref().to_owned(),
            write.value.as_bytes().to_vec(),
            write.sharing,
        ));
        self.next_put_response
            .lock()
            .take()
            .unwrap_or(Ok(()))
            .map(|()| PutOutcome {
                created: true,
                validator: Validator {
                    id: uuid::Uuid::nil(),
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
            id: uuid::Uuid::nil(),
            version: 1,
        })
    }

    async fn delete(
        &self,
        _ctx: &SecurityContext,
        key: &SecretRef,
        _precondition: WritePrecondition,
    ) -> Result<(), CredStoreError> {
        self.delete_calls.lock().push(key.as_ref().to_owned());
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

fn tenant_facade_cfg() -> TenantFacadeConfig {
    TenantFacadeConfig {
        realm_defaults: serde_json::Value::Null,
        tenant_group_root: "/tenants".into(),
        provision_timeout_ms: 30_000,
    }
}

fn build_facade(server: &MockServer) -> TenantFacade {
    build_facade_with_cs(server, Arc::new(StubCredStore))
}

/// Variant of [`build_facade`] that lets the caller inject a custom
/// [`CredStoreClientV1`] — Adopted-mode tests use this to seed the
/// per-realm `OpenBao` response.
fn build_facade_with_cs(server: &MockServer, cs: Arc<dyn CredStoreClientV1>) -> TenantFacade {
    build_facade_with_unified_client(server, cs)
}

/// Builder for Created-mode tests that record `put`/`delete` calls.
/// The `mutator` is a [`ConfigurableStubOpenBao`] used as the single
/// unified [`CredStoreClientV1`]; pre-seed any needed `get` responses via
/// [`ConfigurableStubOpenBao::seed_get`] before calling this.
fn build_facade_with_cs_and_mutator(
    server: &MockServer,
    mutator: Arc<dyn CredStoreClientV1>,
) -> TenantFacade {
    build_facade_with_unified_client(server, mutator)
}

fn build_facade_with_unified_client(
    server: &MockServer,
    client: Arc<dyn CredStoreClientV1>,
) -> TenantFacade {
    build_facade_with_client_and_purge(server, client, noop_purge_hook())
}

fn build_facade_with_purge_hook(
    server: &MockServer,
    hook: Arc<dyn TenantAccountPurgeHook>,
) -> TenantFacade {
    build_facade_with_client_and_purge(server, Arc::new(StubCredStore), hook)
}

fn build_facade_with_client_and_purge(
    server: &MockServer,
    client: Arc<dyn CredStoreClientV1>,
    hook: Arc<dyn TenantAccountPurgeHook>,
) -> TenantFacade {
    let kc_cfg = keycloak_cfg(&server.uri());
    let realm_admin_cfg = kc_cfg.realm_admin.clone();
    let tenant_cfg = tenant_facade_cfg();

    let cs_reader = CredStoreReader::new(Arc::clone(&client), build_system_ctx(Uuid::nil()));
    let cs_writer = CredStoreWriter::new(client, build_system_ctx(Uuid::nil()));

    let transport: Arc<dyn KcTransport> = Arc::new(ReqwestKcTransport::new(reqwest::Client::new()));
    let factory = Arc::new(KeycloakAdminClientFactory::new(
        kc_cfg,
        transport,
        cs_reader,
        Arc::new(crate::domain::test_support::NoopMetrics),
    ));
    TenantFacade::new(
        tenant_cfg,
        realm_admin_cfg,
        factory,
        Arc::new(MetadataCodec),
        cs_writer,
        noop_tenant_metrics(),
        noop_credstore_metrics(),
        noop_metadata_metrics(),
        noop_failure_metrics(),
        hook,
    )
}

/// Build a Root-target request with the AM bootstrap saga's typical
/// `root_tenant_metadata` forwarded as `provisioning_metadata` — mode
/// = shared, realm = platform. This mirrors the production wiring after
/// the plugin no longer accepts `metadata = None`, so the AM
/// deployer is required to set `bootstrap.root_tenant_metadata`.
fn req_for_root(tenant_id: Uuid) -> IdpProvisionTenantRequest {
    IdpProvisionTenantRequest::for_root(
        tenant_id,
        "platform-root",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({
        "mode": "shared",
        "realm_name": "platform"
    }))
}

/// Mount `GET /realms/master` → 200 so the pre-saga
/// [`KeycloakAdminClientFactory::health_probe`] (called at the top of
/// every saga entry-point per DESIGN "Component Model") passes. Without this mount,
/// every facade test would hit wiremock's default 404 in the probe and
/// surface `PluginError::Config` before the saga-specific mocks ever
/// run. Tests that deliberately exercise the pre-saga KC-outage path
/// SHOULD NOT call this helper.
async fn mount_kc_health_probe_ok(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/realms/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(server)
        .await;
}

/// Mount the two token endpoints (master + the realm under test) so the
/// factory's `bootstrap_client()` and `realm_client(...)` both succeed.
/// Also mounts the health-probe endpoint — every saga test that issues
/// a token POST is also going to hit the health probe first.
async fn mount_token_endpoints(server: &MockServer, realm_under_test: &str) {
    mount_kc_health_probe_ok(server).await;
    Mock::given(method("POST"))
        .and(path("/realms/master/protocol/openid-connect/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "t-master",
            "expires_in": 600
        })))
        .mount(server)
        .await;
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

#[tokio::test]
async fn absent_metadata_on_root_rejects_missing_realm_name() {
    // a Root provision with absent `provisioning_metadata` has no
    // realm to bind to — explicit `realm_name` is required for Root. This is
    // caller-input misconfiguration on the AM side, so it surfaces as
    // `PluginError::ProvisionInputRejected` (mapped to a clean 400), which the
    // Phase 15 boundary maps to `IdpProvisionFailure::InvalidInput`.
    let server = MockServer::start().await;
    // No HTTP mocks needed — the failure short-circuits before any KC call.
    let facade = build_facade(&server);
    let ctx = build_system_ctx(Uuid::nil());
    let req = IdpProvisionTenantRequest::for_root(
        Uuid::new_v4(),
        "platform-root",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    );
    let err = facade
        .provision_tenant_inner(&ctx, &req)
        .await
        .expect_err("absent metadata must error");
    assert!(
        matches!(err, PluginError::ProvisionInputRejected { ref detail } if detail.contains("root provisioning requires") && detail.contains("realm_name")),
        "expected PluginError::ProvisionInputRejected(root provisioning requires ... realm_name), got {err:?}",
    );
}

#[tokio::test]
async fn explicit_shared_mode_against_default_realm() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "platform").await;

            Mock::given(method("GET"))
                .and(path("/admin/realms/platform"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;

            // Parent group already exists — skip the create.
            let parent_uuid = Uuid::new_v4();
            Mock::given(method("GET"))
                .and(path("/admin/realms/platform/groups"))
                .and(query_param("search", "tenants"))
                .and(query_param("exact", "true"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": parent_uuid.to_string(), "name": "tenants"}
                ])))
                .mount(&server)
                .await;

            let tenant_uuid = Uuid::new_v4();
            let group_uuid = Uuid::new_v4();
            Mock::given(method("POST"))
                .and(path_regex(format!(
                    r"^/admin/realms/platform/groups/{parent_uuid}/children$"
                )))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": group_uuid.to_string(),
                    "name": tenant_uuid.to_string()
                })))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            // a shared child inherits its realm from the parent's
            // idp_metadata (explicit `realm_name` is ignored). Supply a
            // parent_context whose decoded realm is `platform`.
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "child-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({ "mode": "shared" }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            let meta = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("provision ok");

            assert_eq!(meta.realm_name, "platform");
            assert_eq!(meta.realm_binding, RealmBinding::Shared);
            assert!(meta.admin_secret_ref.is_none());
            assert_eq!(meta.tenant_group_id, group_uuid);
        },
    )
    .await;
}

#[tokio::test]
async fn explicit_shared_mode_against_non_default_realm_uses_env_backed_admin() {
    // Shared is a single env-backed path regardless of realm
    // name. The plugin no longer distinguishes a "non-default Shared
    // realm" sub-case; multi-region Shared is out-of-scope for v1 and
    // future work goes through Adopted.
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "region-eu").await;

            Mock::given(method("GET"))
                .and(path("/admin/realms/region-eu"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;

            let parent_uuid = Uuid::new_v4();
            Mock::given(method("GET"))
                .and(path("/admin/realms/region-eu/groups"))
                .and(query_param("search", "tenants"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": parent_uuid.to_string(), "name": "tenants"}
                ])))
                .mount(&server)
                .await;

            let tenant_uuid = Uuid::new_v4();
            let group_uuid = Uuid::new_v4();
            Mock::given(method("POST"))
                .and(path(format!(
                    "/admin/realms/region-eu/groups/{parent_uuid}/children"
                )))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": group_uuid.to_string(),
                    "name": tenant_uuid.to_string()
                })))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            // shared child inherits the parent's realm (`region-eu`).
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "child-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({ "mode": "shared" }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "region-eu",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            let meta = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("provision ok");

            assert_eq!(meta.realm_name, "region-eu");
            assert_eq!(meta.realm_binding, RealmBinding::Shared);
            assert!(
                meta.admin_secret_ref.is_none(),
                "Shared binding always persists admin_secret_ref = None (DESIGN domain model)",
            );
            assert_eq!(meta.tenant_group_id, group_uuid);
        },
    )
    .await;
}

#[tokio::test]
async fn missing_realm_surfaces_as_kc_rest_404() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "platform").await;

            Mock::given(method("GET"))
                .and(path("/admin/realms/platform"))
                .respond_with(ResponseTemplate::new(404).set_body_string("Realm not found"))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = req_for_root(Uuid::new_v4());
            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("missing realm must error");
            assert!(
                matches!(
                    err,
                    PluginError::KcRest {
                        method: "GET",
                        status: KcStatusKind::Http(404),
                        ref path_template,
                        ..
                    } if path_template == "/admin/realms/{realm}"
                ),
                "expected KcRest{{ GET 404 /admin/realms/{{realm}} }}, got {err:?}",
            );
        },
    )
    .await;
}

#[tokio::test]
async fn malformed_metadata_surfaces_as_input_rejected() {
    let server = MockServer::start().await;
    // No HTTP mocks needed — the failure short-circuits before any KC call.
    let facade = build_facade(&server);
    let ctx = build_system_ctx(Uuid::nil());
    // `mode` typed as a number — fails `WireMetadata` deserialisation.
    // surfaces wire-shape failures as `ProvisionInputRejected` ("malformed").
    let req = IdpProvisionTenantRequest::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        "child-name",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({"mode": 42}));

    let err = facade
        .provision_tenant_inner(&ctx, &req)
        .await
        .expect_err("malformed metadata must error");
    assert!(
        matches!(err, PluginError::ProvisionInputRejected { ref detail } if detail.contains("malformed")),
        "expected PluginError::ProvisionInputRejected(malformed ...), got {err:?}",
    );
}

#[tokio::test]
async fn invalid_mode_surfaces_as_input_rejected() {
    let server = MockServer::start().await;
    let facade = build_facade(&server);
    let ctx = build_system_ctx(Uuid::nil());
    let req = IdpProvisionTenantRequest::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        "child-name",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({"mode": "wat", "realm_name": "platform"}));

    let err = facade
        .provision_tenant_inner(&ctx, &req)
        .await
        .expect_err("invalid mode must error");
    assert!(
        matches!(err, PluginError::ProvisionInputRejected { ref detail } if detail.contains("not allowed")),
        "expected PluginError::ProvisionInputRejected(not allowed ...), got {err:?}",
    );
}

// ---------------------------------------------------------------------
// Phase 9 — Adopted mode (DESIGN "Domain Model", "Interactions & Sequences", PRD "Explicit Realm Binding", "Interactions & Sequences")
// ---------------------------------------------------------------------

/// Happy path: target realm `partner-acme` exists, the parent `/tenants`
/// group exists with zero subgroups, the per-realm admin secret is
/// pre-provisioned in `OpenBao`. Expect Adopted metadata with
/// `admin_secret_ref = Some(<templated key>)`.
#[tokio::test]
async fn adopted_with_empty_realm_provisions_with_secret_ref() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        // Bootstrap probes that the realm exists.
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        // Parent group search (bootstrap admin) — present.
        let parent_uuid = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": parent_uuid.to_string(), "name": "tenants"}
            ])))
            .mount(&server)
            .await;

        // Emptiness probe (bootstrap admin) — zero children.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/partner-acme/groups/{parent_uuid}/children"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        // Realm-admin then creates the per-tenant child subgroup.
        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        Mock::given(method("POST"))
            .and(path(format!(
                "/admin/realms/partner-acme/groups/{parent_uuid}/children"
            )))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": group_uuid.to_string(),
                "name": tenant_uuid.to_string()
            })))
            .mount(&server)
            .await;

        // Per-realm OpenBao response — keyed by the templated SecretRef.
        let cs = Arc::new(ConfigurableStubCS::with_entry(
            "keycloak-idp-realm-admin-partner-acme-secret",
            "per-realm-ra-secret",
        ));
        let facade = build_facade_with_cs(&server, cs);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            tenant_uuid,
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let meta = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect("adopted provision ok");

        assert_eq!(meta.realm_name, "partner-acme");
        assert_eq!(meta.realm_binding, RealmBinding::Adopted);
        assert_eq!(
            meta.admin_secret_ref.as_deref(),
            Some("keycloak-idp-realm-admin-partner-acme-secret"),
            "Adopted always carries the templated SecretRef",
        );
        assert_eq!(meta.tenant_group_id, group_uuid);
        assert_eq!(meta.admin_client_id, "keycloak-idp-plugin-realm-admin");
    })
    .await;
}

/// Absent parent tenant group is success-equivalent (DESIGN "Interactions & Sequences"): the
/// realm-admin step creates both the parent and child groups.
#[tokio::test]
async fn adopted_with_absent_parent_group_succeeds() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        // Parent group search: returns [] on the first TWO calls (bootstrap
        // emptiness probe at Adopted step 2 + realm-admin's initial probe
        // in `find_or_create_top_group` step 4), then returns the created
        // group on the third call — the post-create lookup re-search that
        // recovers the id without parsing the empty 201 body (KC 26.x
        // contract). See test_am_tenant_tree's barrier-flip test for the
        // matching production codepath.
        let parent_uuid = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .with_priority(1)
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": parent_uuid.to_string(), "name": "tenants"}
            ])))
            .mount(&server)
            .await;

        // Realm-admin creates parent /tenants — body intentionally empty to
        // match KC 26.x behaviour (id is in `Location` header only).
        Mock::given(method("POST"))
            .and(path("/admin/realms/partner-acme/groups"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        // Realm-admin creates per-tenant child subgroup.
        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        Mock::given(method("POST"))
            .and(path(format!(
                "/admin/realms/partner-acme/groups/{parent_uuid}/children"
            )))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": group_uuid.to_string(),
                "name": tenant_uuid.to_string()
            })))
            .mount(&server)
            .await;

        let cs = Arc::new(ConfigurableStubCS::with_entry(
            "keycloak-idp-realm-admin-partner-acme-secret",
            "per-realm-ra-secret",
        ));
        let facade = build_facade_with_cs(&server, cs);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            tenant_uuid,
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let meta = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect("adopted provision with absent parent ok");
        assert_eq!(meta.realm_binding, RealmBinding::Adopted);
        assert_eq!(meta.tenant_group_id, group_uuid);
    })
    .await;
}

/// Parent /tenants has an existing subgroup → CleanFailure-equivalent.
/// Surfaced as `PluginError::KcRest { status: Http(409), .. }` so the
/// Phase 15 boundary's 4xx-→-CleanFailure table picks it up without a
/// dedicated variant (see module commit notes for the variant
/// rationale).
#[tokio::test]
async fn adopted_with_non_empty_parent_group_returns_clean_failure() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let parent_uuid = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": parent_uuid.to_string(), "name": "tenants"}
            ])))
            .mount(&server)
            .await;

        // Parent has one existing subgroup → emptiness probe fails.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/partner-acme/groups/{parent_uuid}/children"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": Uuid::new_v4().to_string(), "name": "existing-tenant"}
            ])))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("non-empty parent must error");
        assert!(
            matches!(
                err,
                PluginError::KcRest {
                    method: "GET",
                    status: KcStatusKind::Http(409),
                    ref path_template,
                    ref body_first_2kb,
                    ..
                } if path_template == "/admin/realms/{realm}/groups/{parent_uuid}/children"
                    && body_first_2kb.contains("partner-acme")
                    && body_first_2kb.contains("not empty")
            ),
            "expected KcRest{{ GET 409 /children, body mentions realm + not empty }}, got {err:?}",
        );
    })
    .await;
}

/// Shared-mode realm probe 403 (bootstrap admin lacks `view-realm`) →
/// `PluginError::BootstrapPermsMissing { realm }` (DESIGN "API Contracts").
/// Phase 15 translates this to `IdpProvisionFailure::UnsupportedOperation`.
#[tokio::test]
async fn shared_with_realm_probe_403_returns_bootstrap_perms_missing() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "partner-acme").await;

            Mock::given(method("GET"))
                .and(path("/admin/realms/partner-acme"))
                .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            // shared child inherits the parent's realm (`partner-acme`).
            let req = IdpProvisionTenantRequest::new(
                Uuid::new_v4(),
                Uuid::new_v4(),
                "child-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({ "mode": "shared" }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "partner-acme",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("403 on realm probe must error");
            assert!(
                matches!(err, PluginError::BootstrapPermsMissing { ref realm } if realm == "partner-acme"),
                "expected BootstrapPermsMissing{{ realm: partner-acme }}, got {err:?}",
            );
        },
    )
    .await;
}

/// Adopted-mode realm probe 403 (bootstrap admin lacks `view-realm`) →
/// `PluginError::BootstrapPermsMissing { realm }` (DESIGN "API Contracts").
#[tokio::test]
async fn adopted_with_realm_probe_403_returns_bootstrap_perms_missing() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("403 on realm probe must error");
        assert!(
            matches!(err, PluginError::BootstrapPermsMissing { ref realm } if realm == "partner-acme"),
            "expected BootstrapPermsMissing{{ realm: partner-acme }}, got {err:?}",
        );
    })
    .await;
}

/// Adopted-mode group search 403 (bootstrap admin lacks `query-groups`) →
/// `PluginError::BootstrapPermsMissing { realm }` (DESIGN "API Contracts").
#[tokio::test]
async fn adopted_with_group_search_403_returns_bootstrap_perms_missing() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        // Realm probe succeeds — gates the failure to the group-search step.
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("403 on group search must error");
        assert!(
            matches!(err, PluginError::BootstrapPermsMissing { ref realm } if realm == "partner-acme"),
            "expected BootstrapPermsMissing{{ realm: partner-acme }}, got {err:?}",
        );
    })
    .await;
}

/// Non-2xx coverage gap: 5xx on the Adopted group-search probe surfaces as
/// `KcRest{ GET 503 /admin/realms/{realm}/groups }` — distinct from the
/// 403→`BootstrapPermsMissing` and 404→`KcRest` paths.
#[tokio::test]
async fn adopted_with_group_search_5xx_returns_kc_rest() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("5xx on group search must error");
        assert!(
            matches!(
                err,
                PluginError::KcRest {
                    method: "GET",
                    status: KcStatusKind::Http(503),
                    ref path_template,
                    ..
                } if path_template == "/admin/realms/{realm}/groups"
            ),
            "expected KcRest{{ GET 503 /admin/realms/{{realm}}/groups }}, got {err:?}",
        );
    })
    .await;
}

/// Non-2xx coverage gap: 5xx on the Adopted `has_subgroup` probe surfaces
/// as `KcRest{ GET 503 /children }` — the parent group is found, then the
/// children-listing call fails with a transient upstream error.
#[tokio::test]
async fn adopted_with_has_subgroup_5xx_returns_kc_rest() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let parent_uuid = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": parent_uuid.to_string(), "name": "tenants"}
            ])))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/partner-acme/groups/{parent_uuid}/children"
            )))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("5xx on has_subgroup must error");
        assert!(
            matches!(
                err,
                PluginError::KcRest {
                    method: "GET",
                    status: KcStatusKind::Http(503),
                    ref path_template,
                    ..
                } if path_template == "/admin/realms/{realm}/groups/{parent_uuid}/children"
            ),
            "expected KcRest{{ GET 503 /children }}, got {err:?}",
        );
    })
    .await;
}

/// Adopted-mode realm probe 404 surfaces as `KcRest{ GET 404 }` (same
/// shape as the Shared-mode test, but with `mode = adopted`).
#[tokio::test]
async fn adopted_against_missing_realm_returns_kc_rest_404() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-acme"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Realm not found"))
            .mount(&server)
            .await;

        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = IdpProvisionTenantRequest::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "child-name",
            GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        )
        .with_metadata(serde_json::json!({
            "mode": "adopted",
            "realm_name": "partner-acme"
        }));

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("missing realm must error");
        assert!(
            matches!(
                err,
                PluginError::KcRest {
                    method: "GET",
                    status: KcStatusKind::Http(404),
                    ref path_template,
                    ..
                } if path_template == "/admin/realms/{realm}"
            ),
            "expected KcRest{{ GET 404 /admin/realms/{{realm}} }}, got {err:?}",
        );
    })
    .await;
}

// ---------------------------------------------------------------------
// Phase 10 — Created mode (DESIGN "Interactions & Sequences" and "Security
// and Data Protection"; PRD "Explicit Realm Binding")
// ---------------------------------------------------------------------

/// Mount the full happy-path KC choreography for Created mode:
/// (1) realm probe → 404, (2) realm-create → 201, (3) client-create →
/// 201, (4) lookup created client by clientId → returns its UUID, (5)
/// service-account-user lookup → user UUID, (6) realm-management client
/// lookup by clientId → its UUID, (7) available role list → all 7
/// `REALM_ADMIN_REQUIRED_ROLES` present, (8) POST role-mappings → 204,
/// (9) GET client-secret → plaintext. Plus the realm-admin group steps.
///
/// The caller passes the `tenant_uuid` (so it can derive the generated
/// `realm-<tenant_uuid>` and seed the matching credstore key). Returns
/// `(group_uuid, parent_uuid)`.
async fn mount_created_happy_path(
    server: &MockServer,
    tenant_uuid: Uuid,
    realm: &str,
    client_secret_plaintext: &str,
) -> (Uuid, Uuid) {
    mount_token_endpoints(server, realm).await;

    // Step 1: realm existence probe → 404 (realm absent).
    Mock::given(method("GET"))
        .and(path(format!("/admin/realms/{realm}")))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(server)
        .await;

    // Step 2: realm-create → 201.
    Mock::given(method("POST"))
        .and(path("/admin/realms"))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;

    mount_created_downstream(server, realm, tenant_uuid, client_secret_plaintext).await
}

/// Mount the post-realm-create KC choreography (client create, lookups,
/// role mappings, client-secret read, and the realm-admin group steps) for
/// Created mode. Split out of [`mount_created_happy_path`] so the
/// idempotency / reconcile tests can drive their own realm GET/POST
/// behaviour while reusing this long downstream tail. Does NOT mount the
/// token endpoints, the realm existence probe, or `POST /admin/realms`.
async fn mount_created_downstream(
    server: &MockServer,
    realm: &str,
    tenant_uuid: Uuid,
    client_secret_plaintext: &str,
) -> (Uuid, Uuid) {
    let admin_client_uuid = Uuid::new_v4();
    let sa_user_uuid = Uuid::new_v4();
    let rm_client_uuid = Uuid::new_v4();
    let parent_uuid = Uuid::new_v4();
    let group_uuid = Uuid::new_v4();

    // Step 3: realm-admin client create → 201.
    Mock::given(method("POST"))
        .and(path(format!("/admin/realms/{realm}/clients")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;

    // Step 4: lookup created realm-admin client by clientId.
    Mock::given(method("GET"))
        .and(path(format!("/admin/realms/{realm}/clients")))
        .and(query_param("clientId", "keycloak-idp-plugin-realm-admin"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": admin_client_uuid.to_string()}
        ])))
        .mount(server)
        .await;

    // Step 5: service-account-user lookup.
    Mock::given(method("GET"))
        .and(path(format!(
            "/admin/realms/{realm}/clients/{admin_client_uuid}/service-account-user"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": sa_user_uuid.to_string()
        })))
        .mount(server)
        .await;

    // Step 6: realm-management client lookup.
    Mock::given(method("GET"))
        .and(path(format!("/admin/realms/{realm}/clients")))
        .and(query_param("clientId", "realm-management"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": rm_client_uuid.to_string()}
        ])))
        .mount(server)
        .await;

    // Step 7: available realm-management roles for the SA user. Echo
    // back all 7 required roles so the selection step succeeds.
    let role_ids: Vec<serde_json::Value> = REALM_ADMIN_REQUIRED_ROLES
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": Uuid::new_v4().to_string(),
                "name": *n,
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(format!(
            "/admin/realms/{realm}/users/{sa_user_uuid}/role-mappings/clients/{rm_client_uuid}/available"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Array(role_ids)))
        .mount(server).await;

    // Step 8: POST role-mappings → 204.
    Mock::given(method("POST"))
        .and(path(format!(
            "/admin/realms/{realm}/users/{sa_user_uuid}/role-mappings/clients/{rm_client_uuid}"
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;

    // Step 9: read client_secret.
    Mock::given(method("GET"))
        .and(path(format!(
            "/admin/realms/{realm}/clients/{admin_client_uuid}/client-secret"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "secret",
            "value": client_secret_plaintext,
        })))
        .mount(server)
        .await;

    // Realm-admin step: parent group missing on initial probe, then visible
    // after POST creates it.
    //
    // Mock sequence — the saga calls `GET /groups?search=tenants&exact=true`
    // TWICE in the create branch: once before the POST (to decide whether to
    // create) and once AFTER the POST (to recover the new group's id, since
    // Keycloak returns 201 + empty body + `Location` header on POST /groups).
    // The first call must return `[]`; the second must return the populated
    // entry. We express that with two mocks at different priorities:
    //   * priority 1, `up_to_n_times(1)` → answers the first request only
    //   * priority 5 (default) → answers everything after
    // Lower priority number = higher precedence in wiremock-rs.
    Mock::given(method("GET"))
        .and(path(format!("/admin/realms/{realm}/groups")))
        .and(query_param("search", "tenants"))
        .and(query_param("exact", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/admin/realms/{realm}/groups")))
        .and(query_param("search", "tenants"))
        .and(query_param("exact", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": parent_uuid.to_string(), "name": "tenants"}
        ])))
        .mount(server)
        .await;
    // POST returns 201 with empty body — matches KC 26.x's actual behaviour
    // (group id only in the `Location` header). The plugin no longer parses
    // this body; it re-issues the GET search instead.
    Mock::given(method("POST"))
        .and(path(format!("/admin/realms/{realm}/groups")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;
    // Per-tenant child subgroup.
    Mock::given(method("POST"))
        .and(path(format!(
            "/admin/realms/{realm}/groups/{parent_uuid}/children"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": group_uuid.to_string(),
            "name": tenant_uuid.to_string()
        })))
        .mount(server)
        .await;

    (group_uuid, parent_uuid)
}

fn created_request(tenant_uuid: Uuid) -> IdpProvisionTenantRequest {
    // created mode GENERATES `realm-<tenant_id>` — no `realm_name`
    // on the wire (supplying one is rejected).
    IdpProvisionTenantRequest::new(
        tenant_uuid,
        Uuid::new_v4(),
        "child-name",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({ "mode": "created" }))
}

/// Happy path: full Created saga succeeds end-to-end. Asserts
/// (1) the returned metadata carries `realm_binding = Created` and
/// the templated `admin_secret_ref`, (2) the `tenant_group_id` comes
/// from the realm-admin child-group create, (3) the `OpenBao` stub
/// recorded exactly ONE `put` keyed by the templated `SecretRef` with
/// `SharingMode::Tenant` and the verbatim client-secret bytes.
#[tokio::test]
async fn created_happy_path_writes_secret_and_returns_created_metadata() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        // created mode generates `realm-<tenant_id>`.
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        let secret_key = format!("keycloak-idp-realm-admin-{realm}-secret");
        let (group_uuid, _parent_uuid) =
            mount_created_happy_path(&server, tenant_uuid, &realm, "the-generated-client-secret")
                .await;

        // Per-realm OpenBao read for the realm-admin client AFTER the
        // plugin's `put` lands (the factory caches the resulting token
        // so this only needs the read seam wired). Seed the read
        // response directly into the unified stub.
        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        mutator.seed_get(&secret_key, "the-generated-client-secret");
        let facade = build_facade_with_unified_client(
            &server,
            Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>,
        );
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let meta = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect("created provision ok");

        assert_eq!(meta.version, "v1");
        assert_eq!(meta.realm_name, realm);
        assert_eq!(meta.realm_binding, RealmBinding::Created);
        assert_eq!(meta.admin_secret_ref.as_deref(), Some(secret_key.as_str()),);
        assert_eq!(meta.tenant_group_id, group_uuid);
        assert_eq!(meta.admin_client_id, "keycloak-idp-plugin-realm-admin");

        let recorded = mutator.put_calls.lock();
        assert_eq!(
            recorded.len(),
            1,
            "Created mode MUST persist exactly one OpenBao put"
        );
        assert_eq!(recorded[0].0, secret_key);
        assert_eq!(recorded[0].1, b"the-generated-client-secret");
        assert_eq!(
            recorded[0].2,
            SharingMode::Tenant,
            "Created-mode put must use SharingMode::Tenant per DESIGN security and data protection",
        );
    })
    .await;
}

/// Regression: Created-mode saga MUST force-refresh the bootstrap token
/// between `create_realm` (step 2) and `create_realm_admin_client` (step 3).
///
/// Why this matters — Keycloak's role-claim issuance is point-in-time. When
/// the bootstrap client creates a new realm via `POST /admin/realms`, KC
/// auto-grants the creator `realm-admin` on the new realm, but the access
/// token already in flight (minted BEFORE the realm existed) does not carry
/// the new mapping. The next bootstrap call against
/// `POST /admin/realms/{new_realm}/clients` then returns `403 Forbidden`.
/// The reactive-401 wrapper only refreshes on 401, so without the explicit
/// `invalidate_bootstrap()` between steps 2 and 3 the saga surfaces
/// `AmbiguousCreated { stage: KcClientCreate }` → AM 500.
///
/// We pin the observable consequence of the fix: the bootstrap `master`
/// token endpoint is hit AT LEAST TWICE per Created-mode tenant create —
/// once for the cold-cache warm-up that covers steps 1-2, and once more
/// after the mid-saga invalidate so step 3 carries a fresh bearer with the
/// new `realm-admin` claim. Drop the invalidate and the count collapses to
/// 1 — the assertion below catches that regression directly.
#[tokio::test]
async fn created_saga_remints_bootstrap_token_after_realm_create() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        let (_group_uuid, _parent_uuid) =
            mount_created_happy_path(&server, tenant_uuid, &realm, "the-generated-client-secret")
                .await;
        let cs = Arc::new(ConfigurableStubCS::with_entry(
            &format!("keycloak-idp-realm-admin-{realm}-secret"),
            "the-generated-client-secret",
        ));
        let facade = build_facade_with_cs(&server, cs);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect("created provision ok");

        // Count POST hits on the bootstrap (master) token endpoint. The
        // cold-cache warm-up at the start of the saga is one hit; the
        // post-`create_realm` invalidate forces a second one so step 3's
        // `POST /admin/realms/{realm}/clients` is sent with a token that
        // carries the just-granted `realm-admin` claim.
        let bootstrap_token_mints = server
            .received_requests()
            .await
            .expect("mock server records requests")
            .iter()
            .filter(|r| {
                r.method == wiremock::http::Method::POST
                    && r.url.path() == "/realms/master/protocol/openid-connect/token"
            })
            .count();
        assert!(
            bootstrap_token_mints >= 2,
            "Created-mode saga MUST re-mint the bootstrap token after `create_realm` \
             so the step-3 bearer carries the new `realm-admin` claim; observed \
             {bootstrap_token_mints} mint(s) — fix likely reverted"
        );
    })
    .await;
}

/// `mode = created` against a realm that already exists → bootstrap
/// probe returns 200 → [`PluginError::CreatedRealmExists`]. NO realm
/// is created and NO `OpenBao` put happens. Phase 15 maps this to
/// `CleanFailure` per DESIGN "API Contracts".
#[tokio::test]
async fn created_against_existing_realm_returns_clean_failure_equivalent() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        mount_token_endpoints(&server, &realm).await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        let facade = build_facade_with_cs_and_mutator(
            &server,
            Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>,
        );
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("existing realm must error");
        assert!(
            matches!(
                err,
                PluginError::CreatedRealmExists { realm: ref got }
                    if got == &realm
            ),
            "expected CreatedRealmExists{{ realm: {realm} }}, got {err:?}",
        );
        assert!(
            mutator.put_calls.lock().is_empty(),
            "no OpenBao writes on validation rejection",
        );
    })
    .await;
}

/// Bootstrap probe 404; realm-create POST returns 503; reconciliation probe
/// returns 404 again. The second probe proves the mutation retained no realm,
/// so this is clean and retryable rather than ambiguous.
#[tokio::test]
async fn created_kc_realm_create_503_then_absent_is_clean_retryable() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        mount_token_endpoints(&server, &realm).await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/realms"))
            .respond_with(ResponseTemplate::new(503).set_body_string("boom"))
            .mount(&server)
            .await;

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        let facade = build_facade_with_cs_and_mutator(
            &server,
            Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>,
        );
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("503 on realm-create must error");
        assert!(
            matches!(
                err,
                PluginError::RealmCreateNotRetained {
                    realm: ref got,
                    ref detail,
                } if got == &realm && detail.contains("503") && detail.contains("boom")
            ),
            "expected RealmCreateNotRetained with the create diagnostic, got {err:?}",
        );
        assert!(
            mutator.put_calls.lock().is_empty(),
            "no OpenBao writes when KC realm-create fails",
        );
    })
    .await;
}

/// If the post-create reconciliation probe cannot answer, absence has not been
/// proven and the original staged ambiguity must survive unchanged.
#[tokio::test]
async fn created_kc_realm_create_503_then_probe_503_remains_ambiguous() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        mount_token_endpoints(&server, &realm).await;

        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(503).set_body_string("probe unavailable"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/realms"))
            .respond_with(ResponseTemplate::new(503).set_body_string("create unavailable"))
            .mount(&server)
            .await;

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        let facade = build_facade_with_cs_and_mutator(
            &server,
            Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>,
        );
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("an inconclusive reconciliation probe must stay ambiguous");
        assert!(
            matches!(
                err,
                PluginError::AmbiguousCreated {
                    stage: AmbiguousStage::KcRealmCreate,
                    ref detail,
                } if detail.contains("503") && detail.contains("create unavailable")
            ),
            "expected the original KcRealmCreate ambiguity, got {err:?}",
        );
        assert!(mutator.put_calls.lock().is_empty());
    })
    .await;
}

// ---------------------------------------------------------------------
// Created-mode realm-create idempotency (run am-functional-22):
// the realm-create step must reconcile a 409 / lost-our-own-create against
// an ownership marker instead of blanket-tagging `AmbiguousCreated`.
// ---------------------------------------------------------------------

/// A `GET /admin/realms/{realm}` body carrying the plugin's ownership
/// attribute for `tenant_id` — reconcile recognises this realm as "ours".
fn realm_body_with_owner(realm: &str, tenant_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "realm": realm,
        "attributes": { "cf.provisioning.tenant_id": tenant_id.to_string() },
    })
}

/// Regression (run am-functional-22): under KC load the realm-create POST
/// is retried after a timeout; the first POST already created the realm, so
/// the retry hits KC's unique-name constraint → 409. The plugin MUST
/// recognise the realm as its own (ownership marker on the re-GET) and treat
/// the create as idempotent success — NOT surface `AmbiguousCreated` → 500.
#[tokio::test]
async fn created_realm_create_409_with_our_marker_reconciles_to_success() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        let realm = realm.as_str();
        mount_token_endpoints(&server, realm).await;

        // Initial existence probe → 404 (realm absent at decision time).
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Re-GET after the 409 → realm present, carrying OUR ownership marker.
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(realm_body_with_owner(realm, tenant_uuid)),
            )
            .mount(&server)
            .await;
        // realm-create POST → 409 (the duplicate from the retried POST).
        Mock::given(method("POST"))
            .and(path("/admin/realms"))
            .respond_with(ResponseTemplate::new(409).set_body_string("Conflict detected"))
            .mount(&server)
            .await;

        let _ = mount_created_downstream(&server, realm, tenant_uuid, "sekret").await;

        let cs = Arc::new(ConfigurableStubCS::with_entry(
            &format!("keycloak-idp-realm-admin-{realm}-secret"),
            "sekret",
        ));
        let facade = build_facade_with_cs(&server, cs);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let meta = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect("409 on our own realm-create retry must reconcile to success");
        assert_eq!(meta.realm_binding, RealmBinding::Created);
        assert_eq!(meta.realm_name, realm);
    })
    .await;
}

/// Re-provisioning a tenant whose dedicated realm already exists AND carries
/// our ownership marker is idempotent: the plugin adopts the existing realm
/// (no second `POST /admin/realms`) and completes the saga.
#[tokio::test]
async fn created_existing_realm_with_our_marker_is_adopted() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        let realm = realm.as_str();
        mount_token_endpoints(&server, realm).await;
        // Existence probe → 200 with our marker (a prior attempt made it).
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(realm_body_with_owner(realm, tenant_uuid)),
            )
            .mount(&server)
            .await;
        let _ = mount_created_downstream(&server, realm, tenant_uuid, "sekret").await;

        let cs = Arc::new(ConfigurableStubCS::with_entry(
            &format!("keycloak-idp-realm-admin-{realm}-secret"),
            "sekret",
        ));
        let facade = build_facade_with_cs(&server, cs);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let meta = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect("existing realm with our marker must be adopted");
        assert_eq!(meta.realm_binding, RealmBinding::Created);

        let realm_creates = server
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter(|r| r.method == wiremock::http::Method::POST && r.url.path() == "/admin/realms")
            .count();
        assert_eq!(realm_creates, 0, "adopt path must NOT create a realm");
    })
    .await;
}

/// Existence probe returns a realm owned by a DIFFERENT tenant (foreign
/// marker) → clean `CreatedRealmExists`, never adoption, never a POST.
#[tokio::test]
async fn created_existing_realm_with_foreign_marker_returns_clean_failure() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        let realm = realm.as_str();
        mount_token_endpoints(&server, realm).await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(realm_body_with_owner(realm, Uuid::new_v4())),
            )
            .mount(&server)
            .await;
        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);
        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("foreign realm must be a clean failure");
        assert!(
            matches!(err, PluginError::CreatedRealmExists { realm: ref got } if got == realm),
            "expected CreatedRealmExists, got {err:?}",
        );
    })
    .await;
}

/// A 409 on realm-create where the existing realm is FOREIGN (someone else
/// won a create race) → clean `CreatedRealmExists`, not success and not
/// ambiguous.
#[tokio::test]
async fn created_realm_create_409_with_foreign_marker_returns_clean_failure() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        let realm = realm.as_str();
        mount_token_endpoints(&server, realm).await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(realm_body_with_owner(realm, Uuid::new_v4())),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/realms"))
            .respond_with(ResponseTemplate::new(409).set_body_string("conflict"))
            .mount(&server)
            .await;
        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);
        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("409 with foreign owner must be clean failure");
        assert!(
            matches!(err, PluginError::CreatedRealmExists { .. }),
            "expected CreatedRealmExists, got {err:?}",
        );
    })
    .await;
}

/// Bootstrap probe 404, realm-create 201; client-create POST returns 500
/// → `AmbiguousCreated { stage: KcClientCreate, .. }`. KC may have
/// created the realm-admin client; reconciliation is the runbook.
#[tokio::test]
async fn created_kc_client_create_5xx_returns_ambiguous_kc_client_create() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        mount_token_endpoints(&server, &realm).await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/realms"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        // Client-create fails with 500 — the test's whole point.
        Mock::given(method("POST"))
            .and(path(format!("/admin/realms/{realm}/clients")))
            .respond_with(ResponseTemplate::new(500).set_body_string("client boom"))
            .mount(&server)
            .await;

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        let facade =
            build_facade_with_cs_and_mutator(&server, Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("5xx on client-create must error");
        assert!(
            matches!(
                err,
                PluginError::AmbiguousCreated {
                    stage: AmbiguousStage::KcClientCreate,
                    ref detail,
                } if detail.contains("500") && detail.contains("client boom")
            ),
            "expected AmbiguousCreated{{ stage: KcClientCreate, detail mentions 500/client boom }}, got {err:?}",
        );
        assert!(
            mutator.put_calls.lock().is_empty(),
            "no OpenBao writes when KC client-create fails",
        );
    })
    .await;
}

/// KC realm-create + client-create succeed; the POST role-mappings call
/// returns 500 → `AmbiguousCreated { stage: KcRoleMapping, .. }`. KC has
/// the realm-admin client and SA user but the role binding may be
/// partial — reconciliation re-applies the mapping.
#[tokio::test]
async fn created_kc_role_mapping_5xx_returns_ambiguous_kc_role_mapping() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        // Manually mount the choreography up to (but excluding) the
        // POST role-mappings so we can override that step with 500.
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        mount_token_endpoints(&server, &realm).await;
        let admin_client_uuid = Uuid::new_v4();
        let sa_user_uuid = Uuid::new_v4();
        let rm_client_uuid = Uuid::new_v4();

        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/realms"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/admin/realms/{realm}/clients")))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}/clients")))
            .and(query_param("clientId", "keycloak-idp-plugin-realm-admin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": admin_client_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/{realm}/clients/{admin_client_uuid}/service-account-user"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": sa_user_uuid.to_string()
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}/clients")))
            .and(query_param("clientId", "realm-management"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": rm_client_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        let role_ids: Vec<serde_json::Value> = REALM_ADMIN_REQUIRED_ROLES
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": Uuid::new_v4().to_string(),
                    "name": *n,
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/{realm}/users/{sa_user_uuid}/role-mappings/clients/{rm_client_uuid}/available"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Array(role_ids)))
            .mount(&server)
            .await;
        // POST role-mappings fails with 500 — the test's whole point.
        Mock::given(method("POST"))
            .and(path(format!(
                "/admin/realms/{realm}/users/{sa_user_uuid}/role-mappings/clients/{rm_client_uuid}"
            )))
            .respond_with(ResponseTemplate::new(500).set_body_string("rm boom"))
            .mount(&server)
            .await;

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        let facade =
            build_facade_with_cs_and_mutator(&server, Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("5xx on POST role-mappings must error");
        assert!(
            matches!(
                err,
                PluginError::AmbiguousCreated {
                    stage: AmbiguousStage::KcRoleMapping,
                    ref detail,
                } if detail.contains("500") && detail.contains("rm boom")
            ),
            "expected AmbiguousCreated{{ stage: KcRoleMapping, detail mentions 500/rm boom }}, got {err:?}",
        );
        assert!(
            mutator.put_calls.lock().is_empty(),
            "no OpenBao writes when KC role-mapping fails",
        );
    })
    .await;
}

/// All KC steps succeed; `OpenBao` `put` fails →
/// `AmbiguousCreated { stage: OpenBaoPutAfterKcSuccess, .. }`. This is
/// the saga's "point of no return" — KC has irreversible state and
/// reconciliation must `DELETE` the realm + defensive `OpenBao` delete
/// (PRD "Risks", orphaned-resource-after-lost-response row).
#[tokio::test]
async fn created_openbao_put_failure_returns_ambiguous_openbao_after_kc_success() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        let _ = mount_created_happy_path(&server, tenant_uuid, &realm, "secret-value").await;

        let mutator = Arc::new(ConfigurableStubOpenBao::with_put_failure(
            CredStoreError::service_unavailable("openbao down"),
        ));
        // put fails before the realm-admin token exchange (step 7), so no
        // get-response seed is needed.
        let facade = build_facade_with_cs_and_mutator(
            &server,
            Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>,
        );
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("openbao put failure must error");
        assert!(
            matches!(
                err,
                PluginError::AmbiguousCreated {
                    stage: AmbiguousStage::OpenBaoPutAfterKcSuccess,
                    ref detail,
                } if detail.contains("credstore") && detail.contains("openbao down")
            ),
            "expected AmbiguousCreated{{ stage: OpenBaoPutAfterKcSuccess }}, got {err:?}",
        );
        // The plugin DID attempt the put (recorded by the stub) even
        // though the response was an Err — that's the whole point of
        // "Ambiguous": the call may have landed.
        assert_eq!(
            mutator.put_calls.lock().len(),
            1,
            "the put was attempted exactly once before the failure",
        );
    })
    .await;
}

/// KC realm-create, client-create, role-mapping all succeed; the
/// `client-secret` GET returns 500 →
/// `AmbiguousCreated { stage: KcClientSecretRead, .. }`. KC has full
/// state at this point but the plugin cannot complete the `OpenBao`
/// write without the plaintext.
#[tokio::test]
async fn created_client_secret_read_failure_returns_ambiguous_kc_client_secret_read() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        // Manually mount the choreography up to (but excluding) the
        // client-secret read so we can override that step with 500.
        let tenant_uuid = Uuid::new_v4();
        let realm = format!("realm-{tenant_uuid}");
        mount_token_endpoints(&server, &realm).await;
        let admin_client_uuid = Uuid::new_v4();
        let sa_user_uuid = Uuid::new_v4();
        let rm_client_uuid = Uuid::new_v4();

        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}")))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/admin/realms"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/admin/realms/{realm}/clients")))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}/clients")))
            .and(query_param("clientId", "keycloak-idp-plugin-realm-admin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": admin_client_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/{realm}/clients/{admin_client_uuid}/service-account-user"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": sa_user_uuid.to_string()
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/admin/realms/{realm}/clients")))
            .and(query_param("clientId", "realm-management"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": rm_client_uuid.to_string()}
            ])))
            .mount(&server)
            .await;
        let role_ids: Vec<serde_json::Value> = REALM_ADMIN_REQUIRED_ROLES
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": Uuid::new_v4().to_string(),
                    "name": *n,
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/{realm}/users/{sa_user_uuid}/role-mappings/clients/{rm_client_uuid}/available"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Array(role_ids)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/admin/realms/{realm}/users/{sa_user_uuid}/role-mappings/clients/{rm_client_uuid}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        // Client-secret read fails with 500 — the test's whole point.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/{realm}/clients/{admin_client_uuid}/client-secret"
            )))
            .respond_with(ResponseTemplate::new(500).set_body_string("kc broke"))
            .mount(&server)
            .await;

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        let facade =
            build_facade_with_cs_and_mutator(&server, Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>);
        let ctx = build_system_ctx(Uuid::nil());
        let req = created_request(tenant_uuid);

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("client-secret read failure must error");
        assert!(
            matches!(
                err,
                PluginError::AmbiguousCreated {
                    stage: AmbiguousStage::KcClientSecretRead,
                    ref detail,
                } if detail.contains("500") && detail.contains("kc broke")
            ),
            "expected AmbiguousCreated{{ stage: KcClientSecretRead }}, got {err:?}",
        );
        assert!(
            mutator.put_calls.lock().is_empty(),
            "the saga never reached the OpenBao put",
        );
    })
    .await;
}

// ---------------------------------------------------------------------
// Phase 11 — Deprovision (DESIGN "Interactions & Sequences", "API Contracts",
// "Audit and Metrics Boundary", "Observability", "Domain Model")
// ---------------------------------------------------------------------

use account_management_sdk::idp::IdpDeprovisionTenantRequest;
use wiremock::matchers::method as wm_method;

/// Build an [`IdpDeprovisionTenantRequest`] with the given `tenant_id`
/// and persisted metadata blob. `tenant_name` and `tenant_type` are
/// stubbed — the facade doesn't read them.
fn deprovision_req(
    tenant_id: Uuid,
    metadata: Option<serde_json::Value>,
) -> IdpDeprovisionTenantRequest {
    IdpDeprovisionTenantRequest::new(IdpTenantContext::new(
        tenant_id,
        "stub-tenant",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
        metadata,
    ))
}

/// Build a [`SecurityContext`] whose `subject_type` is `"am.system"`
/// (mirrors AM's `for_module_init` / `for_bootstrap` shape). The
/// missing-metadata branch keys on this string per DESIGN "Observability" /
/// `keycloak_idp_plugin_deprovision_missing_metadata_total`.
fn build_am_system_ctx() -> Arc<SecurityContext> {
    let ctx = SecurityContext::builder()
        .subject_id(uuid::uuid!("00000000-0000-cf01-0000-616d73797374"))
        .subject_type("am.system")
        .subject_tenant_id(Uuid::nil())
        .build()
        .expect("am.system ctx builds");
    Arc::new(ctx)
}

/// Build a non-system [`SecurityContext`] (REST-forwarded user, etc.).
/// `subject_type = "rest"` — used to verify the missing-metadata branch
/// does NOT emit the system-actor warn for non-system callers.
fn build_non_system_ctx() -> Arc<SecurityContext> {
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("rest")
        .subject_tenant_id(Uuid::nil())
        .build()
        .expect("rest ctx builds");
    Arc::new(ctx)
}

/// Default-shared platform realm; happy path. The realm-admin secret
/// resolves via env (DESIGN "Domain Model" — `admin_secret_ref = None`), the
/// realm-admin DELETEs the tenant group with 204, and the facade
/// short-circuits without touching the bootstrap or `OpenBao` seams.
#[tokio::test]
async fn deprovision_shared_happy_path_returns_ok() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "platform").await;

            let tenant_uuid = Uuid::new_v4();
            let group_uuid = Uuid::new_v4();
            Mock::given(wm_method("DELETE"))
                .and(path(format!("/admin/realms/platform/groups/{group_uuid}")))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;

            let purge_hook = Arc::new(RecordingTenantAccountPurgeHook::default());
            let facade = build_facade_with_purge_hook(
                &server,
                Arc::clone(&purge_hook) as Arc<dyn TenantAccountPurgeHook>,
            );
            let ctx = build_system_ctx(Uuid::nil());
            let req = deprovision_req(
                tenant_uuid,
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    group_uuid,
                    None,
                )),
            );
            facade
                .deprovision_tenant_inner(&ctx, &req)
                .await
                .expect("deprovision ok");
            assert_eq!(
                purge_hook.calls(),
                vec![tenant_uuid],
                "tenant deprovision must invoke the mandatory credential-purge hook"
            );
        },
    )
    .await;
}

/// A purge failure is fail-closed: tenant teardown must not start while a
/// machine credential may still be live.
#[tokio::test]
async fn deprovision_stops_before_tenant_teardown_when_service_account_purge_fails() {
    let server = MockServer::start().await;
    mount_kc_health_probe_ok(&server).await;

    let tenant_uuid = Uuid::new_v4();
    let group_uuid = Uuid::new_v4();
    Mock::given(wm_method("DELETE"))
        .and(path(format!("/admin/realms/platform/groups/{group_uuid}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;

    let purge_hook = Arc::new(RecordingTenantAccountPurgeHook::with_outcome(Err(
        PluginError::KcRest {
            method: "DELETE",
            path_template: "/admin/realms/{realm}/clients/{uuid}".into(),
            status: KcStatusKind::Http(503),
            body_first_2kb: "unavailable".into(),
        },
    )));
    let facade = build_facade_with_purge_hook(
        &server,
        Arc::clone(&purge_hook) as Arc<dyn TenantAccountPurgeHook>,
    );
    let ctx = build_system_ctx(Uuid::nil());
    let req = deprovision_req(
        tenant_uuid,
        Some(make_metadata(
            "platform",
            RealmBinding::Shared,
            group_uuid,
            None,
        )),
    );

    let err = facade
        .deprovision_tenant_inner(&ctx, &req)
        .await
        .expect_err("purge failure must abort tenant teardown");

    assert!(
        matches!(err, PluginError::DeprovisionRetryable { .. }),
        "retryable purge failure must retain the tenant for a later retry: {err:?}"
    );
    assert_eq!(purge_hook.calls(), vec![tenant_uuid]);
}

/// Adopted realm `partner-acme`; happy path. The realm-admin secret
/// is in `OpenBao` under the templated key. After the realm-admin
/// DELETEs the tenant group, the facade returns OK without touching
/// the realm-delete endpoint (NOT mocked → would 404 if hit).
#[tokio::test]
async fn deprovision_adopted_happy_path_returns_ok() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-acme").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        Mock::given(wm_method("DELETE"))
            .and(path(format!(
                "/admin/realms/partner-acme/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let cs = Arc::new(ConfigurableStubCS::with_entry(
            "keycloak-idp-realm-admin-partner-acme-secret",
            "per-realm-ra-secret",
        ));
        let facade = build_facade_with_cs(&server, cs);
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_req(
            tenant_uuid,
            Some(make_metadata(
                "partner-acme",
                RealmBinding::Adopted,
                group_uuid,
                Some("keycloak-idp-realm-admin-partner-acme-secret"),
            )),
        );
        facade
            .deprovision_tenant_inner(&ctx, &req)
            .await
            .expect("deprovision ok");
    })
    .await;
}

/// Created realm `partner-created`; last-tenant teardown. After the
/// realm-admin DELETEs the tenant group with 204, the bootstrap admin
/// counts subgroups under `/tenants` → 0 → realm DELETE + `OpenBao`
/// secret DELETE.
#[tokio::test]
async fn deprovision_created_last_tenant_tears_down_realm_and_secret() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-created").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let parent_uuid = Uuid::new_v4();

        // realm-admin DELETE group → 204.
        Mock::given(wm_method("DELETE"))
            .and(path(format!(
                "/admin/realms/partner-created/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        // bootstrap admin: find parent /tenants → present.
        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-created/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": parent_uuid.to_string(), "name": "tenants"}
            ])))
            .mount(&server)
            .await;

        // bootstrap admin: count subgroups → 0.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/partner-created/groups/{parent_uuid}/children"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        // bootstrap admin: DELETE realm → 204.
        Mock::given(wm_method("DELETE"))
            .and(path("/admin/realms/partner-created"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        mutator.seed_get(
            "keycloak-idp-realm-admin-partner-created-secret",
            "per-realm-ra-secret",
        );
        let facade = build_facade_with_unified_client(
            &server,
            Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>,
        );
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_req(
            tenant_uuid,
            Some(make_metadata(
                "partner-created",
                RealmBinding::Created,
                group_uuid,
                Some("keycloak-idp-realm-admin-partner-created-secret"),
            )),
        );
        facade
            .deprovision_tenant_inner(&ctx, &req)
            .await
            .expect("deprovision ok");

        let deletes = mutator.delete_calls.lock();
        assert_eq!(
            deletes.len(),
            1,
            "Created last-tenant teardown MUST issue exactly one OpenBao delete",
        );
        assert_eq!(
            deletes[0], "keycloak-idp-realm-admin-partner-created-secret",
            "OpenBao delete MUST key on the templated realm-admin SecretRef",
        );
    })
    .await;
}

/// Created realm with one remaining tenant under `/tenants`. The
/// realm DELETE and `OpenBao` secret DELETE MUST NOT happen — the
/// realm stays live for the other tenant (DESIGN "Interactions & Sequences").
#[tokio::test]
async fn deprovision_created_with_remaining_tenants_keeps_realm() {
    let server = MockServer::start().await;
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async {
        mount_token_endpoints(&server, "partner-busy").await;

        let tenant_uuid = Uuid::new_v4();
        let group_uuid = Uuid::new_v4();
        let parent_uuid = Uuid::new_v4();
        let surviving_uuid = Uuid::new_v4();

        Mock::given(wm_method("DELETE"))
            .and(path(format!(
                "/admin/realms/partner-busy/groups/{group_uuid}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/admin/realms/partner-busy/groups"))
            .and(query_param("search", "tenants"))
            .and(query_param("exact", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": parent_uuid.to_string(), "name": "tenants"}
            ])))
            .mount(&server)
            .await;

        // One surviving sibling under /tenants — keep the realm.
        Mock::given(method("GET"))
            .and(path(format!(
                "/admin/realms/partner-busy/groups/{parent_uuid}/children"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": surviving_uuid.to_string(), "name": "other-tenant"}
            ])))
            .mount(&server)
            .await;

        // Realm DELETE intentionally NOT mocked — would 404 if hit.

        let mutator = Arc::new(ConfigurableStubOpenBao::default());
        mutator.seed_get(
            "keycloak-idp-realm-admin-partner-busy-secret",
            "per-realm-ra-secret",
        );
        let facade = build_facade_with_unified_client(
            &server,
            Arc::clone(&mutator) as Arc<dyn CredStoreClientV1>,
        );
        let ctx = build_system_ctx(Uuid::nil());
        let req = deprovision_req(
            tenant_uuid,
            Some(make_metadata(
                "partner-busy",
                RealmBinding::Created,
                group_uuid,
                Some("keycloak-idp-realm-admin-partner-busy-secret"),
            )),
        );
        facade
            .deprovision_tenant_inner(&ctx, &req)
            .await
            .expect("deprovision ok");

        assert!(
            mutator.delete_calls.lock().is_empty(),
            "OpenBao secret MUST stay when the realm is still occupied",
        );
    })
    .await;
}

/// Vendor 404 on the tenant group DELETE → `DeprovisionNotFound`
/// keyed on the metadata realm name (success-equivalent in AM's
/// hard-delete pipeline; Phase 15 maps to `IdpDeprovisionFailure::NotFound`).
#[tokio::test]
async fn deprovision_with_vendor_404_on_group_delete_returns_not_found() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "platform").await;

            let group_uuid = Uuid::new_v4();
            Mock::given(wm_method("DELETE"))
                .and(path(format!(
                    "/admin/realms/platform/groups/{group_uuid}"
                )))
                .respond_with(ResponseTemplate::new(404).set_body_string("gone"))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = deprovision_req(
                Uuid::new_v4(),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    group_uuid,
                    None,
                )),
            );
            let err = facade
                .deprovision_tenant_inner(&ctx, &req)
                .await
                .expect_err("404 on group delete must error");
            assert!(
                matches!(err, PluginError::DeprovisionNotFound { ref realm_name } if realm_name == "platform"),
                "expected DeprovisionNotFound{{ realm_name: platform }}, got {err:?}",
            );
        },
    )
    .await;
}

/// Vendor 410 on the tenant group DELETE → same `DeprovisionNotFound`
/// shape as the 404 case (DESIGN "Audit and Metrics Boundary" — both are "already absent").
#[tokio::test]
async fn deprovision_with_vendor_410_on_group_delete_returns_not_found() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "platform").await;

            let group_uuid = Uuid::new_v4();
            Mock::given(wm_method("DELETE"))
                .and(path(format!(
                    "/admin/realms/platform/groups/{group_uuid}"
                )))
                .respond_with(ResponseTemplate::new(410).set_body_string("gone-gone"))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = deprovision_req(
                Uuid::new_v4(),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    group_uuid,
                    None,
                )),
            );
            let err = facade
                .deprovision_tenant_inner(&ctx, &req)
                .await
                .expect_err("410 on group delete must error");
            assert!(
                matches!(err, PluginError::DeprovisionNotFound { ref realm_name } if realm_name == "platform"),
                "expected DeprovisionNotFound{{ realm_name: platform }}, got {err:?}",
            );
        },
    )
    .await;
}

/// Vendor 503 on the tenant group DELETE → `DeprovisionRetryable`
/// with the status code AND body text embedded in `detail` (so the
/// operator runbook can grep for "503"/body specifics; DESIGN "Audit and Metrics Boundary").
#[tokio::test]
async fn deprovision_5xx_on_group_delete_returns_retryable() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            mount_token_endpoints(&server, "platform").await;

            let group_uuid = Uuid::new_v4();
            Mock::given(wm_method("DELETE"))
                .and(path(format!("/admin/realms/platform/groups/{group_uuid}")))
                .respond_with(ResponseTemplate::new(503).set_body_string("kc-out"))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = deprovision_req(
                Uuid::new_v4(),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    group_uuid,
                    None,
                )),
            );
            let err = facade
                .deprovision_tenant_inner(&ctx, &req)
                .await
                .expect_err("503 on group delete must error");
            assert!(
                matches!(
                    err,
                    PluginError::DeprovisionRetryable { ref detail }
                        if detail.contains("503") && detail.contains("kc-out")
                ),
                "expected DeprovisionRetryable detail to mention 503 + body, got {err:?}",
            );
        },
    )
    .await;
}

/// Missing metadata + `am.system` ctx → `DeprovisionNotFound { realm_name = "unknown" }`.
/// Also exercises the `tracing::warn!` placeholder for the
/// `keycloak_idp_plugin_deprovision_missing_metadata_total` counter that Phase
/// 14 will wire to `OTel` (we can't easily assert the warn emission
/// without a tracing subscriber, but exercising the branch keeps it
/// reachable + clippy-clean).
#[tokio::test]
async fn deprovision_missing_metadata_am_system_returns_not_found() {
    let server = MockServer::start().await;
    // No HTTP / OpenBao mocks needed — the failure short-circuits
    // before any KC call (DESIGN "Interactions & Sequences" / "Audit and Metrics Boundary" missing-metadata branch).
    let facade = build_facade(&server);
    let ctx = build_am_system_ctx();
    let req = deprovision_req(Uuid::new_v4(), None);
    let err = facade
        .deprovision_tenant_inner(&ctx, &req)
        .await
        .expect_err("missing metadata must error");
    assert!(
        matches!(err, PluginError::DeprovisionNotFound { ref realm_name } if realm_name == "unknown"),
        "expected DeprovisionNotFound{{ realm_name: unknown }}, got {err:?}",
    );
}

/// Missing metadata + non-system ctx → `DeprovisionNotFound { realm_name = "unknown" }`
/// (same outer shape; the only difference vs the `am.system` case is
/// no warn is emitted — see DESIGN "Observability" on why only system-actor
/// callers contribute to the trigger metric).
#[tokio::test]
async fn deprovision_missing_metadata_non_system_returns_not_found() {
    let server = MockServer::start().await;
    let facade = build_facade(&server);
    let ctx = build_non_system_ctx();
    let req = deprovision_req(Uuid::new_v4(), None);
    let err = facade
        .deprovision_tenant_inner(&ctx, &req)
        .await
        .expect_err("missing metadata must error");
    assert!(
        matches!(err, PluginError::DeprovisionNotFound { ref realm_name } if realm_name == "unknown"),
        "expected DeprovisionNotFound{{ realm_name: unknown }}, got {err:?}",
    );
}

/// Malformed metadata blob (decodes as raw JSON but does not match
/// the `TenantIdpMetadataV1` shape — missing `version`) →
/// `DeprovisionTerminal { detail }`. Operator-must-intervene.
#[tokio::test]
async fn deprovision_with_malformed_metadata_returns_terminal() {
    let server = MockServer::start().await;
    let facade = build_facade(&server);
    let ctx = build_system_ctx(Uuid::nil());
    let req = deprovision_req(
        Uuid::new_v4(),
        Some(serde_json::json!({"this_is_not_a_valid_blob": true})),
    );
    let err = facade
        .deprovision_tenant_inner(&ctx, &req)
        .await
        .expect_err("malformed metadata must error");
    assert!(
        matches!(err, PluginError::DeprovisionTerminal { ref detail } if detail.contains("metadata decode failed")),
        "expected DeprovisionTerminal{{ detail mentions decode }}, got {err:?}",
    );
}

// ── PRD "Explicit Realm Binding": `realm_name` regex validation tests ────────────────────

/// Helper: a Root request carrying explicit `{mode: shared, realm_name}` — the
/// realm-regex validator runs on the explicit Root realm.
fn root_req_with_realm(realm: &str) -> IdpProvisionTenantRequest {
    IdpProvisionTenantRequest::for_root(
        Uuid::new_v4(),
        "platform-root",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({ "mode": "shared", "realm_name": realm }))
}

/// A `realm_name` containing a dot (`bad.name`) MUST be rejected with
/// `PluginError::ProvisionInputRejected` and the canonical detail string that
/// starts with `realm_name rejected`.
#[test]
fn parse_input_rejects_realm_name_with_dot() {
    let req = root_req_with_realm("bad.name");
    let err = TenantFacade::parse_input(&req).expect_err("must reject");
    let detail = match err {
        PluginError::ProvisionInputRejected { detail } => detail,
        #[allow(clippy::panic)] // test-only assertion; unreachable on correct implementation
        other => panic!("expected PluginError::ProvisionInputRejected, got {other:?}"),
    };
    assert!(
        detail.contains("realm_name rejected"),
        "detail must explain rejection, got: {detail}"
    );
}

/// A valid `realm_name` with letters, hyphens, and underscores MUST be
/// accepted and round-trip through `ParsedInput` unchanged.
#[test]
fn parse_input_accepts_realm_name_partner_acme_v2() {
    let req = root_req_with_realm("partner-acme_v2");
    let parsed = TenantFacade::parse_input(&req).expect("must accept");
    assert_eq!(parsed.realm_name, "partner-acme_v2");
    assert_eq!(parsed.realm_binding, RealmBinding::Shared);
}

/// A `realm_name` of 201 characters MUST be rejected (max is 200).
#[test]
fn parse_input_rejects_realm_name_over_200_chars() {
    let long = "a".repeat(201);
    let req = root_req_with_realm(&long);
    let err = TenantFacade::parse_input(&req).expect_err("must reject over-length");
    assert!(matches!(err, PluginError::ProvisionInputRejected { .. }));
}

/// An empty `realm_name` MUST be rejected (min length is 1). Root with an empty
/// explicit `realm_name` deserialises as `Some("")` and fails the regex.
#[test]
fn parse_input_rejects_empty_realm_name() {
    let req = root_req_with_realm("");
    let err = TenantFacade::parse_input(&req).expect_err("must reject empty");
    assert!(matches!(err, PluginError::ProvisionInputRejected { .. }));
}

/// DESIGN "Interactions & Sequences" — when the saga body takes longer than `provision_timeout_ms`,
/// `provision_tenant_inner` must return `AmbiguousCreated { stage: Timeout }`.
///
/// We set `provision_timeout_ms = 50ms` and wire the token endpoint to
/// delay 200ms, which is longer than the budget. The delay is injected at
/// the very first Keycloak call (bootstrap token exchange) so the saga
/// naturally stalls past the timeout without requiring KC admin mocks.
#[tokio::test]
async fn provision_tenant_returns_ambiguous_timeout_when_saga_exceeds_provision_timeout_ms() {
    use std::time::Duration;

    let server = wiremock::MockServer::start().await;
    mount_kc_health_probe_ok(&server).await;

    // Mount the master-realm token endpoint with a 200ms delay — longer than
    // our 50ms budget so the timeout fires before any saga step completes.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
            "/realms/master/protocol/openid-connect/token",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(serde_json::json!({
                    "access_token": "tok",
                    "expires_in": 600
                })),
        )
        .mount(&server)
        .await;

    // Build a facade with a 50ms timeout.
    let kc_cfg = keycloak_cfg(&server.uri());
    let realm_admin_cfg = kc_cfg.realm_admin.clone();
    let tenant_cfg = TenantFacadeConfig {
        realm_defaults: serde_json::Value::Null,
        tenant_group_root: "/tenants".into(),
        provision_timeout_ms: 50,
    };

    let client: Arc<dyn CredStoreClientV1> = Arc::new(StubCredStore);
    let cs_reader = crate::domain::credstore::CredStoreReader::new(
        Arc::clone(&client),
        crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil()),
    );
    let cs_writer = crate::domain::credstore::CredStoreWriter::new(
        client,
        crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil()),
    );
    let transport: Arc<dyn crate::domain::kc::transport::KcTransport> =
        Arc::new(ReqwestKcTransport::new(reqwest::Client::new()));
    let factory = Arc::new(crate::domain::kc::factory::KeycloakAdminClientFactory::new(
        kc_cfg,
        transport,
        cs_reader,
        Arc::new(crate::domain::test_support::NoopMetrics),
    ));
    let facade = TenantFacade::new(
        tenant_cfg,
        realm_admin_cfg,
        factory,
        Arc::new(crate::domain::metadata_codec::MetadataCodec),
        cs_writer,
        noop_tenant_metrics(),
        noop_credstore_metrics(),
        noop_metadata_metrics(),
        noop_failure_metrics(),
        noop_purge_hook(),
    );

    let ctx = crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil());
    let req = req_for_root(uuid::Uuid::nil());
    let result = facade.provision_tenant_inner(&ctx, &req).await;

    assert!(
        matches!(
            result,
            Err(PluginError::AmbiguousCreated {
                stage: AmbiguousStage::Timeout,
                ..
            })
        ),
        "expected AmbiguousCreated(Timeout), got: {result:?}"
    );
}

// ── the admin-bind scope note §A — admin_user_id bind tests ────────────────────────────────────

/// Mount the Shared-mode happy-path KC calls (realm probe + token endpoints +
/// parent group search + child group create) for `platform`. Returns
/// `(tenant_uuid, group_uuid, parent_uuid)`.
async fn mount_shared_happy_path(server: &MockServer) -> (Uuid, Uuid, Uuid) {
    mount_token_endpoints(server, "platform").await;

    Mock::given(method("GET"))
        .and(path("/admin/realms/platform"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(server)
        .await;

    let parent_uuid = Uuid::new_v4();
    Mock::given(method("GET"))
        .and(path("/admin/realms/platform/groups"))
        .and(query_param("search", "tenants"))
        .and(query_param("exact", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": parent_uuid.to_string(), "name": "tenants"}
        ])))
        .mount(server)
        .await;

    let tenant_uuid = Uuid::new_v4();
    let group_uuid = Uuid::new_v4();
    Mock::given(method("POST"))
        .and(path_regex(format!(
            r"^/admin/realms/platform/groups/{parent_uuid}/children$"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": group_uuid.to_string(),
            "name": tenant_uuid.to_string()
        })))
        .mount(server)
        .await;

    (tenant_uuid, group_uuid, parent_uuid)
}

/// Happy path: `admin_user_id` present → GET then PUT on the user endpoint.
/// Assert the PUT body contains `attributes.tenant_id` and
/// `attributes.user_type`, and that the returned metadata is unchanged.
#[tokio::test]
async fn provision_tenant_shared_binds_admin_user_when_metadata_has_user_id() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();
            let existing_user_json = serde_json::json!({
                "id": admin_user_id.to_string(),
                "username": "admin",
                "attributes": {}
            });

            // GET user → 200 with existing representation.
            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(existing_user_json.clone()))
                .mount(&server)
                .await;

            // PUT user → 204 (Keycloak returns 204 No Content on success).
            Mock::given(method("PUT"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            let meta = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("provision with admin_user_id ok");

            // Returned metadata is unchanged — same as non-bind Shared.
            assert_eq!(meta.realm_name, "platform");
            assert_eq!(meta.realm_binding, RealmBinding::Shared);
            assert!(meta.admin_secret_ref.is_none());
            assert_eq!(meta.tenant_group_id, group_uuid);

            // Verify the PUT was received by inspecting wiremock's received requests.
            let received = server.received_requests().await.unwrap();
            let put_req = received.iter().find(|r| {
                r.method.as_str() == "PUT"
                    && r.url.path().ends_with(&format!("/users/{admin_user_id}"))
            });
            assert!(put_req.is_some(), "PUT /users/{admin_user_id} must be sent");

            let put_body: serde_json::Value =
                serde_json::from_slice(&put_req.unwrap().body).expect("PUT body is JSON");
            let attrs = put_body
                .get("attributes")
                .expect("PUT body must have attributes");
            assert_eq!(
                attrs.get("tenant_id"),
                Some(&serde_json::json!([tenant_uuid.to_string()])),
                "attributes.tenant_id must be set to [request.tenant_id]"
            );
            assert_eq!(
                attrs.get("user_type"),
                Some(&serde_json::json!(["user"])),
                "attributes.user_type must be [\"user\"]"
            );
        },
    )
    .await;
}

/// No `admin_user_id` in metadata → NO GET/PUT on /users/ is issued.
/// Existing happy-path behavior is preserved.
#[tokio::test]
async fn provision_tenant_shared_no_bind_when_metadata_omits_user_id() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;

            // NOTE: No /users/ mock mounted — if the impl hits it, wiremock
            // returns 404 and the test would fail.

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared"
                // admin_user_id intentionally omitted
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            let meta = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("provision without admin_user_id ok");

            assert_eq!(meta.realm_binding, RealmBinding::Shared);
            assert_eq!(meta.tenant_group_id, group_uuid);

            // Assert no GET/PUT /users/ was issued.
            let received = server.received_requests().await.unwrap();
            let user_reqs: Vec<_> = received
                .iter()
                .filter(|r| r.url.path().contains("/users/"))
                .collect();
            assert!(
                user_reqs.is_empty(),
                "no /users/ requests expected when admin_user_id is absent, got: {user_reqs:?}"
            );
        },
    )
    .await;
}

/// GET /users/{id} returns 404 → `PluginError::AmbiguousCreated` tagged
/// `AdminUserBind`. The 404 happens AFTER `ensure_tenant_group` has
/// already created the per-tenant KC group, so the saga left a real
/// side-effect — surfacing this as `CleanFailure` would tell AM
/// "nothing happened, safe to retry clean" and leak an orphaned group
/// on every retry. Ambiguous triggers AM's reconciliation path.
#[tokio::test]
async fn provision_tenant_shared_bind_user_not_found_returns_ambiguous_admin_user_bind() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) =
                mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();

            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(
                    ResponseTemplate::new(404).set_body_string("User not found"),
                )
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata("platform", RealmBinding::Shared, Uuid::new_v4(), None)),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("404 on user GET must error");
            assert!(
                matches!(
                    err,
                    PluginError::AmbiguousCreated {
                        stage: AmbiguousStage::AdminUserBind,
                        ref detail,
                    } if detail.contains("404") && detail.contains("/users/")
                ),
                "expected AmbiguousCreated{{AdminUserBind}} (saga had already created the tenant group at step 3), got {err:?}"
            );
        },
    )
    .await;
}

/// GET → 200 valid user; PUT → 500 → `PluginError::AmbiguousCreated`
/// tagged `AdminUserBind` (the tenant group was already created at this
/// point, so the bind failure is a partial side-effect, not a clean
/// rejection).
#[tokio::test]
async fn provision_tenant_shared_bind_put_failure_returns_ambiguous_admin_user_bind() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) =
                mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();

            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin"
                })))
                .mount(&server)
                .await;

            Mock::given(method("PUT"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(
                    ResponseTemplate::new(500).set_body_string("kc boom"),
                )
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata("platform", RealmBinding::Shared, Uuid::new_v4(), None)),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("500 on user PUT must error");
            assert!(
                matches!(
                    err,
                    PluginError::AmbiguousCreated {
                        stage: AmbiguousStage::AdminUserBind,
                        ref detail,
                    } if detail.contains("PUT") && detail.contains("500") && detail.contains("kc boom")
                ),
                "expected AmbiguousCreated {{ AdminUserBind, PUT 500 body kc boom }}, got {err:?}"
            );
        },
    )
    .await;
}

/// GET returns user with existing `attributes.foo = ["bar"]`.
/// After bind, PUT body must still contain `foo` alongside new `tenant_id`
/// and `user_type`.
#[tokio::test]
async fn provision_tenant_shared_bind_preserves_existing_user_attributes() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();

            // User has an existing attribute `foo = ["bar"]`.
            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "foo": ["bar"]
                    }
                })))
                .mount(&server)
                .await;

            Mock::given(method("PUT"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("provision with existing attributes ok");

            let received = server.received_requests().await.unwrap();
            let put_req = received
                .iter()
                .find(|r| {
                    r.method.as_str() == "PUT"
                        && r.url.path().ends_with(&format!("/users/{admin_user_id}"))
                })
                .expect("PUT /users/{id} must be sent");
            let put_body: serde_json::Value =
                serde_json::from_slice(&put_req.body).expect("PUT body is JSON");
            let attrs = put_body
                .get("attributes")
                .expect("PUT body must have attributes");

            // Existing `foo` attribute must be preserved.
            assert_eq!(
                attrs.get("foo"),
                Some(&serde_json::json!(["bar"])),
                "existing attribute foo must be preserved"
            );
            // New attributes must be set.
            assert_eq!(
                attrs.get("tenant_id"),
                Some(&serde_json::json!([tenant_uuid.to_string()])),
            );
            assert_eq!(attrs.get("user_type"), Some(&serde_json::json!(["user"])),);
        },
    )
    .await;
}

/// Pure parser test: `admin_user_id` present as valid UUID string →
/// deserialises cleanly. Built as a Root request so the explicit `realm_name`
/// is honoured (shared mode permits `admin_user_id`).
#[test]
fn parse_input_accepts_metadata_with_admin_user_id() {
    let user_id = Uuid::new_v4();
    let req = IdpProvisionTenantRequest::for_root(
        Uuid::new_v4(),
        "platform-root",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({
        "mode": "shared",
        "realm_name": "platform",
        "admin_user_id": user_id.to_string()
    }));
    let parsed = TenantFacade::parse_input(&req).expect("must accept valid UUID");
    assert_eq!(parsed.realm_name, "platform");
    assert_eq!(parsed.realm_binding, RealmBinding::Shared);
    assert_eq!(parsed.admin_user_id, Some(user_id));
}

/// Pure parser test: `admin_user_id = "not-a-uuid"` → malformed wire shape →
/// `PluginError::ProvisionInputRejected` (detail mentions `malformed`).
#[test]
fn parse_input_rejects_malformed_admin_user_id() {
    let req = IdpProvisionTenantRequest::for_root(
        Uuid::new_v4(),
        "platform-root",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({
        "mode": "shared",
        "realm_name": "platform",
        "admin_user_id": "not-a-uuid"
    }));
    let err = TenantFacade::parse_input(&req).expect_err("must reject bad UUID");
    assert!(
        matches!(err, PluginError::ProvisionInputRejected { ref detail } if detail.contains("malformed")),
        "expected PluginError::ProvisionInputRejected(malformed ...), got {err:?}"
    );
}

/// Pure parser test: `admin_user_id = ""` (empty string) → decodes to `None`.
///
/// Reason: a deployment wiring this field through `${VAR}` expansion with a
/// POSIX default-empty leaves `""` when the env var is unset. The decoder must treat that as "no bind"
/// rather than fail the saga.
#[test]
fn parse_input_accepts_empty_admin_user_id_as_none() {
    let req = IdpProvisionTenantRequest::for_root(
        Uuid::new_v4(),
        "platform-root",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({
        "mode": "shared",
        "realm_name": "platform",
        "admin_user_id": ""
    }));
    let parsed = TenantFacade::parse_input(&req).expect("empty admin_user_id must decode as None");
    assert_eq!(parsed.admin_user_id, None);
}

/// Pure parser test: `admin_user_id` is only honoured in `mode=shared`.
/// Adopted-mode requests carrying the field MUST be rejected at parse
/// time so a silent drop doesn't mask operator misconfiguration.
#[test]
fn parse_input_rejects_admin_user_id_in_adopted_mode() {
    let req = IdpProvisionTenantRequest::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        "child-name",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({
        "mode": "adopted",
        "realm_name": "partner-acme",
        "admin_user_id": "550e8400-e29b-41d4-a716-446655440000"
    }));
    let err = TenantFacade::parse_input(&req)
        .expect_err("admin_user_id in adopted mode must be rejected");
    assert!(
        matches!(
            err,
            PluginError::ProvisionInputRejected { ref detail }
                if detail.contains("admin_user_id is only supported in mode=shared")
                    && detail.contains("adopted")
        ),
        "expected ProvisionInputRejected(admin_user_id only in shared, got mode=adopted), got {err:?}",
    );
}

/// Same as the adopted-mode case for `mode=created`.
#[test]
fn parse_input_rejects_admin_user_id_in_created_mode() {
    let req = IdpProvisionTenantRequest::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        "child-name",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({
        "mode": "created",
        "realm_name": "partner-acme",
        "admin_user_id": "550e8400-e29b-41d4-a716-446655440000"
    }));
    let err = TenantFacade::parse_input(&req)
        .expect_err("admin_user_id in created mode must be rejected");
    assert!(
        matches!(
            err,
            PluginError::ProvisionInputRejected { ref detail }
                if detail.contains("admin_user_id is only supported in mode=shared")
                    && detail.contains("created")
        ),
        "expected ProvisionInputRejected(admin_user_id only in shared, got mode=created), got {err:?}",
    );
}

/// DESIGN "Interactions & Sequences" — symmetric with the provision-timeout test above:
/// `deprovision_tenant_inner` wraps its saga in `tokio::time::timeout`
/// keyed on `provision_timeout_ms` and returns `DeprovisionRetryable`
/// on elapse. The bootstrap-admin token endpoint is wired with a
/// 200ms delay; 50ms budget guarantees the saga is cancelled at the
/// first KC hop.
#[tokio::test]
async fn deprovision_tenant_returns_retryable_when_saga_exceeds_provision_timeout_ms() {
    use std::time::Duration;

    let server = wiremock::MockServer::start().await;
    mount_kc_health_probe_ok(&server).await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
            "/realms/master/protocol/openid-connect/token",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(serde_json::json!({
                    "access_token": "tok",
                    "expires_in": 600
                })),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
            "/realms/platform/protocol/openid-connect/token",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(serde_json::json!({
                    "access_token": "tok",
                    "expires_in": 600
                })),
        )
        .mount(&server)
        .await;

    let kc_cfg = keycloak_cfg(&server.uri());
    let realm_admin_cfg = kc_cfg.realm_admin.clone();
    let tenant_cfg = TenantFacadeConfig {
        realm_defaults: serde_json::Value::Null,
        tenant_group_root: "/tenants".into(),
        provision_timeout_ms: 50,
    };

    let client: Arc<dyn CredStoreClientV1> = Arc::new(StubCredStore);
    let cs_reader = crate::domain::credstore::CredStoreReader::new(
        Arc::clone(&client),
        crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil()),
    );
    let cs_writer = crate::domain::credstore::CredStoreWriter::new(
        client,
        crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil()),
    );
    let transport: Arc<dyn crate::domain::kc::transport::KcTransport> =
        Arc::new(ReqwestKcTransport::new(reqwest::Client::new()));
    let factory = Arc::new(crate::domain::kc::factory::KeycloakAdminClientFactory::new(
        kc_cfg,
        transport,
        cs_reader,
        Arc::new(crate::domain::test_support::NoopMetrics),
    ));
    let facade = TenantFacade::new(
        tenant_cfg,
        realm_admin_cfg,
        factory,
        Arc::new(crate::domain::metadata_codec::MetadataCodec),
        cs_writer,
        noop_tenant_metrics(),
        noop_credstore_metrics(),
        noop_metadata_metrics(),
        noop_failure_metrics(),
        noop_purge_hook(),
    );

    let ctx = crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil());
    let tenant_id = uuid::Uuid::new_v4();
    let metadata = make_metadata(
        "platform",
        crate::domain::metadata_codec::RealmBinding::Shared,
        uuid::Uuid::new_v4(),
        None,
    );
    let req = deprovision_req(tenant_id, Some(metadata));
    let result = facade.deprovision_tenant_inner(&ctx, &req).await;

    assert!(
        matches!(
            result,
            Err(PluginError::DeprovisionRetryable { ref detail })
                if detail.contains("timed out")
                    && detail.contains("provision_timeout_ms")
        ),
        "expected DeprovisionRetryable{{timed out}}, got {result:?}",
    );
}

/// Created-mode provision with operator-supplied `realm_defaults`
/// containing a non-allowlisted key must fail with `PluginError::Config`
/// BEFORE the `POST /admin/realms` is issued. The allowlist check sits
/// at the top of `create_realm`; we wire only the preconditions
/// (token endpoint + realm-absent probe) and expect the saga to
/// short-circuit on the allowlist guard.
#[tokio::test]
async fn create_realm_rejects_non_allowlisted_realm_defaults_key() {
    let server = wiremock::MockServer::start().await;
    mount_kc_health_probe_ok(&server).await;

    // Bootstrap-admin token endpoint.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
            "/realms/master/protocol/openid-connect/token",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok",
                "expires_in": 600
            })),
        )
        .mount(&server)
        .await;
    // `assert_realm_absent` probe — 404 means the realm does not yet
    // exist, which is the precondition Created mode requires.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/admin/realms/new-tenant-realm"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // Build a facade whose tenant config carries an explicitly
    // non-allowlisted realm-default key (`smtpServer` — credential-
    // exfil-risk).
    let kc_cfg = keycloak_cfg(&server.uri());
    let realm_admin_cfg = kc_cfg.realm_admin.clone();
    let tenant_cfg = TenantFacadeConfig {
        realm_defaults: serde_json::json!({
            "smtpServer": {"host": "evil.example", "from": "no-reply@evil"}
        }),
        tenant_group_root: "/tenants".into(),
        provision_timeout_ms: 30_000,
    };

    let client: Arc<dyn CredStoreClientV1> = Arc::new(StubCredStore);
    let cs_reader = crate::domain::credstore::CredStoreReader::new(
        Arc::clone(&client),
        crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil()),
    );
    let cs_writer = crate::domain::credstore::CredStoreWriter::new(
        client,
        crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil()),
    );
    let transport: Arc<dyn crate::domain::kc::transport::KcTransport> =
        Arc::new(ReqwestKcTransport::new(reqwest::Client::new()));
    let factory = Arc::new(crate::domain::kc::factory::KeycloakAdminClientFactory::new(
        kc_cfg,
        transport,
        cs_reader,
        Arc::new(crate::domain::test_support::NoopMetrics),
    ));
    let facade = TenantFacade::new(
        tenant_cfg,
        realm_admin_cfg,
        factory,
        Arc::new(crate::domain::metadata_codec::MetadataCodec),
        cs_writer,
        noop_tenant_metrics(),
        noop_credstore_metrics(),
        noop_metadata_metrics(),
        noop_failure_metrics(),
        noop_purge_hook(),
    );

    let ctx = crate::domain::system_actor::build_system_ctx(uuid::Uuid::nil());
    let req = IdpProvisionTenantRequest::for_root(
        uuid::Uuid::new_v4(),
        "tenant-with-bad-defaults",
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
    )
    .with_metadata(serde_json::json!({
        "mode": "created",
        "realm_name": "new-tenant-realm"
    }));

    let result = facade.provision_tenant_inner(&ctx, &req).await;
    assert!(
        matches!(
            &result,
            Err(PluginError::Config { detail })
                if detail.contains("non-allowlisted key `smtpServer`")
        ),
        "expected Config(non-allowlisted key smtpServer), got {result:?}",
    );
}

/// Hijack guard: a `provision_tenant` POST that supplies an
/// `admin_user_id` pointing at a KC user already bound to a DIFFERENT
/// tenant must be rejected with `PluginError::AmbiguousCreated` tagged
/// `AdminUserBind`. Without the guard, an operator authorised to
/// provision tenants could silently transfer that user's
/// attribute-based authz to the new tenant.
///
/// Why `AmbiguousCreated` and not `Config`: the hijack check happens
/// AFTER `ensure_tenant_group` (`provision_shared` Step 3) has already
/// created the per-tenant KC group. A `Config` rejection would tell
/// AM "nothing happened, safe to retry clean" and leak an orphaned
/// group on every retry. The Ambiguous tag routes through AM's
/// reconciliation path.
#[tokio::test]
async fn provision_tenant_shared_bind_rejects_admin_user_already_bound_to_other_tenant() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) =
                mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();
            let foreign_tenant_id = Uuid::new_v4();

            // GET returns user already bound to a DIFFERENT tenant.
            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "tenant_id": [foreign_tenant_id.to_string()],
                        "user_type": ["user"]
                    }
                })))
                .mount(&server)
                .await;

            // PUT must NOT be sent. To distinguish "no PUT" from "PUT
            // returned 404 → AmbiguousCreated with body about 404", we
            // assert on the `detail` carrying the hijack-guard message
            // (which only the in-process guard can produce); a PUT-to-
            // wiremock-default-404 would surface a `detail` mentioning
            // `kc:PUT` and `404`.

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata("platform", RealmBinding::Shared, Uuid::new_v4(), None)),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("foreign tenant_id must be rejected");
            assert!(
                matches!(
                    err,
                    PluginError::AmbiguousCreated {
                        stage: AmbiguousStage::AdminUserBind,
                        ref detail,
                    } if detail.contains("already bound to a different tenant")
                        && detail.contains("hijack")
                ),
                "expected AmbiguousCreated{{AdminUserBind}} (hijack guard fires AFTER tenant group is created at step 3), got {err:?}",
            );
        },
    )
    .await;
}

/// Hijack guard counterpoint: idempotent re-bind (same admin user,
/// same tenant — bootstrap re-run scenario) MUST succeed without
/// `PluginError::Config`. Otherwise a re-bootstrap after a partial
/// AM-saga failure would falsely refuse to converge.
#[tokio::test]
async fn provision_tenant_shared_bind_accepts_admin_user_already_bound_to_same_tenant() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();

            // GET returns user already bound to THIS same tenant.
            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "tenant_id": [tenant_uuid.to_string()],
                        "user_type": ["user"]
                    }
                })))
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("re-bind with same tenant_id must be idempotent OK");
        },
    )
    .await;
}

/// Regression: previously, a multi-valued `attributes.tenant_id`
/// (e.g. `["A", "B"]`) where the requested tenant matched ANY element
/// passed the hijack guard, after which the PUT step unconditionally
/// wrote `[tenant_id]` — silently dropping the user's other tenant
/// binding(s). The plugin's v1 model is single-tenant-per-user
/// (`user_facade::provision_user` writes a singleton; PRD "Out of Scope"
/// defers `move_user_tenant`), so multi-valued attribute state is by
/// definition unexpected operator state. The guard now refuses to
/// guess which bindings are safe to drop and bails as
/// `AmbiguousCreated{AdminUserBind}` (Step 3 already created the
/// tenant group → reconciliation, not retry-clean). Substring
/// `multi-valued` is stable for log-grep and for distinguishing this
/// bail-class from the foreign-single-tenant hijack case.
#[tokio::test]
async fn provision_tenant_shared_bind_rejects_multi_valued_tenant_id_containing_target() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();
            let other_tenant_id = Uuid::new_v4();

            // GET returns user bound to a multi-valued tenant_id whose
            // first element is the REQUESTED tenant. The previous
            // guard's `.any()` check would pass and then drop
            // `other_tenant_id` on the PUT. The new guard refuses.
            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "tenant_id": [tenant_uuid.to_string(), other_tenant_id.to_string()],
                        "user_type": ["user"]
                    }
                })))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata("platform", RealmBinding::Shared, Uuid::new_v4(), None)),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("multi-valued tenant_id must be rejected even when target is present");
            assert!(
                matches!(
                    err,
                    PluginError::AmbiguousCreated {
                        stage: AmbiguousStage::AdminUserBind,
                        ref detail,
                    } if detail.contains("multi-valued")
                        && detail.contains("silently drop")
                ),
                "expected AmbiguousCreated{{AdminUserBind}} with multi-valued classification, got {err:?}",
            );

            // The PUT must NOT have happened — otherwise other_tenant_id
            // would be silently dropped on Keycloak's side.
            let received = server.received_requests().await.unwrap();
            let put_to_user = received.iter().find(|r| {
                r.method.as_str() == "PUT"
                    && r.url.path().ends_with(&format!("/users/{admin_user_id}"))
            });
            assert!(
                put_to_user.is_none(),
                "PUT /users/{admin_user_id} must NOT be issued - the displacement is the bug we're fixing"
            );
        },
    )
    .await;
}

/// Multi-valued `attributes.tenant_id` with NO match for the requested
/// tenant also bails — but classified as `multi-valued` (not `different
/// tenant`) so the operator's mental model of "user is unexpectedly
/// cross-tenant" matches the log message. Previously this case was
/// caught by the same `.any()`-returns-false branch as the
/// foreign-single-tenant case and reported as "different tenant", which
/// understated the underlying anomaly.
#[tokio::test]
async fn provision_tenant_shared_bind_rejects_multi_valued_tenant_id_all_foreign() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;

            let admin_user_id = Uuid::new_v4();
            let foreign_a = Uuid::new_v4();
            let foreign_b = Uuid::new_v4();

            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "tenant_id": [foreign_a.to_string(), foreign_b.to_string()],
                        "user_type": ["user"]
                    }
                })))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata("platform", RealmBinding::Shared, Uuid::new_v4(), None)),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("multi-valued tenant_id (all foreign) must be rejected");
            assert!(
                matches!(
                    err,
                    PluginError::AmbiguousCreated {
                        stage: AmbiguousStage::AdminUserBind,
                        ref detail,
                    } if detail.contains("multi-valued") && detail.contains("hijack")
                ),
                "expected AmbiguousCreated{{AdminUserBind}} with multi-valued classification, got {err:?}",
            );
        },
    )
    .await;
}

/// Empty array `attributes.tenant_id = []` is unexpected state — KC
/// normally drops attribute keys when their value list empties. The
/// guard treats it as a non-singleton (len != 1) → bail. Pinned so
/// future refactors don't quietly accept it.
#[tokio::test]
async fn provision_tenant_shared_bind_rejects_empty_array_tenant_id() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;
            let admin_user_id = Uuid::new_v4();

            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "tenant_id": [],
                        "user_type": ["user"]
                    }
                })))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("empty-array tenant_id must be rejected");
            assert!(
                matches!(
                    err,
                    PluginError::AmbiguousCreated {
                        stage: AmbiguousStage::AdminUserBind,
                        ..
                    }
                ),
                "expected AmbiguousCreated{{AdminUserBind}}, got {err:?}",
            );
        },
    )
    .await;
}

/// Legacy / hand-edited shape: bare string `attributes.tenant_id = "T"`
/// (not the canonical array). The plugin never writes this shape
/// (`user_facade::provision_user` and this function both write a
/// singleton array), but an operator may have set it manually. When
/// T matches the requested tenant, the guard accepts it as an
/// idempotent re-bind for backward-compat with hand-edited records.
/// Pinned because the new guard's stricter array-len check could
/// otherwise regress this case.
#[tokio::test]
async fn provision_tenant_shared_bind_accepts_bare_string_tenant_id_matching_target() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;
            let admin_user_id = Uuid::new_v4();

            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "tenant_id": tenant_uuid.to_string(),
                        "user_type": ["user"]
                    }
                })))
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(204))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("bare-string tenant_id matching target must be idempotent OK");
        },
    )
    .await;
}

/// Singleton array with a non-string element (e.g. `[42]`) — bizarre
/// operator state, but pin the rejection so a refactor that loosens
/// the `values[0].as_str() == Some(target)` check doesn't silently
/// start accepting it.
#[tokio::test]
async fn provision_tenant_shared_bind_rejects_singleton_array_with_non_string_element() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            let (tenant_uuid, _group_uuid, _parent_uuid) = mount_shared_happy_path(&server).await;
            let admin_user_id = Uuid::new_v4();

            Mock::given(method("GET"))
                .and(path(format!(
                    "/admin/realms/platform/users/{admin_user_id}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": admin_user_id.to_string(),
                    "username": "admin",
                    "attributes": {
                        "tenant_id": [42],
                        "user_type": ["user"]
                    }
                })))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::new(
                tenant_uuid,
                Uuid::new_v4(),
                "admin-name",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "admin_user_id": admin_user_id.to_string()
            }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
                Some(make_metadata(
                    "platform",
                    RealmBinding::Shared,
                    Uuid::new_v4(),
                    None,
                )),
            ));

            let err = facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect_err("non-string array element must be rejected");
            assert!(
                matches!(
                    err,
                    PluginError::AmbiguousCreated {
                        stage: AmbiguousStage::AdminUserBind,
                        ..
                    }
                ),
                "expected AmbiguousCreated{{AdminUserBind}}, got {err:?}",
            );
        },
    )
    .await;
}

// =====================================================================
// Pre-saga health_probe wiring (DESIGN "Component Model")
//
// The probe runs at the top of every `provision_tenant_inner` and at
// the post-decode step of every `deprovision_tenant_inner` so a
// flat-out KC outage short-circuits BEFORE any saga step touches KC.
// Provision maps to `CleanFailure` (no side effect yet — AM can retry
// clean). Deprovision maps to `Retryable` (KC down is transient by
// definition).
// =====================================================================

#[tokio::test]
async fn provision_tenant_bails_with_clean_failure_when_kc_unreachable_at_pre_saga_probe() {
    let server = MockServer::start().await;
    // No /realms/master mock — wiremock's default 404 simulates a KC
    // that's listening but not serving the master realm. With the
    // pre-saga health_probe wired, this should bail BEFORE saga work.
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async move {
        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let req = req_for_root(Uuid::new_v4());

        let err = facade
            .provision_tenant_inner(&ctx, &req)
            .await
            .expect_err("pre-saga KC outage must bail before saga work");
        assert!(
            matches!(
                err,
                PluginError::Config { ref detail }
                    if detail.contains("pre-saga KC health probe failed")
            ),
            "expected Config(pre-saga KC health probe failed ...), got {err:?}",
        );
    })
    .await;
}

#[tokio::test]
async fn deprovision_tenant_bails_with_retryable_when_kc_unreachable_at_pre_saga_probe() {
    let server = MockServer::start().await;
    // No /realms/master mock.
    temp_env::async_with_vars([("TEST_BOOTSTRAP_SECRET", Some("topsecret"))], async move {
        let facade = build_facade(&server);
        let ctx = build_system_ctx(Uuid::nil());
        let tenant_id = Uuid::new_v4();
        // Well-formed metadata so the deprovision saga gets past
        // the missing-metadata + decode branches and reaches the
        // post-decode health-probe gate (the only point at which
        // the probe is expected to fire on the deprovision path).
        let metadata = make_metadata("platform", RealmBinding::Shared, Uuid::new_v4(), None);
        let req = deprovision_req(tenant_id, Some(metadata));

        let err = facade
            .deprovision_tenant_inner(&ctx, &req)
            .await
            .expect_err("pre-saga KC outage must bail before saga work");
        assert!(
            matches!(
                err,
                PluginError::DeprovisionRetryable { ref detail }
                    if detail.contains("pre-saga KC health probe failed")
            ),
            "expected DeprovisionRetryable(pre-saga KC health probe failed ...), got {err:?}",
        );
    })
    .await;
}

// =====================================================================
// Reactive-401 wiring (DESIGN "Credentials, Token Cache, and Rotation")
//
// When KC rejects the cached realm-admin bearer with 401 (operator
// rotated the secret + OpenBao was updated; only the in-process
// token cache is stale), the per-realm Admin REST call inside the
// saga must re-acquire and retry exactly once. The end-to-end
// provision_tenant call has to succeed; the operator never sees the
// 401 surface as an AM-reaper bounce.
// =====================================================================

#[tokio::test]
async fn provision_tenant_recovers_from_realm_admin_rotation_via_reactive_401() {
    let server = MockServer::start().await;
    temp_env::async_with_vars(
        [
            ("TEST_BOOTSTRAP_SECRET", Some("topsecret")),
            ("TEST_REALM_ADMIN_SECRET", Some("ra")),
        ],
        async move {
            // Health probe + bootstrap-side work — all happy-path.
            mount_kc_health_probe_ok(&server).await;
            Mock::given(method("POST"))
                .and(path("/realms/master/protocol/openid-connect/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "t-master",
                    "expires_in": 600
                })))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/admin/realms/platform"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;

            // Realm-admin token endpoint — count how often we mint a
            // fresh bearer so the test can assert the wrapper actually
            // invalidated and re-acquired (rotation recovery proof).
            let realm_token_calls = Arc::new(AtomicUsize::new(0));
            let realm_token_clone = Arc::clone(&realm_token_calls);
            Mock::given(method("POST"))
                .and(path("/realms/platform/protocol/openid-connect/token"))
                .respond_with(move |_req: &wiremock::Request| {
                    realm_token_clone.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "access_token": "t-realm",
                        "expires_in": 600
                    }))
                })
                .mount(&server)
                .await;

            // Parent tenant group lookup (`find_top_group`, inside
            // `ensure_tenant_group` → `find_or_create_top_group`): 401
            // on the first attempt (simulating a stale bearer post-
            // rotation), 200 on the second.
            let parent_uuid = Uuid::new_v4();
            let lookup_calls = Arc::new(AtomicUsize::new(0));
            let lookup_clone = Arc::clone(&lookup_calls);
            Mock::given(method("GET"))
                .and(path("/admin/realms/platform/groups"))
                .and(query_param("search", "tenants"))
                .and(query_param("exact", "true"))
                .respond_with(move |_req: &wiremock::Request| {
                    let n = lookup_clone.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        ResponseTemplate::new(401).set_body_string("invalid_token")
                    } else {
                        ResponseTemplate::new(200).set_body_json(serde_json::json!([
                            { "id": parent_uuid.to_string(), "name": "tenants" }
                        ]))
                    }
                })
                .mount(&server)
                .await;

            // Child group create — happy path. This step only runs on
            // the SECOND attempt (after the wrapper retried the whole
            // closure with the fresh bearer).
            let tenant_uuid = Uuid::new_v4();
            let group_uuid = Uuid::new_v4();
            Mock::given(method("POST"))
                .and(path_regex(format!(
                    r"^/admin/realms/platform/groups/{parent_uuid}/children$"
                )))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": group_uuid.to_string(),
                    "name": tenant_uuid.to_string()
                })))
                .mount(&server)
                .await;

            let facade = build_facade(&server);
            let ctx = build_system_ctx(Uuid::nil());
            let req = IdpProvisionTenantRequest::for_root(
                tenant_uuid,
                "platform-root",
                GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~"),
            )
            .with_metadata(serde_json::json!({
                "mode": "shared",
                "realm_name": "platform"
            }));

            facade
                .provision_tenant_inner(&ctx, &req)
                .await
                .expect("reactive-401 retry must recover provision_tenant");

            // Two realm-admin token POSTs: initial cache-miss fetch +
            // post-401 re-acquire. If the wrapper hadn't invalidated,
            // this would be 1.
            assert_eq!(
                realm_token_calls.load(Ordering::SeqCst),
                2,
                "realm-admin token endpoint must be hit twice (initial + invalidate-refetch)",
            );
            // Two group-lookup GETs: the 401 attempt + the retry.
            assert_eq!(
                lookup_calls.load(Ordering::SeqCst),
                2,
                "find_top_group must be retried exactly once after the 401",
            );
        },
    )
    .await;
}

// ── effective-realm resolution in `parse_input` ───────────────────
//
// `parse_input` now takes the whole `IdpProvisionTenantRequest` and derives the
// effective realm from `target` + mode:
//   * Root                       → explicit `realm_name` (required, unchanged)
//   * Child + shared / no mode   → inherit parent realm from `parent_context`;
//                                  explicit `realm_name` ignored; no parent
//                                  metadata → `ProvisionInputRejected`
//   * Child + created            → generate `realm-<tenant_id>`; explicit
//                                  `realm_name` → `ProvisionInputRejected`
//   * Child + adopted            → explicit `realm_name` (required, unchanged)
mod effective_realm {
    use super::*;

    fn tt() -> GtsTypeId {
        GtsTypeId::new("gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~")
    }

    fn child(tenant_id: Uuid) -> IdpProvisionTenantRequest {
        IdpProvisionTenantRequest::new(tenant_id, Uuid::new_v4(), "child-name", tt())
    }

    fn parent_ctx_with_realm(realm: &str) -> IdpTenantContext {
        IdpTenantContext::new(
            Uuid::new_v4(),
            "parent",
            tt(),
            Some(make_metadata(
                realm,
                RealmBinding::Shared,
                Uuid::new_v4(),
                None,
            )),
        )
    }

    /// Shared child inherits the parent's realm from `parent_context`.
    #[test]
    fn shared_child_inherits_parent_realm() {
        let req = child(Uuid::new_v4())
            .with_metadata(serde_json::json!({ "mode": "shared" }))
            .with_parent_context(parent_ctx_with_realm("platform"));
        let parsed = TenantFacade::parse_input(&req).expect("shared child must inherit");
        assert_eq!(parsed.realm_binding, RealmBinding::Shared);
        assert_eq!(parsed.realm_name, "platform");
    }

    /// Mode absent defaults to shared and still inherits from the parent.
    #[test]
    fn mode_absent_defaults_to_shared_and_inherits() {
        let req = child(Uuid::new_v4()).with_parent_context(parent_ctx_with_realm("region-eu"));
        // No provisioning_metadata at all → mode defaults to shared.
        let parsed = TenantFacade::parse_input(&req).expect("absent mode must default to shared");
        assert_eq!(parsed.realm_binding, RealmBinding::Shared);
        assert_eq!(parsed.realm_name, "region-eu");
    }

    /// Shared child IGNORES an explicit `realm_name` and inherits anyway.
    #[test]
    fn shared_child_ignores_explicit_realm_name() {
        let req = child(Uuid::new_v4())
            .with_metadata(serde_json::json!({ "mode": "shared", "realm_name": "ignored" }))
            .with_parent_context(parent_ctx_with_realm("platform"));
        let parsed = TenantFacade::parse_input(&req).expect("shared child must inherit");
        assert_eq!(parsed.realm_name, "platform");
    }

    /// Created child generates `realm-<tenant_id>` and binds Created.
    #[test]
    fn created_child_generates_realm_name() {
        let id = Uuid::from_u128(1);
        let req = child(id).with_metadata(serde_json::json!({ "mode": "created" }));
        let parsed = TenantFacade::parse_input(&req).expect("created child must generate a realm");
        assert_eq!(parsed.realm_binding, RealmBinding::Created);
        assert_eq!(parsed.realm_name, format!("realm-{id}"));
    }

    /// Created child with an explicit `realm_name` is rejected (generated only).
    #[test]
    fn created_with_explicit_realm_rejected() {
        let req = child(Uuid::new_v4())
            .with_metadata(serde_json::json!({ "mode": "created", "realm_name": "foo" }));
        assert!(matches!(
            TenantFacade::parse_input(&req),
            Err(PluginError::ProvisionInputRejected { .. })
        ));
    }

    /// Shared child with no `parent_context` cannot inherit a realm → rejected.
    #[test]
    fn shared_child_no_parent_metadata_rejected() {
        let req = child(Uuid::new_v4()).with_metadata(serde_json::json!({ "mode": "shared" }));
        assert!(matches!(
            TenantFacade::parse_input(&req),
            Err(PluginError::ProvisionInputRejected { .. })
        ));
    }

    /// Shared child whose parent carries an undecodable metadata blob → rejected.
    #[test]
    fn shared_child_undecodable_parent_metadata_rejected() {
        let req = child(Uuid::new_v4())
            .with_metadata(serde_json::json!({ "mode": "shared" }))
            .with_parent_context(IdpTenantContext::new(
                Uuid::new_v4(),
                "parent",
                tt(),
                // No `version` key → MetadataCodec::decode errors.
                Some(serde_json::json!({ "realm_name": "platform" })),
            ));
        assert!(matches!(
            TenantFacade::parse_input(&req),
            Err(PluginError::ProvisionInputRejected { .. })
        ));
    }

    /// Root still requires an explicit `realm_name` (unchanged behaviour).
    #[test]
    fn root_requires_explicit_realm() {
        let req = IdpProvisionTenantRequest::for_root(Uuid::new_v4(), "platform-root", tt())
            .with_metadata(serde_json::json!({ "mode": "shared", "realm_name": "platform" }));
        let parsed = TenantFacade::parse_input(&req).expect("root must accept explicit realm");
        assert_eq!(parsed.realm_name, "platform");
        assert_eq!(parsed.realm_binding, RealmBinding::Shared);
    }

    /// Root with no `realm_name` in its metadata is rejected.
    #[test]
    fn root_without_realm_name_rejected() {
        let req = IdpProvisionTenantRequest::for_root(Uuid::new_v4(), "platform-root", tt())
            .with_metadata(serde_json::json!({ "mode": "shared" }));
        assert!(matches!(
            TenantFacade::parse_input(&req),
            Err(PluginError::ProvisionInputRejected { .. })
        ));
    }

    /// Adopted child still requires an explicit `realm_name` (unchanged).
    #[test]
    fn adopted_child_requires_explicit_realm() {
        let req = child(Uuid::new_v4())
            .with_metadata(serde_json::json!({ "mode": "adopted", "realm_name": "partner-acme" }));
        let parsed = TenantFacade::parse_input(&req).expect("adopted child must accept realm");
        assert_eq!(parsed.realm_binding, RealmBinding::Adopted);
        assert_eq!(parsed.realm_name, "partner-acme");
    }

    /// Adopted child with no `realm_name` is rejected.
    #[test]
    fn adopted_child_without_realm_rejected() {
        let req = child(Uuid::new_v4()).with_metadata(serde_json::json!({ "mode": "adopted" }));
        assert!(matches!(
            TenantFacade::parse_input(&req),
            Err(PluginError::ProvisionInputRejected { .. })
        ));
    }
}
