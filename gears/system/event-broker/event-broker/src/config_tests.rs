//! Per-topic settings: what an operator may write, and what the shapes admit.
//!
//! What a topic *resolves to* is not here. Resolution folds these entries
//! together with what the topic's own specification declares, which this type
//! cannot see, so the ladder and its outcomes live with
//! `crate::domain::resolution` and are tested there.

use std::time::Duration;

use serde_json::json;

use crate::config::{EventBrokerConfig, RetentionSize, TopicSettingsError};

/// The backend types the fixtures name. Written out rather than imported from
/// the plugin: the gear deliberately does not depend on a backend crate, and an
/// operator writes these strings by hand too.
const SQLITE_BACKEND: &str = "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~";
const POSTGRES_BACKEND: &str = "gts.cf.core.events.backend.v1~cf.core.backend.postgres.v1~";

const TOPIC_TYPE: &str = "gts.cf.core.events.topic.v1~";
const USAGE_TOPIC: &str = "gts.cf.core.events.topic.v1~cf.billing.usage.topic.v1";
const AUDIT_TOPIC: &str = "gts.cf.core.events.topic.v1~cf.billing.audit.topic.v1";
/// A whole `[modules.event_broker]` section, deserialized the way the platform
/// deserializes it, with `topics` supplied by the caller.
fn config_with_topics(topics: &serde_json::Value) -> EventBrokerConfig {
    serde_json::from_value(json!({
        "mode": "standalone",
        "default_storage_backend": SQLITE_BACKEND,
        "topics": topics.clone(),
    }))
    .expect("test configuration deserializes")
}

#[test]
fn a_two_entry_map_deserializes_one_instance_less_key_and_one_fully_qualified() {
    let cfg = config_with_topics(&json!({
        TOPIC_TYPE: { "partitions": 8 },
        USAGE_TOPIC: { "partitions": 32 },
    }));

    assert_eq!(
        cfg.topics.entries.keys().collect::<Vec<_>>(),
        vec![TOPIC_TYPE, USAGE_TOPIC],
        "both keys are present, and neither was folded into the other"
    );
    assert_eq!(cfg.topics.entries[TOPIC_TYPE].partitions, Some(8));
    assert_eq!(cfg.topics.entries[USAGE_TOPIC].partitions, Some(32));
}

#[test]
fn retention_deserializes_a_humantime_duration_and_a_byte_count() {
    let cfg = config_with_topics(&json!({
        TOPIC_TYPE: {
            "partitions": 1,
            "retention": { "duration": "30d", "size_bytes": 128_000_000 },
        },
    }));

    let entry = cfg.topics.entries[TOPIC_TYPE]
        .retention
        .as_ref()
        .expect("the block was written");
    assert_eq!(entry.duration, Some(Duration::from_hours(24 * 30)));
    assert_eq!(entry.size_bytes, RetentionSize::Bytes(128_000_000));
}

/// The distinction the resolver depends on: an entry that omits `duration` is
/// not an entry that asked for the default. Only the absence is visible here;
/// what stands in its place when the topic itself declares a retention is the
/// resolver's to apply, and is asserted there.
#[test]
fn an_entry_that_states_no_duration_is_distinguishable_from_one_that_states_it() {
    let silent = config_with_topics(&json!({
        TOPIC_TYPE: { "partitions": 1, "retention": { "size_bytes": 4096 } },
    }));
    let stated = config_with_topics(&json!({
        TOPIC_TYPE: { "partitions": 1, "retention": { "duration": "168h" } },
    }));

    assert_eq!(
        silent.topics.entries[TOPIC_TYPE]
            .retention
            .as_ref()
            .and_then(|retention| retention.duration),
        None,
        "an omitted duration must stay absent rather than materialising as the default",
    );
    assert_eq!(
        stated.topics.entries[TOPIC_TYPE]
            .retention
            .as_ref()
            .and_then(|retention| retention.duration),
        Some(crate::config::DEFAULT_RETENTION_DURATION),
        "a duration written as the same value the default happens to be is still a statement",
    );
}

/// A byte bound has to be releasable, or a bounded type default would reach
/// every topic of that type with no way out.
#[test]
fn an_explicitly_unbounded_byte_bound_releases_a_topic_from_a_bounded_default() {
    let cfg = config_with_topics(&json!({
        TOPIC_TYPE: {
            "partitions": 1,
            "retention": { "duration": "30d", "size_bytes": 128_000_000 },
        },
        USAGE_TOPIC: { "retention": { "size_bytes": null } },
    }));

    assert_eq!(
        cfg.topics.entries[USAGE_TOPIC]
            .retention
            .as_ref()
            .map(|retention| retention.size_bytes),
        Some(RetentionSize::Unbounded),
        "an explicit null is a statement, not an omission",
    );
}

#[test]
fn an_unknown_key_in_a_topic_entry_is_a_deserialization_error() {
    let err = serde_json::from_value::<EventBrokerConfig>(json!({
        "mode": "standalone",
        "default_storage_backend": SQLITE_BACKEND,
        "topics": { TOPIC_TYPE: { "partition": 8 } },
    }))
    .expect_err("a typo must not be silently ignored");
    assert!(
        err.to_string().contains("unknown field `partition`"),
        "expected the typo to be named, got: {err}"
    );
}

// ── The one backend the instance runs ─────────────────────────────────────

