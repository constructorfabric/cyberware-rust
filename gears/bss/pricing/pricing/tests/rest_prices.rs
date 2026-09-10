//! The price plane, driven through the real router.
//!
//! Every refusal is asserted by its **code** rather than only its status, and
//! every denial by an **unchanged store** as well as a 403 — see
//! `tests/rest_plans.rs` for why both habits are load-bearing.
//!
//! The pagination cases are the ones that would otherwise pass vacuously: an
//! exact-multiple boundary, a full walk, and inserts on both sides of a live
//! cursor. A test that only asked for one short page would be green against a
//! `next_cursor` that never stopped.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::plans::PLANS;
use bss_pricing::domain::price_row::{MinQtyUsageFallback, ReservationFlavor};
use rest_support::{
    Harness, body_json, etag_of, location_of, price_rows, problem_code, refused_by, request,
    seed_draft_plan, seed_price, with_headers,
};
use uuid::Uuid;

fn prices_path(plan_id: Uuid) -> String {
    format!("{PLANS}/{plan_id}/prices")
}

fn price_path(plan_id: Uuid, price_id: Uuid) -> String {
    format!("{PLANS}/{plan_id}/prices/{price_id}")
}

/// A well-formed create body on a named region, so several can coexist.
fn create_body(region: &str) -> serde_json::Value {
    serde_json::json!({
        "scope_key": {
            "currency": "USD",
            "region": region,
            "phase": harness_phase(),
            "price_eligibility": "all_subscriptions",
            "charge_kind": "recurring",
            "cohort": serde_json::Value::Null
        },
        "content": {
            "model_kind": "flat",
            "amount_minor": 1_500,
            "tax_inclusive": false
        }
    })
}

fn harness_phase() -> String {
    rest_support::seeded_phase().get().to_string()
}

fn keyed(key: &str) -> Vec<(&str, &str)> {
    vec![("idempotency-key", key)]
}

async fn seeded_plan(harness: &Harness) -> Uuid {
    let plan_id = Uuid::now_v7();
    seed_draft_plan(harness, plan_id).await;
    plan_id
}

// ---------------------------------------------------------------------------
// Create.
// ---------------------------------------------------------------------------

/// A row authored on a region the tenant never declared is refused **at save**.
///
/// §2 step 2 and C2 both say the membership is validated *"at save **and**
/// publish"*, and only the publish half was built: an undeclared region was
/// accepted 201 here and refused much later, at publish, by someone who was
/// probably not the author. `inst-mc-region`'s save half is this.
///
/// The refusal is asserted by its **code**, because a 400 on this route is also
/// what a malformed body gets, and a case that could not tell the two apart
/// would stay green if the region check were replaced by any other validation.
#[tokio::test]
async fn a_row_on_an_undeclared_region_is_refused_at_save() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("ap-south")),
            &keyed("undeclared-region-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "REGION_UNKNOWN");
    assert!(
        price_rows(&harness, plan_id).await.is_empty(),
        "a refused save writes no row"
    );
}

/// The declared regions still author cleanly, so the check is not refusing
/// everything.
#[tokio::test]
async fn a_row_on_a_declared_region_still_authors() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("declared-region-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
}

/// A **retired** region cannot take a new row either.
///
/// The universe is the `active` set — `overlay_repo::declares`' predicate one
/// plane over — so a value that reached `retired` must not validate a new row
/// against itself. Without this the retire guard would be a door that closes
/// only from one side.
#[tokio::test]
async fn a_row_on_a_retired_region_is_refused_at_save() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    common::retire_fixture_region(&harness.db, harness.tenant, "EU").await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("retired-region-1"),
        ))
        .await;

    assert_eq!(problem_code(response).await, "REGION_UNKNOWN");
}

#[tokio::test]
async fn a_create_answers_201_with_the_location_and_the_rows_own_tag() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("price-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = location_of(&response);
    let tag = etag_of(&response);
    let body = body_json(response).await;
    let price_id = body["price_id"].as_str().expect("the id is answered");
    assert_eq!(
        location,
        Some(format!("{}/{price_id}", prices_path(plan_id)))
    );
    assert_eq!(tag, Some("\"0\"".to_owned()));
    assert_eq!(
        body["scope_key"]["charge_kind"],
        serde_json::json!("recurring"),
        "the row's charge_kind is the KEY's, so the response echoes what was stored: {body}"
    );
}

/// Create's `Location` is a real GET of the row, and that GET's `ETag` is the
/// token the next `PATCH` must present.
#[tokio::test]
async fn create_location_is_a_get_of_the_row() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let created = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("price-get"),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let location = location_of(&created).expect("create sets Location");
    let price_id = body_json(created).await["price_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("id");

    let got = harness
        .allowed()
        .send(request("GET", &location, None))
        .await;
    assert_eq!(got.status(), StatusCode::OK);
    let etag = etag_of(&got).expect("GET emits ETag");
    let body = body_json(got).await;
    assert_eq!(body["price_id"], price_id.to_string());
    assert_eq!(body["lifecycle_state"], "draft");
    assert_eq!(body["row_version"], 0);
    assert_eq!(etag, "\"0\"");

    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, price_id),
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 99 } })),
            &[("if-match", etag.as_str())],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::OK);
}

/// An unknown price reads like an absent one — no existence leak.
#[tokio::test]
async fn get_unknown_price_is_404() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let response = harness
        .allowed()
        .send(request("GET", &price_path(plan_id, Uuid::now_v7()), None))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A matching `If-None-Match` is 304 with no body — GET plan's rule.
#[tokio::test]
async fn get_if_none_match_matching_tag_is_304() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;
    let first = harness
        .allowed()
        .send(request("GET", &price_path(plan_id, seeded.price_id), None))
        .await;
    let etag = etag_of(&first).expect("ETag");
    let again = harness
        .allowed()
        .send(with_headers(
            "GET",
            &price_path(plan_id, seeded.price_id),
            None,
            &[("if-none-match", etag.as_str())],
        ))
        .await;
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        body_json(again).await,
        serde_json::Value::Null,
        "RFC 9110 forbids a 304 body"
    );
}

#[tokio::test]
async fn a_replayed_create_answers_the_original_price_id() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("price-2"),
        ))
        .await;
    let first_id = body_json(first).await["price_id"].clone();

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("price-2"),
        ))
        .await;

    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(body_json(replay).await["price_id"], first_id);
    assert_eq!(price_rows(&harness, plan_id).await.len(), 1);
}

/// A replay is answered from the dedup record, not re-judged against a world
/// that has moved under it.
///
/// The region is retired between the first call and the retry. Judged ahead of
/// the gate, `require_declared_region` answers `REGION_UNKNOWN` for a row that
/// is already filed — the caller is told their write never happened while the
/// recorded `201` sits unread in the dedup cell — which is the inversion the
/// mandatory `Idempotency-Key` exists to prevent.
/// `customer_groups::create_membership` carries the same rule and keeps its
/// guard inside the closure for exactly this reason.
///
/// The retirement is what arms the case. A retry under an unchanged world is
/// already covered by `a_replayed_create_answers_the_original_price_id`, and it
/// passes whichever side of the gate the check sits on — so it could never see
/// this. The paired control is
/// `a_row_on_a_retired_region_is_refused_at_save`: a **fresh** row on the same
/// retired region still has to be refused, or this would be satisfied by
/// deleting the check.
#[tokio::test]
async fn a_replay_is_answered_after_its_region_is_retired() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("price-region-retired-between"),
        ))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_id = body_json(first).await["price_id"].clone();

    common::retire_fixture_region(&harness.db, harness.tenant, "EU").await;

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("price-region-retired-between"),
        ))
        .await;

    assert_eq!(
        replay.status(),
        StatusCode::CREATED,
        "the replay answers the recorded 201, not a fresh judgement of the world"
    );
    assert_eq!(body_json(replay).await["price_id"], first_id);
    assert_eq!(
        price_rows(&harness, plan_id).await.len(),
        1,
        "and the replay files nothing of its own"
    );
}

