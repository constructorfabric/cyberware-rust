//! Decoder tests: both framings, fed the same frames, decode to the same
//! sequence. Mirrors the server encoders in
//! `event-broker/src/api/rest/handlers/delivery/encode.rs`.

use super::*;

const HEARTBEAT: &str = r#"{"kind":"heartbeat","at":"2026-01-02T03:04:05Z"}"#;
const TOPOLOGY: &str = r#"{"kind":"topology","topology_version":2,"assigned":[]}"#;

fn as_values(frames: Vec<FrameWire>) -> Vec<serde_json::Value> {
    frames
        .into_iter()
        .map(|f| serde_json::to_value(f).unwrap())
        .collect()
}

fn sse_blob() -> Vec<u8> {
    // axum SSE: `event: <kind>` then `data: <json>` then a blank line.
    format!("event: heartbeat\ndata: {HEARTBEAT}\n\nevent: topology\ndata: {TOPOLOGY}\n\n")
        .into_bytes()
}

fn multipart_blob(boundary: &str) -> Vec<u8> {
    // Mirrors `to_multipart_part`: boundary, JSON content type, blank line, body,
    // trailing CRLF.
    format!(
        "--{boundary}\r\nContent-Type: application/json\r\n\r\n{HEARTBEAT}\r\n\
         --{boundary}\r\nContent-Type: application/json\r\n\r\n{TOPOLOGY}\r\n"
    )
    .into_bytes()
}

#[test]
fn sse_and_multipart_decode_to_the_same_frames() {
    let mut sse = Decoder::sse();
    sse.push(&sse_blob());
    let sse_frames = as_values(sse.drain().unwrap());

    let mut mp = Decoder::multipart("evbk-boundary".to_owned());
    mp.push(&multipart_blob("evbk-boundary"));
    let mp_frames = as_values(mp.drain().unwrap());

    assert_eq!(sse_frames, mp_frames);
    assert_eq!(
        sse_frames,
        vec![
            serde_json::from_str::<serde_json::Value>(HEARTBEAT).unwrap(),
            serde_json::from_str::<serde_json::Value>(TOPOLOGY).unwrap(),
        ]
    );
}

#[test]
fn sse_buffers_a_partial_event_across_chunks() {
    let blob = sse_blob();
    let split = blob.len() / 2;
    let mut sse = Decoder::sse();
    sse.push(&blob[..split]);
    let first = sse.drain().unwrap();
    sse.push(&blob[split..]);
    let mut all = as_values(first);
    all.extend(as_values(sse.drain().unwrap()));
    assert_eq!(all.len(), 2);
    assert_eq!(all[0], serde_json::from_str::<serde_json::Value>(HEARTBEAT).unwrap());
    assert_eq!(all[1], serde_json::from_str::<serde_json::Value>(TOPOLOGY).unwrap());
}
