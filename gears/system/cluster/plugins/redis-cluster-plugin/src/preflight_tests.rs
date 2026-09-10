use super::*;

/// The durability reading for a server that fsyncs before acknowledging — the
/// one configuration ADR-009 rates safe.
const DURABLE: DurabilityReading = DurabilityReading::Readable {
    appendonly: true,
    appendfsync: Appendfsync::Always,
};

/// The Redis default: an append-only file fsynced once a second, so a crash can
/// lose up to a second of already-acknowledged writes.
const EVERYSEC: DurabilityReading = DurabilityReading::Readable {
    appendonly: true,
    appendfsync: Appendfsync::Everysec,
};

fn decide(
    topology: TopologyFinding,
    durability: Option<Durability>,
    readability: DurabilityReading,
) -> ConsistencyDecision {
    decide_consistency(topology, durability, readability).expect("this row does not contradict")
}

// ---------------------------------------------------------------------------
// DESIGN.md §3.6, row by row.
// ---------------------------------------------------------------------------

#[test]
fn row_1_a_verified_durable_single_node_is_linearizable() {
    let decision = decide(TopologyFinding::VerifiedSingleNode, None, DURABLE);
    assert_eq!(decision.consistency, CacheConsistency::Linearizable);
    assert!(
        !decision.asserted_not_verified,
        "both halves were read off the server, so nothing is being taken on trust"
    );
}

#[test]
fn row_2_a_single_node_with_weaker_durability_is_eventually_consistent() {
    // A crash between the ack and the fsync loses accepted writes, which for
    // CAS-based leader election means two leaders.
    for reading in [
        EVERYSEC,
        DurabilityReading::Readable {
            appendonly: false,
            appendfsync: Appendfsync::Always,
        },
        DurabilityReading::Readable {
            appendonly: true,
            appendfsync: Appendfsync::No,
        },
        DurabilityReading::Unreadable,
    ] {
        let decision = decide(TopologyFinding::VerifiedSingleNode, None, reading);
        assert_eq!(
            decision.consistency,
            CacheConsistency::EventuallyConsistent,
            "{reading:?} is not fsync-always and must not reach Linearizable"
        );
    }
}

#[test]
fn row_3_a_replicated_primary_is_eventually_consistent_however_durable() {
    // Durability cannot rescue this row: replication is asynchronous, so every
    // failover may promote a node that never saw an accepted write, no matter
    // how carefully the old primary fsynced it.
    let decision = decide(
        TopologyFinding::Replicated,
        Some(Durability::FsyncAlways),
        DURABLE,
    );
    assert_eq!(decision.consistency, CacheConsistency::EventuallyConsistent);
    assert!(!decision.asserted_not_verified);
}

#[test]
fn row_4_redis_cluster_is_eventually_consistent() {
    let decision = decide(
        TopologyFinding::Cluster,
        Some(Durability::FsyncAlways),
        DURABLE,
    );
    assert_eq!(decision.consistency, CacheConsistency::EventuallyConsistent);
}

#[test]
fn row_5_an_unknown_topology_is_eventually_consistent() {
    let decision = decide(
        TopologyFinding::Unknown,
        Some(Durability::FsyncAlways),
        DURABLE,
    );
    assert_eq!(decision.consistency, CacheConsistency::EventuallyConsistent);
}

// ---------------------------------------------------------------------------
// Hints: trusted, cross-checked, and contradicted.
// ---------------------------------------------------------------------------

#[test]
fn an_uncheckable_fsync_always_hint_is_trusted_and_flagged() {
    // The managed-Redis case: `CONFIG` is refused, so the plugin cannot verify
    // the claim. It declares what the operator said and reports back that the
    // caller should log `cluster.provider.consistency_asserted` — an operator's
    // claim is then visible in the logs of the deployment that made it.
    let decision = decide(
        TopologyFinding::VerifiedSingleNode,
        Some(Durability::FsyncAlways),
        DurabilityReading::Unreadable,
    );
    assert_eq!(decision.consistency, CacheConsistency::Linearizable);
    assert!(decision.asserted_not_verified);
}

