//! The bundle plane, driven through the real router.
//!
//! Every refusal is asserted by its **code** rather than only its status, which
//! is the habit `rest_plans.rs` explains and which matters more here than
//! anywhere else in this gear: §5 types all ten composition refusals `422`, the
//! platform has no 422 at all, and every one of them therefore arrives as a
//! `400`. A test asserting the status would be asserting nothing — it is the
//! code string that tells `CURRENCY_NOT_COVERED` from `FREQUENCY_MISMATCH`.
//!
//! The publish cases drive the whole pipeline end to end: a composition is
//! authored through the `PATCH`, judged by the rules over rows this suite seeds
//! into the store, and — when it passes — normalized onto its absorber with
//! `BundleUpdated` in the same transaction.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::bundles::BUNDLES;
use rest_support::{
    Harness, approval_row, approval_rows, body_json, etag_of, location_of, problem_code,
    seed_current_plan, seed_draft_plan, seed_foreign_current_plan, seed_foreign_plan, seed_price,
    with_headers,
};
use time::OffsetDateTime;
use uuid::Uuid;

fn bundle_path(bundle_id: Uuid) -> String {
    format!("{BUNDLES}/{bundle_id}")
}

fn publish_path(bundle_id: Uuid) -> String {
    format!("{BUNDLES}/{bundle_id}/publish")
}

/// A problem document with one id struck out of it, so two answers about
/// different ids can be compared for everything **except** the id.
async fn redacting(response: axum::http::Response<axum::body::Body>, id: Uuid) -> String {
    body_json(response)
        .await
        .to_string()
        .replace(&id.to_string(), "<id>")
}

/// Create a bundle on a fresh draft plan and hand back both ids.
async fn seed_bundle(harness: &Harness) -> (Uuid, Uuid) {
    seed_bundle_with(harness, "sum_of_parts").await
}

/// The same, on a named basis.
async fn seed_bundle_with(harness: &Harness, basis: &str) -> (Uuid, Uuid) {
    let plan_id = Uuid::now_v7();
    seed_draft_plan(harness, plan_id).await;
    harness.attach_shape(plan_id, 0).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "price_basis": basis,
                "invoice_itemization": "itemize",
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    let bundle_id = body["bundle_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
        .expect("the response must name the bundle it created");
    (plan_id, bundle_id)
}

// ---------------------------------------------------------------------------
// `inst-ba-author` — creating the bundle.
// ---------------------------------------------------------------------------

/// The create's own promises: the `Location` §3 requires, the body echoing what
/// was authored, and the row reading back on `GET /bundles/{bundleId}`.
///
/// **The whole body of this case was `assert_ne!(bundle_id, Uuid::nil())` and
/// `assert_ne!(plan_id, Uuid::nil())` until 2026-08-20** — where `plan_id` was a
/// `Uuid::now_v7()` the fixture itself had just minted and `bundle_id` had already
/// been parsed out of a body `seed_bundle` asserted was a `201`. Neither could fail
/// for any behaviour of the route, so the file's first case measured nothing beyond
/// what its own helper already asserted.
#[tokio::test]
async fn a_bundle_is_created_on_its_plan() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    harness.attach_shape(plan_id, 0).await;

    let created = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "price_basis": "sum_of_parts",
                "invoice_itemization": "itemize",
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(created.status(), StatusCode::CREATED);
    let location = location_of(&created).expect("a performed create carries its Location");
    let view = body_json(created).await;
    let bundle_id = view["bundle_id"]
        .as_str()
        .and_then(|raw| raw.parse::<Uuid>().ok())
        .unwrap_or_else(|| panic!("the response names the bundle it created: {view}"));
    assert_eq!(
        location,
        format!("/bss-pricing/v1/bundles/{bundle_id}"),
        "the Location names the resource the body names"
    );
    assert_eq!(
        view["plan_id"],
        plan_id.to_string(),
        "the bundle is created **on its plan**, which is this case's name: {view}"
    );
    assert_eq!(view["price_basis"], "sum_of_parts", "{view}");
    assert_eq!(view["invoice_itemization"], "itemize", "{view}");

    // And it is a row, not a rendered response: the id the `Location` points at
    // resolves.
    let read = harness
        .allowed()
        .send(with_headers("GET", &bundle_path(bundle_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let stored = body_json(read).await;
    assert_eq!(stored["bundle_id"], bundle_id.to_string(), "{stored}");
    assert_eq!(stored["plan_id"], plan_id.to_string(), "{stored}");
    assert_eq!(stored["price_basis"], "sum_of_parts", "{stored}");
    assert_eq!(stored["invoice_itemization"], "itemize", "{stored}");
}

/// A `plan_id` in another tenant reads exactly like one that does not exist, and
/// the plan's own tenant can still bundle it afterwards — A1-2.
///
/// This is the second route in the gear that takes a plan id from a **caller** and
/// creates something out of it; `POST /plans/{planId}/clone` is the first, and
/// `rest_plans`' `a_foreign_tenants_plan_cannot_be_cloned_and_reads_like_an_absent_one`
/// is this case's sibling. Nothing between the wire and the insert read the plan
/// here: the handler gated, parsed the body, claimed the idempotency key, parsed
/// two tokens and went straight into `bundle_repo::create_on`, whose only read
/// looks for *a bundle*. So the answer to "does this foreign plan have a bundle"
/// came out of `uq_pricing_bundle_plan`, which carried no tenant — `409` when it
/// did and `201` when it did not, and in the `201` case the row **landed** and
/// took the owner's slot for good.
///
/// The last third of the case is the half a fix to the read alone leaves standing:
/// the owning tenant must still be able to create its own bundle. Asserted as a
/// `201` rather than as "not a 409", because the wire code is what distinguishes
/// the two failures — `BUNDLE_EXISTS_ON_PLAN` against a row it cannot read is
/// precisely the state that has no remedy through this API.
#[tokio::test]
async fn a_bundle_on_a_foreign_tenants_plan_is_a_404_and_leaves_the_plan_bundleable() {
    let harness = Harness::new().await;
    let foreign = Uuid::now_v7();
    let absent = Uuid::now_v7();
    seed_foreign_plan(&harness, foreign).await;

    let foreign_answer = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({
                "plan_id": foreign,
                "price_basis": "sum_of_parts",
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    let absent_answer = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({
                "plan_id": absent,
                "price_basis": "sum_of_parts",
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(foreign_answer.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent_answer.status(), StatusCode::NOT_FOUND);

    // **The whole document, not just the status.** Each answer echoes the id its
    // own caller supplied and nothing else, so the two are byte-identical once
    // that id is taken out — which is what makes the 404 uninformative about
    // whether the foreign plan exists. Comparing the raw bodies would compare two
    // uuids this test itself chose and would pass against any pair of documents
    // that differed anywhere.
    let foreign_document = redacting(foreign_answer, foreign).await;
    let absent_document = redacting(absent_answer, absent).await;
    assert!(
        foreign_document.contains("<id>"),
        "the substitution must have found the id it removes, or the comparison below proves \
         nothing: {foreign_document}"
    );
    assert_eq!(
        foreign_document, absent_document,
        "a foreign plan and an absent one must answer the same document"
    );

    // The tenant that owns the plan is unobstructed. Under the old index this is
    // what the squatted row would have taken away, permanently.
    let owner = harness
        .other_tenant()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({
                "plan_id": foreign,
                "price_basis": "sum_of_parts",
            })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::CREATED,
        "the plan's own tenant must still be able to bundle it"
    );
}

/// `inst-bb-declared`: the basis MUST be declared, and `BASIS_MISSING` is what
/// an absent one is told.
///
/// The field is `Option` on the wire precisely so this code is reachable — a
/// required field would be refused by the deserializer with a message the design
/// set does not own.
#[tokio::test]
async fn a_bundle_with_no_declared_basis_is_refused_by_code() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({ "plan_id": plan_id })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let detail = body_json(response).await.to_string();
    assert!(
        detail.contains("BASIS_MISSING"),
        "the refusal must carry the code the design set declares, got: {detail}"
    );
}

/// **A retried create is replayed, not re-executed** (Z12-7, `inst-bk-idem`).
///
/// The route took the `Idempotency-Key` and discarded it — `let _client_key = …`,
/// required of every caller and used by nothing — so this suite's every key was a
/// fresh `Uuid::now_v7()` and no second call ever carried one a first call used.
/// What a retry actually got was `BUNDLE_EXISTS_ON_PLAN`: the plan's uniqueness
/// index caught the second insert, so a client that retried on a timeout was told
/// its bundle already existed, by somebody, with no id and no way to tell its own
/// first attempt from another operator's.
///
/// Both halves are asserted. The status **and the body** come back as the first
/// call's, because a replay that re-rendered would hand the caller something it was
/// never told; and the store still holds one bundle, which is what makes this a
/// proof about at-most-once rather than about a status code.
#[tokio::test]
async fn a_retried_create_replays_the_first_answer_and_creates_one_bundle() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    harness.attach_shape(plan_id, 0).await;
    let key = Uuid::now_v7().to_string();
    let request = serde_json::json!({
        "plan_id": plan_id,
        "price_basis": "sum_of_parts",
        "invoice_itemization": "itemize",
    });

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(request.clone()),
            &[("idempotency-key", &key)],
        ))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first = body_json(first).await;

    let replay = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(request),
            &[("idempotency-key", &key)],
        ))
        .await;

    assert_eq!(
        replay.status(),
        StatusCode::CREATED,
        "a replay answers what the first call answered, not what the uniqueness index thinks \
         of a second insert"
    );
    let replayed = body_json(replay).await;
    assert_eq!(
        replayed, first,
        "verbatim: the bundle id above all, since a fresh one would name a bundle the caller \
         cannot address"
    );

    // The positive control against a replay that is merely a refusal wearing a
    // 201: exactly one bundle exists, and it is the one both answers named.
    let listed = harness
        .allowed()
        .send(with_headers(
            "GET",
            &format!("{BUNDLES}?$filter=plan_id%20eq%20{plan_id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = body_json(listed).await;
    let items = listed["items"].as_array().expect("a page of bundles");
    assert_eq!(items.len(), 1, "one create, one bundle: {listed}");
    assert_eq!(items[0]["bundle_id"], first["bundle_id"]);
}

/// **A different bundle under a spent key is refused** (Z12-7).
///
/// Armed on a genuinely different request — another basis, another itemization —
/// under the same key. Replaying the *same* body proves the opposite property and
/// is the case above, which is this one's positive control. The refusal is the
/// gate's own `IDEMPOTENCY_PAYLOAD_MISMATCH` rather than a `BUNDLE_EXISTS_ON_PLAN`
/// that happens to be produced by an index: the two are different facts, and only
/// the first tells the caller that the key is the problem.
#[tokio::test]
async fn a_different_create_under_a_spent_key_is_refused_as_a_payload_mismatch() {
    let harness = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&harness, plan_id).await;
    harness.attach_shape(plan_id, 0).await;
    let key = Uuid::now_v7().to_string();

    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "price_basis": "sum_of_parts",
                "invoice_itemization": "itemize",
            })),
            &[("idempotency-key", &key)],
        ))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let refused = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "price_basis": "own_price",
                "invoice_itemization": "aggregate",
            })),
            &[("idempotency-key", &key)],
        ))
        .await;

    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(
        problem_code(refused).await,
        "IDEMPOTENCY_PAYLOAD_MISMATCH",
        "a key held by one request must not answer for another, and the refusal has to name \
         the key rather than the plan's uniqueness"
    );
}

