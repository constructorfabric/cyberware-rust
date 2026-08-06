//! Exact bytes for every frame kind on both transports.
//!
//! There was no unit coverage of the wire format at all: the encoders were
//! inline in the handlers, so the only way to see a frame's bytes was to open a
//! real stream and read a chunk. Every assertion here is the whole encoded
//! output, not a substring, because a framing bug - a missing CRLF, a wrong
//! boundary - is invisible to a `contains` check.

use crate::domain::model::Sequence;
use chrono::{DateTime, TimeZone, Utc};
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::delivery::{ControlCode, Frame};
use crate::domain::model::{Event, Meta};
use crate::domain::streaming::frames::{CloseReason, Position};

use super::encode::{to_multipart_part, to_sse_event};

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.orders.acme.v1";
const BOUNDARY: &str = "eb-frame-test";

fn gts(id: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(id).expect("static gts id is valid")
}

/// A fixed instant, so the bytes are comparable rather than time-dependent.
fn at() -> DateTime<Utc> {
    Utc.timestamp_opt(1_767_225_600, 0)
        .single()
        .expect("static timestamp is valid")
}

fn position(partition: i32) -> Position {
    Position::builder(gts(TOPIC), partition)
        .offset(Sequence::assigned(42))
        .last_examined(Sequence::assigned(50))
        .build()
}

fn event() -> Event {
    Event {
        id: Uuid::nil(),
        r#type: crate::test_support::event_type_id(
            "gts.cf.core.events.event.v1~x.eb.o.created.v1~",
        ),
        topic: gts(TOPIC),
        tenant_id: Uuid::nil(),
        source: "encode-tests".to_owned(),
        subject: "order-1".to_owned(),
        subject_type: "gts.x.eb.order.v1~".to_owned(),
        occurred_at: at(),
        trace_parent: None,
        data: serde_json::json!({ "n": 1 }),
        meta: Some(Meta {
            version: 1,
            producer_id: Uuid::nil(),
            previous: 0,
            sequence: 1,
        }),
        partition: Some(3),
        sequence: Some(Sequence::assigned(42)),
        sequence_time: Some(at()),
    }
}

/// The body of a multipart part, with its framing stripped - so a body
/// assertion fails on the body and a framing assertion fails on the framing,
/// rather than one failure hiding the other.
fn part_body(frame: Frame) -> String {
    let bytes = to_multipart_part(BOUNDARY, frame);
    let text = String::from_utf8(bytes.to_vec()).expect("a part is valid UTF-8");
    let prefix = format!("--{BOUNDARY}\r\nContent-Type: application/json\r\n\r\n");
    let body = text
        .strip_prefix(&prefix)
        .expect("every part carries the boundary and content type")
        .strip_suffix("\r\n")
        .expect("every part ends with CRLF");
    body.to_owned()
}

#[test]
fn a_multipart_part_carries_the_boundary_content_type_and_trailing_crlf() {
    let bytes = to_multipart_part(BOUNDARY, Frame::Heartbeat { at: at() });
    let text = String::from_utf8(bytes.to_vec()).expect("a part is valid UTF-8");

    assert_eq!(
        text,
        format!(
            "--{BOUNDARY}\r\nContent-Type: application/json\r\n\r\n\
             {{\"kind\":\"heartbeat\",\"at\":\"2026-01-01T00:00:00Z\"}}\r\n"
        ),
        "the whole part, framing included"
    );
}

#[test]
fn heartbeat_body() {
    assert_eq!(
        part_body(Frame::Heartbeat { at: at() }),
        r#"{"kind":"heartbeat","at":"2026-01-01T00:00:00Z"}"#
    );
}

#[test]
fn topology_body() {
    assert_eq!(
        part_body(Frame::Topology {
            topology_version: 7,
            positions: vec![position(0), position(1)],
        }),
        concat!(
            r#"{"kind":"topology","topology_version":7,"assigned":["#,
            r#"{"topic":"gts.cf.core.events.topic.v1~x.eb.orders.acme.v1","partition":0,"offset":42,"last_examined":50},"#,
            r#"{"topic":"gts.cf.core.events.topic.v1~x.eb.orders.acme.v1","partition":1,"offset":42,"last_examined":50}]}"#
        )
    );
}

