#![allow(clippy::expect_used)]

use std::sync::Arc;

use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use gts::GtsTypeId;
use quota_enforcement_sdk::{
    EvaluationAttribution, MetricId, ResourceProjection, SCOPE_TENANT, SCOPE_USER, SubjectClaim,
    SubjectRef, TenantId,
};
use serde_json::{Map, Value, json};
use toolkit_security::pep_properties;
use uuid::Uuid;

use super::{AdmittedEvaluation, Attribution};
use crate::domain::admission::Admission;
use crate::domain::catalog::{CatalogBuilder, CatalogConfig, ProjectionContractCatalog};
use crate::domain::error::DomainError;
use crate::domain::pep::{actions, properties, resources};
use crate::domain::ports::metrics::{DenialReason, ValidationReason, ValidationSurface};
use crate::domain::tokens;
use crate::test_support::{
    DenyAllPdp, FakeContractRegistry, LLM_MODEL_RESOURCE, LLM_TENANT_PROJECTION, LLM_TOKEN_REQUEST,
    LLM_USER_PROJECTION, METRIC_OTHER, METRIC_REQUESTS, METRIC_TOKENS, PermitTenantsPdp,
    RecordingMetrics, TupleMatchingPdp, ctx, tenant,
};

const GROUP_SCOPE: &str = "gts.cf.core.qe.scope.v1~cf.core.qe.group.v1";

fn type_id(raw: &str) -> GtsTypeId {
    GtsTypeId::try_new(raw).expect("type id")
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

/// The `llm_gateway` catalogue with both scopes and the model resource.
async fn catalog(registry: &FakeContractRegistry) -> ProjectionContractCatalog {
    let metrics = RecordingMetrics::default();
    CatalogBuilder::new(registry, &metrics)
        .build(&CatalogConfig {
            subject_projections: vec![type_id(LLM_USER_PROJECTION), type_id(LLM_TENANT_PROJECTION)],
            resource_projections: vec![type_id(LLM_MODEL_RESOURCE)],
        })
        .await
        .expect("catalogue")
}

struct Harness {
    catalog: ProjectionContractCatalog,
    admission: Admission,
    metrics: Arc<RecordingMetrics>,
}

impl Harness {
    async fn with_pdp(pdp: Arc<dyn AuthZResolverApi>) -> Self {
        let registry = FakeContractRegistry::llm_gateway();
        let metrics = Arc::new(RecordingMetrics::default());
        Self {
            catalog: catalog(&registry).await,
            admission: Admission::new(PolicyEnforcer::new(pdp), metrics.clone()),
            metrics,
        }
    }

    async fn admit(
        &self,
        attribution: EvaluationAttribution,
    ) -> Result<AdmittedEvaluation, DomainError> {
        Attribution::new(&self.admission, &self.catalog, self.metrics.as_ref())
            .admit_evaluation(&ctx(), &resources::OPERATION, actions::DEBIT, attribution)
            .await
    }
}

fn user(id: &str) -> SubjectClaim {
    SubjectClaim {
        kind: SCOPE_USER.to_owned(),
        id: id.to_owned(),
    }
}

/// A conforming token debit for the fixture tenant and user `u-1`.
fn request() -> EvaluationAttribution {
    EvaluationAttribution {
        tenant_id: tenant(),
        metric: METRIC_TOKENS.to_owned(),
        subjects: vec![user("u-1")],
        metadata: Some(object(&json!({ "region": "eu-west-1" }))),
        resource: None,
    }
}

fn permitting() -> Arc<PermitTenantsPdp> {
    Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()]))
}

fn invalid(field: &'static str, reason: &'static str) -> DomainError {
    DomainError::InvalidArgument { field, reason }
}

#[tokio::test]
async fn a_conforming_request_maps_tenant_and_user_to_the_owner_projections() {
    let pdp = permitting();
    let h = Harness::with_pdp(pdp.clone()).await;
    let admitted = h.admit(request()).await.expect("admitted");

    assert_eq!(
        admitted.attribution.subjects,
        vec![
            SubjectRef {
                projection_type: type_id(LLM_TENANT_PROJECTION),
                subject_id: tenant().to_string(),
            },
            SubjectRef {
                projection_type: type_id(LLM_USER_PROJECTION),
                subject_id: "u-1".to_owned(),
            },
        ],
        "tenant scope materialized first, then the mapped claim; no caller-selected projection"
    );
    assert_eq!(admitted.attribution.tenant_id, tenant());
    assert!(
        admitted
            .attribution
            .access_scope
            .contains_uuid(pep_properties::OWNER_TENANT_ID, tenant().as_uuid())
    );
    assert_eq!(
        admitted.metric,
        MetricId::parse(METRIC_TOKENS).expect("metric")
    );
    assert_eq!(
        admitted.input.request,
        object(&json!({ "region": "eu-west-1" }))
    );
    assert_eq!(admitted.input.resource, None);
    assert_eq!(pdp.calls(), 1, "exactly one PDP round trip");
    assert!(h.metrics.denials().is_empty());
    assert!(h.metrics.contract_failures().is_empty());
    assert!(h.metrics.admitted_violations().is_empty());
}

