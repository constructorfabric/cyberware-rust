//! `handlers/ingest/topics.rs` coverage (`eb-rest-handlers` task 11.3): pagination,
//! filter, segment manifest, 404 cases.
//!
//! Every test asserts the exact response body received. `list_topics` sorts
//! by `id` before paginating (`pagination::paginate_by_key`'s real keyset
//! order - the platform standard, not the `HashMap`-backed
//! `InMemoryDomainRepo`'s own iteration order), so item order across pages
//! *is* part of the contract; the pagination test below still resolves
//! each item by id rather than assuming a fixed page split, since `next_cursor`/
//! `prev_cursor` are now opaque `CursorV1` tokens (not a predictable
//! offset), fetched from each response rather than hand-encoded.
//! A topic DTO reports the registered instance's own values only - `id`,
//! `description`, `retention` - so every expected body here is fully known up
//! front, with nothing server-generated to read back out of the response.

use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::BTreeSet;

use crate::test_support::{EventBrokerHarness, Json, StaticTypesRegistry};

#[tokio::test]
async fn list_topics_happy_path_returns_all_seeded_topics() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.b.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;

    let resp = harness.api_v1().get_topics().send().await;

    resp.assert_status(200);
    let mut body = resp.json();
    body["items"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|t| t["id"].as_str().unwrap().to_owned());
    assert_eq!(
        body,
        json!({
            "items": [
                {
                    "id": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1",
                    "description": "a topic this test publishes to",
                    "retention": null,
                },
                {
                    "id": "gts.cf.core.events.topic.v1~x.eb.b.topic.v1",
                    "description": "a topic this test publishes to",
                    "retention": null,
                },
            ],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn list_topics_paginates_with_limit_and_cursor() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.b.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.c.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;

    // Fetched once, unpaginated, to establish each topic's exact expected
    // body before exercising pagination itself.
    let all = harness.api_v1().get_topics().send().await;
    all.assert_status(200);
    let expected_by_id: std::collections::BTreeMap<String, serde_json::Value> = all.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["id"].as_str().unwrap().to_owned(), item.clone()))
        .collect();
    assert_eq!(expected_by_id.len(), 3);

    let page1 = harness
        .api_v1()
        .get_topics()
        .with_query("limit", "2")
        .send()
        .await;
    page1.assert_status(200);
    let body1 = page1.json();
    let items1 = body1["items"].as_array().unwrap().clone();
    assert_eq!(
        items1.len(),
        2,
        "page 1 must carry exactly `limit` items: {body1}"
    );
    assert_eq!(body1["page_info"]["limit"], 2);
    assert!(body1["page_info"]["prev_cursor"].is_null());
    let next_cursor = body1["page_info"]["next_cursor"]
        .as_str()
        .expect("a third topic remains, so page 1 must carry a next_cursor")
        .to_owned();

    let page2 = harness
        .api_v1()
        .get_topics()
        .with_query("limit", "2")
        .with_query("cursor", next_cursor)
        .send()
        .await;
    page2.assert_status(200);
    let body2 = page2.json();
    let items2 = body2["items"].as_array().unwrap().clone();
    assert_eq!(
        items2.len(),
        1,
        "page 2 must carry the one remaining item: {body2}"
    );
    assert_eq!(body2["page_info"]["limit"], 2);
    assert!(body2["page_info"]["next_cursor"].is_null());
    let prev_cursor = body2["page_info"]["prev_cursor"]
        .as_str()
        .expect("page 1 exists, so page 2 must carry a prev_cursor")
        .to_owned();

    // Item *position* across pages isn't part of the contract beyond the
    // fixed `id`-ascending order `paginate_by_key` applies (see module doc
    // comment) - check every returned item against its exact expected
    // body, and check that the two pages together cover every seeded topic
    // exactly once.
    let mut seen = BTreeSet::new();
    for item in items1.iter().chain(items2.iter()) {
        let id = item["id"].as_str().unwrap();
        assert_eq!(
            item, &expected_by_id[id],
            "item for '{id}' did not match its expected body"
        );
        assert!(
            seen.insert(id.to_owned()),
            "'{id}' appeared on more than one page"
        );
    }
    assert_eq!(seen, expected_by_id.keys().cloned().collect());

    // `prev_cursor` must round-trip back to exactly page 1.
    let back = harness
        .api_v1()
        .get_topics()
        .with_query("limit", "2")
        .with_query("cursor", prev_cursor)
        .send()
        .await;
    back.assert_status(200);
    let body_back = back.json();
    assert_eq!(
        body_back["items"], body1["items"],
        "walking prev_cursor from page 2 must return exactly page 1"
    );
    assert!(body_back["page_info"]["prev_cursor"].is_null());
}

