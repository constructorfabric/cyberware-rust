//! The plan plane, driven through the real router.
//!
//! `tower::ServiceExt::oneshot` over the router `register_rest` mounts, so the
//! extractors, the PEP gate, the repositories and the response serialization are
//! all in the path — the same harness `tests/rest_frontier.rs` established, over
//! a migrated in-memory `SQLite`.
//!
//! Two habits run through every case here and both are deliberate. A denial
//! asserts **403 and an unchanged database**, because a 403 alone would also be
//! produced by a handler that wrote first and checked second — "the authz gate
//! before the repository" is only a claim about source order until somebody
//! observes the store. And a refusal asserts its **discriminator**, never only
//! its status: §3.3 makes the code the one a consumer matches on, and several
//! distinct refusals share one status. Where the refusal mints no code -
//! `DomainError::InvalidRequest` renders `invalid_argument` with no `reason`,
//! which is D-141's shape and the majority of this door's 400s - the
//! discriminator is the canonical family plus the guard's own sentence, and
//! [`refused_by`] is what asserts it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

#[path = "rest_plans/list_query.rs"]
mod list_query;

use axum::http::StatusCode;
use bss_pricing::api::rest::plans::{PLANS, PLANS_COUNTS};
use std::collections::HashMap;

fn clone_path(plan_id: Uuid) -> String {
    format!("{PLANS}/{plan_id}/clone")
}
use rest_support::{
    Harness, approval_rows, audit_rows, body_json, code_in, denial_reason, etag_of, location_of,
    not_found_code, plan_count, plan_row_version, plan_state, problem_code, problem_family,
    refused_by, request, seed_current_plan, seed_draft_plan, seed_foreign_plan, seed_price,
    seed_publishable_plan, violation_for, with_headers,
};
use serde_json::json;
use uuid::Uuid;

fn plan_path(plan_id: Uuid) -> String {
    format!("{PLANS}/{plan_id}")
}

#[tokio::test]
async fn an_unauthenticated_read_is_refused_before_anything_else() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();

    let response = harness
        .anonymous()
        .send(request("GET", &plan_path(plan_id), None))
        .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// **The harness runs production's last layer, and these are the two members that
/// prove it.**
///
/// `canonical_error_middleware` was mounted once in `module.rs` and in **no** test
/// harness, so every functional test in this crate asserted an error body
/// production then rewrites. What it does is not cosmetic: it deserializes any
/// `application/problem+json` body into `Problem`, fills `instance` and `trace_id`,
/// and **re-serializes** — a round trip through one type, which is a projection. A
/// member a handler emits that `Problem` does not carry is dropped in production
/// and was present in every test, on the envelope this gear's whole contract tells
/// a consumer to key on.
///
/// `instance` and `trace_id` are the assertion because they are the two the
/// middleware *adds*: no refusal in the corpus could carry either before it was
/// mounted, so this case cannot pass against a harness that omits the layer. An
/// operator correlating a client's 400 to a server log needs both.
///
/// Asserted on a **403** rather than a 401, so the document under test is one the
/// gear's own ladder produced rather than one the auth extension shortcuts.
#[tokio::test]
async fn a_refusal_carries_the_instance_and_trace_id_the_error_layer_fills() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .denied()
        .send(with_headers(
            "GET",
            &plan_path(plan_id),
            None,
            &[(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let problem = body_json(response).await;
    assert_eq!(
        problem["instance"],
        serde_json::json!(plan_path(plan_id)),
        "the layer fills `instance` from the request path: {problem}"
    );
    assert_eq!(
        problem["trace_id"],
        serde_json::json!("4bf92f3577b34da6a3ce929d0e0e4736"),
        "and `trace_id` from the caller's W3C traceparent: {problem}"
    );
}

#[tokio::test]
async fn a_denied_read_is_403_and_names_the_denial() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .denied()
        .send(request("GET", &plan_path(plan_id), None))
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // **The reason, not the family.** Every 403 this gear can emit is built by
    // `permission_denied()`, so the canonical family is the same for the PDP's
    // refusal and for each of the gear's own arms; `with_reason` is the only
    // free field such an error has.
    //
    // **And no row readback.** A `GET` writes nothing whether the PDP was asked
    // or not, so a version compared across one is green with the whole authz
    // layer removed. Nor would `Harness::recording` prove it: that records the
    // requests the PDP was handed, which is a different claim from the
    // repository never having been reached.
    assert_eq!(
        denial_reason(response).await,
        "no_catalog_role: no catalog role grants this pair",
        "the PDP funnel is what refused: every 403 this gear emits shares the \
         canonical family, so the reason is the only field that says which door, \
         and it must be the PDP's own deny_reason rather than a generic constant"
    );
}

#[tokio::test]
async fn the_open_draft_is_what_an_author_is_answered() {
    // A plan with both a current revision and an open draft: the author is
    // editing the draft, and a read that answered the published revision would
    // hand them a body their next PATCH could not match.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;
    let response = harness
        .allowed()
        .send(request("GET", &plan_path(plan_id), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let published = body_json(response).await;
    assert_eq!(published["lifecycle_state"], serde_json::json!("published"));
    assert_eq!(published["revision"], serde_json::json!(0));

    harness.open_successor(plan_id).await;

    let response = harness
        .allowed()
        .send(request("GET", &plan_path(plan_id), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let draft = body_json(response).await;
    assert_eq!(draft["lifecycle_state"], serde_json::json!("draft"));
    assert_eq!(draft["revision"], serde_json::json!(1));
}

#[tokio::test]
async fn the_read_carries_the_revisions_child_sets_and_its_etag() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    harness.attach_shape(plan_id, 0).await;

    let response = harness
        .allowed()
        .send(request("GET", &plan_path(plan_id), None))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let tag = etag_of(&response);
    let body = body_json(response).await;
    assert_eq!(
        tag,
        Some(format!(
            "\"{}-{}\"",
            body["revision"].as_u64().unwrap(),
            body["row_version"].as_u64().unwrap()
        )),
        "a caller that cannot obtain a tag cannot satisfy the next verb's precondition"
    );
    // Both operands of that equality come out of one response, so it says the
    // tag agrees with the body and not that either is right. The literal is what
    // the fixture is: `attach_shape` files three child sets and each is a write,
    // so the draft stands at revision 0 version 3 — a tag assembled from a
    // counter this surface does not own would have to coincide with both.
    assert_eq!(tag, Some("\"0-3\"".to_owned()), "{body}");

    assert_eq!(
        body["phases"].as_array().map(Vec::len),
        Some(1),
        "S2 §5's PATCH names six facets, so a read that omits them cannot round-trip one: {body}"
    );
    assert_eq!(
        body["addon_rules"].as_array().map(Vec::len),
        Some(1),
        "{body}"
    );
    assert!(body["descriptor_set"].is_object(), "{body}");
    // The fourth child set, present as an empty list on a revision that defines
    // none. Asserted rather than assumed because the `composites` facet's whole
    // round trip depends on the member existing on every read: an author cannot
    // preserve a definition's id (D-106) through a wholesale replace if the read
    // sometimes omits the field it comes from.
    assert_eq!(
        body["composites"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );

    // The fifth (D-319), on the composites' argument exactly: the
    // `period_floor_caps` facet replaces the set wholesale, so an author adding
    // a bound in a second market has to read the first one back to resubmit it.
    assert_eq!(
        body["period_floor_caps"].as_array().map(Vec::len),
        Some(0),
        "{body}"
    );
}

#[tokio::test]
async fn an_absent_plan_and_a_foreign_tenants_plan_answer_identically() {
    // The owner's 200 is the baseline: without it the 404 below would be
    // consistent with the plan simply not existing, and the test would prove
    // nothing about isolation.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let owner = harness
        .allowed()
        .send(request("GET", &plan_path(plan_id), None))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::OK,
        "the baseline: it is readable"
    );

    let foreign = harness
        .other_tenant()
        .send(request("GET", &plan_path(plan_id), None))
        .await;
    let absent = harness
        .allowed()
        .send(request("GET", &plan_path(Uuid::now_v7()), None))
        .await;

    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        absent.status(),
        StatusCode::NOT_FOUND,
        "absent and out-of-scope are one answer, or the surface leaks whose catalog exists"
    );
}

#[tokio::test]
async fn a_plan_whose_only_revision_is_abandoned_is_not_readable() {
    // A read is not one of the four authoring calls S2 §5 owes
    // PLAN_ABANDONED_NO_SUCCESSOR: the plan holds no revision an author can act
    // on, which is the 404 this gear already gives for "nothing here for you".
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    harness.abandon_draft(plan_id, 0).await;

    let response = harness
        .allowed()
        .send(request("GET", &plan_path(plan_id), None))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// The writes.
// ---------------------------------------------------------------------------

/// A minimal well-formed create body.
fn create_body(tier: &str) -> serde_json::Value {
    serde_json::json!({ "plan_tier": tier, "billing_cycle": "recurring" })
}

fn keyed(key: &str) -> Vec<(&str, &str)> {
    vec![("idempotency-key", key)]
}

/// The name goes out on the create and comes back on the read (D-318).
///
/// The round trip is the claim worth pinning: the column, the request member,
/// the two views and the mapper are five places, and a name accepted by the
/// write and dropped by the read looks exactly like a write that never landed.
#[tokio::test]
async fn a_plan_can_be_created_with_a_name_and_reads_back_with_it() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(serde_json::json!({
                "plan_tier": "gold",
                "plan_name": "Managed WordPress",
                "billing_cycle": "recurring",
            })),
            &keyed("create-named"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["plan_name"], serde_json::json!("Managed WordPress"));
    let plan_id = body["plan_id"]
        .as_str()
        .expect("the id is answered")
        .to_owned();

    let read = harness
        .allowed()
        .send(with_headers(
            "GET",
            &format!("{PLANS}/{plan_id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(
        body_json(read).await["plan_name"],
        serde_json::json!("Managed WordPress")
    );
}

/// An unnamed plan answers `null`, not an empty string (D-318).
#[tokio::test]
async fn a_plan_created_without_a_name_answers_null_for_it() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-unnamed"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    // The distinction the whole `PLAN_NAME_INVALID` rule exists to keep: absent
    // is `null`, and `""` is never stored, so a client testing truthiness and a
    // client testing `=== null` agree.
    assert_eq!(
        body_json(response).await["plan_name"],
        serde_json::Value::Null
    );
}

/// The create performs D-19's creation-time act: one terminal `evergreen` phase.
///
/// Pinned on the **201 body** and not only on a later read, because the id is
/// what a caller has to key its price rows on — a phase discoverable only by a
/// second GET is a phase every client has to go looking for, and the client that
/// does not go looking authors rows against an id it invented (which is exactly
/// how the stand acquired a plan whose rows name a phase nobody attached).
#[tokio::test]
async fn a_created_plan_carries_one_terminal_evergreen_phase() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-seeds-phase"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;

    let phases = body["phases"]
        .as_array()
        .expect("the created plan answers its phase set");
    assert_eq!(
        phases.len(),
        1,
        "exactly one phase, and it is the terminal one"
    );
    assert_eq!(phases[0]["kind"], serde_json::json!("evergreen"));
    assert_eq!(phases[0]["converts_to_phase_id"], serde_json::Value::Null);
    assert_eq!(phases[0]["phase_duration_days"], serde_json::Value::Null);
    assert_eq!(phases[0]["display_trial_days"], serde_json::Value::Null);

    // The seed is part of creation, not an edit of it: a bumped version would
    // mean the plan was born carrying a change nobody made, and the next
    // `If-Match` a client sends is the one this body just handed it.
    assert_eq!(body["row_version"], serde_json::json!(0));

    let plan_id = body["plan_id"].as_str().expect("the id is answered");
    let seeded = phases[0]["phase_id"]
        .as_str()
        .expect("the phase id is answered");

    let read = harness
        .allowed()
        .send(with_headers(
            "GET",
            &format!("{PLANS}/{plan_id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let read_body = body_json(read).await;
    assert_eq!(
        read_body["phases"][0]["phase_id"],
        serde_json::json!(seeded),
        "the read answers the same phase the create minted"
    );
}

/// A replay seeds nothing further.
///
/// The guard stores the first caller's body, so the second call must answer the
/// same single phase — not a second one, and not an empty set.
#[tokio::test]
async fn a_replayed_create_answers_the_same_single_phase() {
    let harness = Harness::new().await;

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-seed-replay"),
        ))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await;
    let minted = first_body["phases"][0]["phase_id"].clone();

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-seed-replay"),
        ))
        .await;
    let replay_body = body_json(replay).await;
    assert_eq!(replay_body["phases"].as_array().map(Vec::len), Some(1));
    assert_eq!(replay_body["phases"][0]["phase_id"], minted);
}

/// An empty name is refused rather than stored beside `NULL` (D-318).
#[tokio::test]
async fn an_empty_plan_name_is_refused_at_the_write() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(serde_json::json!({
                "plan_tier": "gold",
                "plan_name": "   ",
                "billing_cycle": "recurring",
            })),
            &keyed("create-blank-name"),
        ))
        .await;

    // **400, not 422**, and the number is the crate's convention rather than
    // this rule's choice: a write-stage violation renders through
    // `failed_precondition()`, which is the architectural 422 this gear answers
    // as 400 — the same status every `rest_prices` write-stage refusal asserts.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "PLAN_NAME_INVALID");
}

#[tokio::test]
async fn a_create_answers_201_with_the_location_and_the_tag_a_caller_needs_next() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = location_of(&response);
    let tag = etag_of(&response);
    let body = body_json(response).await;
    let plan_id = body["plan_id"].as_str().expect("the id is answered");
    assert_eq!(
        location,
        Some(format!("{PLANS}/{plan_id}")),
        "the surface mints the id and has to return it before the row is durable"
    );
    assert_eq!(
        tag,
        Some("\"0-0\"".to_owned()),
        "revision 0 at version 0: the tag names its subject, so the caller can edit exactly \
         the revision it was just handed"
    );
    assert_eq!(body["revision"], serde_json::json!(0));
    assert_eq!(body["lifecycle_state"], serde_json::json!("draft"));
}

#[tokio::test]
async fn a_replayed_create_answers_the_original_id_and_creates_nothing() {
    // The whole reason the response body is recorded: a retry must be told the
    // plan id the FIRST call minted, or the caller ends up with two plans and a
    // reference to neither.
    let harness = Harness::new().await;
    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-2"),
        ))
        .await;
    let first_id = body_json(first).await["plan_id"].clone();

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-2"),
        ))
        .await;

    assert_eq!(replay.status(), StatusCode::CREATED);
    let replayed = body_json(replay).await;
    assert_eq!(
        replayed["plan_id"], first_id,
        "a replay answers the recorded body, id and all"
    );
    assert_eq!(
        plan_count(&harness).await,
        1,
        "an answered key must not have run its mutation again"
    );
}

#[tokio::test]
async fn one_key_with_two_different_bodies_is_refused_by_its_code() {
    let harness = Harness::new().await;
    harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("create-3"),
        ))
        .await;

    let clash = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("platinum")),
            &keyed("create-3"),
        ))
        .await;

    assert_eq!(clash.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(clash).await, "IDEMPOTENCY_PAYLOAD_MISMATCH");
    assert_eq!(plan_count(&harness).await, 1);
}

#[tokio::test]
async fn a_create_without_an_idempotency_key_is_refused_and_writes_nothing() {
    // An unguarded create on a governed authoring plane is the retry hazard the
    // gate exists for; S2 §5 gives this verb the client-key cell, so absent is a
    // malformed request rather than an unguarded execution.
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(request("POST", PLANS, Some(create_body("gold"))))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "Idempotency-Key: this operation is guarded at-most-once",
    );
    assert_eq!(plan_count(&harness).await, 0);
}

// ---------------------------------------------------------------------------
// The clone route (§5 `algo-clone`; D-19, D-275).
// ---------------------------------------------------------------------------

/// A clone answers `201`, the new plan's `Location`, and a receipt naming both
/// plans.
///
/// The `Location` matters more here than on the create: the caller did not
/// choose the id and has no other way to learn it, the receipt being the only
/// place it appears.
#[tokio::test]
async fn a_clone_answers_201_with_the_new_plans_location_and_its_receipt() {
    let harness = Harness::new().await;
    let source = Uuid::now_v7();
    seed_current_plan(&harness, source).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(source),
            None,
            &keyed("clone-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = location_of(&response);
    let body = body_json(response).await;
    let plan_id = body["plan_id"].as_str().expect("the id is answered");
    assert_eq!(location, Some(format!("{PLANS}/{plan_id}")));
    assert_ne!(
        plan_id,
        source.to_string(),
        "a clone is a new plan, not a revision of the source"
    );
    assert_eq!(body["cloned_from"], serde_json::json!(source.to_string()));
    assert_eq!(
        plan_count(&harness).await,
        2,
        "the source and its clone, and nothing else"
    );
    assert_eq!(
        plan_state(&harness, Uuid::parse_str(plan_id).expect("a uuid"), 0).await,
        Some("draft".to_owned()),
        "the clone is an ordinary draft (`inst-cl-draft`)"
    );
}

/// A replay answers the **first** caller's plan and clones nothing.
///
/// The create's reason, sharpened: a caller that retried and got a second plan
/// would hold a reference to neither, and unlike a create it cannot tell the two
/// apart by their content — a clone of one source is identical to another.
#[tokio::test]
async fn a_replayed_clone_answers_the_first_callers_plan_and_clones_nothing() {
    let harness = Harness::new().await;
    let source = Uuid::now_v7();
    seed_current_plan(&harness, source).await;

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(source),
            None,
            &keyed("clone-2"),
        ))
        .await;
    let first_id = body_json(first).await["plan_id"].clone();

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(source),
            None,
            &keyed("clone-2"),
        ))
        .await;

    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(body_json(replay).await["plan_id"], first_id);
    assert_eq!(
        plan_count(&harness).await,
        2,
        "an answered key must not have run its mutation again"
    );
}

