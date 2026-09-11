//! Tenant-traversal scope on an interest.
//!
//! The wire gives `max_depth` three meanings across two JSON forms - absent,
//! `0`, `n`, and `null` - and the whole reason the domain models it as an enum
//! rather than an `Option<u32>` is that two of those forms are opposites:
//! absent means the narrowest scope and `null` means the widest. These tests
//! pin that mapping in both directions, because getting it backwards would
//! silently widen a subscription's tenant scope rather than fail.

use std::num::NonZeroU32;

use serde_json::json;
use uuid::Uuid;

use super::model::{BarrierMode, Interest, TenantTraversalDepth};

fn interest_json(overrides: &serde_json::Value) -> serde_json::Value {
    let mut base = json!({
        "topic": "gts.cf.core.events.topic.v1~x.eb.orders.acme.v1",
        "tenant_id": "fec5ff3e-4d0d-416e-95e0-c354028c1b12",
        "types": ["gts.cf.core.events.event.v1~x.eb.orders.created.v1~"],
    });
    if let (Some(base), Some(overrides)) = (base.as_object_mut(), overrides.as_object()) {
        for (key, value) in overrides {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

fn depth_of(overrides: &serde_json::Value) -> TenantTraversalDepth {
    serde_json::from_value::<Interest>(interest_json(overrides))
        .expect("interest deserializes")
        .depth
}

#[test]
fn an_absent_max_depth_is_the_current_tenant_only() {
    assert_eq!(depth_of(&json!({})), TenantTraversalDepth::CurrentTenant);
}

#[test]
fn a_zero_max_depth_is_the_current_tenant_only() {
    assert_eq!(
        depth_of(&json!({ "max_depth": 0 })),
        TenantTraversalDepth::CurrentTenant
    );
}

#[test]
fn a_positive_max_depth_is_that_many_levels_of_descendants() {
    assert_eq!(
        depth_of(&json!({ "max_depth": 3 })),
        TenantTraversalDepth::Descendants(NonZeroU32::new(3).expect("3 is non-zero"))
    );
}

/// The case that makes the enum worth having: an explicit `null` is the
/// *widest* scope, and an `Option<u32>` would make it indistinguishable from
/// the absent case above, which is the narrowest.
#[test]
fn an_explicit_null_max_depth_is_unlimited_descendants() {
    assert_eq!(
        depth_of(&json!({ "max_depth": null })),
        TenantTraversalDepth::UnlimitedDescendants
    );
}

#[test]
fn every_depth_round_trips_through_the_wire_form() {
    for depth in [
        TenantTraversalDepth::CurrentTenant,
        TenantTraversalDepth::Descendants(NonZeroU32::new(1).expect("1 is non-zero")),
        TenantTraversalDepth::Descendants(NonZeroU32::new(9).expect("9 is non-zero")),
        TenantTraversalDepth::UnlimitedDescendants,
    ] {
        assert_eq!(
            TenantTraversalDepth::from_max_depth(depth.max_depth()),
            depth,
            "{depth:?} did not survive a round trip through max_depth"
        );
    }
}

/// A stored `Subscription` round-trips through this shape in the cluster
/// cache, so the serialized form is asserted whole rather than field by field.
#[test]
fn an_interest_serializes_to_the_full_wire_shape() {
    let interest = Interest {
        topic: toolkit_gts::GtsInstanceId::try_new(
            "gts.cf.core.events.topic.v1~x.eb.orders.acme.v1",
        )
        .expect("static topic id is valid"),
        tenant_id: Uuid::parse_str("fec5ff3e-4d0d-416e-95e0-c354028c1b12").expect("static uuid"),
        depth: TenantTraversalDepth::UnlimitedDescendants,
        barrier_mode: BarrierMode::Ignore,
        types: vec!["gts.cf.core.events.event.v1~x.eb.orders.created.v1~".to_owned()],
        filter: None,
    };

    assert_eq!(
        serde_json::to_value(&interest).expect("interest serializes"),
        json!({
            "topic": "gts.cf.core.events.topic.v1~x.eb.orders.acme.v1",
            "tenant_id": "fec5ff3e-4d0d-416e-95e0-c354028c1b12",
            "max_depth": null,
            "barrier_mode": "ignore",
            "types": ["gts.cf.core.events.event.v1~x.eb.orders.created.v1~"],
            "filter": null,
        })
    );
}

#[test]
fn barrier_mode_defaults_to_respect_when_absent() {
    let interest: Interest =
        serde_json::from_value(interest_json(&json!({}))).expect("interest deserializes");

    assert_eq!(interest.barrier_mode, BarrierMode::Respect);
    assert_eq!(BarrierMode::default(), BarrierMode::Respect);
}

#[test]
fn barrier_mode_wire_tokens_agree_with_its_serde_renaming() {
    for mode in [BarrierMode::Respect, BarrierMode::Ignore] {
        assert_eq!(
            serde_json::to_value(mode).expect("mode serializes"),
            json!(mode.as_wire()),
            "as_wire disagrees with serde for {mode:?}"
        );
        assert_eq!(BarrierMode::from_wire(mode.as_wire()), Some(mode));
    }
}

#[test]
fn an_unrecognised_barrier_mode_is_rejected_rather_than_defaulted() {
    assert_eq!(BarrierMode::from_wire("traverse"), None);
    assert_eq!(BarrierMode::from_wire("Respect"), None);
    assert_eq!(BarrierMode::from_wire(""), None);
}
