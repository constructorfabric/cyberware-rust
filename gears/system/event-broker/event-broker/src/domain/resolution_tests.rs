//! The ladder, one case per rung, and the provenance each rung produces.
//!
//! Every case goes through [`super::resolve`], the one entry point production
//! code calls, so a passing test is evidence about the path a caller takes.

use std::time::Duration;

use serde_json::json;
use toolkit_gts::GtsInstanceId;

use super::{Declaration, Source, resolve};
use crate::config::{DEFAULT_RETENTION_DURATION, EventBrokerConfig};

const TOPIC_TYPE: &str = "gts.cf.core.events.topic.v1~";
const USAGE_TOPIC: &str = "gts.cf.core.events.topic.v1~cf.billing.usage.topic.v1";
const SQLITE_BACKEND: &str = "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~";
const POSTGRES_BACKEND: &str = "gts.cf.core.events.backend.v1~cf.core.backend.postgres.v1~";

fn topic() -> GtsInstanceId {
    GtsInstanceId::try_new(USAGE_TOPIC).expect("test topic id is a valid GTS instance id")
}

fn config(topics: &serde_json::Value) -> EventBrokerConfig {
    serde_json::from_value(json!({
        "mode": "standalone",
        "default_storage_backend": SQLITE_BACKEND,
        "topics": topics.clone(),
    }))
    .expect("test configuration deserializes")
}

fn declaring(retention: Duration) -> Declaration {
    Declaration {
        retention: Some(retention),
    }
}

/// One rung of the retention ladder: what configuration says, what the topic
/// declares, and the value and source that must come out.
struct Rung {
    what: &'static str,
    topics: serde_json::Value,
    declared: Declaration,
    duration: Duration,
    source: Source,
}

#[test]
fn each_rung_of_the_retention_ladder_wins_only_over_the_ones_below_it() {
    let rungs = vec![
        Rung {
            what: "nothing states a duration, so the built-in stands",
            topics: json!({}),
            declared: Declaration::default(),
            duration: DEFAULT_RETENTION_DURATION,
            source: Source::BuiltIn,
        },
        Rung {
            what: "the topic declares one and configuration is silent",
            topics: json!({}),
            declared: declaring(Duration::from_hours(72)),
            duration: Duration::from_hours(72),
            source: Source::Specification,
        },
        Rung {
            what: "an operator's entry for the type overrides the declaration",
            topics: json!({ TOPIC_TYPE: { "retention": { "duration": "30d" } } }),
            declared: declaring(Duration::from_hours(72)),
            duration: Duration::from_hours(24 * 30),
            source: Source::TypeEntry,
        },
        Rung {
            what: "the topic's own entry beats the entry for its type",
            topics: json!({
                TOPIC_TYPE: { "retention": { "duration": "30d" } },
                USAGE_TOPIC: { "retention": { "duration": "1h" } },
            }),
            declared: declaring(Duration::from_hours(72)),
            duration: Duration::from_hours(1),
            source: Source::TopicEntry,
        },
        Rung {
            what: "an entry for the type with no duration does not displace the declaration",
            topics: json!({ TOPIC_TYPE: { "retention": { "size_bytes": 4096 } } }),
            declared: declaring(Duration::from_hours(72)),
            duration: Duration::from_hours(72),
            source: Source::Specification,
        },
    ];

    for rung in rungs {
        let settings = resolve(&config(&rung.topics), &topic(), &rung.declared)
            .unwrap_or_else(|err| panic!("{}: {err}", rung.what));
        assert_eq!(
            settings.retention().value().duration,
            rung.duration,
            "{}",
            rung.what
        );
        assert_eq!(settings.retention().source(), rung.source, "{}", rung.what);
    }
}

/// The byte bound has no declared tier - a specification cannot say how much
/// disk a deployment spends - so it resolves from configuration alone and is
/// unbounded when nothing names it.
#[test]
fn a_setting_the_specification_cannot_express_resolves_from_configuration_alone() {
    let unstated = resolve(
        &config(&json!({ TOPIC_TYPE: { "retention": { "duration": "30d" } } })),
        &topic(),
        &declaring(Duration::from_hours(72)),
    )
    .expect("resolves");
    assert_eq!(unstated.retention().value().size_bytes, None);

    let stated = resolve(
        &config(&json!({ TOPIC_TYPE: { "retention": { "size_bytes": 128_000_000 } } })),
        &topic(),
        &declaring(Duration::from_hours(72)),
    )
    .expect("resolves");
    assert_eq!(
        stated.retention().value().size_bytes,
        Some(128_000_000),
        "the bound is configuration's whatever the topic declares"
    );
    assert_eq!(
        stated.retention().source(),
        Source::Specification,
        "and the declaration still owns the duration beside it"
    );
}

#[test]
fn a_partition_count_reports_the_entry_that_supplied_it() {
    let built_in =
        resolve(&config(&json!({})), &topic(), &Declaration::default()).expect("resolves");
    assert_eq!(*built_in.partitions().value(), 8);
    assert_eq!(built_in.partitions().source(), Source::BuiltIn);

    let from_type = resolve(
        &config(&json!({ TOPIC_TYPE: { "partitions": 4 } })),
        &topic(),
        &Declaration::default(),
    )
    .expect("resolves");
    assert_eq!(*from_type.partitions().value(), 4);
    assert_eq!(from_type.partitions().source(), Source::TypeEntry);

    let from_topic = resolve(
        &config(&json!({
            TOPIC_TYPE: { "partitions": 4 },
            USAGE_TOPIC: { "partitions": 32 },
        })),
        &topic(),
        &Declaration::default(),
    )
    .expect("resolves");
    assert_eq!(*from_topic.partitions().value(), 32);
    assert_eq!(from_topic.partitions().source(), Source::TopicEntry);
}