/// **One key against two different sources is a payload mismatch.**
///
/// The case the digest exists for. The request carries no body, so without the
/// source in the hash every clone in a tenant would hash identically and this
/// caller would be handed the *other* source's clone — a replay of an act
/// nobody performed.
#[tokio::test]
async fn one_key_against_two_different_sources_is_refused_by_its_code() {
    let harness = Harness::new().await;
    let first_source = Uuid::now_v7();
    let second_source = Uuid::now_v7();
    seed_current_plan(&harness, first_source).await;
    seed_current_plan(&harness, second_source).await;

    harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(first_source),
            None,
            &keyed("clone-3"),
        ))
        .await;
    let second = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(second_source),
            None,
            &keyed("clone-3"),
        ))
        .await;

    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(second).await, "IDEMPOTENCY_PAYLOAD_MISMATCH");
    assert_eq!(
        plan_count(&harness).await,
        3,
        "two sources and one clone: the refused call wrote nothing"
    );
}

/// A clone without an `Idempotency-Key` is refused before it writes.
#[tokio::test]
async fn a_clone_without_an_idempotency_key_is_refused_and_writes_nothing() {
    let harness = Harness::new().await;
    let source = Uuid::now_v7();
    seed_current_plan(&harness, source).await;

    let response = harness
        .allowed()
        .send(with_headers("POST", &clone_path(source), None, &[]))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The same guard as the create's, and the sentence is what says so: a clone
    // whose *source* refused would also be 400 with one plan standing.
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "Idempotency-Key: this operation is guarded at-most-once",
    );
    assert_eq!(plan_count(&harness).await, 1);
}

/// A plan with no **current** revision is not clonable, and the refusal is a
/// `404` naming the source rather than a `500`.
///
/// A draft-only plan is the case: it exists, an author can read it, and there is
/// nothing published to copy. The clone reads the *current* revision because a
/// draft is an edit in progress and not configuration the plan has.
#[tokio::test]
async fn a_plan_with_nothing_published_cannot_be_cloned() {
    let harness = Harness::new().await;
    let source = Uuid::now_v7();
    seed_draft_plan(&harness, source).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(source),
            None,
            &keyed("clone-4"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        not_found_code(response).await,
        "CLONE_SOURCE_NOT_FOUND",
        "section 5 declares the code, and a bare 404 sends the operator looking for a missing \
         id: the plan is right there, it just has nothing published"
    );
    assert_eq!(
        plan_count(&harness).await,
        1,
        "the refusal wrote no half-made clone"
    );
}

/// **The receipt's counts and its notices, on the wire.**
///
/// Every other clone case here clones a bare plan, so every count is
/// structurally zero and `notice_view`'s three wire codes are never rendered —
/// they could be typoed or swapped with the suite green. This is the case that
/// runs them: a source with a shape and a published price row, whose clone
/// copies phases and rows and whose windows deliberately do not travel.
#[tokio::test]
async fn the_receipt_carries_its_counts_and_names_what_stayed_behind() {
    let harness = Harness::new().await;
    let source = Uuid::now_v7();
    seed_draft_plan(&harness, source).await;
    harness.attach_shape(source, 0).await;
    let price = seed_price(&harness, source, "eu").await;
    harness.publish_price(source, price.price_id).await;
    harness.publish(source, 0).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(source),
            None,
            &keyed("clone-5"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert!(
        body["phases_copied"].as_u64().expect("a count") > 0,
        "the source's phase chain came across: {body}"
    );
    assert_eq!(
        body["prices_copied"],
        serde_json::json!(1),
        "the one published row came across as a draft"
    );
    let notices = body["notices"].as_array().expect("an array");
    assert!(
        notices
            .iter()
            .any(|notice| notice["code"] == "no_coverage_scheduled"),
        "windows are Slice 7 runtime state and never travel, so the clone is \
         coverage-blocked and the receipt has to say so: {notices:?}"
    );
    // The control for the case below: this source *has* a phase, so its phases were
    // copied and nothing was seeded. A notice emitted here would be a notice about
    // an act that did not happen.
    assert!(
        notices
            .iter()
            .all(|notice| notice["code"] != "terminal_phase_adopted"
                && notice["code"] != "terminal_phase_minted"),
        "a copied phase set is not a seeded one: {notices:?}"
    );
}

/// **A clone that seeds a phase the operator did not author says so on the wire —
/// 2026-08-17 review, on D-341.**
///
/// The receipt reported `phases_copied: 0` and no notice at all, so the operator's
/// only signal was `prices_copied: 2` beside `no_coverage_scheduled` — which reads as
/// routine follow-up. D-341 calls the seeded phase an operator-visible consequence
/// and lists two outcomes with different consequences: **adopted**, where the copied
/// rows all named the seeded id and the clone is publishable, and **minted**, where
/// they named two or more and every copied row is now stranded under
/// `PHASE_ROW_ORPHANED`. `phases_copied` stays `0` — nothing was copied, and a count
/// that folded the seed in would say a phase came across from a plan that never had
/// one — so the notice is the vehicle.
///
/// `seed_current_plan`'s own shape is the phase-less source: `create_draft` writes no
/// phase row, only `POST /plans` and `attach_shape` do. Which is the point D-341
/// makes about the population — every plan authored before the seed existed is one.
#[tokio::test]
async fn a_clone_that_seeds_its_terminal_phase_names_the_act_in_its_receipt() {
    let harness = Harness::new().await;
    let source = Uuid::now_v7();
    seed_draft_plan(&harness, source).await;
    let price = seed_price(&harness, source, "eu").await;
    harness.publish_price(source, price.price_id).await;
    harness.publish(source, 0).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(source),
            None,
            &keyed("clone-9"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(
        body["phases_copied"],
        serde_json::json!(0),
        "the source held none, so nothing was copied: {body}"
    );
    assert_eq!(body["prices_copied"], serde_json::json!(1));
    let notices = body["notices"].as_array().expect("an array");
    assert!(
        notices
            .iter()
            .any(|notice| notice["code"] == "terminal_phase_adopted"
                && notice["rows"] == serde_json::json!(1)),
        "the one copied row named one id, so the seed adopted it and the row is \
         attached: {notices:?}"
    );
}

/// The response carries **no** `ETag`, and that is a decision rather than an
/// omission.
///
/// Asserted for `module_test`'s reason about deliberate absences: a decision no
/// test reads is one a later group undoes by being helpful. The clone answers a
/// receipt, not a revision, and a tag on a body that is not the resource is a
/// precondition token pointing at nothing.
#[tokio::test]
async fn the_clone_answers_no_etag_because_its_body_is_not_the_resource() {
    let harness = Harness::new().await;
    let source = Uuid::now_v7();
    seed_current_plan(&harness, source).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(source),
            None,
            &keyed("clone-6"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(etag_of(&response), None);
}

/// A source in **another tenant** answers exactly like an absent one.
///
/// The existence-leak probe, on the one route that takes a plan id from a caller
/// and creates something out of it. `load_current`'s tenant filter is what
/// closes it, and this is what says the filter is there.
#[tokio::test]
async fn a_foreign_tenants_plan_cannot_be_cloned_and_reads_like_an_absent_one() {
    let harness = Harness::new().await;
    let foreign = Uuid::now_v7();
    let absent = Uuid::now_v7();
    seed_foreign_plan(&harness, foreign).await;

    let foreign_answer = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(foreign),
            None,
            &keyed("clone-7"),
        ))
        .await;
    let absent_answer = harness
        .allowed()
        .send(with_headers(
            "POST",
            &clone_path(absent),
            None,
            &keyed("clone-8"),
        ))
        .await;

    assert_eq!(foreign_answer.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent_answer.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        not_found_code(foreign_answer).await,
        not_found_code(absent_answer).await,
        "a foreign plan and an absent one must be indistinguishable, code included"
    );
    assert_eq!(
        plan_count(&harness).await,
        0,
        "the foreign plan is another tenant's and neither call wrote anything here"
    );
}

#[tokio::test]
async fn a_patch_under_a_stale_tag_is_refused_by_its_code_and_the_row_does_not_move() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let before = plan_row_version(&harness, plan_id, 0).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-9\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "STALE_VERSION");
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        before,
        "a refused precondition leaves the row's tag exactly where it was"
    );
}

#[tokio::test]
async fn a_patch_without_a_precondition_is_a_malformed_request() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(request(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "D-141: an absent precondition mints no code of its own"
    );
    // It mints no code, which is why this asserts the family and the sentence:
    // the body is a legal one-facet `shape`, so the only thing wrong with the
    // request is the absent header, and nothing but the sentence says so.
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "If-Match is required on this verb",
    );
    assert_eq!(plan_row_version(&harness, plan_id, 0).await, Some(0));
}

#[tokio::test]
async fn a_patch_naming_two_facets_is_refused_rather_than_half_applied() {
    // Each facet compare-and-swaps on the revision's own row version and bumps
    // it, so the second could not match the tag the first advanced - and the two
    // are separate transactions with a visible half-applied state in between.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": { "plan_tier": "platinum" },
                "addon_rules": []
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "carries exactly one of",
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "neither facet may have landed"
    );
}

#[tokio::test]
async fn a_required_addon_with_no_ceiling_is_refused_by_the_write_not_the_database() {
    // `pricing_plan_addon_rule` carries `required => max_qty >= 1` as a CHECK, so
    // the store refuses this row whatever the surface believes. Before the surface
    // ran the rule, that refusal reached the caller as a **500 naming the
    // constraint** — measured against a live stand while seeding a catalog, where
    // every required add-on 500ed.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "addon_rules": [{
                    "addon_sku_id": Uuid::now_v7(),
                    "required": true,
                    "min_qty": serde_json::Value::Null,
                    "max_qty": serde_json::Value::Null,
                    "step_qty": serde_json::Value::Null,
                    "price_override_ref": serde_json::Value::Null,
                    "depends_on": [],
                    "conflicts_with": []
                }]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "ADDON_QTY_RANGE_INVALID");
}

#[tokio::test]
async fn a_required_addon_with_a_ceiling_still_lands() {
    // The positive control. Without it the check could refuse every required
    // add-on and the case above would still pass.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "addon_rules": [{
                    "addon_sku_id": Uuid::now_v7(),
                    "required": true,
                    "min_qty": 1,
                    "max_qty": 5,
                    "step_qty": serde_json::Value::Null,
                    "price_override_ref": serde_json::Value::Null,
                    "depends_on": [],
                    "conflicts_with": []
                }]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
}

/// **The composite facet lands, and the read hands back what it stored.**
///
/// Before this facet existed `PlanShapeRepo::replace_composites` had no caller in
/// `src/` at all: the `_on` form was reached only by the clone, `copy_composites`
/// only by `open_revision`, and every production path was therefore a copy of a
/// set nothing could originate. So the first thing to prove is not that the store
/// works - four suites already prove that against the repository - but that a
/// **client** can put a composite into it.
///
/// The `GET` half is the other load-bearing one. The facet replaces the set
/// wholesale and `composite_id` is what survives a replace, so a read that did not
/// echo the ids would leave an author able only to re-mint them, and D-106's
/// stable identity would hold for the copy-forward and break for every edit.
#[tokio::test]
async fn a_composites_facet_lands_and_the_read_echoes_what_it_stored() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "composites": [{
                    "output_unit": "vm",
                    "constituent_units": ["vcpu", "ram"],
                    "formula": { "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }
                }]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let patched = body_json(response).await;
    let stored = patched["composites"]
        .as_array()
        .unwrap_or_else(|| panic!("the patch answers the revision it wrote: {patched}"));
    assert_eq!(stored.len(), 1, "{patched}");
    assert_eq!(
        stored[0]["output_unit"],
        serde_json::json!("vm"),
        "{patched}"
    );
    assert_eq!(
        stored[0]["constituent_units"],
        serde_json::json!(["vcpu", "ram"]),
        "{patched}"
    );
    // The formula is opaque to this gear (A4), so what a round trip must preserve
    // is the whole document and not a token from it.
    assert_eq!(
        stored[0]["formula"],
        serde_json::json!({ "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }),
        "{patched}"
    );
    // Minted by the surface, because the body named none.
    let minted = stored[0]["composite_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .unwrap_or_else(|| panic!("an omitted id is minted, never left null: {patched}"));

    let read = body_json(
        harness
            .allowed()
            .send(request("GET", &plan_path(plan_id), None))
            .await,
    )
    .await;

    assert_eq!(
        read["composites"], patched["composites"],
        "a GET answers the set the PATCH stored, id included - otherwise the id was \
         a per-response value and not a persisted one: {read}"
    );
    assert_eq!(
        read["composites"][0]["composite_id"],
        serde_json::json!(minted.to_string()),
        "{read}"
    );
}

/// **A `descriptor_set` facet lands from a client, and the read echoes it.**
///
/// The sixth facet, and the only one whose write path no request in this crate
/// drove: `descriptor_set` appeared here as a read assertion alone, so
/// `Facet::DescriptorSet` → `PlanShapeRepo::set_descriptor_set` had no
/// client-driven coverage — no write, no round trip, no audit record. That is the
/// D-254 class the composites and period-floor-cap cases were written to close,
/// and the audit siblings below argue each shape-repo facet owes its own record.
#[tokio::test]
async fn a_descriptor_set_facet_lands_and_the_read_echoes_what_it_stored() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let set = serde_json::json!({
        "invoice_line_template": "{plan} - {period}",
        "gl_code": "4000-SAAS",
        "itemization_rule": "per_charge",
        "additional": { "cost_centre": "emea-01" }
    });

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "descriptor_set": set })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let patched = body_json(response).await;
    assert_eq!(
        patched["descriptor_set"], set,
        "the patch answers the descriptor set it wrote: {patched}"
    );

    let read = body_json(
        harness
            .allowed()
            .send(request("GET", &plan_path(plan_id), None))
            .await,
    )
    .await;
    assert_eq!(
        read["descriptor_set"], set,
        "a GET answers the set the PATCH stored, not a per-response value: {read}"
    );

    let records = plan_records(&harness, plan_id).await;
    assert_eq!(
        records
            .iter()
            .filter(|(action, ..)| action == "update")
            .count(),
        1,
        "one record for the facet, not one per column: {records:?}"
    );
}

/// **An author who sends the id back keeps the definition; one who omits it mints
/// a new one.**
///
/// This is the whole of why [`PlanView`] gained the facet in the same change the
/// `PATCH` did, stated as an executable fact. The facet replaces wholesale, so
/// "edit this composite's formula" is expressed as "send the set again with the
/// same id and a different formula" - and a surface that could not preserve the id
/// would make every formula edit a new definition, which is exactly what D-106
/// keeps stable so that a draft's edit leaves the published revision alone.
///
/// [`PlanView`]: bss_pricing::api::rest::plans::PlanView
#[tokio::test]
async fn a_supplied_composite_id_survives_a_wholesale_replace() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let chosen = Uuid::now_v7();

    let first = body_json(
        harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &plan_path(plan_id),
                Some(serde_json::json!({
                    "composites": [{
                        "composite_id": chosen,
                        "output_unit": "vm",
                        "constituent_units": ["vcpu", "ram"],
                        "formula": { "op": "sum" }
                    }]
                })),
                &[("if-match", "\"0-0\"")],
            ))
            .await,
    )
    .await;
    assert_eq!(
        first["composites"][0]["composite_id"],
        serde_json::json!(chosen.to_string()),
        "a supplied id is the definition's identity and is not re-minted: {first}"
    );

    // The same definition, re-authored with a new formula under its own id. The
    // tag has moved because the facet bumped the revision's version.
    let tag = format!("\"0-{}\"", first["row_version"].as_u64().unwrap());
    let second = body_json(
        harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &plan_path(plan_id),
                Some(serde_json::json!({
                    "composites": [{
                        "composite_id": chosen,
                        "output_unit": "vm",
                        "constituent_units": ["vcpu", "ram", "disk"],
                        "formula": { "op": "weighted_sum" }
                    }]
                })),
                &[("if-match", &tag)],
            ))
            .await,
    )
    .await;

    assert_eq!(
        second["composites"][0]["composite_id"],
        serde_json::json!(chosen.to_string()),
        "an edit under the same id is an edit and not a second definition: {second}"
    );
    assert_eq!(
        second["composites"][0]["formula"],
        serde_json::json!({ "op": "weighted_sum" }),
        "{second}"
    );
    assert_eq!(
        second["composites"].as_array().map(Vec::len),
        Some(1),
        "the facet replaces wholesale, so re-sending one definition leaves one: {second}"
    );
}

/// An empty list withdraws every composite, which is the only way to withdraw one.
///
/// Stated because the store's operation is delete-then-insert and there is no
/// per-definition verb: if `[]` did not clear the set, a composite authored by
/// mistake could not be removed from a draft at all except by abandoning the whole
/// revision.
#[tokio::test]
async fn an_empty_composites_list_withdraws_the_whole_set() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let attached = body_json(
        harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &plan_path(plan_id),
                Some(serde_json::json!({
                    "composites": [{
                        "output_unit": "vm",
                        "constituent_units": ["vcpu", "ram"],
                        "formula": {}
                    }]
                })),
                &[("if-match", "\"0-0\"")],
            ))
            .await,
    )
    .await;
    assert_eq!(attached["composites"].as_array().map(Vec::len), Some(1));

    let tag = format!("\"0-{}\"", attached["row_version"].as_u64().unwrap());
    let cleared = body_json(
        harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &plan_path(plan_id),
                Some(serde_json::json!({ "composites": [] })),
                &[("if-match", &tag)],
            ))
            .await,
    )
    .await;

    assert_eq!(
        cleared["composites"].as_array().map(Vec::len),
        Some(0),
        "{cleared}"
    );
}

