//! The mass-repricing run's two surfaces, over HTTP
//! (`design/12-operator-efficiency.md` §5, `inst-mr-api`, `inst-mr-journal`,
//! `inst-mr-return`, `inst-mp-grandfathered`; D-88, D-134, D-261, D-307).
//!
//! Every case asserts what the **store** holds and not only what the response
//! said. A `202` carrying a journal assembled from the caller's own request would
//! be a receipt for a freeze that may not have happened — the defect D-225 records
//! for the overlay submit — so the frozen row set is read back through the `GET`,
//! which is also `inst-mr-return`'s progress endpoint and therefore the thing
//! under test rather than a convenience. The two edges out of `validating`
//! (`inst-mr-coalesce`) are asserted the same way: on the **stored** run and the
//! **stored** approval unit, never on the response body alone — a route that
//! answered a materiality literal it had never computed would still pass a test
//! that only read the response, which is exactly the defect a 2026-08-10 review
//! records for the bundle publish's own materiality assertion.
//!
//! **No apply runs on a request here**, because none does in production either:
//! both surfaces that accept a run enqueue its apply on
//! `infra::repricing::RunApplyLane`, and this harness runs no lifecycle. A case
//! whose subject is the apply therefore calls `Harness::drain_repricing_applies`
//! between the `POST` (or the approve) and its assertion, and asserts the drain's
//! **count** as well as the run's state: a run left at `committing` is what a
//! handler that enqueued nothing and a drain that applied nothing both leave
//! behind, so the state alone cannot tell those apart from a real apply.
//!
//! **No case in this file configures a threshold policy unless its own name says
//! so.** A fresh harness has none, so `inst-mat-failsafe` makes every ordinary run
//! here material and therefore `awaiting_approval` — which is what lets the cases
//! that predate the two edges keep asserting the row set and the audit record
//! without also standing up a policy fixture for a fact they are not about.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::body::Body;
use axum::http::{Response, StatusCode};
use bss_pricing::api::rest::repricing_runs::REPRICING_RUNS;
use bss_pricing::authz::{actions, labels};
use bss_pricing::domain::bulk::BulkState;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::RateMinor;
use bss_pricing::domain::scope_key::{Cohort, PriceEligibility};

use bss_pricing::domain::instant::format_rfc3339;
use bss_pricing::domain::instant::utc_ymd_hms;
use rest_support::{
    Harness, approval_rows, approve_threshold_policy, body_json, bulk_operation_row, price_rows,
    problem_code, seed_current_plan, seed_current_plan_with_phase, seed_per_unit_rate_row,
    seed_price, seed_price_keyed, seed_priced_row, with_headers,
};
use time::OffsetDateTime;
use uuid::Uuid;

/// The minted `operation_id` off an accepted run's `202`, with what that response
/// says about the run's state pinned on the way past.
///
/// **The state is part of what the `POST` answers**, so it is asserted where the id
/// is read: an auto-publishable run is handed back at `committing`, its apply
/// enqueued on `RunApplyLane` and not yet run. A case that read only the id would
/// pass against a handler that answered a terminal state it had never reached, and
/// against one that answered `committing` and enqueued nothing — which is why every
/// case below pairs this with the drain's own count.
async fn accepted_committing_run(response: Response<Body>) -> Uuid {
    let view = body_json(response).await;
    assert_eq!(
        view["state"],
        serde_json::json!("committing"),
        "an auto-publishable run stands at `committing` when the POST answers, because its \
         apply is the lane's: {view}"
    );
    view["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .parse()
        .expect("a uuid")
}

/// Far enough out that no wall clock reaches it, the fixtures' standing rule. It
/// matters more here than elsewhere: the changeover is judged against `OffsetDateTime::now_utc()`
/// at every submit, so a relatively-dated instant would make this suite go red on
/// its own one day.
const CHANGEOVER: &str = "2099-08-20T00:00:00.000000Z";

fn run_path(run_id: Uuid) -> String {
    format!("{REPRICING_RUNS}/{run_id}")
}

/// A whole-currency run: `discount 5%`, one axis named.
fn a_run(run_id: Uuid, selector: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "run_id": run_id,
        "selector": selector,
        "adjustment": {
            "adjustment_kind": "discount",
            "magnitude_kind": "percent_bp",
            "adjustment_value": 500,
        },
        "changeover": CHANGEOVER,
    })
}

/// A published row on a named region of a named plan, which is what a run selects.
async fn a_published_row(harness: &Harness, plan: Uuid, region: &str) -> Uuid {
    let row = seed_price(harness, plan, region).await;
    harness.publish_price(plan, row.price_id).await;
    row.price_id
}

#[tokio::test]
async fn a_run_opens_over_the_published_rows_and_freezes_them_pending() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let eu = a_published_row(&harness, plan, "eu").await;
    let us = a_published_row(&harness, plan, "us").await;
    // A **draft** row on the same plan and currency. It must not be selected: the
    // run's domain is the published plane (D-118 gives the draft plane to the bulk
    // import), and a draft in the journal would be a row the apply would supersede
    // without it ever having been current on its key.
    let draft = seed_price(&harness, plan, "apac").await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::ACCEPTED, "inst-mr-return");
    let body = body_json(response).await;
    assert_eq!(body["run_id"], serde_json::json!(run_id.to_string()));
    assert_eq!(
        body["state"],
        serde_json::json!("awaiting_approval"),
        "a fresh harness configures no threshold policy, so `inst-mat-failsafe` makes this run \
         material and it leaves `validating` for `awaiting_approval` on the same request: {body}"
    );

    // The answer lives at the progress endpoint, and this is where the freeze is
    // actually observed: the `POST`'s own journal is read from the store too, so
    // both renderings come from the row set that landed.
    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let run = body_json(read).await;
    let journal = run["journal"].as_array().expect("a journal");
    let selected: Vec<&str> = journal
        .iter()
        .map(|row| row["price_id"].as_str().expect("an id"))
        .collect();
    let mut expected = vec![eu.to_string(), us.to_string()];
    expected.sort();
    assert_eq!(selected, expected, "{run}");
    assert!(
        !selected.contains(&draft.price_id.to_string().as_str()),
        "a draft row is not a repricing run's business: {run}"
    );
    assert!(
        journal
            .iter()
            .all(|row| row["state"] == serde_json::json!("pending")),
        "every frozen row is born pending (D-261): {run}"
    );

    // The report is the run's frozen parameters, rendered from what was parsed —
    // the apply reads them from here, because `pricing_bulk_operation` has no
    // other column for them.
    assert_eq!(run["report"]["selected"], serde_json::json!(2));
    assert_eq!(
        run["report"]["selector"]["currency"],
        serde_json::json!("USD")
    );
    assert_eq!(
        run["report"]["adjustment"]["adjustment_kind"],
        serde_json::json!("discount")
    );
    assert_eq!(
        run["report"]["adjustment"]["adjustment_value"],
        serde_json::json!(500)
    );
    // The report is what `repricing_run_content_hash` pins, so its bytes are the
    // gear's one rendering — the same one the wire uses. It was chrono's
    // `to_rfc3339()` here and `Z` everywhere else, kept apart so a unit opened
    // before the type change still digested to its `content_hash`; that premise
    // was retired with the third renderer, and the pin is now over the form every
    // other instant in the document already had.
    assert_eq!(
        run["report"]["changeover"],
        serde_json::json!("2099-08-20T00:00:00.000000Z"),
        "{run}"
    );
}

#[tokio::test]
async fn a_selector_that_matches_nothing_is_refused_and_opens_no_run() {
    // §5's `RUN_SELECTOR_EMPTY`. The refusal exists because the *success* is
    // indistinguishable from it: a run is complete when no `pending` rows remain,
    // so a run over an empty set is complete at birth and would report a mass
    // adjustment that never happened.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(
                run_id,
                &serde_json::json!({ "region": "antarctica" }),
            )),
            &[],
        ))
        .await;

    // A 400 and not a 422: Foundation section 3.3 gives the platform no 422
    // category, and the code is the discriminator.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "RUN_SELECTOR_EMPTY",
        "the declared code names what happened, and equality is what says it is \
         *this* code rather than one carrying it as a substring"
    );

    // **And the `run_id` is not spent.** This is the half a status assertion
    // cannot see: the refusal happens before anything opens, so a corrected
    // selector may reuse the id — which is the whole reason for refusing ahead of
    // the open rather than opening a run and failing it.
    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(
        read.status(),
        StatusCode::NOT_FOUND,
        "a refused selector opened nothing under this run_id"
    );
}

/// `time::serde::rfc3339` keeps the offset a caller wrote and range-checks only
/// the local date-time. The UTC form of this one is year 10000, which `time`
/// cannot hold, so before `domain::instant::rfc3339` the value passed every
/// floor (it is in the future) and the first render — `frozen_report` — panicked
/// the handler. The boundary refuses it as an unreadable body.
#[tokio::test]
async fn a_changeover_with_no_utc_form_is_refused_as_unreadable_not_answered_500() {
    let harness = Harness::new().await;
    let run_id = Uuid::now_v7();
    let mut body = a_run(run_id, &serde_json::json!({ "currency": "USD" }));
    body["changeover"] = serde_json::json!("9999-12-31T23:00:00-05:00");

    let response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(body), &[]))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await;
    assert!(
        problem.to_string().contains("no UTC representation"),
        "the refusal names the reason: {problem}"
    );
}

#[tokio::test]
async fn a_changeover_that_is_not_in_the_future_is_refused_at_the_submit_floor() {
    // `inst-mr-api` gives the run's changeover `inst-su-instant`'s floors, and this
    // is the submit one: strictly future. The commit floor — a whole batching delay
    // ahead — belongs to an approval commit that does not exist yet, so a run is
    // held to the lenient bound here and the strict one is owed by the apply.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let run_id = Uuid::now_v7();
    let mut body = a_run(run_id, &serde_json::json!({ "currency": "USD" }));
    body["changeover"] = serde_json::json!(format_rfc3339(utc_ymd_hms(2020, 1, 1, 0, 0, 0)));

    let response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(body), &[]))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "SUPERSESSION_INSTANT_PASSED",
        "the floor is the supersession's, and so is its code -- no second code over one bound"
    );

    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(
        read.status(),
        StatusCode::NOT_FOUND,
        "a run refused at the floor opened nothing"
    );
}

#[tokio::test]
async fn a_second_post_under_one_run_id_answers_the_run_it_opened() {
    // §5's Idempotency cell for this surface is `run_id`, so the replay is a read
    // of the run's own unique `(tenant, kind, client_key)` rather than a dedup
    // row — `bulk_imports`' arrangement, one path over.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;
    a_published_row(&harness, plan, "us").await;

    let run_id = Uuid::now_v7();
    let first = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first = body_json(first).await;

    // The retry names a **different selector**, which is what makes this a replay
    // test rather than a repeat: the answer must be the run that was opened, not a
    // run over the set the second request described.
    let second = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "region": "eu" }))),
            &[],
        ))
        .await;
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let second = body_json(second).await;

    assert_eq!(
        second["operation_id"], first["operation_id"],
        "one run_id opens one run"
    );
    assert_eq!(
        second["journal"].as_array().expect("a journal").len(),
        2,
        "the replayed journal is the frozen one, not one re-expanded from the retry: {second}"
    );
    assert_eq!(
        second["report"]["selector"]["region"],
        serde_json::Value::Null,
        "and the frozen report is the first call's: {second}"
    );

    // The freeze happened once. Read back through the store's own ordering, a
    // second `open_rows` over the same set would have collided on the journal's
    // primary key and a *different* set would have doubled the row count.
    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    let run = body_json(read).await;
    assert_eq!(run["journal"].as_array().expect("a journal").len(), 2);
}