#[tokio::test]
async fn the_pdp_receives_the_complete_tuple_with_the_target_tenant() {
    let pdp = permitting();
    let h = Harness::with_pdp(pdp.clone()).await;
    let mut with_resource = request();
    with_resource.resource = Some(ResourceProjection {
        r#type: LLM_MODEL_RESOURCE.to_owned(),
        id: Some("model-7".to_owned()),
        metadata: Some(object(&json!({ "model_family": "gpt" }))),
    });
    h.admit(with_resource).await.expect("admitted");

    let resource = pdp.last_resource().expect("the PDP saw a resource");
    assert_eq!(
        resource.properties.get(pep_properties::OWNER_TENANT_ID),
        Some(&Value::String(tenant().as_uuid().to_string()))
    );
    assert_eq!(
        resource.properties.get(properties::METRIC),
        Some(&Value::String(METRIC_TOKENS.to_owned()))
    );
    assert_eq!(
        resource.properties.get(properties::SUBJECTS),
        Some(&json!([{ "kind": SCOPE_USER, "id": "u-1" }]))
    );
    assert_eq!(
        resource.properties.get(properties::RESOURCE),
        Some(&json!({
            "type": LLM_MODEL_RESOURCE,
            "id": "model-7",
            "metadata": { "model_family": "gpt" }
        }))
    );
}

#[tokio::test]
async fn malformed_shape_is_rejected_before_the_pdp_with_a_stable_token() {
    let pdp = permitting();
    let h = Harness::with_pdp(pdp.clone()).await;

    let cases: Vec<(&str, EvaluationAttribution, DomainError)> = vec![
        (
            "nil tenant",
            EvaluationAttribution {
                tenant_id: TenantId::new(Uuid::nil()),
                ..request()
            },
            invalid("tenant_id", tokens::TENANT_ID_REQUIRED),
        ),
        (
            "metric under another base",
            EvaluationAttribution {
                metric: SCOPE_USER.to_owned(),
                ..request()
            },
            invalid("metric", tokens::METRIC_INVALID),
        ),
        (
            "malformed metric",
            EvaluationAttribution {
                metric: "tokens".to_owned(),
                ..request()
            },
            invalid("metric", tokens::METRIC_INVALID),
        ),
        (
            "empty subject id",
            EvaluationAttribution {
                subjects: vec![user("  ")],
                ..request()
            },
            invalid("subjects", tokens::SUBJECT_ID_REQUIRED),
        ),
        (
            "kind is not a scope instance",
            EvaluationAttribution {
                subjects: vec![SubjectClaim {
                    kind: METRIC_TOKENS.to_owned(),
                    id: "u-1".to_owned(),
                }],
                ..request()
            },
            invalid("subjects", tokens::SUBJECT_KIND_INVALID),
        ),
        (
            "tenant scope repeated",
            EvaluationAttribution {
                subjects: vec![SubjectClaim {
                    kind: SCOPE_TENANT.to_owned(),
                    id: "t".to_owned(),
                }],
                ..request()
            },
            invalid("subjects", tokens::TENANT_SCOPE_REPEATED),
        ),
        (
            "duplicate kind",
            EvaluationAttribution {
                subjects: vec![user("u-1"), user("u-2")],
                ..request()
            },
            invalid("subjects", tokens::SUBJECT_KIND_DUPLICATE),
        ),
        (
            "metadata absent",
            EvaluationAttribution {
                metadata: None,
                ..request()
            },
            invalid("metadata", tokens::METADATA_REQUIRED),
        ),
        (
            "resource type malformed",
            EvaluationAttribution {
                resource: Some(ResourceProjection {
                    r#type: "model".to_owned(),
                    id: None,
                    metadata: Some(Map::new()),
                }),
                ..request()
            },
            invalid("resource.type", tokens::RESOURCE_TYPE_INVALID),
        ),
        (
            "resource metadata absent",
            EvaluationAttribution {
                resource: Some(ResourceProjection {
                    r#type: LLM_MODEL_RESOURCE.to_owned(),
                    id: None,
                    metadata: None,
                }),
                ..request()
            },
            invalid("resource.metadata", tokens::RESOURCE_METADATA_REQUIRED),
        ),
    ];
    let expected_count = cases.len();
    for (name, attribution, expected) in cases {
        let err = h.admit(attribution).await.expect_err(name);
        assert_eq!(err, expected, "{name}");
    }
    assert_eq!(pdp.calls(), 0, "shape checks run before the PDP");
    assert_eq!(
        h.metrics.denials(),
        vec![DenialReason::InvalidArgument; expected_count]
    );
    let failures = h.metrics.contract_failures();
    assert_eq!(failures.len(), expected_count);
    assert!(
        failures
            .iter()
            .all(|(surface, _)| *surface == ValidationSurface::CallerAttribution)
    );
    assert_eq!(
        failures
            .iter()
            .filter(|(_, reason)| *reason == ValidationReason::MetadataMissing)
            .count(),
        2,
        "the two absent-metadata cases"
    );
    assert!(h.metrics.admitted_violations().is_empty());
}

