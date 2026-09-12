//! Tests for the descriptor DTOs.
//!
//! Two properties: the mirrors round-trip against the serde-free SDK types they
//! mirror (`cpt-cf-clst-constraint-no-serde` is why they are separate types at
//! all), and [`ProfileDescriptor`] round-trips through serde without any
//! transport feature enabled — which makes it usable in Profile 1
//! (§12.1).

use super::{
    CacheDescriptor, LeaderElectionDescriptor, LockDescriptor, ProfileDescriptor, ProfileHealth,
    WireCacheConsistency, WireCacheFeatures, WireLeaderElectionFeatures, WireLockFeatures,
};
use crate::cache::{CacheConsistency, CacheFeatures};
use crate::leader::LeaderElectionFeatures;
use crate::lock::LockFeatures;

fn descriptor() -> ProfileDescriptor {
    ProfileDescriptor {
        name: "orders".to_owned(),
        cache: CacheDescriptor {
            consistency: WireCacheConsistency::Linearizable,
            features: WireCacheFeatures {
                prefix_watch: true,
                watch: Some(true),
            },
            provider: "postgres".to_owned(),
        },
        lock: LockDescriptor {
            features: WireLockFeatures { linearizable: true },
            provider: "postgres".to_owned(),
        },
        leader_election: LeaderElectionDescriptor {
            features: WireLeaderElectionFeatures {
                linearizable: false,
            },
            provider: "standalone".to_owned(),
        },
        health: ProfileHealth::Degraded,
    }
}

#[test]
fn cache_consistency_round_trips_through_its_mirror() {
    for original in [
        CacheConsistency::Linearizable,
        CacheConsistency::EventuallyConsistent,
    ] {
        let dto = WireCacheConsistency::from(original);
        assert_eq!(CacheConsistency::from(dto), original);
    }
}

#[test]
fn feature_flags_round_trip_through_their_mirrors() {
    for original in [
        CacheFeatures::new(true),
        CacheFeatures::new(false),
        CacheFeatures::without_watch(),
    ] {
        assert_eq!(
            CacheFeatures::from(WireCacheFeatures::from(original)),
            original
        );
    }
    for linearizable in [true, false] {
        let lock = LockFeatures::new(linearizable);
        assert_eq!(LockFeatures::from(WireLockFeatures::from(lock)), lock);

        let leader = LeaderElectionFeatures::new(linearizable);
        assert_eq!(
            LeaderElectionFeatures::from(WireLeaderElectionFeatures::from(leader)),
            leader
        );
    }
}

#[test]
fn absent_wire_watch_decodes_as_supported() {
    // D2: an old peer predates the `watch` field, so `None` on the wire must
    // decode to `watch: true` — every backend served exact watch before the
    // field existed. An explicit `Some(false)` is the only way to say "no watch".
    let absent = WireCacheFeatures {
        prefix_watch: false,
        watch: None,
    };
    assert!(
        CacheFeatures::from(absent).watch,
        "an absent wire watch must decode as supported"
    );

    let disabled = WireCacheFeatures {
        prefix_watch: false,
        watch: Some(false),
    };
    assert!(
        !CacheFeatures::from(disabled).watch,
        "an explicit Some(false) must decode as unsupported"
    );

    // A local descriptor always sends the bit explicitly, never absent.
    assert_eq!(
        WireCacheFeatures::from(CacheFeatures::without_watch()).watch,
        Some(false)
    );
}

#[test]
fn profile_descriptor_round_trips_through_serde() {
    let original = descriptor();
    let json = serde_json::to_string(&original).expect("the descriptor serialises");
    let decoded: ProfileDescriptor =
        serde_json::from_str(&json).expect("the descriptor deserialises");
    assert_eq!(decoded, original);
}

#[test]
fn health_and_consistency_serialise_as_snake_case() {
    let json = serde_json::to_string(&descriptor()).expect("the descriptor serialises");
    assert!(json.contains("\"health\":\"degraded\""), "got {json}");
    assert!(
        json.contains("\"consistency\":\"linearizable\""),
        "got {json}"
    );
}

/// The descriptor is a published schema (`DescribeProfiles`'s payload), so the
/// schemars derive must actually produce one — a compile-time-shaped property
/// that only a call exercises.
#[test]
fn profile_descriptor_has_a_json_schema() {
    let schema = schemars::schema_for!(ProfileDescriptor);
    let rendered = serde_json::to_string(&schema).expect("the schema serialises");
    assert!(rendered.contains("leader_election"), "got {rendered}");
}

