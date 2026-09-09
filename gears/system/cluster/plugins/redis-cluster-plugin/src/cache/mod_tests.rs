use std::time::Duration;

use super::*;

/// A cache with no pool behind it, for the pure key/channel construction.
///
/// `RedisCache::new` needs a `Pool`, and building one dials nothing — `fred`
/// only connects on `init()` — so this is a real cache object that simply never
/// issues a command.
fn cache(key_prefix: &str) -> RedisCache {
    cache_in_mode(key_prefix, WatchMode::Publish)
}

/// [`cache`], in an explicit [`WatchMode`] — the mode changes what
/// `event_channel` answers, which is the whole of the publish-suppression rule.
fn cache_in_mode(key_prefix: &str, watch_mode: WatchMode) -> RedisCache {
    let pool = fred::types::Builder::default_centralized()
        .build_pool(1)
        .expect("a one-connection pool builds without connecting");
    RedisCache::new(CacheInit {
        pool,
        scripts: Arc::new(ScriptCache::default()),
        key_prefix: key_prefix.to_owned(),
        consistency: CacheConsistency::EventuallyConsistent,
        watch_mode,
        clustered: false,
        wait: WaitPolicy::Disabled,
        database: 0,
        watchers: None,
        signals: crate::test_support::recording_signals().0,
    })
}

#[test]
fn disabled_watch_mode_yields_the_no_publish_sentinel() {
    // The write path is where `watch_mode: disabled` saves anything (DESIGN.md
    // §4.3). Gating only the registry and the subscriptions would leave every
    // write publishing to a channel that by construction has no subscriber —
    // watches off, and none of the cost saved that the mode exists to save.
    //
    // The empty channel is the sentinel every mutation script reads as "do not
    // publish", so this is the assertion that the write path goes quiet.
    let disabled = cache_in_mode("cluster", WatchMode::Disabled);
    assert_eq!(
        disabled.event_channel("k"),
        "",
        "under watch_mode: disabled the channel argument must be the empty no-publish sentinel"
    );
    // The entry key is unaffected: the mode is about events, not about where
    // values live, and a mode that moved keys would make it unswitchable.
    assert_eq!(disabled.entry_key("k"), "cluster:c:k");

    let publishing = cache_in_mode("cluster", WatchMode::Publish);
    assert_eq!(
        publishing.event_channel("k"),
        "cluster:e:c:k",
        "and the default mode must still name a real channel"
    );
}

#[test]
fn every_mutation_script_guards_its_publish_on_a_non_empty_channel() {
    // The other half of the sentinel: it only suppresses anything if each script
    // actually checks it. Asserted over the catalog rather than per script so a
    // sixth mutation script cannot be added with an unguarded PUBLISH.
    for script in crate::scripts::CACHE_SCRIPTS {
        // Whitespace-normalized before matching, so the assertion is about the
        // guard being there and not about how the Lua happens to be laid out —
        // a author who wraps the guard across two lines has not broken it.
        let source = script
            .source
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let publishes = source.matches("redis.call('PUBLISH'").count();
        let guards = source.matches("~= '' then redis.call('PUBLISH'").count();
        assert_eq!(
            publishes, guards,
            "`{}` has {publishes} PUBLISH call(s) but {guards} empty-channel guard(s); every one \
             must be gated so watch_mode: disabled genuinely stops publishing",
            script.name
        );
    }
}

#[test]
fn the_key_and_its_channel_are_built_from_the_same_input() {
    let cache = cache("cluster");
    assert_eq!(
        cache.entry_key("tenant-42/limit"),
        "cluster:c:tenant-42/limit"
    );
    assert_eq!(
        cache.event_channel("tenant-42/limit"),
        "cluster:e:c:tenant-42/limit"
    );
    assert_eq!(cache.entry_prefix(), "cluster:c:");
}

#[test]
fn the_operator_key_prefix_is_honoured() {
    let cache = cache("gears-prod");
    assert_eq!(cache.entry_key("k"), "gears-prod:c:k");
    assert_eq!(cache.event_channel("k"), "gears-prod:e:c:k");
}