#[test]
fn a_standalone_topology_hint_is_also_only_asserted() {
    // `topology: standalone` skips `INFO replication` entirely, so the "no
    // replicas" half of row 1 is the operator's word even when `CONFIG GET`
    // corroborates the durability half.
    let decision = decide(TopologyFinding::AssertedSingleNode, None, DURABLE);
    assert_eq!(decision.consistency, CacheConsistency::Linearizable);
    assert!(decision.asserted_not_verified);
}

#[test]
fn a_contradicted_fsync_always_hint_fails_startup_naming_both_values() {
    let err = decide_consistency(
        TopologyFinding::VerifiedSingleNode,
        Some(Durability::FsyncAlways),
        EVERYSEC,
    )
    .expect_err("a hint the server contradicts must fail startup");
    let ClusterError::InvalidConfig { reason } = err else {
        panic!("a contradicted hint is a config fault, not a provider one");
    };
    assert!(
        reason.contains("fsync_always"),
        "the error must name the claim, got {reason}"
    );
    assert!(
        reason.contains("everysec"),
        "the error must name what the server actually reports, got {reason}"
    );
}

#[test]
fn the_contradiction_check_does_not_depend_on_the_topology() {
    // A claim is contradicted whether or not it would have changed the
    // declaration. Only checking it on the row that can reach `Linearizable`
    // would let the same untrue config sit unreported on a Sentinel deployment
    // until the day it was moved to a single node.
    for topology in [
        TopologyFinding::Replicated,
        TopologyFinding::Cluster,
        TopologyFinding::Unknown,
        TopologyFinding::AssertedSingleNode,
    ] {
        assert!(
            decide_consistency(topology, Some(Durability::FsyncAlways), EVERYSEC).is_err(),
            "{topology:?} must still report the contradicted hint"
        );
    }
}

#[test]
fn a_hint_weaker_than_the_server_is_accepted() {
    // Only the direction that could *upgrade* the declaration fails. Declaring
    // yourself weaker than you are can only under-declare, which is safe — and
    // is a legitimate way to keep a durable-today server from silently becoming
    // the basis of a Linearizable declaration tomorrow.
    let decision = decide(
        TopologyFinding::VerifiedSingleNode,
        Some(Durability::FsyncEverysec),
        DURABLE,
    );
    assert_eq!(decision.consistency, CacheConsistency::EventuallyConsistent);
    assert!(!decision.asserted_not_verified);
}

