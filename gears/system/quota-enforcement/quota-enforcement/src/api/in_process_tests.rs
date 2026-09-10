#![allow(clippy::expect_used)]

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use gts::GtsTypeId;
use quota_enforcement_sdk::testing::InMemoryStorage;
use quota_enforcement_sdk::{
    EnforcementMode, MetricId, PageRequest, PeriodType, QuotaFilter, QuotaManagerClientV1,
    QuotaPatch, QuotaSource, QuotaSpec, QuotaStatus, QuotaType, SubjectRef,
};
use serde_json::json;
use toolkit_canonical_errors::{CanonicalError, Problem};

use super::InProcessQuotaManager;
use crate::domain::Service;
use crate::domain::admission::Admission;
use crate::domain::bootstrap::Bound;
use crate::domain::catalog::{CatalogBuilder, CatalogConfig};
use crate::domain::quotas::QuotaLimits;
use crate::domain::readiness::Readiness;
use crate::test_support::{
    FakeContractRegistry, FakeMetricRegistry, LLM_TENANT_PROJECTION, LLM_USER_PROJECTION,
    METRIC_TOKENS, NoopCoordinator, PermitTenantsPdp, RecordingMetrics, ctx, tenant,
};

fn type_id(raw: &str) -> GtsTypeId {
    GtsTypeId::try_new(raw).expect("type id")
}

fn spec() -> QuotaSpec {
    QuotaSpec {
        tenant_id: tenant(),
        subject: SubjectRef {
            projection_type: type_id(LLM_USER_PROJECTION),
            subject_id: "u1".to_owned(),
        },
        metric: MetricId::parse(METRIC_TOKENS).expect("metric"),
        quota_type: QuotaType::Consumption,
        period: Some(PeriodType::Month),
        enforcement_mode: EnforcementMode::Hard,
        cap: Some(100),
        notification_thresholds: Vec::new(),
        validity_window: None,
        fail_open_hint: false,
        metadata: json!({ "regions": ["eu"], "weight": 5 })
            .as_object()
            .cloned()
            .expect("object"),
        source: QuotaSource::Operator,
    }
}

fn unbound_service() -> Arc<Service> {
    let metrics = Arc::new(RecordingMetrics::default());
    let enforcer = PolicyEnforcer::new(Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])));
    Arc::new(Service::new(
        Admission::new(enforcer, metrics),
        Arc::new(Readiness::new()),
        QuotaLimits {
            metadata_max_bytes: 4096,
            list_max_limit: 500,
            list_max_ids: 100,
        },
    ))
}

async fn bound_service() -> Arc<Service> {
    let service = unbound_service();
    let registry = Arc::new(FakeContractRegistry::llm_gateway());
    let metrics = RecordingMetrics::default();
    let catalog = CatalogBuilder::new(registry.as_ref(), &metrics)
        .build(&CatalogConfig {
            subject_projections: vec![type_id(LLM_USER_PROJECTION), type_id(LLM_TENANT_PROJECTION)],
            resource_projections: Vec::new(),
        })
        .await
        .expect("catalogue");
    service
        .bind(Bound {
            storage: Arc::new(InMemoryStorage::new()),
            coordinator: Arc::new(NoopCoordinator),
            catalog: Arc::new(catalog),
            registry,
            metric_registry: Arc::new(FakeMetricRegistry::classified()),
        })
        .expect("bind");
    service
}

fn status(err: CanonicalError) -> u16 {
    Problem::from(err).status.expect("status")
}

#[tokio::test]
async fn before_bootstrap_every_call_is_not_ready() {
    let client = InProcessQuotaManager::new(unbound_service());
    let err = client
        .create_quota(&ctx(), spec())
        .await
        .expect_err("not ready");
    assert_eq!(status(err), 503);
    let err = client
        .read_quotas(&ctx(), QuotaFilter::default(), PageRequest::default())
        .await
        .expect_err("not ready");
    assert_eq!(status(err), 503);
}

#[tokio::test]
async fn the_client_round_trips_through_the_same_domain_path_as_rest() {
    let client = InProcessQuotaManager::new(bound_service().await);
    let id = client.create_quota(&ctx(), spec()).await.expect("create");
    client
        .update_quota(
            &ctx(),
            id,
            QuotaPatch {
                fail_open_hint: Some(true),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect("update returns unit");
    let page = client
        .read_quotas(&ctx(), QuotaFilter::default(), PageRequest::default())
        .await
        .expect("read");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].quota.id, id);
    assert!(page.items[0].quota.fail_open_hint);
    assert_eq!(page.items[0].quota.record_version, 2);
    assert!(page.items[0].currently_within_window);
    let outcome = client
        .deactivate_quota(&ctx(), id)
        .await
        .expect("deactivate");
    assert!(outcome.resolved_leases.is_empty());
    let page = client
        .read_quotas(
            &ctx(),
            QuotaFilter {
                status: Some(QuotaStatus::Deactivated),
                ..QuotaFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("read deactivated");
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn domain_rejections_arrive_as_canonical_errors() {
    let client = InProcessQuotaManager::new(bound_service().await);
    let rate = QuotaSpec {
        quota_type: QuotaType::Rate,
        period: None,
        ..spec()
    };
    let err = client.create_quota(&ctx(), rate).await.expect_err("rate");
    assert_eq!(status(err), 501);
    let err = client
        .update_quota(
            &ctx(),
            quota_enforcement_sdk::QuotaId::generate(),
            QuotaPatch::default(),
        )
        .await
        .expect_err("an empty patch is rejected before the lookup");
    assert_eq!(status(err), 400);
}

#[tokio::test]
async fn a_caller_supplied_constraint_contract_is_rejected_before_the_lookup() {
    let client = InProcessQuotaManager::new(bound_service().await);
    let id = client.create_quota(&ctx(), spec()).await.expect("created");
    let err = client
        .update_quota(
            &ctx(),
            id,
            QuotaPatch {
                metadata: Some(serde_json::Map::new()),
                constraint_contract: Some(quota_enforcement_sdk::ContractRef {
                    type_id: type_id(crate::test_support::LLM_TOKEN_CONSTRAINT),
                    version: 1,
                }),
                ..QuotaPatch::default()
            },
        )
        .await
        .expect_err("the gear alone names the accepted contract");
    assert!(
        format!("{err:?}").contains("CONSTRAINT_CONTRACT_NOT_CALLER_SUPPLIED"),
        "{err:?}"
    );
    assert_eq!(status(err), 400);
}