#[test]
fn a_backend_reports_the_entry_that_supplied_it_and_resolves_whole() {
    let built_in =
        resolve(&config(&json!({})), &topic(), &Declaration::default()).expect("resolves");
    assert_eq!(built_in.backend().value().r#type.as_ref(), SQLITE_BACKEND);
    assert!(built_in.backend().value().settings.is_empty());
    assert_eq!(built_in.backend().source(), Source::BuiltIn);

    let overridden = resolve(
        &config(&json!({
            TOPIC_TYPE: { "backend": { "type": SQLITE_BACKEND, "path": ":memory:" } },
            USAGE_TOPIC: { "backend": { "type": POSTGRES_BACKEND, "host": "db-1" } },
        })),
        &topic(),
        &Declaration::default(),
    )
    .expect("resolves");
    assert_eq!(
        overridden.backend().value().r#type.as_ref(),
        POSTGRES_BACKEND
    );
    assert_eq!(
        overridden.backend().value().settings,
        json!({ "host": "db-1" })
            .as_object()
            .expect("an object literal")
            .clone(),
        "the block resolves whole, so the type default's `path` does not leak into it"
    );
    assert_eq!(overridden.backend().source(), Source::TopicEntry);
}

#[test]
fn a_partition_count_below_one_is_refused() {
    let err = resolve(
        &config(&json!({ TOPIC_TYPE: { "partitions": 0 } })),
        &topic(),
        &Declaration::default(),
    )
    .expect_err("zero partitions cannot describe a partitioned topic");
    assert_eq!(
        err,
        crate::config::TopicSettingsError::PartitionsOutOfRange {
            topic: USAGE_TOPIC.to_owned(),
            partitions: 0,
        }
    );
}

/// The type default reaches every topic of that type, and an override reaches
/// only its own - the property that makes one entry safe to add.
#[test]
fn an_entry_for_one_topic_leaves_every_other_topic_of_the_type_alone() {
    let cfg = config(&json!({
        TOPIC_TYPE: { "partitions": 8 },
        USAGE_TOPIC: { "partitions": 32 },
    }));
    let audit = GtsInstanceId::try_new("gts.cf.core.events.topic.v1~cf.billing.audit.topic.v1")
        .expect("a valid GTS instance id");

    assert_eq!(
        *resolve(&cfg, &topic(), &Declaration::default())
            .expect("resolves")
            .partitions()
            .value(),
        32,
    );
    assert_eq!(
        *resolve(&cfg, &audit, &Declaration::default())
            .expect("resolves")
            .partitions()
            .value(),
        8,
        "the entry for one topic is not an entry for its neighbours",
    );
}

/// A topic of a type nobody configured is not an error and not unbounded: the
/// built-in tier reaches it, because configuration always carries an entry for
/// the topic type.
#[test]
fn a_topic_whose_type_has_no_entry_resolves_to_the_built_in_tier() {
    // An instance of some other base type: what matters is that no entry is
    // keyed under its type.
    let other_type =
        GtsInstanceId::try_new("gts.cf.core.events.subscription.v1~cf.billing.usage.sub.v1")
            .expect("a valid GTS instance id");

    let settings = resolve(
        &config(&json!({ TOPIC_TYPE: { "partitions": 32 } })),
        &other_type,
        &Declaration::default(),
    )
    .expect("an unconfigured type is not an error");
    assert_eq!(*settings.partitions().value(), 8);
    assert_eq!(settings.partitions().source(), Source::BuiltIn);
}

/// Releasing a byte bound is a statement about that bound alone: the duration
/// the entry inherited stays where it was.
#[test]
fn an_explicitly_unbounded_byte_bound_releases_only_the_byte_bound() {
    let settings = resolve(
        &config(&json!({
            TOPIC_TYPE: { "retention": { "duration": "30d", "size_bytes": 128_000_000 } },
            USAGE_TOPIC: { "retention": { "size_bytes": null } },
        })),
        &topic(),
        &Declaration::default(),
    )
    .expect("resolves");

    assert_eq!(settings.retention().value().size_bytes, None);
    assert_eq!(
        settings.retention().value().duration,
        Duration::from_hours(24 * 30),
        "the inherited duration is untouched",
    );
}

/// An unstated byte bound is not a statement, so a bounded type default still
/// reaches a topic that raised only its duration.
#[test]
fn an_unstated_byte_bound_inherits_the_type_defaults_bound() {
    let settings = resolve(
        &config(&json!({
            TOPIC_TYPE: { "retention": { "duration": "30d", "size_bytes": 128_000_000 } },
            USAGE_TOPIC: { "retention": { "duration": "1h" } },
        })),
        &topic(),
        &Declaration::default(),
    )
    .expect("resolves");

    assert_eq!(
        settings.retention().value().duration,
        Duration::from_hours(1)
    );
    assert_eq!(settings.retention().value().size_bytes, Some(128_000_000));
}