#[tokio::test]
async fn a_patch_on_a_plan_with_no_open_draft_opens_the_successor() {
    // D-146's own sentence requires this arm to exist. The successor's number
    // comes from the whole chain, and the patch lands on the newly opened
    // revision rather than on the frozen one the caller read.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["revision"], serde_json::json!(1), "{body}");
    assert_eq!(
        body["lifecycle_state"],
        serde_json::json!("draft"),
        "{body}"
    );
    assert_eq!(body["plan_tier"], serde_json::json!("platinum"), "{body}");
    assert_eq!(
        plan_state(&harness, plan_id, 0).await.as_deref(),
        Some("published"),
        "the predecessor is untouched until the successor publishes"
    );
}

#[tokio::test]
async fn a_patch_on_the_successor_arm_asserts_the_current_revisions_tag() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "STALE_VERSION");
    assert_eq!(
        plan_state(&harness, plan_id, 1).await,
        None,
        "no successor may be opened under a precondition that did not hold"
    );
}

#[tokio::test]
async fn a_patch_on_a_retired_plan_is_a_stop_and_says_so_in_its_own_words() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;
    harness.retire(plan_id, 0).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    // The design set calls this a 422 and it arrives as a 400 carrying its code:
    // §3.3's status-rendering rule, asserted end to end for the first time.
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the platform has no 422 category; the code is the discriminator"
    );
    assert_eq!(problem_code(response).await, "PLAN_RETIRED_NO_SUCCESSOR");
}

/// The world in which the two version counters **coincide**, which is the only
/// world where a revision binding is the thing doing the refusing.
///
/// Seed a draft at revision 0, version 0; publish it, so revision 0 moves to
/// version 1; open the successor, so revision 1 stands at version 0. The plan
/// now holds a published revision 0 at version 1 and an open draft revision 1 at
/// version **0** — exactly where revision 0 stood when the caller read it.
///
/// A tag of `"0-0"` is therefore **version-correct for the draft and
/// revision-wrong**: the compare-and-swap it would produce (`expected = 0`
/// against revision 1, which stands at 0) *succeeds*. Nothing but the revision
/// binding can refuse it. A staging where the versions differ — say `"0-1"`
/// against a draft at 0 — is answered 409 by the plain staleness test whether
/// the binding exists or not, which is a test that passes for a reason other
/// than the property it names.
async fn plan_with_a_successor_at_the_predecessors_version(harness: &Harness) -> Uuid {
    let plan_id = Uuid::now_v7();
    seed_current_plan(harness, plan_id).await;
    harness.open_successor(plan_id).await;
    assert_eq!(
        (
            plan_row_version(harness, plan_id, 0).await,
            plan_row_version(harness, plan_id, 1).await
        ),
        (Some(1), Some(0)),
        "the staging is the test: revision 1 must stand exactly where revision 0 was read"
    );
    plan_id
}

#[tokio::test]
async fn a_tag_read_from_one_revision_cannot_edit_the_open_draft() {
    // The lost update D-145 spends two paragraphs eliminating, arriving through
    // the **revision** instead of the version - and reachable with no race at
    // all, because `/plans/{planId}` names no revision and the two counters are
    // unrelated. This is `target_revision`'s open-draft arm.
    let harness = Harness::new().await;
    let plan_id = plan_with_a_successor_at_the_predecessors_version(&harness).await;

    let wrong_subject = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        wrong_subject.status(),
        StatusCode::CONFLICT,
        "the version is right for the draft and the revision is not; only the binding refuses"
    );
    assert_eq!(problem_code(wrong_subject).await, "STALE_VERSION");
    assert_eq!(
        plan_row_version(&harness, plan_id, 1).await,
        Some(0),
        "the successor must not have moved under a tag read from its predecessor"
    );

    // The tag that names the successor is the one that edits it.
    let correct = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"1-0\"")],
        ))
        .await;

    assert_eq!(correct.status(), StatusCode::OK);
    let body = body_json(correct).await;
    assert_eq!(body["revision"], serde_json::json!(1), "{body}");
    assert_eq!(body["plan_tier"], serde_json::json!("platinum"), "{body}");
}

#[tokio::test]
async fn a_tag_read_from_one_revision_cannot_abandon_another() {
    // The same staging against `abandon_plan_draft`'s own binding. It needs its
    // own case: the abandon path resolves the draft itself and never goes
    // through `target_revision`, so the patch arm's guard says nothing about it.
    // What an unbound tag destroys here is a whole revision.
    let harness = Harness::new().await;
    let plan_id = plan_with_a_successor_at_the_predecessors_version(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &format!("{PLANS}/{plan_id}/abandon"),
            None,
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "STALE_VERSION");
    assert_eq!(
        plan_state(&harness, plan_id, 1).await.as_deref(),
        Some("draft"),
        "a tag read from revision 0 must not tombstone revision 1"
    );

    // The tag that names the draft is the one that discards it.
    let correct = harness
        .allowed()
        .send(with_headers(
            "POST",
            &format!("{PLANS}/{plan_id}/abandon"),
            None,
            &[("if-match", "\"1-0\"")],
        ))
        .await;

    assert_eq!(correct.status(), StatusCode::OK);
    assert_eq!(
        plan_state(&harness, plan_id, 1).await.as_deref(),
        Some("abandoned")
    );
}

#[tokio::test]
async fn a_tag_read_from_a_tombstone_cannot_open_a_successor() {
    // `target_revision`'s successor arm, staged so the versions coincide there
    // too. Seed, publish (revision 0 -> version 1), open the successor, then
    // abandon it (revision 1 -> version 1). The plan now holds **no** open draft,
    // a current revision 0 at version 1, and a tombstone revision 1 at version 1.
    //
    // A tag of `"1-1"` is the tombstone's own, and its version is exactly the
    // current revision's - so the staleness test passes and only the revision
    // binding can refuse. Without it, a successor opens under a tag read from a
    // revision that was discarded.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;
    harness.open_successor(plan_id).await;
    harness.abandon_draft(plan_id, 1).await;
    assert_eq!(
        (
            plan_row_version(&harness, plan_id, 0).await,
            plan_row_version(&harness, plan_id, 1).await
        ),
        (Some(1), Some(1)),
        "the staging is the test: the tombstone must stand exactly where the current revision does"
    );

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"1-1\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "STALE_VERSION");
    assert_eq!(
        plan_state(&harness, plan_id, 2).await,
        None,
        "no successor may be opened under a tag read from a revision that is not the current one"
    );

    // The tag that names the current revision opens the successor.
    let correct = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    assert_eq!(correct.status(), StatusCode::OK);
    assert_eq!(
        body_json(correct).await["revision"],
        serde_json::json!(2),
        "revision 2, because 1 stays consumed (D-145)"
    );
}

#[tokio::test]
async fn a_read_hands_back_a_tag_that_names_the_revision_it_answered() {
    // The other half: a caller can only obtain a revision-qualified tag if the
    // read emits one, so a `GET` that answered a bare version would make every
    // mutating verb unsatisfiable.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;
    harness.open_successor(plan_id).await;

    let response = harness
        .allowed()
        .send(request("GET", &plan_path(plan_id), None))
        .await;

    assert_eq!(
        etag_of(&response),
        Some("\"1-0\"".to_owned()),
        "the tag names the revision the read answered, not just its version"
    );
}

#[tokio::test]
async fn an_unqualified_tag_is_a_malformed_request_on_a_plan_route() {
    // A bare version is what a caller of the price plane sends, and what this
    // surface used to accept. It has to be refused rather than read as revision
    // 0, or the whole distinction is advisory.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    // **Each value paired with the guard that must refuse it**, because the six do
    // not share one. Four are the revision-tag parser's own sentence; `*` and
    // `W/"..."` are refused earlier, by `strong_tag_body`, and are here because a
    // caller of the price plane sends them. Asserting the status alone let the
    // wildcard stand for the parser and the parser for the wildcard - and every one
    // of the six carries a legal `shape` facet, so a door that had stopped reading
    // the tag at all would still answer 400.
    let unreadable = [
        (
            "\"0\"",
            "a plan revision's tag quotes its revision and its version",
        ),
        (
            "\"0-\"",
            "a plan revision's tag quotes its revision and its version",
        ),
        (
            "\"-0\"",
            "a plan revision's tag quotes its revision and its version",
        ),
        (
            "\"0-0-0\"",
            "a plan revision's tag quotes its revision and its version",
        ),
        ("*", "the wildcard matches any version"),
        ("W/\"0-0\"", "RFC 9110 forbids a weak validator here"),
    ];
    for (raw, sentence) in unreadable {
        let response = harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &plan_path(plan_id),
                Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
                &[("if-match", raw)],
            ))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{raw} must not be readable as a plan precondition"
        );
        refused_by(&body_json(response).await, "invalid_argument", sentence);
    }
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "none of them may have edited anything"
    );
}

#[tokio::test]
async fn an_abandon_tombstones_the_draft_and_keeps_its_number() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &format!("{PLANS}/{plan_id}/abandon"),
            None,
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["lifecycle_state"], serde_json::json!("abandoned"));
    assert_eq!(
        plan_state(&harness, plan_id, 0).await.as_deref(),
        Some("abandoned"),
        "the row is flipped, never deleted: the number it consumed stays consumed (D-145)"
    );
}

#[tokio::test]
async fn abandoning_a_plan_with_no_open_draft_describes_no_alternative_action() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &format!("{PLANS}/{plan_id}/abandon"),
            None,
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "LIFECYCLE_FORBIDDEN");
}

#[tokio::test]
async fn every_authoring_verb_on_a_spent_plan_says_the_id_is_spent() {
    // The refusal S2 §5 explicitly owed this group. Before it, a plan whose only
    // revision is abandoned answered PATCH and abandon with a not-found - which
    // tells an operator to look for a plan that is right there.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    harness.abandon_draft(plan_id, 0).await;

    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(patched).await, "PLAN_ABANDONED_NO_SUCCESSOR");

    let abandoned = harness
        .allowed()
        .send(with_headers(
            "POST",
            &format!("{PLANS}/{plan_id}/abandon"),
            None,
            &[("if-match", "\"0-1\"")],
        ))
        .await;
    assert_eq!(abandoned.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(abandoned).await, "PLAN_ABANDONED_NO_SUCCESSOR");
}

#[tokio::test]
async fn every_write_is_denied_with_the_database_unchanged() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let before = plan_row_version(&harness, plan_id, 0).await;

    for (method, path, body, headers) in [
        (
            "POST",
            PLANS.to_owned(),
            Some(create_body("gold")),
            vec![("idempotency-key", "denied-1")],
        ),
        (
            "PATCH",
            plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            vec![("if-match", "\"0-0\"")],
        ),
        (
            "POST",
            format!("{PLANS}/{plan_id}/abandon"),
            None,
            vec![("if-match", "\"0-0\"")],
        ),
    ] {
        let response = harness
            .denied()
            .send(with_headers(method, &path, body, &headers))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {path} must be denied"
        );
    }

    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        before,
        "the gate runs before the repository, which is only observable as an unchanged store"
    );
    assert_eq!(
        plan_count(&harness).await,
        1,
        "a denied create must leave no row behind"
    );
}

#[tokio::test]
async fn a_write_aimed_at_a_tenant_outside_the_compiled_scope_is_denied() {
    // `access_scope`'s membership assertion: the degraded flat-`In` decision does
    // not re-check `owner_tenant_id`, so a write anchored to a tenant the caller
    // is not authorized for has to be refused there or nowhere. The caller here
    // is authenticated in this tenant and authorized only for the other one.
    let harness = Harness::new().await;

    let response = harness
        .scope_mismatch()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("cross-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        plan_count(&harness).await,
        0,
        "a cross-tenant write must leave no row behind"
    );
}

// ---------------------------------------------------------------------------
// The audit trail the three plan routes write (D-135, D-145, D-158).
// ---------------------------------------------------------------------------

/// The records on one plan's segment, in seq order.
///
/// The segment is the **plan's** (D-135 keys it on the audited subject's
/// aggregate), so this is exactly the answer to "who changed this plan" — the
/// question `pricing_audit_log` answered with nothing before this group.
async fn plan_records(harness: &Harness, plan_id: Uuid) -> Vec<(String, String, String)> {
    rest_support::audit_rows(harness)
        .await
        .into_iter()
        .filter(|row| row.chain_id == plan_id)
        .map(|row| (row.action, row.subject_kind, row.subject_ref))
        .collect()
}

#[tokio::test]
async fn a_plan_create_writes_exactly_one_record_naming_the_revision() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("audit-create"),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let plan_id = body_json(response).await["plan_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("the id is answered");

    assert_eq!(
        plan_records(&harness, plan_id).await,
        vec![(
            "create".to_owned(),
            "plan_revision".to_owned(),
            format!("{plan_id}/0")
        )],
        "one record, naming the (plan_id, revision) durable name"
    );
}

#[tokio::test]
async fn a_plan_patch_writes_exactly_one_record_whichever_facet_it_took() {
    // One record per **route**, not per table the facet touched: the mutation an
    // auditor is asked about is the operator's call, and a `PATCH` submits
    // exactly one facet (D-173's one-facet rule).
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "silver" } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let records = plan_records(&harness, plan_id).await;
    assert_eq!(
        records.last(),
        Some(&(
            "update".to_owned(),
            "plan_revision".to_owned(),
            format!("{plan_id}/0")
        )),
        "{records:?}"
    );
    assert_eq!(
        records
            .iter()
            .filter(|(action, ..)| action == "update")
            .count(),
        1,
        "exactly one, not one per child table: {records:?}"
    );
}

#[tokio::test]
async fn a_plan_patch_on_a_phase_facet_also_writes_exactly_one_record() {
    // The other three facets run through `plan_shape_repo`, each in its own
    // transaction, so each needs its own record - and a test on the shape facet
    // alone would pass over three writers that do not write.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [{
                    "phase_id": Uuid::now_v7(),
                    "kind": "evergreen",
                    "ordinal": 0
                }]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let records = plan_records(&harness, plan_id).await;
    assert_eq!(
        records
            .iter()
            .filter(|(action, ..)| action == "update")
            .count(),
        1,
        "{records:?}"
    );
}

/// One phase-chain body, so the two pins below differ in nothing but the plan.
fn phase_chain(phase_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "phases": [{ "phase_id": phase_id, "kind": "evergreen", "ordinal": 0 }]
    })
}

/// **Two plans of one tenant may each file the same phase id — D-340.**
///
/// This case is the inversion of a pin, and the pin asked for it: it read
/// `…collide_on_a_shared_phase_id_and_the_second_answers_500` and its doc said
/// that if a later slice widened the key this would redden and the change would
/// get written down. `pricing_plan_phase`'s key is tenant- and plan-scoped, so the change is written down
/// here.
///
/// What it was: `PRIMARY KEY (phase_id, plan_revision)` named neither `plan_id`
/// nor `tenant_id`, and `phase_id` is **client-supplied** — `PlanPhaseView` is
/// `api_dto(request, response)` and its own doc invites reuse across revisions
/// (D-83). So two plans of one tenant standing at revision `0` could not both file
/// a phase under one id: the first was a 200 and the second a generic `500`
/// advising a retry that could never succeed. On the stand that produced five
/// drafts keying price rows on one id of which four could never attach it, and a
/// scope key is a price row's identity, so those four had nothing but deletion.
///
/// What still refuses, and is **not** asserted here: one plan's revision naming
/// the same id twice, which the facet's own payload is now the only way to reach.
/// That is `PHASE_ID_IN_USE`'s job and it is pinned where the code lives.
#[tokio::test]
async fn two_plans_of_one_tenant_may_each_file_the_same_phase_id() {
    let harness = Harness::new().await;
    let shared_phase = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    seed_draft_plan(&harness, first).await;
    seed_draft_plan(&harness, second).await;

    let accepted = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(first),
            Some(phase_chain(shared_phase)),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(
        accepted.status(),
        StatusCode::OK,
        "the first filing must succeed, or the second proves nothing"
    );

    let shared = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(second),
            Some(phase_chain(shared_phase)),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        shared.status(),
        StatusCode::OK,
        "the id belongs to a plan now, so the sibling may hold it too"
    );
    // Read back off the answered body rather than inferred from the status: a 200
    // over a write that stored a different id would satisfy the line above.
    let body = body_json(shared).await;
    assert_eq!(
        body["phases"].as_array().map(Vec::len),
        Some(1),
        "the facet is a wholesale replace, so the seeded terminal phase is gone"
    );
    assert_eq!(
        body["phases"][0]["phase_id"],
        serde_json::json!(shared_phase),
        "and the phase the second plan holds is the shared one"
    );
    // The write landed, which is the other half of the inversion: this assertion
    // read `Some(0)` while the PATCH was refused and the revision unmoved.
    assert_eq!(
        plan_row_version(&harness, second, 0).await,
        Some(1),
        "a phase write advances the revision it belongs to"
    );
}

