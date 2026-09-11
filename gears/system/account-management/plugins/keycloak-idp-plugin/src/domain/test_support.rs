//! Shared test fixtures for the domain layer's `*_tests.rs` siblings.
//!
//! Only compiled under `#[cfg(test)]`. Provides:
//!
//! - [`StubCredStore`] — no-op `CredStoreClientV1` (every `get` returns `None`).
//! - [`ConfigurableStubCS`] — keyed-map `CredStoreClientV1` for tests that
//!   need predetermined `get` responses.
//! - [`keycloak_cfg`]`(base_url)` — minimal valid `KeycloakConfig` builder.
//! - [`noop_metrics_*`]`()` typed-port accessors — one `Arc<dyn ...Port>`
//!   per facade-required port, all backed by `NoopMetrics`.
//! - [`make_metadata`]`(...)` — `serde_json::Value` blob simulating persisted `TenantIdpMetadataV1`.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{
    CredStoreClientV1, CredStoreError, Credential, CredentialListItem, CredentialPatch,
    CredentialWrite, PutOutcome, PutPrecondition, Secret, SecretRef, SecretValue, Validator,
    WritePrecondition,
};
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::RealmAdminConfig;
use crate::config::{
    BootstrapAdminConfig, HttpRetryPolicy, KeycloakConfig, KeycloakMetricsConfig, SecretFromEnv,
};
use crate::domain::metadata_codec::RealmBinding;
use crate::domain::ports::metrics::{
    CredstoreMetricsPort, CredstoreOp, CredstoreOutcome, EndpointClass, FailureMetricsPort,
    FailureVariant, KcAdminMetricsPort, MetadataCodecMetricsPort, OrphanCompensationOutcome,
    PluginOp, SaOp, SaOpMetricsPort, TenantLifecycleMetricsPort, TokenRefreshOutcome, TokenTier,
    UserOp, UserOpMetricsPort, VersionObserved,
};

/// No-op implementation of every metrics port. Lives here (under
/// `cfg(test)`-gated `test_support`) because that's the only call site
/// today; if a future caller wants a genuine production "metrics-off"
/// adapter, move the struct + impls back into `domain::ports::metrics`
/// and drop the `cfg(test)` gate.
#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl TenantLifecycleMetricsPort for NoopMetrics {
    fn provision_tenant_duration(&self, _realm_binding: RealmBinding, _secs: f64) {}
    fn realm_bound(&self, _realm_binding: RealmBinding, _realm_name: &str) {}
    fn realm_unbound(&self, _realm_binding: RealmBinding, _realm_name: &str) {}
    fn deprovision_missing_metadata(&self) {}
}

impl UserOpMetricsPort for NoopMetrics {
    fn user_op_duration(&self, _op: UserOp, _secs: f64) {}
    fn orphan_user_compensation(&self, _outcome: OrphanCompensationOutcome) {}
}

impl KcAdminMetricsPort for NoopMetrics {
    fn kc_admin_request_duration(&self, _endpoint_class: EndpointClass, _secs: f64) {}
    fn kc_admin_token_refresh(
        &self,
        _outcome: TokenRefreshOutcome,
        _tier: TokenTier,
        _realm: &str,
    ) {
    }
    fn credential_refresh(&self, _outcome: TokenRefreshOutcome, _tier: TokenTier, _realm: &str) {}
}

impl CredstoreMetricsPort for NoopMetrics {
    fn credstore_write(&self, _op: CredstoreOp, _outcome: CredstoreOutcome) {}
}

impl MetadataCodecMetricsPort for NoopMetrics {
    fn metadata_decode_failure(&self, _version_observed: VersionObserved) {}
}

impl FailureMetricsPort for NoopMetrics {
    fn failure(&self, _op: PluginOp, _variant: FailureVariant) {}
}

impl SaOpMetricsPort for NoopMetrics {
    fn sa_op_duration(&self, _op: SaOp, _secs: f64) {}
}

/// No-op `CredStoreClientV1` — every read returns `Ok(None)`.
///
/// Suitable for Shared-mode tests that never resolve a `SecretSource::OpenBaoRef`
/// (the bootstrap path uses the inline secret carried on the parsed config).
#[domain_model]
pub struct StubCredStore;