/// One bundle per plan, as a conflict rather than a storage failure.
#[tokio::test]
async fn a_second_bundle_on_one_plan_is_a_conflict() {
    let harness = Harness::new().await;
    let (plan_id, _) = seed_bundle(&harness).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            BUNDLES,
            Some(serde_json::json!({ "plan_id": plan_id, "price_basis": "own_price" })),
            &[("idempotency-key", &Uuid::now_v7().to_string())],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(response).await, "BUNDLE_EXISTS_ON_PLAN");
}

// ---------------------------------------------------------------------------
// The composition, under the plan revision's tag.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_composition_is_written_under_the_revisions_tag() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let tag = harness.plan_etag(plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": Uuid::now_v7(),
                    "included_sku_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    // The tag moved: the composition rides the revision, so an edit advances it.
    let next = etag_of(&response).expect("the response must carry the new tag");
    assert_ne!(next, tag, "a composition edit must move the revision's tag");
}

/// The tag is the guard, and a stale one is refused rather than merged.
#[tokio::test]
async fn a_stale_tag_is_refused_on_the_composition_route() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let tag = harness.plan_etag(plan_id).await;

    let body = serde_json::json!({ "plan_revision": 0, "components": [] });
    let first = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(body.clone()),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(first.status(), StatusCode::OK);

    let second = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(body),
            &[("if-match", &tag)],
        ))
        .await;
    // **409, not 412**, and it is the crate's convention rather than this
    // route's choice: `DomainError::StaleVersion` renders through the conflict
    // ladder everywhere in this gear (`rest_plans.rs` asserts the same pair), on
    // the argument that the caller's request was right about the world it was
    // shown and somebody else moved it. Re-reading is the whole remedy.
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(problem_code(second).await, "STALE_VERSION");
}

/// A party may not spell the reserved `platform` sentinel — the invariant that
/// keeps `residualAbsorberParty` unambiguous, refused at the edge.
#[tokio::test]
async fn a_party_named_platform_is_refused_at_the_edge() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let tag = harness.plan_etag(plan_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [],
                "rev_share": [{
                    "vendor_sku_id": Uuid::now_v7(),
                    "platform_cut_bp": 1000,
                    "parties": [{ "party": "platform", "share_bp": 9000 }],
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // **Which 400**, in a file whose module doc says a test asserting the status
    // would be asserting nothing. This case asserted exactly that until
    // 2026-08-20: measured by sending `"plan_revision": "not-a-number"` instead,
    // the deserializer's own 400 satisfied it — so the reserved-sentinel guard
    // could have been removed with this test green. The guard raises a bare
    // `InvalidRequest` and renders no per-violation code, so its sentence is the
    // discriminator.
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("spells the reserved `platform` sentinel")),
        "the sentinel guard is what refused, not another of this route's 400s: {problem}"
    );
}

// ---------------------------------------------------------------------------
// `inst-ba-validate` — the whole rule set, through the publish route.
// ---------------------------------------------------------------------------

