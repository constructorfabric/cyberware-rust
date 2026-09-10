//! The two facts the outbox writer decides before it touches a database: what
//! makes a repeat of one publish the same event, and what a consumer receives.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::json;
use uuid::Uuid;

use super::{
    NewOutboxEvent, PlanPublishedPayload, PriceWindowTransitionPayload, outbox_id,
    plan_published_dedup_key, price_window_transition_dedup_key,
};
use crate::domain::evaluation_policy::EVALUATION_POLICY_GENERATION;
use crate::domain::events::CatalogEvent;
use crate::domain::instant::{format_rfc3339, utc_ymd_hms};
use crate::domain::scope_key::PlanId;
use time::OffsetDateTime;

const PLAN: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00a1);
const TENANT: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00b1);
const CORRELATION: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00c1);
const WINDOW: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00d1);
const PRICE: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00e1);

fn payload() -> PlanPublishedPayload {
    PlanPublishedPayload {
        plan_id: PlanId::new(PLAN),
        revision: 2,
        pending_version_ref: "pend-7".to_owned(),
        price_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
        correlation_id: CORRELATION,
    }
}

/// The window interval this file renders. **2099**, so no assertion here can start
/// answering differently on a date.
fn from() -> OffsetDateTime {
    utc_ymd_hms(2099, 9, 10, 0, 0, 0)
}

fn to() -> OffsetDateTime {
    utc_ymd_hms(2099, 9, 20, 0, 0, 0)
}

fn window_payload() -> PriceWindowTransitionPayload {
    PriceWindowTransitionPayload {
        window_id: WINDOW,
        plan_id: PlanId::new(PLAN),
        price_id: PRICE,
        effective_from: from(),
        effective_to: Some(to()),
        correlation_id: CORRELATION,
    }
}

#[test]
fn one_revision_publishes_under_one_dedup_key() {
    // A repeat of the same publish is the same key, which is what makes
    // `uq_pricing_outbox_dedup_key` refuse it at the writer.
    assert_eq!(
        plan_published_dedup_key(PlanId::new(PLAN), 2),
        plan_published_dedup_key(PlanId::new(PLAN), 2)
    );
    assert_eq!(
        plan_published_dedup_key(PlanId::new(PLAN), 2),
        format!("PlanPublished/{PLAN}/2")
    );
}

#[test]
fn two_different_publishes_are_two_different_keys() {
    let plan = PlanId::new(PLAN);
    let other = PlanId::new(Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00a2));

    assert_ne!(
        plan_published_dedup_key(plan, 2),
        plan_published_dedup_key(plan, 3),
        "a second revision of one plan is a second publish"
    );
    assert_ne!(
        plan_published_dedup_key(plan, 2),
        plan_published_dedup_key(other, 2),
        "the same revision number of two plans is two publishes"
    );
}

#[test]
fn the_event_name_in_the_key_comes_from_the_frozen_set() {
    assert!(
        plan_published_dedup_key(PlanId::new(PLAN), 0)
            .starts_with(CatalogEvent::PlanPublished.as_str())
    );
}

#[test]
fn the_surrogate_key_agrees_with_the_dedup_index() {
    // Derived rather than random, so a repeated publish collides on the primary
    // key as well as on the unique index.
    let key = plan_published_dedup_key(PlanId::new(PLAN), 2);

    assert_eq!(outbox_id(TENANT, &key), outbox_id(TENANT, &key));
    assert_ne!(
        outbox_id(TENANT, &key),
        outbox_id(TENANT, &plan_published_dedup_key(PlanId::new(PLAN), 3))
    );
    assert_ne!(
        outbox_id(TENANT, &key),
        outbox_id(
            Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00b2),
            &key
        ),
        "the dedup index is per tenant, and so is the surrogate"
    );
}

#[test]
fn the_payload_carries_the_pending_handle_and_the_published_rows() {
    assert_eq!(
        payload().to_value(),
        json!({
            "planId": PLAN,
            "revision": 2,
            "pendingVersionRef": "pend-7",
            "priceIds": [Uuid::from_u128(1), Uuid::from_u128(2)],
            "evaluationPolicyVersion": EVALUATION_POLICY_GENERATION,
            "correlationId": CORRELATION,
        })
    );
}

