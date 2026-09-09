use std::collections::BTreeSet;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use toolkit::var_expand::ExpandVars;

use super::*;

/// The two fields [`RedisClusterConfig`] carries and [`RedisLockConfig`] does
/// not, per DESIGN.md §8: each is meaningless without the cache half.
const CACHE_ONLY_FIELDS: [&str; 2] = ["watch_mode", "manage_keyspace_notifications"];

fn minimal(extra: Value) -> Value {
    let mut config = json!({ "url": "redis://redis:6379/0" });
    let (Value::Object(target), Value::Object(source)) = (&mut config, extra) else {
        panic!("both halves must be JSON objects");
    };
    target.extend(source);
    config
}

#[test]
fn cluster_config_applies_documented_defaults() {
    let config: RedisClusterConfig =
        serde_json::from_value(minimal(json!({}))).expect("minimal config deserializes");
    assert_eq!(config.pool_size, 4);
    assert_eq!(config.command_timeout_ms, 5_000);
    assert_eq!(config.key_prefix, "cluster");
    assert_eq!(config.database, 0);
    assert_eq!(config.topology, None);
    assert_eq!(config.durability, None);
    assert_eq!(config.wait_replicas, None);
    assert_eq!(config.wait_timeout_ms, 1_000);
    assert_eq!(config.watch_mode, WatchMode::Publish);
    assert!(!config.manage_keyspace_notifications);
}

#[test]
fn lock_config_applies_the_same_defaults_for_the_fields_it_shares() {
    let config: RedisLockConfig =
        serde_json::from_value(minimal(json!({}))).expect("minimal lock config deserializes");
    assert_eq!(config.pool_size, 4);
    assert_eq!(config.command_timeout_ms, 5_000);
    assert_eq!(config.key_prefix, "cluster");
    assert_eq!(config.database, 0);
    assert_eq!(config.topology, None);
    assert_eq!(config.durability, None);
    assert_eq!(config.wait_replicas, None);
    assert_eq!(config.wait_timeout_ms, 1_000);
}

#[test]
fn cluster_config_round_trips_every_field() {
    let config: RedisClusterConfig = serde_json::from_value(json!({
        "url": "rediss://:pw@redis-primary:6379/3",
        "pool_size": 8,
        "command_timeout_ms": 1_234,
        "key_prefix": "gears",
        "database": 3,
        "topology": "sentinel",
        "durability": "fsync_everysec",
        "wait_replicas": 1,
        "wait_timeout_ms": 2_500,
        "watch_mode": "disabled",
        "manage_keyspace_notifications": true,
    }))
    .expect("full config deserializes");
    assert_eq!(config.url, "rediss://:pw@redis-primary:6379/3");
    assert_eq!(config.pool_size, 8);
    assert_eq!(config.command_timeout_ms, 1_234);
    assert_eq!(config.key_prefix, "gears");
    assert_eq!(config.database, 3);
    assert_eq!(config.topology, Some(Topology::Sentinel));
    assert_eq!(config.durability, Some(Durability::FsyncEverysec));
    assert_eq!(config.wait_replicas, Some(1));
    assert_eq!(config.wait_timeout_ms, 2_500);
    assert_eq!(config.watch_mode, WatchMode::Disabled);
    assert!(config.manage_keyspace_notifications);
}

#[test]
fn every_enum_variant_round_trips() {
    for (raw, expected) in [
        ("standalone", Topology::Standalone),
        ("sentinel", Topology::Sentinel),
        ("cluster", Topology::Cluster),
    ] {
        let config: RedisClusterConfig =
            serde_json::from_value(minimal(json!({ "topology": raw })))
                .expect("a topology hint deserializes");
        assert_eq!(config.topology, Some(expected));
    }

    for (raw, expected) in [
        ("fsync_always", Durability::FsyncAlways),
        ("fsync_everysec", Durability::FsyncEverysec),
        ("none", Durability::None),
    ] {
        let config: RedisClusterConfig =
            serde_json::from_value(minimal(json!({ "durability": raw })))
                .expect("a durability hint deserializes");
        assert_eq!(config.durability, Some(expected));
    }

    for (raw, expected) in [
        ("publish", WatchMode::Publish),
        ("disabled", WatchMode::Disabled),
    ] {
        let config: RedisClusterConfig =
            serde_json::from_value(minimal(json!({ "watch_mode": raw })))
                .expect("a watch_mode deserializes");
        assert_eq!(config.watch_mode, expected);
    }
}