#[tokio::test]
async fn the_grandfathered_class_is_excluded_unless_the_selector_names_it() {
    // `inst-mp-grandfathered` clause 1. A grandfathered row is immutable in price
    // (Foundation section 4.3), so an operator repricing "all USD rows" has not
    // asked to break a retention promise -- the class is excluded structurally.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let ordinary = a_published_row(&harness, plan, "eu").await;

    let generation = utc_ymd_hms(2026, 1, 1, 0, 0, 0);
    let retained = seed_price_keyed(
        &harness,
        plan,
        "eu-2025",
        PriceEligibility::ExistingGrandfathered,
        Cohort::Generation(generation),
    )
    .await;
    harness.publish_price(plan, retained.price_id).await;
    assert_eq!(
        retained.scope_key.price_eligibility(),
        PriceEligibility::ExistingGrandfathered,
        "the fixture really is on the retained class; `ScopeKey::new` is what pairs it with the \
         cohort, and a fixture on the wrong class would make both halves below vacuous"
    );

    let wide = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(wide, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    let selected: Vec<&str> = body["journal"]
        .as_array()
        .expect("a journal")
        .iter()
        .map(|row| row["price_id"].as_str().expect("an id"))
        .collect();
    assert_eq!(
        selected,
        vec![ordinary.to_string()],
        "the retained generation is not in a run that did not name its class: {body}"
    );

    // Clause 2's other half: naming the class **selects** those rows rather than
    // dropping them, because dropping them is the silent skip the clause forbids.
    // The per-row refusal is owed by the apply -- a journal row cannot be born
    // `failed` (D-261) -- so what this asserts is that the rows reach the journal
    // where that refusal will be written, and not that they are refused today.
    let narrow = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(
                narrow,
                &serde_json::json!({ "price_eligibility": "existing_grandfathered" }),
            )),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    let selected: Vec<&str> = body["journal"]
        .as_array()
        .expect("a journal")
        .iter()
        .map(|row| row["price_id"].as_str().expect("an id"))
        .collect();
    assert_eq!(
        selected,
        vec![retained.price_id.to_string()],
        "an explicit inclusion is never a silent skip: {body}"
    );
}

#[tokio::test]
async fn a_cohort_without_its_class_is_refused_with_the_reason_the_axes_do_not_show() {
    // The one selector that is empty **by construction** rather than by what the
    // tenant published: a cohort exists only on the retained class, and the
    // retained class is excluded until `price_eligibility` names it. The two rules
    // live in different documents, so a bare "matched nothing" would send its
    // author hunting the catalog for rows that are sitting right there -- which is
    // the failure mode `RUN_SELECTOR_EMPTY` shares with every count-based refusal.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let generation = utc_ymd_hms(2026, 1, 1, 0, 0, 0);
    let retained = seed_price_keyed(
        &harness,
        plan,
        "eu-2025",
        PriceEligibility::ExistingGrandfathered,
        Cohort::Generation(generation),
    )
    .await;
    harness.publish_price(plan, retained.price_id).await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(
                Uuid::now_v7(),
                &serde_json::json!({ "cohort": format_rfc3339(generation) }),
            )),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(response).await.to_string();
    assert!(
        problem.contains("RUN_SELECTOR_EMPTY") && problem.contains("existing_grandfathered"),
        "the refusal names the axis that would make this selector reachable: {problem}"
    );

    // And the same cohort **with** the class is the request they meant, which is
    // what makes the sentence above advice rather than an apology.
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(
                Uuid::now_v7(),
                &serde_json::json!({
                    "cohort": format_rfc3339(generation),
                    "price_eligibility": "existing_grandfathered",
                }),
            )),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(
        body["journal"]
            .as_array()
            .expect("a journal")
            .first()
            .expect("one row")["price_id"],
        serde_json::json!(retained.price_id.to_string()),
        "{body}"
    );
}

#[tokio::test]
async fn an_unknown_axis_token_is_refused_before_the_selector_is_ever_expanded() {
    // A token outside the eligibility enumeration would otherwise match no row and
    // be reported `RUN_SELECTOR_EMPTY`, which sends an operator hunting for missing
    // rows when what they have is a typo. The axis is read through the same
    // `optional_token` the interactive plane uses, so the refusal enumerates the
    // admitted values.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(
                Uuid::now_v7(),
                &serde_json::json!({ "price_eligibility": "everyone" }),
            )),
            &[],
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The **whole** problem document, not one member of it: this asserts an
    // absence, and reading a single field would let the code it denies sit in a
    // sibling one -- which is where `RUN_SELECTOR_EMPTY` actually renders.
    let problem = body_json(response).await.to_string();
    assert!(
        !problem.contains("RUN_SELECTOR_EMPTY") && !problem.contains("matched no published"),
        "a malformed axis is a malformed request, not an empty result: {problem}"
    );
    assert!(
        problem.contains("existing_grandfathered"),
        "and the refusal enumerates the tokens the axis admits, from the same slice the store's \
         CHECK was written against: {problem}"
    );
}

#[tokio::test]
async fn a_fixed_adjustment_declared_percent_bp_is_refused_by_the_shared_parser() {
    // D-138, through `domain::overlay::adjustment_of` -- the same function the overlay
    // line parser calls. It is asserted here and not only there because a run's
    // adjustment is **not** persisted as an overlay line, so none of the store
    // constraints that back the overlay plane's parse are behind this one: a copy
    // of the parse that drifted would drift silently.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    // Bound to a local, so the id can be read back below. It was inline until
    // 2026-08-20, which is what made the second half of this case unaskable.
    let run_id = Uuid::now_v7();
    let mut body = a_run(run_id, &serde_json::json!({ "currency": "USD" }));
    body["adjustment"]["adjustment_kind"] = serde_json::json!("fixed");

    let response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(body), &[]))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // **Which 400.** `adjustment_of` raises a bare `InvalidRequest` and therefore
    // renders no wire code — `problem_code` panics on it — so the discriminator is
    // the sentence D-138 is stated in. Several other rules on this route answer the
    // same status (an unknown `adjustment_kind` token, an unknown `magnitude_kind`,
    // an `amount` carrying a value, the selector and changeover floors), and the
    // status alone was the whole of this case.
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("a fixed line is always amount-based")),
        "D-138's own refusal answered, not another of this route's 400s: {problem}"
    );

    // **And the `run_id` is not spent**, the half a status assertion cannot see —
    // `a_selector_that_matches_nothing_is_refused_and_opens_no_run` and
    // `a_changeover_that_is_not_in_the_future_is_refused_at_the_submit_floor` both
    // ask this, and this case could not. If the D-138 parse ever moved to after
    // `open_repricing_run`, the id would be permanently spent against
    // `uq_pricing_bulk_operation_client_key` and an operator correcting the
    // adjustment could never reuse it.
    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(
        read.status(),
        StatusCode::NOT_FOUND,
        "a refused adjustment opened nothing under this run_id"
    );
}

#[tokio::test]
async fn one_run_id_opens_one_repricing_run_and_one_bulk_import_alike() {
    // D-307's index half, over HTTP. The two flows have two different idempotency
    // columns (§5), so one token may name one of each -- and a `find_by_client_key`
    // missing its `kind` filter would answer this `GET` with the *import*, which
    // carries no `kind` member on either view to reveal the substitution.
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let shared = Uuid::now_v7();
    let import = harness
        .allowed()
        .send(with_headers(
            "POST",
            bss_pricing::api::rest::bulk_imports::BULK_IMPORTS,
            Some(serde_json::json!({ "rows": [] })),
            &[("idempotency-key", shared.to_string().as_str())],
        ))
        .await;
    assert_eq!(import.status(), StatusCode::ACCEPTED);
    let import = body_json(import).await;

    let run = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(shared, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(run.status(), StatusCode::ACCEPTED);
    let run = body_json(run).await;

    assert_ne!(
        run["operation_id"], import["operation_id"],
        "one client key, two flows, two runs"
    );

    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(shared), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(
        body_json(read).await["operation_id"],
        run["operation_id"],
        "and the progress endpoint answers the repricing run, never the import"
    );
}

/// The debt `api::rest::repricing_runs`' own module doc named: until
/// `AuditSubjectKind` carried a `BulkOperation` member, opening a run wrote **no**
/// audit record at all. `subject_ref` is asserted equal to
/// `audit_repo::bulk_operation_ref(operation_id)` rather than merely "a row
/// exists" — a record naming the wrong subject would verify perfectly and tell an
/// auditor nothing true.
#[tokio::test]
async fn opening_a_run_writes_an_audit_record_naming_the_operation_id() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    let operation_id = body["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .to_owned();

    // A fresh harness configures no threshold policy, so `inst-mat-failsafe`
    // makes this run material and it opens an approval unit too — which appends
    // its **own** `bulk_operation`-subject `submit` record on the same chain
    // (D-158). Filtered to `create` because that is the one this case is about;
    // the `submit` record is `a_material_run_...`'s own assertion.
    let bulk_records: Vec<_> = rest_support::audit_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == "bulk_operation" && row.action == "create")
        .collect();
    assert_eq!(
        bulk_records.len(),
        1,
        "one create record for the one run this test opened: {bulk_records:?}"
    );
    // The action is the filter's, so it is not re-asserted here: a route that wrote
    // `update` leaves `bulk_records` empty and the length check above is what says
    // so.
    let record = &bulk_records[0];
    assert_eq!(
        record.subject_ref, operation_id,
        "the record must name the run it is about, not merely exist"
    );
    assert!(
        record.before_state.is_none(),
        "a run's open has no before-state"
    );
    assert_eq!(
        record
            .after_state
            .as_ref()
            .and_then(|state| state.get("kind")),
        Some(&serde_json::json!("repricing"))
    );
}

/// `inst-mr-coalesce`'s material edge: `validating -> awaiting_approval` under an
/// opened approval unit. A fresh harness configures no threshold policy, so
/// `inst-mat-failsafe` is what makes this run material — asserted on the
/// **stored** run (`bulk_operation_row`, a repository read, never the `POST`'s
/// own response) and the **stored** approval unit (`approval_rows`), per the
/// review finding this suite's own module doc names: a response-only assertion
/// cannot tell a handler that actually evaluated materiality from one that
/// answers a literal.
#[tokio::test]
async fn a_material_run_leaves_validating_for_awaiting_approval_with_an_approval_unit_open() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    // The response supplies only the id the store is then read back by — never
    // the fact under test.
    let operation_id = body_json(response).await["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .to_owned();

    let stored = bulk_operation_row(&harness, operation_id.parse().expect("a uuid")).await;
    assert_eq!(
        stored.state,
        BulkState::AwaitingApproval,
        "no policy is configured, so the fail-safe trips: {stored:?}"
    );

    let units: Vec<_> = approval_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == "bulk_operation" && row.subject_ref == operation_id)
        .collect();
    assert_eq!(
        units.len(),
        1,
        "one approval unit for the one material run this test opened: {units:?}"
    );
    let unit = &units[0];
    assert_eq!(unit.state, "submitted");
    assert_eq!(
        unit.materiality.get("reason"),
        Some(&serde_json::json!("noConfiguredThreshold")),
        "the reason a reviewer would read: {unit:?}"
    );
}

