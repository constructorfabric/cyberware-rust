//! Unit tests for [`GtsSecretTypeResolver`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use toolkit_gts::gts_id;
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{GtsInstance, GtsTypeId, InstanceQuery, RegisterResult, TypeSchemaQuery};

use super::*;
use crate::domain::ports::metrics::NoopMetrics;
use credstore_sdk::SharingMode;

/// Deterministic v5 UUID of a GTS type id, via the SDK helper.
fn uuid_of(gts_id: &str) -> Uuid {
    credstore_sdk::types::type_uuid(gts_id).expect("valid gts id")
}

/// The credstore secret base type as registered by the SDK: abstract (a pure
/// trait/schema carrier — secrets are always typed by a derived id), yet still
/// carrying the generic trait *values* — the chain-merge inherited defaults
/// (leaf wins, base fills the rest) — so every descendant that omits a trait
/// inherits the complete generic value for it.
fn secret_envelope() -> Arc<GtsTypeSchema> {
    let raw = json!({
        "$id": format!("gts://{CREDENTIAL_RESOURCE_TYPE}"),
        "type": "object",
        "x-gts-abstract": true,
        "x-gts-traits": {
            "allow_sharing": ["private", "tenant", "shared"],
            "expirable": false,
            "utf8_only": false,
        },
    });
    Arc::new(
        GtsTypeSchema::try_new(GtsTypeId::new(CREDENTIAL_RESOURCE_TYPE), raw, None, None)
            .expect("envelope schema"),
    )
}

/// A derived secret-type schema declaring its own `x-gts-traits`.
fn derived(type_id: &str, traits: &serde_json::Value) -> GtsTypeSchema {
    let raw = json!({
        "$id": format!("gts://{type_id}"),
        "type": "object",
        "allOf": [{"$ref": format!("gts://{CREDENTIAL_RESOURCE_TYPE}")}],
        "x-gts-traits": traits,
    });
    GtsTypeSchema::try_new(GtsTypeId::new(type_id), raw, None, Some(secret_envelope()))
        .expect("derived schema")
}

fn resolver(registry: Arc<dyn TypesRegistryClient>) -> GtsSecretTypeResolver {
    GtsSecretTypeResolver::new(registry, Arc::new(NoopMetrics))
}

const CUSTOM_TYPE_ID: &str =
    gts_id!("cf.core.credstore.credential.v1~acme.connectors.creds.db_password.v1~");

#[tokio::test]
async fn resolves_gts_id_and_effective_traits() {
    let schema = derived(
        CUSTOM_TYPE_ID,
        &json!({
            "allow_sharing": ["private"],
            "expirable": true,
            "max_size_bytes": 8192,
            "value_schema": {"type": "object", "required": ["password"]},
        }),
    );
    let registry = Arc::new(MockTypesRegistryClient::new().with_type_schemas([schema]));
    let resolved = resolver(registry)
        .resolve(uuid_of(CUSTOM_TYPE_ID))
        .await
        .expect("registered custom type resolves");

    assert_eq!(resolved.gts_id, CUSTOM_TYPE_ID);
    assert!(resolved.traits.allows_sharing(SharingMode::Private));
    assert!(!resolved.traits.allows_sharing(SharingMode::Tenant));
    assert!(resolved.traits.expirable);
    assert_eq!(resolved.traits.max_size_bytes, Some(8192));
    // Undeclared traits inherit the base type's generic values.
    assert!(!resolved.traits.utf8_only);
    assert_eq!(
        resolved.traits.value_schema,
        Some(json!({"type": "object", "required": ["password"]}))
    );
}

