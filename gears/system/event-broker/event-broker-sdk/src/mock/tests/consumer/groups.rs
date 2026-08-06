//! Mirrors scenarios/consumer/groups/. Tests migrated per mock-reference-alignment.
use super::super::helpers::*;
#[cfg(test)]
use toolkit_gts::gts_id;

use crate::api::EventBrokerApi;
use crate::error::EventBrokerError;
use crate::ids::ConsumerGroupId;
use crate::models::{ConsumerGroupKind, CreateConsumerGroupRequest};

/// Scenario: consumer/groups/1.01-positive-create-anonymous-group.md
#[tokio::test]
async fn s1_01_positive_create_anonymous_group() {
    let broker = crate::mock::MockBroker::new();
    let c = ctx();

    let group = broker
        .create_consumer_group(
            &c,
            CreateConsumerGroupRequest {
                client_agent: "order-worker/1.4.0 cf-event-broker-sdk/0.1.0".to_owned(),
                description: Some("order-fulfilment workers".to_owned()),
            },
        )
        .await
        .unwrap();

    assert_ne!(group.id.as_uuid(), uuid::Uuid::nil());
    // kind is "anonymous".
    assert_eq!(group.kind, ConsumerGroupKind::Anonymous);
    // tenant_id and owner_principal_id come from the caller's SecurityContext, not the body.
    assert_eq!(group.tenant_id, c.subject_tenant_id());
    assert_eq!(group.owner_principal_id, c.subject_id());

    // Side effect: a subsequent GET returns the same record.
    let fetched = broker.get_consumer_group(&c, &group.id).await.unwrap();
    assert_eq!(fetched.id, group.id);
    assert_eq!(fetched.kind, ConsumerGroupKind::Anonymous);
    assert_eq!(fetched.tenant_id, c.subject_tenant_id());
}

/// Scenario: consumer/groups/1.02-positive-get-group-by-id.md
#[tokio::test]
async fn s1_02_positive_get_group_by_id() {
    let broker = crate::mock::MockBroker::new();
    let c = ctx();
    let gid = make_group(&c, &broker).await;

    let fetched = broker.get_consumer_group(&c, &gid).await.unwrap();
    assert_eq!(fetched.id, gid);
    assert_eq!(fetched.kind, ConsumerGroupKind::Anonymous);
    assert_eq!(fetched.tenant_id, c.subject_tenant_id());
    assert_eq!(fetched.owner_principal_id, c.subject_id());
}

/// Scenario: consumer/groups/1.03-positive-list-groups.md
#[tokio::test]
async fn s1_03_positive_list_groups() {
    let broker = crate::mock::MockBroker::new();
    let c = ctx();
    let gid = make_group(&c, &broker).await;

    let page = broker
        .list_consumer_groups(&c, crate::models::ConsumerGroupQuery::default())
        .await
        .unwrap();

    assert!(
        page.items.iter().any(|g| g.id == gid),
        "the created group must appear in the listing"
    );
    let listed = page.items.iter().find(|g| g.id == gid).unwrap();
    assert_eq!(listed.kind, ConsumerGroupKind::Anonymous);
    assert_eq!(listed.tenant_id, c.subject_tenant_id());
}

/// Scenario: consumer/groups/1.04-positive-delete-empty-group.md
#[tokio::test]
async fn s1_04_positive_delete_empty_group() {
    let broker = crate::mock::MockBroker::new();
    let c = ctx();
    let gid = make_group(&c, &broker).await;

    // No subscriptions reference the group → DELETE succeeds.
    broker.delete_consumer_group(&c, &gid).await.unwrap();

    // Side effect: the group is absent; a subsequent GET returns ConsumerGroupNotFound.
    let err = broker.get_consumer_group(&c, &gid).await.unwrap_err();
    assert!(
        matches!(err, EventBrokerError::ConsumerGroupNotFound { .. }),
        "deleted group must be absent, got {err:?}"
    );
}

