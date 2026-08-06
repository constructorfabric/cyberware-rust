//! JSON round-trip tests pinning the wire DTO shapes against the server's
//! `#[toolkit_macros::api_dto]` bodies. Each asserts the entire JSON inline.

use serde_json::json;
use uuid::Uuid;

use super::*;

fn uuid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

#[test]
fn publish_event_serialises_with_type_rename_and_meta() {
    let wire = PublishEventWire {
        id: uuid(1),
        type_id: "gts.cf.core.events.event.v1~acme.orders.created.v1~".to_owned(),
        tenant_id: uuid(2),
        source: "orders".to_owned(),
        subject: "order-1".to_owned(),
        subject_type: "gts.cf.core.events.subject.v1~acme.orders.order.v1".to_owned(),
        occurred_at: "2026-01-02T03:04:05Z".parse().unwrap(),
        trace_parent: Some("00-abc-def-01".to_owned()),
        data: Some(json!({ "amount": 100 })),
        meta: Some(MetaWire {
            version: 1,
            producer_id: Some(uuid(3)),
            previous: Some(4),
            sequence: Some(5),
        }),
    };
    assert_eq!(
        serde_json::to_value(&wire).unwrap(),
        json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "type": "gts.cf.core.events.event.v1~acme.orders.created.v1~",
            "tenant_id": "00000000-0000-0000-0000-000000000002",
            "source": "orders",
            "subject": "order-1",
            "subject_type": "gts.cf.core.events.subject.v1~acme.orders.order.v1",
            "occurred_at": "2026-01-02T03:04:05Z",
            "trace_parent": "00-abc-def-01",
            "data": { "amount": 100 },
            "meta": {
                "version": 1,
                "producer_id": "00000000-0000-0000-0000-000000000003",
                "previous": 4,
                "sequence": 5
            }
        })
    );
}

#[test]
fn publish_event_omits_absent_optionals() {
    let wire = PublishEventWire {
        id: uuid(1),
        type_id: "t".to_owned(),
        tenant_id: uuid(2),
        source: "s".to_owned(),
        subject: "sub".to_owned(),
        subject_type: "st".to_owned(),
        occurred_at: "2026-01-02T03:04:05Z".parse().unwrap(),
        trace_parent: None,
        data: None,
        meta: None,
    };
    assert_eq!(
        serde_json::to_value(&wire).unwrap(),
        json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "type": "t",
            "tenant_id": "00000000-0000-0000-0000-000000000002",
            "source": "s",
            "subject": "sub",
            "subject_type": "st",
            "occurred_at": "2026-01-02T03:04:05Z"
        })
    );
}

#[test]
fn register_producer_uses_snake_case_mode() {
    let wire = RegisterProducerWire {
        mode: ProducerModeWire::Chained,
        client_agent: "svc/1.0".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(&wire).unwrap(),
        json!({ "mode": "chained", "client_agent": "svc/1.0" })
    );
}