// Lease and cache wire types

#[test]
fn lease_ref_round_trips_through_serde_with_its_token_intact() {
    use super::{LeaseRef, LeaseToken};

    let original = LeaseRef {
        profile: "orders".to_owned(),
        token: LeaseToken {
            name: "ledger".to_owned(),
            owner: "client-7".to_owned(),
            fence: 42,
        },
        ttl_ms: Some(30_000),
        client_request_id: None,
    };

    let json = serde_json::to_string(&original).expect("serialises");
    let decoded: LeaseRef = serde_json::from_str(&json).expect("deserialises");
    assert_eq!(decoded, original);
}

/// The one deliberate asymmetry in the DTO layer, asserted so it stays deliberate:
/// [`CacheEntry`] carries no expiry, so `WireCacheEntry` -> `CacheEntry` -> `WireCacheEntry` drops
/// `expires_at_ms` while the reverse round trip is lossless.
#[test]
fn cache_entry_round_trip_is_lossless_one_way_and_drops_the_expiry_the_other() {
    use super::WireCacheEntry;
    use crate::cache::CacheEntry;

    let entry = CacheEntry {
        value: b"payload".to_vec(),
        version: 3,
    };
    assert_eq!(CacheEntry::from(WireCacheEntry::from(entry.clone())), entry);

    let dto = WireCacheEntry {
        value: b"payload".to_vec(),
        version: 3,
        expires_at_ms: Some(1_800_000_000_000),
    };
    let via_entry = WireCacheEntry::from(CacheEntry::from(dto.clone()));
    assert_eq!(via_entry.value, dto.value);
    assert_eq!(via_entry.version, dto.version);
    assert_eq!(
        via_entry.expires_at_ms, None,
        "`CacheEntry` has nowhere to keep the expiry, so it must be dropped rather than invented"
    );
}

#[test]
fn leader_status_round_trips_through_its_mirror() {
    use super::WireLeaderStatus;
    use crate::leader::LeaderStatus;

    for original in [
        LeaderStatus::Leader,
        LeaderStatus::Follower,
        LeaderStatus::Lost,
    ] {
        assert_eq!(
            LeaderStatus::from(WireLeaderStatus::from(original)),
            original
        );
    }
}

/// The three [`CacheEvent`](crate::cache::CacheEvent) kinds are lifted to top-level
/// variants of the wire union rather than nested inside an `Event(..)` wrapper
/// (§6.8), so the mapping is worth pinning.
#[test]
fn cache_events_flatten_onto_the_wire_union() {
    use super::WireCacheWatchEvent;
    use crate::cache::CacheEvent;

    let cases = [
        (
            CacheEvent::Changed {
                key: "k".to_owned(),
            },
            "changed",
        ),
        (
            CacheEvent::Deleted {
                key: "k".to_owned(),
            },
            "deleted",
        ),
        (
            CacheEvent::Expired {
                key: "k".to_owned(),
            },
            "expired",
        ),
    ];

    for (event, tag) in cases {
        let dto = WireCacheWatchEvent::from(event);
        let json = serde_json::to_string(&dto).expect("serialises");
        assert!(json.contains(tag), "got {json}");
        let decoded: WireCacheWatchEvent = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(decoded, dto);
    }
}

/// Every wire type needs a schemars schema, because protogen reads the schemas
/// alongside the IR. This exercises the ones with the least obvious derives —
/// byte vectors, a nested token, an enum with payload variants, and the empty
/// responses.
#[test]
fn every_awkward_wire_type_produces_a_schema() {
    use super::{
        CasRequest, DescribeProfilesResponse, LeaderJoined, LeaseRef, LockAcquired, PutRequest,
        PutResponse, ReleaseResponse, RenewResponse, ResignResponse, ScanResponse,
        WireCacheWatchEvent, WireLeaderWatchEvent,
    };

    macro_rules! assert_has_schema {
        ($($ty:ty),+ $(,)?) => {
            $(
                let schema = schemars::schema_for!($ty);
                serde_json::to_string(&schema)
                    .unwrap_or_else(|e| panic!("{} has no serialisable schema: {e}", stringify!($ty)));
            )+
        };
    }

    assert_has_schema!(
        PutRequest,
        PutResponse,
        CasRequest,
        ScanResponse,
        LeaseRef,
        LockAcquired,
        RenewResponse,
        ReleaseResponse,
        ResignResponse,
        LeaderJoined,
        WireCacheWatchEvent,
        WireLeaderWatchEvent,
        DescribeProfilesResponse,
    );
}

