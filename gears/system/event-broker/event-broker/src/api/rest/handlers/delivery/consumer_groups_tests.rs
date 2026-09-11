//! `handlers/delivery/consumer_groups.rs` coverage (`eb-rest-handlers` task 11.4):
//! create id-minting, list, get, delete (empty vs. active-members).
//!
//! Every test asserts the exact response body received. `id`/`created_at`
//! are server-minted (random uuid, wall-clock time) - extracted from the
//! response and reused to build the exact expected body, rather than
//! skipped.

use std::sync::Arc;

use authz_resolver_sdk::{EvaluationRequest, PolicyEnforcer};
use chrono::Utc;
use serde_json::json;
use toolkit_gts::GtsInstanceId;
use toolkit_security::pep_properties;
use uuid::Uuid;

use crate::domain::model::{ConsumerGroup, ConsumerGroupKind};
use crate::domain::repo::ConsumerGroupRepo;
use crate::test_support::{DenyingAuthZ, EventBrokerHarness, Json};

#[tokio::test]
async fn create_consumer_group_mints_a_gts_id_and_returns_201() {
    let harness = EventBrokerHarness::builder().build().await;
    // `ConsumerGroup.tenant_id`/`owner_principal_id` are stamped from the
    // caller's `SecurityContext`, not request-suppliable - read back
    // whatever the harness's own (random) context carries, rather than
    // pinning it via a builder override that only existed for this.
    let tenant_id = harness.security_context().subject_tenant_id();
    let subject_id = harness.security_context().subject_id();

    let resp = harness.api_v1().post_consumer_groups().send().await;

    resp.assert_status(201);
    let body = resp.json();
    let id = body["id"].as_str().expect("id must be a string").to_owned();
    let created_at = body["created_at"]
        .as_str()
        .expect("created_at must be a string")
        .to_owned();
    assert!(
        id.starts_with("gts.cf.core.events.consumer_group.v1~"),
        "id must be minted under the anonymous consumer-group GTS prefix, got '{id}'"
    );
    assert_eq!(
        body,
        json!({
            "id": id,
            "kind": "anonymous",
            "tenant_id": tenant_id,
            "owner_principal_id": subject_id,
            "description": null,
            "created_at": created_at,
        })
    );
}

#[tokio::test]
async fn create_consumer_group_with_client_agent_and_description_returns_201() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_consumer_groups()
        .with_body(Json(&json!({
            "client_agent": "test-agent/1.0",
            "description": "for integration tests",
        })))
        .send()
        .await;

    resp.assert_status(201);
    let body = resp.json();
    // `client_agent` has no broker-side semantic beyond logging
    // (`gts.cf.core.events.consumer_group.v1~.schema.json`'s `CreateRequest.client_agent` doc
    // comment) - not returned on the resource, unlike `description`.
    assert_eq!(body["description"], json!("for integration tests"));
    assert!(
        !body.as_object().unwrap().contains_key("client_agent"),
        "client_agent must not be echoed back on the resource"
    );
}

#[tokio::test]
async fn create_consumer_group_oversized_body_returns_413() {
    let harness = EventBrokerHarness::builder().build().await;

    // One byte past axum's own implicit `Bytes` extractor default (2 MiB) -
    // `create_consumer_group`'s `body: axum::body::Bytes` parameter gets
    // this limit for free (`eb-dispatcher-proxy-error-handling`'s
    // Decisions), so this must reject before the handler body ever runs.
    let oversized_body = vec![0u8; 2 * 1024 * 1024 + 1];

    let resp = harness
        .api_v1()
        .post_consumer_groups()
        .with_body(oversized_body)
        .send()
        .await;

    resp.assert_status(413);
}