/// **And a phase id one tenant holds is free for another — D-340.**
///
/// The other inversion, and the half that matters for isolation. `phase_id` is
/// client-supplied and the old key carried no `tenant_id`, so an authenticated
/// caller of one tenant occupied a `(phase_id, 0)` slot no caller of any other
/// tenant could then use: a tenant's write could be made to fail by a stranger,
/// and the difference between that failure and a success answered *is this id in
/// use somewhere I cannot read* — an existence oracle over another tenant's rows,
/// on a table this gear scopes by `tenant_id` everywhere else.
#[tokio::test]
async fn a_phase_id_one_tenant_holds_is_free_for_another() {
    let harness = Harness::new().await;
    let shared_phase = Uuid::now_v7();
    let mine = Uuid::now_v7();
    let theirs = Uuid::now_v7();
    seed_draft_plan(&harness, mine).await;
    rest_support::seed_foreign_plan(&harness, theirs).await;

    let accepted = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(mine),
            Some(phase_chain(shared_phase)),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(accepted.status(), StatusCode::OK);

    let theirs_too = harness
        .other_tenant()
        .send(with_headers(
            "PATCH",
            &plan_path(theirs),
            Some(phase_chain(shared_phase)),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        theirs_too.status(),
        StatusCode::OK,
        "the key carries the tenant now, so one tenant's id says nothing about another's"
    );
    let body = body_json(theirs_too).await;
    assert_eq!(
        body["phases"][0]["phase_id"],
        serde_json::json!(shared_phase),
        "and the foreign tenant's plan holds the id, so this is not a 200 over a no-op"
    );
}

/// **A `phases` payload naming one id twice answers `PHASE_ID_IN_USE` (409), not
/// a `500` advising a retry — D-340.**
///
/// The two cases above are the collisions D-340 made legal; this is the one it
/// left, and after the widening it is the **only reachable** arrangement. The
/// facet is a wholesale replace — `replace_phases_on` deletes the revision's
/// phase rows and re-inserts the payload inside one transaction — so no other
/// writer can be holding a `(tenant, plan, revision, phase)` slot this write
/// wants. The one caller who can is the payload itself.
///
/// What it answered before: every `DbErr` out of `insert_phases` mapped to
/// `RepoError::Db` and reached the caller as `500 Internal, "please retry
/// later"` — false for a class no retry can fix, and naming neither the
/// constraint nor the id, which is the single fact an author needs.
///
/// **The colliding payload is a graph-valid chain**, and that is load-bearing twice
/// over. Two terminal rows collide on `uq_pricing_plan_phase_terminal` —
/// `(plan_id, plan_revision) WHERE converts_to_phase_id IS NULL` — which is a
/// *different* constraint whose remedy is to give one of them a `convertsToPhaseId`,
/// and would let this case pass while the primary key's refusal was still untyped;
/// `plan_shape_repo_tests` pins that discrimination on both engines' renderings
/// without a database. **And since the 2026-08-17 review the chain itself is judged
/// at this door** (`inst-ph-graph`, write stage), so a payload has to be a legal
/// chain to reach the key at all: this one repeats its **trial** phase, which leaves
/// exactly one terminal phase, no dangling edge and no cycle. The two-row form this
/// case used before — one id twice, the first converting to itself — is a self-cycle
/// and is now refused by the graph rule before the store ever sees it, which would
/// have turned a pin on `PHASE_ID_IN_USE` into a pin on nothing.
#[tokio::test]
async fn a_phases_payload_naming_one_id_twice_answers_the_conflict_and_names_it() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let phase = Uuid::now_v7();
    let terminal = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    // The positive control, first and on the same plan: a two-row payload is
    // accepted, so what the refusal below discriminates is the repeated id and
    // not the arity. Without it a door that refused every multi-phase chain —
    // every trial-plus-evergreen plan in the gear — would satisfy the assertion
    // that follows. The trial converts into the phase beside it: it used to name a
    // freshly minted id belonging to no phase of the payload, which the write-stage
    // `inst-ph-graph` now refuses as a dangling edge — a control carrying the fault
    // the rule judges.
    let chain = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    {
                        "phase_id": phase,
                        "kind": "trial",
                        "ordinal": 0,
                        "converts_to_phase_id": terminal,
                        "phase_duration_days": 14
                    },
                    { "phase_id": terminal, "kind": "evergreen", "ordinal": 1 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(
        chain.status(),
        StatusCode::OK,
        "two distinct phases in one payload must still be accepted"
    );

    let collided = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    {
                        "phase_id": phase,
                        "kind": "trial",
                        "ordinal": 0,
                        "converts_to_phase_id": terminal,
                        "phase_duration_days": 14
                    },
                    { "phase_id": terminal, "kind": "evergreen", "ordinal": 1 },
                    {
                        "phase_id": phase,
                        "kind": "trial",
                        "ordinal": 2,
                        "converts_to_phase_id": terminal,
                        "phase_duration_days": 14
                    }
                ]
            })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    assert_eq!(collided.status(), StatusCode::CONFLICT);
    let body = body_json(collided).await;
    assert_eq!(
        code_in(&body),
        "PHASE_ID_IN_USE",
        "the code is the discriminator a client matches on: {body}"
    );
    assert!(
        body.to_string().contains(&phase.to_string()),
        "the refusal must name the id, which is the one fact the author acts on: {body}"
    );

    // The refused replace rolled back whole: the chain the control wrote is
    // still there and the revision did not move. A 409 over a half-applied
    // delete-then-insert would leave the plan with fewer phases than it had.
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(1),
        "a refused child write must not move the revision"
    );
    let read = body_json(
        harness
            .allowed()
            .send(request("GET", &plan_path(plan_id), None))
            .await,
    )
    .await;
    assert_eq!(
        read["phases"].as_array().map(Vec::len),
        Some(2),
        "the accepted chain survives the refusal: {read}"
    );
}

/// **Two terminal phases in one payload is refused by the rule that owns the
/// requirement — 2026-08-17 review, on D-340.**
///
/// It answered `500 … Please retry later`. The payload trips
/// `uq_pricing_plan_phase_terminal`, which `names_the_phase_key` deliberately routes
/// away from `PHASE_ID_IN_USE` — rightly, since the remedy is to give one of them a
/// `convertsToPhaseId` rather than to rename it — and the non-matching arm lands in
/// `RepoError::Db`. That is the exact class D-340 filed as `[H]`: caller-owned,
/// caller-fixable, no retry helps, and the response withholds the one fact the
/// author needs.
///
/// The fix is at the door and not at the store: `inst-ph-graph` already owns "not
/// exactly one terminal phase" and says in as many words that the partial `UNIQUE`
/// can carry only the at-most-one half, the pipeline running before the write. So the
/// rule refuses it with its own message and the constraint becomes a backstop no
/// caller reaches — rather than a second store-side owner of one requirement, which
/// is how two owners of one rule come to disagree.
#[tokio::test]
async fn a_phases_payload_carrying_two_terminal_phases_is_refused_by_the_rule_that_owns_it() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    { "phase_id": first, "kind": "evergreen", "ordinal": 0 },
                    { "phase_id": second, "kind": "evergreen", "ordinal": 1 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        refused.status(),
        StatusCode::BAD_REQUEST,
        "a caller-fixable payload fault is not a storage failure"
    );
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PHASE_GRAPH_INVALID", "{body}");
    let rendered = body.to_string();
    assert!(
        rendered.contains(&first.to_string()) && rendered.contains(&second.to_string()),
        "the refusal names both claimants, because either one may be the one to convert: {body}"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "a refused child write must not move the revision"
    );
}

/// **A `convertsToPhaseId` naming nothing is refused at the write too — the same
/// rule, the same door.**
///
/// This payload was **accepted** (`200`) and refused at publish. It is the other half
/// of arming the probe against the rule rather than against one of its faults: a door
/// wired to a terminal-count check alone would satisfy the case above and leave this
/// one exactly as it was. Every operand is in the request — the facet is a wholesale
/// replace, so no later call completes a dangling edge, it replaces the whole set —
/// which is what makes the edge judgeable here under D-312's test.
#[tokio::test]
async fn a_phases_payload_whose_edge_names_no_phase_of_the_set_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let authored = Uuid::now_v7();
    let absent = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [{
                    "phase_id": authored,
                    "kind": "trial",
                    "ordinal": 0,
                    "converts_to_phase_id": absent,
                    "phase_duration_days": 14
                }]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PHASE_GRAPH_INVALID", "{body}");
    assert!(
        body.to_string().contains(&absent.to_string()),
        "the refusal names the target that resolves to nothing: {body}"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "nothing landed"
    );
}

/// A draft plan carrying one draft price row on [`rest_support::seeded_phase`],
/// with that phase attached.
///
/// Returns the revision's row version, because every case below continues the
/// version chain from here. The row is seeded **before** the phase is attached on
/// purpose: the attach is itself a `phases` write on a plan carrying a row, and it
/// is the write that *repairs* a stranding rather than causing one — which the
/// D-342 door must not refuse, or an author handed an orphaned row would have no
/// call left that fixes it.
async fn seed_plan_with_a_row_on_its_phase(harness: &Harness, plan_id: Uuid) -> u64 {
    seed_draft_plan(harness, plan_id).await;
    seed_price(harness, plan_id, "eu").await;

    let attached = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(phase_chain(rest_support::seeded_phase().get())),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(
        attached.status(),
        StatusCode::OK,
        "attaching the phase a stranded row names is the repair, not the fault"
    );
    1
}

/// One phases body over a chain that keeps `seeded_phase` and prepends a trial.
///
/// The positive control of D-342's door, and the only one of the three cases that
/// can detect an opt-in applied too widely: the two refusals below would pass
/// whether the door judged this facet or the whole pipeline.
fn trial_before_the_seeded_phase() -> serde_json::Value {
    serde_json::json!({
        "phases": [
            {
                "phase_id": Uuid::now_v7(),
                "kind": "trial",
                "ordinal": 0,
                "converts_to_phase_id": rest_support::seeded_phase().get(),
                "phase_duration_days": 14
            },
            {
                "phase_id": rest_support::seeded_phase().get(),
                "kind": "evergreen",
                "ordinal": 1
            }
        ]
    })
}

/// **A `phases` write that would strand a draft row is refused when it is made —
/// D-342.**
///
/// `RowPhaseAttached` reported through `violate`, which defaults to the Publish
/// stage, so this `PATCH` answered 200 and the author learned at publish. The delay
/// costs the remediation, which is the whole of the argument: at the write there are
/// two remedies — keep the phase, or drop the rows deliberately — while at publish a
/// published row cannot be re-pointed at all, a scope key being the row's identity,
/// and a draft row can only be deleted.
///
/// The facet is a wholesale **replace**, so the stranding is one successful call:
/// it deletes the revision's phase rows and re-inserts the payload rather than
/// merging it.
#[tokio::test]
async fn a_phases_write_dropping_a_phase_a_draft_row_names_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let version = seed_plan_with_a_row_on_its_phase(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            // A different id entirely: the row's phase is simply gone from the set.
            Some(phase_chain(Uuid::now_v7())),
            &[("if-match", &format!("\"0-{version}\""))],
        ))
        .await;

    // 400 for `require_well_formed_plan_name`'s reason, stated there: a
    // write-stage violation renders through `failed_precondition()`, the
    // architectural 422 this gear answers as 400.
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PHASE_ROW_ORPHANED", "{body}");
    assert!(
        body.to_string()
            .contains(&rest_support::seeded_phase().get().to_string()),
        "the refusal names the phase, which is the remedy the author acts on: {body}"
    );

    // Nothing landed. The refusal has to arrive before the delete half of the
    // replace, or an author would be told no and left with fewer phases.
    let read = body_json(
        harness
            .allowed()
            .send(request("GET", &plan_path(plan_id), None))
            .await,
    )
    .await;
    assert_eq!(
        read["phases"][0]["phase_id"],
        serde_json::json!(rest_support::seeded_phase().get()),
        "the phase the row names is still attached: {read}"
    );
    assert_eq!(plan_row_version(&harness, plan_id, 0).await, Some(version));
}

/// The precondition violations a 400 carries, **in the order the door reported
/// them**, as `(type, subject)` pairs.
///
/// A whole-document `contains(CODE)` cannot see order, cannot see a second
/// violation appearing beside the one under test, and is satisfied by the token
/// riding an echoed request field - so a case built on it stays green on an
/// envelope carrying the wrong set. Mirrors `rest_windows::precondition_violations`.
async fn precondition_violations(
    response: axum::http::Response<axum::body::Body>,
) -> Vec<(String, String)> {
    let body = body_json(response).await;
    body.get("context")
        .and_then(|c| c.get("violations"))
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("a 400 carries context.violations: {body}"))
        .iter()
        .map(|v| {
            (
                v.get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                v.get("subject")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

/// **The empty set is the same refusal — D-342.**
///
/// Its own case rather than a line in the one above, because it is the shape
/// easiest to *type* and option (c) of the decision was to refuse only this one.
/// That option was rejected for inverting the ordering by likelihood — the
/// replace-that-drops-one is the shape easiest to reach by accident — so both
/// cases have to stand or the rejected option is what shipped.
///
/// **The empty set is two faults since the 2026-08-17 review**, and the envelope
/// carries both: it strands the rows *and* leaves the revision with no terminal
/// phase (`inst-ph-graph`, which now runs at this door too). The stranding is asserted
/// as the **first** violation deliberately — it is the one an author acts on, and the
/// door's rule order is what puts it there.
#[tokio::test]
async fn an_empty_phases_write_on_a_plan_holding_rows_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let version = seed_plan_with_a_row_on_its_phase(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "phases": [] })),
            &[("if-match", &format!("\"0-{version}\""))],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let violations = precondition_violations(refused).await;
    let codes: Vec<&str> = violations.iter().map(|(code, _)| code.as_str()).collect();
    // Both remediations, and the stranding **first** - the claim in this case's
    // doc, which a `contains` over the rendered document could not measure.
    assert_eq!(
        codes,
        ["PHASE_ROW_ORPHANED", "PHASE_GRAPH_INVALID"],
        "an empty phase set strands the rows and leaves no terminal phase, and one \
         response carries both, the actionable one first: {violations:?}"
    );
    assert!(
        violations.iter().all(|(_, subject)| !subject.is_empty()),
        "each violation names what it is about: {violations:?}"
    );
    assert_eq!(plan_row_version(&harness, plan_id, 0).await, Some(version));
}

/// **The empty set is refused on a plan holding no rows at all — D-343, and this is
/// the arm where the refusal is wider than D-342's.**
///
/// D-342 scoped its cost to plans *already holding price rows*, so on a row-less
/// draft `PATCH {"phases": []}` answered **200** until `inst-ph-graph` joined this
/// door: `PhaseGraphIntegrity` reports zero terminals over an **empty** phase set,
/// and there is nothing for `RowPhaseAttached` to strand. The widening is intended
/// rather than incidental — after `inst-ph-default` every plan is created carrying a
/// terminal phase, so emptying the set puts the plan back in exactly the state this
/// program abolished, and no authoring step needs to pass through it. D-343 records
/// it because D-343 first declined to flag itself on the ground that D-342 had
/// settled the question, and D-342 was never asked about this class.
///
/// **The absence of `PHASE_ROW_ORPHANED` is half the claim.** It is the only thing
/// separating this arm from its neighbour above, which asserts both codes: a door
/// that reported the stranding here would be reporting it over a row set that is
/// empty, and a probe asserting only `PHASE_GRAPH_INVALID` would pass on either.
///
/// The plan is created through **`POST /plans`** and not `seed_draft_plan`, because
/// the seeded terminal phase is the route's act (`inst-ph-default` lives in the
/// handler, not in `create_draft`) — and it is the phase this write empties.
#[tokio::test]
async fn an_empty_phases_write_on_a_plan_holding_no_rows_is_refused_for_the_graph_alone() {
    let harness = Harness::new().await;

    let created = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(create_body("gold")),
            &keyed("empty-phases-no-rows"),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = body_json(created).await;
    let plan_id: Uuid = created_body["plan_id"]
        .as_str()
        .expect("the id is answered")
        .parse()
        .expect("the id is a uuid");
    assert_eq!(
        created_body["phases"].as_array().map(Vec::len),
        Some(1),
        "the premise: the create seeded the one phase this write removes: {created_body}"
    );

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "phases": [] })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let violations = precondition_violations(refused).await;
    let codes: Vec<&str> = violations.iter().map(|(code, _)| code.as_str()).collect();
    // Equality, not "contains one and not the other": the absence of
    // `PHASE_ROW_ORPHANED` is half the claim, and so is there being nothing else.
    assert_eq!(
        codes,
        ["PHASE_GRAPH_INVALID"],
        "there is no row to strand, and reporting one would send the author looking \
         for rows that do not exist: {violations:?}"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "nothing landed, so the plan still holds the phase it was created with"
    );
}

/// **The positive control: a replace that keeps every named phase still
/// succeeds.**
///
/// D-342 clause (3) — the opt-in is scoped to this facet and this rule, and this is
/// what makes the scoping checkable. A too-wide opt-in shows up **here** rather
/// than in the refusals, which would pass either way: turning the whole plan
/// pipeline on at this door would refuse this plan for having no frequency, no
/// descriptor set and a trial phase carrying no coverage, none of which is a fault
/// of the request.
///
/// Prepending a trial is the ordinary authoring act the facet exists for, and it is
/// green before D-342 as well as after.
#[tokio::test]
async fn a_phases_write_that_keeps_every_named_phase_still_succeeds() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let version = seed_plan_with_a_row_on_its_phase(&harness, plan_id).await;

    let accepted = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(trial_before_the_seeded_phase()),
            &[("if-match", &format!("\"0-{version}\""))],
        ))
        .await;

    assert_eq!(accepted.status(), StatusCode::OK);
    let body = body_json(accepted).await;
    assert_eq!(
        body["phases"].as_array().map(Vec::len),
        Some(2),
        "both phases landed, so this is not a 200 over a rejected write: {body}"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(version + 1)
    );
}

