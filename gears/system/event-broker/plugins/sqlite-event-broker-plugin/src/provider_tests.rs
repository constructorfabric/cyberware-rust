//! The provider seam: what selects this backend, what it opens, what it
//! refuses.

use event_broker_sdk::Sequence;
use event_broker_sdk::models::Event;
use event_broker_sdk::{EventBrokerBackendProvider, StorageBackendError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{BACKEND_TYPE, SqliteBackendProvider};

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.provider.topic.v1";

fn options(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match json {
        serde_json::Value::Object(map) => map,
        other => panic!("backend options are an object, got {other}"),
    }
}

fn event() -> Event {
    Event {
        id: Uuid::now_v7(),
        type_id: "gts.cf.core.events.event.v1~x.eb.provider.foo.v1".to_owned(),
        tenant_id: Uuid::now_v7(),
        source: "provider-tests".to_owned(),
        subject: "s".to_owned(),
        subject_type: "gts.x.eb.provider.subject.v1~".to_owned(),
        occurred_at: chrono::Utc::now(),
        trace_parent: None,
        data: Some(serde_json::json!({ "hello": "world" })),
        partition: Some(0),
        sequence: None,
        sequence_time: None,
        meta: None,
    }
}

#[test]
fn the_backend_type_is_what_a_topics_backend_block_names() {
    assert_eq!(
        BACKEND_TYPE, "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~",
        "the identifier an operator writes in `backend.type` to select this backend"
    );
    assert_eq!(SqliteBackendProvider.backend_type(), BACKEND_TYPE);
}

#[tokio::test]
async fn an_unrecognized_setting_is_a_startup_error_rather_than_a_dropped_typo() {
    let outcome = SqliteBackendProvider
        .build_backend(&options(serde_json::json!({ "file": "/var/lib/eb.db" })))
        .await;

    let Err(err) = outcome else {
        panic!("a setting this backend does not understand must not be ignored");
    };
    match err {
        StorageBackendError::InvalidConfig { detail, instance } => {
            // The gear hands the block over without judging it, so this is the
            // only place a typo can be caught - and it names the field.
            assert_eq!(instance, BACKEND_TYPE);
            assert_eq!(
                detail,
                format!("{BACKEND_TYPE}: unknown field `file`, expected `path`")
            );
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_options_open_an_event_log_that_outlives_no_process() {
    let backend = SqliteBackendProvider
        .build_backend(&options(serde_json::json!({})))
        .await
        .expect("an event log with no configured location is an in-memory one");

    backend
        .persist(&SecurityContext::anonymous(), TOPIC, 0, &[event()])
        .await
        .expect("the tables this backend owns are applied to the database it opened");
}

/// The whole point of the separation: the file the operator names is the file
/// the events land in, and it is this backend's own - the gear never sees it.
#[tokio::test]
async fn a_configured_path_is_the_file_the_events_land_in() {
    let path = std::env::temp_dir()
        .join(format!("cf-eb-sqlite-plugin-{}", Uuid::now_v7().simple()))
        .join("nested")
        .join("event_log.db");
    let backend = SqliteBackendProvider
        .build_backend(&options(
            serde_json::json!({ "path": path.to_string_lossy() }),
        ))
        .await
        .expect("a file event log must open, creating its parent directories");

    backend
        .persist(&SecurityContext::anonymous(), TOPIC, 0, &[event()])
        .await
        .expect("persist into the configured file");

    assert!(
        path.is_file(),
        "the configured path must be the database that was opened: {}",
        path.display()
    );

    // Reopening the same path finds the event still there: the file is the
    // event log, not a scratch copy of one.
    let reopened = SqliteBackendProvider
        .build_backend(&options(
            serde_json::json!({ "path": path.to_string_lossy() }),
        ))
        .await
        .expect("reopening an existing event log must not re-create it");
    let stored = reopened
        .read(&SecurityContext::anonymous(), TOPIC, 0, Sequence::NONE, 10)
        .await
        .expect("read the reopened event log back");
    assert_eq!(
        stored
            .iter()
            .map(|e| e.sequence.map(Sequence::as_i64))
            .collect::<Vec<_>>(),
        vec![Some(1)],
        "the one event persisted before the reopen is the one event still stored"
    );
}
