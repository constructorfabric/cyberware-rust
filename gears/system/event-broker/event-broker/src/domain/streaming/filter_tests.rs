//! Pure: interests in, a yes or no per event out. No storage, no runtime.

use crate::domain::model::Sequence;
use chrono::Utc;
use serde_json::json;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{BarrierMode, Event, FilterSpec, Interest, TenantTraversalDepth};

use super::filter::{EventFilter, InterestFilter};

fn gts(id: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(id).expect("static gts id is valid")
}

fn topic() -> GtsInstanceId {
    gts("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
}

fn other_topic() -> GtsInstanceId {
    gts("gts.cf.core.events.topic.v1~x.eb.other.acme.v1")
}

const CREATED: &str = "gts.cf.core.events.event.v1~x.eb.orders.created.v1~";

fn interest(tenant: Uuid, patterns: &[&str]) -> Interest {
    Interest {
        topic: topic(),
        tenant_id: tenant,
        depth: TenantTraversalDepth::CurrentTenant,
        barrier_mode: BarrierMode::Respect,
        types: patterns.iter().map(|p| (*p).to_owned()).collect(),
        filter: None,
    }
}

fn event(tenant: Uuid, on_topic: GtsInstanceId, event_type: &str) -> Event {
    Event {
        id: Uuid::nil(),
        r#type: crate::test_support::event_type_id(event_type),
        topic: on_topic,
        tenant_id: tenant,
        source: "filter-test".to_owned(),
        subject: "order".to_owned(),
        subject_type: "order".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: json!({}),
        meta: None,
        partition: Some(0),
        sequence: Some(Sequence::assigned(1)),
        sequence_time: None,
    }
}

#[test]
fn an_exact_type_on_the_right_topic_and_tenant_matches() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(tenant, &[CREATED])]).expect("compiles");

    assert!(filter.matches(&event(tenant, topic(), CREATED)));
}

#[test]
fn an_event_on_another_topic_is_rejected() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(tenant, &[CREATED])]).expect("compiles");

    assert!(!filter.matches(&event(tenant, other_topic(), CREATED)));
}

#[test]
fn an_event_of_another_tenant_is_rejected() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(tenant, &[CREATED])]).expect("compiles");

    // Segments are shared between subscriptions of different tenants, so this is
    // the predicate that keeps one tenant's events out of another's stream.
    assert!(!filter.matches(&event(Uuid::new_v4(), topic(), CREATED)));
}

#[test]
fn an_unmatched_type_is_rejected() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(tenant, &[CREATED])]).expect("compiles");

    assert!(!filter.matches(&event(
        tenant,
        topic(),
        "gts.cf.core.events.event.v1~x.eb.orders.cancelled.v1~"
    )));
}

#[test]
fn a_wildcard_segment_matches_any_one_segment() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(
        tenant,
        &["gts.cf.core.events.event.v1~x.eb.orders.*.v1~"],
    )])
    .expect("compiles");

    assert!(filter.matches(&event(tenant, topic(), CREATED)));
    assert!(filter.matches(&event(
        tenant,
        topic(),
        "gts.cf.core.events.event.v1~x.eb.orders.cancelled.v1~"
    )));
}

#[test]
fn a_wildcard_does_not_span_several_segments() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(
        tenant,
        &["gts.cf.core.events.event.v1~x.eb.orders.*"],
    )])
    .expect("compiles");

    // A wildcard fills exactly one segment, so a pattern with fewer segments
    // than the id cannot match. Otherwise a trailing `*` would admit everything
    // beneath it, which is a different and much broader rule than GTS states.
    assert!(!filter.matches(&event(tenant, topic(), CREATED)));
}

#[test]
fn any_one_of_several_patterns_suffices() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(
        tenant,
        &[
            "gts.cf.core.events.event.v1~x.eb.orders.cancelled.v1~",
            CREATED,
        ],
    )])
    .expect("compiles");

    assert!(filter.matches(&event(tenant, topic(), CREATED)));
}

#[test]
fn an_interest_with_no_patterns_matches_nothing() {
    let tenant = Uuid::new_v4();
    let filter = InterestFilter::compile(&[interest(tenant, &[])]).expect("compiles");

    // Not "matches everything". An interest that names no types has declared no
    // interest, and defaulting to everything would deliver a tenant's whole
    // topic to a subscription that asked for none of it.
    assert!(!filter.matches(&event(tenant, topic(), CREATED)));
}

#[test]
fn no_interests_at_all_matches_nothing() {
    let filter = InterestFilter::compile(&[]).expect("compiles");

    assert_eq!(filter.interest_count(), 0);
    assert!(!filter.matches(&event(Uuid::new_v4(), topic(), CREATED)));
}

#[test]
fn a_partial_wildcard_segment_is_rejected_at_compile() {
    let tenant = Uuid::new_v4();

    // `orders*` is not a GTS wildcard - a wildcard fills its whole segment.
    // Rejecting at compile is what makes this a `400` at JOIN rather than a
    // stream that silently matches nothing.
    let error = InterestFilter::compile(&[interest(
        tenant,
        &["gts.cf.core.events.event.v1~x.eb.orders*.v1"],
    )])
    .expect_err("must reject a partial wildcard");

    assert!(format!("{error:?}").contains("BadTypePattern"));
}

#[test]
fn more_than_one_wildcard_segment_is_rejected_at_compile() {
    let tenant = Uuid::new_v4();

    let error = InterestFilter::compile(&[interest(
        tenant,
        &["gts.cf.core.events.event.v1~x.eb.*.*.v1"],
    )])
    .expect_err("must reject two wildcards");

    assert!(format!("{error:?}").contains("BadTypePattern"));
}

#[test]
fn interests_on_several_topics_are_kept_apart() {
    let tenant = Uuid::new_v4();
    let mut second = interest(tenant, &[CREATED]);
    second.topic = other_topic();
    let filter =
        InterestFilter::compile(&[interest(tenant, &[CREATED]), second]).expect("compiles");

    assert_eq!(filter.interest_count(), 2);
    assert!(filter.matches(&event(tenant, topic(), CREATED)));
    assert!(filter.matches(&event(tenant, other_topic(), CREATED)));
}

#[test]
fn a_filter_spec_compiles_and_does_not_yet_decide() {
    let tenant = Uuid::new_v4();
    let mut carrying = interest(tenant, &[CREATED]);
    carrying.filter = Some(FilterSpec {
        engine: "cel".to_owned(),
        expression: "event.data.total_cents > 100000".to_owned(),
    });
    let filter = InterestFilter::compile(&[carrying]).expect("a filter spec must not fail compile");

    // The expression language and its engine belong to ADR-0005, still proposed.
    // Until an engine exists a `FilterSpec` neither admits nor rejects, so an
    // event matching topic, type and tenant is delivered. Rejecting the spec at
    // compile would fail a JOIN that the scenarios require to succeed.
    assert!(filter.matches(&event(tenant, topic(), CREATED)));
}