/// A **published** plan holding a published price row on
/// [`rest_support::seeded_phase`], with that phase attached and **no open draft**.
///
/// The arrangement all three of D-342's own cases are missing: each of them edits a
/// plan that already holds a draft, so the arm of `target_revision` that *opens a
/// successor* was never under the door. Returns the tag a `PATCH` has to present,
/// read off the route rather than computed, because the publish moves the
/// revision's version.
async fn seed_published_plan_with_a_row_on_its_phase(
    harness: &Harness,
    plan_id: Uuid,
) -> (String, u64) {
    seed_draft_plan(harness, plan_id).await;
    let price = seed_price(harness, plan_id, "eu").await;
    harness.attach_shape(plan_id, 0).await;
    harness.publish_price(plan_id, price.price_id).await;
    harness.publish(plan_id, 0).await;

    let version = plan_row_version(harness, plan_id, 0)
        .await
        .expect("the published revision stands somewhere");
    assert_eq!(
        plan_state(harness, plan_id, 0).await.as_deref(),
        Some("published"),
        "the successor arm is only reachable from a plan with no open draft"
    );
    assert_eq!(
        plan_row_version(harness, plan_id, 1).await,
        None,
        "the staging is the test: there must be no successor before the call"
    );
    (harness.plan_etag(plan_id).await, version)
}

/// **A refused `phases` write opens no successor — 2026-08-17 review, on D-342's
/// door.**
///
/// The door was placed *after* `target_revision`, which on a plan with no open
/// draft **writes**: it opens a successor revision and copies the phase set
/// forward. So a refused phases write on a published plan answered `400` and left
/// revision 1 sitting in `draft` behind it — an edit the author never made, on a
/// call they were told did not happen, and one that consumes the plan's single open
/// draft slot (`OPEN_DRAFT_REVISION_EXISTS`) until somebody abandons it.
///
/// The refusal itself was already right, so the status says nothing about this
/// defect: what has to be read back is **the plan's revisions**, which is why the
/// two assertions below are about revision 1 and not about the body. The door needs
/// no revision to judge — `price_repo::load_for_plan` is plan-scoped and not
/// filtered by revision, and the number appears only in the finding's subject label
/// — so it now runs before anything is opened.
#[tokio::test]
async fn a_refused_phases_write_on_a_published_plan_opens_no_successor() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (tag, version) = seed_published_plan_with_a_row_on_its_phase(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "phases": [] })),
            &[("if-match", &tag)],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(refused).await,
        "PHASE_ROW_ORPHANED",
        "the refusal was already correct; the side effect is the defect"
    );

    assert_eq!(
        plan_row_version(&harness, plan_id, 1).await,
        None,
        "a refused write must leave no successor revision behind"
    );
    assert_eq!(
        plan_state(&harness, plan_id, 0).await.as_deref(),
        Some("published"),
        "and the revision it was refused on stays exactly as it was"
    );
    assert_eq!(plan_row_version(&harness, plan_id, 0).await, Some(version));
}

/// **The positive control: a phases write on a published plan still opens the
/// successor it is supposed to.**
///
/// The other half of moving the door, and the half a refusal test cannot see: an
/// order that refused *before* resolving the revision would satisfy the case above
/// by never opening a successor at all — including for the writes that should open
/// one. Prepending a trial while keeping the phase the row names is that write.
#[tokio::test]
async fn an_accepted_phases_write_on_a_published_plan_opens_its_successor() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (tag, _) = seed_published_plan_with_a_row_on_its_phase(&harness, plan_id).await;

    let accepted = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(trial_before_the_seeded_phase()),
            &[("if-match", &tag)],
        ))
        .await;

    assert_eq!(accepted.status(), StatusCode::OK);
    let body = body_json(accepted).await;
    assert_eq!(
        body["revision"],
        serde_json::json!(1),
        "the patch landed on the successor this call opened: {body}"
    );
    assert_eq!(
        body["phases"].as_array().map(Vec::len),
        Some(2),
        "and the payload landed on it: {body}"
    );
    assert_eq!(
        plan_state(&harness, plan_id, 1).await.as_deref(),
        Some("draft"),
        "the successor exists and is the open draft"
    );
}

#[tokio::test]
async fn the_abandon_flip_is_audited_exactly_as_the_deletion_it_replaces_was() {
    // D-145 in as many words. The flip is a distinct action from a delete
    // because the row survives and its `(plan_id, revision)` name stays
    // consumed - an auditor reading `delete` here would read a permanent name as
    // reusable.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    harness.abandon_draft(plan_id, 0).await;

    let records = plan_records(&harness, plan_id).await;
    assert_eq!(
        records.last(),
        Some(&(
            "abandon".to_owned(),
            "plan_revision".to_owned(),
            format!("{plan_id}/0")
        )),
        "{records:?}"
    );
}

#[tokio::test]
async fn a_refused_patch_leaves_no_record_of_having_happened() {
    // The record is inside the mutation's own transaction (D-135), so a refusal
    // writes neither the edit nor a trace of one. A post-hoc writer would pass
    // this by accident; what it would fail is the converse, which
    // `tests/sqlite_plan_repo.rs` proves by making the append itself refuse.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let before = plan_records(&harness, plan_id).await.len();

    let stale = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "silver" } })),
            &[("if-match", "\"0-9\"")],
        ))
        .await;

    assert_eq!(stale.status(), StatusCode::CONFLICT);
    // This one *does* mint a code, so it gets the file's own habit rather than the
    // family-and-sentence fallback: several distinct refusals on this door are 409,
    // and a case reading the status alone would pass on any of them.
    assert_eq!(problem_code(stale).await, "STALE_VERSION");
    assert_eq!(
        plan_records(&harness, plan_id).await.len(),
        before,
        "a refused edit records nothing"
    );
}

/// One record's action and the pair `inst-au-complete` names first.
type StatePair = (String, Option<serde_json::Value>, Option<serde_json::Value>);

/// The records on one plan's segment with their before/after states, in seq
/// order.
///
/// A second reader beside `plan_records`, because the triple that one returns
/// left `before_state` and `after_state` **unasserted on every route** - and they
/// are the fields `inst-au-complete` names first. Blanking both to `None` in
/// either writer used to leave the whole suite green.
async fn plan_state_pairs(harness: &Harness, plan_id: Uuid) -> Vec<StatePair> {
    rest_support::audit_rows(harness)
        .await
        .into_iter()
        .filter(|row| row.chain_id == plan_id)
        .map(|row| (row.action, row.before_state, row.after_state))
        .collect()
}

#[tokio::test]
async fn a_patch_on_a_published_plan_audits_the_revision_it_minted() {
    // The revision-opening is a mutation transaction of its own, and an operator
    // call that lands on a published plan is **two** of them. Auditing only the
    // second left "who created plan/1?" unanswerable, though D-145 makes that
    // number permanent and every revision-scoped child table copies against it.
    //
    // Worse, the two can part company: the open commits and the facet write
    // fails, and the plan then holds a revision number nothing recorded the
    // minting of. Delete `open_revision`'s `audit_repo::append` and this fails.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;

    // The published revision stands at version 1: the publish flip advanced it.
    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "silver" } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let records = plan_records(&harness, plan_id).await;
    // The successor's identity, then the edit that landed on it.
    assert!(
        records.contains(&(
            "create".to_owned(),
            "plan_revision".to_owned(),
            format!("{plan_id}/1")
        )),
        "the minted revision is recorded: {records:?}"
    );
    assert!(
        records.contains(&(
            "update".to_owned(),
            "plan_revision".to_owned(),
            format!("{plan_id}/1")
        )),
        "and so is the edit: {records:?}"
    );
}

#[tokio::test]
async fn every_plan_record_carries_the_before_and_after_state_its_action_implies() {
    // `inst-au-complete`'s first field, on every plan route. A create has no
    // before-state - the revision did not exist, and the absence is the whole
    // difference between minting a name and editing one - and every other action
    // has both.
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "silver" } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    harness.abandon_draft(plan_id, 0).await;

    let pairs = plan_state_pairs(&harness, plan_id).await;
    assert_eq!(pairs.len(), 3, "create, update, abandon: {pairs:?}");

    let (action, before, after) = &pairs[0];
    assert_eq!(action, "create");
    assert!(before.is_none(), "a create has no before-state: {before:?}");
    assert_eq!(
        after.as_ref().and_then(|state| state.get("lifecycleState")),
        Some(&serde_json::json!("draft"))
    );
    assert_eq!(
        after.as_ref().and_then(|state| state.get("rowVersion")),
        Some(&serde_json::json!(0))
    );

    let (action, before, after) = &pairs[1];
    assert_eq!(action, "update");
    assert_eq!(
        before.as_ref().and_then(|state| state.get("rowVersion")),
        Some(&serde_json::json!(0)),
        "the version the caller's precondition matched"
    );
    assert_eq!(
        after.as_ref().and_then(|state| state.get("rowVersion")),
        Some(&serde_json::json!(1)),
        "and the one the swap advanced it to"
    );
    assert_eq!(
        before
            .as_ref()
            .and_then(|state| state.get("lifecycleState")),
        Some(&serde_json::json!("draft"))
    );

    let (action, before, after) = &pairs[2];
    assert_eq!(action, "abandon");
    assert_eq!(
        before
            .as_ref()
            .and_then(|state| state.get("lifecycleState")),
        Some(&serde_json::json!("draft")),
        "the flip's before-state is the draft it discarded"
    );
    assert_eq!(
        after.as_ref().and_then(|state| state.get("lifecycleState")),
        Some(&serde_json::json!("abandoned")),
        "and its after-state is the tombstone, which is what makes D-145's flip \
         distinguishable from a deletion in the record"
    );
}

// ---------------------------------------------------------------------------
// D-178 — one correlation per operator call
// ---------------------------------------------------------------------------

/// **Two records, one call, one correlation id.**
///
/// The successor arm of `PATCH /plans/{planId}` writes **two** audit records:
/// `open_revision`'s `create` for the revision it mints, and the facet write's
/// `update`. D-178 clause (2) is that a single operator call produces **one**
/// correlation across everything it writes, and it is the clause with teeth:
/// D-135 segments the chain per aggregate, so a call whose records land at two
/// positions - and, once the bulk plane lands, on two segments - has the
/// correlation as the only thing saying they were one act.
///
/// **Asserting "not NULL" here would prove nothing**, which is why the assertion
/// is equality between the two. A per-handler `Uuid::now_v7()` - the shape this
/// task replaced on the publish route, and the shape a later group would reach
/// for again - satisfies non-NULL on every record and fails this.
#[tokio::test]
async fn two_records_of_one_patch_carry_one_correlation_id() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_current_plan(&harness, plan_id).await;
    let before = audit_rows(&harness).await.len();

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = audit_rows(&harness).await;
    let written = &rows[before..];
    assert_eq!(
        written.len(),
        2,
        "the successor arm mints a revision and writes a facet: two records"
    );
    assert_eq!(
        written
            .iter()
            .map(|row| row.action.as_str())
            .collect::<Vec<_>>(),
        vec!["create", "update"],
        "and they are those two acts, in that order"
    );

    let first = written[0]
        .correlation_id
        .expect("the minted revision's record carries a correlation");
    let second = written[1]
        .correlation_id
        .expect("the facet write's record carries a correlation");
    assert_eq!(
        first, second,
        "one operator call, one correlation - a value minted per record would pass a \
         not-NULL assertion and fail this one"
    );
}

/// And two calls are two correlations.
///
/// The other half of "request-scoped". Without it a constant - or a value minted
/// once at gear boot - would satisfy the test above, and every record the gear
/// ever wrote would correlate to every other one.
#[tokio::test]
async fn two_patches_carry_two_correlation_ids() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let first_tag = {
        let response = harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &plan_path(plan_id),
                Some(serde_json::json!({ "shape": { "plan_tier": "platinum" } })),
                &[("if-match", "\"0-0\"")],
            ))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        etag_of(&response).expect("a tag")
    };
    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "plan_tier": "titanium" } })),
            &[("if-match", &first_tag)],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let rows = audit_rows(&harness).await;
    let updates: Vec<_> = rows
        .iter()
        .filter(|row| row.action == "update")
        .filter_map(|row| row.correlation_id)
        .collect();
    assert_eq!(updates.len(), 2, "two facet writes, two records");
    assert_ne!(updates[0], updates[1], "two calls are two acts");
}