/// **The other refusal the create's gate can raise**, and this route's module doc
/// promises both.
///
/// The handler digests the body (`preconditions::request_digest`) and hands it to
/// `idempotent::guarded` as `request_hash`, so a spent key over a changed body is
/// a `409`. Nothing here drove that: only the same-payload replay above, which the
/// digest is not needed to answer. Six sibling suites each carry this case.
#[tokio::test]
async fn a_spent_key_replayed_over_a_different_body_is_refused_by_its_code() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("price-3"),
        ))
        .await;

    let mismatched = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("US")),
            &keyed("price-3"),
        ))
        .await;

    assert_eq!(mismatched.status(), StatusCode::CONFLICT);
    assert_eq!(
        problem_code(mismatched).await,
        "IDEMPOTENCY_PAYLOAD_MISMATCH"
    );
    assert_eq!(
        price_rows(&harness, plan_id).await.len(),
        1,
        "the refused replay wrote no second row"
    );
}

#[tokio::test]
async fn a_second_draft_on_one_canonical_key_is_refused_by_its_code() {
    // D-148 put the duplicate-key refusal on the draft plane too: the published
    // partial UNIQUE cannot see a second draft, which is the ambiguity publish
    // would fail on - discovered a round trip earlier.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("dup-1"),
        ))
        .await;

    let clash = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("dup-2"),
        ))
        .await;

    assert_eq!(clash.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(clash).await, "DUPLICATE_SCOPE_KEY");
    assert_eq!(price_rows(&harness, plan_id).await.len(), 1);
}

// ---------------------------------------------------------------------------
// The two Slice-10 primitives, refused rather than stored.
// ---------------------------------------------------------------------------

/// A create body whose content carries one extra member.
fn create_body_with(region: &str, key: &str, value: serde_json::Value) -> serde_json::Value {
    let mut body = create_body(region);
    body["content"][key] = value;
    body
}

/// Assert a refusal is the 400 the Foundation validation envelope renders, that
/// it names the field, and that no row was stored.
///
/// The status matters twice over: a 422 would contradict §3.3 (no path in this
/// gear produces one) and a 500 would say the gear tried and broke rather than
/// that it does not support the field.
async fn assert_refused_naming(response: axum::response::Response<axum::body::Body>, field: &str) {
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an unsupported primitive is a malformed request, not a 422 and not a fault"
    );
    let body = body_json(response).await;
    let rendered = body.to_string();
    assert!(
        rendered.contains(field),
        "the refusal must name the field the caller has to remove: {rendered}"
    );
    // The envelope's **own `status` member**, not a substring of the rendered
    // body. `rendered.contains("422")` also matches the hex of every id the
    // envelope carries, and `instance` carries two UUIDv7s whose random tails
    // put a literal `422` in roughly one body in fifty. CI drew one on
    // 2026-08-19 — `.../prices/01a019e1-6e6c-7c11-af97-f35d642285a6` — and the
    // red said nothing about any status. The status line above already proves
    // the response is a 400; this proves the envelope agrees with it.
    assert_eq!(
        body["status"].as_u64(),
        Some(400),
        "no path in this gear produces a 422, and the envelope must carry the \
         status the response line does: {rendered}"
    );
}

#[tokio::test]
async fn a_create_carrying_a_tier_qualification_window_is_refused_at_any_value() {
    // `inst-tt-forbidden` refuses an EXPLICIT window of any value - `current`
    // included - because an accepted-but-ignored value masks authoring errors.
    // A check that only caught `trailing_period` would store the default spelled
    // out, and nothing in the crate judges either.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    for (index, window) in ["trailing_period", "current"].into_iter().enumerate() {
        let response = harness
            .allowed()
            .send(with_headers(
                "POST",
                &prices_path(plan_id),
                Some(create_body_with(
                    "EU",
                    "tier_qualification_window",
                    serde_json::json!(window),
                )),
                &keyed(&format!("tqw-{index}")),
            ))
            .await;

        assert_refused_naming(response, "tier_qualification_window").await;
    }
    assert!(
        price_rows(&harness, plan_id).await.is_empty(),
        "neither refusal may have stored a row"
    );
}

/// A create body on a **usage** key: the shape `includedAllowance` is authorable
/// on since D-45's compile landed.
fn usage_create_body(region: &str, meter: &str) -> serde_json::Value {
    serde_json::json!({
        "scope_key": {
            "currency": "USD",
            "region": region,
            "phase": harness_phase(),
            "price_eligibility": "all_subscriptions",
            "charge_kind": "usage",
            "cohort": serde_json::Value::Null
        },
        "content": {
            "model_kind": "per_unit",
            "unit_rate_nano_minor": 700_000_000_i64,
            "tax_inclusive": false,
            "meter": meter,
            "billing_granularity": "per_hour"
        }
    })
}

#[tokio::test]
async fn a_usage_row_carrying_a_none_allowance_is_authored_and_echoed() {
    // **The positive control for the whole slice.** Every refusal below would
    // pass identically against the field-wide refusal this change removed, so
    // the case that proves the removal is the one that stores a declaration.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let mut body = usage_create_body("EU", "egress.gb");
    body["content"]["included_allowance"] =
        serde_json::json!({ "quantity": 100, "rollover_policy": "none" });

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(body),
            &keyed("allow-none"),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "rolloverPolicy = none has a rule that judges it and a compile that honours it"
    );
    let echoed = body_json(response).await;
    assert_eq!(
        echoed["content"]["included_allowance"]["quantity"],
        serde_json::json!(100),
        "{echoed}"
    );
    assert_eq!(
        echoed["content"]["included_allowance"]["rollover_policy"],
        serde_json::json!("none"),
        "{echoed}"
    );
    assert_eq!(price_rows(&harness, plan_id).await.len(), 1);
}

#[tokio::test]
async fn an_allowance_on_a_non_usage_key_is_refused_at_the_write() {
    // D-312's category: the declaration and a frozen non-usage `chargeKind` are
    // both in the request, so `inst-ac-gate` refuses at the write rather than
    // storing a row only publish could reject.
    //
    // Asserted by **code**, because a 400 on this route is also what a malformed
    // body gets and a case that could not tell them apart would stay green if
    // the gate were replaced by any other validation.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body_with(
                "EU",
                "included_allowance",
                serde_json::json!({ "quantity": 100, "rollover_policy": "none" }),
            )),
            &keyed("allow-recurring"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "ALLOWANCE_ON_NON_USAGE");
    assert!(
        price_rows(&harness, plan_id).await.is_empty(),
        "the refusal may not have stored a row"
    );
}

#[tokio::test]
async fn a_create_carrying_a_carried_allowance_is_refused_on_a_key_that_would_otherwise_take_it() {
    // The **key** is the one an allowance is authorable on, so nothing but the
    // rollover policy is at fault: `inst-ac-carry` materializes a promotional
    // grant into `pricing_plan_grant`, and this gear has no such table, so an
    // accepted declaration would freeze a carry horizon nothing ever issues.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let mut body = usage_create_body("EU", "egress.gb");
    body["content"]["included_allowance"] =
        serde_json::json!({ "quantity": 100, "rollover_policy": "carry" });

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(body),
            &keyed("allow-carry"),
        ))
        .await;

    assert_refused_naming(response, "included_allowance").await;
    assert!(
        price_rows(&harness, plan_id).await.is_empty(),
        "the refusal may not have stored a row"
    );
}

