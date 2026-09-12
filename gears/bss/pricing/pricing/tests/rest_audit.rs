//! `GET /bss-pricing/v1/audit`, driven through the real router — S5 §5's Auditor
//! read (`inst-au-read`; D-12, D-125, D-135, Z13-8).
//!
//! # What this suite is really about
//!
//! `infra/error_mapping.rs` justifies dropping the detail from three 403 arms
//! because the attempt "is already on `pricing_audit_log` as a `deny` record
//! carrying that id — a durable trail rather than a log line". Until this route
//! existed, nothing an operator can reach could read that record: the only reader
//! in the tree was `rest_support::audit_rows`, a **test-side** query over the
//! table, which is exactly why the gap was invisible to a green suite.
//!
//! So the first case does not assert "the route returns rows". It reproduces the
//! refusal the ladder names, through the router, and then reads the compensating
//! record back **through the route** — the id, the actor, the verb and the code —
//! because that pairing is the claim, and a page of rows is not.
//!
//! Every case here reads through the mounted route and asserts on the response
//! body. Where a case needs to know what the store holds, it says so and uses
//! `audit_rows` as the *independent* answer to compare the route against; a suite
//! that only compared the route to itself could not tell a page that dropped
//! records from a store that never had them.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::audit::AUDIT;
use rest_support::{
    Harness, approval_rows, at, audit_rows, body_json, problem_code, request,
    seed_publishable_plan, with_headers,
};
use uuid::Uuid;

const SUBMITTER: Uuid = Uuid::from_u128(0x5_c0);

/// **Every seeded audit row carries the fixture's instant, not the wall clock.**
///
/// `rest_support::stamp` is reached by every seeder in the harness, and it read
/// `OffsetDateTime::now_utc()`: each run wrote a different `recorded_at`, so no suite could assert
/// the recorded instant by equality and a seeder that dropped the stamp entirely
/// was indistinguishable from one that kept it. The instant is a fact of the
/// fixture like `plan_id` is, and this is what makes it one.
///
/// The seed alone, with no route driven: a mutation arriving over HTTP is stamped
/// from the request clock and is *supposed* to be `now`.
#[tokio::test]
async fn every_seeded_audit_row_carries_the_fixtures_instant() {
    let harness = Harness::new().await;
    seed_publishable_plan(&harness, Uuid::now_v7()).await;

    let rows = audit_rows(&harness).await;
    assert!(
        !rows.is_empty(),
        "the seed must write audit rows for this to be an assertion at all"
    );
    for row in &rows {
        assert_eq!(
            row.recorded_at,
            at(10),
            "a seeded row stamped off the wall clock: {row:?}"
        );
    }
}

/// One page, as the route answers it.
async fn page(harness: &Harness, query: &str) -> serde_json::Value {
    let path = if query.is_empty() {
        AUDIT.to_owned()
    } else {
        format!("{AUDIT}?{query}")
    };
    let response = harness.allowed().send(request("GET", &path, None)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the Auditor read must answer 200"
    );
    body_json(response).await
}

/// `(chain_id, seq)` of every entry on a page, which is the pair that names a
/// record.
fn keys(page: &serde_json::Value) -> Vec<(String, i64)> {
    page["entries"]
        .as_array()
        .expect("a page carries an entries array")
        .iter()
        .map(|entry| {
            (
                entry["chain_id"]
                    .as_str()
                    .expect("every record names its segment")
                    .to_owned(),
                entry["seq"].as_i64().expect("and its position in it"),
            )
        })
        .collect()
}

/// A plan with one pending unit over it, opened through the publish route —
/// `rest_approvals.rs`'s helper, and the same reason for using the route: a unit
/// staged by calling the service directly is a unit no operator can produce.
async fn a_pending_unit(harness: &Harness) -> Uuid {
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(harness, plan_id).await;
    let response = harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/publish"),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "the submit must succeed for anything below to be about a decision"
    );
    let rows = approval_rows(harness).await;
    assert_eq!(rows.len(), 1);
    rows[0].approval_id
}

// ---------------------------------------------------------------------------
// The dependant: the record the error ladder points at is reachable.
// ---------------------------------------------------------------------------

