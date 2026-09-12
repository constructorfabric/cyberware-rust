//! Tests for the client-side descriptor cache.

use super::DescriptorCache;
use crate::dto::{
    CacheDescriptor, LeaderElectionDescriptor, LockDescriptor, ProfileDescriptor, ProfileHealth,
    WireCacheConsistency, WireCacheFeatures, WireLeaderElectionFeatures, WireLockFeatures,
};

/// A descriptor for `name`, distinguishable by its cache provider.
fn descriptor(name: &str, provider: &str) -> ProfileDescriptor {
    ProfileDescriptor {
        name: name.to_owned(),
        cache: CacheDescriptor {
            consistency: WireCacheConsistency::Linearizable,
            features: WireCacheFeatures {
                prefix_watch: true,
                watch: Some(true),
            },
            provider: provider.to_owned(),
        },
        lock: LockDescriptor {
            features: WireLockFeatures { linearizable: true },
            provider: provider.to_owned(),
        },
        leader_election: LeaderElectionDescriptor {
            features: WireLeaderElectionFeatures { linearizable: true },
            provider: provider.to_owned(),
        },
        health: ProfileHealth::Serving,
    }
}

#[test]
fn an_empty_cache_answers_nothing() {
    let cache = DescriptorCache::new();
    assert!(cache.get("orders").is_none());
}

#[test]
fn populate_indexes_by_profile_name() {
    let cache = DescriptorCache::new();
    cache.populate(
        1,
        vec![
            descriptor("orders", "postgres"),
            descriptor("audit", "standalone"),
        ],
    );

    assert_eq!(
        cache.get("orders").expect("populated").cache.provider,
        "postgres"
    );
    assert_eq!(
        cache.get("audit").expect("populated").cache.provider,
        "standalone"
    );
    assert!(cache.get("nowhere").is_none());
}

#[test]
fn populate_replaces_the_set_rather_than_merging_it() {
    // `DescribeProfiles` answers with the server's whole bound set, so a profile
    // absent from a later response is one the server no longer binds. Merging
    // would leave it answering `consistency()` forever (DESIGN section 5.6
    // phase C).
    let cache = DescriptorCache::new();
    cache.populate(1, vec![descriptor("orders", "postgres")]);
    cache.populate(2, vec![descriptor("audit", "standalone")]);

    assert!(
        cache.get("orders").is_none(),
        "a profile the server stopped binding must stop answering"
    );
    assert!(cache.get("audit").is_some());
}

#[test]
fn a_response_from_an_older_generation_is_still_applied() {
    // `generation` counts publishes within one server *process* and restarts from
    // 0 in a fresh pod, so a lower generation does not mean "stale" -- across a
    // rolling restart it is what every healthy pod answers with, because the
    // draining pod published the empty set at generation+1 on its way out (DESIGN
    // section 4.8). Dropping it wedged the client permanently.
    let cache = DescriptorCache::new();
    cache.populate(7, vec![descriptor("audit", "standalone")]);
    cache.populate(3, vec![descriptor("orders", "postgres")]);

    assert!(
        cache.get("orders").is_some(),
        "the answering pod's set must be adopted whatever generation it carries"
    );
    assert!(
        cache.get("audit").is_none(),
        "and it replaces the previous set wholesale rather than merging"
    );
}

#[test]
fn a_regressed_generation_is_recorded_rather_than_remembered() {
    // The consequence that matters: having once seen a high generation must not
    // leave the cache refusing lower ones forever. Two successive reads from a
    // replacement pod both land.
    let cache = DescriptorCache::new();
    cache.populate(9, Vec::new());
    cache.populate(1, vec![descriptor("orders", "postgres")]);
    cache.populate(1, vec![descriptor("orders", "postgres")]);

    assert!(cache.get("orders").is_some());
}

#[test]
fn an_equal_generation_refreshes_rather_than_being_dropped() {
    // Health moves without the server republishing its profile set, so the
    // refresh poll normally arrives at an unchanged generation carrying fresher
    // health (DESIGN section 4.4). Dropping it would freeze health forever.
    let cache = DescriptorCache::new();
    cache.populate(4, vec![descriptor("orders", "postgres")]);

    let mut degraded = descriptor("orders", "postgres");
    degraded.health = ProfileHealth::Degraded;
    cache.populate(4, vec![degraded]);

    assert_eq!(
        cache.get("orders").expect("populated").health,
        ProfileHealth::Degraded
    );
}