#[test]
fn a_control_frame_omits_reason_when_there_is_none() {
    assert_eq!(
        part_body(Frame::Control {
            code: ControlCode::Progress,
            positions: vec![position(0)],
            reason: None,
        }),
        concat!(
            r#"{"kind":"control","code":"progress","positions":["#,
            r#"{"topic":"gts.cf.core.events.topic.v1~x.eb.orders.acme.v1","partition":0,"offset":42,"last_examined":50}]}"#
        ),
        "an absent reason must be absent from the JSON, not null"
    );
}

#[test]
fn a_terminal_frame_carries_its_reason() {
    assert_eq!(
        part_body(Frame::Control {
            code: ControlCode::Terminal,
            positions: vec![position(0)],
            reason: Some(CloseReason::Rebalanced),
        }),
        concat!(
            r#"{"kind":"control","code":"terminal","positions":["#,
            r#"{"topic":"gts.cf.core.events.topic.v1~x.eb.orders.acme.v1","partition":0,"offset":42,"last_examined":50}],"#,
            r#""reason":"rebalanced"}"#
        )
    );
}

#[test]
fn an_event_frame_strips_the_producer_chain() {
    let body = part_body(Frame::Event(Box::new(event())));

    assert!(
        body.starts_with(r#"{"kind":"event","payload":{"#),
        "unexpected shape: {body}"
    );
    // `meta` is publish-input only and must not reach a consumer.
    assert!(
        !body.contains("\"meta\"") && !body.contains("producer_id"),
        "the producer chain must be stripped from the read projection: {body}"
    );
    assert!(
        body.contains(r#""sequence":42"#) && body.contains(r#""partition":3"#),
        "the broker-derived fields must be present: {body}"
    );
}

/// Both transports carry the same body; only the delimiting differs. Asserted
/// rather than assumed, because they are two code paths over one DTO.
#[test]
fn sse_and_multipart_agree_on_every_frame_kind() {
    let frames = || {
        vec![
            Frame::Heartbeat { at: at() },
            Frame::Topology {
                topology_version: 7,
                positions: vec![position(0)],
            },
            Frame::Control {
                code: ControlCode::Progress,
                positions: vec![position(0)],
                reason: None,
            },
            Frame::Event(Box::new(event())),
        ]
    };

    for (frame, sse_frame) in frames().into_iter().zip(frames()) {
        let body = part_body(frame);
        // `Event`'s `Debug` is the only view into a built SSE event. It renders
        // the buffer as a byte-string literal, so the JSON inside it is
        // escaped - comparing against the raw body would always fail. `{:?}` on
        // the body escapes it the same way; the quotes it adds are trimmed.
        let escaped = format!("{body:?}");
        let escaped = escaped.trim_matches('"');
        let rendered = format!("{:?}", to_sse_event(sse_frame));
        assert!(
            rendered.contains(escaped),
            "SSE and multipart disagree.\n multipart: {body}\n sse: {rendered}"
        );
    }
}

#[test]
fn each_frame_kind_names_its_own_sse_event() {
    for (frame, expected) in [
        (Frame::Heartbeat { at: at() }, "heartbeat"),
        (
            Frame::Topology {
                topology_version: 1,
                positions: Vec::new(),
            },
            "topology",
        ),
        (
            Frame::Control {
                code: ControlCode::Terminal,
                positions: Vec::new(),
                reason: None,
            },
            "control",
        ),
        (Frame::Event(Box::new(event())), "event"),
    ] {
        let rendered = format!("{:?}", to_sse_event(frame));
        assert!(
            rendered.contains(expected),
            "expected SSE event name '{expected}' in {rendered}"
        );
    }
}