#[tokio::test]
async fn a_patch_cannot_slip_either_primitive_past_the_create_check() {
    // The `PATCH` is a WHOLE-content submission, so a caller who creates a clean
    // row and then patches it would otherwise reach exactly the state the create
    // refuses. Both refusals run in `content_of`, which both verbs call.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    for (field, value) in [
        (
            "tier_qualification_window",
            serde_json::json!("trailing_period"),
        ),
        (
            "included_allowance",
            serde_json::json!({ "quantity": 100, "rollover_policy": "carry" }),
        ),
    ] {
        let response = harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &price_path(plan_id, seeded.price_id),
                Some(serde_json::json!({
                    "content": {
                        "model_kind": "flat",
                        "amount_minor": 99,
                        field: value
                    }
                })),
                &[("if-match", "\"0\"")],
            ))
            .await;

        assert_refused_naming(response, field).await;
    }
    let after = price_rows(&harness, plan_id).await;
    assert_eq!(after[0].row_version.get(), 0, "no refusal moved the row");
}

#[tokio::test]
async fn a_row_carrying_neither_primitive_creates_and_patches_exactly_as_before() {
    // The refusal is conditioned on a NON-NULL value, so an explicit `null` is
    // the same request as an absent member. A refusal that fired on presence
    // rather than on a value would break every client that serializes its whole
    // content type.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let created = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body_with(
                "EU",
                "tier_qualification_window",
                serde_json::Value::Null,
            )),
            &keyed("null-window"),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let price_id = body_json(created).await["price_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("the id is answered");

    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, price_id),
            Some(serde_json::json!({
                "content": {
                    "model_kind": "flat",
                    "amount_minor": 42,
                    "included_allowance": serde_json::Value::Null
                }
            })),
            &[("if-match", "\"0\"")],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::OK);
    let after = price_rows(&harness, plan_id).await;
    assert_eq!(after[0].row_version.get(), 1, "the edit landed");
}

// ---------------------------------------------------------------------------
// Patch.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_patch_under_a_stale_tag_is_refused_and_the_row_does_not_move() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, seeded.price_id),
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 99 } })),
            &[("if-match", "\"7\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "STALE_VERSION");
    let after = price_rows(&harness, plan_id).await;
    assert_eq!(after[0].row_version.get(), 0, "the tag stayed where it was");
}

#[tokio::test]
async fn a_patch_may_not_move_the_canonical_scope_key() {
    // `update_draft` cannot move a key, so a body naming a different one is a
    // refusal rather than a silent no-op: a key decides which duplicate a row
    // is, which chain it joins and which window covers it.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, seeded.price_id),
            Some(serde_json::json!({
                "scope_key": {
                    "currency": "USD",
                    "region": "US",
                    "phase": harness_phase(),
                    "price_eligibility": "all_subscriptions",
                    "charge_kind": "recurring",
                    "cohort": serde_json::Value::Null
                },
                "content": { "model_kind": "flat", "amount_minor": 99 }
            })),
            &[("if-match", "\"0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The guard's own sentence: `PATCH` on this row answers a code-less 400 for
    // several independent faults, and the scope-key immutability rule is the one
    // under test.
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "the canonical scope key is immutable",
    );
    let after = price_rows(&harness, plan_id).await;
    assert_eq!(after[0].scope_key.region().as_str(), "EU");
    assert_eq!(after[0].row_version.get(), 0);
}

#[tokio::test]
async fn a_price_under_the_wrong_plans_url_is_not_found() {
    // A path that names a parent it does not check lets a caller mutate a row
    // through the wrong plan's URL - and makes the authz `resource_id` argument
    // a fiction.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let other_plan = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(other_plan, seeded.price_id),
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 99 } })),
            &[("if-match", "\"0\"")],
        ))
        .await;
    let deleted = harness
        .allowed()
        .send(with_headers(
            "DELETE",
            &price_path(other_plan, seeded.price_id),
            None,
            &[("if-match", "\"0\"")],
        ))
        .await;
    let got = harness
        .allowed()
        .send(request(
            "GET",
            &price_path(other_plan, seeded.price_id),
            None,
        ))
        .await;

    assert_eq!(patched.status(), StatusCode::NOT_FOUND);
    assert_eq!(deleted.status(), StatusCode::NOT_FOUND);
    assert_eq!(got.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        price_rows(&harness, plan_id).await.len(),
        1,
        "the row survived both"
    );
}

// ---------------------------------------------------------------------------
// Delete.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_delete_without_a_precondition_is_refused_and_the_row_survives() {
    // The D-141 defect itself: this verb's idempotency cell used to be empty, so
    // a draft row could be destroyed under an unknown version. What a blind
    // delete destroys is a concurrent editor's uncommitted work.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    let response = harness
        .allowed()
        .send(request(
            "DELETE",
            &price_path(plan_id, seeded.price_id),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(response).await,
        "invalid_argument",
        "If-Match is required on this verb: a mutation of a draft row",
    );
    assert_eq!(
        price_rows(&harness, plan_id).await.len(),
        1,
        "an unconditional delete must not have taken effect"
    );
}

#[tokio::test]
async fn a_delete_under_the_right_tag_takes_the_row_and_its_bands() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    let response = harness
        .allowed()
        .send(with_headers(
            "DELETE",
            &price_path(plan_id, seeded.price_id),
            None,
            &[("if-match", "\"0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(price_rows(&harness, plan_id).await.is_empty());
}

#[tokio::test]
async fn a_published_row_is_refused_rather_than_deleted() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;
    harness.publish_price(plan_id, seeded.price_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "DELETE",
            &price_path(plan_id, seeded.price_id),
            None,
            &[("if-match", "\"0\"")],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a design-set 422 arrives as a 400 carrying its code"
    );
    assert_eq!(problem_code(response).await, "LIFECYCLE_FORBIDDEN");
    assert_eq!(price_rows(&harness, plan_id).await.len(), 1);
}

// ---------------------------------------------------------------------------
// The list, and the cursor's keyset walk.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_last_full_page_says_there_is_nothing_after_it() {
    // The exact-multiple boundary: with `limit` rows left, a walk that decided
    // `next_cursor` from the page size alone would return a token pointing at an
    // empty page - and a client that stops on `null` would make one request too
    // many, forever.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    for region in ["EU", "US", "UK", "JP"] {
        seed_price(&harness, plan_id, region).await;
    }

    let response = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{}?limit=4", prices_path(plan_id)),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(4), "{body}");
    assert!(
        body["page_info"]["next_cursor"].is_null(),
        "an exhausted result carries no forward token: {body}"
    );
    assert!(body["page_info"]["prev_cursor"].is_null(), "{body}");
    assert_eq!(body["page_info"]["limit"], serde_json::json!(4));
}

#[tokio::test]
async fn a_walk_visits_every_row_exactly_once() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let mut expected: Vec<String> = Vec::new();
    for region in ["EU", "US", "UK", "JP", "CA"] {
        expected.push(
            seed_price(&harness, plan_id, region)
                .await
                .price_id
                .to_string(),
        );
    }
    expected.sort();

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let path = match &cursor {
            Some(token) => format!("{}?limit=2&cursor={token}", prices_path(plan_id)),
            None => format!("{}?limit=2", prices_path(plan_id)),
        };
        let body = body_json(harness.allowed().send(request("GET", &path, None)).await).await;
        for item in body["items"].as_array().expect("items") {
            seen.push(item["price_id"].as_str().expect("id").to_owned());
        }
        match body["page_info"]["next_cursor"].as_str() {
            Some(token) => cursor = Some(token.to_owned()),
            None => break,
        }
    }

    assert_eq!(seen, expected, "every row, exactly once, in key order");
}