#[test]
fn unknown_enum_variants_are_rejected() {
    // `watch_mode: keyspace` is the specific one worth pinning: DESIGN.md §13
    // D4 considered a third mode and decided against it, so an operator who
    // read the design and tried it must get an error rather than a silent
    // fall back to `publish`.
    for (field, raw) in [
        ("topology", "redis-cluster"),
        ("durability", "fsync"),
        ("watch_mode", "keyspace"),
    ] {
        let result: Result<RedisClusterConfig, _> =
            serde_json::from_value(minimal(json!({ field: raw })));
        assert!(
            result.is_err(),
            "an unknown `{field}` variant `{raw}` must be rejected"
        );
    }
}

#[test]
fn an_operator_typo_is_rejected_rather_than_ignored() {
    // `deny_unknown_fields` is what makes this a startup error instead of a
    // pool that quietly stays at its default size.
    let result: Result<RedisClusterConfig, _> =
        serde_json::from_value(minimal(json!({ "pool_sise": 8 })));
    let err = result.expect_err("a misspelled field must be rejected");
    assert!(
        err.to_string().contains("pool_sise"),
        "the error must name the key the operator wrote, got {err}"
    );
}

#[test]
fn the_lock_config_rejects_the_cache_only_fields() {
    // Not merely "unknown": binding a lock-only provider and setting
    // `watch_mode` means the operator expects a watch they will not get.
    for field in CACHE_ONLY_FIELDS {
        let value = if field == "manage_keyspace_notifications" {
            json!(true)
        } else if field == "watch_mode" {
            json!("publish")
        } else {
            json!(1_000)
        };
        let result: Result<RedisLockConfig, _> =
            serde_json::from_value(minimal(json!({ field: value })));
        assert!(
            result.is_err(),
            "RedisLockConfig must reject the cache-only field `{field}`"
        );
    }
}