/// `inst-mr-coalesce`'s non-material edge: `validating -> committing`, with **no**
/// approval unit opened, and then `committing -> completed` once the lane applies
/// the run. A test that only proved the material run above stops would pass against
/// a handler that stops every run; this one fails such a handler, because it asserts
/// that the `POST` hands back `committing`, that the drain found an apply to run,
/// and that the approval store holds nothing for it.
#[tokio::test]
async fn a_non_material_run_leaves_validating_for_committing_with_no_approval_unit_open() {
    let harness = Harness::new().await;
    // Any bar at all is below nothing for a zero-delta act (`rest_support`'s own
    // doc on `approve_threshold_policy`): what matters is only that USD has an
    // entry, not its size.
    approve_threshold_policy(&harness, &[("USD", 1_000_000)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(&harness, plan).await;
    // `seed_price`/`a_published_row` leave `amount_minor` unset, which is a real
    // `NotComputable("amount_minor")` delta and would make this run material via
    // `alwaysMaterialTrigger` whatever the threshold says — the positive control
    // needs a row the per-currency comparison can actually compare.
    let priced = seed_priced_row(&harness, plan, "eu", 9_900).await;
    harness.publish_price(plan, priced.price_id).await;
    // `inst-wc-required`, via the apply's own aggregate pass: a supersession
    // presupposes current coverage, and `harness.publish_price` — the raw
    // `publish_rows` door, not the full pipeline — schedules none. Every
    // fixture in this crate that is meant to publish **cleanly** through the
    // real pipeline calls this (`tests/common/mod.rs`'s own module doc).
    common::schedule_coverage_window(
        &harness.db.conn().expect("conn"),
        &harness.scope(),
        harness.tenant,
        priced.price_id,
        rest_support::seed_stamp(),
    )
    .await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id = accepted_committing_run(response).await;
    // **The apply is the lane's, and this is where it runs.** The count is the
    // discrimination the assertion below cannot make on its own: a run standing at
    // `committing` is what a handler that enqueued nothing and a drain that applied
    // nothing both leave behind.
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the POST enqueued exactly one apply"
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::Completed,
        "USD has a configured entry and the zero-delta act never reaches it, so the run is \
         auto-publishable, and the apply the lane ran has nothing to refuse it on: {stored:?}"
    );

    let units: Vec<_> = approval_rows(&harness)
        .await
        .into_iter()
        .filter(|row| {
            row.subject_kind == "bulk_operation" && row.subject_ref == operation_id.to_string()
        })
        .collect();
    assert!(
        units.is_empty(),
        "an auto-publishable run opens no approval unit: {units:?}"
    );
}

/// `inst-mr-coalesce`'s own claim, armed directly: *"any row over its
/// own-currency threshold trips the run"* — the run's own **adjustment
/// magnitude**, not merely whether a policy exists. One fixture (one seeded
/// row, one configured threshold) carries two runs that differ only in the
/// `percent_bp` they submit: the smaller stays under the bar and the larger
/// crosses it. A change set built from zero-delta rows would answer
/// `noConfiguredThreshold` or `autoPublishable` identically for both, whatever
/// the adjustment said — which is exactly the gap a 2026-08-11 review found in
/// this suite's first draft, before either case here existed.
#[tokio::test]
async fn the_runs_own_adjustment_magnitude_against_the_configured_threshold_decides_materiality() {
    let harness = Harness::new().await;
    // $10.00 in USD's own currency: below the row's 5% move (500 minor) and
    // above nothing a discount could reach — the bar the pair straddles.
    approve_threshold_policy(&harness, &[("USD", 1_000)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(&harness, plan).await;
    let priced = seed_priced_row(&harness, plan, "eu", 10_000).await;
    harness.publish_price(plan, priced.price_id).await;
    // `inst-wc-required`: the under-the-bar run below is non-material, so the lane
    // applies it, and the row needs real coverage for that apply to have anything to
    // supersede.
    common::schedule_coverage_window(
        &harness.db.conn().expect("conn"),
        &harness.scope(),
        harness.tenant,
        priced.price_id,
        rest_support::seed_stamp(),
    )
    .await;

    // Under the bar: 5% of 10_000 is 500 minor, and 500 < 1_000. Run first,
    // while nothing holds the row's key.
    let under_id = Uuid::now_v7();
    let mut under_body = a_run(under_id, &serde_json::json!({ "currency": "USD" }));
    under_body["adjustment"]["adjustment_kind"] = serde_json::json!("markup");
    under_body["adjustment"]["adjustment_value"] = serde_json::json!(500);
    let under_response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(under_body), &[]))
        .await;
    assert_eq!(under_response.status(), StatusCode::ACCEPTED);
    let under_operation_id = accepted_committing_run(under_response).await;
    // **The apply is the lane's, and this is where it runs.** The count is the
    // discrimination the assertion below cannot make on its own: a run standing at
    // `committing` is what a handler that enqueued nothing and a drain that applied
    // nothing both leave behind.
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the POST enqueued exactly one apply"
    );

    let under_stored = bulk_operation_row(&harness, under_operation_id).await;
    assert_eq!(
        under_stored.state,
        BulkState::Completed,
        "500 minor is under the 1_000 bar, so the run is auto-publishable and the apply the \
         lane ran has nothing to refuse it on: {under_stored:?}"
    );
    let under_units: Vec<_> = approval_rows(&harness)
        .await
        .into_iter()
        .filter(|row| {
            row.subject_kind == "bulk_operation"
                && row.subject_ref == under_operation_id.to_string()
        })
        .collect();
    assert!(
        under_units.is_empty(),
        "under the bar opens no unit: {under_units:?}"
    );

    // Over the bar: 20% of 10_000 is 2_000 minor, and 2_000 >= 1_000. Same row,
    // same policy — only the adjustment's own size differs from the case above.
    let over_id = Uuid::now_v7();
    let mut over_body = a_run(over_id, &serde_json::json!({ "currency": "USD" }));
    over_body["adjustment"]["adjustment_kind"] = serde_json::json!("markup");
    over_body["adjustment"]["adjustment_value"] = serde_json::json!(2_000);
    let over_response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(over_body), &[]))
        .await;
    assert_eq!(over_response.status(), StatusCode::ACCEPTED);
    let over_operation_id = body_json(over_response).await["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .to_owned();

    let over_stored =
        bulk_operation_row(&harness, over_operation_id.parse().expect("a uuid")).await;
    assert_eq!(
        over_stored.state,
        BulkState::AwaitingApproval,
        "2_000 minor reaches the 1_000 bar: {over_stored:?}"
    );
    let over_units: Vec<_> = approval_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == "bulk_operation" && row.subject_ref == over_operation_id)
        .collect();
    assert_eq!(
        over_units.len(),
        1,
        "over the bar opens exactly one unit: {over_units:?}"
    );
    assert_eq!(
        over_units[0].materiality.get("reason"),
        Some(&serde_json::json!("thresholdReached")),
        "the real per-row comparison tripped it, not the fail-safe: {:?}",
        over_units[0]
    );
    // The `minor`-scale half of the tripped document, whose companion is the
    // `nanoMinor` one in `a_per_unit_runs_rate_move_crosses_the_configured_threshold`:
    // a `flat` row's money **is** whole minor units, so the label is the move's
    // own fact rather than a constant either case could have hard-coded.
    //
    // The baseline is `10_500`, not the seeded `10_000`: the under-the-bar run
    // above **applied**, so the published occupant of the key by now is its
    // successor at `+5%` and the second run reprices that. Read from the store
    // rather than assumed, since the id is the successor's too.
    let repriced = price_rows(&harness, plan)
        .await
        .into_iter()
        .find(|row| row.supersedes_price_id == Some(priced.price_id))
        .expect("the under-the-bar run's apply wrote a successor");
    assert_eq!(
        over_units[0].materiality["tripped"],
        serde_json::json!({
            "price_id": repriced.price_id,
            "currency": "USD",
            "from_minor": 10_500,
            "to_minor": 12_600,
            "scale": "minor",
        }),
        "20% of the 10_500 now published is 12_600, in USD's own minor units, on the row that \
         holds the key: {:?}",
        over_units[0]
    );
}

/// Pins `project_amount`'s rounding **rule**, not merely its arithmetic —
/// half-to-even (`round_half_even`), not truncation. `10_007` minor at
/// `1_000 bp` (10%) is `1_000.7` minor: the one case the pair above cannot
/// reach, since `500`/`10_000` and `2_000`/`10_000` both divide evenly and no
/// rounding rule contributes any error either way. Here the remainder (`7_000`
/// of `10_000`) is past the halfway point, so half-to-even rounds the move up
/// to `1_001` and it **reaches** the `1_001` bar; truncation over the
/// identical arithmetic would have given `1_000` and stayed **under** it. The
/// threshold sits exactly on the half-to-even answer and one short of the
/// truncated one, so reverting the rounding rule reddens this test.
#[tokio::test]
async fn the_projected_amount_rounds_half_to_even_not_toward_zero() {
    let harness = Harness::new().await;
    approve_threshold_policy(&harness, &[("USD", 1_001)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let priced = seed_priced_row(&harness, plan, "eu", 10_007).await;
    harness.publish_price(plan, priced.price_id).await;

    let run_id = Uuid::now_v7();
    let mut body = a_run(run_id, &serde_json::json!({ "currency": "USD" }));
    body["adjustment"]["adjustment_kind"] = serde_json::json!("markup");
    body["adjustment"]["adjustment_value"] = serde_json::json!(1_000);
    let response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(body), &[]))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id = body_json(response).await["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .to_owned();

    let stored = bulk_operation_row(&harness, operation_id.parse().expect("a uuid")).await;
    assert_eq!(
        stored.state,
        BulkState::AwaitingApproval,
        "10_007 * 1_000 bp / 10_000 is exactly 1_000.7 minor: half-to-even rounds the move up to \
         1_001, which reaches the 1_001 bar, so this run is material. Truncation over the \
         identical arithmetic would have given 1_000 and stayed under it -- this assertion is the \
         rounding rule, not merely that some computation ran: {stored:?}"
    );

    let units: Vec<_> = approval_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == "bulk_operation" && row.subject_ref == operation_id)
        .collect();
    assert_eq!(
        units.len(),
        1,
        "the rounded-up move reaches the bar and opens exactly one unit: {units:?}"
    );
    assert_eq!(
        units[0].materiality.get("reason"),
        Some(&serde_json::json!("thresholdReached")),
        "the real per-row comparison tripped it: {:?}",
        units[0]
    );
}

// ---------------------------------------------------------------------------
// D-311's `per_unit` half: the money is a rate, and both the apply and the
// materiality verdict have to see it move.
// ---------------------------------------------------------------------------

/// The rate every case below seeds, in stored 10⁻⁹ minor units — `$0.230777165`
/// per unit.
///
/// Sub-minor-unit, the reason `RateMinor` exists at all, and deliberately
/// **not** a round count of nano-minor units: `23_077_701_650 x 700 bp` is
/// `1_615_439_115.5`, a remainder of exactly half over an **odd** quotient, so
/// half-to-even rounds the move up to `1_615_439_116` where truncation would
/// have stopped one nano-minor short. The pinned successor below therefore
/// separates four implementations — the no-op this defect was, a truncating
/// projection, one routed through `project_amount`'s coarse minor-unit scale,
/// and the correct one.
const PER_UNIT_RATE_NANO: i64 = 23_077_701_650;
/// `PER_UNIT_RATE_NANO` after `+7%`, half-to-even.
const PER_UNIT_RATE_AFTER_MARKUP_NANO: i64 = 24_693_140_766;
/// The `percent_bp` both cases submit. The move it produces is `1.615439116`
/// whole minor units, and the two bars below straddle it: `1` is crossed, `2`
/// is not.
const PER_UNIT_MARKUP_BP: i64 = 700;