#[tokio::test]
async fn a_row_inserted_ahead_of_a_live_cursor_is_not_skipped_and_one_behind_is_not_duplicated() {
    // D-125's stability guarantee, in the one direction a keyset walk actually
    // gives it. What it does NOT give - a row DELETED behind the cursor - is
    // stated in `api::rest::cursor`'s module doc rather than asserted here,
    // because no cursor over a mutable draft plane can promise it.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    for region in ["EU", "US", "UK", "JP"] {
        seed_price(&harness, plan_id, region).await;
    }

    let first = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!("{}?limit=2", prices_path(plan_id)),
                None,
            ))
            .await,
    )
    .await;
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .expect("a walk of four rows in pages of two continues")
        .to_owned();
    let page_one: Vec<String> = first["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["price_id"].as_str().expect("id").to_owned())
        .collect();

    // Two more rows land mid-walk. Their ids are v7, so they sort AFTER
    // everything already there - ahead of the cursor.
    seed_price(&harness, plan_id, "AU").await;
    seed_price(&harness, plan_id, "NZ").await;

    let second = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!("{}?limit=10&cursor={cursor}", prices_path(plan_id)),
                None,
            ))
            .await,
    )
    .await;
    let page_two: Vec<String> = second["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["price_id"].as_str().expect("id").to_owned())
        .collect();

    for id in &page_one {
        assert!(
            !page_two.contains(id),
            "a row at or before the cursor must never be visited twice"
        );
    }
    assert_eq!(
        page_one.len() + page_two.len(),
        6,
        "the two rows inserted ahead of the cursor are visited, not skipped"
    );
}

#[tokio::test]
async fn the_page_parameters_are_refused_or_clamped_as_the_contract_says() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    seed_price(&harness, plan_id, "EU").await;

    let zero = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{}?limit=0", prices_path(plan_id)),
            None,
        ))
        .await;
    assert_eq!(zero.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(zero).await,
        "invalid_argument",
        "Invalid page size parameter",
    );

    let garbage = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{}?cursor=not-a-cursor!!", prices_path(plan_id)),
            None,
        ))
        .await;
    assert_eq!(garbage.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(garbage).await,
        "invalid_argument",
        "invalid cursor",
    );

    let oversized = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{}?limit=5000", prices_path(plan_id)),
            None,
        ))
        .await;
    assert_eq!(
        oversized.status(),
        StatusCode::OK,
        "the cap is a server limit, not a caller mistake"
    );
    let body = body_json(oversized).await;
    assert_eq!(body["page_info"]["limit"], serde_json::json!(1_000));
}

#[tokio::test]
async fn a_named_lifecycle_state_key_is_refused_and_odata_filter_matches() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    seed_price(&harness, plan_id, "EU").await;

    let named = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{}?lifecycle_state=draft", prices_path(plan_id)),
            None,
        ))
        .await;
    assert_eq!(named.status(), StatusCode::BAD_REQUEST);
    refused_by(
        &body_json(named).await,
        "invalid_argument",
        "unrecognized query parameter `lifecycle_state`",
    );

    let draft = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!(
                    "{}?$filter=lifecycle_state%20eq%20'draft'",
                    prices_path(plan_id)
                ),
                None,
            ))
            .await,
    )
    .await;
    let published = body_json(
        harness
            .allowed()
            .send(request(
                "GET",
                &format!(
                    "{}?$filter=lifecycle_state%20eq%20'published'",
                    prices_path(plan_id)
                ),
                None,
            ))
            .await,
    )
    .await;
    assert_eq!(
        draft["items"].as_array().expect("draft page").len(),
        1,
        "the seeded row is a draft"
    );
    assert!(
        published["items"]
            .as_array()
            .expect("published page")
            .is_empty(),
        "$filter=lifecycle_state eq 'published' must drop the draft; ignoring $filter would keep it"
    );
}

/// **A foreign tenant's caller moves no row of this tenant's.**
///
/// `other_tenant()` reached this file once, on the list `GET`.
/// `every_price_route_is_denied_with_the_database_unchanged` drives the three
/// mutating verbs with `denied()` - a caller the PDP refuses outright - so no
/// mutating price route was ever driven by a caller who is legitimately scoped,
/// in a tenant of their own, at this tenant's row. The two are different
/// mechanisms: the first is the gate, this is the tenant predicate underneath it.
///
/// The owner's own `PATCH` at the end is the positive control.
#[tokio::test]
async fn a_foreign_tenants_caller_moves_no_row_of_this_tenants() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;
    let before = price_rows(&harness, plan_id).await;
    assert_eq!(before.len(), 1, "the premise: one row stands");

    for (method, body, headers) in [
        (
            "PATCH",
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 1 } })),
            vec![("if-match", "\"0\"")],
        ),
        ("DELETE", None, vec![("if-match", "\"0\"")]),
    ] {
        let response = harness
            .other_tenant()
            .send(with_headers(
                method,
                &price_path(plan_id, seeded.price_id),
                body,
                &headers,
            ))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "a row of another tenant is not a row this caller can {method}"
        );
    }

    let created = harness
        .other_tenant()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("US")),
            &[("idempotency-key", "foreign-tenant-create")],
        ))
        .await;
    assert_eq!(
        created.status(),
        StatusCode::NOT_FOUND,
        "and no row can be authored onto another tenant's plan"
    );

    let after = price_rows(&harness, plan_id).await;
    assert_eq!(after.len(), 1, "nothing was created");
    assert_eq!(
        after[0].row_version.get(),
        before[0].row_version.get(),
        "and the standing row did not move"
    );

    // The positive control: the same verb, from the owner, still lands - so the
    // three refusals above are about the caller and not about a dead route.
    let own = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, seeded.price_id),
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 1 } })),
            &[("if-match", "\"0\"")],
        ))
        .await;
    assert_eq!(own.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_walk_is_tenant_scoped_and_a_foreign_plans_rows_are_invisible() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    seed_price(&harness, plan_id, "EU").await;

    let owner = body_json(
        harness
            .allowed()
            .send(request("GET", &prices_path(plan_id), None))
            .await,
    )
    .await;
    assert_eq!(owner["items"].as_array().map(Vec::len), Some(1));

    let foreign = harness
        .other_tenant()
        .send(request("GET", &prices_path(plan_id), None))
        .await;
    assert_eq!(foreign.status(), StatusCode::OK);
    let body = body_json(foreign).await;
    assert_eq!(
        body["items"].as_array().map(Vec::len),
        Some(0),
        "the compiled scope is the SQL filter, so a foreign tenant sees an empty page: {body}"
    );
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_price_route_is_denied_with_the_database_unchanged() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    for (method, path, body, headers) in [
        (
            "POST",
            prices_path(plan_id),
            Some(create_body("US")),
            vec![("idempotency-key", "denied-price")],
        ),
        (
            "PATCH",
            price_path(plan_id, seeded.price_id),
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 1 } })),
            vec![("if-match", "\"0\"")],
        ),
        (
            "DELETE",
            price_path(plan_id, seeded.price_id),
            None,
            vec![("if-match", "\"0\"")],
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
    let list = harness
        .denied()
        .send(request("GET", &prices_path(plan_id), None))
        .await;
    assert_eq!(list.status(), StatusCode::FORBIDDEN);

    let after = price_rows(&harness, plan_id).await;
    assert_eq!(after.len(), 1, "no row created and none deleted");
    assert_eq!(after[0].row_version.get(), 0, "and none edited");
}

#[tokio::test]
async fn an_unauthenticated_price_request_is_refused_before_the_gate() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let response = harness
        .anonymous()
        .send(request("GET", &prices_path(plan_id), None))
        .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// The audit trail the three price routes write (D-135, D-158).