#[test]
fn the_payload_stamps_all_three_snapshot_segments() {
    // This test used to pin the *absence* of the third segment, because
    // `PricingSnapshotRef` has three parts, the commit produced two, and no
    // document said what produced the third. D-162 says: the evaluation-policy
    // generation, a declared constant of the gear. So the pin flips, and the
    // thing it still forbids is the one D-161 forbade -- a value that is not a
    // real generation.
    let rendered = payload().to_value();
    let object = rendered.as_object().expect("a payload is a JSON object");

    assert_eq!(
        object.get("pendingVersionRef"),
        Some(&json!("pend-7")),
        "segment 1: the registry's pending handle"
    );
    assert_eq!(
        object.get("priceIds"),
        Some(&json!([Uuid::from_u128(1), Uuid::from_u128(2)])),
        "segment 2: the resolved price ids"
    );
    assert_eq!(
        object.get("evaluationPolicyVersion"),
        Some(&json!(EVALUATION_POLICY_GENERATION)),
        "segment 3: the evaluation-policy generation"
    );

    // No `pricingSnapshotRef` envelope: the three segments are stamped flat
    // because no document declares a nesting for this event, and a wire
    // structure invented at a keyboard is what D-158 forbids one line over.
    assert!(!object.contains_key("pricingSnapshotRef"));
}

/// Both window events' bodies, whole, against a literal.
///
/// **Whole-value equality**, which is what asserts the key set **closed** — the
/// idiom `the_payload_carries_the_pending_handle_and_the_published_rows` uses one
/// event over. Before this, two of the payload's keys were asserted, on one of the
/// two events, by `tests/sqlite_window_activation.rs`; the expiry's body was
/// asserted nowhere at all, and rendering every `PriceWindowExpired` with
/// `"state": "active"` left 1150 tests green.
///
/// The two bodies are now **equal**, and that is the fix to F10 rather than an
/// omission: the state token was a second spelling of the event's own name, so it
/// is gone and the name is the discriminator. Which is why this test asserts the
/// name and the dedup key beside the body — those are what differ, and asserting
/// only the body would leave two events that are two events by nothing this file
/// checks.
#[test]
fn both_window_transition_events_carry_the_same_whole_body_under_different_names() {
    let at = utc_ymd_hms(2026, 8, 4, 9, 0, 0);
    let payload = window_payload();
    let expected = json!({
        "windowId": WINDOW,
        "planId": PLAN,
        "priceId": PRICE,
        "effectiveFrom": format_rfc3339(from()),
        "effectiveTo": format_rfc3339(to()),
        "correlationId": CORRELATION,
    });

    let activated = NewOutboxEvent::price_window_activated(TENANT, &payload, at);
    assert_eq!(activated.payload, expected);
    assert_eq!(activated.event, CatalogEvent::PriceWindowActivated);
    assert_eq!(
        activated.dedup_key,
        price_window_transition_dedup_key(CatalogEvent::PriceWindowActivated, WINDOW)
    );
    assert_eq!(activated.aggregate_id, PLAN, "ordering is per plan");

    let expired = NewOutboxEvent::price_window_expired(TENANT, &payload, at);
    assert_eq!(expired.payload, expected);
    assert_eq!(expired.event, CatalogEvent::PriceWindowExpired);
    assert_eq!(
        expired.dedup_key,
        price_window_transition_dedup_key(CatalogEvent::PriceWindowExpired, WINDOW)
    );
    assert_eq!(expired.aggregate_id, PLAN);

    assert_ne!(
        activated.dedup_key, expired.dedup_key,
        "a window's expiry must not dedup against its own activation"
    );
}

/// An open-ended window renders `effectiveTo` as **`null`**, not as an absent key.
///
/// The absence is a value (`inst-ws-expire`: an open-ended window never expires),
/// and the closed key set above is what makes the distinction assertable: a
/// renderer that dropped the key on `None` would produce a body six keys wide for
/// one window and five for another, and a consumer reading "no `effectiveTo` key"
/// cannot tell open-ended from a field the producer forgot.
#[test]
fn an_open_ended_windows_payload_says_null_rather_than_omitting_the_key() {
    let at = utc_ymd_hms(2026, 8, 4, 9, 0, 0);
    let payload = PriceWindowTransitionPayload {
        effective_to: None,
        ..window_payload()
    };

    let event = NewOutboxEvent::price_window_activated(TENANT, &payload, at);

    assert_eq!(
        event.payload,
        json!({
            "windowId": WINDOW,
            "planId": PLAN,
            "priceId": PRICE,
            "effectiveFrom": format_rfc3339(from()),
            "effectiveTo": null,
            "correlationId": CORRELATION,
        })
    );
}

#[test]
fn the_constructor_fixes_the_name_the_aggregate_and_the_key() {
    let at = utc_ymd_hms(2026, 8, 3, 9, 0, 0);
    let event = NewOutboxEvent::plan_published(TENANT, &payload(), at);

    assert_eq!(event.event, CatalogEvent::PlanPublished);
    assert_eq!(event.aggregate_id, PLAN, "ordering is per plan");
    assert_eq!(
        event.dedup_key,
        plan_published_dedup_key(PlanId::new(PLAN), 2)
    );
    assert_eq!(event.correlation_id, CORRELATION);
    assert_eq!(event.enqueued_at, at);
}