/// An unpublished component blocks, and the report names the code.
#[tokio::test]
async fn an_unpublished_component_blocks_the_publish_by_code() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let tag = harness.plan_etag(plan_id).await;
    let component = Uuid::now_v7();

    let composed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": component,
                    "included_sku_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;
    // **The setup is asserted.** An empty composition yields the *same*
    // `COMPONENT_UNPUBLISHED` (see `a_composition_with_no_components_blocks_the_publish`),
    // so a `PATCH` that failed for any reason — a stale tag, a body change — left
    // this case unable to tell "an unpublished component blocks" from "the setup
    // never landed". Measured on 2026-08-20 by presenting `If-Match: "9-9"`: the
    // publish still answered `COMPONENT_UNPUBLISHED` and the case still passed.
    assert_eq!(
        composed.status(),
        StatusCode::OK,
        "the composition must land for the refusal below to be about its component"
    );

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "markets": [{ "currency": "EUR", "region": "EU" }],
            })),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let detail = body_json(response).await.to_string();
    assert!(
        detail.contains("COMPONENT_UNPUBLISHED"),
        "the report must name the failing rule, got: {detail}"
    );
}

/// **A component in another tenant reads exactly like one that does not exist**
/// — A1-6, traced to the end rather than left as a question.
///
/// `bundle_component.component_plan_id` is client-supplied, carries no foreign key
/// and is checked by nothing on the write path: `PATCH /bundles/{id}` stores any
/// uuid. The review asked whether a component naming another tenant's plan is
/// resolved in the caller's scope before it can matter, and the answer is that it
/// is — at publish, where the reference is first dereferenced.
/// `component_defects` runs all three of its reads through `.secure()` with the
/// caller's scope, so a **published** plan in another tenant contributes
/// `Unpublished` and nothing else, exactly as an absent id does.
///
/// The published foreign plan is what makes this a measurement. A foreign *draft*
/// would answer `COMPONENT_UNPUBLISHED` against an unscoped read too, so a probe
/// using one would pass with the scoping removed.
///
/// The report bodies are compared whole with each component id struck out: it is
/// not enough that both refuse, because `ComponentDefect` renders three distinct
/// sentences and a leak here would be a *different sentence*, not a different
/// status. The positive control is the caller's own published plan, which reaches
/// a different refusal entirely — proof that the two answers above are the scope
/// talking and not a rule that refuses every component.
#[tokio::test]
async fn a_component_in_another_tenant_reads_exactly_like_an_absent_one() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;

    let foreign = Uuid::now_v7();
    seed_foreign_current_plan(&harness, foreign).await;
    let absent = Uuid::now_v7();

    let report_for = async |component: Uuid| {
        let tag = harness.plan_etag(plan_id).await;
        let patched = harness
            .allowed()
            .send(with_headers(
                "PATCH",
                &bundle_path(bundle_id),
                Some(serde_json::json!({
                    "plan_revision": 0,
                    "components": [{
                        "component_plan_id": component,
                        "included_sku_id": Uuid::now_v7(),
                    }],
                })),
                &[("if-match", &tag)],
            ))
            .await;
        assert_eq!(
            patched.status(),
            StatusCode::OK,
            "the write stores any uuid; the reference is dereferenced at publish"
        );

        let published = harness
            .allowed()
            .send(with_headers(
                "POST",
                &publish_path(bundle_id),
                Some(serde_json::json!({
                    "plan_revision": 0,
                    "markets": [{ "currency": "EUR", "region": "EU" }],
                })),
                &[],
            ))
            .await;
        assert_eq!(published.status(), StatusCode::BAD_REQUEST);
        redacting(published, component).await
    };

    let foreign_report = report_for(foreign).await;
    let absent_report = report_for(absent).await;
    assert!(
        foreign_report.contains("COMPONENT_UNPUBLISHED"),
        "got: {foreign_report}"
    );
    assert!(
        foreign_report.contains("<id>"),
        "the substitution must have found the id it removes: {foreign_report}"
    );
    assert_eq!(
        foreign_report, absent_report,
        "a published plan in another tenant must be indistinguishable from an absent one, \
         sentence for sentence"
    );

    // The positive control. The caller's own published plan is *not* refused
    // `COMPONENT_UNPUBLISHED`, so the two reports above are the caller's scope
    // talking rather than a rule that refuses every component whatever it names.
    let own = Uuid::now_v7();
    seed_current_plan(&harness, own).await;
    let own_report = report_for(own).await;
    assert!(
        !own_report.contains("COMPONENT_UNPUBLISHED"),
        "the caller's own published plan is published; got: {own_report}"
    );
}

/// A `sum_of_parts` bundle referencing nothing sums nothing.
#[tokio::test]
async fn a_composition_with_no_components_blocks_the_publish() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let tag = harness.plan_etag(plan_id).await;

    let composed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "components": [] })),
            &[("if-match", &tag)],
        ))
        .await;
    // The write under test has to land, or the publish below is refused for a
    // bundle with no composition row at all and the empty set is not what it
    // measured — the same publish, for a different reason, over a fixture that
    // never reached the state under test.
    let composed_status = composed.status();
    assert_eq!(
        composed_status,
        StatusCode::OK,
        "the empty composition must be stored: {}",
        body_json(composed).await
    );

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let detail = body_json(response).await.to_string();
    assert!(detail.contains("COMPONENT_UNPUBLISHED"), "got: {detail}");
}

/// A rev-share group more than 1 bp out is `RESIDUAL_OVER_TOLERANCE` (D-07) —
/// the six-way even split that decision names as the operator's to reconcile.
#[tokio::test]
async fn a_group_over_tolerance_blocks_the_publish_by_code() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let tag = harness.plan_etag(plan_id).await;

    harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [],
                "rev_share": [{
                    "vendor_sku_id": Uuid::now_v7(),
                    "platform_cut_bp": 0,
                    "parties": [
                        { "party": "a", "share_bp": 1666 },
                        { "party": "b", "share_bp": 1666 },
                        { "party": "c", "share_bp": 1666 },
                        { "party": "d", "share_bp": 1666 },
                        { "party": "e", "share_bp": 1666 },
                        { "party": "f", "share_bp": 1666 },
                    ],
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let detail = body_json(response).await.to_string();
    assert!(detail.contains("RESIDUAL_OVER_TOLERANCE"), "got: {detail}");
}

/// **The aggregate report.** One pass names every failing rule, which is what
/// makes a composition remediable in one edit rather than in as many edits as it
/// has faults.
#[tokio::test]
async fn one_publish_reports_every_failing_rule_at_once() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let tag = harness.plan_etag(plan_id).await;

    harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": Uuid::now_v7(),
                    "included_sku_id": Uuid::now_v7(),
                }],
                "rev_share": [{
                    "vendor_sku_id": Uuid::now_v7(),
                    "platform_cut_bp": 0,
                    "parties": [{ "party": "a", "share_bp": 5000 }],
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "markets": [{ "currency": "EUR", "region": "EU" }],
            })),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let detail = body_json(response).await.to_string();
    for code in [
        "COMPONENT_UNPUBLISHED",
        "CURRENCY_NOT_COVERED",
        "RESIDUAL_OVER_TOLERANCE",
    ] {
        assert!(
            detail.contains(code),
            "{code} must be reported alongside the others, got: {detail}"
        );
    }
}

// ---------------------------------------------------------------------------
// `inst-ba-return` — 202 and the always-material verdict.
// ---------------------------------------------------------------------------