// ---------------------------------------------------------------------------

/// The records on one plan's segment, in seq order.
///
/// A price row's chain is the **plan's** (D-135 keys the segment on the audited
/// subject's aggregate, and a price row's aggregate is the plan it prices), so a
/// plan's authoring history and its rows' edits are one walk rather than a join.
async fn plan_records(harness: &Harness, plan_id: Uuid) -> Vec<(String, String, String)> {
    rest_support::audit_rows(harness)
        .await
        .into_iter()
        .filter(|row| row.chain_id == plan_id)
        .map(|row| (row.action, row.subject_kind, row.subject_ref))
        .collect()
}

#[tokio::test]
async fn a_price_create_writes_exactly_one_record_naming_the_row() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let before = plan_records(&harness, plan_id).await.len();

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("audit-create"),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let price_id = body_json(response).await["price_id"]
        .as_str()
        .expect("the id is answered")
        .to_owned();

    let records = plan_records(&harness, plan_id).await;
    assert_eq!(records.len(), before + 1, "{records:?}");
    assert_eq!(
        records.last(),
        Some(&("create".to_owned(), "price_unit".to_owned(), price_id)),
        "{records:?}"
    );
}

#[tokio::test]
async fn a_price_patch_writes_exactly_one_record_naming_the_row() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;
    let before = plan_records(&harness, plan_id).await.len();

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, seeded.price_id),
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 99 } })),
            &[("if-match", "\"0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let records = plan_records(&harness, plan_id).await;
    assert_eq!(records.len(), before + 1, "{records:?}");
    assert_eq!(
        records.last(),
        Some(&(
            "update".to_owned(),
            "price_unit".to_owned(),
            seeded.price_id.to_string()
        )),
        "{records:?}"
    );
}

#[tokio::test]
async fn a_price_delete_writes_a_record_that_outlives_the_row_it_names() {
    // The row is gone and the record is not, which is the whole point of an
    // append-only trail on a >= 7-year horizon: "who deleted this" is answerable
    // precisely because the answer does not live on the deleted row.
    //
    // The action is `delete` and not `abandon`: a never-published price row is
    // really removed (S3 sec 4.3), where a discarded plan revision is flipped and
    // keeps its number (D-145).
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;
    let before = plan_records(&harness, plan_id).await.len();

    let response = harness
        .allowed()
        .send(with_headers(
            "DELETE",
            &price_path(plan_id, seeded.price_id),
            None,
            &[("if-match", "\"0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(price_rows(&harness, plan_id).await.is_empty());

    let records = plan_records(&harness, plan_id).await;
    assert_eq!(records.len(), before + 1, "{records:?}");
    assert_eq!(
        records.last(),
        Some(&(
            "delete".to_owned(),
            "price_unit".to_owned(),
            seeded.price_id.to_string()
        )),
        "{records:?}"
    );
}

#[tokio::test]
async fn a_refused_price_write_leaves_no_record_of_having_happened() {
    // The record is inside the mutation's own transaction (D-135). Two refusals,
    // one per verb, because each opens its own transaction.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;
    let before = plan_records(&harness, plan_id).await.len();

    for (method, body) in [
        (
            "PATCH",
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 99 } })),
        ),
        ("DELETE", None),
    ] {
        let response = harness
            .allowed()
            .send(with_headers(
                method,
                &price_path(plan_id, seeded.price_id),
                body,
                &[("if-match", "\"7\"")],
            ))
            .await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{method}");
    }

    assert_eq!(
        plan_records(&harness, plan_id).await.len(),
        before,
        "neither refusal recorded anything"
    );
}

#[tokio::test]
async fn every_price_record_extends_the_plans_own_segment() {
    // D-135's keying, asserted rather than assumed: a price row's records must
    // not open a segment of their own, or "who changed this plan" becomes a join
    // across chains and the roll-up has one more head to carry.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, seeded.price_id),
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 99 } })),
            &[("if-match", "\"0\"")],
        ))
        .await;

    let chains: std::collections::BTreeSet<Uuid> = rest_support::audit_rows(&harness)
        .await
        .into_iter()
        .map(|row| row.chain_id)
        .collect();
    assert_eq!(
        chains,
        std::collections::BTreeSet::from([plan_id]),
        "one segment for the plan and everything under it"
    );
}

/// One record's action and the pair `inst-au-complete` names first.
type StatePair = (String, Option<serde_json::Value>, Option<serde_json::Value>);

#[tokio::test]
async fn every_price_record_carries_the_before_and_after_state_its_action_implies() {
    // `inst-au-complete`'s first field, on every price route. Blanking both to
    // `None` in `record_price_mutation` used to leave the whole suite green.
    //
    // A delete has no after-state, and that is the assertion rather than an
    // omission: an empty object would be a claim that the row still stands in
    // some shape, and the row is gone.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;
    for (method, body, tag) in [
        (
            "PATCH",
            Some(serde_json::json!({ "content": { "model_kind": "flat", "amount_minor": 99 } })),
            "\"0\"",
        ),
        ("DELETE", None, "\"1\""),
    ] {
        let response = harness
            .allowed()
            .send(with_headers(
                method,
                &price_path(plan_id, seeded.price_id),
                body,
                &[("if-match", tag)],
            ))
            .await;
        assert!(
            response.status().is_success(),
            "{method}: {:?}",
            response.status()
        );
    }

    let pairs: Vec<StatePair> = rest_support::audit_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == "price_unit")
        .map(|row| (row.action, row.before_state, row.after_state))
        .collect();
    assert_eq!(pairs.len(), 3, "create, update, delete: {pairs:?}");

    let (action, before, after) = &pairs[0];
    assert_eq!(action, "create");
    assert!(before.is_none(), "a create has no before-state: {before:?}");
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
        Some(&serde_json::json!(1))
    );

    let (action, before, after) = &pairs[2];
    assert_eq!(action, "delete");
    assert_eq!(
        before.as_ref().and_then(|state| state.get("rowVersion")),
        Some(&serde_json::json!(1)),
        "the row as it stood when it was taken"
    );
    assert!(
        after.is_none(),
        "and no after-state: an empty object would claim the row still stands: {after:?}"
    );
}

#[tokio::test]
async fn two_lines_of_one_market_render_two_distinct_keys() {
    // **The read side of D-196 clause (3), on the wire.** `ScopeKeyRequest` has no
    // usage-line member by decision — the line is authored on the content — but the
    // *response* view is the store's rendering of what a row is filed under, and it
    // showed eight axes. So two rows on two meters of one market answered with byte-
    // identical `scope_key` objects, and the surface a reviewer approves from could
    // not tell them apart. Nothing here is about what a caller may say; it is about
    // what they are told.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let author = async |meter: &str, key: &str| {
        let response = harness
            .allowed()
            .send(with_headers(
                "POST",
                &prices_path(plan_id),
                Some(serde_json::json!({
                    "scope_key": {
                        "currency": "USD",
                        "region": "EU",
                        "phase": harness_phase(),
                        "price_eligibility": "all_subscriptions",
                        "charge_kind": "usage",
                        "cohort": serde_json::Value::Null
                    },
                    "content": {
                        "model_kind": "per_unit",
                        "amount_minor": 700,
                        "tax_inclusive": false,
                        "meter": meter,
                        "billing_granularity": "per_hour"
                    }
                })),
                &keyed(key),
            ))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "{:?}",
            response.body()
        );
        body_json(response).await
    };

    let cloudlets = author("cloudlets", "d196-view-1").await;
    let egress = author("egress_gb", "d196-view-2").await;

    assert_ne!(
        cloudlets["scope_key"], egress["scope_key"],
        "two meters of one market are two keys and the view has to say so: {} vs {}",
        cloudlets["scope_key"], egress["scope_key"]
    );
    assert_eq!(
        cloudlets["scope_key"]["meter"],
        serde_json::json!("cloudlets")
    );
    assert_eq!(
        cloudlets["scope_key"]["dimension_key"],
        serde_json::Value::Null,
        "the undimensioned line renders as absent rather than as the store's empty sentinel"
    );
    // **And the row's own member spells it the same way.** One body carries the
    // fact twice - `content.dimension_key` and `scope_key.dimension_key` - and a
    // consumer resolving which line a usage row prices joins them. Two spellings
    // of one absence make that join say the row is dimensioned differently from
    // its own key.
    assert_eq!(
        cloudlets["content"]["dimension_key"], cloudlets["scope_key"]["dimension_key"],
        "one body, one fact, one spelling: {cloudlets}"
    );
}

