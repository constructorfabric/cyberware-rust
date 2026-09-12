#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use quota_enforcement_sdk::{MetricId, MetricKind, SCOPE_USER};
use types_registry_sdk::TypesRegistryClient;

use super::CachedMetricRegistry;
use crate::domain::error::DomainError;
use crate::domain::ports::metric_registry::{
    Freshness, MetricDescriptor, MetricMode, MetricRegistry,
};
use crate::test_support::{
    FailingInstanceRegistry, METRIC_OTHER, METRIC_REQUESTS, METRIC_TOKENS,
    classified_metric_document, metric_base_documents, mock_registry, unclassified_metric_document,
};

const TTL: Duration = Duration::from_mins(1);
const GRACE: Duration = Duration::from_mins(5);
const GAUGE_DIRECT: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.gpu_seconds.v1";
const BAD_KIND: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.bad_kind.v1";
const BAD_MODE: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.bad_mode.v1";

fn metric(id: &str) -> MetricId {
    MetricId::parse(id).expect("metric id")
}

/// The metric base plus classified, unclassified, and misclassified instances.
fn registry() -> FailingInstanceRegistry {
    let base = metric_base_documents().remove(0);
    let instances = vec![
        classified_metric_document(METRIC_TOKENS, "counter", "quota_gated"),
        classified_metric_document(GAUGE_DIRECT, "gauge", "direct"),
        unclassified_metric_document(METRIC_OTHER),
        classified_metric_document(BAD_KIND, "histogram", "direct"),
        classified_metric_document(BAD_MODE, "counter", "sometimes"),
    ];
    let scope_type = crate::test_support::llm_gateway_documents();
    let mock = mock_registry(&[base], &instances).with_instances(
        scope_type
            .iter()
            .filter(|doc| crate::test_support::document_id(doc) == SCOPE_USER)
            .map(|doc| types_registry_sdk::testing::make_test_instance(SCOPE_USER, doc.clone())),
    );
    FailingInstanceRegistry::new(mock)
}

fn cached(registry: Arc<FailingInstanceRegistry>, capacity: usize) -> CachedMetricRegistry {
    CachedMetricRegistry::new(
        registry as Arc<dyn TypesRegistryClient>,
        capacity,
        TTL,
        GRACE,
    )
}

#[tokio::test]
async fn classifications_are_parsed_into_the_closed_enums() {
    let registry = Arc::new(registry());
    let cached = cached(registry, 8);
    let tokens = cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect("answer")
        .expect("registered");
    assert_eq!(
        tokens.descriptor,
        MetricDescriptor {
            kind: MetricKind::Counter,
            mode: MetricMode::QuotaGated
        }
    );
    assert_eq!(tokens.freshness, Freshness::Fresh);
    let gpu = cached
        .describe(&metric(GAUGE_DIRECT))
        .await
        .expect("answer")
        .expect("registered");
    assert_eq!(
        gpu.descriptor,
        MetricDescriptor {
            kind: MetricKind::Gauge,
            mode: MetricMode::Direct
        }
    );
}

#[tokio::test]
async fn missing_or_unknown_classification_is_an_explicit_error_never_a_default() {
    let cached = cached(Arc::new(registry()), 8);
    for id in [METRIC_OTHER, BAD_KIND, BAD_MODE] {
        let err = cached.describe(&metric(id)).await.expect_err(id);
        assert_eq!(
            err,
            DomainError::MetricClassificationInvalid {
                metric: id.to_owned()
            },
            "{id}"
        );
    }
}

#[tokio::test]
async fn an_unknown_instance_or_one_of_another_type_is_not_a_metric() {
    let cached = cached(Arc::new(registry()), 8);
    assert_eq!(
        cached
            .describe(&metric(METRIC_REQUESTS))
            .await
            .expect("answer"),
        None,
        "not registered"
    );
    assert_eq!(
        cached
            .describe(&MetricId::parse(SCOPE_USER).expect("instance id"))
            .await
            .expect("answer"),
        None,
        "an instance of the scope type is not a metric"
    );
}

#[tokio::test(start_paused = true)]
async fn cached_answers_are_served_within_the_ttl_and_refreshed_after_it() {
    let registry = Arc::new(registry());
    let cached = cached(registry.clone(), 8);
    cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect("first");
    cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect("second");
    assert_eq!(registry.instance_calls(), 1, "a fresh hit makes no call");
    tokio::time::advance(TTL + Duration::from_secs(1)).await;
    cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect("third");
    assert_eq!(
        registry.instance_calls(),
        2,
        "past the ttl the registry is asked again"
    );
}

#[tokio::test(start_paused = true)]
async fn an_outage_serves_a_stale_answer_within_the_grace_and_fails_closed_beyond_it() {
    let registry = Arc::new(registry());
    let cached = cached(registry.clone(), 8);
    cached.describe(&metric(METRIC_TOKENS)).await.expect("warm");
    registry.fail_instances();

    let within_ttl = cached.describe(&metric(METRIC_TOKENS)).await.expect("hit");
    assert_eq!(within_ttl.map(|c| c.freshness), Some(Freshness::Fresh));

    tokio::time::advance(TTL + Duration::from_secs(1)).await;
    let stale = cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect("served from the cache")
        .expect("known");
    assert_eq!(stale.freshness, Freshness::Stale);
    assert_eq!(stale.descriptor.kind, MetricKind::Counter);

    let cold = cached
        .describe(&metric(GAUGE_DIRECT))
        .await
        .expect_err("never cached");
    assert!(
        matches!(cold, DomainError::TypesRegistryUnavailable(_)),
        "{cold:?}"
    );

    tokio::time::advance(GRACE).await;
    let expired = cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect_err("beyond ttl + grace nothing is served");
    assert!(
        matches!(expired, DomainError::TypesRegistryUnavailable(_)),
        "{expired:?}"
    );

    registry.recover();
    let again = cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect("answer")
        .expect("known");
    assert_eq!(again.freshness, Freshness::Fresh);
}

#[tokio::test]
async fn the_cache_evicts_the_least_recently_used_entry_at_capacity() {
    let registry = Arc::new(registry());
    let cached = cached(registry.clone(), 1);
    cached.describe(&metric(METRIC_TOKENS)).await.expect("a");
    cached
        .describe(&metric(GAUGE_DIRECT))
        .await
        .expect("b evicts a");
    cached
        .describe(&metric(METRIC_TOKENS))
        .await
        .expect("a again");
    assert_eq!(registry.instance_calls(), 3);
}