/// A composition with nothing to refuse answers **202** and opens D-104's
/// always-material unit — asserted on the **stored** record, not on the body.
///
/// The body assertion below is kept, but it is deliberately no longer the only
/// one, and the reason is that on its own it could not fail. `bundles.rs` wrote
/// `materiality: "alwaysMaterialTrigger".to_owned()` as an unconditional literal,
/// so this test compared the handler's constant against a copy of itself: no
/// input could redden it, and while green it reported D-104 as covered on this
/// route. `alwaysMaterialTrigger` is in fact the token `evaluate` answers for an
/// act trigger, which is what made the tautology so hard to see — the value was
/// right and nothing had computed it.
///
/// `approval_rows` is what tells the two apart. It was in scope in this file's
/// sibling case (`a_composition_edit_voids_the_pending_unit_over_its_plan`) the
/// whole time; applying it here is what surfaced that the publish opened no unit
/// at all.
#[tokio::test]
async fn a_clean_publish_is_accepted_and_opens_the_always_material_unit() {
    let harness = Harness::new().await;
    // An `own_price` bundle with no components and no markets has no coverage
    // walk to fail and no rev-share to reconcile: the smallest publishable
    // composition there is.
    let (plan_id, bundle_id) = seed_bundle_with(&harness, "own_price").await;
    let tag = harness.plan_etag(plan_id).await;
    harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "components": [] })),
            &[("if-match", &tag)],
        ))
        .await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;

    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "publish refused: {body}");
    assert_eq!(
        body["materiality"].as_str(),
        Some("alwaysMaterialTrigger"),
        "D-104: a composition publish is material whatever a threshold says"
    );

    // The load-bearing half. A handler that renders the token and evaluates
    // nothing passes every assertion above it.
    let opened = approval_rows(&harness).await;
    let expected_ref = format!("{plan_id}/composition/0");
    let Some(opened_row) = opened.iter().find(|row| row.subject_ref == expected_ref) else {
        panic!(
            "D-104: the composition publish must open an always-material unit over \
             {expected_ref}; the store holds {} unit(s): {:?}",
            opened.len(),
            opened.iter().map(|r| &r.subject_ref).collect::<Vec<_>>()
        )
    };
    let opened_id = opened_row.approval_id;
    let unit = approval_row(&harness, opened_id).await;
    assert_eq!(
        unit.materiality["reason"], "alwaysMaterialTrigger",
        "the stored verdict names the rule that fired, which is what an auditor reads"
    );
    // **The third arm of the classifier, and the control for the two cases above.**
    // This plan has no live revision at all, so there is no composition for this one
    // to be a re-split *of* — which is D-104's own first clause, "bundle creation".
    // Asserted here rather than in a case of its own because this is already the
    // fixture for it, and because a classifier answering `revenueShareChange`
    // whenever a rev-share exists would pass both cases above and fail this.
    assert_eq!(
        unit.materiality["trigger"], "bundleComposition",
        "a first composition is the composition act: {}",
        unit.materiality
    );
    assert_eq!(
        unit.state,
        bss_pricing::domain::approval::ApprovalState::Submitted,
        "D-104: a single principal's publish stages the composition; it does not commit it"
    );
}

/// The **positive control** for the case above, and the other half of D-104's
/// two-call shape: a second, independent principal approves the staged unit and
/// the same call then publishes.
///
/// Without this the suite would prove only that the composition stops — a handler
/// that staged every publish and could never commit one would pass every
/// assertion in `a_clean_publish_is_accepted_and_opens_the_always_material_unit`.
/// That is the failure mode the fix itself introduced: before D-104 was enforced
/// here the publish arm was the only arm, and afterwards it was reachable by no
/// test at all.
#[tokio::test]
async fn an_independent_approval_is_what_publishes_the_composition() {
    const COMPOSITION_REVIEWER: Uuid = Uuid::from_u128(0x_b0d1_e504);

    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle_with(&harness, "own_price").await;
    let tag = harness.plan_etag(plan_id).await;
    harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "components": [] })),
            &[("if-match", &tag)],
        ))
        .await;

    // Call one: stages.
    let staged = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;
    let staged_body = body_json(staged).await;
    assert_eq!(
        staged_body["outcome"].as_str(),
        Some("submitted_for_approval"),
        "the first call over this content stages it: {staged_body}"
    );
    let approval_id = staged_body["approval"]["approval_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| panic!("the 202 names the unit it opened: {staged_body}"));

    let approved = harness
        .allowed_as(COMPOSITION_REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    let approved_status = approved.status();
    let approved_body = body_json(approved).await;
    assert_eq!(
        approved_status,
        StatusCode::OK,
        "an independent principal is what authorizes a composition change (D-104): \
         {approved_body}"
    );

    // Call two: publishes, on the strength of that decision and no other.
    let published = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;
    let published_body = body_json(published).await;
    assert_eq!(
        published_body["outcome"].as_str(),
        Some("published"),
        "the approved content publishes on the next call: {published_body}"
    );
    assert_eq!(
        published_body["materiality"].as_str(),
        Some("alwaysMaterialTrigger"),
        "and the reason is the evaluator's on both arms, not a per-arm literal"
    );
}

// ---------------------------------------------------------------------------
// D-104's **two** triggers — which act the record names (D-232's second half).
// ---------------------------------------------------------------------------

/// Write a composition at `revision`, under the plan's current tag.
async fn write_composition(
    harness: &Harness,
    plan_id: Uuid,
    bundle_id: Uuid,
    revision: u64,
    body: serde_json::Value,
) {
    let tag = harness.plan_etag(plan_id).await;
    let mut payload = body;
    payload["plan_revision"] = serde_json::json!(revision);
    let response = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(payload),
            &[("if-match", &tag)],
        ))
        .await;
    let status = response.status();
    let written = body_json(response).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the composition write must land: {written}"
    );
    // **The route writes at the tag's revision, not at the body's**, so a fixture
    // that assumed otherwise would seed the wrong revision and the case above it
    // would be asserting about a diff nobody authored.
    assert_eq!(
        written["plan_revision"].as_u64(),
        Some(revision),
        "the composition must land on the revision this fixture means: {written}"
    );
}

/// Publish the bundle at `revision` and hand back the stored unit the call opened.
async fn staged_unit(
    harness: &Harness,
    plan_id: Uuid,
    bundle_id: Uuid,
    revision: u64,
) -> serde_json::Value {
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": revision, "markets": [] })),
            &[],
        ))
        .await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "publish refused: {body}");

    let expected_ref = format!("{plan_id}/composition/{revision}");
    let rows = approval_rows(harness).await;
    let opened = rows
        .iter()
        .find(|row| row.subject_ref == expected_ref)
        .unwrap_or_else(|| {
            panic!(
                "the publish must open a unit over {expected_ref}; the store holds {:?}",
                rows.iter().map(|r| &r.subject_ref).collect::<Vec<_>>()
            )
        });
    approval_row(harness, opened.approval_id).await.materiality
}

/// A group of two named parties splitting one vendor's revenue.
///
/// Rev-share is authorable on `sum_of_parts` only (`REVSHARE_BASIS_UNSUPPORTED`,
/// D-55), which is why these cases seed the default basis rather than the
/// `own_price` shape the two cases above them use — and a `sum_of_parts` bundle
/// referencing no component is refused too, so they carry a real published one.
fn one_group(vendor: Uuid, alpha_bp: i64, beta_bp: i64) -> serde_json::Value {
    serde_json::json!([{
        "vendor_sku_id": vendor,
        "platform_cut_bp": 0,
        "parties": [
            { "party": "alpha", "share_bp": alpha_bp },
            { "party": "beta", "share_bp": beta_bp },
        ],
    }])
}

/// One published plan, referenced as a component.
fn one_component(plan: Uuid) -> serde_json::Value {
    serde_json::json!([{ "component_plan_id": plan, "included_sku_id": Uuid::now_v7() }])
}