/// One composite id, two plans, and **both filings are accepted.**
///
/// **Inverted rather than deleted, which is what its own doc asked for.** It
/// pinned a `500`: `PRIMARY KEY (composite_id, plan_revision)` carried no
/// `plan_id` and no `tenant_id`, and `composite_id` is **client-supplied** —
/// `CompositeMeterRequest` makes it `Option<Uuid>` and its own doc invites a
/// read-modify-write round trip, since a `GET` echoes the ids. So the flow that
/// doc recommends, read plan A's composites and paste them onto plan B at
/// revision `0`, collided on the primary key and answered a generic internal
/// fault. The pin ended *"if a later slice widens the key … this reddens and the
/// change gets written down (D-304)"*, and `pricing_composite_meter` is that widening.
///
/// The optional id was always the right call (D-298) and this was its unpaid cost.
/// It is paid: a composite id belongs to a **plan**, so two plans of one tenant
/// may hold the same one, and two tenants may hold it at the same revision number.
/// The half that stays refused is one revision holding it twice, which the
/// composite rules are written as though were true —
/// `sqlite_plan_repo::one_revision_still_may_not_hold_one_composite_id_twice` is
/// that direction.
///
/// The second filing is read back rather than only counted `200`: a widening that
/// let the row land and then lost it would answer `200` here too.
#[tokio::test]
async fn two_plans_of_one_tenant_may_share_a_composite_id() {
    let harness = Harness::new().await;
    let shared = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    seed_draft_plan(&harness, first).await;
    seed_draft_plan(&harness, second).await;

    let body = |id: Uuid| {
        serde_json::json!({
            "composites": [{
                "composite_id": id,
                "output_unit": "vm",
                "constituent_units": ["vcpu", "ram"],
                "formula": { "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }
            }]
        })
    };

    let accepted = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(first),
            Some(body(shared)),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(
        accepted.status(),
        StatusCode::OK,
        "the first filing must succeed, or the second proves nothing"
    );

    let second_filing = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(second),
            Some(body(shared)),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(
        second_filing.status(),
        StatusCode::OK,
        "a composite id belongs to a plan, not to a revision number"
    );

    let read = harness
        .allowed()
        .send(with_headers("GET", &plan_path(second), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(
        body_json(read).await["composites"][0]["composite_id"],
        shared.to_string(),
        "the second plan holds the shared id, so the write landed rather than being swallowed"
    );
}

/// Two composites of one body sharing an `output_unit` are refused **400**, by
/// the facet door rather than by the store.
///
/// **This test was the pin on the 500, and its own doc asked to be reddened**:
/// "pinned so that giving *this* one a code later reddens here (D-304)". Review
/// M8/M20/M50 measured the 500 on the deployed platform and the door landed on
/// 2026-08-20; this is the other side of that pin.
///
/// `uq_pricing_composite_meter_output` is `(tenant_id, plan_id, plan_revision,
/// output_unit)` and `inst-cm-output`'s "one output unit per revision" still
/// lives there — the index is unchanged and remains the guarantee. What changed
/// is that a plain typo in a facet body no longer has to reach it: the door
/// refuses the repeated unit first, so the caller is told which unit collided
/// instead of being told to retry later.
///
/// **It is `InvalidRequest` and not a code of its own**, deliberately.
/// `plan_rules`' `DECLARED` pairs every refusal code with its spelling in the
/// design set and `plan_rules_tests` refuses one the document does not carry, so
/// a code cannot be minted at a REST door — four were tried here and withdrawn
/// when that gate caught them. Giving this fault a normative code is still owed;
/// what is no longer owed is the 500.
#[tokio::test]
async fn two_composites_of_one_body_sharing_an_output_unit_are_refused() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let collided = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "composites": [
                    {
                        "output_unit": "vm",
                        "constituent_units": ["vcpu", "ram"],
                        "formula": { "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }
                    },
                    {
                        "output_unit": "vm",
                        "constituent_units": ["vcpu", "disk"],
                        "formula": { "op": "weighted_sum", "weights": { "vcpu": 1, "disk": 1 } }
                    }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        collided.status(),
        StatusCode::BAD_REQUEST,
        "the facet door refuses the repeated output unit before the store sees it, so this is \
         the caller's mistake and not an internal fault"
    );
    // The family and the guard's own sentence, not a bare status and a two-letter
    // token: `PATCH /plans/{planId}` has several independent producers of a 400
    // carrying no wire code, and `vm` is a substring of enough of this document
    // that its presence says nothing about which guard answered.
    refused_by(
        &body_json(collided).await,
        "invalid_argument",
        "two composites of this revision output `vm`",
    );
}

/// **The period floor/cap facet lands, and the read echoes what it stored**
/// (D-319).
///
/// The same argument the composites case above makes: before this facet the
/// repository method had no caller in `src/`, so every production path was a
/// copy of a set nothing could originate and both publish rules ran on a
/// permanently empty vector — D-254's class, which this slice has now landed
/// four times. What is under test is not that the store works, but that a
/// **client** can put a minimum into it.
///
/// The `GET` half is load-bearing for its own reason: the facet replaces the set
/// wholesale, so an author adding a second market has to read the first one back
/// to resubmit it. A read that omitted the member would make a two-market plan
/// unauthorable one market at a time.
#[tokio::test]
async fn a_period_floor_cap_facet_lands_and_the_read_echoes_what_it_stored() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "period_floor_caps": [
                    { "currency": "usd", "region": "us", "floor_minor": 50000, "cap_minor": 500_000 },
                    { "currency": "EUR", "region": "de", "floor_minor": 40000 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let patched = body_json(response).await;
    let stored = patched["period_floor_caps"]
        .as_array()
        .unwrap_or_else(|| panic!("the patch answers the revision it wrote: {patched}"));
    assert_eq!(stored.len(), 2, "{patched}");
    // In `(currency, region)` order, and the currency **normalized** — the
    // authored `usd` comes back `USD`, because the code is an ISO 4217 value and
    // not the string the client typed. A round trip that echoed the input would
    // leave two spellings of one market in the caller's hands.
    assert_eq!(
        stored[0],
        serde_json::json!({
            "currency": "EUR", "region": "de", "floor_minor": 40000, "cap_minor": null
        }),
        "{patched}"
    );
    assert_eq!(
        stored[1],
        serde_json::json!({
            "currency": "USD", "region": "us", "floor_minor": 50000, "cap_minor": 500_000
        }),
        "{patched}"
    );

    let read = body_json(
        harness
            .allowed()
            .send(request("GET", &plan_path(plan_id), None))
            .await,
    )
    .await;
    assert_eq!(
        read["period_floor_caps"], patched["period_floor_caps"],
        "a GET answers the set the PATCH stored: {read}"
    );
}

/// A `PATCH` naming the period floor/cap facet **and** another one is refused,
/// like every other pair (D-173).
///
/// Its own case rather than a line in the existing two-facet test, because the
/// arity count in `Facet::of` is hand-written: a sixth facet added to the enum
/// and forgotten in that sum makes a two-facet body look like a one-facet body,
/// and the existing case would not notice — it names two facets that are both
/// still counted.
///
/// **The partner is `addon_rules: []` and the assertion is the arity sentence**,
/// and both halves are the case. Sending `phases: []` and asserting `status == 400`
/// instead makes it vacuous: drop the `period_floor_caps` term from `Facet::of`'s sum — the
/// exact defect this case exists for — and `named == 1`, control falls into the
/// `phases` arm, `{"phases": []}` is independently refused 400
/// `PHASE_GRAPH_INVALID`, nothing lands, and **both assertions still pass**. An
/// empty add-on rule set is legal alone, as the sibling two-facet case uses it, so
/// the arity is the only thing wrong with this body — and the sentence is what says
/// the arity is what refused it.
#[tokio::test]
async fn a_patch_naming_the_period_facet_and_another_is_refused() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "period_floor_caps": [
                    { "currency": "USD", "region": "us", "floor_minor": 50000 }
                ],
                "addon_rules": []
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "carries exactly one of",
    );

    let read = body_json(
        harness
            .allowed()
            .send(request("GET", &plan_path(plan_id), None))
            .await,
    )
    .await;
    assert_eq!(
        read["period_floor_caps"],
        serde_json::json!([]),
        "neither facet may have landed: {read}"
    );
}

// ---------------------------------------------------------------------------
// The write doors the 2026-08-20 review wave found missing (H3, H4, H8, H12,
// H13). Each case below is a payload whose whole remedy is one field and which
// reached the caller as a `500 … please retry later`, because the table's own
// `CHECK` was the first thing to read the field.
// ---------------------------------------------------------------------------

/// The grant set and the change contract a **create** accepts are the ones it
/// stores (H3).
///
/// `POST /plans` declares [`PlanShapeRequest`] as its body, which carries both
/// members, digests the whole body into the idempotency hash, and answered `201`
/// for a plan holding neither: `shape_of` mapped eleven of thirteen members and
/// `create_draft_on` wrote `EntitlementGrants::default()` over the top. Neither
/// member was on [`PlanView`] either, so the loss was invisible in the response
/// and this case could only be made against the column.
///
/// **Both halves are asserted now.** [`PlanView`] carries the pair from
/// 2026-08-20, so the body assertions are the ones a client can make and the
/// store assertions are the control: a view that echoed the request without ever
/// writing it would satisfy the first set and fail the second.
///
/// The `PATCH` half is the same assertion on the same plan, and it is the control:
/// the two verbs share one request type, so a door that stored nothing on either
/// would be caught by the create half alone, and a door that stored on neither
/// verb would look identical to today's defect from the outside.
#[tokio::test]
async fn a_create_carrying_entitlement_grants_and_a_change_contract_stores_them() {
    let harness = Harness::new().await;
    let target = Uuid::now_v7();

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(serde_json::json!({
                "plan_tier": "gold",
                "billing_cycle": "recurring",
                "entitlement_grants": {
                    "plan_tier_ref": "tier-gold",
                    "feature_flags": { "sso": true },
                    "quotas": { "seats": 50 }
                },
                "change_contract": {
                    "allowed_change_targets": [target],
                    "comparability_rank": 20,
                    "usage_counter_on_plan_change": "carry"
                }
            })),
            &keyed("create-with-grants"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    // Revision 0 at version **0**: the two members ride the create's own INSERT,
    // so the plan is not born carrying an edit nobody made — the property
    // `a_created_plan_carries_one_terminal_evergreen_phase` pins for the phase
    // seed, asserted here for the columns.
    assert_eq!(body["row_version"], serde_json::json!(0), "{body}");
    let plan_id = Uuid::parse_str(body["plan_id"].as_str().expect("the id is answered"))
        .expect("the id is a uuid");

    // **The 201 body shows both members back.** `PlanView` carried neither until
    // 2026-08-20, so a create that accepted them and dropped them was invisible on
    // the wire and this case could only read the column. It reads both now, and the
    // body assertion is the one a client can make.
    assert_eq!(
        body["entitlement_grants"]["plan_tier_ref"],
        serde_json::json!("tier-gold"),
        "{body}"
    );
    assert_eq!(
        body["entitlement_grants"]["feature_flags"]["sso"],
        serde_json::json!(true),
        "{body}"
    );
    assert_eq!(
        body["entitlement_grants"]["quotas"]["seats"],
        serde_json::json!(50),
        "{body}"
    );
    assert_eq!(
        body["change_contract"]["allowed_change_targets"],
        serde_json::json!([target.to_string()]),
        "{body}"
    );
    assert_eq!(
        body["change_contract"]["comparability_rank"],
        serde_json::json!(20),
        "{body}"
    );
    assert_eq!(
        body["change_contract"]["usage_counter_on_plan_change"],
        serde_json::json!("carry"),
        "{body}"
    );

    let stored = stored_revision(&harness, plan_id, 0).await;
    assert_eq!(
        stored.entitlement_grants.plan_tier_ref.as_deref(),
        Some("tier-gold"),
        "the create stores the tier the grants resolved from"
    );
    assert_eq!(
        stored
            .entitlement_grants
            .plan_level
            .feature_flags
            .get("sso"),
        Some(&true),
        "the create stores the flags it accepted"
    );
    assert_eq!(
        stored.entitlement_grants.plan_level.quotas.get("seats"),
        Some(&50),
        "the create stores the quotas it accepted"
    );
    assert_eq!(
        stored.change_contract.allowed_change_targets,
        Some(vec![target]),
        "the create stores the change contract it accepted"
    );
    assert_eq!(stored.change_contract.comparability_rank, Some(20));

    // The mirror image, on the verb that already honoured both members: one
    // request type, two consumers, one answer.
    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": {
                    "entitlement_grants": { "quotas": { "seats": 90 } }
                }
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::OK);
    assert_eq!(
        stored_revision(&harness, plan_id, 0)
            .await
            .entitlement_grants
            .plan_level
            .quotas
            .get("seats"),
        Some(&90),
        "the patch replaces the set the create stored"
    );

    // **And the authoring read answers what the patch left.** The `entitlement_grants`
    // member replaces the set wholesale, so an author who cannot read it back cannot
    // resubmit the parts they did not mean to drop — `PlanView::composites`' argument,
    // and here the patch above is the demonstration: it named `quotas` alone and the
    // read has to show that the tier reference and the flags went with them.
    let read = harness
        .allowed()
        .send(request("GET", &plan_path(plan_id), None))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let read = body_json(read).await;
    assert_eq!(
        read["entitlement_grants"]["quotas"]["seats"],
        serde_json::json!(90),
        "{read}"
    );
    assert!(
        read["entitlement_grants"]["plan_tier_ref"].is_null(),
        "a wholesale replace dropped the tier reference, and the read must say so: {read}"
    );
    assert_eq!(
        read["entitlement_grants"]["feature_flags"],
        serde_json::json!({}),
        "{read}"
    );
    // The change contract is a different facet member, so the shape patch above left
    // it standing - and the read is where an author confirms that.
    assert_eq!(
        read["change_contract"]["allowed_change_targets"],
        serde_json::json!([target.to_string()]),
        "{read}"
    );
    assert_eq!(
        read["change_contract"]["usage_counter_on_plan_change"],
        serde_json::json!("carry"),
        "{read}"
    );
}

/// The revision the store holds, read past the views.
///
/// Here rather than in `rest_support` because it is the only case that needs it.
/// `PlanView` renders both members since 2026-08-20, so this is no longer the only
/// way to see them — it is the **control** for the wire assertions beside it, and
/// what it proves is that the body was rendered from a column and not echoed back
/// from the request that set it.
async fn stored_revision(
    harness: &Harness,
    plan_id: Uuid,
    revision: u64,
) -> bss_pricing::domain::plan::PlanRevision {
    harness
        .state
        .plans
        .find_revision(
            &toolkit_db::secure::AccessScope::allow_all(),
            harness.tenant,
            bss_pricing::domain::scope_key::PlanId::new(plan_id),
            revision,
        )
        .await
        .expect("read the revision")
        .expect("the revision exists")
}

/// A period floor of `0` is refused by the write, not by the database (H4/H13).
///
/// `chk_pricing_plan_period_floor_cap_floor_positive` is `floor_minor IS NULL OR
/// floor_minor > 0`, and `MinorAmount::new(0)` is `Ok` — zero is a legal amount
/// for a free trial phase and a zero-rated usage row — so the bound reached the
/// insert and the caller was answered a 500 carrying a constraint name. All four
/// arms of `inst-pfc-amount` were dead at publish for the same reason: the
/// `CHECK`s guarantee no stored row can ever be the subject the rule judges.
#[tokio::test]
async fn a_period_floor_cap_with_a_zero_floor_is_refused_by_the_write_not_the_database() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "period_floor_caps": [
                    { "currency": "USD", "region": "EU", "floor_minor": 0 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        refused.status(),
        StatusCode::BAD_REQUEST,
        "a caller-fixable bound is not a storage failure"
    );
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PERIOD_FLOOR_CAP_AMOUNT_INVALID", "{body}");
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "a refused child write must not move the revision"
    );
}

/// A floor above its cap, refused at the same door (H4/H13).
///
/// `chk_pricing_plan_period_floor_cap_ordered`. Its own case rather than a second
/// payload in the one above, because a door wired to the zero test alone would
/// satisfy that one and leave this exactly as it was — the rule has four arms and
/// each is a separate reachability claim.
#[tokio::test]
async fn a_period_floor_cap_whose_floor_is_above_its_cap_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "period_floor_caps": [
                    { "currency": "USD", "region": "us", "floor_minor": 900, "cap_minor": 100 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PERIOD_FLOOR_CAP_AMOUNT_INVALID", "{body}");
    // The violation's own description, bound to the subject the write door leaves
    // on it. Three-digit needles over the whole document do not tell `900` from
    // `9000`, and neither numeral says which bound it is — which is the half an
    // operator acts on. The subject is the candidate rather than a resource
    // because `without_placeholder_plan` strips the nil-plan prefix this rule
    // stamps and this rule appends nothing after it.
    let violation = violation_for(&body, "(the candidate shape in this request)")
        .unwrap_or_else(|| panic!("the refusal carries a violation for the candidate: {body}"));
    assert!(
        violation.contains("the period floor on (USD, us) is 900 and its cap is 100"),
        "the refusal names both bounds and which is which, because either one may be the one \
         to move: {violation}"
    );
    assert_eq!(plan_row_version(&harness, plan_id, 0).await, Some(0));
}

/// A bound authoring neither a floor nor a cap, refused at the same door
/// (H4/H13).
///
/// `chk_pricing_plan_period_floor_cap_present`. Both members are `Option<i64>` on
/// the wire, so an entry naming a market and nothing else parses.
#[tokio::test]
async fn a_period_floor_cap_authoring_neither_bound_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "period_floor_caps": [{ "currency": "EUR", "region": "de" }]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PERIOD_FLOOR_CAP_AMOUNT_INVALID", "{body}");
    assert_eq!(plan_row_version(&harness, plan_id, 0).await, Some(0));
}

/// A trial phase whose `displayTrialDays` drifts from its `phaseDurationDays` is
/// refused at the write (H8).
///
/// `chk_pricing_plan_phase_display_trial_days` is `display_trial_days IS NULL OR
/// display_trial_days = phase_duration_days`, and `inst-ph-trial` reported only
/// the two faults that `CHECK` **cannot** see — a non-trial kind, and a trial with
/// no duration to project, which NULL propagation leaves satisfied. The one it can
/// see was the one nothing refused before the insert, so the author was told to
/// retry later about two numbers they had typed themselves.
///
/// Both numbers are named, because either is the one to move.
#[tokio::test]
async fn a_trial_phase_whose_display_days_drift_from_its_duration_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let trial = Uuid::now_v7();
    let terminal = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    {
                        "phase_id": trial,
                        "kind": "trial",
                        "ordinal": 0,
                        "converts_to_phase_id": terminal,
                        "phase_duration_days": 14,
                        "display_trial_days": 30
                    },
                    { "phase_id": terminal, "kind": "evergreen", "ordinal": 1 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        refused.status(),
        StatusCode::BAD_REQUEST,
        "a caller-fixable payload fault is not a storage failure"
    );
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "DISPLAY_TRIAL_DAYS_INVALID", "{body}");
    // Bound to the phase, and to the field each number was authored under. `14`
    // and `30` are two-character needles over a document carrying two
    // client-minted UUIDs rendered as hex, so an unrelated pair satisfies them —
    // and neither tells `14` from `140`.
    // Bound to the phase: `inst-ph-trial` stamps `{shape}|{phase_id}`, and the
    // phase pass runs over the patched shape's real identity, so
    // `without_placeholder_plan` finds no nil prefix to strip. The facet rules
    // one screen up run over a nil-plan candidate and keep the other spelling.
    let violation = violation_for(&body, &format!("{plan_id}/0|{trial}"))
        .unwrap_or_else(|| panic!("the refusal carries a violation for the trial phase: {body}"));
    assert!(
        violation.contains("carries displayTrialDays 30 and phaseDurationDays 14"),
        "the refusal names both numbers under the names they were authored by: {violation}"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "nothing landed"
    );
}

/// The positive control: a trial phase projecting the duration it has still
/// lands.
///
/// Without it the door could refuse every `displayTrialDays` and the case above
/// would still pass.
#[tokio::test]
async fn a_trial_phase_projecting_the_duration_it_has_still_lands() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let trial = Uuid::now_v7();
    let terminal = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    {
                        "phase_id": trial,
                        "kind": "trial",
                        "ordinal": 0,
                        "converts_to_phase_id": terminal,
                        "phase_duration_days": 14,
                        "display_trial_days": 14
                    },
                    { "phase_id": terminal, "kind": "evergreen", "ordinal": 1 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    let body = body_json(landed).await;
    assert_eq!(body["phases"].as_array().map(Vec::len), Some(2), "{body}");
}

/// A purchase window admitting no quantity is refused at the create (H12).
///
/// `chk_pricing_plan_purchase_qty` refuses `purchase_min_qty > purchase_max_qty`
/// at the column, so `PURCHASE_QTY_RANGE_INVALID` was a **declared code no path
/// could raise**: the rule that owns it reports at publish over a shape loaded
/// back from storage, and the `CHECK` guarantees the row it judges cannot be
/// stored. The create answered `500 … please retry later` for a payload whose
/// whole remedy is one field.
#[tokio::test]
async fn a_create_whose_purchase_window_admits_no_quantity_is_refused_at_the_write() {
    let harness = Harness::new().await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            PLANS,
            Some(serde_json::json!({
                "plan_tier": "gold",
                "billing_cycle": "one_time",
                "purchase_min_qty": 5,
                "purchase_max_qty": 2
            })),
            &keyed("create-inverted-qty"),
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PURCHASE_QTY_RANGE_INVALID", "{body}");
    assert_eq!(
        plan_count(&harness).await,
        0,
        "the refusal is ahead of the insert, so no revision may exist"
    );
}

/// The same window, submitted as a `PATCH` (H12).
///
/// The two verbs share `shape_of`/`shape_patch` and the door is the one call both
/// make, so this is the case that proves the door is on the shape and not on the
/// create.
#[tokio::test]
async fn a_patch_whose_purchase_window_admits_no_quantity_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": { "purchase_min_qty": 9, "purchase_max_qty": 3 }
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PURCHASE_QTY_RANGE_INVALID", "{body}");
    assert_eq!(plan_row_version(&harness, plan_id, 0).await, Some(0));
}

/// A window that admits a quantity still lands — the positive control for both
/// cases above.
#[tokio::test]
async fn a_purchase_window_that_admits_a_quantity_lands() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": { "purchase_min_qty": 3, "purchase_max_qty": 3 }
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(1),
        "the equal-bound window is the narrowest one that admits a quantity"
    );
}

/// An availability window that ends before it starts is refused at the write
/// (H12).
///
/// `chk_pricing_plan_availability` is `available_to > available_from`, and nothing
/// judged the ordering: `AvailabilityInsideCoverage` reads `available_to` against a
/// key's window coverage, never against `available_from`. So the pair reached the
/// `UPDATE` and came back a 500.
///
/// It carries no rule code, and that is the shape of the refusal rather than an
/// omission: no code in the design set names this fault, and the surface's own
/// precedent for a pairing the store would refuse is `frequency_of`'s — an
/// `invalid_argument` naming both fields, minted by nobody.
#[tokio::test]
async fn a_patch_whose_availability_window_ends_before_it_starts_is_refused_at_the_write() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": {
                    "available_from": "2026-09-01T00:00:00.000000Z",
                    "available_to": "2026-08-01T00:00:00.000000Z"
                }
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let family = problem_family(refused).await;
    assert_eq!(family, "invalid_argument");
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "nothing landed"
    );
}