/// The base type is abstract (`x-gts-abstract`) — a trait carrier, never a
/// concrete secret type. The types-registry SDK does not surface
/// abstractness, so the resolver rejects the base by its known id, with the
/// same `UNKNOWN_SECRET_TYPE` violation as a foreign type.
#[tokio::test]
async fn the_abstract_envelope_itself_is_rejected_as_a_secret_type() {
    let envelope = (*secret_envelope()).clone();
    let registry = Arc::new(MockTypesRegistryClient::new().with_type_schemas([envelope]));
    let err = resolver(registry)
        .resolve(uuid_of(CREDENTIAL_RESOURCE_TYPE))
        .await
        .expect_err("abstract base type must not type a secret");
    assert!(
        matches!(
            err,
            DomainError::TypeViolation {
                reason: reasons::UNKNOWN_SECRET_TYPE,
                ..
            }
        ),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn unknown_uuid_is_an_unknown_secret_type_violation() {
    let registry = Arc::new(MockTypesRegistryClient::new());
    let err = resolver(registry)
        .resolve(Uuid::from_u128(0xDEAD))
        .await
        .expect_err("unregistered uuid must reject");
    assert!(
        matches!(
            err,
            DomainError::TypeViolation {
                reason: reasons::UNKNOWN_SECRET_TYPE,
                ..
            }
        ),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn non_secret_type_is_an_unknown_secret_type_violation() {
    // Registered and resolvable, but rooted outside the credstore secret
    // envelope — must not be admitted as a secret type even though its
    // traits could deserialize by luck.
    let alien_root = Arc::new(
        GtsTypeSchema::try_new(
            GtsTypeId::new(gts_id!("acme.core.events.type.v1~")),
            json!({"type": "object"}),
            None,
            None,
        )
        .expect("alien root"),
    );
    let alien_id = gts_id!("acme.core.events.type.v1~acme.commerce.orders.order.v1~");
    let alien = GtsTypeSchema::try_new(
        GtsTypeId::new(alien_id),
        json!({"type": "object"}),
        None,
        Some(alien_root),
    )
    .expect("alien leaf");
    let registry = Arc::new(MockTypesRegistryClient::new().with_type_schemas([alien]));
    let err = resolver(registry)
        .resolve(uuid_of(alien_id))
        .await
        .expect_err("non-secret type must reject");
    assert!(
        matches!(
            err,
            DomainError::TypeViolation {
                reason: reasons::UNKNOWN_SECRET_TYPE,
                ..
            }
        ),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("does not descend"), "got: {err}");
}

#[tokio::test]
async fn malformed_effective_traits_fail_closed_as_service_unavailable() {
    // `allow_sharing` declared with a value outside the SharingMode enum:
    // the registry would reject this at registration, so hitting it at
    // resolve time means the registered chain is broken — 503, not 400.
    let schema = derived(CUSTOM_TYPE_ID, &json!({"allow_sharing": ["everyone"]}));
    let registry = Arc::new(MockTypesRegistryClient::new().with_type_schemas([schema]));
    let err = resolver(registry)
        .resolve(uuid_of(CUSTOM_TYPE_ID))
        .await
        .expect_err("malformed traits must fail closed");
    assert!(
        matches!(err, DomainError::ServiceUnavailable { .. }),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("malformed"), "got: {err}");
}

// -------- transport / infra --------

/// Minimal fake for the transport-failure and timeout paths the stateful
/// SDK mock cannot express. Only `get_type_schema_by_uuid` is reachable.
struct FailingRegistry {
    error: CanonicalError,
    delay: Option<Duration>,
    calls: Mutex<u32>,
}

impl FailingRegistry {
    fn new(error: CanonicalError) -> Self {
        Self {
            error,
            delay: None,
            calls: Mutex::new(0),
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

#[async_trait]
impl TypesRegistryClient for FailingRegistry {
    async fn register(
        &self,
        _entities: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unreachable!()
    }
    async fn register_type_schemas(
        &self,
        _type_schemas: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unreachable!()
    }
    async fn get_type_schema(&self, _type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        unreachable!()
    }
    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        *self.calls.lock().expect("lock") += 1;
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        Err(self.error.clone())
    }
    async fn get_type_schemas(
        &self,
        _type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        unreachable!()
    }
    async fn get_type_schemas_by_uuid(
        &self,
        _type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        unreachable!("resolver uses the single-key variant")
    }
    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        unreachable!()
    }
    async fn register_instances(
        &self,
        _instances: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unreachable!()
    }
    async fn get_instance(&self, _id: &str) -> Result<GtsInstance, CanonicalError> {
        unreachable!()
    }
    async fn get_instance_by_uuid(&self, _uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        unreachable!()
    }
    async fn get_instances(
        &self,
        _ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        unreachable!()
    }
    async fn get_instances_by_uuid(
        &self,
        _uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        unreachable!()
    }
    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        unreachable!()
    }
}

#[tokio::test]
async fn registry_transport_error_maps_to_service_unavailable() {
    let registry = Arc::new(FailingRegistry::new(types_registry_sdk::testing::internal(
        "registry exploded",
    )));
    let err = resolver(registry)
        .resolve(Uuid::from_u128(0x1))
        .await
        .expect_err("transport error must propagate as 503");
    assert!(
        matches!(err, DomainError::ServiceUnavailable { .. }),
        "got: {err:?}"
    );
    // Curated wire detail; the raw registry error stays in the cause chain.
    assert_eq!(
        err.to_string(),
        "service unavailable: types-registry unavailable"
    );
}

#[tokio::test(start_paused = true)]
async fn slow_registry_times_out_as_service_unavailable() {
    let registry = Arc::new(
        FailingRegistry::new(types_registry_sdk::testing::internal("never reached"))
            .with_delay(Duration::from_millis(50)),
    );
    let resolver = GtsSecretTypeResolver::with_timeout(registry.clone(), Arc::new(NoopMetrics), 10);
    let err = resolver
        .resolve(Uuid::from_u128(0x1))
        .await
        .expect_err("slow registry must time out");
    assert!(
        matches!(err, DomainError::ServiceUnavailable { .. }),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("timeout exceeded"), "got: {err}");
    assert_eq!(*registry.calls.lock().expect("lock"), 1);
}