/// **The claim this whole change exists to make**: a publish whose only movement is
/// a re-split names `revenueShareChange`, and a reader of the approval record can
/// tell it from a component swap.
///
/// D-104 registers two triggers precisely so that *"an operator reading the approval
/// record should not have to infer"* whether what moved was the customer's
/// composition or the vendor's payout. Until 2026-08-16 the record named neither:
/// `publish_bundle` declared `Trigger::BundleComposition` unconditionally, and the
/// verdict carried no trigger identity at all — so both acts produced a
/// byte-identical response and a byte-identical stored document (D-232, D-321).
///
/// **A weaker probe would have passed before any of it.** `materiality.material` was
/// already `true` and `materiality.reason` was already `alwaysMaterialTrigger` for
/// every composition publish, so a case asserting either would have been green
/// against the state this closes. The assertion is on `trigger`, and it is paired
/// with the two controls below rather than standing alone: one says a component
/// change answers the *other* token, and one says a first publish does — so a
/// classifier that answered `revenueShareChange` for everything cannot pass the set.
#[tokio::test]
async fn a_rev_share_only_republish_names_the_rev_share_act() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let vendor = Uuid::now_v7();
    let component_plan = Uuid::now_v7();
    seed_current_plan(&harness, component_plan).await;
    let components = one_component(component_plan);

    // Revision 0's composition, published as the plan's content — this is the
    // baseline the diff runs against.
    write_composition(
        &harness,
        plan_id,
        bundle_id,
        0,
        serde_json::json!({
            "components": components,
            "rev_share": one_group(vendor, 5000, 5000),
        }),
    )
    .await;
    harness.publish(plan_id, 0).await;
    harness.open_successor(plan_id).await;

    // Revision 1 moves the split and nothing else. The component set is byte-equal
    // on both sides, so what changed is who is paid.
    write_composition(
        &harness,
        plan_id,
        bundle_id,
        1,
        serde_json::json!({
            "components": components,
            "rev_share": one_group(vendor, 7000, 3000),
        }),
    )
    .await;

    let stored = staged_unit(&harness, plan_id, bundle_id, 1).await;

    assert_eq!(
        stored["reason"], "alwaysMaterialTrigger",
        "the rule that fired is unchanged; this case is about the act beside it"
    );
    assert_eq!(
        stored["trigger"], "revenueShareChange",
        "D-104's second trigger: the shares moved and the component set did not, and \
         a rev-share re-split *is* vendor payout — got {stored}"
    );
}

/// The **positive control**: the same fixture, moving a component instead, answers
/// the other token.
///
/// Without it `a_rev_share_only_republish_names_the_rev_share_act` proves only that
/// *some* token reaches the column — a classifier hard-coded to `revenueShareChange`
/// would pass it, which is the literal-shaped defect this surface has now produced
/// three times (`bundles.rs`' own materiality literal, `overlays.rs`' before it, and
/// `bulkGroupMove`'s dated comment).
#[tokio::test]
async fn a_component_change_over_the_same_baseline_names_the_composition_act() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;
    let vendor = Uuid::now_v7();
    // Two published plans are two legal components; markets are empty, so the
    // coverage walk has nothing to refuse either for.
    let first_component = Uuid::now_v7();
    let second_component = Uuid::now_v7();
    seed_current_plan(&harness, first_component).await;
    seed_current_plan(&harness, second_component).await;
    let one = one_component(first_component);

    write_composition(
        &harness,
        plan_id,
        bundle_id,
        0,
        serde_json::json!({ "components": one, "rev_share": one_group(vendor, 5000, 5000) }),
    )
    .await;
    harness.publish(plan_id, 0).await;
    harness.open_successor(plan_id).await;

    // The split is held **still** and a second component arrives, which is the
    // exact inverse of the case above over an identically-built baseline.
    let mut two = one.clone();
    two.as_array_mut()
        .expect("a component list")
        .push(one_component(second_component)[0].clone());
    write_composition(
        &harness,
        plan_id,
        bundle_id,
        1,
        serde_json::json!({ "components": two, "rev_share": one_group(vendor, 5000, 5000) }),
    )
    .await;

    let stored = staged_unit(&harness, plan_id, bundle_id, 1).await;

    assert_eq!(
        stored["trigger"], "bundleComposition",
        "the component set moved, so the record must say what the customer receives \
         changed — got {stored}"
    );
}

// ---------------------------------------------------------------------------
// `inst-bc-coverage`'s narrowing — which rows count, driven end to end.
// ---------------------------------------------------------------------------

/// A **published component whose only row is still a draft** covers nothing.
///
/// This pair exists because a probe found the gap it closes: deleting the
/// `lifecycle_state = 'published'` conjunct from the service's coverage filter
/// reddened **nothing**, since no case here had a draft row for the filter to
/// exclude. The narrowing was enforced and untested at this layer, which is a
/// guard that can be removed under a green tree.
///
/// The component's *plan* is published, so `COMPONENT_UNPUBLISHED` must **not**
/// fire — that is what makes this a test of the row filter rather than of the
/// plan check standing in front of it.
#[tokio::test]
async fn a_published_components_draft_row_does_not_cover_a_market() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;

    // A published component plan carrying one row that never published.
    let component = Uuid::now_v7();
    // No `attach_shape` on the component, and the reason is now only the second
    // one: the price row does not need the phase row to exist at all — the `phase`
    // axis is a bare uuid (D-19) and carries no foreign key. It used to be forced
    // as well, the harness attaching a **fixed** phase id onto a key
    // (`phase_id, plan_revision`) that gave one id to one plan, so a second plan at
    // revision 0 collided with the bundle plan's own; D-340 widened that key.
    seed_draft_plan(&harness, component).await;
    seed_price(&harness, component, "EU").await;
    harness.publish(component, 0).await;

    let tag = harness.plan_etag(plan_id).await;
    harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": component,
                    "included_sku_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "markets": [{ "currency": "USD", "region": "EU" }],
            })),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let detail = body_json(response).await.to_string();
    assert!(
        detail.contains("CURRENCY_NOT_COVERED"),
        "a draft row must not count as coverage, got: {detail}"
    );
    assert!(
        !detail.contains("COMPONENT_UNPUBLISHED"),
        "the component's plan IS published; this must be the row filter and not the \
         plan check, got: {detail}"
    );
}

/// The other half of the same fact, and the half that makes the pair a
/// discrimination rather than a rule that always refuses: publish the very same
/// row and the very same composition passes its coverage walk.
///
/// Without this case the filter could be `WHERE false` and the case above would
/// still be green.
#[tokio::test]
async fn the_same_row_published_does_cover_the_market() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;

    let component = Uuid::now_v7();
    seed_draft_plan(&harness, component).await;
    let row = seed_price(&harness, component, "EU").await;
    harness.publish(component, 0).await;
    harness.publish_price(component, row.price_id).await;

    let tag = harness.plan_etag(plan_id).await;
    let composed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": component,
                    "included_sku_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(
        composed.status(),
        StatusCode::OK,
        "the composition must land, or the publish below covers an empty one"
    );

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "markets": [{ "currency": "USD", "region": "EU" }],
            })),
            &[],
        ))
        .await;

    // **The publish is asserted to succeed**, not merely to be free of one code
    // string. This case is the declared discrimination control for the one above,
    // and until 2026-08-20 it bound `status` for the failure message only: any
    // failure that was not the coverage refusal — a dropped `PATCH` leaving an
    // empty composition, a `409` on the tag, a `500` — left the body free of
    // `CURRENCY_NOT_COVERED` and this control green, so the coverage filter could
    // have been `WHERE false` with the pair still passing.
    let status = response.status();
    let detail = body_json(response).await.to_string();
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "a published row on the sold market covers it, so the publish is accepted: {detail}"
    );
    assert!(
        !detail.contains("CURRENCY_NOT_COVERED"),
        "a published row on the sold market must cover it, got {status}: {detail}"
    );
}