/// A `+7%` run over one named currency.
fn a_markup_run(run_id: Uuid) -> serde_json::Value {
    let mut body = a_run(run_id, &serde_json::json!({ "currency": "USD" }));
    body["adjustment"]["adjustment_kind"] = serde_json::json!("markup");
    body["adjustment"]["adjustment_value"] = serde_json::json!(PER_UNIT_MARKUP_BP);
    body
}

/// **Consequence 1**: the apply writes a successor whose rate actually moved.
///
/// D-311 moved a `per_unit` row's money out of `amount_minor` into `unit_rate`
/// and made `amount_minor` NULL by rule on such a row
/// (`check_amount_placement`), while `project_row` went on repricing
/// `per_unit` through the shared `flat` arm — an `and_then` over a field that
/// is guaranteed absent, so it short-circuited and the row came back
/// unchanged. The apply then superseded a published row and published a
/// **byte-identical** successor in its place, writing the audit record, the
/// outbox events and a journal marked `applied` for a reprice that repriced
/// nothing.
///
/// The bar is `2` minor units and the move is `1.615439116`, so this run is under it
/// and non-material — which is what puts its apply on the lane rather than behind an
/// approval, and gives this case a successor to read once the drain has run it. The
/// assertion is on the **stored** successor's rate, never on the response: a run that
/// reported `completed` having written a copy of its predecessor passes every weaker
/// check.
#[tokio::test]
async fn a_per_unit_rows_rate_moves_through_the_apply() {
    let harness = Harness::new().await;
    // 2 minor units, in USD's own currency. `AmountMove::reaches_absolute`
    // raises the bar into the move's nano-minor scale (D-311), so the
    // comparison is `1.615439116 < 2` and the run is auto-publishable.
    approve_threshold_policy(&harness, &[("USD", 2)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(&harness, plan).await;
    let priced = seed_per_unit_rate_row(&harness, plan, "eu", PER_UNIT_RATE_NANO).await;
    harness.publish_price(plan, priced.price_id).await;
    // `inst-wc-required`: the apply supersedes, and a supersession presupposes
    // current coverage the raw publish door never schedules.
    common::schedule_coverage_window(
        &harness.db.conn().expect("conn"),
        &harness.scope(),
        harness.tenant,
        priced.price_id,
        rest_support::seed_stamp(),
    )
    .await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_markup_run(run_id)),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id = accepted_committing_run(response).await;
    // **The apply is the lane's, and this is where it runs.** The count is the
    // discrimination the assertion below cannot make on its own: a run standing at
    // `committing` is what a handler that enqueued nothing and a drain that applied
    // nothing both leave behind.
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the POST enqueued exactly one apply"
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::Completed,
        "1.615439116 minor units is under the 2-minor bar, so the run auto-publishes and the \
         apply the lane ran has nothing to refuse it on: {stored:?}"
    );

    let rows = price_rows(&harness, plan).await;
    let successor = rows
        .iter()
        .find(|row| row.supersedes_price_id == Some(priced.price_id))
        .unwrap_or_else(|| {
            panic!("the apply writes exactly one successor for the row it superseded: {rows:?}")
        });
    assert_eq!(
        successor.row.unit_rate.map(RateMinor::nano_minor),
        Some(PER_UNIT_RATE_AFTER_MARKUP_NANO),
        "+7% of {PER_UNIT_RATE_NANO} nano-minor is 1_615_439_115.5, half-to-even 1_615_439_116. A \
         successor still at {PER_UNIT_RATE_NANO} is the shared-`flat`-arm no-op this case exists \
         for -- an apply that superseded a published row and republished it unchanged while its \
         journal said `applied`: {successor:?}"
    );
    assert!(
        successor.row.amount_minor.is_none(),
        "the successor keeps its money in the one column its kind prices from: two priced columns \
         are AMOUNT_PLACEMENT_INVALID's two competing prices: {successor:?}"
    );
}

/// **Consequence 2**, and the one that reaches consumers approver-free: a
/// `per_unit` run whose rate move crosses the configured bar is judged
/// **material** and stops for a second principal.
///
/// `run_materiality` projects through the same `project_row`, so while that
/// function left `unit_rate` alone every `per_unit` row's delta was a rate
/// compared against itself — zero, under every bar — and the run skipped the
/// approval gate entirely on its way to `committing`. The fixture is the same
/// row and the same `+7%` as the case above; only the bar differs, `1` minor
/// unit instead of `2`, and `1.615439116 >= 1`. A projection that leaves the
/// rate published answers `autoPublishable` here whatever the bar says, so
/// this case is red against the defect and green only against a real
/// comparison.
#[tokio::test]
async fn a_per_unit_runs_rate_move_crosses_the_configured_threshold() {
    let harness = Harness::new().await;
    approve_threshold_policy(&harness, &[("USD", 1)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(&harness, plan).await;
    let priced = seed_per_unit_rate_row(&harness, plan, "eu", PER_UNIT_RATE_NANO).await;
    harness.publish_price(plan, priced.price_id).await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_markup_run(run_id)),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id = body_json(response).await["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .to_owned();

    let stored = bulk_operation_row(&harness, operation_id.parse().expect("a uuid")).await;
    assert_eq!(
        stored.state,
        BulkState::AwaitingApproval,
        "the rate moves 1.615439116 minor units and the bar is 1: a run that reached `committing` \
         here is one whose projection never moved the rate, so its delta was zero and it skipped \
         the approval gate on a change consumers do see: {stored:?}"
    );

    let units: Vec<_> = approval_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == "bulk_operation" && row.subject_ref == operation_id)
        .collect();
    assert_eq!(
        units.len(),
        1,
        "crossing the bar opens exactly one unit: {units:?}"
    );
    assert_eq!(
        units[0].materiality.get("reason"),
        Some(&serde_json::json!("thresholdReached")),
        "the real per-currency comparison tripped it, not the no-policy fail-safe -- a policy is \
         configured here precisely so the fail-safe cannot supply this verdict: {:?}",
        units[0]
    );
    // **The number the approver actually reads**, and the half this case was
    // missing: `reason` alone says a bar was reached and not by how much. The
    // two amounts are the rate's own nano-minor units (D-311), so the document
    // has to label them as such -- rendered under the `minor` label they read
    // as $230,777,016.50 -> $246,931,407.66, a factor of 10^9 out, on the one
    // screen the two-person rule exists to put in front of a second principal.
    assert_eq!(
        units[0].materiality["tripped"],
        serde_json::json!({
            "price_id": priced.price_id,
            "currency": "USD",
            "from_minor": PER_UNIT_RATE_NANO,
            "to_minor": PER_UNIT_RATE_AFTER_MARKUP_NANO,
            "scale": "nanoMinor",
        }),
        "the stored verdict carries the move that tripped, in the units it was measured in: {:?}",
        units[0]
    );
}

/// The refusal half, on `per_unit`'s field: an adjustment that computes **no**
/// rate mutation must fail the row rather than mark it `applied` over a
/// successor identical to its predecessor.
///
/// `project_rate` returns `None` for an `amount` markup/discount and for a
/// `fixed` line, and `project_row` reads that as "leave the rate published" —
/// which on the apply side is indistinguishable from the defect above unless
/// something refuses. `apply_rows_in` is that something, and its `matches!`
/// named `graduated`/`volume` only: the arm that would have caught `per_unit`
/// was never reached, because while `per_unit` shared `flat`'s arm no
/// adjustment of any kind moved such a row and the omission showed up as
/// nothing.
///
/// A `fixed` line is the sharpest case: it is the one adjustment that reads a
/// currency minor-unit literal as a price outright, and applied to a rate it
/// would silently reinterpret `$0.50` as a rate scale that can never express
/// the sub-minor-unit value the column exists for.
#[tokio::test]
async fn a_fixed_adjustment_fails_a_per_unit_row_rather_than_applying_nothing_to_it() {
    let harness = Harness::new().await;
    // A bar the zero delta this adjustment projects can never reach, so the run is
    // judged non-material and its apply goes on the lane rather than behind an
    // approval — which is what puts the refusal under test rather than the approval
    // gate.
    approve_threshold_policy(&harness, &[("USD", 1_000_000)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(&harness, plan).await;
    let priced = seed_per_unit_rate_row(&harness, plan, "eu", PER_UNIT_RATE_NANO).await;
    harness.publish_price(plan, priced.price_id).await;
    common::schedule_coverage_window(
        &harness.db.conn().expect("conn"),
        &harness.scope(),
        harness.tenant,
        priced.price_id,
        rest_support::seed_stamp(),
    )
    .await;

    let run_id = Uuid::now_v7();
    let mut body = a_run(run_id, &serde_json::json!({ "currency": "USD" }));
    body["adjustment"] = serde_json::json!({
        "adjustment_kind": "fixed",
        "magnitude_kind": "amount",
        "amounts": [{ "currency": "USD", "value_minor": 50 }],
    });
    let response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(body), &[]))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id = accepted_committing_run(response).await;
    // **The apply is the lane's, and this is where it runs.** The count is the
    // discrimination the assertion below cannot make on its own: a run standing at
    // `committing` is what a handler that enqueued nothing and a drain that applied
    // nothing both leave behind.
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the POST enqueued exactly one apply"
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::CompletedWithConflicts,
        "a row this run cannot honestly reprice is a conflict, not a success: {stored:?}"
    );

    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let run = body_json(read).await;
    let journal = run["journal"].as_array().expect("a journal");
    assert_eq!(journal.len(), 1, "one selected row: {run}");
    assert_eq!(
        journal[0]["state"],
        serde_json::json!("failed"),
        "the row is `failed`, and an `applied` here is the silent no-op this guard exists for -- \
         a successor byte-identical to its predecessor under a journal reporting success: {run}"
    );
    // Three spans and not a four-character needle: `rate` occurs in `rate-priced`,
    // in `unit rate`, and in the run's own prose, so a `contains("rate")` is
    // satisfied by a reason that names neither the row nor the remedy.
    let reason = journal[0]["failure_reason"]
        .as_str()
        .expect("a failed row carries its reason");
    assert!(
        reason.contains(&priced.price_id.to_string()),
        "the reason names the row that refused: {reason}"
    );
    assert!(
        reason.contains("hold a rate, not an amount"),
        "and why it refused, which is the thing the operator has to reconsider: {reason}"
    );
    assert!(
        reason.contains("Re-run with a percent_bp markup/discount"),
        "and the move that would work: the run refuses the whole plan, so without \
         it the operator is told what failed and not what to do instead: {reason}"
    );

    let rows = price_rows(&harness, plan).await;
    assert!(
        rows.iter()
            .all(|row| row.supersedes_price_id != Some(priced.price_id)),
        "a refused row is superseded by nothing: {rows:?}"
    );
    let predecessor = rows
        .iter()
        .find(|row| row.price_id == priced.price_id)
        .expect("the seeded row is still there");
    assert_eq!(
        predecessor.lifecycle_state,
        LifecycleState::Published,
        "and it is still the published occupant of its key: {predecessor:?}"
    );
    assert_eq!(
        predecessor.row.unit_rate.map(RateMinor::nano_minor),
        Some(PER_UNIT_RATE_NANO),
        "at the rate it was published with: {predecessor:?}"
    );
}