#[test]
fn the_backend_is_built_from_the_selection_the_configured_topics_name() {
    let cfg = config_with_topics(&json!({
        TOPIC_TYPE: {
            "partitions": 1,
            "backend": { "type": SQLITE_BACKEND, "path": "/var/lib/event-broker/event_log.db" },
        },
    }));

    let selection = cfg.backend_selection().expect("one entry cannot disagree");
    assert_eq!(selection.r#type.as_ref(), SQLITE_BACKEND);
    assert_eq!(
        selection.settings,
        json!({ "path": "/var/lib/event-broker/event_log.db" })
            .as_object()
            .expect("a settings block is an object")
            .clone()
    );
}

/// Nothing on disk is invented for a deployment that named no backend: it gets
/// the deployment default with no settings, and what that means is the
/// backend's to decide.
#[test]
fn configuration_naming_no_backend_at_all_yields_the_deployment_default() {
    let cfg = config_with_topics(&json!({
        TOPIC_TYPE: { "partitions": 1, "retention": { "duration": "1h" } },
    }));

    let selection = cfg.backend_selection().expect("no entry can disagree");
    assert_eq!(selection.r#type.as_ref(), SQLITE_BACKEND);
    assert_eq!(selection.settings, serde_json::Map::new());
}

/// One instance runs one backend, so two entries describing different ones is
/// an operator mistake worth failing on: picking one would store the other
/// topic's events somewhere its own configuration never named.
#[test]
fn two_entries_naming_different_settings_fail_loudly() {
    let cfg = config_with_topics(&json!({
        AUDIT_TOPIC: { "backend": { "type": SQLITE_BACKEND, "path": "/var/lib/eb/audit.db" } },
        USAGE_TOPIC: { "backend": { "type": SQLITE_BACKEND, "path": "/var/lib/eb/usage.db" } },
    }));

    assert_eq!(
        cfg.backend_selection()
            .expect_err("two different event logs cannot both be the one this instance opens"),
        TopicSettingsError::BackendsDisagree {
            first: AUDIT_TOPIC.to_owned(),
            second: USAGE_TOPIC.to_owned(),
        }
    );
}

/// The type is half the statement, so two entries agreeing on their settings
/// and disagreeing on which backend holds them is the same mistake.
#[test]
fn two_entries_naming_different_backend_types_fail_loudly() {
    let cfg = config_with_topics(&json!({
        AUDIT_TOPIC: { "backend": { "type": SQLITE_BACKEND } },
        USAGE_TOPIC: { "backend": { "type": POSTGRES_BACKEND } },
    }));

    assert_eq!(
        cfg.backend_selection()
            .expect_err("one instance cannot run two backends"),
        TopicSettingsError::BackendsDisagree {
            first: AUDIT_TOPIC.to_owned(),
            second: USAGE_TOPIC.to_owned(),
        }
    );
}

/// An entry with no backend block of its own says nothing about the backend,
/// the same way it inherits everything else it does not name.
#[test]
fn an_entry_with_no_backend_block_does_not_conflict_with_one_that_has_it() {
    let cfg = config_with_topics(&json!({
        TOPIC_TYPE: { "partitions": 1, "backend": { "type": SQLITE_BACKEND, "path": ":memory:" } },
        USAGE_TOPIC: { "partitions": 32 },
        AUDIT_TOPIC: { "retention": { "duration": "1h" } },
    }));

    let selection = cfg
        .backend_selection()
        .expect("silence is not disagreement");
    assert_eq!(selection.r#type.as_ref(), SQLITE_BACKEND);
    assert_eq!(
        selection.settings,
        json!({ "path": ":memory:" })
            .as_object()
            .expect("a settings block is an object")
            .clone()
    );
}

// ── What the block itself admits ──────────────────────────────────────────

/// A block that names a backend and leaves out which one is a mistake, not a
/// request for the deployment default: omitting the whole block is how an entry
/// says nothing.
#[test]
fn a_backend_block_with_no_type_fails_to_deserialize() {
    let err = serde_json::from_value::<EventBrokerConfig>(json!({
        "mode": "standalone",
        "default_storage_backend": SQLITE_BACKEND,
        "topics": { TOPIC_TYPE: { "partitions": 1, "backend": { "path": ":memory:" } } },
    }))
    .expect_err("a backend block must say which backend");
    assert!(
        err.to_string().contains("missing field `type`"),
        "expected the missing field to be named, got: {err}"
    );
}

/// The gear holds a backend's own settings opaque, so an unrecognised key
/// travels to the plugin instead of being refused here - the plugin publishes
/// the schema that can judge it.
#[test]
fn an_unrecognised_key_inside_a_backend_block_reaches_the_backend() {
    let cfg = config_with_topics(&json!({
        TOPIC_TYPE: {
            "partitions": 1,
            "backend": { "type": SQLITE_BACKEND, "pathh": ":memory:" },
        },
    }));

    let selection = cfg.backend_selection().expect("the gear does not judge it");
    assert_eq!(
        selection.settings,
        json!({ "pathh": ":memory:" })
            .as_object()
            .expect("a settings block is an object")
            .clone(),
        "the typo is handed on verbatim; the plugin's own type is what rejects it"
    );
}

/// A short alias is no longer a backend name. Failing at load beats resolving
/// to whichever backend happened to be linked into the build.
#[test]
fn a_short_alias_as_the_deployment_backend_fails_to_deserialize() {
    let err = serde_json::from_value::<EventBrokerConfig>(json!({
        "mode": "standalone",
        "default_storage_backend": "sqlite",
        "topics": {},
    }))
    .expect_err("a backend is named by its GTS type");
    assert!(
        err.to_string().contains("sqlite"),
        "expected the rejected value to be named, got: {err}"
    );
}
