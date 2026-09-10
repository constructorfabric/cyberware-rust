//! The `PriceOverlay` authoring surface, end to end —
//! `design/09-price-overlays.md` §5.
//!
//! What this suite is for, beyond driving the four authoring routes: **the status split**.
//! §5 types seven of the nine overlay codes as architectural 422s and two of them
//! **409 outright**, and this platform has no 422 category at all — so the seven
//! reach the wire as one aggregate 400 whose per-violation codes are the
//! discriminators, and the two reach it as conflicts. That difference is
//! invisible to every unit test of the rules, because the rules raise all nine
//! into one report; it exists only at this seam.
//!
//! `rest_authz` drives the same five routes for the **gate**. Nothing here
//! re-proves that.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::overlays::{PRICE_OVERLAY_SUBMIT, PRICE_OVERLAYS};
use rest_support::{
    Harness, body_json, code_in, etag_of, location_of, problem_code, request, with_headers,
};
use time::OffsetDateTime;
use uuid::Uuid;

fn overlay_path(overlay_id: Uuid) -> String {
    format!("{PRICE_OVERLAYS}/{overlay_id}")
}

fn submit_path(overlay_id: Uuid) -> String {
    PRICE_OVERLAY_SUBMIT.replace("{overlayId}", &overlay_id.to_string())
}

/// The ordinary line: a `global` overlay's list-default discount.
fn default_discount(bp: i64) -> serde_json::Value {
    serde_json::json!({
        "adjustment_kind": "discount",
        "magnitude_kind": "percent_bp",
        "adjustment_value": bp,
    })
}

/// Author a `global` overlay at `precedence`, and hand back its id.
async fn seed_overlay(harness: &Harness, precedence: i32) -> Uuid {
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": precedence,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    body["price_overlay_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("the created overlay's id")
}

/// Create's `Location` is a real GET of the draft, and that GET's `ETag` is
/// the token the next `PATCH` must present.
#[tokio::test]
async fn create_location_is_a_get_of_the_draft() {
    let harness = Harness::new().await;
    let created = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let location = location_of(&created).expect("create sets Location");
    let overlay = body_json(created).await["price_overlay_id"]
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
    assert_eq!(body["price_overlay_id"], overlay.to_string());
    assert_eq!(body["lifecycle_state"], "draft");
    assert_eq!(body["revision"], 0);
    assert_eq!(body["row_version"], 0);
    assert_eq!(etag, "\"0-0\"");

    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &overlay_path(overlay),
            Some(serde_json::json!({ "revision": 0, "lines": [default_discount(1500)] })),
            &[("if-match", etag.as_str())],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::OK);
}

/// An unknown overlay reads like an absent one — no existence leak.
#[tokio::test]
async fn get_unknown_overlay_is_404() {
    let harness = Harness::new().await;
    let response = harness
        .allowed()
        .send(request("GET", &overlay_path(Uuid::now_v7()), None))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// With no draft, the by-id read answers the published revision.
#[tokio::test]
async fn get_published_overlay_without_a_draft_returns_published() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;
    harness
        .state
        .overlays
        .publish_revision(
            &harness.scope(),
            harness.tenant,
            overlay,
            0,
            rest_support::seed_stamp(),
        )
        .await
        .expect("published");
    let got = harness
        .allowed()
        .send(request("GET", &overlay_path(overlay), None))
        .await;
    assert_eq!(got.status(), StatusCode::OK);
    let body = body_json(got).await;
    assert_eq!(body["lifecycle_state"], "published");
}

/// A matching `If-None-Match` is 304 with no body — GET plan's rule.
#[tokio::test]
async fn get_if_none_match_matching_tag_is_304() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;
    let first = harness
        .allowed()
        .send(request("GET", &overlay_path(overlay), None))
        .await;
    let etag = etag_of(&first).expect("ETag");
    let again = harness
        .allowed()
        .send(with_headers(
            "GET",
            &overlay_path(overlay),
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

/// Every precondition-violation code the response carries.
///
/// The RFC 9457 document puts them at `context.violations[].type` — the
/// **code** is the `type` of a violation, and its prose is the `description`.
/// That placement is what "the code is the discriminator, not the status"
/// means concretely, and asserting on it is what stops a refusal degrading into
/// a message a consumer would have to parse.
fn violation_codes(body: &serde_json::Value) -> Vec<String> {
    body["context"]["violations"]
        .as_array()
        .map(|violations| {
            violations
                .iter()
                .filter_map(|v| v["type"].as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// `inst-pl-author` / `inst-pl-return`.
// ---------------------------------------------------------------------------

/// **A retried create is replayed, not re-executed** (Z12-7's overlay half).
///
/// The route took the `Idempotency-Key` and discarded it — `let _client_key = …`,
/// required of every caller and read by nothing — and unlike the bundle create
/// there is no uniqueness index standing behind this one. So a retry did not even
/// get a wrong-but-plausible conflict: it **created a second draft overlay**, with
/// its own id, its own revision 0 and its own audit trail, and answered `201` as
/// if that were the first call's answer.
///
/// Both halves are asserted. The status **and the body** come back as the first
/// call's, because a replay that re-rendered would hand the caller an id naming an
/// overlay its first attempt never made; and the store still holds one overlay,
/// which is what makes this a proof about at-most-once rather than about a status
/// code. The list read is the load-bearing half here — a route that minted a second
/// overlay and echoed the first body would pass the status assertion alone.
#[tokio::test]
async fn a_retried_create_replays_the_first_answer_and_creates_one_overlay() {
    let harness = Harness::new().await;
    let key = Uuid::now_v7().to_string();
    let payload = serde_json::json!({
        "scope_class": "global",
        "precedence": 10,
        "tax_basis": "delegated_tariffs",
        "target_plan_ids": [],
        "lines": [default_discount(1000)],
    });

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(payload.clone()),
            &[("idempotency-key", &key)],
        ))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first = body_json(first).await;

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(payload),
            &[("idempotency-key", &key)],
        ))
        .await;

    assert_eq!(
        replay.status(),
        StatusCode::CREATED,
        "a replay answers what the first call answered"
    );
    let replayed = body_json(replay).await;
    assert_eq!(
        replayed, first,
        "verbatim: the overlay id above all, since a fresh one would name a draft the caller \
         never authored"
    );

    // The positive control against a replay that is merely an echo over a second
    // insert: exactly one overlay exists, and it is the one both answers named.
    let listed = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = body_json(listed).await;
    let overlays = listed["overlays"].as_array().expect("the list");
    assert_eq!(overlays.len(), 1, "one create, one overlay: {listed}");
    assert_eq!(overlays[0]["price_overlay_id"], first["price_overlay_id"]);
}

/// **A different overlay under a spent key is refused** (Z12-7's overlay half).
///
/// Armed on a genuinely different request — another precedence, another tax basis,
/// another magnitude — under the same key. Replaying the *same* body proves the
/// opposite property and is the case above, which is this one's positive control.
/// Before the gate was wired this second call was a plain `201` over a **second**
/// draft overlay: nothing in the store objects to two drafts, so there was not even
/// a misleading conflict to be told about.
#[tokio::test]
async fn a_different_create_under_a_spent_key_is_refused_as_a_payload_mismatch() {
    let harness = Harness::new().await;
    let key = Uuid::now_v7().to_string();

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            })),
            &[("idempotency-key", &key)],
        ))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 20,
                "tax_basis": "inclusive",
                "target_plan_ids": [],
                "lines": [default_discount(2500)],
            })),
            &[("idempotency-key", &key)],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(
        problem_code(refused).await,
        "IDEMPOTENCY_PAYLOAD_MISMATCH",
        "a key held by one request must not author a second overlay for another"
    );

    // The refusal has to have refused a *write*, not only a status: the store
    // holds the first overlay and nothing else.
    let listed = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;
    let listed = body_json(listed).await;
    let overlays = listed["overlays"].as_array().expect("the list");
    assert_eq!(
        overlays.len(),
        1,
        "the refused create wrote nothing: {listed}"
    );
}