/// Scenario: consumer/groups/1.05-negative-delete-group-with-active-members.md
#[tokio::test]
async fn s1_05_negative_delete_group_with_active_members() {
    // Setup: create a group, then perform a cold JOIN so one active member exists.
    let (broker, _h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    // DELETE is rejected while the group has an active member.
    let err = broker.delete_consumer_group(&c, &gid).await.unwrap_err();
    assert!(
        matches!(err, EventBrokerError::ConsumerGroupHasActiveMembers { .. }),
        "delete must be refused while members are active, got {err:?}"
    );

    // Side effect: the group is unchanged (still present).
    assert!(broker.get_consumer_group(&c, &gid).await.is_ok());

    // After the active subscription LEAVEs, a retried DELETE succeeds (per positive-1.4).
    broker.leave(&c, sub.subscription_id).await.unwrap();
    broker.delete_consumer_group(&c, &gid).await.unwrap();
}

/// Scenario: consumer/groups/1.07-negative-get-unknown-group.md
#[tokio::test]
async fn s1_07_negative_get_unknown_group() {
    let broker = crate::mock::MockBroker::new();
    let c = ctx();
    let unknown = ConsumerGroupId::new(uuid::Uuid::nil());

    let err = broker.get_consumer_group(&c, &unknown).await.unwrap_err();
    assert!(
        matches!(err, EventBrokerError::ConsumerGroupNotFound { .. }),
        "unknown group lookup must return ConsumerGroupNotFound, got {err:?}"
    );
}

/// Scenario: consumer/groups/1.06-negative-invalid-client-agent.md
#[tokio::test]
async fn s1_06_negative_invalid_client_agent() {
    let broker = crate::mock::MockBroker::new();
    let c = ctx();

    // A non-ASCII `client_agent` breaks the encoding rule. The rejection names
    // the field and states the requirement, and carries no part of the value.
    let err = broker
        .create_consumer_group(
            &c,
            CreateConsumerGroupRequest {
                client_agent: "consumer/\u{2713}1.0".to_owned(),
                description: None,
            },
        )
        .await
        .expect_err("non-ASCII client_agent must be rejected");
    assert!(
        matches!(
            &err,
            EventBrokerError::InvalidTextField { field, detail, reason, instance }
                if *field == "client_agent"
                    && detail == "must contain only printable ASCII (0x20-0x7E)"
                    && *reason == "ascii_only"
                    && instance == "/v1/consumer-groups"
        ),
        "expected InvalidTextField(client_agent, ascii_only), got {err:?}"
    );

    // A `DEL` is rejected too, which `str::is_ascii` used to admit.
    let err = broker
        .create_consumer_group(
            &c,
            CreateConsumerGroupRequest {
                client_agent: "consumer\u{7f}".to_owned(),
                description: None,
            },
        )
        .await
        .expect_err("a DEL byte in client_agent must be rejected");
    assert!(
        matches!(&err, EventBrokerError::InvalidTextField { reason, .. } if *reason == "ascii_only"),
        "expected ascii_only, got {err:?}"
    );

    // One byte past the bound breaks the length rule, which reports its own
    // reason so a caller knows to shorten rather than re-encode.
    let err = broker
        .create_consumer_group(
            &c,
            CreateConsumerGroupRequest {
                client_agent: "a".repeat(257),
                description: None,
            },
        )
        .await
        .expect_err("oversized client_agent must be rejected");
    assert!(
        matches!(
            &err,
            EventBrokerError::InvalidTextField { field, detail, reason, .. }
                if *field == "client_agent"
                    && detail == "must be 1-256 bytes, got 257"
                    && *reason == "field_too_long"
        ),
        "expected InvalidTextField(client_agent, field_too_long), got {err:?}"
    );

    // `description` is bounded by the same rule - the case the scenario sends,
    // a multibyte character next to a control byte.
    let err = broker
        .create_consumer_group(
            &c,
            CreateConsumerGroupRequest {
                client_agent: "consumer/1.0".to_owned(),
                description: Some("caf\u{e9}\u{2} orders".to_owned()),
            },
        )
        .await
        .expect_err("non-ASCII description must be rejected");
    assert!(
        matches!(
            &err,
            EventBrokerError::InvalidTextField { field, reason, .. }
                if *field == "description" && *reason == "ascii_only"
        ),
        "expected InvalidTextField(description, ascii_only), got {err:?}"
    );

    // An empty description is accepted: the field is optional, so "" and an
    // absent field agree.
    broker
        .create_consumer_group(
            &c,
            CreateConsumerGroupRequest {
                client_agent: "consumer/1.0".to_owned(),
                description: Some(String::new()),
            },
        )
        .await
        .expect("an empty description is within the rule");
}

/// Scenario: consumer/groups/1.08-positive-named-group-join.md
#[tokio::test]
async fn s1_08_positive_named_group_join() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    // A named group is provisioned via types_registry (no POST /v1/consumer-groups).
    let named_gts = gts_id!("cf.core.events.consumer_group.v1~vendor.audit.pipeline.x.v1");
    let named = ConsumerGroupId::from_gts(named_gts);
    h.register_named_group(named_gts).await;

    // JOIN the well-known identifier directly (the :consume grant is HTTP-layer authz).
    let assignment = join_group(&c, &broker, &named, TOPIC).await;
    assert!(
        !assignment.assigned.is_empty(),
        "JOIN to a provisioned named group is admitted and assigned a partition"
    );

    // The registry record reports kind = Named.
    let group = broker.get_consumer_group(&c, &named).await.unwrap();
    assert_eq!(
        group.kind,
        ConsumerGroupKind::Named,
        "provisioned group is Named"
    );
}