#[test]
fn an_eventually_consistent_declaration_is_never_flagged_as_asserted() {
    // The flag warns about an unverified upgrade; there is no such thing as an
    // unverified downgrade, and emitting the WARN on one would train operators
    // to ignore it.
    for topology in [
        TopologyFinding::Replicated,
        TopologyFinding::Cluster,
        TopologyFinding::Unknown,
        TopologyFinding::VerifiedSingleNode,
        TopologyFinding::AssertedSingleNode,
    ] {
        let decision = decide(topology, None, DurabilityReading::Unreadable);
        if decision.consistency == CacheConsistency::EventuallyConsistent {
            assert!(!decision.asserted_not_verified, "{topology:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Resolving the topology input.
// ---------------------------------------------------------------------------

#[test]
fn an_operator_topology_hint_replaces_detection() {
    for (hint, expected) in [
        (Topology::Standalone, TopologyFinding::AssertedSingleNode),
        (Topology::Sentinel, TopologyFinding::Replicated),
        (Topology::Cluster, TopologyFinding::Cluster),
    ] {
        assert_eq!(
            resolve_topology(Some(hint), TopologyFinding::VerifiedSingleNode),
            expected,
            "the {hint:?} hint must win over detection"
        );
    }
    assert_eq!(
        resolve_topology(None, TopologyFinding::Replicated),
        TopologyFinding::Replicated
    );
}

#[test]
fn only_a_primary_reporting_zero_replicas_is_a_verified_single_node() {
    assert_eq!(
        topology_from_replication(Some(ReplicationInfo {
            role: ReplicationRole::Primary,
            connected_replicas: Some(0),
        })),
        TopologyFinding::VerifiedSingleNode
    );
    assert_eq!(
        topology_from_replication(Some(ReplicationInfo {
            role: ReplicationRole::Primary,
            connected_replicas: Some(2),
        })),
        TopologyFinding::Replicated
    );
    assert_eq!(
        topology_from_replication(Some(ReplicationInfo {
            role: ReplicationRole::Replica,
            connected_replicas: Some(0),
        })),
        TopologyFinding::Replicated,
        "a replica is replicated by definition, whatever it reports downstream"
    );
    // Absent is not zero: the only row that can weaken a guarantee by being
    // wrong is the one that needs an explicit answer.
    assert_eq!(
        topology_from_replication(Some(ReplicationInfo {
            role: ReplicationRole::Primary,
            connected_replicas: None,
        })),
        TopologyFinding::Replicated
    );
    assert_eq!(
        topology_from_replication(None),
        TopologyFinding::Unknown,
        "an unreadable INFO is Unknown, so the caller can log why rather than what"
    );
}

// ---------------------------------------------------------------------------
// Reply parsers.
// ---------------------------------------------------------------------------

#[test]
fn info_replication_parses_a_lone_primary() {
    let reply = "# Replication\r\nrole:master\r\nconnected_slaves:0\r\n\
                 master_failover_state:no-failover\r\n";
    assert_eq!(
        parse_info_replication(reply),
        Some(ReplicationInfo {
            role: ReplicationRole::Primary,
            connected_replicas: Some(0),
        })
    );
}

#[test]
fn info_replication_parses_a_primary_with_replicas() {
    // The `slave0:` line is the reason the parser splits on the *first* colon
    // only: its value is full of them.
    let reply = "# Replication\r\nrole:master\r\nconnected_slaves:2\r\n\
                 slave0:ip=10.0.0.2,port=6379,state=online,offset=99,lag=0\r\n\
                 slave1:ip=10.0.0.3,port=6379,state=online,offset=99,lag=1\r\n";
    let info = parse_info_replication(reply).expect("a primary with replicas parses");
    assert_eq!(info.role, ReplicationRole::Primary);
    assert_eq!(info.connected_replicas, Some(2));
}

#[test]
fn info_replication_parses_a_primary_with_no_connected_slaves_field() {
    let reply = "# Replication\r\nrole:master\r\n";
    assert_eq!(
        parse_info_replication(reply),
        Some(ReplicationInfo {
            role: ReplicationRole::Primary,
            connected_replicas: None,
        })
    );
}

#[test]
fn info_replication_parses_a_replica() {
    let reply = "# Replication\r\nrole:slave\r\nmaster_host:10.0.0.1\r\nmaster_link_status:up\r\n";
    let info = parse_info_replication(reply).expect("a replica parses");
    assert_eq!(info.role, ReplicationRole::Replica);
}

#[test]
fn an_info_reply_with_no_usable_role_is_none() {
    for reply in [
        "",
        "# Replication\r\n",
        "# Replication\r\nrole:sentinel\r\n",
        "this is not an INFO reply at all",
    ] {
        assert!(
            parse_info_replication(reply).is_none(),
            "`{reply}` carries no recognizable role"
        );
    }
}

#[test]
fn parse_info_skips_headers_and_blank_lines() {
    let fields =
        parse_info("# Server\r\nredis_version:7.2.4\r\n\r\n# Clients\r\nconnected_clients:3\r\n");
    assert_eq!(fields.get("redis_version").copied(), Some("7.2.4"));
    assert_eq!(fields.get("connected_clients").copied(), Some("3"));
    assert_eq!(
        fields.len(),
        2,
        "`#` headers and blank lines are not fields"
    );
}

#[test]
fn config_get_parses_pairs_including_an_empty_value() {
    // The empty value is the ordinary case, not an edge one: an unconfigured
    // `notify-keyspace-events` reads back exactly like this, and collapsing it
    // to "absent" would confuse "no flags are set" with "the server would not
    // tell me" — opposite conclusions.
    let reply = vec![
        "notify-keyspace-events".to_owned(),
        String::new(),
        "maxmemory-policy".to_owned(),
        "noeviction".to_owned(),
    ];
    let parsed = parse_config_get(&reply).expect("an even-length reply parses");
    assert_eq!(
        parsed.get("notify-keyspace-events").map(String::as_str),
        Some("")
    );
    assert_eq!(
        parsed.get("maxmemory-policy").map(String::as_str),
        Some("noeviction")
    );
}

#[test]
fn config_get_parses_an_empty_reply() {
    // What a server answers for a parameter it does not have.
    assert!(
        parse_config_get(&[])
            .expect("an empty reply parses")
            .is_empty()
    );
}

#[test]
fn an_odd_length_config_get_reply_is_an_error() {
    let reply = vec!["appendfsync".to_owned()];
    let err = parse_config_get(&reply).expect_err("an unpaired element must not be dropped");
    assert!(matches!(err, ClusterError::Provider { .. }));
}

// ---------------------------------------------------------------------------
// Keyspace notification flags and the eviction policy.
// ---------------------------------------------------------------------------

#[test]
fn the_required_flag_set_is_the_minimal_one() {
    // DESIGN.md §4.3: the minimal correct set is `Kxe`, and
    // `g` and `$` add a notification for every generic and every string command
    // *server-wide* — on a shared Redis that is unrelated tenants' traffic.
    assert_eq!(REQUIRED_KEYSPACE_FLAGS, "Kxe");
    assert!(!REQUIRED_KEYSPACE_FLAGS.contains('g'));
    assert!(!REQUIRED_KEYSPACE_FLAGS.contains('$'));
}

#[test]
fn the_eviction_flag_set_asks_for_no_expiry() {
    // The standalone lock plugin's set (DESIGN.md §3.5, §3.7). `x` is absent on
    // purpose rather than by omission: a lapsed lease is found by the next
    // acquire attempt, so asking a shared server to turn on a server-wide flag
    // this deployment never reads would charge unrelated tenants for nothing.
    assert_eq!(EVICTION_KEYSPACE_FLAGS, "Ke");
    assert!(
        !EVICTION_KEYSPACE_FLAGS.contains('x'),
        "a lock-only deployment has no use for `expired`"
    );
    // And the eviction half is common to both, or the combined plugin would be
    // observing evictions the standalone one could not.
    for flag in EVICTION_KEYSPACE_FLAGS.chars() {
        assert!(
            REQUIRED_KEYSPACE_FLAGS.contains(flag),
            "`{flag}` is needed for eviction observation, which both plugins do"
        );
    }
}

#[test]
fn an_unconfigured_server_is_missing_every_flag() {
    assert_eq!(
        missing_keyspace_flags("", REQUIRED_KEYSPACE_FLAGS),
        vec!['K', 'x', 'e']
    );
    assert_eq!(merge_keyspace_flags("", REQUIRED_KEYSPACE_FLAGS), "Kxe");
    // The lock-only deployment is told about its own two and never about `x`.
    assert_eq!(
        missing_keyspace_flags("", EVICTION_KEYSPACE_FLAGS),
        vec!['K', 'e']
    );
}

#[test]
fn a_server_configured_for_evictions_only_still_owes_the_cache_its_expiry_flag() {
    // The case the split exists for: `Ke` satisfies a lock-only deployment
    // completely and leaves a cache one without `Expired`. One constant for both
    // would have to pick a side, and either choice is a wrong answer for the
    // other deployment.
    assert!(missing_keyspace_flags("Ke", EVICTION_KEYSPACE_FLAGS).is_empty());
    assert_eq!(
        missing_keyspace_flags("Ke", REQUIRED_KEYSPACE_FLAGS),
        vec!['x']
    );
}

#[test]
fn a_correctly_configured_server_needs_nothing_added() {
    assert!(missing_keyspace_flags("Kxe", REQUIRED_KEYSPACE_FLAGS).is_empty());
    assert_eq!(merge_keyspace_flags("Kxe", REQUIRED_KEYSPACE_FLAGS), "Kxe");
}

#[test]
fn the_all_classes_alias_covers_the_event_flags_but_not_the_keyspace_one() {
    // Redis defines `A` as every class except `K`, `E`, `m`, and `n`, so `KA`
    // already delivers `expired` and `evicted`. Treating `A` as an opaque
    // character would make the plugin rewrite a perfectly good config.
    assert!(missing_keyspace_flags("KA", REQUIRED_KEYSPACE_FLAGS).is_empty());
    assert_eq!(merge_keyspace_flags("KA", REQUIRED_KEYSPACE_FLAGS), "KA");
    assert!(missing_keyspace_flags("KA", EVICTION_KEYSPACE_FLAGS).is_empty());
    // `A` on its own still lacks `K`, which is what actually routes the events
    // to a `__keyspace@…__` channel.
    assert_eq!(
        missing_keyspace_flags("A", REQUIRED_KEYSPACE_FLAGS),
        vec!['K']
    );
    assert_eq!(merge_keyspace_flags("A", REQUIRED_KEYSPACE_FLAGS), "AK");
}

#[test]
fn merging_preserves_flags_the_server_already_had() {
    // `notify-keyspace-events` is server-wide, so replacing it would switch off
    // notifications an unrelated tenant is subscribed to.
    assert_eq!(
        missing_keyspace_flags("Elg", REQUIRED_KEYSPACE_FLAGS),
        vec!['K', 'x', 'e']
    );
    let merged = merge_keyspace_flags("Elg", REQUIRED_KEYSPACE_FLAGS);
    assert!(merged.starts_with("Elg"), "existing flags must survive");
    for flag in REQUIRED_KEYSPACE_FLAGS.chars() {
        assert!(
            merged.contains(flag),
            "`{flag}` must be present in {merged}"
        );
    }
}

#[test]
fn only_noeviction_is_a_safe_maxmemory_policy() {
    assert!(maxmemory_policy_is_safe("noeviction"));
    assert!(maxmemory_policy_is_safe(" noeviction\n"));
    for policy in [
        "allkeys-lru",
        "allkeys-lfu",
        "allkeys-random",
        "volatile-lru",
        "volatile-ttl",
        "",
    ] {
        assert!(
            !maxmemory_policy_is_safe(policy),
            "`{policy}` can evict a lock or leader key with no TTL having lapsed"
        );
    }
}

#[test]
fn appendfsync_parses_the_three_values_redis_reports() {
    assert_eq!(Appendfsync::parse("always"), Some(Appendfsync::Always));
    assert_eq!(Appendfsync::parse("everysec"), Some(Appendfsync::Everysec));
    assert_eq!(Appendfsync::parse("no"), Some(Appendfsync::No));
    assert_eq!(Appendfsync::parse("sometimes"), None);
}

// ---------------------------------------------------------------------------
// Sharded pub/sub detection (DESIGN.md §13 D3).
// ---------------------------------------------------------------------------

#[test]
fn sharded_pubsub_is_detected_from_the_major_version_only() {
    // The finding feeds a DEBUG line and nothing else, so a false negative costs
    // a log line while a false positive would put a wrong capability claim into
    // the record a follow-up decision is made from. Everything unparseable
    // therefore answers `false`.
    for version in ["7.0.0", "7.2.4", "8.0.1", " 7.4.1 "] {
        assert!(
            supports_sharded_pubsub(version),
            "{version} should support SPUBLISH/SSUBSCRIBE"
        );
    }
    for version in ["6.2.14", "5.0.7", "", "unknown", "v7.0.0"] {
        assert!(
            !supports_sharded_pubsub(version),
            "{version} must not be reported as supporting sharded pub/sub"
        );
    }
    assert_eq!(SHARDED_PUBSUB_MAJOR, 7);
}