/// A save lands a **draft** and publishes nothing (`inst-pl-return`).
#[tokio::test]
async fn a_save_lands_a_draft_and_publishes_nothing() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let response = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let overlays = body["overlays"].as_array().expect("the list");
    assert_eq!(overlays.len(), 1);
    assert_eq!(
        overlays[0]["lifecycle_state"], "draft",
        "nothing publishes from a save"
    );
    assert_eq!(overlays[0]["price_overlay_id"], overlay.to_string());
    assert_eq!(
        overlays[0]["disclosure"], "restricted",
        "L6's fail-closed default"
    );
    assert_eq!(
        overlays[0]["scope_value"],
        serde_json::Value::Null,
        "the classless scope carries no value"
    );
}

/// **`disclosure` is authorable, and an unrecognized token is refused.**
///
/// The suite's only `disclosure` assertion read the server default `restricted`
/// (`a_save_lands_a_draft_and_publishes_nothing`), and until 2026-08-20 no case in
/// this file — or anywhere under `tests/` — *sent* the member on the create. So
/// L6's commercial-sensitivity switch had no wire coverage at all on the one
/// surface that sets it: a mapping that ignored `body.disclosure` and always wrote
/// `Restricted` (over-disclosing nothing, but silently discarding the operator's
/// `public`), and one that accepted an arbitrary token, both left the file green.
///
/// Both halves in one case, over one route: the accepted value read back off the
/// list, and the refusal for a token `Disclosure::parse` does not know.
#[tokio::test]
async fn a_public_disclosure_is_authored_and_read_back_and_a_bad_token_is_refused() {
    let harness = Harness::new().await;

    let created = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "disclosure": "public",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let overlay_id = body_json(created).await["price_overlay_id"]
        .as_str()
        .expect("the create names the overlay")
        .to_owned();

    let listed = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let body = body_json(listed).await;
    let overlays = body["overlays"].as_array().expect("the list");
    let authored = overlays
        .iter()
        .find(|row| row["price_overlay_id"] == serde_json::json!(overlay_id))
        .unwrap_or_else(|| panic!("the overlay just authored must be listed: {body}"));
    assert_eq!(
        authored["disclosure"], "public",
        "the authored value, not the fail-closed default: {body}"
    );

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 11,
                "tax_basis": "delegated_tariffs",
                "disclosure": "semi_public",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    // The refusal is a bare `InvalidRequest` and renders no per-violation code, so
    // its sentence is the discriminator — the same reading the scope- and
    // line-pairing cases below take.
    let problem = body_json(refused).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("disclosure `semi_public`")),
        "an unrecognized disclosure is named, not silently defaulted: {problem}"
    );
    let after = body_json(
        harness
            .allowed()
            .send(request("GET", PRICE_OVERLAYS, None))
            .await,
    )
    .await;
    assert_eq!(
        after["overlays"].as_array().expect("the list").len(),
        1,
        "and the refused create wrote nothing: {after}"
    );
}

/// **`TAX_BASIS_UNDECLARED` is reachable, and it is the discriminator.**
///
/// L5's *silence fails*. The field is `Option` on the wire precisely so this
/// code is reachable — a required field would be refused by the deserializer
/// with a message the design set does not own.
#[tokio::test]
async fn an_overlay_with_no_tax_basis_is_refused_by_its_own_code() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 10,
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "section 5's 422 is architectural; this platform renders it 400"
    );
    let body = body_json(response).await;
    assert!(
        violation_codes(&body).contains(&"TAX_BASIS_UNDECLARED".to_owned()),
        "the code must be the discriminator, not the message: {body}"
    );
}

