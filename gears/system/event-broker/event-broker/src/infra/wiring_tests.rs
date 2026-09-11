//! The production wiring, exercised through the harness that builds it.
//!
//! `infra/loader/shard_tests.rs` proves the loader fills a cache when handed
//! one directly. This proves `build_topic_manager` + `spawn_loader` -
//! constructed the way `module.rs` constructs them, from `LoaderConfig` - fill
//! the cache the delivery service was handed. Nothing here calls `absorb`, so a
//! filled cache means the wired loader did it.

use crate::domain::model::Sequence;
use std::time::Duration;

use serde_json::json;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::Assignment;
use crate::domain::streaming::source::PartitionKey;
use crate::infra::loader::attach::{AttachRequest, attach_readers};
use crate::test_support::{EventBrokerHarness, Json, StaticTypesRegistry};

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~";

fn registry() -> StaticTypesRegistry {
    StaticTypesRegistry::of(json!([
        { "id": TOPIC, "partitions": 1 },
        {
            "id": EVENT_TYPE,
            "topic": TOPIC,
            "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
        },
    ]))
}

/// Polls until `f` holds or the budget runs out, so the assertion reports the
/// real state rather than a timeout.
async fn until<F: Fn() -> bool>(budget: Duration, f: F) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f()
}

#[tokio::test]
async fn the_wired_loader_fills_the_cache_the_delivery_service_holds() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(registry())
        .build()
        .await;

    let key = PartitionKey::new(
        GtsInstanceId::try_new(TOPIC).expect("static topic id is valid"),
        0,
    );
    // A reader, not just a partition. The loader fills *ahead of readers* -
    // `scan_demands` derives demand from registered reader positions - so an
    // attached partition with nobody reading it correctly generates no fetch.
    // This is what a session does at open, via the same call.
    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let _slots = attach_readers(&AttachRequest {
        topics: harness.topics(),
        assigned: &[Assignment {
            topic: GtsInstanceId::try_new(TOPIC).expect("static topic id is valid"),
            partition: 0,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        }],
        cursors: &[],
        ready: &ready,
    });
    let partition = harness.topics().attach(&key);
    assert_eq!(
        partition.cache().stats().resident().events(),
        0,
        "nothing is resident before anything is published"
    );

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": EVENT_TYPE,
            "tenant_id": Uuid::new_v4(),
            "source": "test-the_wired_loader_fills_the_cache",
            "subject": "s-the_wired_loader_fills_the_cache",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": chrono::Utc::now().to_rfc3339(),
        })))
        .send()
        .await;
    resp.assert_status(202);

    // The ingest outbox persists asynchronously and the loader fetches on its
    // own tick, so both have to be allowed to happen.
    let filled = until(Duration::from_secs(10), || {
        partition.cache().stats().resident().events() > 0
    })
    .await;

    assert!(
        filled,
        "the wired loader never filled the cache: resident {} events, absorbed {}",
        partition.cache().stats().resident().events(),
        partition.cache().stats().absorbed().events()
    );
    assert!(
        partition.cache().stats().balances(),
        "accounting must balance after the loader absorbed"
    );
}

/// Configuration reaches the code it governs.
///
/// Not a tautology: before this change five of six `StreamingConfig` fields had
/// no reader at all, and the loader's eight knobs existed in no configuration
/// struct. A knob nothing reads is a promise to an operator that the broker
/// does not keep, so this asserts the values arrive rather than that they parse.
#[tokio::test]
async fn loader_configuration_reaches_the_policies_it_governs() {
    use crate::config::LoaderConfig;

    let cfg = LoaderConfig {
        fetch_max_events: 7,
        poll_floor_ms: 3,
        poll_ceiling_ms: 9,
        residency_limit_bytes: 4096,
        gap_threshold_events: 1234,
        ..LoaderConfig::default()
    };
    let topics = crate::infra::wiring::build_topic_manager(&cfg);
    let policy = topics.policy();

    assert_eq!(policy.fetch_max_events(), 7);
    assert_eq!(policy.reclaim().gap_threshold_events(), 1234);
    assert_eq!(policy.reclaim().residency_limit_bytes(), 4096);
    // The poller's bounds are a `Duration` pair, so compare in millis.
    assert_eq!(policy.poll().floor(), std::time::Duration::from_millis(3));
    assert_eq!(policy.poll().ceiling(), std::time::Duration::from_millis(9));
}