// ---------------------------------------------------------------------------
// Section 10's currency-binding counter, cases (ii) and (iii), through the router.
// ---------------------------------------------------------------------------

/// The counter section 10 names.
const CURRENCY_BINDING: &str = "pricing_currency_binding_blocks_total";

/// Every label the counter can carry, so a case can assert the ones it is *not*.
///
/// `CurrencyBindingCase::ALL`'s spellings. Written out here rather than iterated
/// from the enum on purpose: the label values are the operator-facing series
/// names, and a test deriving them from the same `as_str` the adapter uses would
/// stay green through a rename that broke every dashboard.
const CASES: [&str; 3] = ["required_addon", "bundle_sum_of_parts", "bundle_own_price"];

/// Publish a one-component bundle on `basis`, selling `(USD, EU)`, the component's
/// only row published or not, and hand back the response body as text.
///
/// The seeding is `a_published_components_draft_row_does_not_cover_a_market`'s,
/// which is the shape already proven to reach the coverage walk: the component's
/// *plan* publishes, so `COMPONENT_UNPUBLISHED` is not what fires, and whether the
/// market is covered is then the single variable.
async fn publish_one_component_bundle(harness: &Harness, basis: &str, covered: bool) -> String {
    let (plan_id, bundle_id) = seed_bundle_with(harness, basis).await;

    let component = Uuid::now_v7();
    seed_draft_plan(harness, component).await;
    let row = seed_price(harness, component, "EU").await;
    harness.publish(component, 0).await;
    if covered {
        harness.publish_price(component, row.price_id).await;
    }

    let tag = harness.plan_etag(plan_id).await;
    let composed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": component,
                    "included_sku_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(
        composed.status(),
        StatusCode::OK,
        "the composition must land: every caller of this helper reasons from the \
         *component* being covered or not, and a dropped PATCH leaves an empty \
         composition that refuses for its own reason"
    );

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "markets": [{ "currency": "USD", "region": "EU" }],
            })),
            &[],
        ))
        .await;
    // `covered` decides the outcome, so the helper holds the publish to it rather
    // than handing every caller a body to infer success from the absence of a code
    // string — the shape `the_same_row_published_does_cover_the_market` carried
    // until 2026-08-20, and `a_covered_bundle_publish_counts_no_block` inherited
    // it verbatim from here.
    let status = response.status();
    let detail = body_json(response).await.to_string();
    assert_eq!(
        status,
        if covered {
            StatusCode::ACCEPTED
        } else {
            StatusCode::BAD_REQUEST
        },
        "a {basis} bundle whose component is covered={covered}: {detail}"
    );
    detail
}

/// **A blocked `sum_of_parts` publish counts case (ii) and nothing else.**
///
/// Asserted through the router, which is the standard `rest_preview.rs` states for
/// the sibling instrument: what is proven is that a real refusal reported a real
/// series, not that the adapter can increment its own counter. Both of this
/// counter's bundle cases were reachable only from `metrics_tests.rs`, which calls
/// the port directly — deleting the emission in `infra/bundle.rs` left the whole
/// suite green.
///
/// The **other two labels are asserted zero**, and that is the load-bearing half
/// rather than padding. `bundle_rules` raises the same `CURRENCY_NOT_COVERED`
/// string for cases (i), (ii) and (iii), so an emitter that chose its label by
/// scanning violation codes would count this block as `required_addon` while the
/// refusal an operator reads stayed correct. Only the label can tell the two
/// apart.
#[tokio::test]
async fn a_blocked_sum_of_parts_publish_counts_the_bundle_sum_case() {
    let harness = Harness::new().await;
    let detail = publish_one_component_bundle(&harness, "sum_of_parts", false).await;

    assert!(
        detail.contains("CURRENCY_NOT_COVERED"),
        "the block this counts must be the coverage one, got: {detail}"
    );
    harness.metrics.force_flush();

    assert_eq!(
        harness
            .metrics
            .counter_value(CURRENCY_BINDING, &[("case", "bundle_sum_of_parts")]),
        1,
        "one composition mistake is one block on the declared basis's case"
    );
    for other in CASES.iter().filter(|case| **case != "bundle_sum_of_parts") {
        assert_eq!(
            harness
                .metrics
                .counter_value(CURRENCY_BINDING, &[("case", other)]),
            0,
            "a sum_of_parts bundle's block must not be reported as {other}"
        );
    }
}

/// The same block on an `own_price` bundle is case (iii), and this is what makes
/// the pair a **discrimination** rather than one label asserted twice.
///
/// The composition, the component, the market and the refusal code are identical
/// to the case above; only the declared basis differs. An emitter reading the
/// report instead of the basis would answer `bundle_sum_of_parts` to both and pass
/// one of the two cases.
#[tokio::test]
async fn a_blocked_own_price_publish_counts_the_other_bundle_case() {
    let harness = Harness::new().await;
    let detail = publish_one_component_bundle(&harness, "own_price", false).await;

    assert!(
        detail.contains("CURRENCY_NOT_COVERED"),
        "the block this counts must be the coverage one, got: {detail}"
    );
    harness.metrics.force_flush();

    assert_eq!(
        harness
            .metrics
            .counter_value(CURRENCY_BINDING, &[("case", "bundle_own_price")]),
        1,
        "the label is the composition's declared basis, not the violation's code"
    );
    for other in CASES.iter().filter(|case| **case != "bundle_own_price") {
        assert_eq!(
            harness
                .metrics
                .counter_value(CURRENCY_BINDING, &[("case", other)]),
            0,
            "an own_price bundle's block must not be reported as {other}"
        );
    }
}

/// **A composition that covers its market counts nothing.**
///
/// The negative control the two above rest on: an emitter that counted every
/// publish, or every validation run, would satisfy both of them and would report a
/// healthy catalog as permanently blocked. Same route, same seed, one row
/// published.
#[tokio::test]
async fn a_covered_bundle_publish_counts_no_block() {
    let harness = Harness::new().await;
    let detail = publish_one_component_bundle(&harness, "sum_of_parts", true).await;

    assert!(
        !detail.contains("CURRENCY_NOT_COVERED"),
        "a published row on the sold market covers it, got: {detail}"
    );
    harness.metrics.force_flush();

    for case in CASES {
        assert_eq!(
            harness
                .metrics
                .counter_value(CURRENCY_BINDING, &[("case", case)]),
            0,
            "a composition that covers its markets must report no {case} block"
        );
    }
}

// ---------------------------------------------------------------------------
// The TOCTOU guard, on the plane that rides a plan revision.
// ---------------------------------------------------------------------------