/// The scope pairing is refused at the edge, both ways.
///
/// **Each body is held to its own sentence**, and the status alone was the whole
/// of this case until 2026-08-20 — so an unrelated validation firing first, or the
/// two arms collapsing into one generic refusal, left it green. `scope_of` raises
/// a bare `InvalidRequest` and therefore renders **no** per-violation code (this
/// file's `violation_codes` reads `context.violations`, which a parse refusal does
/// not carry — only the §5 rule report does), so the detail is the discriminator
/// here and not the code.
#[tokio::test]
async fn the_global_class_and_a_scope_value_are_refused_together() {
    let harness = Harness::new().await;

    for (body, expected) in [
        (
            serde_json::json!({
                "scope_class": "global",
                "scope_value": "everyone",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            }),
            "the global scope class carries no scope_value",
        ),
        (
            serde_json::json!({
                "scope_class": "brand",
                "precedence": 11,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            }),
            "a brand-scoped overlay must name a scope_value",
        ),
    ] {
        let response = harness
            .allowed()
            .send(with_headers(
                "POST",
                PRICE_OVERLAYS,
                Some(body),
                &[("idempotency-key", &Uuid::now_v7().to_string())],
            ))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let problem = body_json(response).await;
        assert!(
            problem["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(expected)),
            "the refusal must be `{expected}`, not another of this route's 400s: {problem}"
        );
    }
}

/// A `fixed` line declared `percent_bp` is refused (D-138), and a `targetSku`
/// line with no plan is too — both are pairings the domain cannot represent, so
/// the edge is where they are told apart from a well-formed request.
#[tokio::test]
async fn the_unrepresentable_line_pairings_are_refused_at_the_edge() {
    let harness = Harness::new().await;

    for (line, expected) in [
        (
            serde_json::json!({
                "adjustment_kind": "fixed",
                "magnitude_kind": "percent_bp",
                "adjustment_value": 5000,
            }),
            "a fixed line is always amount-based",
        ),
        (
            serde_json::json!({
                "target_sku": "sku-a",
                "adjustment_kind": "discount",
                "magnitude_kind": "percent_bp",
                "adjustment_value": 1000,
            }),
            "a target_sku line must name its plan_id",
        ),
        (
            serde_json::json!({
                "cohort": "2099-03-01T00:00:00Z",
                "adjustment_kind": "discount",
                "magnitude_kind": "percent_bp",
                "adjustment_value": 1000,
            }),
            "a cohort line must name its plan_id",
        ),
    ] {
        let response = harness
            .allowed()
            .send(with_headers(
                "POST",
                PRICE_OVERLAYS,
                Some(serde_json::json!({
                    "scope_class": "global",
                    "precedence": 10,
                    "tax_basis": "delegated_tariffs",
                    "target_plan_ids": [],
                    "lines": [line],
                })),
                &[("idempotency-key", &Uuid::now_v7().to_string())],
            ))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // **Each pairing held to its own sentence.** Three bodies shared one
        // status assertion until 2026-08-20, so any 400 from any other rule
        // satisfied all three -- and `line_of`/`adjustment_of` raise bare
        // `InvalidRequest`s that carry no per-violation code, so the detail is
        // what tells one unrepresentable pairing from another.
        let problem = body_json(response).await;
        assert!(
            problem["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(expected)),
            "the refusal must be `{expected}`, not another of this route's 400s: {problem}"
        );
    }
}

/// **D-67's range fails the SAVE, and it does so as a 400.**
///
/// `discount / percent_bp = 15000` is D-67's own "150% of list" data-entry
/// inversion. It is refused twice over — by
/// `chk_pricing_price_overlay_line_discount_ceiling` in the store and by
/// `check_authored_shape` at this edge — and the edge is what makes the refusal
/// legible: **this case answered 500 before that entry point existed**, because
/// the `CHECK` reached the caller as a driver error for a request whose whole
/// remedy is to correct one number.
#[tokio::test]
async fn an_out_of_range_magnitude_is_refused_at_the_save() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [default_discount(15_000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an out-of-range magnitude is the caller's mistake, not an internal fault"
    );
    let body = body_json(response).await;
    assert!(
        violation_codes(&body).contains(&"ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE".to_owned()),
        "got {body}"
    );
}

// ---------------------------------------------------------------------------
// The `PATCH` — the line set, wholesale, under the overlay's own tag.
// ---------------------------------------------------------------------------

/// The line set is replaced wholesale and the entity tag moves with it.
#[tokio::test]
async fn replacing_the_line_set_bumps_the_overlays_own_tag() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &overlay_path(overlay),
            Some(serde_json::json!({
                "revision": 0,
                "lines": [default_discount(2500), {
                    "adjustment_kind": "markup",
                    "magnitude_kind": "amount",
                    "amounts": [{ "currency": "EUR", "value_minor": 500 }],
                    "plan_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .expect("an entity tag"),
        "\"0-1\"",
        "the overlay revision's own tag, not a plan's"
    );
}

/// **One currency named twice in one line is refused, naming it.**
///
/// `CurrencyCode::new` upper-cases, so `USD` and `usd` are one key, and
/// `AmountSet::new` collects into a map — last wins. Without a refusal at this
/// door the two pairs collapse silently and that market is priced at the second
/// value, which is the one outcome an author cannot see: the request was
/// accepted, the read back shows one entry, and nothing says which of their two
/// numbers survived. The two sibling authored-proposal constructors,
/// `ThresholdVersion::new` and `MembershipMoveSet::new`, refuse exactly this
/// shape on exactly that ground.
///
/// The line is otherwise the one `replacing_the_line_set_bumps_the_overlays_own_tag`
/// gets a `200` for, so the duplicate is the only thing left for the route to
/// refuse.
#[tokio::test]
async fn one_currency_named_twice_in_a_line_is_refused_naming_it() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &overlay_path(overlay),
            Some(serde_json::json!({
                "revision": 0,
                "lines": [default_discount(2500), {
                    "adjustment_kind": "markup",
                    "magnitude_kind": "amount",
                    "amounts": [
                        { "currency": "USD", "value_minor": 100 },
                        { "currency": "usd", "value_minor": 9_999 },
                    ],
                    "plan_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "two values for one market is the author's mistake, and losing one of them silently is \
         not an answer"
    );
    let problem = body_json(response).await;
    // The currency, not just the status. `adjustment_of` raises a bare
    // `InvalidRequest` carrying no per-violation code, so the detail is the whole
    // of what tells this refusal from the route's other 400s -- and the author's
    // next move is to delete one of two entries, which they cannot do unless the
    // answer says which market.
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("USD")),
        "the refusal must name the currency the author repeated: {problem}"
    );
}

/// **The tag a `PATCH` needs is obtainable from a read**, which is the whole of why
/// `row_version` is on the view (review R-1).
///
/// Every other case in this file composes `If-Match` from what the test itself set
/// up — `"0-0"` written out by hand — and that is exactly how the gap survived. A
/// client cannot do that. It creates an overlay, and then at some point it reloads.
/// `GET /price-overlays/{id}` is the reload; this case pins the **list** half: a
/// caller paging the tenant can still compose the tag from `revision` and
/// `row_version` on the page, without a collection `ETag`.
///
/// This carried two names until 2026-08-20: a second case,
/// `the_if_match_tag_can_be_rebuilt_from_the_list_after_a_concurrent_edit`,
/// repeated it step for step — seed, `PATCH` under `"0-0"`, read the list, rebuild
/// `"{revision}-{row_version}"`, `PATCH` again and expect `200` — and differed only
/// in **omitting** the `row_version == 1` assertion below. Two names for one
/// behavioural check inflate a count without adding coverage, and a future edit to
/// one would have left the other silently asserting the same thing.
#[tokio::test]
async fn the_list_carries_the_token_the_next_edit_has_to_present() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    // Move the version off its initial value, so a test that merely guessed `0`
    // would pass for the wrong reason.
    let bump = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &overlay_path(overlay),
            Some(serde_json::json!({ "revision": 0, "lines": [default_discount(1500)] })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;
    assert_eq!(bump.status(), StatusCode::OK);

    // Now read it the way a client that has reloaded must.
    let listed = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let body = body_json(listed).await;
    let row = body["overlays"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["price_overlay_id"].as_str() == Some(&overlay.to_string()))
        })
        .cloned()
        .expect("the overlay just created is in the list");

    let revision = row["revision"]
        .as_u64()
        .expect("the list names the revision");
    let row_version = row["row_version"]
        .as_u64()
        .expect("the list names the concurrency token the next edit must present");
    assert_eq!(
        row_version, 1,
        "one edit has happened, so the token has moved once"
    );

    // The proof: that tag, and nothing the test knew by other means, is accepted.
    let edit = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &overlay_path(overlay),
            Some(serde_json::json!({ "revision": revision, "lines": [default_discount(2500)] })),
            &[(
                "if-match",
                Box::leak(format!("\"{revision}-{row_version}\"").into_boxed_str()),
            )],
        ))
        .await;
    assert_eq!(
        edit.status(),
        StatusCode::OK,
        "a tag composed from the list read is the tag the PATCH wants"
    );
}

/// A stale tag is a 409 and changes nothing.
#[tokio::test]
async fn a_stale_tag_is_refused_as_a_conflict() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let edit = |tag: &'static str| {
        with_headers(
            "PATCH",
            &overlay_path(overlay),
            Some(serde_json::json!({ "revision": 0, "lines": [default_discount(2500)] })),
            &[("if-match", tag)],
        )
    };
    assert_eq!(
        harness.allowed().send(edit("\"0-0\"")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        harness.allowed().send(edit("\"0-0\"")).await.status(),
        StatusCode::CONFLICT,
        "the second edit against the spent tag is a conflict"
    );
}

// ---------------------------------------------------------------------------
// The submit, and the status split §5 types.
// ---------------------------------------------------------------------------

/// A clean overlay submits **202** and is always material (D-50).
#[tokio::test]
async fn a_clean_submit_is_accepted_and_always_material() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(
        body["materiality"], "alwaysMaterialTrigger",
        "an overlay line has no per-currency baseline to threshold (D-50, G1)"
    );
    assert_eq!(
        body["warnings"].as_array().map(Vec::len),
        Some(0),
        "a clean overlay raises no advisory either"
    );
}