// ---------------------------------------------------------------------------
// D-134's failure unit, over the widened rate refusal: the plan, not the row.
// ---------------------------------------------------------------------------

/// The `flat` amount both cases below seed, and the `fixed` line they submit.
///
/// `9_900 -> 50` is a move of `9_850` minor units, which is real, large, and
/// still far under the `1_000_000` bar these cases configure — so the run is
/// auto-publishable and its apply goes on the lane rather than behind an approval,
/// which is what puts the *apply's* failure unit under test rather than the approval
/// gate.
const FLAT_AMOUNT_MINOR: i64 = 9_900;
/// The `fixed` line's literal. Not a divisor or multiple of
/// [`FLAT_AMOUNT_MINOR`] and not the projection of it under any percentage, so
/// the successor this value pins cannot also be produced by a `markup`,
/// a truncation, or a row left at its published amount.
const FIXED_LINE_MINOR: i64 = 50;

/// A `fixed $0.50` run over one named currency — the one adjustment kind
/// `project_rate` refuses outright on a rate.
fn a_fixed_run(run_id: Uuid) -> serde_json::Value {
    let mut body = a_run(run_id, &serde_json::json!({ "currency": "USD" }));
    body["adjustment"] = serde_json::json!({
        "adjustment_kind": "fixed",
        "magnitude_kind": "amount",
        "amounts": [{ "currency": "USD", "value_minor": FIXED_LINE_MINOR }],
    });
    body
}

/// **The positive control** for the mixed-plan case below, and the half that
/// makes it mean anything: on a plan holding **only** a `flat` row, this exact
/// `fixed` line applies.
///
/// Without this, "both rows failed" over the mixed plan would be satisfied by a
/// `flat` row that was never repriceable under a `fixed` adjustment in the
/// first place — the fixture-proves-nothing shape. Here the same row, the same
/// adjustment and the same bar produce `applied` and a successor at the
/// literal, so the *only* difference in the case below is the `per_unit` row
/// sitting beside it.
#[tokio::test]
async fn a_flat_row_alone_applies_the_same_fixed_line_the_mixed_plan_refuses() {
    let harness = Harness::new().await;
    approve_threshold_policy(&harness, &[("USD", 1_000_000)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(&harness, plan).await;
    let flat = seed_priced_row(&harness, plan, "us", FLAT_AMOUNT_MINOR).await;
    harness.publish_price(plan, flat.price_id).await;
    common::schedule_coverage_window(
        &harness.db.conn().expect("conn"),
        &harness.scope(),
        harness.tenant,
        flat.price_id,
        rest_support::seed_stamp(),
    )
    .await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_fixed_run(run_id)),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id = accepted_committing_run(response).await;
    // **The apply is the lane's, and this is where it runs.** The count is the
    // discrimination the assertion below cannot make on its own: a run standing at
    // `committing` is what a handler that enqueued nothing and a drain that applied
    // nothing both leave behind.
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the POST enqueued exactly one apply"
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::Completed,
        "a `fixed` line on a `flat` row is a well-defined reprice and nothing refuses it: {stored:?}"
    );

    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    let run = body_json(read).await;
    let journal = run["journal"].as_array().expect("a journal");
    assert_eq!(journal.len(), 1, "one selected row: {run}");
    assert_eq!(
        journal[0]["state"],
        serde_json::json!("applied"),
        "the control is the row applying: {run}"
    );

    let rows = price_rows(&harness, plan).await;
    let successor = rows
        .iter()
        .find(|row| row.supersedes_price_id == Some(flat.price_id))
        .unwrap_or_else(|| {
            panic!("the apply writes a successor for the row it superseded: {rows:?}")
        });
    assert_eq!(
        successor
            .row
            .amount_minor
            .map(bss_pricing::domain::money::MinorAmount::get),
        Some(FIXED_LINE_MINOR),
        "a `fixed` line sets the amount to its literal, so the control's successor is a real \
         reprice and not the published amount carried forward: {successor:?}"
    );
}

/// **D-134, `inst-mr-apply`**: the transaction unit is the **plan**, not the
/// row — *"a per-row validation failure fails **every** row of that plan with
/// the shared reason — never a partial plan"*
/// (`design/12-operator-efficiency.md` §5 `inst-mr-apply`, and the Mass
/// Repricing `DoD`'s own **MUST** in §10).
///
/// So the widened D-311 rate refusal is deliberately **not** the per-row
/// refusal `inst-mp-grandfathered` owes: that clause names its own unit in as
/// many words (*"fails **that row**"*, `inst-mp-grandfathered` clause 1a) and
/// the general rule governs everything it does not name. A mixed plan — a
/// `per_unit` row the run cannot honestly reprice beside a `flat` row it could
/// — therefore fails **whole**, and the `flat` row's successor is never
/// written even though the projection for it is well-defined. D-134's own
/// reason is why: the plan-level aggregate pass runs inside the plan's commit
/// transaction over the row set *as it will stand post-commit*, and a partial
/// plan is a set that pass never evaluated.
///
/// The other half this pins is the **shared reason**. Before this case the
/// refusal's rendering opened `price <id>:` and named the offending row alone,
/// which is then stamped onto every row of the plan — so the `flat` row's
/// journal entry read as a statement about itself, asserting that a row whose
/// money is an `amount_minor` holds a rate. An operator reading that entry
/// cannot tell why a row the run could reprice did not apply, which is the
/// whole of what the journal is for.
#[tokio::test]
async fn a_per_unit_refusal_fails_its_whole_plan_with_a_reason_that_says_so() {
    let harness = Harness::new().await;
    approve_threshold_policy(&harness, &[("USD", 1_000_000)]).await;
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(&harness, plan).await;
    // One plan, two kinds, on two regions of the one currency the selector
    // names — the mixed plan no single-row case can see.
    let per_unit = seed_per_unit_rate_row(&harness, plan, "eu", PER_UNIT_RATE_NANO).await;
    harness.publish_price(plan, per_unit.price_id).await;
    let flat = seed_priced_row(&harness, plan, "us", FLAT_AMOUNT_MINOR).await;
    harness.publish_price(plan, flat.price_id).await;
    for price_id in [per_unit.price_id, flat.price_id] {
        common::schedule_coverage_window(
            &harness.db.conn().expect("conn"),
            &harness.scope(),
            harness.tenant,
            price_id,
            rest_support::seed_stamp(),
        )
        .await;
    }

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_fixed_run(run_id)),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id = accepted_committing_run(response).await;
    // **The apply is the lane's, and this is where it runs.** The count is the
    // discrimination the assertion below cannot make on its own: a run standing at
    // `committing` is what a handler that enqueued nothing and a drain that applied
    // nothing both leave behind.
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the POST enqueued exactly one apply"
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::CompletedWithConflicts,
        "every row of the one plan failed, so the run is a conflict rather than a success: \
         {stored:?}"
    );

    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let run = body_json(read).await;
    let journal = run["journal"].as_array().expect("a journal");
    assert_eq!(journal.len(), 2, "both published rows were frozen: {run}");
    let entry = |price_id: Uuid| -> &serde_json::Value {
        journal
            .iter()
            .find(|row| row["price_id"] == serde_json::json!(price_id))
            .unwrap_or_else(|| panic!("the journal carries {price_id}: {run}"))
    };
    let per_unit_entry = entry(per_unit.price_id);
    let flat_entry = entry(flat.price_id);
    assert_eq!(
        (&per_unit_entry["state"], &flat_entry["state"]),
        (&serde_json::json!("failed"), &serde_json::json!("failed")),
        "D-134: the plan is the transaction unit, so the `flat` row the run could reprice fails \
         with the `per_unit` row it cannot -- never a partial plan: {run}"
    );

    let flat_reason = flat_entry["failure_reason"]
        .as_str()
        .expect("a `failed` row carries its reason");
    assert_eq!(
        Some(flat_reason),
        per_unit_entry["failure_reason"].as_str(),
        "`inst-mr-apply`'s words are `the shared reason`, one rendering of the rolled-back \
         transaction for every row of the plan: {run}"
    );
    assert!(
        flat_reason.contains(&per_unit.price_id.to_string()),
        "the reason names the row that actually refused, which is the one an operator has to \
         change or exclude -- and it is not this entry's own row: {flat_reason}"
    );
    assert!(
        !flat_reason.contains(&flat.price_id.to_string()),
        "and it does not name this row as a cause: a `flat` row's money is an amount, so a \
         reason claiming it holds a rate is false of it: {flat_reason}"
    );
    assert!(
        flat_reason.contains("D-134"),
        "and it states the failure unit, so an entry on a row the run could have repriced says \
         why it did not: {flat_reason}"
    );

    let rows = price_rows(&harness, plan).await;
    assert!(
        rows.iter().all(|row| row.supersedes_price_id.is_none()),
        "nothing was superseded: a partial plan is the state D-134's aggregate pass never \
         evaluated, and the `flat` row's successor is the one that would have made it partial: \
         {rows:?}"
    );
    for price_id in [per_unit.price_id, flat.price_id] {
        let predecessor = rows
            .iter()
            .find(|row| row.price_id == price_id)
            .expect("the seeded row is still there");
        assert_eq!(
            predecessor.lifecycle_state,
            LifecycleState::Published,
            "and both rows are still the published occupants of their keys: {predecessor:?}"
        );
    }
    assert_eq!(
        rows.iter()
            .find(|row| row.price_id == flat.price_id)
            .and_then(|row| row.row.amount_minor)
            .map(bss_pricing::domain::money::MinorAmount::get),
        Some(FLAT_AMOUNT_MINOR),
        "at the amount it was published with -- the control above proves this same row and this \
         same adjustment produce {FIXED_LINE_MINOR} when no `per_unit` row shares its plan"
    );
}

// ---------------------------------------------------------------------------
// §4 transition 6 (`inst-bs-reject`, D-267) — the refused batch approval.
//
// The state has been in the store since `pricing_bulk_operation` and `BulkState`
// since D-290, and **nothing in production wrote it**: D-267 recorded that
// finding itself ("Nothing drives the new state") and left the writer to a
// later group. `approve_approval` grew its bulk-operation arm on 2026-08-12
// (`apply_approved_repricing_run`); the refusing half had no counterpart, so a
// run whose batch approval was refused sat in `awaiting_approval` forever with
// its `run_id` spent against `uq_pricing_bulk_operation_client_key` — which is
// the exact strand D-267 exists to close, reopened one door along.
// ---------------------------------------------------------------------------

/// The independent principal who decides a run's batch unit.
///
/// Its own constant rather than `POLICY_REVIEWER`: `chk_pricing_approval_distinct_principals`
/// is real, the run is submitted by the harness's own caller, and a suite that
/// reused the threshold fixture's reviewer would be asserting against whichever
/// identity that fixture happens to carry.
const RUN_REVIEWER: Uuid = Uuid::from_u128(0x5eed_12be);