#[tokio::test]
async fn two_spellings_of_one_meter_are_one_key_on_the_wire_too() {
    // **The wire is where the raw string entered.** `content_of` copies
    // `view.meter` and `view.dimension_key` verbatim — by design, it is a decode —
    // so this path is the one that could actually author `"cloudlets "`, and the
    // key it was filed under carried `cloudlets` because `Meter::new` trims. The
    // occupancy read renders the axis, both partial UNIQUE indexes compare the
    // columns, and neither found the other spelling: two rows stood on one
    // canonical scope key, and the plan's next publish flipped both of them.
    //
    // The store-level mechanism is pinned in `sqlite_price_repo`; what this adds is
    // that the whole route — decode, key contradiction check, door — spends the one
    // normalization, and that the response echoes the axis rather than the spelling.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let padded = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(usage_create_body("EU", "cloudlets ")),
            &keyed("ws-meter-1"),
        ))
        .await;
    assert_eq!(padded.status(), StatusCode::CREATED, "{:?}", padded.body());
    let created = body_json(padded).await;
    assert_eq!(
        created["scope_key"]["meter"],
        serde_json::json!("cloudlets"),
        "the view renders the axis the row is filed under, not the caller's spelling"
    );
    assert_eq!(
        created["content"]["meter"],
        serde_json::json!("cloudlets"),
        "and the row's own copy of that axis is the same value, so a read-modify-write \
         resubmits a line the update guard recognises"
    );

    let clash = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(usage_create_body("EU", "cloudlets")),
            &keyed("ws-meter-2"),
        ))
        .await;
    assert_eq!(clash.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(clash).await, "DUPLICATE_SCOPE_KEY");
    assert_eq!(
        price_rows(&harness, plan_id).await.len(),
        1,
        "one canonical key, one row"
    );
}

#[tokio::test]
async fn a_metered_row_may_patch_while_echoing_the_key_it_cannot_fully_name() {
    // **The hole D-196 clause (3) opened on this path, pinned.** The usage line
    // is authored on the *content* view — `ScopeKeyRequest` has no `meter`
    // member — so the door derives the ninth and tenth axes from the content and
    // a stored usage row's key carries a line no request body could have stated.
    // The key-immutability check above compares the named key against the stored
    // one, and comparing those two raw would refuse **every** `PATCH` that echoes
    // its own key on a metered row: the caller would be told their key moved by
    // naming exactly the key they are on.
    //
    // The comparison is therefore over the axes the wire can express, with the
    // stored line carried onto the named key first. The neighbouring case proves
    // the check still refuses a key that really did move.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let create = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(serde_json::json!({
                "scope_key": {
                    "currency": "USD",
                    "region": "EU",
                    "phase": harness_phase(),
                    "price_eligibility": "all_subscriptions",
                    "charge_kind": "usage",
                    "cohort": serde_json::Value::Null
                },
                "content": {
                    "model_kind": "per_unit",
                    "amount_minor": 700,
                    "tax_inclusive": false,
                    "meter": "cloudlets",
                    "billing_granularity": "per_hour"
                }
            })),
            &keyed("d196-clause3-create"),
        ))
        .await;
    assert_eq!(create.status(), StatusCode::CREATED, "{:?}", create.body());
    let price_id = price_rows(&harness, plan_id).await[0].price_id;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, price_id),
            Some(serde_json::json!({
                "scope_key": {
                    "currency": "USD",
                    "region": "EU",
                    "phase": harness_phase(),
                    "price_eligibility": "all_subscriptions",
                    "charge_kind": "usage",
                    "cohort": serde_json::Value::Null
                },
                "content": {
                    "model_kind": "per_unit",
                    "amount_minor": 900,
                    "tax_inclusive": false,
                    "meter": "cloudlets",
                    "billing_granularity": "per_hour"
                }
            })),
            &[("if-match", "\"0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK, "{:?}", response.body());
    let after = price_rows(&harness, plan_id).await;
    assert_eq!(
        after[0]
            .row
            .amount_minor
            .map(bss_pricing::domain::money::MinorAmount::get),
        Some(900)
    );
    assert_eq!(
        after[0]
            .scope_key
            .meter()
            .map(bss_pricing::domain::scope_key::Meter::as_str),
        Some("cloudlets"),
        "the line the row is filed under is untouched by an ordinary content edit"
    );
}

/// Re-rating a `per_unit` draft through `PATCH` lands in the **store**.
///
/// D-311 moved a `per_unit` row's money out of `amount_minor` into a column of
/// its own, and `content_assignments` — the hand-written list of columns an
/// `update_draft` writes — was not extended with it. The `UPDATE` therefore never
/// named `unit_rate_nano`: the route answered `200`, the response body rendered
/// the **stored** record so it looked consistent, and the author's new rate was
/// gone. That is the **third** site that one column split stranded — after
/// `project_row` and the tier-band site, both already fixed — and the only one no
/// grep for the class could reach, because a hand-written list of
/// `(Column, Value)` pairs looks nothing like the code those greps matched.
///
/// The readback is [`price_rows`], which reads the store — **not** the `PATCH`
/// response and not the request body. Either of those would have passed against
/// the defect.
#[tokio::test]
async fn a_patch_that_re_rates_a_per_unit_row_moves_the_stored_rate() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let create = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(serde_json::json!({
                "scope_key": {
                    "currency": "USD",
                    "region": "EU",
                    "phase": harness_phase(),
                    "price_eligibility": "all_subscriptions",
                    "charge_kind": "recurring",
                    "cohort": serde_json::Value::Null
                },
                "content": {
                    "model_kind": "per_unit",
                    // Neither rate is a whole number of minor units, and neither is
                    // the other scaled by a power of ten: a slip between the wire's
                    // nano scale and the minor-unit one lands on a number these
                    // assertions refuse rather than on a plausible price.
                    "unit_rate_nano_minor": 1_234_567_891_i64,
                    "quantity_source": "manual",
                    "manual_quantity": 12,
                    "tax_inclusive": false
                }
            })),
            &keyed("per-unit-re-rate"),
        ))
        .await;
    assert_eq!(create.status(), StatusCode::CREATED, "{:?}", create.body());
    let price_id = price_rows(&harness, plan_id).await[0].price_id;

    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, price_id),
            Some(serde_json::json!({
                "content": {
                    "model_kind": "per_unit",
                    "unit_rate_nano_minor": 987_654_321_i64,
                    "quantity_source": "manual",
                    "manual_quantity": 12,
                    "tax_inclusive": false
                }
            })),
            &[("if-match", "\"0\"")],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::OK, "{:?}", patched.body());

    let after = price_rows(&harness, plan_id).await;
    assert_eq!(
        after[0]
            .row
            .unit_rate
            .map(bss_pricing::domain::money::RateMinor::nano_minor),
        Some(987_654_321),
        "the rate the author submitted is the rate the store holds"
    );
    assert_eq!(after[0].row_version.get(), 1, "the edit landed");
}