/// **The 422 family: one aggregate 400 carrying every code.**
///
/// This is what makes an overlay remediable in one pass rather than in as many
/// round trips as it has faults (Foundation §4.2).
#[tokio::test]
async fn the_architectural_422s_arrive_as_one_aggregate_400() {
    let harness = Harness::new().await;
    // A brand overlay whose value no taxonomy declares, with a line naming a
    // plan outside its (empty) target_ref. The magnitude is deliberately **in
    // range**: D-67's bound is refused at the save (see
    // `an_out_of_range_magnitude_is_refused_at_the_save`), so an out-of-range
    // line would never reach the submit this case is about.
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "brand",
                "scope_value": "nobody-declared-this",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [{
                    "plan_id": Uuid::now_v7(),
                    "adjustment_kind": "discount",
                    "magnitude_kind": "percent_bp",
                    "adjustment_value": 1_500,
                }],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "a save runs no world-dependent rule"
    );
    let overlay = body_json(response).await["price_overlay_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("the created overlay");

    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the architectural 422 family renders 400"
    );
    let body = body_json(response).await;
    let codes = violation_codes(&body);
    for expected in ["SCOPE_VALUE_UNKNOWN", "OVERLAY_LINE_TARGET_UNKNOWN"] {
        assert!(
            codes.contains(&expected.to_owned()),
            "{expected} must be in the one-pass report, got {codes:?} from {body}"
        );
    }
}

/// **The 409 family: lifted out of the envelope, because §5 types it 409.**
///
/// A caller told two sibling overlays hold a precedence acts by re-reading, not
/// by editing a field of their own request — which is what a 409 asks for and a
/// 400 would not.
#[tokio::test]
async fn a_duplicate_precedence_is_a_conflict_and_not_a_validation_envelope() {
    let harness = Harness::new().await;
    let first = seed_overlay(&harness, 10).await;
    harness
        .state
        .overlays
        .publish_revision(
            &harness.scope(),
            harness.tenant,
            first,
            0,
            rest_support::seed_stamp(),
        )
        .await
        .expect("the first overlay publishes at precedence 10");

    let rival = seed_overlay(&harness, 10).await;
    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(rival),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "section 5 types PRECEDENCE_DUPLICATE 409 outright"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["context"]["reason"], "PRECEDENCE_DUPLICATE",
        "the code is the discriminator: {body}"
    );
}