/// Open a material run over one published row and return `(run_id, operation_id,
/// approval_id)`.
///
/// A fresh harness configures no threshold policy, so `inst-mat-failsafe` makes
/// the run material and `advance_on_verdict` parks it in `awaiting_approval`
/// under an open unit — which is the only state transition 6 leaves from.
async fn a_run_awaiting_its_batch_approval(harness: &Harness) -> (Uuid, Uuid, Uuid) {
    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation_id: Uuid = body_json(response).await["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .parse()
        .expect("a uuid");

    let unit = approval_rows(harness)
        .await
        .into_iter()
        .find(|row| {
            row.subject_kind == "bulk_operation" && row.subject_ref == plan_free_ref(operation_id)
        })
        .expect("the material run opened its batch unit");
    assert_eq!(unit.state, "submitted");
    (run_id, operation_id, unit.approval_id)
}

/// How `audit_repo::bulk_operation_ref` renders a run — the bare operation id.
fn plan_free_ref(operation_id: Uuid) -> String {
    operation_id.to_string()
}

/// §4 transition 6: **FROM** `awaiting_approval` **TO** `rejected` **WHEN** the
/// batch approval is refused (D-267). Terminal, and carrying the instant it
/// ended, because `chk_pricing_bulk_operation_completed_at` puts the new state
/// in the terminal set beside the other three.
#[tokio::test]
async fn a_refused_batch_approval_ends_the_run_in_rejected() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;
    let (_, operation_id, approval_id) = a_run_awaiting_its_batch_approval(&harness).await;

    let refused = harness
        .allowed_as(RUN_REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/reject"),
            Some(serde_json::json!({ "reason": "the segment is repriced next quarter" })),
            &[],
        ))
        .await;
    assert_eq!(
        refused.status(),
        StatusCode::OK,
        "an independent principal refuses the batch unit"
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::Rejected,
        "inst-bs-reject: a refused batch approval takes the run out of `awaiting_approval`, \
         which is the one edge D-267 added and the only exit that state has besides approval: \
         {stored:?}"
    );
    assert!(
        stored.completed_at.is_some(),
        "a rejected run is over, so it carries the instant it ended (D-267): {stored:?}"
    );
}

/// The positive control the case above needs, and it is not a formality: a hook
/// that moved **every** decided bulk unit's run to `rejected` would pass that
/// test exactly as a correct one does. An *approved* unit takes the run the
/// other way, and this asserts the run is not `rejected` after one.
///
/// **The approve enqueues the apply rather than running it**, so the lane is drained
/// before the state is read. The drain's own count is what discriminates here rather
/// than the state: the approve takes the run to `committing` on the request, which is
/// not `rejected` either, so a state assertion alone would pass against an approve arm
/// that enqueued nothing at all.
#[tokio::test]
async fn an_approved_batch_approval_never_leaves_the_run_rejected() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;
    let (_, operation_id, approval_id) = a_run_awaiting_its_batch_approval(&harness).await;

    let approved = harness
        .allowed_as(RUN_REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(approved.status(), StatusCode::OK);
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the approve enqueued exactly one apply"
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_ne!(
        stored.state,
        BulkState::AwaitingApproval,
        "the apply the lane ran spends `inst-bs-commit`'s edge off that state: {stored:?}"
    );
    assert_ne!(
        stored.state,
        BulkState::Rejected,
        "and it spends that edge, never transition 6: {stored:?}"
    );
}

/// **The state an approved run is *queued* in is the state an operator has to spend.**
///
/// `apply_approved_repricing_run` hands the apply to the lane and the apply runs
/// arbitrarily later, so every way it can fail to arrive — a full lane, a replica
/// whose applier is gone, a shutdown between the two — leaves the run exactly where
/// the approve put it. `abandon_committing_run` acts on `committing` and refuses
/// everything else, so an approve that queued the run at `awaiting_approval` would
/// hand back a run with no door at all: the abort answers `LIFECYCLE_FORBIDDEN`,
/// nothing sweeps it and no redrive is built, and its journal rows stay `pending` for
/// good. `infra::repricing::begin_committing_in` spends `inst-bs-commit`'s edge on the
/// approve request, before the enqueue, and that is what this pins — over a run the
/// real `POST …/approve` produced, and with **no drain**, because the still-queued
/// apply is the whole fixture. A full lane needs no failure to reproduce it.
///
/// The sibling `a_run_the_lane_has_not_applied_is_ended_by_the_abort_route` proves the
/// same door over the non-material path, whose `committing` edge `open_run` took in
/// the transaction that froze the journal. The two arms reach the lane through
/// different writers, so neither case stands in for the other.
#[tokio::test]
async fn an_approved_run_the_lane_has_not_applied_is_ended_by_the_abort_route() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;
    let (run_id, operation_id, approval_id) = a_run_awaiting_its_batch_approval(&harness).await;

    let approved = harness
        .allowed_as(RUN_REVIEWER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(approved.status(), StatusCode::OK);

    let queued = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        queued.state,
        BulkState::Committing,
        "the approve spends `inst-bs-commit`'s edge before it enqueues, so the run the lane holds \
         is one the abort route reaches: {queued:?}"
    );

    let aborted = harness
        .allowed()
        .send(with_headers("POST", &abort_path(run_id), None, &[]))
        .await;
    assert_eq!(
        aborted.status(),
        StatusCode::OK,
        "an approved run whose apply never arrived is ended by an operator: {}",
        body_json(aborted).await
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert!(
        stored.state.is_terminal(),
        "and it lands terminal rather than standing queued for good: {stored:?}"
    );
    assert!(
        stored
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_some(),
        "on the operator's own decision, which the note records: {:?}",
        stored.report
    );
}

/// **The PDP questions both batch decisions ask, in order** — the pair
/// `rest_authz.rs` names as owed and cannot reach.
///
/// `POST /approvals/{approvalId}/approve` and `.../reject` each ask a *second*
/// question when the unit they decided is a repricing run's batch unit:
/// `apply_approved_repricing_run` (`api/rest/approvals.rs`) and
/// `advance_run_to_rejected` both compile a **fresh** `plan × write` scope, never
/// reusing the `approval × approve` scope the decision already holds — an approver
/// who cannot write the plan must not be handed the authority to apply what they
/// approved by having approved it.
///
/// `rest_authz.rs`'s census cannot bind either: a question behind a branch its seed
/// does not take is never recorded, and its `seed` opens a **plan-content** unit, so
/// a `FURTHER_QUESTIONS` row for this pair would fail its sequence equality on every
/// run. Its own doc named the approve half as owed to this suite and did not name the
/// reject half at all. So the sequence is pinned here, where the batch unit exists —
/// and it is pinned whole and in order, `require_constraints` included, exactly as
/// the census does it: a question added, removed, reordered or re-labelled fails.
///
/// **The discrimination is the census's own row.** `FURTHER_QUESTIONS` has no entry
/// for either decision route, so `every_route_asks_the_catalogued_pair` pins that a
/// decision on a **plan-content** unit asks exactly *one* question. This case pins
/// two on a batch unit, so the second question is provably the bulk arm's rather
/// than something every decision asks.
///
/// **`recording()` is an independent principal**: `Harness::client` mints a fresh
/// subject id per call, so the recorder is never the run's submitter and
/// `chk_pricing_approval_distinct_principals` is satisfied without a named constant.
#[tokio::test]
async fn both_batch_decisions_ask_approval_approve_then_a_fresh_plan_write() {
    for (action, body) in [
        ("approve", None),
        (
            "reject",
            Some(serde_json::json!({ "reason": "the segment is repriced next quarter" })),
        ),
    ] {
        let harness = Harness::new().await;
        let plan = Uuid::now_v7();
        seed_current_plan(&harness, plan).await;
        a_published_row(&harness, plan, "eu").await;
        let (_, _, approval_id) = a_run_awaiting_its_batch_approval(&harness).await;

        let (client, seen) = harness.recording();
        let response = client
            .send(with_headers(
                "POST",
                &format!("/bss-pricing/v1/approvals/{approval_id}/{action}"),
                body,
                &[],
            ))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the decision has to land, or the second question is behind a branch \
             nothing took: {}",
            body_json(response).await
        );

        let asked = seen.lock().expect("recorder");
        let questions: Vec<(&str, &str, bool)> = asked
            .iter()
            .map(|request| {
                (
                    request.resource.resource_type.as_str(),
                    request.action.name.as_str(),
                    request.context.require_constraints,
                )
            })
            .collect();
        assert_eq!(
            questions,
            vec![
                (labels::APPROVAL, actions::APPROVE, true),
                (labels::PLAN, actions::WRITE, true),
            ],
            "`{action}` on a batch unit asks the decision's own gate and then a fresh \
             plan x write"
        );
        // The gate is anchored to the unit it decides; the apply's question is
        // anchored to nothing, because the rows it will write span whatever the
        // run selected. Asserted because `resource_id` is an input to the
        // decision, so a role definition constraining either pair by id is
        // evaluated against whatever this handler passed.
        assert_eq!(
            asked[0].resource.id.map(|id| id.to_string()),
            Some(approval_id.to_string()),
            "the decision's gate names the unit"
        );
        assert_eq!(
            asked[1].resource.id, None,
            "and the apply's question is not anchored to the approval id"
        );
    }
}

// ---------------------------------------------------------------------------
// §3 `inst-mp-pending` (D-35) — a selector row on a key another unit holds.
//
// D-35 decided **both** halves on 2026-07-10: a batch row whose key already
// holds a pending interactive unit fails **per row** naming that unit, and a
// submitted batch pins every key it contains. The bulk import has carried the
// first half since D-286 (`infra::import::key_holds_a_pending_unit`); the run
// had only the second, so a selector that reached one held key made
// `approval_repo::open` collide on `uq_pricing_approval_key_pending` and the
// **whole** `POST` failed — every other row of the run refused with it, and no
// run opened at all. "Fails per-row" had no referent on this surface.
// ---------------------------------------------------------------------------

/// Open a pending interactive unit holding exactly `key`, through the approval
/// store's own writer — the same door every supersession and cutover submit
/// goes through, so the register row is real rather than fabricated.
async fn a_pending_unit_holding(harness: &Harness, key: &str) -> Uuid {
    let conn = harness.db.conn().expect("conn");
    let approval_id = Uuid::now_v7();
    bss_pricing::infra::storage::repo::approval_repo::open(
        &conn,
        &harness.scope(),
        bss_pricing::infra::storage::repo::approval_repo::NewApproval {
            approval_id,
            tenant_id: harness.tenant,
            subject_ref: bss_pricing::infra::storage::repo::audit_repo::plan_revision_ref(
                bss_pricing::domain::scope_key::PlanId::new(Uuid::now_v7()),
                0,
            ),
            subject_kind: bss_pricing::domain::audit::AuditSubjectKind::PlanRevision,
            content_hash: vec![0u8; 32],
            materiality: serde_json::json!({ "material": true, "reason": "an interactive unit" }),
            held_keys: std::iter::once(key.to_owned()).collect(),
        },
        rest_support::seed_stamp(),
    )
    .await
    .expect("seed a pending interactive unit holding the key");
    approval_id
}