/// **A composition edit voids the pending unit over the plan it rides.**
///
/// `inst-ap-pin` states the guard without qualification: *"any mutation of the subject
/// while `submitted` invalidates the pending approval"*. A bundle composition **is** a
/// mutation of a plan revision — it rides the revision, and the edit takes the
/// revision's entity tag and advances it.
///
/// **It holds, and this case exists because two of the three things that would break it
/// are true.** Written first as a demonstration that it did *not* hold, and the
/// hypothesis was wrong at exactly one link:
///
/// 1. `PlanShape` — what a `plan_revision` unit pins — carries **no composition**. Its
///    nineteen fields are the plan's own shape, its rows and its windows; the component
///    list, the rev-share splits, `price_basis` and `invoiceItemization` are none of
///    them. **True.**
/// 2. So a component swap **does not move the pin**: `content_matches_pin` would stay
///    `true` across it, which is the answer a reviewer's read is built to trust.
///    **True.**
/// 3. And nothing voids the unit. **False** — and the reason is one level down, which
///    is why it survived a grep: `bundle_repo` never names `void_pending_units_of`, but
///    `replace_composition` calls `plan_repo::record_revision_mutation`, whose *first*
///    statement is that void. The composition edit is recorded as a plan-revision
///    mutation, so it inherits the TOCTOU guard rather than restating it.
///
/// Which makes 1 and 2 harmless **only while 3 holds**, and that is what this case
/// pins. A refactor that gave the composition its own recorder — a plausible thing to
/// want, since the audit record it writes is a plan record for a bundle act — would
/// reopen D-104's own scenario (*"a component swap reached consumers with no
/// approver"*) through the **approved** path, which is the worse half: the approval
/// record would say the act was reviewed, and the composition reaching consumers would
/// be one no reviewer ever saw. Nothing asserted this before.
///
/// The control below is not ceremony. Without it the case passes over a guard that does
/// not exist, because a unit that was never `submitted` reads `voided` at the end
/// whatever the edit did.
#[tokio::test]
async fn a_composition_edit_voids_the_pending_unit_over_its_plan() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;

    let opened = harness
        .governance
        .approvals
        .submit(
            &harness.scope(),
            harness.tenant,
            bss_pricing::domain::scope_key::PlanId::new(plan_id),
            Uuid::now_v7(),
            serde_json::json!({ "material": true, "reason": "alwaysMaterialTrigger" }),
            bss_pricing::domain::audit::AuditStamp {
                actor_principal_id: Uuid::from_u128(0x_b0_11),
                recorded_at: OffsetDateTime::now_utc(),
                correlation_id: Uuid::from_u128(0x_b0_c0),
            },
        )
        .await
        .expect("a plan-revision unit opens over the plan the bundle rides");

    // **The control.** Without it this case proves nothing: a unit that was never
    // `submitted` reads "voided" at the end whatever the composition edit did, and the
    // assertion below would pass over a guard that does not exist.
    assert_eq!(
        harness
            .read_approval(opened.approval_id)
            .await
            .expect("the unit reads back")
            .state
            .as_str(),
        "submitted",
        "the unit must be pending before the composition moves under it"
    );

    let tag = harness.plan_etag(plan_id).await;
    let swapped = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": Uuid::now_v7(),
                    "included_sku_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(swapped.status(), StatusCode::OK);

    let record = harness
        .read_approval(opened.approval_id)
        .await
        .expect("the unit reads back");
    assert_eq!(
        record.state.as_str(),
        "voided",
        "the composition moved under a pending unit whose pin cannot see it, so the unit \
         must be voided rather than left approvable"
    );
}

/// **Does the approved unit's pin actually cover the composition?**
///
/// F-1's fix pins the *plan shape* (`infra::publish::assemble`), on the reasoning
/// that a composition normalizes onto its absorber inside the plan. If that is
/// false at submit time, then two different component sets hash identically and an
/// approve taken over one authorizes the other — the exact approval-bypass shape
/// D-196's content-pin miss produced on the scope key.
///
/// D-104 exists precisely because a `sum_of_parts` recomposition carries **no
/// price-row delta at all**, which is what makes this worth an armed test rather
/// than an argument.
#[tokio::test]
async fn an_approved_composition_does_not_authorize_a_different_one() {
    const REVIEWER: Uuid = Uuid::from_u128(0x_b0d1_e505);

    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle_with(&harness, "own_price").await;

    let tag = harness.plan_etag(plan_id).await;
    let composed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "components": [] })),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(
        composed.status(),
        StatusCode::OK,
        "the composition the reviewer is going to approve must exist"
    );

    let staged = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;
    let staged_body = body_json(staged).await;
    let approval_id = staged_body["approval"]["approval_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| panic!("the 202 names its unit: {staged_body}"));

    let decided = harness
        .allowed_as(REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        decided.status(),
        StatusCode::OK,
        "there is no approval to be bypassed unless this one landed"
    );
    assert_eq!(
        approval_row(&harness, approval_id).await.state.as_str(),
        "approved",
        "and the record carries the decision, not merely the response"
    );

    // The composition moves **after** the approval, and it moves for real: a
    // component the reviewer never saw. Published, so the publish below is refused
    // by no component rule and the *only* thing left to answer is whether the
    // approval still covers the set.
    //
    // **The edit used to be a no-op.** It sent
    // `{"plan_revision":0,"components":[],"rev_share":[]}` against a pre-approval
    // `{"plan_revision":0,"components":[]}`, and `CompositionRequest::rev_share` is
    // `#[serde(default)]` (`api/rest/bundles.rs`) — so both bodies deserialized
    // to the *identical* composition and the case's stated claim was never
    // exercised. It passed on the revision tag having moved, which is a different
    // fact, and this is the approval-bypass shape the doc above says the case is
    // armed against. (A `rev_share` split is not usable as the difference here:
    // `REVSHARE_BASIS_UNSUPPORTED` refuses one on an `own_price` bundle at publish,
    // D-55.)
    let newcomer = Uuid::now_v7();
    seed_draft_plan(&harness, newcomer).await;
    harness.publish(newcomer, 0).await;

    let tag = harness.plan_etag(plan_id).await;
    let edited = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": newcomer,
                    "included_sku_id": Uuid::now_v7(),
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;
    let edited_status = edited.status();
    assert_eq!(
        edited_status,
        StatusCode::OK,
        "the edit must land, or the publish below re-publishes what was approved"
    );

    let after = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;
    let after_body = body_json(after).await;

    assert_eq!(
        after_body["outcome"].as_str(),
        Some("submitted_for_approval"),
        "the edit (PATCH answered {edited_status}) must invalidate the approval, so this call \
         stages a fresh unit rather than publishing on the old one: {after_body}"
    );

    // **What this still cannot separate, said rather than implied.** Every `PATCH`
    // moves the composition's row version, so an approval invalidated by "the
    // component set changed" and one invalidated by "the tag moved" are
    // indistinguishable from outside this route — there is no way to author a
    // different component set at an unchanged tag. What the case above now
    // guarantees is that the set the reviewer agreed to is not the set that
    // publishes; the narrower claim (which operand the pin reads) is
    // `infra::publish::assemble`'s to answer, at a layer where the shape can be
    // varied without a `PATCH`. The positive control that a *re-issued, unedited*
    // publish does commit on the approval is
    // `an_independent_approval_is_what_publishes_the_composition`.
}