/// **A `fixed` line over a lower layer is not itself a refusal.**
///
/// Named for what the body proves, not for what the surface owes. This case was
/// called `a_fixed_line_over_a_lower_layer_warns_on_the_succeeding_path` until
/// 2026-08-20 and it never reached a succeeding path or observed a warning — its
/// own comment at the foot said so, and what it actually asserts is a 400 carrying
/// `SCOPE_VALUE_UNKNOWN`. A name asserting a property the body cannot see reads as
/// covered in any census of test names, which is how the uncovered case below
/// stopped being visible.
///
/// D-138's warning rides the succeeding path, which is the only place it can be
/// advisory: the `ValidationFailed` envelope exists on the rejecting path only, so
/// a warning carried only there would be computed and discarded — the defect D-197
/// records for the plan plane, not repeated here. **That is the claim this file
/// still does not test**, and the foot of this function records why.
#[tokio::test]
async fn a_fixed_line_over_a_lower_layer_is_not_itself_the_refusal() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();

    // A published `global` overlay at precedence 5, carrying a list-default line
    // — the layer a higher-precedence `fixed` would discard.
    let lower = seed_overlay(&harness, 5).await;
    harness
        .state
        .overlays
        .publish_revision(
            &harness.scope(),
            harness.tenant,
            lower,
            0,
            rest_support::seed_stamp(),
        )
        .await
        .expect("the lower layer publishes");

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "region",
                "scope_value": "eu-west",
                "precedence": 20,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [plan],
                "lines": [{
                    "plan_id": plan,
                    "adjustment_kind": "fixed",
                    "magnitude_kind": "amount",
                    "amounts": [{ "currency": "EUR", "value_minor": 5000 }],
                }],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let overlay = body_json(response).await["price_overlay_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("the created overlay");

    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;

    // **This case does NOT observe the warning, and says so rather than claiming
    // to.** The submit is refused — the region value is undeclared and the plan is
    // unpublished — and the `ValidationFailed` envelope carries `violations`
    // only, so `warnings` is dropped on the rejecting path by construction.
    //
    // What it does prove is that a `fixed` line over a lower layer is **not**
    // itself a refusal. The warning is observed non-empty by
    // `domain::overlay_rules::overlay_rules_tests::a_fixed_line_over_a_lower_layer_warns_and_does_not_block`,
    // and the 202's `warnings` field is observed empty by
    // `a_clean_submit_is_accepted_and_always_material`. What no test here reaches
    // is a **non-empty** `warnings` on a 202 — that needs a published plan whose
    // lower-precedence overlay matches it, and this gear mounts no plan-publish
    // route for the harness to drive. Recorded rather than left as a gap a reader
    // would assume covered.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        violation_codes(&body_json(response).await).contains(&"SCOPE_VALUE_UNKNOWN".to_owned()),
        "the refusal must be the world's, not the fixed line's"
    );
}

/// **A duplicate line key is a 400 naming `OVERLAY_LINE_DUPLICATE`, not a 500.**
///
/// D-42's *"one default line"*: two of them collide on the store's null-safe
/// index, which used to reach the caller as a driver error. The save is also the
/// only place the code can fire — the store refusing the duplicate here is what
/// made the `check_lines` arm unreachable at submit.
#[tokio::test]
async fn a_duplicate_line_key_is_refused_at_the_save() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "target_plan_ids": [],
                "lines": [default_discount(1000), default_discount(2000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert!(
        violation_codes(&body).contains(&"OVERLAY_LINE_DUPLICATE".to_owned()),
        "got {body}"
    );
}

/// **An inverted interval is a 400, not a 500** — §1.7's "effective-interval
/// sanity", which had no implementation and reached `chk_pricing_price_overlay_interval`
/// as a driver error.
#[tokio::test]
async fn an_inverted_effective_interval_is_refused_at_the_save() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            PRICE_OVERLAYS,
            Some(serde_json::json!({
                "scope_class": "global",
                "precedence": 10,
                "tax_basis": "delegated_tariffs",
                "effective_from": "2099-06-01T00:00:00Z",
                "effective_to": "2099-01-01T00:00:00Z",
                "target_plan_ids": [],
                "lines": [default_discount(1000)],
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert!(
        violation_codes(&body).contains(&"OVERLAY_INTERVAL_INVALID".to_owned()),
        "got {body}"
    );
}

/// **A `PATCH` whose body and `If-Match` name different revisions is refused.**
///
/// The store is addressed by the tag, so accepting the mismatch would rewrite one
/// revision and report another — and a client that then submitted the revision it
/// was handed would submit a revision it never edited.
#[tokio::test]
async fn a_patch_whose_body_and_tag_disagree_about_the_revision_is_refused() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &overlay_path(overlay),
            Some(serde_json::json!({ "revision": 7, "lines": [default_discount(2500)] })),
            &[("if-match", "\"0-0\"")],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The status alone is not this case's property: a bare `InvalidRequest` --
    // one carrying no per-violation code -- is also what a malformed `If-Match`
    // and every refusal `lines_of` raises answer with, and the body carries a line
    // set. Either firing first satisfies a 400, so the mismatch the case is named
    // for needs the sentence the handler renders for it.
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("the body names revision 7")
                && detail.contains("If-Match names revision 0")),
        "the refusal must name both revisions, not another of this route's 400s: {problem}"
    );
}

/// **Only an open draft is submittable.**
///
/// §5's row is "Submit **the draft**" and `inst-pl-commit` pins the approval unit
/// to one. Submitting a published revision would open a second always-material
/// unit over content that is already live — and `overlay_facts` skips the
/// candidate's own overlay (D-107), so a live revision would validate against a
/// world told to ignore it.
#[tokio::test]
async fn a_published_revision_is_not_submittable() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;
    harness
        .state
        .overlays
        .publish_revision(
            &harness.scope(),
            harness.tenant,
            overlay,
            0,
            rest_support::seed_stamp(),
        )
        .await
        .expect("revision 0 publishes");

    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;

    let response_status = response.status();
    let body = body_json(response).await;
    assert_eq!(
        response_status,
        StatusCode::BAD_REQUEST,
        "a published revision is not a draft, and only a draft is submittable: {body}"
    );
    // The status is shared with this surface's whole architectural-422 family,
    // which `the_architectural_422s_arrive_as_one_aggregate_400` renders on the
    // same 400 — so the code is what says the lifecycle guard is the one that
    // answered.
    assert_eq!(code_in(&body), "LIFECYCLE_FORBIDDEN", "{body}");
}

/// The list read is the operator's and shows a `restricted` overlay (§3 step 7).
#[tokio::test]
async fn the_list_shows_restricted_overlays_to_their_operator() {
    let harness = Harness::new().await;
    seed_overlay(&harness, 10).await;
    seed_overlay(&harness, 20).await;

    let response = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{PRICE_OVERLAYS}?$filter=scope_class%20eq%20'global'"),
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["overlays"].as_array().map(Vec::len), Some(2));

    let response = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{PRICE_OVERLAYS}?$filter=scope_class%20eq%20'partner'"),
            None,
        ))
        .await;
    let body = body_json(response).await;
    assert_eq!(body["overlays"].as_array().map(Vec::len), Some(0));
}

