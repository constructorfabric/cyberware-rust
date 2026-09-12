#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use gts::{GtsInstanceId, GtsTypeId};
use quota_enforcement_sdk::{
    METRIC_BASE_TYPE, REQUEST_BASE, SCOPE_TYPE, SCOPE_USER, SUBJECT_BASE, owned_definitions,
};
use serde_json::{Value, json};
use toolkit_canonical_errors::CanonicalError;

use super::TypesRegistryContracts;
use crate::domain::error::DomainError;
use crate::domain::ports::contracts::ContractRegistry;
use crate::domain::ports::metrics::ValidationReason;
use crate::test_support::{
    LLM_TOKEN_REQUEST, LLM_USER_PROJECTION, METRIC_OTHER, METRIC_TOKENS, MIXIN_REQUEST, MIXIN_TYPE,
    in_process_registry, llm_gateway_documents, metric_base_documents, mixin_request_document,
    mixin_type_document, mock_registry,
};

fn adapter(mock: types_registry_sdk::testing::MockTypesRegistryClient) -> TypesRegistryContracts {
    TypesRegistryContracts::new(Arc::new(mock)).with_deadline(Duration::from_secs(2))
}

fn type_id(raw: &str) -> GtsTypeId {
    GtsTypeId::try_new(raw).expect("type id")
}

/// True when any `$ref` remains anywhere in `value`.
fn has_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.contains_key("$ref") || map.values().any(has_ref),
        Value::Array(items) => items.iter().any(has_ref),
        _ => false,
    }
}

#[tokio::test]
async fn a_derived_contract_resolves_with_its_traits_dialect_and_inlined_parent() {
    let registry = adapter(mock_registry(&llm_gateway_documents(), &[]));
    let user = registry
        .type_schema(&type_id(LLM_USER_PROJECTION))
        .await
        .expect("registry answers")
        .expect("the projection is registered");
    assert!(!user.is_abstract);
    assert_eq!(user.ancestors, vec![type_id(SUBJECT_BASE)]);
    assert!(user.derives_from(SUBJECT_BASE));
    assert_eq!(user.effective_traits["scope"], SCOPE_USER);
    assert_eq!(user.schema["$schema"], crate::test_support::DRAFT7);
    assert!(user.schema["$id"].is_string(), "the id is kept");
    assert!(
        !has_ref(&user.schema),
        "the parent is inlined: {}",
        user.schema
    );

    let base = registry
        .type_schema(&type_id(SUBJECT_BASE))
        .await
        .expect("registry answers")
        .expect("the base is registered");
    assert!(base.is_abstract);
    assert!(base.ancestors.is_empty());
}

#[tokio::test]
async fn local_and_mixin_references_are_resolved_transitively() {
    let documents = vec![mixin_type_document(), mixin_request_document(METRIC_TOKENS)];
    let registry = adapter(mock_registry(&documents, &[]));
    let contract = registry
        .type_schema(&type_id(MIXIN_REQUEST))
        .await
        .expect("registry answers")
        .expect("registered");
    assert!(!has_ref(&contract.schema), "{}", contract.schema);
    assert_eq!(contract.effective_traits["metric"], METRIC_TOKENS);

    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .build(&contract.schema)
        .expect("the resolved contract compiles");
    let good = json!({ "type": MIXIN_REQUEST, "metadata": { "region": "eu", "when": "2026-09-08T10:00:00Z" } });
    assert!(validator.is_valid(&good));
    let bad_region = json!({ "type": MIXIN_REQUEST, "metadata": { "region": "mars" } });
    assert!(!validator.is_valid(&bad_region), "the mixin's enum applies");
    let bad_time =
        json!({ "type": MIXIN_REQUEST, "metadata": { "region": "eu", "when": "yesterday" } });
    assert!(
        !validator.is_valid(&bad_time),
        "the local definition's date-time format is asserted under Draft-07"
    );
}