/// A `PATCH` that moves the usage line is refused by the axis rule's own code.
///
/// **The paired negative for the case above, and the only observation of one arm
/// of `repo_failure`.** `RepoError::UsageLineDisagrees` had no assertion anywhere
/// on what it maps to: the store's suites asserted the `RepoError` and stopped,
/// so corrupting `storage.rs`'s arm to `DomainError::Internal` left the whole
/// crate green while an author whose row's meter disagrees with its key read a
/// 500 for a request they could fix by editing one field. That is the same shape
/// the fold's own comment records having been caught once on `WindowOverlap`.
///
/// The body names **no** `scope_key`, so the key-immutability check above never
/// runs and the refusal can only come from the store's line guard
/// (`price_repo::check_update_keeps_the_line`). Everything else about the content
/// is legal — a `usage` row with a meter and a granularity — so a green here
/// would mean the line really did move, which is D-196's whole point: the pair
/// are key axes, and moving them re-files the row under a key nothing checked for
/// occupancy.
#[tokio::test]
async fn a_patch_that_moves_the_usage_line_is_refused_by_its_code() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let create = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(serde_json::json!({
                "scope_key": {
                    "currency": "USD",
                    "region": "EU",
                    "phase": harness_phase(),
                    "price_eligibility": "all_subscriptions",
                    "charge_kind": "usage",
                    "cohort": serde_json::Value::Null
                },
                "content": {
                    "model_kind": "per_unit",
                    "amount_minor": 700,
                    "tax_inclusive": false,
                    "meter": "cloudlets",
                    "billing_granularity": "per_hour"
                }
            })),
            &keyed("d196-line-move-create"),
        ))
        .await;
    assert_eq!(create.status(), StatusCode::CREATED, "{:?}", create.body());
    let price_id = price_rows(&harness, plan_id).await[0].price_id;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, price_id),
            Some(serde_json::json!({
                "content": {
                    "model_kind": "per_unit",
                    "amount_minor": 700,
                    "tax_inclusive": false,
                    "meter": "egress_gb",
                    "billing_granularity": "per_hour"
                }
            })),
            &[("if-match", "\"0\"")],
        ))
        .await;

    assert_eq!(
        problem_code(response).await,
        "USAGE_LINE_AXIS_MISMATCH",
        "a moved usage line is the axis rule's refusal, not a fault of the store"
    );

    let after = price_rows(&harness, plan_id).await;
    assert_eq!(after.len(), 1, "the refusal creates no second row");
    assert_eq!(
        after[0]
            .scope_key
            .meter()
            .map(bss_pricing::domain::scope_key::Meter::as_str),
        Some("cloudlets"),
        "and the refused edit leaves the stored line where it was"
    );
    assert_eq!(
        after[0].row.meter.as_deref(),
        Some("cloudlets"),
        "on the row's own copy of the pair as well as on its key"
    );
}

// ---------------------------------------------------------------------------
// Slice 10's authorable primitives, end to end through the create surface
// ---------------------------------------------------------------------------

/// A create carrying every authorable Slice-10 field stores all six, and the
/// read surface answers them back (`inst-ad-author`, `inst-rv-attrs`,
/// `inst-ft-typed`, `inst-ft-fallback`, `inst-dr-boundary`).
///
/// **This case exists because a probe found its absence.** Replacing
/// `content_of`'s `reserved_rate_nano_minor` mapping with a hard `None` -- a client
/// authoring a reserved rate and the gear silently discarding it -- reddened
/// *nothing* across the whole fast suite. The authoring path was the third of
/// four layers found untested this way, after the store round trip and the
/// projection, and it is the one an operator meets first.
///
/// The row is a **usage** row because `inst-rv-usage` refuses a reservation
/// anywhere else, and the fallback is authored because `inst-ft-fallback`
/// refuses a usage floor without one -- so this fixture is a row the publish
/// rules would also accept, not merely one the store will hold.
#[tokio::test]
async fn a_create_carrying_every_slice_ten_primitive_stores_all_of_them() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let mut body = create_body("EU");
    body["scope_key"]["charge_kind"] = serde_json::json!("usage");
    body["content"] = serde_json::json!({
        "model_kind": "per_unit",
        "amount_minor": 1_500,
        "tax_inclusive": false,
        "meter": "storage.gb",
        "billing_granularity": "per_hour",
        "reserved_rate_nano_minor": 250_000_000_000_i64,
        "reservation_flavor": "capacity",
        "min_qty_purchase": 7,
        "min_qty_usage": 11,
        "min_qty_usage_fallback": "exception",
        "discount_ref": "promo/spring"
    });

    let created = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(body),
            &keyed("slice-10-primitives"),
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let rows = price_rows(&harness, plan_id).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0].row;

    assert_eq!(
        row.reserved_rate
            .map(bss_pricing::domain::money::RateMinor::nano_minor),
        Some(250_000_000_000),
        "the reserved rate the caller authored"
    );
    assert_eq!(
        row.reservation_flavor,
        Some(ReservationFlavor::Capacity),
        "the flavor decides whether the reserved quantity leaves Q at all"
    );
    assert_eq!(row.min_qty_purchase, Some(7));
    assert_eq!(row.min_qty_usage, Some(11));
    assert_eq!(
        row.min_qty_usage_fallback,
        Some(MinQtyUsageFallback::Exception)
    );
    assert_eq!(row.discount_ref.as_deref(), Some("promo/spring"));
}

/// The vocabulary is closed at the surface: an unknown flavor is a malformed
/// request naming the field, not a stored value and not a fault.
///
/// The paired negative for the case above -- without it, a surface that accepted
/// anything and stored nothing would pass the positive case if the enum ever
/// gained a permissive parse.
#[tokio::test]
async fn a_create_naming_an_unknown_reservation_flavor_is_refused() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let mut body = create_body("EU");
    body["scope_key"]["charge_kind"] = serde_json::json!("usage");
    body["content"]["meter"] = serde_json::json!("storage.gb");
    body["content"]["billing_granularity"] = serde_json::json!("per_hour");
    body["content"]["reserved_rate_nano_minor"] = serde_json::json!(250_000_000_000_i64);
    body["content"]["reservation_flavor"] = serde_json::json!("burst");

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(body),
            &keyed("unknown-flavor"),
        ))
        .await;

    assert_refused_naming(response, "reservation_flavor").await;
    assert!(
        price_rows(&harness, plan_id).await.is_empty(),
        "a refused create stores nothing"
    );
}

// ---------------------------------------------------------------------------
// The plan a row is filed under has to exist.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_row_on_a_plan_this_tenant_does_not_have_is_refused() {
    // The route names a plan in its path and files the row under it. Nothing
    // resolved that plan: a `planId` no revision exists for was accepted, and
    // the row landed under a key no read can reach -- every price read goes
    // through the plan, so the row existed and was unreachable in the same
    // breath.
    //
    // It reads as a tenancy question and is not one. A caller naming another
    // tenant's plan gets exactly what a caller naming an invented uuid gets,
    // because the scope filter already removed the foreign row before this
    // check sees anything -- so no id is confirmed and nothing leaks. What was
    // missing is the *subject*: a write naming a plan the caller does not have
    // is a write about nothing.
    //
    // The sibling guard two functions up says the same about regions, and for
    // the same reason: "was answered 201, and learned of it at publish".
    let harness = Harness::new().await;
    let absent = Uuid::now_v7();

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(absent),
            Some(create_body("EU")),
            &keyed("absent-plan-1"),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a row whose plan does not exist has no subject to be filed under"
    );
    // The family and the plan the door could not find: a 404 on this route is
    // also what a denied read answers, and the two must not be interchangeable.
    refused_by(
        &body_json(response).await,
        "not_found",
        &format!("plan {absent} not found"),
    );
    assert!(
        price_rows(&harness, absent).await.is_empty(),
        "and the refusal stores nothing"
    );
}