/// Two fresh overlays share `(lifecycle_state, revision)` — both `draft` at
/// revision 1 — so under `$orderby=lifecycle_state` the tiebreaker `revision`
/// alone leaves them on one keyset value and page 2 starts past both.
/// `query_with_unique_order` names `price_overlay_id`; this walks the boundary
/// it repairs, one row a page.
#[tokio::test]
async fn a_one_row_walk_under_a_colliding_orderby_visits_every_overlay() {
    let harness = Harness::new().await;
    let mut expected = vec![
        seed_overlay(&harness, 10).await.to_string(),
        seed_overlay(&harness, 20).await.to_string(),
    ];
    expected.sort();

    let mut walked: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..=expected.len() {
        let query = match cursor.as_deref() {
            None => "$orderby=lifecycle_state&limit=1".to_owned(),
            Some(token) => format!("limit=1&cursor={token}"),
        };
        let response = harness
            .allowed()
            .send(request("GET", &format!("{PRICE_OVERLAYS}?{query}"), None))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{query}");
        let page = body_json(response).await;
        let ids = page["overlays"].as_array().expect("a page");
        assert_eq!(ids.len(), 1, "one row a page: {page}");
        walked.extend(
            ids.iter()
                .map(|o| o["price_overlay_id"].as_str().expect("an id").to_owned()),
        );
        match page["page_info"]["next_cursor"].as_str() {
            None => break,
            Some(token) => cursor = Some(token.to_owned()),
        }
    }

    let mut seen = walked.clone();
    seen.sort();
    assert_eq!(
        seen, expected,
        "the walk must visit both overlays, neither twice: {walked:?}"
    );
}

/// Retired named keys and extra query keys are 400, pointing at `$filter`.
#[tokio::test]
async fn a_named_overlay_filter_key_is_refused() {
    let harness = Harness::new().await;
    seed_overlay(&harness, 10).await;

    for query in ["?scope_class=global", "?status=draft"] {
        let response = harness
            .allowed()
            .send(request("GET", &format!("{PRICE_OVERLAYS}{query}"), None))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let body = body_json(response).await;
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("$filter"),
            "{query} names `$filter` as the remedy: {body}"
        );
    }
}

/// **The submit opens the approval unit `inst-pl-commit` promises, and names it.**
///
/// D-225: until 2026-08-06 the route validated, answered 202 and opened **nothing** —
/// so the status was honest about the act and the act was incomplete. A consumer
/// reading `202 Accepted` with `materiality: alwaysMaterialTrigger` was being told a
/// two-person workflow had started, and none had; nothing held the overlay while an
/// approver looked at it, and nothing could ever publish it.
///
/// What the wire has to carry is the unit's **id**, because a submitter cannot chase
/// a review they cannot name — that is what `approvals` already gives every other
/// always-material act in this gear.
#[tokio::test]
async fn a_submit_opens_the_approval_unit_and_names_it_on_the_wire() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    let approval_id = body["approval"]["approval_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .unwrap_or_else(|| panic!("the submit must name the unit it opened: {body}"));

    // The unit exists, it is pending, and it is over **this overlay revision** —
    // not over the overlay, which would let a pin taken on revision 3 authorize a
    // decision about revision 4.
    let record = harness
        .read_approval(approval_id)
        .await
        .unwrap_or_else(|| panic!("approval {approval_id} must be readable"));
    assert_eq!(record.subject_kind.as_str(), "overlay");
    assert_eq!(record.subject_ref, format!("{overlay}/0"));
    assert_eq!(record.state.as_str(), "submitted");
}

/// **The two planes name one overlay act identically** (Z8-6, D-158).
///
/// `audit_repo::overlay_revision_ref`'s own doc calls itself *"one overlay
/// revision's durable **audit and approval** name"*, and it reached one of the two:
/// the approval plane called it, and `overlay_repo::record_overlay_mutation`
/// hand-wrote a bare `price_overlay_id` instead. So the record of the act and the
/// unit authorizing it named their subject two different ways, and a walk that
/// joined the planes on `subject_ref` — the join an auditor read has to make —
/// found nothing. The revision was not *lost*, it rode in `after_state`; it was
/// simply not in the name, which is the only place a join can look.
///
/// The same divergence was corrected for windows (`audit_repo::window_ref`), whose
/// doc states the rule this asserts: *"it keeps both stores on one spelling: the
/// audit record and the approval record of one act name it identically"*.
///
/// **Asserted as an equality between the two stores**, not as a format. A test
/// that pinned `format!("{overlay}/0")` on each side separately would pass over
/// two planes that had drifted to a third spelling together, and would say nothing
/// about the join. The literal shape is pinned once, beside it, so the equality
/// cannot be satisfied by both sides being empty.
#[tokio::test]
async fn the_audit_record_and_the_approval_record_of_one_overlay_act_name_it_identically() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    let approval_id = body["approval"]["approval_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .unwrap_or_else(|| panic!("the submit must name the unit it opened: {body}"));
    let approval = harness
        .read_approval(approval_id)
        .await
        .unwrap_or_else(|| panic!("approval {approval_id} must be readable"));

    // The **create's** record, on the overlay's segment: the mutation
    // `overlay_repo` wrote, as against the `submit` record the approval plane
    // wrote one seq later on the same chain. Both are about revision 0 of one
    // overlay, and until Z8-6 was paid they named it two different ways *inside a
    // single segment* — which is why picking the create by its action matters and
    // taking "the record" would not.
    let chain = bss_pricing::infra::storage::repo::audit_repo::overlay_chain(overlay);
    let records: Vec<_> = rest_support::audit_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.chain_id == chain)
        .collect();
    let created = records
        .iter()
        .find(|row| row.action == "create")
        .unwrap_or_else(|| panic!("the save's own record: {records:?}"));
    assert_eq!(created.subject_kind, "overlay");

    assert_eq!(
        created.subject_ref, approval.subject_ref,
        "the audit record and the approval record of one overlay revision must name it \
         identically, or a walk joining the planes on `subject_ref` finds nothing"
    );
    // And the one spelling both use is the helper's, rather than a third the two
    // planes happened to agree on.
    assert_eq!(
        created.subject_ref,
        bss_pricing::infra::storage::repo::audit_repo::overlay_revision_ref(overlay, 0),
    );
    // The positive control the equality needs: the plane that was already right
    // did not move to meet the one that was wrong.
    let submitted = records
        .iter()
        .find(|row| row.action == "submit")
        .unwrap_or_else(|| panic!("the submit's own record: {records:?}"));
    assert_eq!(
        submitted.subject_ref,
        bss_pricing::infra::storage::repo::audit_repo::overlay_revision_ref(overlay, 0),
    );
}

