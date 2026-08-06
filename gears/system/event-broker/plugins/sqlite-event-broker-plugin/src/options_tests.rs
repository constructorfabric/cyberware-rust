//! What an operator may write under `backend.options`, and what is refused.

use super::{EventLogPath, SqliteBackendOptions};

fn parse(json: serde_json::Value) -> Result<SqliteBackendOptions, serde_json::Error> {
    serde_json::from_value(json)
}

#[test]
fn a_file_path_names_a_file() {
    let options = parse(serde_json::json!({ "path": "/var/lib/event-broker/event_log.db" }))
        .expect("a filesystem path is a location this backend understands");
    assert_eq!(
        options,
        SqliteBackendOptions {
            path: EventLogPath::File("/var/lib/event-broker/event_log.db".into()),
        }
    );
}

#[test]
fn the_memory_form_names_no_file_at_all() {
    let options = parse(serde_json::json!({ "path": ":memory:" }))
        .expect("`:memory:` is a location this backend understands");
    assert_eq!(
        options,
        SqliteBackendOptions {
            path: EventLogPath::InMemory
        }
    );
}

/// Not a file called `~`: the same shorthand the platform's own `home_dir`
/// setting accepts, so the two behave alike in one configuration file.
#[test]
fn a_leading_tilde_expands_to_the_users_home() {
    let home = std::env::home_dir().expect("this test needs a home directory");
    let options = parse(serde_json::json!({ "path": "~/.cf-gears-event-broker/event_log.db" }))
        .expect("a tilde path is a location this backend understands");
    assert_eq!(
        options,
        SqliteBackendOptions {
            path: EventLogPath::File(home.join(".cf-gears-event-broker/event_log.db")),
        }
    );
}

/// Nothing names a location, so nothing on disk is invented for one.
#[test]
fn omitting_the_path_entirely_keeps_the_log_in_memory() {
    let options = parse(serde_json::json!({})).expect("this backend's options are all optional");
    assert_eq!(
        options,
        SqliteBackendOptions {
            path: EventLogPath::InMemory
        }
    );
}

/// A key written in the belief that it does something is a startup error, not a
/// silently dropped line: the event log would otherwise end up somewhere the
/// operator did not intend, and for this backend that can mean nowhere at all.
#[test]
fn an_unknown_key_is_refused_rather_than_ignored() {
    let error = parse(serde_json::json!({ "path": ":memory:", "wal": true }))
        .expect_err("an option this backend does not understand must not be ignored");
    assert_eq!(error.to_string(), "unknown field `wal`, expected `path`");
}

#[test]
fn an_absolute_path_becomes_a_creating_file_dsn() {
    assert_eq!(
        EventLogPath::File("/var/lib/eb/event_log.db".into()).dsn(),
        "sqlite:///var/lib/eb/event_log.db?mode=rwc"
    );
}

#[test]
fn a_relative_path_stays_relative_to_the_working_directory() {
    assert_eq!(
        EventLogPath::File("data/event_log.db".into()).dsn(),
        "sqlite://./data/event_log.db?mode=rwc"
    );
}

#[test]
fn the_memory_form_opens_no_file() {
    assert_eq!(EventLogPath::InMemory.dsn(), "sqlite::memory:");
    assert!(EventLogPath::InMemory.is_in_memory());
    assert!(!EventLogPath::File("event_log.db".into()).is_in_memory());
}