#[tokio::test]
async fn a_missing_mixin_target_is_catalogue_drift() {
    let registry = adapter(mock_registry(&[mixin_request_document(METRIC_TOKENS)], &[]));
    let err = registry
        .type_schema(&type_id(MIXIN_REQUEST))
        .await
        .expect_err("the mixin is not registered");
    match err {
        DomainError::TypesRegistryUnavailable(reason) => {
            assert!(reason.contains(MIXIN_TYPE), "{reason}");
        }
        other => panic!("expected drift, got {other:?}"),
    }
}

#[tokio::test]
async fn discovery_lists_derived_types_from_the_listing_and_follows_no_reference() {
    // The mixin contract's target is absent: resolution would fail, discovery
    // must not even try.
    let mut documents = llm_gateway_documents();
    documents.push(mixin_request_document(METRIC_OTHER));
    let registry = adapter(mock_registry(&documents, &[]));
    let discovered = registry
        .derived_types(&type_id(REQUEST_BASE))
        .await
        .expect("listing succeeds");
    let ids: Vec<&str> = discovered.iter().map(|d| d.id.as_ref()).collect();
    assert!(ids.contains(&LLM_TOKEN_REQUEST), "{ids:?}");
    assert!(ids.contains(&MIXIN_REQUEST), "{ids:?}");
    assert!(
        !ids.contains(&REQUEST_BASE),
        "the base is not derived from itself"
    );
    assert!(
        !ids.iter().any(|id| id.starts_with(SUBJECT_BASE)),
        "the mock ignores patterns; ancestry filtering keeps subjects out: {ids:?}"
    );
    let mixin = discovered
        .iter()
        .find(|d| d.id.as_ref() == MIXIN_REQUEST)
        .expect("listed");
    assert!(!mixin.is_abstract);
    assert_eq!(mixin.declared_traits["metric"], METRIC_OTHER);
}

#[tokio::test]
async fn missing_types_and_instances_are_none_and_transport_failures_lift() {
    let registry = adapter(mock_registry(
        &llm_gateway_documents(),
        &metric_base_documents()[1..],
    ));
    assert!(
        registry
            .type_schema(&type_id(
                "gts.cf.core.qe.subj.v1~cf.nobody.nothing.absent.v1~"
            ))
            .await
            .expect("answers")
            .is_none()
    );
    let metric = GtsInstanceId::try_new(METRIC_TOKENS).expect("instance id");
    assert_eq!(
        registry.instance_type(&metric).await.expect("answers"),
        Some(type_id(METRIC_BASE_TYPE))
    );
    let unknown = GtsInstanceId::try_new(METRIC_OTHER).expect("instance id");
    assert_eq!(
        registry.instance_type(&unknown).await.expect("answers"),
        None
    );

    let failing = adapter(
        types_registry_sdk::testing::MockTypesRegistryClient::new()
            .with_list_error(CanonicalError::internal("registry down").create()),
    );
    let err = failing
        .derived_types(&type_id(REQUEST_BASE))
        .await
        .expect_err("listing fails");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn ensure_registered_is_idempotent_and_a_divergent_definition_is_a_conflict() {
    let client = in_process_registry(Vec::new());
    let registry = TypesRegistryContracts::new(client.clone());
    let definitions = owned_definitions().expect("definitions");

    // The inventory already seeded these; asserting them again is a no-op.
    registry
        .ensure_registered(&definitions)
        .await
        .expect("byte-identical definitions register idempotently");
    registry
        .ensure_registered(&definitions)
        .await
        .expect("and again");

    let mut divergent = definitions
        .iter()
        .find(|d| d.id == SCOPE_TYPE)
        .cloned()
        .expect("scope type");
    divergent.document["description"] = Value::String("changed".to_owned());
    let err = registry
        .ensure_registered(&[divergent])
        .await
        .expect_err("the same id with other content is a conflict");
    assert_eq!(
        err,
        DomainError::CatalogInvalid {
            reason: ValidationReason::DefinitionConflict,
            subject: SCOPE_TYPE.to_owned(),
        }
    );

    // The real registry resolves the QE bases it was seeded with.
    let scope = registry
        .type_schema(&type_id(SCOPE_TYPE))
        .await
        .expect("answers")
        .expect("seeded");
    assert!(!scope.is_abstract);
}