#[tokio::test]
async fn a_pdp_denial_precedes_any_catalogue_lookup() {
    let h = Harness::with_pdp(Arc::new(DenyAllPdp)).await;
    // The kind is unknown to the catalogue; the PDP answers first.
    let unknown_kind = EvaluationAttribution {
        subjects: vec![SubjectClaim {
            kind: GROUP_SCOPE.to_owned(),
            id: "g-1".to_owned(),
        }],
        ..request()
    };
    let err = h.admit(unknown_kind).await.expect_err("denied");
    assert!(matches!(err, DomainError::PdpDenied { .. }), "{err:?}");
    assert!(
        h.metrics.admitted_violations().is_empty(),
        "no catalogue lookup ran"
    );
    assert_eq!(h.metrics.denials(), vec![DenialReason::PermissionDenied]);
}

#[tokio::test]
async fn an_unauthorized_tuple_is_denied_before_evaluation() {
    let expected = object(&json!({
        pep_properties::OWNER_TENANT_ID: tenant().as_uuid().to_string(),
        properties::METRIC: METRIC_TOKENS,
        properties::SUBJECTS: [{ "kind": SCOPE_USER, "id": "u-1" }],
    }));
    let pdp = Arc::new(TupleMatchingPdp::new(expected, vec![tenant().as_uuid()]));
    let h = Harness::with_pdp(pdp.clone()).await;

    h.admit(request()).await.expect("the authorized tuple");

    let other_subject = EvaluationAttribution {
        subjects: vec![user("u-2")],
        ..request()
    };
    assert!(matches!(
        h.admit(other_subject).await,
        Err(DomainError::PdpDenied { .. })
    ));
    let other_metric = EvaluationAttribution {
        metric: METRIC_REQUESTS.to_owned(),
        metadata: Some(Map::new()),
        ..request()
    };
    assert!(matches!(
        h.admit(other_metric).await,
        Err(DomainError::PdpDenied { .. })
    ));
    let other_tenant = EvaluationAttribution {
        tenant_id: TenantId::new(Uuid::from_u128(0xbad)),
        ..request()
    };
    assert!(matches!(
        h.admit(other_tenant).await,
        Err(DomainError::PdpDenied { .. })
    ));
    assert_eq!(pdp.calls(), 4);
    assert!(h.metrics.admitted_violations().is_empty());
}

#[tokio::test]
async fn unknown_and_unadmitted_kinds_are_rejected_after_authorization() {
    let pdp = permitting();
    let h = Harness::with_pdp(pdp.clone()).await;

    let unknown_kind = EvaluationAttribution {
        subjects: vec![SubjectClaim {
            kind: GROUP_SCOPE.to_owned(),
            id: "g-1".to_owned(),
        }],
        ..request()
    };
    assert_eq!(
        h.admit(unknown_kind).await,
        Err(invalid("subjects", tokens::SUBJECT_KIND_NOT_ADMITTED))
    );
    let unadmitted_metric = EvaluationAttribution {
        metric: METRIC_OTHER.to_owned(),
        ..request()
    };
    assert_eq!(
        h.admit(unadmitted_metric).await,
        Err(invalid("metric", tokens::METRIC_NOT_ADMITTED))
    );
    assert_eq!(
        pdp.calls(),
        2,
        "the PDP authorized both before the catalogue"
    );
    assert_eq!(
        h.metrics.admitted_violations(),
        vec![
            ValidationSurface::RequestSubject,
            ValidationSurface::RequestSubject
        ]
    );
    assert!(h.metrics.contract_failures().is_empty());
}

