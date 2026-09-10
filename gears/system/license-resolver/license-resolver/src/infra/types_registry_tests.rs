//! Unit tests for the classification of registry failures.
//!
//! The three-way split is the whole point of the adapter: a caller's typo, an
//! unregistered contract, and an outage must not be reported as each other.

use std::collections::HashMap;

use serde_json::Value;
use types_registry_sdk::testing::{self, MockTypesRegistryClient};
use types_registry_sdk::{GtsInstance, InstanceQuery, RegisterResult, TypeSchemaQuery};
use uuid::Uuid;

use super::*;
use crate::domain::test_support::{USER_SUBJECT, user_subject_schema};

/// Types-registry client whose `get_type_schema` fails with a chosen canonical
/// error; the adapter calls nothing else, so every other method is unreachable.
///
/// Needed because the stock mock only synthesizes `NotFound` and
/// `InvalidArgument`, and the adapter's third arm — everything else is an
/// outage — has to be exercised too.
struct FailingRegistry {
    error: CanonicalError,
}

#[async_trait]
impl TypesRegistryClient for FailingRegistry {
    async fn get_type_schema(&self, _type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        Err(self.error.clone())
    }

    async fn register(&self, _entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        unreachable!()
    }
    async fn register_type_schemas(
        &self,
        _type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        unreachable!()
    }
    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        unreachable!()
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
        unreachable!()
    }
    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        unreachable!()
    }
    async fn register_instances(
        &self,
        _instances: Vec<Value>,
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

fn hub_with(registry: Arc<dyn TypesRegistryClient>) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::default());
    hub.register::<dyn TypesRegistryClient>(registry);
    hub
}

#[tokio::test]
async fn a_registered_contract_resolves_with_its_chain() {
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_type_schemas([user_subject_schema()]));
    let adapter = GtsContractRegistry::new(hub_with(registry));

    let contract = adapter
        .contract_schema(USER_SUBJECT)
        .await
        .expect("a registered contract resolves");
    assert_eq!(contract.type_id.as_ref(), USER_SUBJECT);
    assert_eq!(
        contract.ancestors().count(),
        2,
        "the derivation chain must be linked so origin checks can walk it"
    );
}

#[tokio::test]
async fn an_absent_contract_is_unregistered() {
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
    let adapter = GtsContractRegistry::new(hub_with(registry));

    let err = adapter
        .contract_schema(USER_SUBJECT)
        .await
        .expect_err("nothing is registered");
    assert!(
        matches!(err, ContractRegistryError::Unregistered),
        "expected Unregistered, got: {err:?}"
    );
}

/// A malformed id is the caller's mistake. Folding it into the outage arm would
/// tell an operator the registry is down because a caller sent a typo.
#[tokio::test]
async fn a_malformed_type_id_is_the_callers_fault_not_an_outage() {
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
    let adapter = GtsContractRegistry::new(hub_with(registry));

    for bad in ["", "not-a-gts-id", "gts.cf.core.lic.subj.v1"] {
        let err = match adapter.contract_schema(bad).await {
            Err(err) => err,
            Ok(schema) => panic!("'{bad}' must not resolve, got {}", schema.type_id),
        };
        assert!(
            matches!(err, ContractRegistryError::MalformedTypeId(_)),
            "'{bad}' must be reported as malformed, got: {err:?}"
        );
    }
}

#[tokio::test]
async fn any_other_registry_failure_is_an_outage() {
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(FailingRegistry {
        error: testing::internal("connection reset"),
    });
    let adapter = GtsContractRegistry::new(hub_with(registry));

    let err = adapter
        .contract_schema(USER_SUBJECT)
        .await
        .expect_err("an internal registry failure must not resolve");
    assert!(
        matches!(err, ContractRegistryError::Unavailable(_)),
        "expected Unavailable, got: {err:?}"
    );
}

/// The client is looked up per call, so a hub that has no types-registry yet is
/// an outage rather than a panic or a bad request.
#[tokio::test]
async fn a_hub_without_a_registry_is_an_outage() {
    let adapter = GtsContractRegistry::new(Arc::new(ClientHub::default()));
    let err = adapter
        .contract_schema(USER_SUBJECT)
        .await
        .expect_err("no client means no answer");
    assert!(
        matches!(err, ContractRegistryError::Unavailable(_)),
        "expected Unavailable, got: {err:?}"
    );
}

/// Guards the assumption the adapter rests on: the stock mock mirrors the real
/// client's classification of a bad id.
#[test]
fn the_mock_and_the_adapter_agree_on_what_a_bad_id_looks_like() {
    let err = testing::invalid_gts_id("does not end with `~`");
    assert!(
        matches!(err, CanonicalError::InvalidArgument { .. }),
        "a malformed id must arrive as InvalidArgument, got: {err:?}"
    );
}