/// `inst-mp-pending`: the held row fails **per row**, naming the unit, and its
/// sibling on a free key is frozen `pending` like any other selected row.
///
/// The sibling is the positive control and it is the half that makes this a test
/// of *per-row* refusal rather than of refusal: a handler that failed the whole
/// run — which is what this surface did before — leaves no `pending` row at all,
/// and a handler that failed nothing leaves no `failed` one.
#[tokio::test]
async fn a_selector_row_on_a_held_key_fails_per_row_and_its_sibling_does_not() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let held = seed_price(&harness, plan, "eu").await;
    harness.publish_price(plan, held.price_id).await;
    let free = seed_price(&harness, plan, "us").await;
    harness.publish_price(plan, free.price_id).await;
    let unit = a_pending_unit_holding(&harness, &held.scope_key.to_string()).await;

    let run_id = Uuid::now_v7();
    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "the run opens: a held key refuses its own row, never the batch"
    );
    let operation_id: Uuid = body_json(response).await["operation_id"]
        .as_str()
        .expect("the view carries the minted id")
        .parse()
        .expect("a uuid");

    let read = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(read.status(), StatusCode::OK);
    let run = body_json(read).await;
    let journal = run["journal"].as_array().expect("a journal");
    assert_eq!(
        journal.len(),
        2,
        "both rows are frozen into the journal: {run}"
    );

    let held_row = journal
        .iter()
        .find(|row| row["price_id"] == serde_json::json!(held.price_id.to_string()))
        .expect("the held row is in the journal");
    assert_eq!(
        held_row["state"],
        serde_json::json!("failed"),
        "inst-mp-pending: a selector row whose key holds a pending interactive unit fails \
         per-row: {run}"
    );
    let reason = held_row["failure_reason"]
        .as_str()
        .unwrap_or_else(|| panic!("a failed row carries its reason: {run}"));
    assert!(
        reason.contains(&unit.to_string()),
        "the refusal **names the unit** (D-35), which is the whole of what an operator can act \
         on: {reason}"
    );

    let free_row = journal
        .iter()
        .find(|row| row["price_id"] == serde_json::json!(free.price_id.to_string()))
        .expect("the free row is in the journal");
    assert_eq!(
        free_row["state"],
        serde_json::json!("pending"),
        "its sibling on an unheld key is frozen like any other selected row -- never dropped, \
         and never failed with it: {run}"
    );

    // The other half of D-35, and it is what makes the first half *possible*: the
    // run's own batch unit pins the keys it will act on and **not** the one
    // another unit already holds. Pinning it is what made the whole `POST`
    // collide.
    let conn = harness.db.conn().expect("conn");
    let units: Vec<_> = approval_rows(&harness)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == "bulk_operation")
        .collect();
    assert_eq!(
        units.len(),
        1,
        "the material run opened one batch unit: {units:?}"
    );
    let pinned = bss_pricing::infra::storage::repo::approval_repo::held_keys_of(
        &conn,
        &harness.scope(),
        harness.tenant,
        units[0].approval_id,
    )
    .await
    .expect("read the keys the batch unit holds");
    assert_eq!(
        pinned,
        vec![free.scope_key.to_string()],
        "the batch pins the free key and not the held one (`inst-bk-approval-subset`): a run \
         cannot pin a key the one-pending-unit rule has already given to somebody else, and \
         trying to is what refused the whole batch. Operation {operation_id}"
    );
}

/// D-67's range belongs to the run door too.
///
/// `15000` bp is the shape the "150% of list" data-entry inversion takes —
/// `check_magnitude_range` names it in those words — and `PUT /overlays` refuses
/// it. This door did not, and nothing behind it does either: the run's adjustment
/// never becomes an overlay line, so no rule and no CHECK sees it. Left unrefused
/// it floors every selected row to zero, and an absolute materiality bar above the
/// row's own price does not see that move, so the run publishes with no approval.
#[tokio::test]
async fn a_discount_above_one_hundred_percent_is_refused_at_the_run_door() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let run_id = Uuid::now_v7();
    let body = serde_json::json!({
        "run_id": run_id,
        "selector": { "region": "eu" },
        "adjustment": {
            "adjustment_kind": "discount",
            "magnitude_kind": "percent_bp",
            "adjustment_value": 15_000,
        },
        "changeover": CHANGEOVER,
    });

    let response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(body), &[]))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE",
        "the run door answers the same code the overlay door does"
    );
}

/// The kind declares the direction and the magnitude's sign must not invert it.
///
/// A `markup` of `-100` bp cut every selected row by 1% while the journal recorded
/// a markup — the same missing range check, on the half of it that is about sign
/// rather than ceiling.
#[tokio::test]
async fn a_negative_magnitude_is_refused_whatever_the_kind_declares() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let run_id = Uuid::now_v7();
    let body = serde_json::json!({
        "run_id": run_id,
        "selector": { "region": "eu" },
        "adjustment": {
            "adjustment_kind": "markup",
            "magnitude_kind": "percent_bp",
            "adjustment_value": -100,
        },
        "changeover": CHANGEOVER,
    });

    let response = harness
        .allowed()
        .send(with_headers("POST", REPRICING_RUNS, Some(body), &[]))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(response).await,
        "ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE",
        "a magnitude must be strictly positive; the kind carries the direction"
    );
}

// ---------------------------------------------------------------------------
// D-37's abort: the owner `committing` did not have.
// ---------------------------------------------------------------------------

/// Hold a run in `committing` with one `pending` journal row and its bulk lock
/// taken, addressable by the caller's own `run_id`.
///
/// **What the abort's sweep acts on is state, not a request shape**: a bulk lock and
/// a `pending` journal row under a `committing` run. Writing the three here is the
/// direct way to stand up exactly one of each with nothing else in the store to read
/// back, which is what lets the cases below assert *the* lock and *the* row.
///
/// It is **not** the only way to reach `committing`, and a case whose subject is the
/// state production actually reaches wants [`a_run_the_lane_is_holding`] instead: the
/// apply is the lane's, so a run stands here from the `POST` that accepted it until
/// the lane drains — which is precisely the state an operator finds after a `202`
/// whose effect they never saw.
///
/// The `client_key` is the `run_id`, because that is this plane's idempotency column
/// and the path segment every route here addresses a run by.
async fn a_run_stalled_committing(harness: &Harness, run_id: Uuid, price_id: Uuid) -> Uuid {
    use bss_pricing::domain::bulk::BulkKind;
    use bss_pricing::infra::storage::repo::repricing_journal_repo::NewJournalRow;
    use bss_pricing::infra::storage::repo::{NewBulkOperation, bulk_repo, repricing_journal_repo};

    let conn = harness.db.conn().expect("conn");
    let scope = harness.scope();
    let operation_id = Uuid::now_v7();
    bulk_repo::open(
        &conn,
        &scope,
        NewBulkOperation {
            operation_id,
            tenant_id: harness.tenant,
            kind: BulkKind::Repricing,
            client_key: run_id.to_string(),
            request_hash: b"digest".to_vec(),
            report: serde_json::json!({ "selected": 1 }),
            submitted_by: Uuid::from_u128(0x_ac_13),
            submitted_at: OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("open the run");
    repricing_journal_repo::open_rows(
        &conn,
        &scope,
        &[NewJournalRow {
            run_id: operation_id,
            price_id,
            tenant_id: harness.tenant,
        }],
    )
    .await
    .expect("freeze the journal");
    bulk_repo::advance(
        &conn,
        &scope,
        harness.tenant,
        operation_id,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({ "selected": 1 }),
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("hold the run in committing");
    bulk_repo::take_locks(
        &conn,
        &scope,
        harness.tenant,
        operation_id,
        &[price_id],
        OffsetDateTime::now_utc(),
    )
    .await
    .expect("the apply's own lock");
    operation_id
}

/// A run the **lane** is holding at `committing`: staged through the real `POST`,
/// its apply enqueued on `RunApplyLane` and deliberately not drained.
///
/// [`a_run_stalled_committing`]'s counterpart over production's own staging, and the
/// one every claim about the abort's *subject* should be read off. The caller's
/// `run_id` addresses it; the answer is the minted `operation_id`, the plan and the
/// price row the run selected — the last two because a successor assertion reads the
/// plan's rows and names the predecessor.
///
/// **The bar is the caller's to install.** `approve_threshold_policy` opens an
/// approval unit of its own, so this stages the run and nothing else, and a case that
/// wants an auto-publishable run installs one bar before calling.
async fn a_run_the_lane_is_holding(harness: &Harness, run_id: Uuid) -> (Uuid, Uuid, Uuid) {
    let plan = Uuid::now_v7();
    seed_current_plan_with_phase(harness, plan).await;
    let priced = seed_priced_row(harness, plan, "eu", 9_900).await;
    harness.publish_price(plan, priced.price_id).await;
    common::schedule_coverage_window(
        &harness.db.conn().expect("conn"),
        &harness.scope(),
        harness.tenant,
        priced.price_id,
        rest_support::seed_stamp(),
    )
    .await;

    let response = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    (
        accepted_committing_run(response).await,
        plan,
        priced.price_id,
    )
}

fn abort_path(run_id: Uuid) -> String {
    format!("{}/abort", run_path(run_id))
}

/// **A stalled run has a door out, and going through it releases the lock.**
///
/// `apply_run_in`'s ordinary-`Err` exit and `RunLockGuard`'s `Drop` both leave a run
/// `committing` on purpose — that is what keeps a redrive possible and what stops a
/// storage hiccup freezing every unreached plan `failed` — and this route is what
/// spends what they preserve. Every clause of the sweep is read back off the store
/// rather than off the response, this suite's standing rule.
#[tokio::test]
async fn aborting_a_stalled_run_releases_its_locks_and_decides_its_rows() {
    use bss_pricing::infra::storage::repo::bulk_repo;

    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let row = a_published_row(&harness, plan, "eu").await;
    let run_id = Uuid::now_v7();
    let operation_id = a_run_stalled_committing(&harness, run_id, row).await;

    let aborted = harness
        .allowed()
        .send(with_headers("POST", &abort_path(run_id), None, &[]))
        .await;
    assert_eq!(aborted.status(), StatusCode::OK);
    let body = body_json(aborted).await;
    assert_eq!(
        body["operation_id"],
        serde_json::json!(operation_id.to_string()),
        "the route resolves the run by the caller's `run_id`, not by the minted id: {body}"
    );

    // The lock, which is the clause that unfreezes interactive authoring.
    let conn = harness.db.conn().expect("conn");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &harness.scope(), harness.tenant, row)
            .await
            .expect("read the lock"),
        None,
        "the bulk lock over the run's own row is gone"
    );

    // The row the apply never reached, decided rather than left `pending` under a
    // terminal run — where nothing could ever reach it.
    let stored = bulk_operation_row(&harness, operation_id).await;
    assert!(
        stored.state.is_terminal(),
        "the run itself lands terminal: {stored:?}"
    );
    assert!(
        stored
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_some(),
        "and the note is stamped, which is the only thing telling this operation's replay from \
         an ordinary finish: {:?}",
        stored.report
    );
    assert_eq!(
        body["journal"][0]["state"],
        serde_json::json!("failed"),
        "the row the apply never reached is decided: {body}"
    );
    assert!(
        body["journal"][0]["failure_reason"]
            .as_str()
            .expect("a reason")
            .contains("aborted"),
        "with a reason naming the act, so an operator can tell it from a rule refusal: {body}"
    );

    // D-158's record: `GET /audit` must answer "who ended run X" beside "who
    // opened it", and nothing else on this plane writes it.
    let abandons = rest_support::audit_rows(&harness)
        .await
        .into_iter()
        .filter(|row| {
            row.action == "abandon" && row.subject_ref.contains(&operation_id.to_string())
        })
        .count();
    assert_eq!(abandons, 1, "one abandon record, on the run's own chain");
}

/// **The claim that makes the `POST`'s `202` safe, armed against the state it
/// actually hands back.**
///
/// The response leaves the run `committing` with its apply still on the lane, so
/// every way that apply can fail to arrive — a full lane, a replica whose applier
/// died, a shutdown between the two — leaves exactly this state. The abort route is
/// what spends it, and the sibling case above proves that over a run *staged* at
/// `committing` by hand; this one proves it over one the real `POST` produced, which
/// is the only staging the contract can be read off.
///
/// **Then the apply arrives anyway, and is a no-op**, which is the race the lane
/// created and nothing else in this suite reaches: `apply_run_in`'s terminal arm
/// answers the journal's standing tally and writes nothing, so an operator who
/// aborted does not have the run repriced under them a moment later. Asserted on the
/// absence of a successor row as well as on the state, because a run that ended and a
/// run that ended *and then published* are the same state.
#[tokio::test]
async fn a_run_the_lane_has_not_applied_is_ended_by_the_abort_route() {
    let harness = Harness::new().await;
    // Any bar at all is above nothing, and `9_900 -> +5%` is far under this one: the
    // run is auto-publishable, so its apply is the lane's rather than an approver's.
    approve_threshold_policy(&harness, &[("USD", 1_000_000)]).await;
    let run_id = Uuid::now_v7();
    let (operation_id, plan, priced_price_id) = a_run_the_lane_is_holding(&harness, run_id).await;

    // **No drain here, and that is the fixture.** The lane holds this run's apply,
    // which is precisely the state an operator finds after a `202` they never saw the
    // effect of.
    let aborted = harness
        .allowed()
        .send(with_headers("POST", &abort_path(run_id), None, &[]))
        .await;
    assert_eq!(
        aborted.status(),
        StatusCode::OK,
        "the state the POST hands back is the state this route takes: {}",
        body_json(aborted).await
    );

    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::CompletedWithConflicts,
        "the one row the apply never reached is failed, so the run ends a conflict: {stored:?}"
    );
    assert!(
        stored
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_some(),
        "and the note says an operator ended it: {:?}",
        stored.report
    );

    // And now the apply the `POST` enqueued arrives.
    assert_eq!(
        harness.drain_repricing_applies().await,
        1,
        "the POST had enqueued one apply, which is what this whole case is about"
    );
    let after = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        after.state,
        BulkState::CompletedWithConflicts,
        "a queued apply over a run an operator already ended writes nothing: {after:?}"
    );
    assert_eq!(
        after.completed_at, stored.completed_at,
        "the instant the abort stamped stands, rather than being rewritten by the apply that \
         arrived after it: {after:?}"
    );
    let rows = price_rows(&harness, plan).await;
    assert!(
        rows.iter()
            .all(|row| row.supersedes_price_id != Some(priced_price_id)),
        "and no successor was published under the operator who aborted: {rows:?}"
    );
}