/// **The 403 that drops its detail is recoverable from this surface.**
///
/// The refusal is `SELF_APPROVAL_FORBIDDEN`, whose `reason` is the bare code —
/// `permission_denied()` has no detail slot, so the approval id the caller
/// addressed is not on the wire. The ladder's answer is that the attempt is on the
/// trail. This asserts that answer end to end: the refusal happens through the
/// router, and the record comes back through the route naming the approval, the
/// principal who attempted it, the verb and the code it was refused with.
///
/// **Not "a page with one entry".** The trail of a publishable plan already holds
/// the authoring and submit records, so a positional or count-based assertion
/// would be satisfied by any of them, and would keep passing if the `deny` record
/// stopped being written. What is asserted is the record itself, found by the pair
/// that identifies it.
#[tokio::test]
async fn a_refused_self_approval_is_readable_on_the_trail() {
    let harness = Harness::new().await;
    let approval_id = a_pending_unit(&harness).await;

    let refused = harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let code = problem_code(refused).await;
    assert_eq!(code, "SELF_APPROVAL_FORBIDDEN");

    let page = page(&harness, "limit=1000").await;
    let entries = page["entries"].as_array().expect("a page of records");
    let denial = entries
        .iter()
        .find(|entry| entry["action"] == "deny" && entry["approval_ref"] == approval_id.to_string())
        .unwrap_or_else(|| {
            panic!(
                "the refusal the 403 dropped its detail for must be readable here; the page \
                 carries {:?}",
                entries
                    .iter()
                    .map(|e| (e["action"].clone(), e["approval_ref"].clone()))
                    .collect::<Vec<_>>()
            )
        });

    assert_eq!(
        denial["actor_principal_id"],
        SUBMITTER.to_string(),
        "the record names the principal who attempted it, pseudonymously"
    );
    assert_eq!(
        denial["after_state"]["refusedWith"], code,
        "and the code it was refused with, which is what the 403's bare reason left unexplained"
    );
    assert_eq!(
        denial["subject_kind"], "plan_revision",
        "the attempt is filed against the subject it was about"
    );
    assert!(
        denial["correlation_id"].is_string(),
        "D-178: every record carries the request's correlation id, which is what pulls one \
         operator call's records together: {denial}"
    );
}

// ---------------------------------------------------------------------------
// The walk.
// ---------------------------------------------------------------------------

/// **A one-row-at-a-time walk visits every record exactly once, in order.**
///
/// The full page read in one request is the referent, and `audit_rows` — a direct
/// query over the table — is the independent check that the referent is the whole
/// trail rather than whatever the route felt like returning. Three assertions,
/// because three different defects are possible: a walk that skips (fewer keys), a
/// walk that repeats (duplicate keys), and a walk that reorders (same set,
/// different sequence).
///
/// The seed is two publishable plans and a refused decision, so the trail spans
/// **more than one chain segment** (D-135 gives each aggregate its own) — a walk
/// keyed on `seq` alone would pass over a single segment and interleave two.
#[tokio::test]
async fn a_one_row_walk_visits_every_record_exactly_once_and_in_order() {
    let harness = Harness::new().await;
    let approval_id = a_pending_unit(&harness).await;
    harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    let second = Uuid::now_v7();
    seed_publishable_plan(&harness, second).await;

    let stored = audit_rows(&harness).await;
    let whole = page(&harness, "limit=1000").await;
    let expected = keys(&whole);
    assert_eq!(
        expected.len(),
        stored.len(),
        "the single page must carry the whole trail the store holds"
    );
    assert!(
        expected.len() > 3,
        "the seed must write enough records for a walk to be a walk: {expected:?}"
    );
    let segments: std::collections::BTreeSet<&String> =
        expected.iter().map(|(chain, _)| chain).collect();
    assert!(
        segments.len() > 1,
        "and it must span more than one chain segment, or the ordering claim is untested: \
         {expected:?}"
    );

    let mut walked: Vec<(String, i64)> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..=expected.len() {
        let query = match cursor.as_deref() {
            None => "limit=1".to_owned(),
            Some(token) => format!("limit=1&cursor={token}"),
        };
        let page = page(&harness, &query).await;
        walked.extend(keys(&page));
        match page["next_cursor"].as_str() {
            None => break,
            Some(token) => cursor = Some(token.to_owned()),
        }
    }

    assert_eq!(
        walked, expected,
        "the walk must visit the same records in the same order as the single page"
    );
    let distinct: std::collections::BTreeSet<&(String, i64)> = walked.iter().collect();
    assert_eq!(
        distinct.len(),
        walked.len(),
        "and no record twice: {walked:?}"
    );
}