/// **The approver can read the composition they are approving** (D-61, F-4).
///
/// `GET /approvals/{id}` returns `pinned_content` — the plan the composition rides
/// — and until 2026-08-11 that was all a reviewer of a `bundleComposition` unit
/// got. A plan shape carries no component set and no revenue split, and D-104
/// exists because a `sum_of_parts` recomposition moves no price row at all: the
/// document said nothing about the act being decided, on the one surface in this
/// gear where the money being divided belongs to third parties.
///
/// D-61 is explicit that the `GET` must return the pinned **content**, "not the
/// hash alone, so approval is never hash-blind".
#[tokio::test]
async fn the_unit_shows_its_approver_the_component_set_and_the_revenue_split() {
    let harness = Harness::new().await;
    // `sum_of_parts`: rev-share is authorable on that basis only (D-55), and it is
    // the basis whose recomposition moves no price row — which is the case D-104
    // exists for and the one a plan shape cannot show.
    let (plan_id, bundle_id) = seed_bundle(&harness).await;

    let component_plan = Uuid::now_v7();
    seed_draft_plan(&harness, component_plan).await;
    let row = seed_price(&harness, component_plan, "EU").await;
    harness.publish(component_plan, 0).await;
    harness.publish_price(component_plan, row.price_id).await;
    let vendor_sku = Uuid::now_v7();

    let tag = harness.plan_etag(plan_id).await;
    let patched = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": component_plan,
                    "included_sku_id": vendor_sku,
                }],
                "rev_share": [{
                    "vendor_sku_id": vendor_sku,
                    "platform_cut_bp": 2000,
                    "parties": [{ "party": "acme-vendor", "share_bp": 8000 }],
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::OK);

    let staged = harness
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        ))
        .await;
    let staged_body = body_json(staged).await;
    let approval_id = staged_body["approval"]["approval_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| panic!("the 202 names its unit: {staged_body}"));

    let read = harness
        .allowed()
        .send(with_headers(
            "GET",
            &format!("/bss-pricing/v1/approvals/{approval_id}"),
            None,
            &[],
        ))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let view = body_json(read).await;

    let composition = &view["pinned_composition"];
    assert!(
        composition.is_object(),
        "a bundleComposition unit must render its composition: {view}"
    );
    assert_eq!(
        composition["components"][0]["component_plan_id"],
        component_plan.to_string(),
        "the reviewer sees which plan is in the bundle: {composition}"
    );
    // The half that is third-party money, and the reason D-104 makes this act
    // always material.
    assert_eq!(composition["rev_share"][0]["platform_cut_bp"], 2000);
    assert_eq!(
        composition["rev_share"][0]["parties"][0]["party"],
        "acme-vendor"
    );
    assert_eq!(composition["rev_share"][0]["parties"][0]["share_bp"], 8000);
}

/// **The composition reads back as it was written** (D-310).
///
/// The read F-4 asked for. A composition was reachable through no surface in the
/// gear: not by its author, not by an operator, and — once D-104's always-material
/// unit existed — not by the approver deciding it.
///
/// The members are the authoring shapes, so this asserts the round trip rather
/// than a second rendering: what an author reads back is spelled exactly as what
/// they wrote.
#[tokio::test]
async fn the_composition_reads_back_as_it_was_authored() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle(&harness).await;

    let component_plan = Uuid::now_v7();
    seed_draft_plan(&harness, component_plan).await;
    let row = seed_price(&harness, component_plan, "EU").await;
    harness.publish(component_plan, 0).await;
    harness.publish_price(component_plan, row.price_id).await;
    let vendor_sku = Uuid::now_v7();

    let tag = harness.plan_etag(plan_id).await;
    let written = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({
                "plan_revision": 0,
                "components": [{
                    "component_plan_id": component_plan,
                    "included_sku_id": vendor_sku,
                    "min_qty": 1,
                }],
                "rev_share": [{
                    "vendor_sku_id": vendor_sku,
                    "platform_cut_bp": 1500,
                    "parties": [{ "party": "acme-vendor", "share_bp": 8500 }],
                }],
            })),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(written.status(), StatusCode::OK);

    let read = harness
        .allowed()
        .send(with_headers("GET", &bundle_path(bundle_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let view = body_json(read).await;

    assert_eq!(view["bundle_id"], bundle_id.to_string());
    assert_eq!(view["plan_id"], plan_id.to_string());
    assert_eq!(view["plan_revision"], 0);
    assert_eq!(view["price_basis"], "sum_of_parts");

    assert_eq!(
        view["components"][0]["component_plan_id"],
        component_plan.to_string()
    );
    assert_eq!(view["components"][0]["min_qty"], 1);
    // The half that is third-party money.
    assert_eq!(view["rev_share"][0]["platform_cut_bp"], 1500);
    assert_eq!(view["rev_share"][0]["parties"][0]["party"], "acme-vendor");
    assert_eq!(view["rev_share"][0]["parties"][0]["share_bp"], 8500);
}

/// A bundle of another tenant reads exactly like an absent one.
#[tokio::test]
async fn a_foreign_bundle_is_a_404_rather_than_a_403() {
    let harness = Harness::new().await;
    let (_, bundle_id) = seed_bundle(&harness).await;

    let response = harness
        .other_tenant()
        .send(with_headers("GET", &bundle_path(bundle_id), None, &[]))
        .await;

    // **One answer, asserted.** This was a disjunction over the two answers the
    // contract must *choose between* — `status == NOT_FOUND || status == FORBIDDEN`
    // — which detects a change in neither direction and made the test's own name
    // ("rather than a 403") unlocked by its body.
    //
    // The answer is the 404, and the reason is the one `rest_plans`' pair records:
    // the PDP allows this caller in their own tenant, so the gate passes and the
    // compiled scope binds `tenant_id` in SQL, where the neighbour's bundle is not.
    // A foreign row therefore reads exactly like an absent one, which is the
    // property that stops a 403 confirming the row exists.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The positive control: without it a route that 404'd everything would pass.
    let own = harness
        .allowed()
        .send(with_headers("GET", &bundle_path(bundle_id), None, &[]))
        .await;
    assert_eq!(
        own.status(),
        StatusCode::OK,
        "the owner reads the same bundle, so the 404 above is about the caller"
    );
}

/// **The SQL tenant predicate on the bundle entrance.**
///
/// `rest_authz.rs`'s census cannot reach it: the bundle its seed leaves references
/// no published component, so the **owner** is answered `COMPONENT_UNPUBLISHED`
/// there and the route is listed in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE`. Here
/// the world is `a_clean_publish_is_accepted_and_opens_the_always_material_unit`'s
/// — an `own_price` composition with nothing to refuse — so the owner's identical
/// call is accepted and a refusal of the foreign caller means tenancy rather than a
/// rule.
///
/// `publish_bundle` asks the PDP with `resource_id: None`, so the gate is
/// tenant-wide by construction and the compiled scope's `tenant_id` predicate on
/// `plan_of` is the **whole** of this door's object-level authority.
/// `Harness::denied` and `Harness::scope_mismatch` hand it no caller-supplied id of
/// another tenant's row at all.
#[tokio::test]
async fn a_foreign_tenant_cannot_publish_this_tenants_bundle() {
    let harness = Harness::new().await;
    let (plan_id, bundle_id) = seed_bundle_with(&harness, "own_price").await;
    let tag = harness.plan_etag(plan_id).await;
    let composed = harness
        .allowed()
        .send(with_headers(
            "PATCH",
            &bundle_path(bundle_id),
            Some(serde_json::json!({ "plan_revision": 0, "components": [] })),
            &[("if-match", &tag)],
        ))
        .await;
    assert_eq!(
        composed.status(),
        StatusCode::OK,
        "the composition must land for the owner's control below to be an acceptance"
    );

    let publish = |id: Uuid| {
        with_headers(
            "POST",
            &publish_path(id),
            Some(serde_json::json!({ "plan_revision": 0, "markets": [] })),
            &[],
        )
    };
    rest_support::foreign_is_indistinguishable(
        &harness,
        publish(bundle_id),
        publish(Uuid::now_v7()),
    )
    .await;

    // The control, and it is what makes the two refusals mean anything. Last,
    // because it opens the always-material unit.
    let owner = harness.allowed().send(publish(bundle_id)).await;
    let status = owner.status();
    let body = body_json(owner).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "the owner's identical publish must be accepted, or the refusals above are about the \
         composition rather than about the tenant: {body}"
    );
}