#[tokio::test]
async fn create_consumer_group_body_missing_client_agent_returns_400() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_consumer_groups()
        .with_body(Json(&json!({ "description": "no client_agent" })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "InvalidBody: invalid JSON body: missing field `client_agent` at line 1 \
                       column 33",
            "instance": "/event-broker/v1/consumer-groups",
            "context": {
                "format": "InvalidBody: invalid JSON body: missing field `client_agent` at line \
                           1 column 33",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn create_consumer_group_mints_a_fresh_id_each_call() {
    let harness = EventBrokerHarness::builder().build().await;

    let a = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_ne!(a, b);
}

#[tokio::test]
async fn list_consumer_groups_happy_path() {
    let harness = EventBrokerHarness::builder().build().await;
    let created_a = harness.api_v1().post_consumer_groups().send().await;
    created_a.assert_status(201);
    let created_b = harness.api_v1().post_consumer_groups().send().await;
    created_b.assert_status(201);

    let resp = harness.api_v1().get_consumer_groups().send().await;

    resp.assert_status(200);
    let mut body = resp.json();
    body["items"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|g| g["id"].as_str().unwrap().to_owned());
    let mut expected_items = vec![created_a.json(), created_b.json()];
    expected_items.sort_by_key(|g| g["id"].as_str().unwrap().to_owned());
    assert_eq!(
        body,
        json!({
            "items": expected_items,
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn list_consumer_groups_filters_by_kind() {
    let harness = EventBrokerHarness::builder().build().await;
    let created = harness.api_v1().post_consumer_groups().send().await;
    created.assert_status(201);

    let matching = harness
        .api_v1()
        .get_consumer_groups()
        .with_query("$filter", "kind%20eq%20'anonymous'")
        .send()
        .await;
    matching.assert_status(200);
    assert_eq!(
        matching.json(),
        json!({
            "items": [created.json()],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );

    let non_matching = harness
        .api_v1()
        .get_consumer_groups()
        .with_query("$filter", "kind%20eq%20'named'")
        .send()
        .await;
    non_matching.assert_status(200);
    assert_eq!(
        non_matching.json(),
        json!({
            "items": [],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn get_consumer_group_happy_path() {
    let harness = EventBrokerHarness::builder().build().await;
    let created = harness.api_v1().post_consumer_groups().send().await;

    let id = created.json()["id"].as_str().unwrap().to_owned();
    let resp = harness.api_v1().get_consumer_group(&id).send().await;

    resp.assert_status(200);
    assert_eq!(resp.json(), created.json());
}

#[tokio::test]
async fn get_consumer_group_not_found_returns_404() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .get_consumer_group("gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1")
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": "consumer group 'gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1' is not registered",
            "instance": "/event-broker/v1/consumer-groups/gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1",
            "context": {
                "resource_name": "gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1",
                "resource_type": "gts.cf.core.events.consumer_group.v1~",
            },
        })
    );
}

#[tokio::test]
async fn delete_consumer_group_with_no_members_returns_204() {
    let harness = EventBrokerHarness::builder().build().await;
    let id = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let delete_resp = harness.api_v1().delete_consumer_group(&id).send().await;
    delete_resp.assert_status(204);
    assert_eq!(delete_resp.text(), "", "204 No Content must carry no body");

    let get_resp = harness.api_v1().get_consumer_group(&id).send().await;
    get_resp.assert_status(404);
    assert_eq!(
        get_resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("consumer group '{id}' is not registered"),
            "instance": format!("/event-broker/v1/consumer-groups/{id}"),
            "context": {
                "resource_name": id,
                "resource_type": "gts.cf.core.events.consumer_group.v1~",
            },
        })
    );
}

#[tokio::test]
async fn delete_consumer_group_not_found_returns_404() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .delete_consumer_group("gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1")
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": "consumer group 'gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1' is not registered",
            "instance": "/event-broker/v1/consumer-groups/gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1",
            "context": {
                "resource_name": "gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1",
                "resource_type": "gts.cf.core.events.consumer_group.v1~",
            },
        })
    );
}

#[tokio::test]
async fn delete_consumer_group_with_active_members_returns_409() {
    let harness = EventBrokerHarness::builder().build().await;
    let id = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": id,
            "client_agent": "test-agent",
            "interests": [],
        })))
        .send()
        .await
        .assert_status(201);

    let resp = harness.api_v1().delete_consumer_group(&id).send().await;

    resp.assert_status(409);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 409,
            "detail": "Operation precondition not met",
            "instance": format!("/event-broker/v1/consumer-groups/{id}"),
            "context": {
                "violations": [{
                    "type": "has_active_members",
                    "subject": id,
                    "description": format!("consumer group '{id}' still has active members"),
                }],
                "resource_type": "gts.cf.core.events.consumer_group.v1~",
            },
        })
    );
}

/// Inserts a consumer group of `kind` under `tenant_id` directly into the
/// repo - there's no REST path to create a `Named` group at all, or an
/// `Anonymous` one under a tenant the caller doesn't hold (exactly the
/// property these tests are checking).
async fn seed_consumer_group(
    harness: &EventBrokerHarness,
    id: &str,
    kind: ConsumerGroupKind,
    tenant_id: Uuid,
) {
    harness
        .repo()
        .create_consumer_group(ConsumerGroup {
            id: GtsInstanceId::try_new(id).expect("test-seeded id must be a valid GTS instance id"),
            kind,
            tenant_id,
            owner_principal_id: Uuid::new_v4(),
            description: None,
            created_at: Utc::now(),
        })
        .await
        .expect("seeding a consumer group must not fail");
}