/// **The same walk under a caller's `$orderby`.** `seq` counts within a chain
/// segment, so `recorded_at` plus the `seq` tiebreaker alone is not unique —
/// entries on different chains share both. `query_with_unique_order` names
/// `chain_id`; this is the boundary it repairs, one row a page, with the single
/// page under the same order as the referent.
#[tokio::test]
async fn a_one_row_walk_under_recorded_at_visits_every_record_exactly_once() {
    let harness = Harness::new().await;
    let approval_id = a_pending_unit(&harness).await;
    harness
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    seed_publishable_plan(&harness, Uuid::now_v7()).await;

    let whole = page(&harness, "$orderby=recorded_at&limit=1000").await;
    let expected = keys(&whole);
    let segments: std::collections::BTreeSet<&String> =
        expected.iter().map(|(chain, _)| chain).collect();
    assert!(
        segments.len() > 1,
        "the seed must span more than one chain segment, or nothing collides: {expected:?}"
    );

    let mut walked: Vec<(String, i64)> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..=expected.len() {
        let query = match cursor.as_deref() {
            None => "$orderby=recorded_at&limit=1".to_owned(),
            Some(token) => format!("limit=1&cursor={token}"),
        };
        let page = page(&harness, &query).await;
        walked.extend(keys(&page));
        match page["next_cursor"].as_str() {
            None => break,
            Some(token) => cursor = Some(token.to_owned()),
        }
    }

    assert_eq!(
        walked, expected,
        "the walk must visit the same records in the same order as the single page"
    );
    let distinct: std::collections::BTreeSet<&(String, i64)> = walked.iter().collect();
    assert_eq!(
        distinct.len(),
        walked.len(),
        "and no record twice: {walked:?}"
    );
}

/// **The last page carries no cursor**, which is what lets a client stop without
/// an extra round trip.
///
/// Asserted separately from the walk above because the walk would terminate on its
/// own bound even if the token never went absent, and would then be a test of the
/// loop rather than of the contract.
#[tokio::test]
async fn the_exhausted_walk_hands_back_no_cursor() {
    let harness = Harness::new().await;
    seed_publishable_plan(&harness, Uuid::now_v7()).await;

    let whole = page(&harness, "limit=1000").await;
    assert!(
        !keys(&whole).is_empty(),
        "the seed must write records, or this asserts nothing"
    );
    assert!(
        whole["next_cursor"].is_null(),
        "a page that exhausted the trail must not name a next one: {whole}"
    );
}

// ---------------------------------------------------------------------------
// Isolation, and the two declared 400s.
// ---------------------------------------------------------------------------

/// **One tenant's trail is not another's**, and the page is empty rather than
/// refused.
///
/// The store's `secure()` scope is what enforces it, so this is the assertion that
/// would catch a filter built from the request instead of from the compiled scope.
/// A 200 with no entries is the right answer for the same reason an empty history
/// is: nothing is missing, there is simply nothing this caller may see.
#[tokio::test]
async fn another_tenants_records_are_not_on_this_tenants_page() {
    let harness = Harness::new().await;
    seed_publishable_plan(&harness, Uuid::now_v7()).await;
    assert!(
        !audit_rows(&harness).await.is_empty(),
        "the seed must have written records for the other tenant to be unable to see"
    );

    let response = harness
        .other_tenant()
        .send(request("GET", AUDIT, None))
        .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a caller authorized for another tenant reads their own empty trail"
    );
    let body = body_json(response).await;
    assert_eq!(
        keys(&body),
        Vec::new(),
        "and it holds none of this tenant's records: {body}"
    );
}

/// A page of zero rows never advances, so `limit=0` is refused rather than served.
#[tokio::test]
async fn a_zero_limit_is_refused() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(request("GET", &format!("{AUDIT}?limit=0"), None))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A token this surface did not issue is refused, not silently treated as the
/// beginning of the walk.
///
/// The positive control is every case above that pages successfully: a route that
/// refused every cursor would satisfy this one alone.
#[tokio::test]
async fn a_cursor_this_surface_did_not_issue_is_refused() {
    let harness = Harness::new().await;

    let response = harness
        .allowed()
        .send(request("GET", &format!("{AUDIT}?cursor=not-a-token"), None))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// The gate's positive control has a negative twin here as well as in
/// `rest_authz`'s set-level properties: a caller the PDP refuses gets 403, not an
/// empty page.
///
/// The distinction is the one `rest_history.rs` names — a refusal that arrived as
/// `200 []` would be indistinguishable from a tenant with nothing to show, and the
/// trail is the one store where that difference is the whole point.
#[tokio::test]
async fn a_denied_caller_is_refused_rather_than_shown_an_empty_page() {
    let harness = Harness::new().await;
    seed_publishable_plan(&harness, Uuid::now_v7()).await;

    let response = harness.denied().send(request("GET", AUDIT, None)).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