#[test]
fn interest_max_depth_is_untagged_int_or_null() {
    let unlimited = InterestWire {
        topic: "gts.cf.core.events.topic.v1~acme.orders.v1".to_owned(),
        tenant_id: uuid(2),
        max_depth: MaxDepthWire::Unlimited,
        barrier_mode: BarrierModeWire::Ignore,
        types: vec!["gts.cf.core.events.event.v1~acme.orders.*".to_owned()],
        filter: Some(FilterSpecWire {
            engine: "gts.cf.core.events.filter.v1~cf.core.expression.cel.v1".to_owned(),
            expression: "event.data.amount > 100".to_owned(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&unlimited).unwrap(),
        json!({
            "topic": "gts.cf.core.events.topic.v1~acme.orders.v1",
            "tenant_id": "00000000-0000-0000-0000-000000000002",
            "max_depth": null,
            "barrier_mode": "ignore",
            "types": ["gts.cf.core.events.event.v1~acme.orders.*"],
            "filter": {
                "engine": "gts.cf.core.events.filter.v1~cf.core.expression.cel.v1",
                "expression": "event.data.amount > 100"
            }
        })
    );

    let levels = InterestWire {
        topic: "t".to_owned(),
        tenant_id: uuid(2),
        max_depth: MaxDepthWire::Levels(2),
        barrier_mode: BarrierModeWire::Respect,
        types: vec!["e".to_owned()],
        filter: None,
    };
    assert_eq!(
        serde_json::to_value(&levels).unwrap(),
        json!({
            "topic": "t",
            "tenant_id": "00000000-0000-0000-0000-000000000002",
            "max_depth": 2,
            "barrier_mode": "respect",
            "types": ["e"]
        })
    );
}

#[test]
fn seek_value_is_untagged_int_or_sentinel() {
    assert_eq!(
        serde_json::to_value(SeekValueWire::Exact(42)).unwrap(),
        json!(42)
    );
    assert_eq!(
        serde_json::to_value(SeekValueWire::Sentinel("earliest".to_owned())).unwrap(),
        json!("earliest")
    );
}

#[test]
fn event_type_response_deserialises() {
    let wire: EventTypeWire = serde_json::from_value(json!({
        "id": "gts.cf.core.events.event.v1~acme.orders.created.v1~",
        "topic": "gts.cf.core.events.topic.v1~acme.orders.v1",
        "description": null,
        "allowed_subject_types": ["gts.cf.core.events.subject.v1~acme.orders.order.v1"],
        "partition_key": "/tenant_id",
        "data_schema": { "type": "object" }
    }))
    .unwrap();
    assert_eq!(wire.id, "gts.cf.core.events.event.v1~acme.orders.created.v1~");
    assert_eq!(wire.topic, "gts.cf.core.events.topic.v1~acme.orders.v1");
    assert_eq!(wire.description, None);
    assert_eq!(
        wire.allowed_subject_types,
        vec!["gts.cf.core.events.subject.v1~acme.orders.order.v1".to_owned()]
    );
    assert_eq!(wire.partition_key, "/tenant_id");
    assert_eq!(wire.data_schema, json!({ "type": "object" }));
}

#[test]
fn page_envelope_deserialises() {
    let page: PageWire<TopicWire> = serde_json::from_value(json!({
        "items": [
            { "id": "gts.cf.core.events.topic.v1~acme.orders.v1", "description": "Orders", "retention": "P7D" }
        ],
        "page_info": { "next_cursor": "abc", "prev_cursor": null, "limit": 50 }
    }))
    .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "gts.cf.core.events.topic.v1~acme.orders.v1");
    assert_eq!(page.items[0].description, Some("Orders".to_owned()));
    assert_eq!(page.items[0].retention, Some("P7D".to_owned()));
    assert_eq!(page.page_info.next_cursor, Some("abc".to_owned()));
    assert_eq!(page.page_info.prev_cursor, None);
    assert_eq!(page.page_info.limit, 50);
}

#[test]
fn frame_event_decodes_into_wire_frame() {
    let frame: FrameWire = serde_json::from_value(json!({
        "kind": "event",
        "payload": {
            "id": "00000000-0000-0000-0000-000000000001",
            "type": "gts.cf.core.events.event.v1~acme.orders.created.v1~",
            "topic": "gts.cf.core.events.topic.v1~acme.orders.v1",
            "tenant_id": "00000000-0000-0000-0000-000000000002",
            "source": "orders",
            "subject": "order-1",
            "subject_type": "st",
            "occurred_at": "2026-01-02T03:04:05Z",
            "trace_parent": null,
            "data": { "amount": 100 },
            "partition": 3,
            "sequence": 7,
            "sequence_time": "2026-01-02T03:04:06Z"
        }
    }))
    .unwrap();
    let out = frame.into_frame().unwrap();
    match out {
        crate::api::WireFrame::Event(e) => {
            assert_eq!(e.id, uuid(1));
            assert_eq!(e.type_id, "gts.cf.core.events.event.v1~acme.orders.created.v1~");
            assert_eq!(e.partition, 3);
            assert_eq!(e.sequence, crate::sequence::Sequence::assigned(7));
            assert_eq!(e.data, json!({ "amount": 100 }));
        }
        other => panic!("expected Event frame, got {other:?}"),
    }
}

#[test]
fn frame_control_decodes_code_and_reason() {
    let frame: FrameWire = serde_json::from_value(json!({
        "kind": "control",
        "code": "terminal",
        "positions": [
            { "topic": "gts.cf.core.events.topic.v1~acme.billing.orders.stream.v1", "partition": 0, "offset": 9, "last_examined": 9 }
        ],
        "reason": "rebalance"
    }))
    .unwrap();
    match frame.into_frame().unwrap() {
        crate::api::WireFrame::Control { code, positions, reason } => {
            assert_eq!(code, crate::api::ControlCode::Terminal);
            assert_eq!(positions.len(), 1);
            assert_eq!(positions[0].partition, 0);
            assert_eq!(reason, Some("rebalance".to_owned()));
        }
        other => panic!("expected Control frame, got {other:?}"),
    }
}

#[test]
fn frame_event_missing_sequence_is_transport_error() {
    let frame: FrameWire = serde_json::from_value(json!({
        "kind": "event",
        "payload": {
            "id": "00000000-0000-0000-0000-000000000001",
            "type": "t",
            "topic": "gts.cf.core.events.topic.v1~acme.orders.v1",
            "tenant_id": "00000000-0000-0000-0000-000000000002",
            "source": "s",
            "subject": "sub",
            "subject_type": "st",
            "occurred_at": "2026-01-02T03:04:05Z",
            "trace_parent": null,
            "data": null,
            "partition": 0,
            "sequence": null,
            "sequence_time": null
        }
    }))
    .unwrap();
    assert!(matches!(
        frame.into_frame(),
        Err(EventBrokerError::Transport(_))
    ));
}