#[async_trait]
impl CredStoreClientV1 for StubCredStore {
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
        Ok(None)
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

/// Configurable stub keyed by `SecretRef`.
///
/// Adopted-mode tests seed a response for the per-realm `OpenBao` key so
/// `realm_client(...)` can resolve the secret via `get_secret`. `Secret` is
/// not `Clone` (contains `SecretValue`), so each entry is `take()`n on first
/// match — fine because the factory caches the resulting token.
#[domain_model]
#[derive(Default)]
pub struct ConfigurableStubCS {
    pub responses: Mutex<HashMap<String, Secret>>,
}

impl ConfigurableStubCS {
    #[must_use]
    pub fn with_entry(key: &str, value: &str) -> Self {
        let mut map = HashMap::new();
        map.insert(
            key.to_owned(),
            Secret {
                reference: SecretRef::new(key).expect("valid SecretRef"),
                secret_type: String::new(),
                expires_at: None,
                value: SecretValue::from(value),
                validator: Validator {
                    id: Uuid::nil(),
                    version: 1,
                },
            },
        );
        Self {
            responses: Mutex::new(map),
        }
    }
}

#[async_trait]
impl CredStoreClientV1 for ConfigurableStubCS {
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
        Ok(self.responses.lock().remove(key.as_ref()))
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

/// Build a `KeycloakConfig` pointing at `base` with canonical test defaults.
///
/// Used by `tenant_facade_tests` and `user_facade_tests`; the factory tests
/// use `cfg_with_base_url` which is an alias for this function under a
/// different name.
#[must_use]
pub fn keycloak_cfg(base: &str) -> KeycloakConfig {
    KeycloakConfig {
        base_url: url::Url::parse(base).expect("valid base url"),
        bootstrap_admin: BootstrapAdminConfig {
            realm: "master".into(),
            client_id: "keycloak-idp-plugin-bootstrap".into(),
            client_secret: SecretFromEnv::from_literal_unchecked("topsecret"),
        },
        realm_admin: RealmAdminConfig {
            client_id_default: "keycloak-idp-plugin-realm-admin".into(),
            default_shared_realm_secret: SecretFromEnv::from_literal_unchecked("ra"),
            secret_ref_template: "keycloak-idp-realm-admin-{realm_name}-secret".into(),
        },
        tls_ca_bundle_ref: None,
        admin_token_lifetime_safety_ms: 30_000,
        http_request_timeout_ms: 5_000,
        credential_refresh_interval: None,
        http_retry_policy: HttpRetryPolicy::default(),
        metrics: KeycloakMetricsConfig::default(),
        bootstrap_pre_warm: crate::config::BootstrapPreWarmConfig::default(),
    }
}

/// Typed-port `Arc<dyn ...>` views backed by [`NoopMetrics`] — the
/// safe default for facade tests that don't assert on metric emission.
/// Five accessors so a test can wire exactly the ports its facade
/// constructor expects without taking a dependency on the others.
#[must_use]
pub fn noop_tenant_metrics() -> Arc<dyn TenantLifecycleMetricsPort> {
    Arc::new(NoopMetrics)
}

#[must_use]
pub fn noop_user_metrics() -> Arc<dyn UserOpMetricsPort> {
    Arc::new(NoopMetrics)
}

#[must_use]
pub fn noop_credstore_metrics() -> Arc<dyn CredstoreMetricsPort> {
    Arc::new(NoopMetrics)
}

#[must_use]
pub fn noop_metadata_metrics() -> Arc<dyn MetadataCodecMetricsPort> {
    Arc::new(NoopMetrics)
}

#[must_use]
pub fn noop_failure_metrics() -> Arc<dyn FailureMetricsPort> {
    Arc::new(NoopMetrics)
}

#[must_use]
pub fn noop_sa_metrics() -> Arc<dyn SaOpMetricsPort> {
    Arc::new(NoopMetrics)
}

/// Build a `TenantIdpMetadataV1` JSON blob for use in test request fixtures.
///
/// The `admin_client_id` is always `"keycloak-idp-plugin-realm-admin"` (the
/// test-default from `keycloak_cfg`).
#[must_use]
pub fn make_metadata(
    realm: &str,
    binding: RealmBinding,
    tenant_group_id: Uuid,
    admin_secret_ref: Option<&str>,
) -> serde_json::Value {
    let binding_str = match binding {
        RealmBinding::Shared => "shared",
        RealmBinding::Adopted => "adopted",
        RealmBinding::Created => "created",
    };
    let mut obj = serde_json::json!({
        "version": "v1",
        "realm_name": realm,
        "realm_binding": binding_str,
        "tenant_group_id": tenant_group_id.to_string(),
        "admin_client_id": "keycloak-idp-plugin-realm-admin",
    });
    if let Some(secret) = admin_secret_ref {
        obj.as_object_mut().expect("json::Value is object").insert(
            "admin_secret_ref".into(),
            serde_json::Value::String(secret.into()),
        );
    }
    obj
}