#[test]
fn a_scope_prefixed_key_is_passed_through_untouched() {
    // The SDK's `ScopedCacheBackend` has already composed the consumer's scope
    // into the key; this plugin adds its own prefix on top and never inspects
    // the rest (DESIGN.md §2.1).
    let cache = cache("cluster");
    assert_eq!(
        cache.entry_key("gear/orders/tenant-7/counter"),
        "cluster:c:gear/orders/tenant-7/counter"
    );
}

#[test]
fn a_finite_ttl_becomes_milliseconds() {
    assert_eq!(ttl_argument(Ttl::Of(Duration::from_secs(30))), "30000");
    assert_eq!(ttl_argument(Ttl::Of(Duration::from_millis(1))), "1");
}

#[test]
fn an_indefinite_ttl_becomes_the_persist_sentinel() {
    assert_eq!(ttl_argument(Ttl::Indefinite), "-1");
}

#[test]
fn a_sub_millisecond_ttl_rounds_up_rather_than_deleting_the_key() {
    // `PEXPIRE k 0` deletes outright, so rounding down would turn "expires
    // almost immediately" into "was never stored" — and the caller's next read
    // could not tell that apart from a failed write.
    assert_eq!(ttl_argument(Ttl::Of(Duration::from_nanos(1))), "1");
    assert_eq!(ttl_argument(Ttl::Of(Duration::ZERO)), "1");
}

#[test]
fn an_absurd_ttl_saturates_rather_than_wrapping() {
    // The counterpart of `px_millis`'s test of the same name, and the same
    // reasoning: `Duration::MAX` in milliseconds overflows `u64` by three orders
    // of magnitude, and Redis rejects the saturated value because `PEXPIRE`'s
    // `now + ttl` deadline overflows — which is the honest outcome. What must
    // not happen is a wrap into a short TTL, silently expiring an entry the
    // caller asked to keep.
    assert_eq!(ttl_argument(Ttl::Of(Duration::MAX)), u64::MAX.to_string());
}

/// The `HMGET K v ver` reply shape: a two-element array, either element `nil`.
fn hmget(value: Option<&[u8]>, version: Option<&str>) -> Value {
    Value::Array(vec![
        value.map_or(Value::Null, |bytes| Value::Bytes(bytes.to_vec().into())),
        version.map_or(Value::Null, |raw| Value::String(raw.into())),
    ])
}

#[test]
fn both_fields_nil_decodes_as_absent() {
    assert_eq!(
        decode_entry("k", &hmget(None, None)).expect("decodes"),
        None
    );
}

#[test]
fn a_populated_entry_decodes_to_its_value_and_version() {
    let entry = decode_entry("k", &hmget(Some(b"\x00\x01payload"), Some("7")))
        .expect("decodes")
        .expect("present");
    assert_eq!(entry.value, b"\x00\x01payload");
    assert_eq!(entry.version, 7);
}

#[test]
fn a_version_at_the_hincrby_ceiling_decodes_losslessly() {
    // The claim DESIGN.md §2.2's whole hash encoding rests on: the version is a
    // decimal string on the wire and an integer here, never a float. Routed
    // through an `f64`, `i64::MAX` comes back as 9223372036854775808 — one too
    // many, and every subsequent CAS on the key fails.
    let ceiling = i64::MAX.to_string();
    let entry = decode_entry("k", &hmget(Some(b"v"), Some(&ceiling)))
        .expect("decodes")
        .expect("present");
    assert_eq!(entry.version, 9_223_372_036_854_775_807);
    assert_eq!(entry.version.to_string(), ceiling);
}

#[test]
fn a_version_returned_as_an_integer_decodes_too() {
    // `HINCRBY` replies with an integer and `HGET` with a string, so both shapes
    // reach `decode_version` depending on which command produced them.
    assert_eq!(decode_version(&Value::Integer(42)), Some(42));
    assert_eq!(decode_version(&Value::String("42".into())), Some(42));
    assert_eq!(
        decode_version(&Value::Bytes(b"42".to_vec().into())),
        Some(42)
    );
    assert_eq!(decode_version(&Value::Integer(-1)), None);
    assert_eq!(decode_version(&Value::String("not a number".into())), None);
}