/// **The equality case, which is the one the `CHECK` is strict about.**
///
/// `require_ordered_availability_window` is `if to > from { Ok }` and its own doc
/// calls equality a deliberate refusal, but no case submitted the pair equal: the
/// positive control is a month apart and the refusal above is a strict inversion.
/// Relax the guard to `>=` and both stay green while an instantaneous window
/// reaches `chk_pricing_plan_availability` and comes back the 500 the guard exists
/// to remove — the boundary hole `an_end_restated_at_the_floor_is_accepted` closed
/// for the trailing floor.
#[tokio::test]
async fn a_patch_whose_availability_window_opens_and_closes_at_one_instant_is_refused() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": {
                    "available_from": "2026-09-01T00:00:00.000000Z",
                    "available_to": "2026-09-01T00:00:00.000000Z"
                }
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_family(refused).await, "invalid_argument");
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "nothing landed"
    );
}

/// **One bound at a time is the same fault** (2026-08-20 review, H12's residue).
///
/// Each rule's operand is a *pair*, and `PlanShapePatch` moves whichever members the
/// caller named — so `{"purchase_min_qty": 5}` on a draft already holding a
/// `purchase_max_qty` of 2 is the case above split across two calls. The two doors
/// that landed with H12 read the submitted payload only, so this one reached
/// `chk_pricing_plan_purchase_qty` and answered `500 … please retry later` while its
/// both-bounds-in-one-payload twin answered a named 400.
///
/// The refusal has to be the **same** one, code included: a caller who authored a
/// window in two calls has the identical fault and the identical remedy as one who
/// authored it in one.
#[tokio::test]
async fn a_patch_sending_one_purchase_bound_against_the_stored_other_is_refused_by_its_code() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let ceiling = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_max_qty": 2 } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(
        ceiling.status(),
        StatusCode::OK,
        "a ceiling with no floor is a legitimate draft, and the fixture depends on it"
    );

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_min_qty": 5 } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(code_in(&body), "PURCHASE_QTY_RANGE_INVALID", "{body}");
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(1),
        "the refused patch left the draft's tag where the first one put it"
    );
}

/// The availability window's half of the same residue: `{"available_to"}` alone
/// against a stored `available_from` that is later.
///
/// It carries no rule code — no code in the design set names this fault, and its
/// sibling case above says why minting one here would be wrong — so the family is
/// the discriminator.
#[tokio::test]
async fn a_patch_sending_one_availability_bound_against_the_stored_other_is_refused() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let opened = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": { "available_from": "2026-09-01T00:00:00.000000Z" }
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(opened.status(), StatusCode::OK);

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": { "available_to": "2026-08-01T00:00:00.000000Z" }
            })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_family(refused).await, "invalid_argument");
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(1),
        "nothing landed"
    );
}

/// **The positive control for the merge**, and the half a refusal test cannot see: a
/// single bound that merges into a window the store accepts must still land.
///
/// A door that read the stored row and refused *any* partial pair would satisfy both
/// cases above and break every author who fills a window in two calls, which is the
/// ordinary way a plan is assembled.
#[tokio::test]
async fn a_patch_sending_one_bound_that_merges_into_an_admissible_window_lands() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let ceiling = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_max_qty": 9 } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(ceiling.status(), StatusCode::OK);

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_min_qty": 2 } })),
            &[("if-match", "\"0-1\"")],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    let body = body_json(landed).await;
    assert_eq!(body["purchase_min_qty"], serde_json::json!(2), "{body}");
    assert_eq!(
        body["purchase_max_qty"],
        serde_json::json!(9),
        "and the stored ceiling is still there — the patch moved one member: {body}"
    );
}

/// **A refused partial window opens no successor**, which is the whole reason the
/// door reads the revision the `If-Match` names instead of the one
/// `target_revision` resolves.
///
/// On a plan with no open draft `target_revision` *writes*: it opens a successor and
/// copies the current revision's content — the stored `purchase_max_qty` included —
/// forward. A door behind it would answer the right 400 and leave revision 1 sitting
/// in `draft` on a call the author was told did not happen, consuming the plan's one
/// open-draft slot. That is D-342's defect, and
/// `a_refused_phases_write_on_a_published_plan_opens_no_successor` is the same
/// property for the phase facet.
///
/// The status is therefore not what this case is about: what has to be read back is
/// the plan's revisions.
#[tokio::test]
async fn a_refused_partial_purchase_window_on_a_published_plan_opens_no_successor() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let ceiling = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_max_qty": 2 } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(ceiling.status(), StatusCode::OK);
    harness.publish(plan_id, 0).await;
    let published_version = plan_row_version(&harness, plan_id, 0)
        .await
        .expect("the published revision stands somewhere");
    assert_eq!(
        plan_row_version(&harness, plan_id, 1).await,
        None,
        "the staging is the test: there must be no successor before the call"
    );

    let refused = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_min_qty": 5 } })),
            &[("if-match", &harness.plan_etag(plan_id).await)],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    assert_eq!(
        code_in(&body),
        "PURCHASE_QTY_RANGE_INVALID",
        "the successor would have carried the stored ceiling forward, so the merged pair \
         is the same fault on this arm: {body}"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 1).await,
        None,
        "a refused write must leave no successor revision behind"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(published_version),
        "and the revision it was refused on stays exactly as it was"
    );
}

/// The positive control for the arm above: a partial window that merges admissibly
/// on a **published** plan still opens the successor it is supposed to.
///
/// Without it, a door that refused every shape `PATCH` on the successor arm — or one
/// placed so early that nothing is ever opened — would satisfy the case above.
#[tokio::test]
async fn an_admissible_partial_window_on_a_published_plan_opens_its_successor() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let ceiling = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_max_qty": 9 } })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(ceiling.status(), StatusCode::OK);
    harness.publish(plan_id, 0).await;

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({ "shape": { "purchase_min_qty": 2 } })),
            &[("if-match", &harness.plan_etag(plan_id).await)],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    let body = body_json(landed).await;
    assert_eq!(
        body["revision"],
        serde_json::json!(1),
        "the patch landed on the successor this call opened: {body}"
    );
    assert_eq!(body["purchase_min_qty"], serde_json::json!(2), "{body}");
    assert_eq!(
        body["purchase_max_qty"],
        serde_json::json!(9),
        "carried forward from the predecessor: {body}"
    );
}

/// The same two instants in the order the store accepts still land — the positive
/// control, and the equality case is the one the `CHECK` is strict about.
#[tokio::test]
async fn an_availability_window_that_opens_before_it_closes_lands() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "shape": {
                    "available_from": "2026-08-01T00:00:00.000000Z",
                    "available_to": "2026-09-01T00:00:00.000000Z"
                }
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    let body = body_json(landed).await;
    assert_eq!(
        body["available_to"],
        serde_json::json!("2026-09-01T00:00:00.000000Z"),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// The duplicate-key doors on the three wholesale-replace facets
// (review M8/M50/M51). Each key below is a PRIMARY KEY or a UNIQUE index on the
// table its facet replaces, so before these doors landed on 2026-08-20 the
// payload reached the store and the caller was answered an untyped 500.
// ---------------------------------------------------------------------------

/// **Two rules for one add-on SKU are refused 400, and the refusal names the
/// SKU** (review M8).
///
/// `pricing_plan_addon_rule` is keyed `(plan_id, plan_revision, addon_sku_id)`
/// and `replace_addon_rules` inserts the set row by row, so the second rule for
/// one SKU violated the primary key and came back as `RepoError::Db` -- "An
/// internal error occurred. Please retry later." -- for a payload the caller can
/// fix by deleting one of two lines.
///
/// The bounds on both rules are authorable, deliberately: `AddonQtyRange` runs
/// first, and a fixture that tripped it would prove nothing about the door under
/// test. The two rules disagree about `max_qty` so that the payload is not merely
/// redundant but genuinely ambiguous -- which is the sentence the refusal makes.
///
/// **It is `InvalidRequest` and not a code of its own**, as its two siblings
/// below are: `plan_rules`' `DECLARED` pairs every refusal code with its spelling
/// in the design set and `plan_rules_tests` refuses one the document does not
/// carry, so a code cannot be minted at a REST door. A normative code is still
/// owed; what is no longer owed is the 500.
#[tokio::test]
async fn two_rules_for_one_addon_sku_are_refused_and_name_it() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let sku = Uuid::now_v7();

    let collided = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "addon_rules": [
                    {
                        "addon_sku_id": sku,
                        "required": false,
                        "min_qty": 1,
                        "max_qty": 5,
                        "step_qty": serde_json::Value::Null,
                        "price_override_ref": serde_json::Value::Null,
                        "depends_on": [],
                        "conflicts_with": []
                    },
                    {
                        "addon_sku_id": sku,
                        "required": false,
                        "min_qty": 1,
                        "max_qty": 9,
                        "step_qty": serde_json::Value::Null,
                        "price_override_ref": serde_json::Value::Null,
                        "depends_on": [],
                        "conflicts_with": []
                    }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        collided.status(),
        StatusCode::BAD_REQUEST,
        "the facet door refuses the repeated SKU before the store sees it, so this is the \
         caller's mistake and not an internal fault"
    );
    let body = body_json(collided).await;
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(&sku.to_string()),
        "and it names the add-on that was named twice, which is the whole distance from a \
         500: {detail}"
    );

    // The door runs at the parse, before `target_revision`, so no successor
    // revision was opened on a call the author was told did not happen.
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "the refused facet wrote nothing"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 1).await,
        None,
        "and it opened no successor revision either"
    );
}

/// The positive control: two rules for **two** SKUs, otherwise identical, land.
///
/// Without it the case above could not tell "the duplicate was refused" from "a
/// two-rule set is refused", and the door would be free to reject every add-on
/// set with more than one member.
#[tokio::test]
async fn two_rules_for_two_addon_skus_land_together() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "addon_rules": [
                    {
                        "addon_sku_id": Uuid::now_v7(),
                        "required": false,
                        "min_qty": 1,
                        "max_qty": 5,
                        "step_qty": serde_json::Value::Null,
                        "price_override_ref": serde_json::Value::Null,
                        "depends_on": [],
                        "conflicts_with": []
                    },
                    {
                        "addon_sku_id": Uuid::now_v7(),
                        "required": false,
                        "min_qty": 1,
                        "max_qty": 9,
                        "step_qty": serde_json::Value::Null,
                        "price_override_ref": serde_json::Value::Null,
                        "depends_on": [],
                        "conflicts_with": []
                    }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    let body = body_json(landed).await;
    assert_eq!(
        body["addon_rules"].as_array().map(Vec::len),
        Some(2),
        "both rules are stored, so the door judged the key and not the cardinality: {body}"
    );
}

/// **A composite that names no output unit is refused 400** (review M51).
///
/// The sibling of the repeated-unit case above, and a different mechanism on each
/// side of the fix. `chk_pricing_composite_meter_output_unit` refuses both widths, so
/// each of them reaches the caller as a 500 carrying a constraint name when the door
/// does not answer first — a store refusal rendered as an internal error, over a
/// request that is plainly invalid. The door trims, so both are one 400.
///
/// Both are asserted because reverting the door leaves them failing differently
/// -- the empty one 500s, the blank one succeeds -- and a case that only carried
/// the empty string would say nothing about the second.
#[tokio::test]
async fn a_composite_with_no_output_unit_is_refused() {
    let harness = Harness::new().await;

    for unauthorable in ["", "   "] {
        let plan_id = Uuid::now_v7();
        seed_draft_plan(&harness, plan_id).await;

        let refused = harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &plan_path(plan_id),
                Some(serde_json::json!({
                    "composites": [{
                        "output_unit": unauthorable,
                        "constituent_units": ["vcpu", "ram"],
                        "formula": { "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }
                    }]
                })),
                &[("if-match", "\"0-0\"")],
            ))
            .await;

        assert_eq!(
            refused.status(),
            StatusCode::BAD_REQUEST,
            "a composite must name the unit it outputs; {unauthorable:?} names nothing a meter \
             can be read under"
        );
        // The blankness guard's own sentence. Without it the whitespace case would
        // pass against a door that had stopped trimming and let the store's
        // `chk_pricing_composite_meter_output_unit` answer instead - a 500 rendered
        // 400 by nothing, and the defect `composite_of` records for this very field.
        refused_by(
            &body_json(refused).await,
            "invalid_argument",
            "a composite must name the unit it outputs",
        );
        assert_eq!(
            plan_row_version(&harness, plan_id, 0).await,
            Some(0),
            "and the refused facet wrote nothing"
        );
    }
}

/// The positive control for the case above: one composite naming a unit lands.
///
/// It is the same body with the unit filled in, so the pair says the door judges
/// the `outputUnit` and not the composite facet.
#[tokio::test]
async fn a_composite_naming_its_output_unit_lands() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "composites": [{
                    "output_unit": "vm",
                    "constituent_units": ["vcpu", "ram"],
                    "formula": { "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }
                }]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    let body = body_json(landed).await;
    assert_eq!(
        body["composites"][0]["output_unit"],
        serde_json::json!("vm"),
        "{body}"
    );
}

/// **One market carrying two period floor/caps is refused 400, and the refusal
/// names the market** (review M50).
///
/// `pricing_plan_period_floor_cap` is keyed `(plan_id, plan_revision, currency,
/// region)`, so the second bound on one market violated the primary key and the
/// operator was told to retry later about a payload no retry would fix.
///
/// The two spellings are the case, not an incidental detail: `period_floor_cap_of`
/// normalizes the currency to ISO 4217, so `usd` and `USD` are **one** market by
/// the time the key is built and a door that de-duplicated the wire strings would
/// let this straight through to the index it exists to keep away from.
#[tokio::test]
async fn one_market_carrying_two_period_bounds_is_refused_and_named() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let collided = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "period_floor_caps": [
                    { "currency": "usd", "region": "us", "floor_minor": 50000 },
                    { "currency": "USD", "region": "us", "floor_minor": 70000 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        collided.status(),
        StatusCode::BAD_REQUEST,
        "two bounds on one market do not say which floor applies, and the door says so before \
         the primary key does"
    );
    let body = body_json(collided).await;
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("USD/us"),
        "and it names the market that collided, in the normalized spelling the key is built \
         from -- not the two the caller typed: {detail}"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 0).await,
        Some(0),
        "the refused facet wrote nothing"
    );
    assert_eq!(
        plan_row_version(&harness, plan_id, 1).await,
        None,
        "and opened no successor revision"
    );
}

/// The positive control: two bounds on two markets land together.
///
/// The one-market pair above differs from this one in a single character, which
/// is what makes the two cases a measurement of the door rather than of the
/// facet.
#[tokio::test]
async fn two_period_bounds_on_two_markets_land_together() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let landed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "period_floor_caps": [
                    { "currency": "usd", "region": "us", "floor_minor": 50000 },
                    { "currency": "USD", "region": "ca", "floor_minor": 70000 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(landed.status(), StatusCode::OK);
    let body = body_json(landed).await;
    assert_eq!(
        body["period_floor_caps"].as_array().map(Vec::len),
        Some(2),
        "one currency in two regions is two markets: {body}"
    );
}

// ---------------------------------------------------------------------------
// GET /plans — AM-style OData (D-125 amendment).
// ---------------------------------------------------------------------------

fn plan_ids(body: &serde_json::Value) -> Vec<String> {
    body["items"]
        .as_array()
        .expect("plans ride `items`")
        .iter()
        .map(|row| {
            row["plan_id"]
                .as_str()
                .expect("each summary names plan_id")
                .to_owned()
        })
        .collect()
}

/// Named `lifecycle_state=` is retired. The page is narrowed only through `$filter`.
#[tokio::test]
async fn a_named_lifecycle_state_key_on_the_plan_list_is_refused() {
    let harness = Harness::new().await;
    seed_draft_plan(&harness, Uuid::now_v7()).await;

    let named = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{PLANS}?lifecycle_state=draft"),
            None,
        ))
        .await;
    assert_eq!(named.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(named).await,
        "invalid_argument",
        "unrecognized query parameter `lifecycle_state`",
    );

    let extra = harness
        .allowed()
        .send(request("GET", &format!("{PLANS}?status=draft"), None))
        .await;
    assert_eq!(extra.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(extra).await,
        "invalid_argument",
        "unrecognized query parameter `status`",
    );
}

/// Omit `lifecycle_state` → one authoring revision per plan (draft if any, else current).
#[tokio::test]
async fn omitting_lifecycle_on_the_plan_list_keeps_the_authoring_default() {
    let harness = Harness::new().await;
    let draft = Uuid::now_v7();
    let published = Uuid::now_v7();
    seed_draft_plan(&harness, draft).await;
    seed_current_plan(&harness, published).await;

    let page = body_json(harness.allowed().send(request("GET", PLANS, None)).await).await;
    let ids = plan_ids(&page);
    assert!(
        ids.contains(&draft.to_string()) && ids.contains(&published.to_string()),
        "a listing over current revisions alone would hide the open draft: {page}"
    );

    let only_draft = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!("{PLANS}?$filter=lifecycle_state%20eq%20'draft'"),
                None,
            ))
            .await,
    )
    .await;
    assert_eq!(
        plan_ids(&only_draft),
        vec![draft.to_string()],
        "$filter=lifecycle_state eq 'draft' must drop the published plan: {only_draft}"
    );

    let only_published = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!("{PLANS}?$filter=lifecycle_state%20eq%20'published'"),
                None,
            ))
            .await,
    )
    .await;
    assert_eq!(
        plan_ids(&only_published),
        vec![published.to_string()],
        "$filter=lifecycle_state eq 'published' must drop the draft: {only_published}"
    );
}