#[tokio::test]
async fn list_topics_filters_by_id() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.b.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .get_topics()
        .with_query(
            "$filter",
            "id%20eq%20'gts.cf.core.events.topic.v1~x.eb.a.topic.v1'",
        )
        .send()
        .await;

    resp.assert_status(200);
    assert_eq!(
        resp.json(),
        json!({
            "items": [{
                "id": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1",
                "description": "a topic this test publishes to",
                "retention": null,
            }],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn list_topics_invalid_cursor_returns_400() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .get_topics()
        .with_query("cursor", "not-a-valid-cursor!!")
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": "/event-broker/v1/topics",
            "context": {
                "resource_type": "gts.cf.core.odata.query.v1~",
                "field_violations": [{
                    "field": "cursor",
                    "description": "invalid cursor",
                    "reason": "INVALID_CURSOR",
                }],
            },
        })
    );
}

#[tokio::test]
async fn get_topic_segments_happy_path_reflects_published_events() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.a.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;

    for _ in 0..3 {
        harness
            .api_v1()
            .post_events()
            .with_body(Json(&json!({
                "id": uuid::Uuid::new_v4(),
                "type": "gts.cf.core.events.event.v1~x.eb.a.foo.v1~",
                "tenant_id": uuid::Uuid::new_v4(),
                "source": "test",
                "subject": "s1",
                "subject_type": "gts.x.eb.t1.subject.v1~",
                "occurred_at": Utc::now().to_rfc3339(),
            })))
            .send()
            .await
            .assert_status(202);
    }

    // Publish is now asynchronous (the ingest outbox drains in the
    // background, design.md D5) - `end_sequence` only reaches 3 once the
    // processor has drained all three rows, so poll briefly rather than
    // asserting on the very next request.
    let body = {
        let mut body = json!(null);
        for _ in 0..50 {
            let resp = harness
                .api_v1()
                .get_topic_segments()
                .with_query("topic", "gts.cf.core.events.topic.v1~x.eb.a.topic.v1")
                .with_query("partition", "0")
                .send()
                .await;
            resp.assert_status(200);
            body = resp.json();
            if body["end_sequence"] == json!(3) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            body["end_sequence"],
            json!(3),
            "all 3 published events must have drained from the ingest outbox within the poll window"
        );
        body
    };
    // `start_time`/`end_time` are the real publish-time timestamps (not
    // request input) - not predictable to the millisecond, but must be
    // present, RFC-3339, and ordered start <= end. Every other field is
    // checked exactly below, so nothing in the body goes unchecked.
    let start_time = body["start_time"]
        .as_str()
        .expect("start_time must be a string")
        .to_owned();
    let end_time = body["end_time"]
        .as_str()
        .expect("end_time must be a string")
        .to_owned();
    assert!(
        DateTime::parse_from_rfc3339(&start_time).unwrap()
            <= DateTime::parse_from_rfc3339(&end_time).unwrap(),
        "start_time ({start_time}) must be <= end_time ({end_time})"
    );
    assert_eq!(
        body,
        json!({
            "topic": "gts.cf.core.events.topic.v1~x.eb.a.topic.v1",
            "partition": 0,
            "start_sequence": 1,
            "end_sequence": 3,
            "start_time": start_time,
            "end_time": end_time,
            "segments": [{ "start_sequence": 1, "end_sequence": 3, "event_count": 3 }],
        })
    );
}

#[tokio::test]
async fn get_topic_segments_unregistered_topic_returns_404() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .get_topic_segments()
        .with_query("topic", "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1")
        .with_query("partition", "0")
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": "topic 'gts.cf.core.events.topic.v1~x.eb.missing.topic.v1' is not registered",
            "instance": "/event-broker/v1/topics/segments",
            "context": {
                "resource_name": "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1",
                "resource_type": "gts.cf.core.events.topic.v1~",
            },
        })
    );
}