#[test]
fn a_half_populated_hash_is_an_error_rather_than_an_absence() {
    // Every mutation writes both fields inside one script, so this means some
    // other writer owns the key. Reporting it as absent would let the next write
    // merge into a stranger's hash.
    for reply in [hmget(Some(b"v"), None), hmget(None, Some("3"))] {
        let err = decode_entry("k", &reply).expect_err("a torn entry must not read as absent");
        assert!(matches!(err, ClusterError::Provider { .. }), "got {err:?}");
    }
}

#[test]
fn a_reply_that_is_not_a_two_element_array_is_an_error() {
    for reply in [
        Value::Null,
        Value::Integer(1),
        Value::Array(vec![Value::Null]),
        Value::Array(vec![Value::Null, Value::Null, Value::Null]),
    ] {
        assert!(
            decode_entry("k", &reply).is_err(),
            "{reply:?} is not an HMGET reply this plugin can have produced"
        );
    }
}

#[test]
fn the_decode_error_names_the_key() {
    let err = decode_entry("tenant-42/limit", &Value::Integer(1)).expect_err("errors");
    assert!(
        err.to_string().contains("tenant-42/limit"),
        "an operator needs to know which key, got {err}"
    );
}

#[test]
fn a_successful_cas_reply_carries_the_new_version() {
    let reply = Value::Array(vec![Value::Integer(1), Value::Integer(8)]);
    assert_eq!(
        decode_cas_reply("k", &reply).expect("decodes"),
        CasOutcome::Swapped { version: 8 }
    );
}

#[test]
fn a_version_mismatch_carries_the_current_entry_for_the_conflict() {
    // The reason the script returns three elements rather than a bare 0: the
    // caller populates `CasConflict { current }` from it without a second round
    // trip.
    let reply = Value::Array(vec![
        Value::Integer(0),
        Value::String("5".into()),
        Value::Bytes(b"current".to_vec().into()),
    ]);
    assert_eq!(
        decode_cas_reply("k", &reply).expect("decodes"),
        CasOutcome::Conflict {
            current: Some(CacheEntry {
                value: b"current".to_vec(),
                version: 5,
            })
        }
    );
}

#[test]
fn an_absent_key_is_a_conflict_with_no_current_entry() {
    // `{0}` — a real answer (the key is gone), not a missing one.
    let reply = Value::Array(vec![Value::Integer(0)]);
    assert_eq!(
        decode_cas_reply("k", &reply).expect("decodes"),
        CasOutcome::Conflict { current: None }
    );
}

#[test]
fn a_cas_reply_shape_the_script_cannot_produce_is_an_error() {
    for reply in [
        Value::Integer(1),
        Value::Array(vec![]),
        Value::Array(vec![
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3),
        ]),
    ] {
        assert!(
            decode_cas_reply("k", &reply).is_err(),
            "{reply:?} would mean the loaded script is not the catalogued one"
        );
    }
}

#[tokio::test]
async fn watch_is_unsupported_until_the_subscriber_lands() {
    // A cache built with no watcher registry answers `Unsupported` rather than
    // handing back a channel nothing ever sends on — a silent never-firing watch
    // is indistinguishable from a quiet key.
    let cache = cache("cluster");
    assert!(matches!(
        cache.watch("k").await,
        Err(ClusterError::Unsupported { feature: "watch" })
    ));
    assert!(matches!(
        cache.watch_prefix("k").await,
        Err(ClusterError::Unsupported {
            feature: "prefix_watch"
        })
    ));
    assert!(!cache.features().prefix_watch);
}

#[test]
fn the_consistency_declaration_is_whatever_the_preflight_computed() {
    let pool = fred::types::Builder::default_centralized()
        .build_pool(1)
        .expect("a pool builds");
    let linearizable = RedisCache::new(CacheInit {
        pool,
        scripts: Arc::new(ScriptCache::default()),
        key_prefix: "cluster".to_owned(),
        consistency: CacheConsistency::Linearizable,
        watch_mode: WatchMode::Publish,
        clustered: false,
        wait: WaitPolicy::Disabled,
        database: 0,
        watchers: None,
        signals: crate::test_support::recording_signals().0,
    });
    assert_eq!(
        linearizable.consistency(),
        CacheConsistency::Linearizable,
        "the cache reports the startup decision, it does not re-derive one"
    );
    assert_eq!(
        cache("cluster").consistency(),
        CacheConsistency::EventuallyConsistent
    );
}