/// `$top` is `limit`. A bounded page names a cursor the next request resumes from
/// without repeating `$orderby` (the extractor forbids the pair).
#[tokio::test]
async fn the_plan_list_walks_with_top_and_a_cursor() {
    let harness = Harness::new().await;
    let mut authored = Vec::new();
    for _ in 0..3 {
        let plan_id = Uuid::now_v7();
        seed_draft_plan(&harness, plan_id).await;
        authored.push(plan_id.to_string());
    }

    let first = body_json(
        harness
            .allowed()
            .send(request("GET", &format!("{PLANS}?$top=2"), None))
            .await,
    )
    .await;
    let page_one = plan_ids(&first);
    assert_eq!(page_one.len(), 2, "$top=2 bounds the page: {first}");
    assert_eq!(first["page_info"]["limit"], serde_json::json!(2));
    let token = first["page_info"]["next_cursor"]
        .as_str()
        .expect("a page with more behind it names where to resume")
        .to_owned();

    let second = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!("{PLANS}?limit=2&cursor={token}"),
                None,
            ))
            .await,
    )
    .await;
    let page_two = plan_ids(&second);
    assert!(
        page_one.iter().all(|id| !page_two.contains(id)),
        "the walk must not repeat a row: {page_one:?} ∩ {page_two:?}"
    );
    let mut seen = page_one;
    seen.extend(page_two);
    seen.sort();
    authored.sort();
    assert_eq!(
        seen, authored,
        "every seeded plan appears exactly once across the two pages"
    );
}

/// A cursor is valid only for the same `$filter`. Reusing it under another is 400.
#[tokio::test]
async fn a_plan_list_cursor_is_invalid_under_a_different_filter() {
    let harness = Harness::new().await;
    seed_draft_plan(&harness, Uuid::now_v7()).await;
    seed_draft_plan(&harness, Uuid::now_v7()).await;

    let first = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!("{PLANS}?$filter=lifecycle_state%20eq%20'draft'&limit=1"),
                None,
            ))
            .await,
    )
    .await;
    let token = first["page_info"]["next_cursor"]
        .as_str()
        .expect("two drafts and limit=1 leave a cursor")
        .to_owned();

    let reused = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{PLANS}?$filter=lifecycle_state%20eq%20'published'&cursor={token}"),
            None,
        ))
        .await;
    assert_eq!(
        reused.status(),
        StatusCode::BAD_REQUEST,
        "a cursor walked under one $filter must not continue under another: {}",
        body_json(reused).await
    );
}

/// The old timestamp spelling is not a silent alias for a changed time semantic.
#[tokio::test]
async fn the_plan_list_refuses_the_retired_timestamp_sort_key() {
    let harness = Harness::new().await;
    let response = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{PLANS}?$orderby=created_at_utc%20desc"),
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "created_at_utc",
    );
}

// ---------------------------------------------------------------------------
// D-359: the plan reads report the unit a revision waits on.
// ---------------------------------------------------------------------------

/// The submitter of the units below. A named principal rather than the default
/// client's, because `independent_approver` refuses a self-decision and the
/// withdraw case needs the submitter specifically.
const SUBMITTER: Uuid = Uuid::from_u128(0x5_c0);

/// Open a unit over a publishable plan through the publish route, and hand back
/// its id — `rest_approvals::a_pending_unit`'s arrangement, kept here so this
/// suite does not depend on that file's helpers.
async fn a_unit_over(h: &Harness, plan_id: Uuid, etag: &str) -> Uuid {
    let submitted = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
            None,
            &[("if-match", etag)],
        ))
        .await;
    assert_eq!(
        submitted.status(),
        StatusCode::ACCEPTED,
        "the submit must open a unit for anything below to be about the read"
    );
    let rows = approval_rows(h).await;
    assert_eq!(rows.len(), 1, "one unit, so the id below is unambiguous");
    rows[0].approval_id
}

/// D-359: a revision waiting on a unit reads that unit; before the submit it
/// reads `[]`; and the plan's `ETag` does not move when the unit opens.
///
/// The tag assertion is the load-bearing one. A pending unit is not plan
/// content, so a tag that tracked it would stale every author's `If-Match` for
/// a change they did not make — and the field is only safe to add to this body
/// because `plan_tag` digests the revision and its `row_version` alone.
#[tokio::test]
async fn the_plan_read_reports_its_pending_unit_and_its_tag_does_not_move() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&harness, plan_id).await;

    let before = harness
        .allowed_as(SUBMITTER)
        .send(request("GET", &plan_path(plan_id), None))
        .await;
    let tag_before = etag_of(&before).expect("the read carries a tag");
    assert_eq!(
        body_json(before).await["pending_approvals"],
        json!([]),
        "nothing is pending before the submit"
    );

    let unit = a_unit_over(&harness, plan_id, &seeded.etag()).await;

    let after = harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "GET",
            &plan_path(plan_id),
            None,
            &[("if-none-match", &tag_before)],
        ))
        .await;
    assert_eq!(
        after.status(),
        StatusCode::OK,
        "pending changes must not return 304"
    );
    assert_eq!(after.headers()["cache-control"], "private, no-store");
    assert_eq!(
        etag_of(&after).expect("tag"),
        tag_before,
        "a pending unit is not plan content, so the tag must not move"
    );
    let body = body_json(after).await;
    assert_eq!(
        body["lifecycle_state"], "draft",
        "no plan state was added: the revision itself stays draft"
    );
    assert_eq!(
        body["pending_approvals"][0]["approval_id"],
        unit.to_string(),
        "{body}"
    );
    assert_eq!(
        body["pending_approvals"]
            .as_array()
            .expect("pending units")
            .len(),
        1
    );
    assert!(body.get("pending_approval").is_none());
    assert_eq!(
        body["pending_approvals"][0]["subject_kind"],
        "plan_revision"
    );
    assert!(
        body["pending_approvals"][0]["submitted_at"].is_string(),
        "and when it was opened: {body}"
    );
    assert!(
        body["pending_approvals"][0]
            .get("submitter_principal")
            .is_none(),
        "who submitted it is `approval x read`'s fact, not `plan x read`'s: {body}"
    );
    assert!(
        body["pending_approvals"][0].get("materiality").is_none(),
        "and so is why it was material: {body}"
    );
}

/// D-359: the field follows the unit rather than a state of its own — a withdraw
/// clears it with nothing written to the plan.
#[tokio::test]
async fn a_withdrawn_unit_clears_the_pending_field() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&harness, plan_id).await;
    let unit = a_unit_over(&harness, plan_id, &seeded.etag()).await;
    let before = harness
        .allowed_as(SUBMITTER)
        .send(request("GET", &plan_path(plan_id), None))
        .await;
    let tag = etag_of(&before).expect("tag");

    let withdrawn = harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{unit}/withdraw"),
            None,
            &[],
        ))
        .await;
    assert_eq!(withdrawn.status(), StatusCode::OK);

    let after = harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "GET",
            &plan_path(plan_id),
            None,
            &[("if-none-match", &tag)],
        ))
        .await;
    assert_eq!(after.status(), StatusCode::OK);
    assert_eq!(after.headers()["cache-control"], "private, no-store");
    assert_eq!(etag_of(&after), Some(tag));
    let body = body_json(after).await;
    assert_eq!(body["pending_approvals"], json!([]), "{body}");
    assert_eq!(
        body["lifecycle_state"], "draft",
        "the revision was draft throughout"
    );
}

/// D-359: the list marks exactly the plans a `plan_revision` unit is open over.
///
/// Two plans in one page, because a flag that answered the same thing for every
/// row would satisfy an assertion on either one alone.
#[tokio::test]
async fn the_plan_list_marks_the_plans_in_review() {
    let harness = Harness::new().await;
    let reviewed = Uuid::now_v7();
    let idle = Uuid::now_v7();
    let seeded = seed_publishable_plan(&harness, reviewed).await;
    seed_publishable_plan(&harness, idle).await;
    let unit = a_unit_over(&harness, reviewed, &seeded.etag()).await;

    let response = harness
        .allowed_as(SUBMITTER)
        .send(request("GET", "/bss-pricing/v1/plans", None))
        .await;
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let page = body_json(response).await;
    let by_id: std::collections::HashMap<String, &serde_json::Value> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|plan| (plan["plan_id"].as_str().expect("plan_id").to_owned(), plan))
        .collect();
    assert_eq!(
        by_id[&reviewed.to_string()]["pending_approvals"][0]["approval_id"],
        unit.to_string(),
        "the plan with an open unit: {page}"
    );
    assert_eq!(
        by_id[&idle.to_string()]["pending_approvals"],
        json!([]),
        "and not the one without: {page}"
    );
    assert!(by_id[&reviewed.to_string()].get("in_review").is_none());
    let detail = body_json(
        harness
            .allowed_as(SUBMITTER)
            .send(request("GET", &plan_path(reviewed), None))
            .await,
    )
    .await;
    assert_eq!(
        by_id[&reviewed.to_string()]["pending_approvals"],
        detail["pending_approvals"]
    );
}

// ---------------------------------------------------------------------------
// D-357 / D-358: the phase label, and the middle kind spelled `interim`.
// ---------------------------------------------------------------------------

/// A phase carries its operator label through the `phases` facet and back, and
/// the middle kind is spelled `interim` (D-357, D-358).
///
/// Both the write's response and a fresh read are asserted: the response says
/// what the parser kept, the read says what the store kept, and D-357 is only
/// landed when the two agree.
#[tokio::test]
async fn a_phase_label_round_trips_and_the_middle_kind_is_interim() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    // A bare draft carries no phase yet, so this facet write authors the whole
    // chain, terminal included.
    let steady = Uuid::now_v7();
    let onboarding = Uuid::now_v7();

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    {
                        "phase_id": onboarding,
                        "kind": "interim",
                        "display_name": "Onboarding",
                        "ordinal": 0,
                        "converts_to_phase_id": steady,
                        "phase_duration_days": 30
                    },
                    { "phase_id": steady, "kind": "evergreen", "ordinal": 1 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    let status = response.status();
    let written = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["phases"][0]["kind"], serde_json::json!("interim"));
    assert_eq!(
        written["phases"][0]["display_name"],
        serde_json::json!("Onboarding")
    );
    assert!(
        written["phases"][1]["display_name"].is_null(),
        "an unlabelled phase carries no label key at all: {written}"
    );

    let read = body_json(
        harness
            .allowed()
            .send(with_headers("GET", &plan_path(plan_id), None, &[]))
            .await,
    )
    .await;
    assert_eq!(
        read["phases"], written["phases"],
        "the store kept what the parser kept"
    );
}

/// `intro` is not accepted as a legacy spelling (D-358): the write refuses it,
/// names the field, and writes nothing.
#[tokio::test]
async fn the_old_intro_token_is_refused_by_name_and_writes_nothing() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let steady = Uuid::now_v7();

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    {
                        "phase_id": Uuid::now_v7(),
                        "kind": "intro",
                        "ordinal": 0,
                        "converts_to_phase_id": steady,
                        "phase_duration_days": 30
                    },
                    { "phase_id": steady, "kind": "evergreen", "ordinal": 1 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await.to_string();
    assert!(
        problem.contains("phases.kind")
            && problem.contains("`intro`")
            && problem.contains("interim"),
        "the refusal names the field, the token sent and the tokens accepted: {problem}"
    );

    let read = body_json(
        harness
            .allowed()
            .send(with_headers("GET", &plan_path(plan_id), None, &[]))
            .await,
    )
    .await;
    assert_eq!(
        read["phases"].as_array().map(Vec::len),
        Some(0),
        "a refused facet write leaves the bare draft as it was: {read}"
    );
}

/// A blank label is an absence wearing a value's shape: it reads back as no
/// label, so `""` and `"   "` never become a second spelling of "unlabelled".
#[tokio::test]
async fn a_blank_phase_label_reads_back_as_no_label() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    let steady = Uuid::now_v7();

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(plan_id),
            Some(serde_json::json!({
                "phases": [
                    { "phase_id": steady, "kind": "evergreen", "display_name": "   ", "ordinal": 0 }
                ]
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    let status = response.status();
    let written = body_json(response).await;
    assert_eq!(status, StatusCode::OK, "{written}");
    // `null`, not an absent key: the field renders like its three optional
    // siblings on this view, so a client reads one shape for a labelled and an
    // unlabelled phase. What is asserted here is that whitespace does not become
    // a label, which is unchanged.
    assert!(
        written["phases"][0]["display_name"].is_null(),
        "whitespace is not a label: {written}"
    );
}

// ---------------------------------------------------------------------------
// D-360: the list carries each plan's row facts; a counts read feeds the tabs.
// ---------------------------------------------------------------------------

/// Each list row carries `price_row_count`, `model_kinds` and `currencies` over
/// its `draft` + `published` rows, and the count **is** the prices list's own
/// unfiltered page length - the definition, not a coincidence. A plan with no
/// rows renders zero and two empty lists rather than omitting the fields.
#[tokio::test]
async fn the_list_carries_each_plans_row_facts_and_they_match_the_prices_list() {
    let harness = Harness::new().await;
    let seeded_id = Uuid::now_v7();
    seed_publishable_plan(&harness, seeded_id).await;
    let created = body_json(
        harness
            .allowed()
            .send(with_headers(
                "POST",
                PLANS,
                Some(create_body("gold")),
                &keyed("k-d360-empty"),
            ))
            .await,
    )
    .await;
    let empty_id: Uuid = created["plan_id"]
        .as_str()
        .expect("plan_id")
        .parse()
        .expect("uuid");

    let page = body_json(harness.allowed().send(request("GET", PLANS, None)).await).await;
    let by_id: HashMap<String, &serde_json::Value> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| (item["plan_id"].as_str().expect("plan_id").to_owned(), item))
        .collect();
    let seeded = by_id[&seeded_id.to_string()];
    assert_eq!(seeded["price_row_count"], 1, "{seeded}");
    assert_eq!(
        seeded["model_kinds"],
        serde_json::json!(["flat"]),
        "{seeded}"
    );
    assert_eq!(seeded["currencies"], serde_json::json!(["EUR"]), "{seeded}");
    let fresh = by_id[&empty_id.to_string()];
    assert_eq!(fresh["price_row_count"], 0, "{fresh}");
    assert_eq!(fresh["model_kinds"], serde_json::json!([]), "{fresh}");
    assert_eq!(fresh["currencies"], serde_json::json!([]), "{fresh}");

    let prices = body_json(
        harness
            .allowed()
            .send(request("GET", &format!("{PLANS}/{seeded_id}/prices"), None))
            .await,
    )
    .await;
    assert_eq!(
        prices["items"].as_array().map(Vec::len),
        Some(1),
        "the count is the prices list's unfiltered page length: {prices}"
    );
}

/// `GET /plans/counts` folds the list's authoring collapse over the whole
/// catalogue: a plan with an open draft is `draft`, else its current state -
/// so opening a successor draft over a published plan moves it from
/// `published` to `draft`, exactly as the list's row does.
#[tokio::test]
async fn the_counts_read_folds_the_authoring_state_over_the_whole_catalogue() {
    let harness = Harness::new().await;
    let (d1, d2, p1) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    seed_publishable_plan(&harness, d1).await;
    seed_publishable_plan(&harness, d2).await;
    let published = seed_publishable_plan(&harness, p1).await;
    harness.publish(p1, published.revision).await;

    let counts = body_json(
        harness
            .allowed()
            .send(request("GET", PLANS_COUNTS, None))
            .await,
    )
    .await;
    assert_eq!(
        counts,
        serde_json::json!({ "total": 3, "draft": 2, "published": 1, "retired": 0 }),
        "{counts}"
    );

    // **A second tenant reads zeros over the same three plans.**
    //
    // The route answers "over the whole catalogue", and the only proof that the
    // catalogue is the *caller's* was one tenant deep in `sqlite_plan_repo`, which
    // cannot see which tenant id the handler passes. A handler folding the counts
    // unscoped, or over the wrong tenant, answered `{3,2,1,0}` here and stayed
    // green — the sibling `/approvals/counts` carries this case for that reason.
    let foreign = body_json(
        harness
            .other_tenant()
            .send(request("GET", PLANS_COUNTS, None))
            .await,
    )
    .await;
    assert_eq!(
        foreign,
        serde_json::json!({ "total": 0, "draft": 0, "published": 0, "retired": 0 }),
        "another tenant's catalogue is empty, not this one: {foreign}"
    );

    // Open a successor draft over p1 through the surface: a PATCH on a published
    // plan opens the next revision (D-170).
    let read = harness
        .allowed()
        .send(with_headers("GET", &plan_path(p1), None, &[]))
        .await;
    let tag = etag_of(&read).expect("the published plan carries a tag");
    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &plan_path(p1),
            Some(serde_json::json!({ "shape": { "plan_tier": "silver" } })),
            &[("if-match", &tag)],
        ))
        .await;
    let status = patched.status();
    assert!(
        status.is_success(),
        "the successor opens: {status} {}",
        body_json(patched).await
    );

    let counts = body_json(
        harness
            .allowed()
            .send(request("GET", PLANS_COUNTS, None))
            .await,
    )
    .await;
    assert_eq!(
        counts,
        serde_json::json!({ "total": 3, "draft": 3, "published": 0, "retired": 0 }),
        "the plan with an open draft counts as draft: {counts}"
    );
}