/// **A replayed abort is answered the run it already ended, and a run that ended on
/// its own is refused.**
///
/// The two halves are one case because each is the other's control. This plane
/// declares no `Idempotency-Key` — its idempotency column is the `run_id` in the
/// path — so what makes the retry safe is the state guard plus the note the sweep
/// writes, and nothing else: a client whose abort succeeded and whose response was
/// lost must not be told `LIFECYCLE_FORBIDDEN` over an act that worked. The guard
/// stays whole all the same, because a terminal run *nobody aborted* has a report
/// whose every row was attempted and a `completed_at` an abort would rewrite.
#[tokio::test]
async fn a_replayed_abort_is_answered_and_a_run_that_ended_on_its_own_is_refused() {
    let harness = Harness::new().await;
    // **Over a run the lane is actually holding**, not one staged at `committing`
    // through the repository: a replay is what a client whose first response was lost
    // sends, and the run it names is one the `POST` produced and the lane has not yet
    // applied. Staging the state by hand would prove the guard over a shape no client
    // can be holding a lost receipt for.
    approve_threshold_policy(&harness, &[("USD", 1_000_000)]).await;
    let run_id = Uuid::now_v7();
    let (operation_id, _, _) = a_run_the_lane_is_holding(&harness, run_id).await;

    let first = harness
        .allowed()
        .send(with_headers("POST", &abort_path(run_id), None, &[]))
        .await;
    assert_eq!(first.status(), StatusCode::OK);

    let replay = harness
        .allowed()
        .send(with_headers("POST", &abort_path(run_id), None, &[]))
        .await;
    assert_eq!(
        replay.status(),
        StatusCode::OK,
        "a replayed abort is answered the run it already ended"
    );
    let body = body_json(replay).await;
    assert_eq!(
        body["operation_id"],
        serde_json::json!(operation_id.to_string()),
        "and it is the same run, not a second sweep: {body}"
    );

    // The control. A run that reached a terminal state on its own carries no note,
    // so the guard answers about it — and it answers with a code rather than a bare
    // 400, because an operator has to be able to branch on it.
    let finished = Uuid::now_v7();
    let other_plan = Uuid::now_v7();
    seed_current_plan(&harness, other_plan).await;
    let other_row = a_published_row(&harness, other_plan, "us").await;
    // Staged by hand, and here that is the point rather than a convenience: what this
    // half needs is a run that reached a terminal state **without** an abort, which is
    // the one shape the abort route itself cannot produce.
    let finished_op = a_run_stalled_committing(&harness, finished, other_row).await;
    {
        use bss_pricing::infra::storage::repo::bulk_repo;
        let conn = harness.db.conn().expect("conn");
        bulk_repo::advance(
            &conn,
            &harness.scope(),
            harness.tenant,
            finished_op,
            BulkState::Committing,
            BulkState::Completed,
            serde_json::json!({ "selected": 1 }),
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("the run finishes on its own");
    }

    let refused = harness
        .allowed()
        .send(with_headers("POST", &abort_path(finished), None, &[]))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_code(refused).await,
        "LIFECYCLE_FORBIDDEN",
        "the refusal carries its code, not a bare 400"
    );
}

/// An abort naming a run this tenant never opened is a 404, on the read that
/// resolves it — the same answer the progress endpoint gives, so a probe cannot
/// learn from the abort that a run exists.
/// **A caller of another tenant cannot abort this tenant's run**, and the refusal is
/// the one an unknown run id gets.
///
/// The sibling read case's reason, on the verb that writes. `rest_authz`'s census
/// cannot reach this route — `drive` fills `{runId}` with a fixed literal
/// `absent_ids` does not vary, so its `foreign` and `absent` requests are the same
/// bytes — and its by-id write loop is where a route that resolved its object
/// before narrowing on the tenant would otherwise be caught.
///
/// What the write costs is more than the read's: the sweep releases the run's bulk
/// locks and decides its rows, so a foreign caller reaching it unfreezes another
/// tenant's price rows for interactive authoring and force-fails every row that
/// tenant's apply had not reached.
#[tokio::test]
async fn a_foreign_tenants_run_cannot_be_aborted_and_the_owners_identical_call_can() {
    use bss_pricing::infra::storage::repo::bulk_repo;

    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    let row = a_published_row(&harness, plan, "eu").await;
    let run_id = Uuid::now_v7();
    let operation_id = a_run_stalled_committing(&harness, run_id, row).await;

    let foreign = harness
        .other_tenant()
        .send(with_headers("POST", &abort_path(run_id), None, &[]))
        .await;
    let absent = harness
        .other_tenant()
        .send(with_headers("POST", &abort_path(Uuid::now_v7()), None, &[]))
        .await;

    let foreign_status = foreign.status();
    let absent_status = absent.status();
    assert!(
        !foreign_status.is_success(),
        "another tenant aborted this tenant's run"
    );
    assert_eq!(
        foreign_status, absent_status,
        "another tenant's run answers {foreign_status} and an unknown id {absent_status}; the \
         difference is a probe for which run ids are real"
    );
    assert_eq!(
        body_json(foreign).await.get("type"),
        body_json(absent).await.get("type"),
        "and alike in the problem `type`, or the body distinguishes what the status does not"
    );

    // Read off the store and not off the response: a handler that swept and then
    // failed to render would answer an error over a run it had already ended.
    let stored = bulk_operation_row(&harness, operation_id).await;
    assert_eq!(
        stored.state,
        BulkState::Committing,
        "the refused abort left the run where it was: {stored:?}"
    );
    assert!(
        stored
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_none(),
        "and stamped no abort note: {:?}",
        stored.report
    );
    let conn = harness.db.conn().expect("conn");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &harness.scope(), harness.tenant, row)
            .await
            .expect("read the lock"),
        Some(operation_id),
        "and the run's bulk lock still stands, which is what a foreign sweep would release"
    );

    // The positive control, and the whole route is under it: without a call that
    // succeeds, a route refusing every caller for an unrelated reason reads as
    // tenant isolation. `rest_support` grants `token_scopes(["*"])`, so the scopes
    // are not what separates these two callers — the tenant is.
    let owner = harness
        .allowed()
        .send(with_headers("POST", &abort_path(run_id), None, &[]))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::OK,
        "the run's own tenant aborts it, or the refusals above are about the request"
    );
    assert!(
        bulk_operation_row(&harness, operation_id)
            .await
            .report
            .get(bss_pricing::infra::bulk::ABORTED_MEMBER)
            .is_some(),
        "and the owner's abort is the one that swept"
    );
}

#[tokio::test]
async fn aborting_a_run_that_does_not_exist_is_a_404() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(with_headers("POST", &abort_path(Uuid::now_v7()), None, &[]))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// **A caller of another tenant cannot read this tenant's run**, and the refusal is
/// the one an unknown run id gets.
///
/// The only shape that exercises the SQL tenant predicate on this route.
/// `rest_authz`'s census cannot: `drive` fills `{runId}` with a fixed literal
/// `absent_ids` does not vary, so its `foreign` and `absent` requests are the same
/// bytes and the route is listed in its `BY_ID_READS_THIS_FIXTURE_CANNOT_STAGE`.
///
/// A run's progress body carries the journal — every selected `price_id` and its
/// state — so a read that resolved the run before narrowing hands another tenant's
/// row identities over whole, not merely the fact that a run exists.
#[tokio::test]
async fn a_foreign_tenants_run_reads_like_an_unknown_one() {
    let harness = Harness::new().await;
    let plan = Uuid::now_v7();
    seed_current_plan(&harness, plan).await;
    a_published_row(&harness, plan, "eu").await;

    let run_id = Uuid::now_v7();
    let opened = harness
        .allowed()
        .send(with_headers(
            "POST",
            REPRICING_RUNS,
            Some(a_run(run_id, &serde_json::json!({ "currency": "USD" }))),
            &[],
        ))
        .await;
    assert_eq!(
        opened.status(),
        StatusCode::ACCEPTED,
        "the fixture needs a run"
    );

    let foreign = harness
        .other_tenant()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    let absent = harness
        .other_tenant()
        .send(with_headers("GET", &run_path(Uuid::now_v7()), None, &[]))
        .await;

    let foreign_status = foreign.status();
    let absent_status = absent.status();
    assert!(
        !foreign_status.is_success(),
        "another tenant read this tenant's run and its journal"
    );
    assert_eq!(
        foreign_status, absent_status,
        "another tenant's run answers {foreign_status} and an unknown id {absent_status}; the \
         difference is a probe for which run ids are real"
    );
    assert_eq!(
        body_json(foreign).await.get("type"),
        body_json(absent).await.get("type"),
        "and alike in the problem `type`, or the body distinguishes what the status does not"
    );

    // The control. Without it a route that refused every reader would satisfy the
    // two assertions above.
    let owner = harness
        .allowed()
        .send(with_headers("GET", &run_path(run_id), None, &[]))
        .await;
    assert_eq!(
        owner.status(),
        StatusCode::OK,
        "the run's own tenant reads it, or the refusals above are about the request"
    );
    assert_eq!(
        body_json(owner).await["run_id"],
        serde_json::json!(run_id.to_string())
    );
}