// ---------------------------------------------------------------------------
// `PlanRetired` (`inst-rt-event`, D-128).
// ---------------------------------------------------------------------------

fn retired_payload() -> super::PlanRetiredPayload {
    super::PlanRetiredPayload {
        plan_id: PlanId::new(PLAN),
        revision: 2,
        pending_version_ref: "pend-9".to_owned(),
        cancelled_window_ids: vec![WINDOW],
        correlation_id: CORRELATION,
    }
}

#[test]
fn the_retirement_constructor_fixes_the_name_the_aggregate_and_the_key() {
    let at = utc_ymd_hms(2026, 8, 7, 9, 0, 0);
    let event = NewOutboxEvent::plan_retired(TENANT, &retired_payload(), at);

    assert_eq!(event.event, CatalogEvent::PlanRetired);
    // The **plan**, not a stream of its own: a consumer that saw `PlanRetired`
    // out of order against the `PlanPublished` it follows would evict a plan it
    // had never warmed.
    assert_eq!(event.aggregate_id, PLAN, "ordering is per plan");
    assert_eq!(
        event.dedup_key,
        super::plan_retired_dedup_key(PlanId::new(PLAN), 2)
    );
    assert_eq!(event.correlation_id, CORRELATION);
    assert_eq!(event.enqueued_at, at);
}

#[test]
fn a_retirement_and_a_publish_of_one_revision_are_different_events() {
    // Both are keyed `(plan, revision)` and a shared prefix would make the
    // retirement of revision 2 dedup against the publish of revision 2 - one
    // aggregate, one dedup index, and the second write silently dropped. The
    // event name is what keeps them apart.
    let plan = PlanId::new(PLAN);
    assert_ne!(
        super::plan_retired_dedup_key(plan, 2),
        plan_published_dedup_key(plan, 2)
    );
    assert!(super::plan_retired_dedup_key(plan, 2).starts_with("PlanRetired/"));
}

#[test]
fn the_retirement_payload_renders_the_wire_keys() {
    assert_eq!(
        retired_payload().to_value(),
        serde_json::json!({
            "planId": PLAN,
            "revision": 2,
            "pendingVersionRef": "pend-9",
            "cancelledWindowIds": [WINDOW],
            "correlationId": CORRELATION,
        })
    );
}

#[test]
fn an_empty_cancellation_list_renders_as_an_empty_array_not_as_a_missing_key() {
    // D-182's fail-closed posture makes the empty list this system's only case,
    // so it is the one a consumer will actually parse. A missing key and an
    // empty array are different things to a consumer that iterates.
    let payload = super::PlanRetiredPayload {
        cancelled_window_ids: Vec::new(),
        ..retired_payload()
    };
    assert_eq!(
        payload.to_value().get("cancelledWindowIds"),
        Some(&serde_json::json!([]))
    );
}

// ---------------------------------------------------------------------------
// What a losing writer is told to retry against (D-159).
// ---------------------------------------------------------------------------

const OVERLAY: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00f1);

fn overlay_payload() -> super::PriceOverlayPublishedPayload {
    super::PriceOverlayPublishedPayload {
        price_overlay_id: OVERLAY,
        revision: 3,
        scope: crate::domain::overlay::ScopeSelector::Global,
        precedence: 10,
        pending_version_ref: "pend-9".to_owned(),
        correlation_id: CORRELATION,
    }
}

/// The 409's aggregate names the aggregate the event is **ordered within**, and
/// for an overlay publish that is the overlay.
///
/// `contention_or_db` puts this string into `RepoError::ConcurrentMutation
/// { aggregate }`, which `repo_failure` renders straight into the
/// `CONCURRENT_MUTATION` body — so a hard-coded noun is not a phrasing choice. It
/// said `plan <overlay id>` for every one of these, sending a losing overlay
/// publish to look up a plan that does not exist.
#[test]
fn a_losing_overlay_publish_is_told_to_retry_against_the_overlay() {
    let event = NewOutboxEvent::price_overlay_published(TENANT, &overlay_payload(), from());

    assert_eq!(
        super::contention_aggregate(&event),
        format!("price overlay {OVERLAY}"),
        "the aggregate is the overlay, not a plan -- its constructor says so"
    );
}

/// And the thirteen plan-ordered events keep saying `plan`.
///
/// Both halves matter: the fix is that the noun follows the aggregate, not that
/// one noun replaced another.
#[test]
fn a_plan_ordered_event_is_still_told_to_retry_against_the_plan() {
    let event = NewOutboxEvent::plan_published(TENANT, &payload(), from());

    assert_eq!(super::contention_aggregate(&event), format!("plan {PLAN}"));
}