#[tokio::test]
async fn a_row_on_a_plan_this_tenant_does_have_still_lands() {
    // The positive control. Without it the guard above is satisfied by a route
    // that refuses every create, and the whole price surface could be dead
    // while this file stayed green.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(create_body("EU")),
            &keyed("present-plan-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(price_rows(&harness, plan_id).await.len(), 1);
}

// --- D-312: key contradictions are refused where they are made ---------------

/// The body from the screenshot that started this: `flat` on a `usage` key.
fn flat_on_usage_body() -> serde_json::Value {
    let mut body = create_body("EU");
    body["scope_key"]["charge_kind"] = serde_json::json!("usage");
    body["scope_key"]["meter"] = serde_json::json!("addon_dr");
    body
}

#[tokio::test]
async fn a_kind_illegal_on_the_charge_kind_is_refused_at_save() {
    // It used to answer 201, and again 200 on every subsequent PATCH; the refusal
    // arrived at publish, as the whole plan's, naming a rule the author met
    // several screens earlier.
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(flat_on_usage_body()),
            &keyed("d312-flat-on-usage-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "MODEL_KIND_CHARGEKIND_MISMATCH"
    );
    assert!(
        price_rows(&harness, plan_id).await.is_empty(),
        "a refused save writes no row"
    );
}

#[tokio::test]
async fn an_eval_policy_field_off_its_charge_kind_is_refused_at_save() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let body = create_body_with("EU", "billing_granularity", serde_json::json!("whole_unit"));

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(body),
            &keyed("d312-granularity-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "EVAL_POLICY_MISPLACED");
}

/// **The shape that actually produced the stored population**, and the one the
/// first implementation of this check let through.
///
/// D-312 counted eleven contradictory rows on the stand; nine are
/// `billing_granularity` on a recurring key, written by the Studio's
/// `defaultContent` with the model-kind picker never touched. So the body that
/// matters carries the misplaced field and **no `model_kind` at all** — and
/// `inst-mk-forbidden` used to return early on a missing kind, which meant the
/// check built for these rows answered 201 to exactly them. The fault's operands
/// are the field and the frozen `charge_kind`; the kind is absent from the fault,
/// not merely from the row.
#[tokio::test]
async fn an_eval_policy_field_is_refused_before_a_kind_has_been_picked() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let mut body = create_body("EU");
    body["content"] =
        serde_json::json!({ "billing_granularity": "whole_unit", "tax_inclusive": false });

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(body),
            &keyed("d312-granularity-no-kind-1"),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem_code(response).await, "EVAL_POLICY_MISPLACED");
    assert!(
        price_rows(&harness, plan_id).await.is_empty(),
        "a refused save writes no row"
    );
}

/// The half that decides whether this change is the design or its opposite.
///
/// A row missing its kind, its price, or its bands is a row a later call
/// completes — the multi-call authoring the whole publish-time rule set exists to
/// allow. If any of these ever answers 400, the write plane has become a
/// validator and nothing else here would say so.
#[tokio::test]
async fn an_incomplete_row_still_saves() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    let mut no_kind = create_body("EU");
    no_kind["content"] = serde_json::json!({ "tax_inclusive": false });
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(no_kind),
            &keyed("d312-incomplete-1"),
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "a row whose kind has not arrived yet is authorable"
    );

    let mut unpriced = create_body("EU");
    unpriced["scope_key"]["charge_kind"] = serde_json::json!("one_time");
    unpriced["content"] = serde_json::json!({ "model_kind": "flat", "tax_inclusive": false });
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(unpriced),
            &keyed("d312-incomplete-2"),
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "a row whose amount has not been typed yet is authorable"
    );
}

/// The PATCH door, on a row that is legal until the edit makes it otherwise.
///
/// This is the door the Studio actually used: an author opens a stored usage row
/// and picks a kind the charge kind does not allow. Covering only the create would
/// have left it open — and the row would go on saving 200 until publish refused
/// the whole plan.
#[tokio::test]
async fn an_edit_into_a_key_contradiction_is_refused_on_patch() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;

    // A legal usage row, authored through the route.
    let mut legal = create_body("EU");
    legal["scope_key"]["charge_kind"] = serde_json::json!("usage");
    legal["scope_key"]["meter"] = serde_json::json!("addon_dr");
    legal["content"] = serde_json::json!({
        "model_kind": "per_unit",
        "unit_rate_nano_minor": 1_000_000_000_i64,
        "billing_granularity": "whole_unit",
        "tax_inclusive": false
    });
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &prices_path(plan_id),
            Some(legal),
            &keyed("d312-patch-1"),
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "the legal usage row must author, or this test proves nothing"
    );
    let tag = etag_of(&response).expect("the create answers a tag");
    let body = body_json(response).await;
    let price_id = body["price_id"]
        .as_str()
        .expect("the id is answered")
        .to_owned();

    // The edit the picker used to offer: `flat`, on a usage key.
    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &price_path(plan_id, price_id.parse().expect("uuid")),
            Some(serde_json::json!({
                "content": { "model_kind": "flat", "amount_minor": 300, "tax_inclusive": false }
            })),
            &[("if-match", tag.as_str())],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "MODEL_KIND_CHARGEKIND_MISMATCH"
    );
}

/// **The SQL tenant predicate on the delete.**
///
/// `rest_authz.rs`'s census cannot reach it: the row its seed leaves carries a
/// window, and `price_repo::refuse_if_a_window_stands_on_it` answers the **owner**
/// `PRICE_WINDOW_SCHEDULED` there — a row a window has ever stood on can never be
/// deleted, so no act of that fixture makes the owner's call succeed and the route
/// is listed in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE`. Here [`seed_price`]
/// leaves a draft row no window stands on, which is
/// `a_delete_under_the_right_tag_takes_the_row_and_its_bands`' world: the owner's
/// identical call is accepted and a refusal of the foreign caller means tenancy
/// rather than a window.
///
/// `Harness::denied` drives a PDP that refuses everything and
/// `Harness::scope_mismatch` exercises `access_scope`'s write-target membership
/// assertion; neither hands a caller-supplied id of another tenant's row to a
/// repository, which is the only way the predicate is exercised at all. A handler
/// that resolved `{priceId}` before narrowing would satisfy both of those and
/// destroy another tenant's row.
#[tokio::test]
async fn a_foreign_tenant_cannot_delete_this_tenants_price_row() {
    let harness = Harness::new().await;
    let plan_id = seeded_plan(&harness).await;
    let seeded = seed_price(&harness, plan_id, "EU").await;

    // The **`{priceId}`** varies and the plan stays: the absent arm has to differ
    // from the foreign one in the id whose tenant is in question, so that a handler
    // narrowing on the plan alone cannot answer both from the same lookup.
    let delete = |price_id: Uuid| {
        with_headers(
            "DELETE",
            &price_path(plan_id, price_id),
            None,
            &[("if-match", "\"0\"")],
        )
    };
    rest_support::foreign_is_indistinguishable(
        &harness,
        delete(seeded.price_id),
        delete(Uuid::now_v7()),
    )
    .await;

    // The control, and it is what makes the two refusals mean anything. Last,
    // because it takes the row.
    let owner = harness.allowed().send(delete(seeded.price_id)).await;
    assert_eq!(
        owner.status(),
        StatusCode::NO_CONTENT,
        "the owner's identical delete must be accepted, or the refusals above are about the \
         request rather than about the tenant"
    );
    assert!(price_rows(&harness, plan_id).await.is_empty());
}