#[tokio::test]
async fn get_consumer_group_rejects_a_caller_from_a_different_tenant_for_anonymous() {
    let foreign_tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: move |req: &EvaluationRequest| {
                req.resource.properties.get(pep_properties::OWNER_TENANT_ID)
                    == Some(&json!(foreign_tenant_id.to_string()))
            },
        })))
        .build()
        .await;
    let id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.foreignanon.v1";
    seed_consumer_group(
        &harness,
        id,
        ConsumerGroupKind::Anonymous,
        foreign_tenant_id,
    )
    .await;

    let resp = harness.api_v1().get_consumer_group(id).send().await;

    resp.assert_status(403);
}

#[tokio::test]
async fn delete_consumer_group_rejects_a_caller_from_a_different_tenant_for_anonymous() {
    let foreign_tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: move |req: &EvaluationRequest| {
                req.resource.properties.get(pep_properties::OWNER_TENANT_ID)
                    == Some(&json!(foreign_tenant_id.to_string()))
            },
        })))
        .build()
        .await;
    let id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.foreignanon.v1";
    seed_consumer_group(
        &harness,
        id,
        ConsumerGroupKind::Anonymous,
        foreign_tenant_id,
    )
    .await;

    let resp = harness.api_v1().delete_consumer_group(id).send().await;

    resp.assert_status(403);
    assert!(
        harness
            .repo()
            .find_consumer_group(&GtsInstanceId::try_new(id).unwrap())
            .await
            .expect("repo lookup must not fail")
            .is_some(),
        "a denied delete must not remove the consumer group"
    );
}

#[tokio::test]
async fn named_consumer_group_get_and_delete_succeed_regardless_of_caller_tenant() {
    let foreign_tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            // Denies every tenant-scope check unconditionally - if `Named`
            // groups were (incorrectly) subject to the same tenant check as
            // `Anonymous` ones, this would turn the assertions below into
            // 403s instead of success.
            deny_if: |req: &EvaluationRequest| {
                req.resource
                    .properties
                    .contains_key(pep_properties::OWNER_TENANT_ID)
            },
        })))
        .build()
        .await;
    let id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.namedgroup.v1";
    seed_consumer_group(&harness, id, ConsumerGroupKind::Named, foreign_tenant_id).await;

    harness
        .api_v1()
        .get_consumer_group(id)
        .send()
        .await
        .assert_status(200);
    harness
        .api_v1()
        .delete_consumer_group(id)
        .send()
        .await
        .assert_status(204);
}

#[tokio::test]
async fn list_consumer_groups_excludes_a_different_tenants_anonymous_groups_but_keeps_named() {
    let foreign_tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: move |req: &EvaluationRequest| {
                req.resource.properties.get(pep_properties::OWNER_TENANT_ID)
                    == Some(&json!(foreign_tenant_id.to_string()))
            },
        })))
        .build()
        .await;
    let own_tenant_id = harness.security_context().subject_tenant_id();
    let own_anon_id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.ownanon.v1";
    let foreign_anon_id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.foreignanon.v1";
    let named_id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.namedgroup.v1";
    seed_consumer_group(
        &harness,
        own_anon_id,
        ConsumerGroupKind::Anonymous,
        own_tenant_id,
    )
    .await;
    seed_consumer_group(
        &harness,
        foreign_anon_id,
        ConsumerGroupKind::Anonymous,
        foreign_tenant_id,
    )
    .await;
    seed_consumer_group(
        &harness,
        named_id,
        ConsumerGroupKind::Named,
        foreign_tenant_id,
    )
    .await;

    let resp = harness.api_v1().get_consumer_groups().send().await;

    resp.assert_status(200);
    let mut ids: Vec<String> = resp.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    let mut expected = vec![own_anon_id.to_owned(), named_id.to_owned()];
    expected.sort();
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn create_consumer_group_rejects_a_caller_without_define_permission() {
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| req.action.name == "define",
        })))
        .build()
        .await;

    let resp = harness.api_v1().post_consumer_groups().send().await;

    resp.assert_status(403);
}

#[tokio::test]
async fn named_consumer_group_get_and_delete_reject_a_caller_without_permission() {
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| {
                req.resource.properties.contains_key("consumer_group_id")
            },
        })))
        .build()
        .await;
    let id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.namedgroup.v1";
    seed_consumer_group(&harness, id, ConsumerGroupKind::Named, Uuid::new_v4()).await;

    harness
        .api_v1()
        .get_consumer_group(id)
        .send()
        .await
        .assert_status(403);
    harness
        .api_v1()
        .delete_consumer_group(id)
        .send()
        .await
        .assert_status(403);
    assert!(
        harness
            .repo()
            .find_consumer_group(&GtsInstanceId::try_new(id).unwrap())
            .await
            .expect("repo lookup must not fail")
            .is_some(),
        "a denied delete must not remove the consumer group"
    );
}