/// The acknowledgement responses carry the registry generation rather than being
/// empty, because `toolkit-contract-protogen` rejects a message with no fields.
/// `tests/protogen_shape.rs` is what proves the projection; this pins the field so
/// it cannot be "tidied away" back into an empty struct.
#[test]
fn acknowledgement_responses_carry_the_registry_generation() {
    use super::{PutResponse, ReleaseResponse, RenewResponse, ResignResponse};

    macro_rules! assert_carries_generation {
        ($($ty:ty),+ $(,)?) => {
            $(
                let rendered = serde_json::to_string(&schemars::schema_for!($ty))
                    .expect("the schema serialises");
                assert!(
                    rendered.contains("generation"),
                    "{} must carry a field — protogen rejects an empty message — got {rendered}",
                    stringify!($ty)
                );
            )+
        };
    }

    assert_carries_generation!(PutResponse, RenewResponse, ReleaseResponse, ResignResponse);
}

/// Byte fields must publish `format: "byte"`, which makes protogen emit
/// proto3 `bytes`. Without it schemars renders `Vec<u8>` as an integer array and
/// protogen faithfully projects `repeated int64` — every byte of every cache value
/// as a separately tagged varint.
#[test]
fn byte_fields_publish_the_bytes_format() {
    use super::{CadRequest, CasRequest, PutRequest, WireCacheEntry};

    macro_rules! assert_bytes_format {
        ($($ty:ty),+ $(,)?) => {
            $(
                let rendered = serde_json::to_string(&schemars::schema_for!($ty))
                    .expect("the schema serialises");
                assert!(
                    rendered.contains(r#""format":"byte""#),
                    "{} has a byte field that would project to `repeated int64`, got {rendered}",
                    stringify!($ty)
                );
            )+
        };
    }

    assert_bytes_format!(WireCacheEntry, PutRequest, CasRequest, CadRequest);
}

/// The watch unions are flat discriminated messages, and the discriminator is a
/// **unit-only** enum — that is what lets it project to a proto3 enum.
#[test]
fn watch_event_discriminators_are_unit_only_enums() {
    use super::{CacheWatchEventKind, LeaderWatchEventKind};

    for rendered in [
        serde_json::to_string(&schemars::schema_for!(CacheWatchEventKind)).expect("schema"),
        serde_json::to_string(&schemars::schema_for!(LeaderWatchEventKind)).expect("schema"),
    ] {
        assert!(
            rendered.contains(r#""type":"string""#),
            "a payload-carrying discriminator would not project to a proto enum, got {rendered}"
        );
    }
}

/// The constructors keep the "kind decides which fields are populated" invariant,
/// which the flat wire shape cannot express in the type system.
#[test]
fn watch_event_constructors_populate_only_their_own_payload() {
    use super::{
        CacheWatchEventKind, LeaderWatchEventKind, WireCacheWatchEvent, WireError,
        WireLeaderStatus, WireLeaderWatchEvent,
    };

    let changed = WireCacheWatchEvent::key_event(CacheWatchEventKind::Changed, "k".to_owned());
    assert_eq!(changed.key.as_deref(), Some("k"));
    assert!(changed.dropped.is_none() && changed.error.is_none());

    let lagged = WireCacheWatchEvent::lagged(7);
    assert_eq!(lagged.dropped, Some(7));
    assert!(lagged.key.is_none() && lagged.error.is_none());

    let reset = WireCacheWatchEvent::reset();
    assert_eq!(reset.kind, CacheWatchEventKind::Reset);
    assert!(reset.key.is_none() && reset.dropped.is_none() && reset.error.is_none());

    let closed = WireCacheWatchEvent::closed(WireError {
        error_domain: "cluster.v1".to_owned(),
        error_code: "shutdown".to_owned(),
        detail: "draining".to_owned(),
        data: "null".to_owned(),
    });
    assert_eq!(closed.kind, CacheWatchEventKind::Closed);
    assert!(closed.error.is_some());
    assert!(closed.key.is_none() && closed.dropped.is_none());

    let status = WireLeaderWatchEvent::status(WireLeaderStatus::Leader);
    assert_eq!(status.kind, LeaderWatchEventKind::Status);
    assert_eq!(status.status, Some(WireLeaderStatus::Leader));
    assert!(status.dropped.is_none() && status.error.is_none());
}
