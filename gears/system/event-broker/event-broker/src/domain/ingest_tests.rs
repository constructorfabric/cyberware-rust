//! Partition selection: what the pointer resolves to, and where that lands.
//!
//! Tested here rather than over HTTP because no publish response carries the
//! partition - the endpoint answers `202` with no body - so the observable
//! behaviour lives at this boundary. What an operator sees on the wire is the
//! ordering these values produce, which is the property each case below states.

use uuid::Uuid;

use super::{PublishRequest, partition_for};
use crate::domain::backend::partition_input;
use crate::domain::error::DomainError;

const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~x.eb.part.foo.v1~";

fn request(tenant: Uuid, subject: &str, data: serde_json::Value) -> PublishRequest {
    PublishRequest {
        id: Uuid::new_v4(),
        r#type: crate::test_support::event_type_id(EVENT_TYPE),
        tenant_id: tenant,
        source: "partition-test".to_owned(),
        subject: subject.to_owned(),
        subject_type: "gts.x.eb.part.subject.v1~".to_owned(),
        occurred_at: chrono::Utc::now(),
        trace_parent: None,
        data,
        meta: None,
    }
}

/// The default the base declares: a tenant's events are co-located whatever
/// else differs about them, which is what gives a tenant total ordering on a
/// topic without anyone declaring anything.
#[test]
fn the_tenant_pointer_co_locates_a_tenants_events() {
    let tenant = Uuid::new_v4();
    let first = request(tenant, "order-1", serde_json::json!({ "order_id": "a" }));
    let second = request(tenant, "order-2", serde_json::json!({ "order_id": "b" }));

    let first_input = partition_input(&first, "/tenant_id").expect("the tenant always resolves");
    let second_input = partition_input(&second, "/tenant_id").expect("the tenant always resolves");

    assert_eq!(first_input, tenant.to_string());
    assert_eq!(first_input, second_input);
    assert_eq!(
        partition_for(&first_input, 8),
        partition_for(&second_input, 8),
        "same input, same partition"
    );
}

/// A pointer may reach into the payload, which is the case a bare field name
/// could not express.
#[test]
fn a_pointer_into_the_payload_groups_by_that_member() {
    let one = request(
        Uuid::new_v4(),
        "order-1",
        serde_json::json!({ "order_id": "shared" }),
    );
    let another = request(
        Uuid::new_v4(),
        "order-2",
        serde_json::json!({ "order_id": "shared" }),
    );

    let one_input = partition_input(&one, "/data/order_id").expect("the member is present");
    let another_input = partition_input(&another, "/data/order_id").expect("the member is present");

    assert_eq!(one_input, "shared");
    assert_eq!(
        partition_for(&one_input, 8),
        partition_for(&another_input, 8),
        "two different tenants sharing the pointed-at member land together - which is what \
         proves the key is the type's choice rather than the tenant's"
    );
}

/// A numeric member is hashable without a producer stringifying it first.
#[test]
fn a_numeric_member_resolves_to_its_json_form() {
    let event = request(
        Uuid::new_v4(),
        "order-1",
        serde_json::json!({ "order_id": 42 }),
    );

    assert_eq!(
        partition_input(&event, "/data/order_id").expect("a number is hashable"),
        "42"
    );
}

/// Registration proves the member is *declared*, not that every event carries
/// it. An event omitting an optional member is refused rather than routed by
/// something else, because a silent fallback would split one key's events
/// across two partitions.
#[test]
fn a_pointer_resolving_to_nothing_is_refused() {
    let event = request(Uuid::new_v4(), "order-1", serde_json::json!({}));

    let err = partition_input(&event, "/data/order_id")
        .expect_err("an absent member cannot select a partition");
    assert!(
        matches!(
            err,
            DomainError::Validation {
                code: "PartitionKeyUnresolved",
                ..
            }
        ),
        "unexpected error: {err}"
    );
}

#[test]
fn a_pointer_resolving_to_null_is_refused() {
    let event = request(
        Uuid::new_v4(),
        "order-1",
        serde_json::json!({ "order_id": null }),
    );

    let err = partition_input(&event, "/data/order_id")
        .expect_err("null is not a key, it is the absence of one");
    assert!(
        matches!(
            err,
            DomainError::Validation {
                code: "PartitionKeyUnresolved",
                ..
            }
        ),
        "unexpected error: {err}"
    );
}

/// A container has no stable hash input: two objects that are equal as values
/// may serialize differently, so hashing one would make co-location depend on
/// key order.
#[test]
fn a_pointer_resolving_to_a_container_is_refused() {
    let event = request(
        Uuid::new_v4(),
        "order-1",
        serde_json::json!({ "order_id": { "nested": true } }),
    );

    let err =
        partition_input(&event, "/data/order_id").expect_err("an object cannot select a partition");
    assert!(
        matches!(
            err,
            DomainError::Validation {
                code: "PartitionKeyUnresolved",
                ..
            }
        ),
        "unexpected error: {err}"
    );
}

/// The count bounds the answer, and the mask keeps a signed remainder out of
/// it: every partition a topic has is reachable and none outside it is.
#[test]
fn a_partition_is_always_inside_the_topics_count() {
    for count in [1_u32, 2, 16, 64] {
        for n in 0..200 {
            let partition = partition_for(&format!("key-{n}"), count);
            assert!(
                partition >= 0 && partition < i32::try_from(count).expect("small count"),
                "count {count} produced partition {partition}"
            );
        }
    }
}