/// Reads a type's accepted field names back out of the `unknown field …,
/// expected one of …` error `deny_unknown_fields` produces.
///
/// The mechanical half of the drift guard below. Deriving the field set from
/// the type itself is what makes the guard real: a list written by hand here
/// would have to be updated by the same person who forgot to update the second
/// config type, so it would pass exactly when it should fail.
///
/// If serde ever changes that message, this panics rather than silently
/// returning an empty set and reporting two types as identical.
fn accepted_fields<T: DeserializeOwned>() -> BTreeSet<String> {
    let result: Result<T, _> =
        serde_json::from_value(json!({ "a_field_no_config_type_declares": true }));
    let message = result
        .err()
        .expect("deny_unknown_fields must reject an unknown key")
        .to_string();
    let (_, expected) = message
        .split_once("expected one of ")
        .expect("serde's unknown-field error must list the accepted fields");
    expected
        .split(',')
        .filter_map(|field| field.trim().split('`').nth(1))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_two_config_types_have_not_drifted() {
    // DESIGN.md §8's "the two config types cannot drift" requirement, enforced
    // the only way it can be once the shared subset is duplicated rather than
    // flattened (see the module docs for why it has to be): the lock config's
    // fields must be exactly the cluster config's, minus the three cache-only
    // ones. Adding a field to one type and not the other fails here.
    let cluster = accepted_fields::<RedisClusterConfig>();
    let lock = accepted_fields::<RedisLockConfig>();

    let cache_only: BTreeSet<String> = CACHE_ONLY_FIELDS.iter().map(|f| (*f).to_owned()).collect();
    let expected_lock_fields: BTreeSet<String> = cluster.difference(&cache_only).cloned().collect();

    assert_eq!(
        lock, expected_lock_fields,
        "RedisLockConfig must carry exactly RedisClusterConfig's fields minus {CACHE_ONLY_FIELDS:?}"
    );
    assert!(
        cache_only.is_subset(&cluster),
        "every field named cache-only must actually exist on RedisClusterConfig"
    );
}

#[test]
fn the_shared_fields_deserialize_identically_in_both_types() {
    // The value half of the drift guard: matching field *names* would still
    // allow the two types to disagree on a default or a type.
    let shared = json!({
        "url": "redis://:pw@h:6379/1",
        "pool_size": 9,
        "command_timeout_ms": 111,
        "key_prefix": "shared",
        "database": 1,
        "topology": "cluster",
        "durability": "fsync_always",
        "wait_replicas": 2,
        "wait_timeout_ms": 222,
    });
    let cluster: RedisClusterConfig =
        serde_json::from_value(shared.clone()).expect("cluster config deserializes");
    let lock: RedisLockConfig =
        serde_json::from_value(shared).expect("lock config deserializes from the same object");

    assert_eq!(cluster.url, lock.url);
    assert_eq!(cluster.pool_size, lock.pool_size);
    assert_eq!(cluster.command_timeout_ms, lock.command_timeout_ms);
    assert_eq!(cluster.key_prefix, lock.key_prefix);
    assert_eq!(cluster.database, lock.database);
    assert_eq!(cluster.topology, lock.topology);
    assert_eq!(cluster.durability, lock.durability);
    assert_eq!(cluster.wait_replicas, lock.wait_replicas);
    assert_eq!(cluster.wait_timeout_ms, lock.wait_timeout_ms);
}

#[test]
fn url_expands_a_default_when_the_var_is_unset() {
    let mut config: RedisClusterConfig = serde_json::from_value(json!({
        "url": "redis://:${REDIS_CLUSTER_PW_UNSET:-fallbackpw}@redis:6379/0",
    }))
    .expect("config deserializes");
    config
        .expand_vars()
        .expect("expansion with a default succeeds");
    assert_eq!(config.url, "redis://:fallbackpw@redis:6379/0");
}

#[test]
fn a_missing_env_var_without_a_default_is_an_error() {
    // The failure mode this prevents: a literal `${REDIS_PASSWORD}` reaching
    // `fred` as the password, which fails at connect time with an auth error
    // that says nothing about the unset variable.
    let mut config: RedisClusterConfig = serde_json::from_value(json!({
        "url": "redis://:${REDIS_CLUSTER_PW_UNSET_NODEFAULT}@redis:6379/0",
    }))
    .expect("config deserializes");
    assert!(
        config.expand_vars().is_err(),
        "a referenced env var with no default and no value must surface as an error"
    );

    let mut lock: RedisLockConfig = serde_json::from_value(json!({
        "url": "redis://:${REDIS_CLUSTER_PW_UNSET_NODEFAULT}@redis:6379/0",
    }))
    .expect("lock config deserializes");
    assert!(
        lock.expand_vars().is_err(),
        "the lock config must expand its url the same way"
    );
}

#[test]
fn debug_masks_the_url() {
    // The url embeds a password after expansion, and DESIGN.md §8 requires it
    // to be masked. A `{:?}` in a log line or a panic message would otherwise
    // leak it.
    let cluster: RedisClusterConfig =
        serde_json::from_value(json!({ "url": "redis://:supersecret@redis:6379/0" }))
            .expect("config deserializes");
    let rendered = format!("{cluster:?}");
    assert!(
        !rendered.contains("supersecret"),
        "Debug must not leak the password"
    );
    assert!(
        !rendered.contains("redis://"),
        "Debug must not leak the url"
    );
    assert!(
        rendered.contains(REDACTED_URL),
        "Debug must show the redaction marker"
    );

    let lock: RedisLockConfig =
        serde_json::from_value(json!({ "url": "redis://:supersecret@redis:6379/0" }))
            .expect("lock config deserializes");
    let rendered = format!("{lock:?}");
    assert!(
        !rendered.contains("supersecret"),
        "lock Debug must not leak the password"
    );
    assert!(
        rendered.contains(REDACTED_URL),
        "lock Debug must show the redaction marker"
    );
}

#[test]
fn debug_still_renders_every_other_field() {
    // A hand-written `Debug` is a place a field can be silently dropped, and a
    // config key missing from a diagnostic dump is exactly the kind of gap
    // nobody notices until they need it.
    let config: RedisClusterConfig = serde_json::from_value(json!({
        "url": "redis://redis:6379/0",
        "watch_mode": "disabled",
        "topology": "cluster",
    }))
    .expect("config deserializes");
    let rendered = format!("{config:?}");
    for field in [
        "pool_size",
        "command_timeout_ms",
        "key_prefix",
        "database",
        "topology",
        "durability",
        "wait_replicas",
        "wait_timeout_ms",
        "watch_mode",
        "manage_keyspace_notifications",
    ] {
        assert!(
            rendered.contains(field),
            "Debug must render `{field}`, got {rendered}"
        );
    }
}

#[test]
fn validate_accepts_the_defaults() {
    let config: RedisClusterConfig =
        serde_json::from_value(minimal(json!({}))).expect("minimal config deserializes");
    config
        .validate()
        .expect("the documented defaults are valid");

    let lock: RedisLockConfig =
        serde_json::from_value(minimal(json!({}))).expect("minimal lock config deserializes");
    lock.validate().expect("the documented defaults are valid");
}

#[test]
fn validate_rejects_every_zero_that_removes_a_bound() {
    for field in ["pool_size", "command_timeout_ms", "wait_timeout_ms"] {
        let config: RedisClusterConfig = serde_json::from_value(minimal(json!({ field: 0 })))
            .expect("a zero value still deserializes; validate is what rejects it");
        let err = config
            .validate()
            .expect_err("a zero {field} must be rejected");
        let ClusterError::InvalidConfig { reason } = err else {
            panic!("a zero `{field}` must be an InvalidConfig, not a provider error");
        };
        assert!(
            reason.contains(field),
            "the error must name the offending key, got {reason}"
        );
    }
}

#[test]
fn the_lock_config_validates_its_own_three_zeros() {
    for field in ["pool_size", "command_timeout_ms", "wait_timeout_ms"] {
        let config: RedisLockConfig = serde_json::from_value(minimal(json!({ field: 0 })))
            .expect("a zero value still deserializes");
        let err = config
            .validate()
            .expect_err("a zero value must be rejected");
        assert!(
            matches!(err, ClusterError::InvalidConfig { ref reason } if reason.contains(field)),
            "the lock config must reject a zero `{field}`, got {err}"
        );
    }
}

#[test]
fn the_duration_accessors_agree_with_their_millisecond_fields() {
    let config: RedisClusterConfig = serde_json::from_value(minimal(json!({
        "command_timeout_ms": 250,
        "wait_timeout_ms": 750,
    })))
    .expect("config deserializes");
    assert_eq!(config.command_timeout(), Duration::from_millis(250));
    assert_eq!(config.wait_timeout(), Duration::from_millis(750));

    let lock: RedisLockConfig = serde_json::from_value(minimal(json!({
        "command_timeout_ms": 250,
        "wait_timeout_ms": 750,
    })))
    .expect("lock config deserializes");
    assert_eq!(lock.command_timeout(), Duration::from_millis(250));
    assert_eq!(lock.wait_timeout(), Duration::from_millis(750));
}