#[tokio::test]
async fn list_consumer_groups_excludes_a_named_group_without_consume_permission() {
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| {
                req.resource.properties.contains_key("consumer_group_id")
            },
        })))
        .build()
        .await;
    let visible_id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.ownanon.v1";
    let hidden_named_id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.namedgroup.v1";
    seed_consumer_group(
        &harness,
        visible_id,
        ConsumerGroupKind::Anonymous,
        harness.security_context().subject_tenant_id(),
    )
    .await;
    seed_consumer_group(
        &harness,
        hidden_named_id,
        ConsumerGroupKind::Named,
        Uuid::new_v4(),
    )
    .await;

    let resp = harness.api_v1().get_consumer_groups().send().await;

    resp.assert_status(200);
    let ids: Vec<String> = resp.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec![visible_id]);
}

// -- text-field encoding and length ---------------------------------------
//
// The response body names the field and states the requirement, and carries
// no part of the submitted value: a caller already knows what it sent, and an
// error travels into logs and clients where the request was never meant to go.

#[tokio::test]
async fn create_consumer_group_rejects_a_non_ascii_client_agent() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_consumer_groups()
        .with_body(Json(&json!({ "client_agent": "consumer/\u{2713}1.0" })))
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
            "instance": "/event-broker/v1/consumer-groups",
            "context": {
                "field_violations": [{
                    "field": "client_agent",
                    "description": "must contain only printable ASCII (0x20-0x7E)",
                    "reason": "ascii_only",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn create_consumer_group_rejects_an_over_length_client_agent() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_consumer_groups()
        .with_body(Json(&json!({ "client_agent": "a".repeat(257) })))
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
            "instance": "/event-broker/v1/consumer-groups",
            "context": {
                "field_violations": [{
                    "field": "client_agent",
                    "description": "must be 1-256 bytes, got 257",
                    "reason": "field_too_long",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn create_consumer_group_rejects_an_empty_client_agent() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_consumer_groups()
        .with_body(Json(&json!({ "client_agent": "" })))
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
            "instance": "/event-broker/v1/consumer-groups",
            "context": {
                "field_violations": [{
                    "field": "client_agent",
                    "description": "must be 1-256 bytes, got 0",
                    "reason": "field_too_long",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn create_consumer_group_rejects_a_non_ascii_description() {
    let harness = EventBrokerHarness::builder().build().await;

    // The pair the scenario sends: a multibyte character next to a control
    // byte, both outside the printable range.
    let resp = harness
        .api_v1()
        .post_consumer_groups()
        .with_body(Json(&json!({
            "client_agent": "consumer/1.0",
            "description": "caf\u{e9}\u{2} orders",
        })))
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
            "instance": "/event-broker/v1/consumer-groups",
            "context": {
                "field_violations": [{
                    "field": "description",
                    "description": "must contain only printable ASCII (0x20-0x7E)",
                    "reason": "ascii_only",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

/// The gear and the SDK mock are two implementations of one API, so a value
/// one rejects the other must reject identically. They share the `validate`
/// module, and this asserts the wire shapes agree too - the field, the reason,
/// the status and the canonical category - so a divergence introduced in
/// either mapping fails here rather than in an integration run.
#[test]
fn a_text_field_rejection_is_identical_from_the_gear_and_the_mock() {
    use toolkit_canonical_errors::{CanonicalError, Problem};

    fn problem_of(err: CanonicalError) -> serde_json::Value {
        let problem = Problem::from(err)
            .with_instance("/event-broker/v1/consumer-groups")
            .with_trace_id("trace-parity");
        serde_json::to_value(problem).expect("problem serializes")
    }

    let bad = "agent\u{1}/1.0";
    let violation = event_broker_sdk::validate::client_agent(bad)
        .expect_err("the value breaks the encoding rule");

    let from_gear: CanonicalError =
        crate::domain::error::DomainError::from(violation.clone()).into();
    let from_mock: CanonicalError =
        event_broker_sdk::EventBrokerError::invalid_text_field(&violation, "/v1/consumer-groups")
            .into();

    // Rendered through one instance/trace so only the rejection's own fields
    // can differ.
    let gear = problem_of(from_gear);
    let mock = problem_of(from_mock);

    for key in ["type", "title", "status", "detail"] {
        assert_eq!(gear[key], mock[key], "{key} differs between gear and mock");
    }
    assert_eq!(
        gear["context"]["field_violations"],
        mock["context"]["field_violations"]
    );
    assert_eq!(
        gear["context"]["resource_type"],
        mock["context"]["resource_type"]
    );
    assert_eq!(
        gear["context"]["field_violations"][0]["reason"],
        json!("ascii_only")
    );
}