#[tokio::test]
async fn metadata_is_validated_as_the_contract_envelope() {
    let h = Harness::with_pdp(permitting()).await;

    let empty_is_fine_for_request_count = EvaluationAttribution {
        metric: METRIC_REQUESTS.to_owned(),
        metadata: Some(Map::new()),
        ..request()
    };
    h.admit(empty_is_fine_for_request_count)
        .await
        .expect("the request_count contract declares no properties; {} conforms");

    let token_without_region = EvaluationAttribution {
        metadata: Some(Map::new()),
        ..request()
    };
    assert_eq!(
        h.admit(token_without_region).await,
        Err(invalid("metadata", tokens::CONTRACT_VIOLATION))
    );
    let token_with_extra = EvaluationAttribution {
        metadata: Some(object(&json!({ "region": "eu", "temperature": 0.7 }))),
        ..request()
    };
    assert_eq!(
        h.admit(token_with_extra).await,
        Err(invalid("metadata", tokens::CONTRACT_VIOLATION))
    );
    assert_eq!(
        h.metrics.contract_failures(),
        vec![
            (
                ValidationSurface::RequestSubject,
                ValidationReason::SchemaViolation
            ),
            (
                ValidationSurface::RequestSubject,
                ValidationReason::SchemaViolation
            ),
        ]
    );
}

#[tokio::test]
async fn the_resource_projection_is_validated_and_an_absent_id_stays_absent() {
    let h = Harness::with_pdp(permitting()).await;
    let with_resource = |projection: ResourceProjection| EvaluationAttribution {
        resource: Some(projection),
        ..request()
    };

    let without_id = ResourceProjection {
        r#type: LLM_MODEL_RESOURCE.to_owned(),
        id: None,
        metadata: Some(object(&json!({ "model_family": "gpt" }))),
    };
    let admitted = h
        .admit(with_resource(without_id.clone()))
        .await
        .expect("the resource base allows an omitted id");
    assert_eq!(admitted.input.resource, Some(without_id));

    let with_id = ResourceProjection {
        r#type: LLM_MODEL_RESOURCE.to_owned(),
        id: Some("model-7".to_owned()),
        metadata: Some(object(&json!({ "model_family": "gpt" }))),
    };
    h.admit(with_resource(with_id)).await.expect("with id");

    let missing_family = ResourceProjection {
        r#type: LLM_MODEL_RESOURCE.to_owned(),
        id: None,
        metadata: Some(Map::new()),
    };
    assert_eq!(
        h.admit(with_resource(missing_family)).await,
        Err(invalid("resource", tokens::CONTRACT_VIOLATION))
    );
    let not_a_resource = ResourceProjection {
        r#type: LLM_TOKEN_REQUEST.to_owned(),
        id: None,
        metadata: Some(Map::new()),
    };
    assert_eq!(
        h.admit(with_resource(not_a_resource)).await,
        Err(invalid("resource.type", tokens::RESOURCE_TYPE_UNKNOWN))
    );
    assert_eq!(
        h.metrics.contract_failures(),
        vec![
            (
                ValidationSurface::RequestResource,
                ValidationReason::SchemaViolation
            ),
            (
                ValidationSurface::RequestResource,
                ValidationReason::ProjectionNotResolvable
            ),
        ]
    );
}

#[tokio::test]
async fn the_ingress_path_makes_no_registry_call() {
    let registry = FakeContractRegistry::llm_gateway();
    let catalog = catalog(&registry).await;
    let reads_after_bootstrap = registry.read_calls();
    registry.fail_all();

    let metrics = Arc::new(RecordingMetrics::default());
    let admission = Admission::new(PolicyEnforcer::new(permitting()), metrics.clone());
    Attribution::new(&admission, &catalog, metrics.as_ref())
        .admit_evaluation(&ctx(), &resources::OPERATION, actions::DEBIT, request())
        .await
        .expect("the catalogue answers with the registry down");
    assert_eq!(registry.read_calls(), reads_after_bootstrap);
}
