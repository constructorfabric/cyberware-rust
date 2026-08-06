//! `handlers/ingest/event_types.rs` coverage (`eb-rest-handlers` task 11.3):
//! pagination and filter (by `id` and `topic`).
//!
//! Every test asserts the exact response body received - see
//! `topics_tests.rs`'s module doc comment for why the pagination test
//! below still resolves each item by id (opaque `CursorV1` cursors fetched
//! from each response, not hand-encoded) even though `list_event_types`
//! now sorts by `id` before paginating (`pagination::paginate_by_key`'s
//! real keyset order).
//! An event-type DTO is a projection of the registered type schema and carries
//! no registration timestamp, so every expected body here is fully known up
//! front.
//! `StaticTypesRegistry` requires an event type's `topic` to match a topic
//! declared in the same list, so every topic referenced below is declared
//! too, even though the old hand-seeded `EventType` literals never bothered
//! to register a matching `Topic`.

use serde_json::json;
use std::collections::BTreeSet;

use crate::test_support::{EventBrokerHarness, StaticTypesRegistry};

#[tokio::test]
async fn list_event_types_happy_path_returns_all_seeded_types() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.a.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
            },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t2.b.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1",
            },
        ])))
        .build()
        .await;

    let resp = harness.api_v1().get_event_types().send().await;

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
                    "id": "gts.cf.core.events.event.v1~x.eb.t1.a.v1~",
                    "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                    "description": null,
                    "allowed_subject_types": [],
                    "partition_key": "/tenant_id",
                    "data_schema": {
                        "allOf": [
                            { "type": ["object", "null"] },
                            { "type": ["object", "null"] },
                        ],
                    },
                },
                {
                    "id": "gts.cf.core.events.event.v1~x.eb.t2.b.v1~",
                    "topic": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1",
                    "description": null,
                    "allowed_subject_types": [],
                    "partition_key": "/tenant_id",
                    "data_schema": {
                        "allOf": [
                            { "type": ["object", "null"] },
                            { "type": ["object", "null"] },
                        ],
                    },
                },
            ],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn list_event_types_filters_by_id() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.a.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
            },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t2.b.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1",
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .get_event_types()
        .with_query(
            "$filter",
            "id%20eq%20'gts.cf.core.events.event.v1~x.eb.t1.a.v1~'",
        )
        .send()
        .await;

    resp.assert_status(200);
    assert_eq!(
        resp.json(),
        json!({
            "items": [{
                "id": "gts.cf.core.events.event.v1~x.eb.t1.a.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "description": null,
                "allowed_subject_types": [],
                "partition_key": "/tenant_id",
                "data_schema": {
                        "allOf": [
                            { "type": ["object", "null"] },
                            { "type": ["object", "null"] },
                        ],
                    },
            }],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn list_event_types_filters_by_topic() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.a.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
            },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t2.b.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1",
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .get_event_types()
        .with_query(
            "$filter",
            "topic%20eq%20'gts.cf.core.events.topic.v1~x.eb.t2.topic.v1'",
        )
        .send()
        .await;

    resp.assert_status(200);
    assert_eq!(
        resp.json(),
        json!({
            "items": [{
                "id": "gts.cf.core.events.event.v1~x.eb.t2.b.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1",
                "description": null,
                "allowed_subject_types": [],
                "partition_key": "/tenant_id",
                "data_schema": {
                        "allOf": [
                            { "type": ["object", "null"] },
                            { "type": ["object", "null"] },
                        ],
                    },
            }],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn list_event_types_paginates_with_limit() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.type0.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
            },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.type1.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
            },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.type2.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
            },
        ])))
        .build()
        .await;

    // Fetched once, unpaginated, to establish each event type's exact
    // expected body before exercising pagination itself.
    let all = harness.api_v1().get_event_types().send().await;
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
        .get_event_types()
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
        .expect("a third event type remains, so page 1 must carry a next_cursor")
        .to_owned();

    let page2 = harness
        .api_v1()
        .get_event_types()
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
        .get_event_types()
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