/// **One pending unit per overlay revision** — a second submit is refused, not
/// silently duplicated.
///
/// `inst-co-single-pending`'s posture for a subject that holds **no canonical scope
/// key**: that rule binds units whose change set touches a key, and an overlay's
/// touches none, so the guard here is the subject ref rather than the held-key
/// register. Two pending units over one revision would leave two reviewers approving
/// two contents whose order of arrival decides the overlay.
#[tokio::test]
async fn a_second_submit_of_one_revision_is_refused_while_the_first_is_pending() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;
    let body = serde_json::json!({ "revision": 0 });

    let first = harness
        .allowed()
        .send(request("POST", &submit_path(overlay), Some(body.clone())))
        .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);

    let second = harness
        .allowed()
        .send(request("POST", &submit_path(overlay), Some(body)))
        .await;

    let status = second.status();
    let rendered = body_json(second).await.to_string();
    assert_eq!(status, StatusCode::CONFLICT, "{rendered}");
    assert!(
        rendered.contains("PENDING_CHANGE_UNIT_EXISTS"),
        "the refusal names the rule rather than a generic conflict: {rendered}"
    );
}

/// **An opened overlay unit can be decided**, which means its pin re-derives.
///
/// The half a submit is worthless without: `re_derive` re-reads the subject under the
/// same encoding the pin was taken with, and an approve compares the two. Its overlay
/// arm refused outright while nothing opened such a unit — correctly then, and a
/// false claim the moment `submit_overlay` existed — so every overlay unit would have
/// been un-approvable, which is a unit that can be opened and never decided.
///
/// This crate has had that exact defect **twice**, on `price_unit` and on `window`,
/// and both times it was invisible until something actually decided one: the arm
/// resolved the wrong revision, `content_matches_pin` answered `false`, and the
/// decision returned `APPROVAL_CONTENT_MISMATCH` for a subject nobody had touched.
#[tokio::test]
async fn an_overlay_unit_can_be_approved_because_its_pin_re_derives() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    let opened = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;
    assert_eq!(opened.status(), StatusCode::ACCEPTED);
    let approval_id = body_json(opened).await["approval"]["approval_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the unit is named");

    harness
        .governance
        .approvals
        .decide(
            &harness.scope(),
            harness.tenant,
            bss_pricing::infra::approval::DecideRequest {
                approval_id,
                decision: bss_pricing::domain::approval::DecisionBy::Approve(REVIEWER),
                reason: None,
                approver_regions: bss_pricing::infra::approval::RegionGrant::Untransported,
                stamp: bss_pricing::domain::audit::AuditStamp {
                    actor_principal_id: REVIEWER,
                    recorded_at: OffsetDateTime::now_utc(),
                    correlation_id: Uuid::from_u128(0x_9d_c0),
                },
                withdraw_authority: bss_pricing::domain::approval::WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the reviewer approves an overlay unit");

    let record = harness
        .read_approval(approval_id)
        .await
        .expect("the unit reads back");
    assert_eq!(record.state.as_str(), "approved");
}

/// The second principal — never the submitter, per `inst-tp-distinct`.
const REVIEWER: Uuid = Uuid::from_u128(0x_9d_22);

/// **A rejected unit leaves the overlay exactly as it was, and it re-submits.**
///
/// `inst-as-reject` promises a rejected non-plan subject returns to *"its slice-defined
/// pre-submit state"*. For an overlay that costs **nothing**, and the reason is worth
/// asserting rather than assuming: the submit writes to `pricing_approval` and to
/// nothing else — it does not flip the revision's lifecycle, does not stage a copy and
/// does not touch a line — so the pre-submit state *is* the current state and there is
/// no compensating write for a rejection to make.
///
/// That is a property of this slice's design and not a general truth: the plan plane's
/// submit has a draft to return, and Slice 7 parks a cutover's whole three-operation
/// payload in the approval record precisely because there is nowhere else to hold it.
/// Asserted here so a later change that gives the submit a side effect fails this case
/// rather than silently making `inst-as-reject` unimplemented.
#[tokio::test]
async fn a_rejected_unit_leaves_the_overlay_untouched_and_it_submits_again() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;
    let body = serde_json::json!({ "revision": 0 });

    let before = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;
    let before = body_json(before).await;

    let opened = harness
        .allowed()
        .send(request("POST", &submit_path(overlay), Some(body.clone())))
        .await;
    let approval_id = body_json(opened).await["approval"]["approval_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("the unit is named");

    harness
        .governance
        .approvals
        .decide(
            &harness.scope(),
            harness.tenant,
            bss_pricing::infra::approval::DecideRequest {
                approval_id,
                decision: bss_pricing::domain::approval::DecisionBy::Reject(REVIEWER),
                reason: Some("not this quarter".to_owned()),
                approver_regions: bss_pricing::infra::approval::RegionGrant::Untransported,
                stamp: bss_pricing::domain::audit::AuditStamp {
                    actor_principal_id: REVIEWER,
                    recorded_at: OffsetDateTime::now_utc(),
                    correlation_id: Uuid::from_u128(0x_9d_c1),
                },
                withdraw_authority: bss_pricing::domain::approval::WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the reviewer rejects");

    let after = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;
    assert_eq!(
        body_json(after).await,
        before,
        "a rejection writes nothing to the overlay plane"
    );

    // And the key freed: the author fixes the objection and submits the same revision
    // again. A rejection that left the subject un-submittable would be a decision the
    // operator cannot act on.
    let again = harness
        .allowed()
        .send(request("POST", &submit_path(overlay), Some(body)))
        .await;
    assert_eq!(again.status(), StatusCode::ACCEPTED);
}

// ---------------------------------------------------------------------------
// The second act: publish (D-234).
// ---------------------------------------------------------------------------

/// A second principal approves the unit, through the service.
///
/// Not through `POST …/approvals/{id}/approve`: this case is about the **publish
/// arm**, and routing the decision as well would make a failure ambiguous between
/// the two surfaces. The approval route has its own suite.
///
/// The *decision* it hands the service is the route's, though — tenant-scoped and
/// `Untransported` — because a helper that bypasses the surface still has to
/// supply the operands the surface supplies, or the arm under test is certified
/// against a shape no caller can send.
async fn approve(harness: &Harness, approval_id: &str) {
    let approver = Uuid::from_u128(0xa9_9a);
    harness
        .governance
        .approvals
        .decide(
            &harness.scope(),
            harness.tenant,
            bss_pricing::infra::approval::DecideRequest {
                approval_id: Uuid::parse_str(approval_id).expect("an approval id"),
                decision: bss_pricing::domain::approval::DecisionBy::Approve(approver),
                reason: None,
                // What the mounted route decides with:
                // `api::rest::approvals::region_grant_of_this_surface` is a `const
                // fn` answering `Untransported` and nothing else, so no request
                // reaching this gear carries an `Explicit` grant -- the variant is
                // read in production (`infra::approval` maps it to
                // `ApproverReach::Granted`) but nothing on this path produces one.
                // An empty explicit grant is besides the *narrowest* operand
                // `approver_reach` can be given, the opposite of the route's, so a
                // region-scope rule added to the decide path would be judged here
                // against an input no caller can send. The file's two other
                // `decide` calls already pass this pair.
                approver_regions: bss_pricing::infra::approval::RegionGrant::Untransported,
                stamp: bss_pricing::domain::audit::AuditStamp {
                    actor_principal_id: approver,
                    recorded_at: OffsetDateTime::now_utc(),
                    correlation_id: Uuid::now_v7(),
                },
                withdraw_authority: bss_pricing::domain::approval::WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("a second principal approves the unit");
}

/// The submit route's two acts, driven through the real edge.
///
/// S9 §5 spells this route as *"Submit the draft — always-material Slice 5
/// approval unit (D-50), **then the D-06 publish unit**"*, so the two acts are one
/// route by the design set's own arrangement.
///
/// The first call opens the unit and answers **202**. The second, once a second
/// principal has approved *this content*, commits and answers **200** carrying the
/// registry's pending handle. Matching on the content and not merely on the
/// subject is what keeps the route out of a dead end: an approval whose subject
/// moved after the decision covers content that no longer exists.
#[tokio::test]
async fn an_approved_overlay_publishes_on_the_second_call_to_the_same_route() {
    let harness = Harness::new().await;
    let overlay = seed_overlay(&harness, 10).await;

    // Act one: the submit.
    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let opened = body_json(response).await;
    assert_eq!(opened["outcome"], "submitted_for_approval");
    assert_eq!(opened["pending_version_ref"], serde_json::Value::Null);
    let approval_id = opened["approval"]["approval_id"]
        .as_str()
        .expect("the unit is named on the wire (D-225)")
        .to_owned();

    // A second principal decides it.
    approve(&harness, &approval_id).await;

    // Act two: the same route, the same body, and now it publishes.
    let response = harness
        .allowed()
        .send(request(
            "POST",
            &submit_path(overlay),
            Some(serde_json::json!({ "revision": 0 })),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let published = body_json(response).await;
    assert_eq!(published["outcome"], "published");
    assert!(
        published["pending_version_ref"].is_string(),
        "the receipt names the registry handle, which is not a CatalogVersion: {published}"
    );
}

// ---------------------------------------------------------------------------
// D-125's collection contract on the overlay list.
//
// The decision is a Foundation convention "inherited by every slice surface":
// every collection GET returns pages -- `limit` (default 100, hard cap 1,000)
// plus an opaque `cursor`, with `next_cursor` until the result is exhausted.
// `api::rest::cursor` says the same in its own words, "decided once for every
// list surface this gear serves".
//
// This surface did not serve it, and nothing here noticed for a simple reason:
// every case in this file was written from the handler, so it asserted the shape
// the handler produced. The gap surfaced from the other side -- an e2e suite
// authored against the design set rather than against this code.
// ---------------------------------------------------------------------------

/// Seed `n` overlays on distinct precedences, returning their ids in id order.
async fn seed_overlays(harness: &Harness, n: usize) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        ids.push(seed_overlay(harness, 1_000 + i32::try_from(i).expect("small")).await);
    }
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn the_list_answers_a_page_envelope_with_a_cursor() {
    // The contract's shape. A bare array is the pre-D-125 answer, and a caller
    // handed one has no way to ask for the next page at all.
    let harness = Harness::new().await;
    seed_overlays(&harness, 3).await;

    let response = harness
        .allowed()
        .send(request("GET", PRICE_OVERLAYS, None))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert!(
        body.get("page_info").is_some(),
        "D-125 requires a page envelope on every collection GET: {body}"
    );
    assert_eq!(
        body["page_info"]["limit"],
        serde_json::json!(100),
        "the server default is 100 (D-125)"
    );
}

#[tokio::test]
async fn a_limit_bounds_the_page_and_names_where_to_resume() {
    // Two rows asked for out of three: the page carries two and says there is
    // more. A surface that ignored `limit` would return all three and a client
    // walking it would never terminate a page loop.
    let harness = Harness::new().await;
    let ids = seed_overlays(&harness, 3).await;

    let response = harness
        .allowed()
        .send(request("GET", &format!("{PRICE_OVERLAYS}?limit=2"), None))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let rows = body["overlays"].as_array().expect("the list");
    assert_eq!(rows.len(), 2, "`limit` bounds the page: {body}");
    assert!(
        body["page_info"]["next_cursor"].is_string(),
        "a page with more behind it must name where to resume: {body}"
    );

    // And the walk resumes strictly after the row the cursor names.
    let cursor = body["page_info"]["next_cursor"].as_str().expect("token");
    let second = harness
        .allowed()
        .send(request(
            "GET",
            &format!("{PRICE_OVERLAYS}?limit=2&cursor={cursor}"),
            None,
        ))
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second = body_json(second).await;
    let tail = second["overlays"].as_array().expect("the list");
    assert_eq!(
        tail.len(),
        1,
        "the last page carries the remainder: {second}"
    );
    assert_eq!(
        tail[0]["price_overlay_id"],
        serde_json::json!(ids[2].to_string()),
        "the walk resumes strictly after the cursor, in key order"
    );
    assert!(
        second["page_info"]["next_cursor"].is_null(),
        "an exhausted walk says so rather than pointing at an empty page: {second}"
    );
}
