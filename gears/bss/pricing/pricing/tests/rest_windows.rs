//! `GET /bss-pricing/v1/plans/{planId}/coverage` over the real router.
//!
//! What this suite owns that `domain/coverage_tests.rs` cannot: that the report an
//! operator reads is **the same answer** the publish refusal gave them, under the
//! same key rendering, off a real store.
//!
//! The gate itself is not asserted here. `rest_authz.rs`'s census carries this
//! route now, so `every_route_asks_the_catalogued_pair`,
//! `a_pdp_outage_fails_closed_on_every_route` and the deny/unconstrained
//! properties all range over it — one set of gate properties for every route,
//! rather than a per-suite copy free to be the weaker one.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::windows::PLAN_COVERAGE;
use bss_pricing::domain::approval::ApprovalState;
use bss_pricing::domain::audit::{AuditStamp, AuditSubjectKind};
use bss_pricing::domain::contracts::{EntitlementGrants, PlanChangeContract};
use bss_pricing::domain::instant::format_rfc3339;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::storage::repo::window_repo::{NewWindow, schedule};
use rest_support::{
    Harness, body_json, request, seed_draft_plan, seed_price, seed_publishable_plan, with_headers,
};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use uuid::Uuid;

/// One value for the whole binary; what it is asserted about is nothing.
const CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

/// The two principals the one publishing case here needs. `inst-tp-distinct` is
/// identity and not role, so a commit cannot be reached with one of them.
const SUBMITTER: Uuid = Uuid::from_u128(0x5_c7);
const APPROVER: Uuid = Uuid::from_u128(0xa_c7);

fn coverage_path(plan_id: Uuid) -> String {
    PLAN_COVERAGE.replace("{planId}", &plan_id.to_string())
}

fn publish_path(plan_id: Uuid) -> String {
    format!("/bss-pricing/v1/plans/{plan_id}/publish")
}

/// A window-scale instant: 2099-10-01 plus `day` days.
///
/// A **fixed** date, deliberately after `common::COVERAGE_TO_UTC` so a window this
/// suite schedules never overlaps the one `seed_publishable_plan` already put on
/// the row, and never computed from `now` — a fixture dated off the clock asserts
/// something different every day it runs. The year tracks that constant's, which is
/// 2099 so that no real clock ever falls inside a fixture interval; see its doc.
/// The gear's own rendering, not a second one written here.
///
/// This used to call `time`'s well-known `Rfc3339` directly, which is a
/// re-implementation of the thing under test: the day the gear's renderer
/// changed, every expectation built through this helper asserted the form the
/// gear had stopped emitting, and the failure read as a wire-format regression
/// rather than as a stale fixture.
fn wire(at: OffsetDateTime) -> String {
    bss_pricing::domain::instant::format_rfc3339(at)
}

fn at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 10, 1, 0, 0, 0) + time::Duration::days(day)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: rest_support::SEED_ACTOR,
        recorded_at: at(0),
        correlation_id: CORRELATION,
    }
}

/// Schedule a window on `price_id` through the store.
///
/// **Through the repository even though the route is now mounted**, and the reason
/// changed with it. It used to be that `POST …/prices/{priceId}/windows` did not
/// exist; it does, and this file drives it in the cases below. What keeps the
/// *coverage-report* fixtures on the repository is independence: the report's cases
/// stage interval sets the route would refuse — interior gaps, most of all, which is
/// the state an operator opens the report to see — and a fixture that had to pass
/// `inst-fg-detect` could not stage the very fault the surface exists to report.
/// `rest_support::seed_window` carries the same note for the same reason.
async fn window(h: &Harness, price_id: Uuid, id: u128, from: i64, to: Option<i64>) {
    let conn = h.state.db.conn().expect("conn");
    schedule(
        &conn,
        &h.scope(),
        NewWindow {
            window_id: Uuid::from_u128(id),
            tenant_id: h.tenant,
            price_id,
            effective_from: at(from),
            effective_to: to.map(at),
            reason_code: "priceIncrease".to_owned(),
        },
        stamp(),
    )
    .await
    .unwrap_or_else(|e| panic!("schedule window {id}: {e}"));
}

async fn coverage(h: &Harness, plan_id: Uuid) -> serde_json::Value {
    let response = h
        .allowed()
        .send(request("GET", &coverage_path(plan_id), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

/// The report's entry for `scope_key`, by its canonical rendering.
fn entry<'a>(report: &'a serde_json::Value, scope_key: &str) -> &'a serde_json::Value {
    report
        .get("keys")
        .and_then(|k| k.as_array())
        .expect("the report carries a key list")
        .iter()
        .find(|e| e.get("scope_key").and_then(|k| k.as_str()) == Some(scope_key))
        .unwrap_or_else(|| panic!("no entry for {scope_key} in {report}"))
}

fn keys_of(report: &serde_json::Value) -> Vec<String> {
    report
        .get("keys")
        .and_then(|k| k.as_array())
        .expect("the report carries a key list")
        .iter()
        .map(|e| {
            e.get("scope_key")
                .and_then(|k| k.as_str())
                .expect("a rendered key")
                .to_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The report an operator remediates from
// ---------------------------------------------------------------------------

/// **The property this route exists for**: the key the publish refusal names is
/// the key the report calls uncovered, spelled identically.
///
/// An operator reads `WINDOW_COVERAGE_MISSING` on a key, opens this report, and has
/// to find that key without transcribing ten axes. Two renderings of one key
/// would make the remediation surface unusable while every test of each half
/// passed — so the assertion is that the two strings are **equal**, taken from the
/// two responses rather than from a fixture either of them could disagree with.
///
/// The seeded plan is unpublishable for other reasons as well — `seed_draft_plan`
/// authors no frequency and `seed_price` no amount, deliberately, so that the
/// authoring suites can drive a plan that never publishes. That is why the
/// assertion **filters** for `WINDOW_COVERAGE_MISSING` instead of taking the whole
/// violation list: the property is that the coverage finding names this key in this
/// spelling, not that coverage is the plan's only fault.
///
/// # The row is **published** here, and after D-332 that is the whole fixture
///
/// The publish now opens coverage itself for a key whose every billable row is a
/// draft it is freezing, so a draft row — what this test seeded until
/// 2026-08-17 — no longer produces the refusal this test is about. The case that
/// keeps the rule's full force is the other one: a key that **had** coverage and
/// lost it, which is an author's mistake rather than an artefact of ordering.
/// Publishing the row through `publish_price` and scheduling nothing is exactly
/// that key, and it is the population an operator actually remediates from.
#[tokio::test]
async fn the_report_names_the_uncovered_key_the_publish_refusal_named() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&h, plan_id).await;
    h.attach_shape(plan_id, 0).await;
    let row = seed_price(&h, plan_id, "EU").await;
    h.publish_price(plan_id, row.price_id).await;

    let refused = h
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", "\"0-3\"")],
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let problem = body_json(refused).await;
    let refusal_subjects: Vec<&str> = problem
        .get("context")
        .and_then(|c| c.get("violations"))
        .and_then(|v| v.as_array())
        .expect("the refusal enumerates violations")
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("WINDOW_COVERAGE_MISSING"))
        .map(|v| {
            v.get("subject")
                .and_then(|s| s.as_str())
                .expect("a named subject")
        })
        .collect();
    assert_eq!(
        refusal_subjects.len(),
        1,
        "one uncovered key, named once: {problem}"
    );

    let report = coverage(&h, plan_id).await;
    assert_eq!(
        keys_of(&report),
        vec![refusal_subjects[0].to_owned()],
        "the report names the key the refusal named, in the same spelling"
    );
    let uncovered = entry(&report, refusal_subjects[0]);
    assert_eq!(uncovered.get("required"), Some(&serde_json::json!(true)));
    assert_eq!(uncovered.get("covered"), Some(&serde_json::json!(false)));
    assert_eq!(
        uncovered.get("coverage_end"),
        Some(&serde_json::json!({ "kind": "uncovered", "at": null }))
    );
    assert_eq!(
        uncovered.get("intervals"),
        Some(&serde_json::json!([])),
        "an uncovered key carries the empty interval list, not an absent one"
    );
    assert_eq!(uncovered.get("interior_gaps"), Some(&serde_json::json!([])));
    // And the row really is the one the report is about.
    assert_eq!(
        report.get("plan_id"),
        Some(&serde_json::json!(plan_id)),
        "the report names its own plan"
    );
    // And the rendered key is the seeded row's own, by its canonical rendering.
    // The equality above already carries this; what a disjunction over two
    // spellings of the currency would add is only tolerance, on the one surface
    // whose whole point is that there is one spelling.
    assert_eq!(keys_of(&report), vec![row.scope_key.to_string()]);
}

/// A covered key reads covered, with its interval and its derived coverage end.
///
/// The world that makes the case above about coverage being *absent* rather than
/// about the report being empty of everything.
#[tokio::test]
async fn a_covered_key_carries_its_interval_and_its_coverage_end() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let report = coverage(&h, plan_id).await;
    let keys = keys_of(&report);
    assert_eq!(keys.len(), 1, "one billable key: {report}");
    let covered = entry(&report, &keys[0]);

    assert_eq!(covered.get("required"), Some(&serde_json::json!(true)));
    assert_eq!(covered.get("covered"), Some(&serde_json::json!(true)));
    assert_eq!(
        covered.get("intervals"),
        Some(&serde_json::json!([{
            "effective_from": wire(common_from()),
            "effective_to": wire(common_to()),
            "state": "scheduled",
        }])),
        "the seed's coverage window, whole"
    );
    assert_eq!(
        covered.get("coverage_end"),
        Some(&serde_json::json!({ "kind": "ends", "at": wire(common_to()) }))
    );
    assert_eq!(covered.get("interior_gaps"), Some(&serde_json::json!([])));
    // The plan really is publishable, so `covered` is not a report about a plan
    // that is broken for some other reason.
    let accepted = h
        .allowed()
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &format!("\"{}-{}\"", 0, seeded.version.get()))],
        ))
        .await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
}

fn common_from() -> OffsetDateTime {
    let (y, m, d) = common::COVERAGE_FROM_UTC;
    utc_ymd_hms(y, m, d, 0, 0, 0)
}

fn common_to() -> OffsetDateTime {
    let (y, m, d) = common::COVERAGE_TO_UTC;
    utc_ymd_hms(y, m, d, 0, 0, 0)
}

/// An interior gap is named with `[gapStart, gapEnd)` — §9's API criterion.
///
/// The two windows this schedules sit after the seed's, so the key holds three
/// intervals and **two** holes: the seed's window is adjacent to neither of them,
/// so the run between it and the first of these is a gap like any other. Both are
/// asserted, in `gapStart` order.
#[tokio::test]
async fn an_interior_gap_is_named_with_its_bounds_and_its_key() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    window(&h, seeded.price_id, 0x_a1, 10, Some(20)).await;
    window(&h, seeded.price_id, 0x_a2, 30, Some(40)).await;

    let report = coverage(&h, plan_id).await;
    let keys = keys_of(&report);
    assert_eq!(keys.len(), 1);
    let key = entry(&report, &keys[0]);

    assert_eq!(
        key.get("interior_gaps"),
        Some(&serde_json::json!([
            // The seed's window ends where the window scale begins, and the first
            // of these starts ten days later.
            { "gap_start": wire(common_to()), "gap_end": wire(at(10)) },
            { "gap_start": wire(at(20)), "gap_end": wire(at(30)) },
        ])),
        "both holes, in gapStart order, half-open"
    );
    assert_eq!(
        key.get("covered"),
        Some(&serde_json::json!(true)),
        "a gapped key still holds a live window: the two rules are different findings"
    );
    assert_eq!(
        key.get("coverage_end"),
        Some(&serde_json::json!({ "kind": "ends", "at": wire(at(40)) })),
        "the coverage end is the far end, which is why the gap list is a separate fact"
    );
}

/// An open-ended window renders `open_ended` and a null instant, never a missing
/// one.
#[tokio::test]
async fn an_open_ended_window_renders_its_own_kind() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    window(&h, seeded.price_id, 0x_b1, 10, None).await;

    let report = coverage(&h, plan_id).await;
    let keys = keys_of(&report);
    assert_eq!(
        entry(&report, &keys[0]).get("coverage_end"),
        Some(&serde_json::json!({ "kind": "open_ended", "at": null }))
    );
}

/// A plan with no rows and no windows is `200` with an empty key list.
///
/// A state, not an absent resource — the reading `frontier.rs` argues for the
/// empty frontier, and for its reason: a 404 here would be indistinguishable from
/// the 404 a scope denial answers with, and this gear answers those identically on
/// purpose.
#[tokio::test]
async fn a_plan_with_nothing_on_it_is_an_empty_report_and_not_a_404() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&h, plan_id).await;

    let report = coverage(&h, plan_id).await;
    assert_eq!(
        report,
        serde_json::json!({ "plan_id": plan_id, "keys": [] })
    );
}

/// A published, priced, windowed plan in the **other** tenant, so an empty
/// coverage report about it is evidence rather than a tautology.
///
/// `seed_foreign_plan` alone leaves a *draft* revision with no price rows and no
/// windows, and `plan_coverage` answers `{plan_id, keys: []}` for such a plan
/// whether or not the read is scoped — which is what
/// `an_absent_plan_and_a_foreign_one_read_alike` rested on until 2026-08-20. Its
/// twin `seed_foreign_current_plan` exists for exactly this reason and states it:
/// "the published state is the one where an unscoped read and a scoped read give
/// different answers, and therefore the only one that can tell them apart."
///
/// Written here rather than in `rest_support` because it is one suite's fixture:
/// the foreign row goes in through the same repositories `seed_price` and
/// `Harness::publish_price` use, with `other_scope()`/`other` substituted for the
/// caller's pair.
async fn seed_foreign_priced_plan(h: &Harness, plan_id: Uuid) -> String {
    use bss_pricing::domain::concurrency::RowVersion;
    use bss_pricing::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
    use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
    use bss_pricing::domain::price_record::PriceContent;
    use bss_pricing::domain::price_row::{ModelKind, PriceRow};
    use bss_pricing::domain::scope_key::{ChargeKind, Cohort, PriceEligibility, Region, ScopeKey};
    use bss_pricing::infra::storage::repo::{NewPriceDraft, price_repo};

    rest_support::seed_foreign_current_plan(h, plan_id).await;

    let key = ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new("USD").expect("currency"),
        Region::new("eu").expect("region"),
        rest_support::seeded_phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("scope key");
    let rendered = key.to_string();

    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(1_500).expect("a non-negative amount"));
    let record = h
        .state
        .prices
        .create_draft(
            &h.other_scope(),
            h.other,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: key,
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    // Stated by the row, because `price_repo::publish_rows` refuses
                    // a publish that resolves no category (H14, review 2026-08-19)
                    // and this seeder publishes against an empty readiness. The
                    // neighbouring `Some("half_up")` below is the same repair one
                    // frozen column over; neither is anything this suite asserts.
                    tax_category_ref: Some("standard".to_owned()),
                    billing_timing: None,
                    proration_contract: Some(ProrationContract {
                        billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
                        proration_basis: ProrationBasis::CalendarDaysActual,
                        credit_on_downgrade: false,
                    }),
                    rounding_policy_ref: None,
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: Uuid::from_u128(0x_f0_e1),
                created_at_utc: common_from(),
                correlation_id: Uuid::from_u128(0x_f0_e2),
            },
        )
        .await
        .expect("seed the foreign draft price row");

    let scope = h.other_scope();
    let tenant = h.other;
    let plan = PlanId::new(plan_id);
    let price_id = record.price_id;
    let (_, outcome) =
        h.db.db()
            .in_transaction::<Vec<Uuid>, bss_pricing::infra::storage::RepoError, _>(move |txn| {
                Box::pin(async move {
                    price_repo::publish_rows(
                        txn,
                        &scope,
                        tenant,
                        plan,
                        &[(price_id, RowVersion::new(0))],
                        &bss_pricing::domain::tax_display::RegionTaxReadiness::empty(),
                        Some("half_up"),
                    )
                    .await
                })
            })
            .await;
    outcome.expect("publish the foreign price row");

    schedule(
        &h.db.conn().expect("conn"),
        &h.other_scope(),
        NewWindow {
            window_id: Uuid::now_v7(),
            tenant_id: h.other,
            price_id,
            effective_from: common_from(),
            effective_to: None,
            reason_code: "seed".to_owned(),
        },
        rest_support::stamp_of(Uuid::from_u128(0x_f0_e1), common_from()),
    )
    .await
    .expect("schedule the foreign window");

    rendered
}

/// A plan that does not exist at all reads the same way, and so does one in
/// another tenant.
///
/// The surface leaks no existence: "there is nothing here" and "there is something
/// here you may not see" are one answer, which is what the SQL-level scope buys.
///
/// **The foreign plan is published, priced and windowed**, which is what makes the
/// empty report a measurement. It was a bare `seed_foreign_plan` draft until
/// 2026-08-20 — no price rows, no windows — so `{plan_id, keys: []}` was the answer
/// an unscoped read would have given too, and the one probe in this file for a
/// cross-tenant leak on this route could not fail. The first `assert` below is that
/// arming — it proves the foreign world is not empty — and the second names the
/// predicate that excludes it: the tenant argument, not the access scope.
#[tokio::test]
async fn an_absent_plan_and_a_foreign_one_read_alike() {
    let h = Harness::new().await;
    let absent = Uuid::now_v7();
    let foreign = Uuid::now_v7();
    let foreign_key = seed_foreign_priced_plan(&h, foreign).await;

    // **The arming, and what it does and does not model** (2026-08-21 review,
    // finding 7). This read proves the foreign world is **not empty** — one
    // published billable row on `foreign_key` — which is what turns the `keys: []`
    // below into evidence about exclusion rather than a statement about an empty
    // plan. That is all it proves, and the doc above used to claim more: it is not
    // "the answer an unscoped read would give" for this route, because
    // `price_repo::load_page` puts `price::Column::TenantId.eq(tenant_id)` in its
    // `WHERE` **independently** of `.secure().scope_with(scope)`, and
    // `get_plan_coverage` always passes `ctx.subject_tenant_id()`. Deleting the
    // `scope_with` would not make this route leak, so no probe shaped like this one
    // can arm against it.
    //
    // Which predicate does the excluding is therefore measured directly, below.
    let foreign_world = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.db.conn().expect("conn"),
        &toolkit_db::secure::AccessScope::allow_all(),
        h.other,
        PlanId::new(foreign),
        &[bss_pricing::domain::lifecycle::LifecycleState::Published],
    )
    .await
    .expect("read the foreign plan's rows");
    assert_eq!(
        foreign_world
            .iter()
            .map(|record| record.scope_key.to_string())
            .collect::<Vec<_>>(),
        vec![foreign_key],
        "the foreign plan is published, priced and windowed, so an empty coverage report \
         below is a measurement rather than a statement about an empty plan"
    );

    // **The tenant argument is what excludes it**, and this is the assertion that
    // says so: the same repository call under a scope that admits everything, on the
    // *caller's* tenant — which is what `get_plan_coverage` passes — answers nothing
    // for the foreign plan. The exclusion in the report below is this predicate's,
    // and naming it here is what stops the case reading as a scope probe it is not.
    let under_callers_tenant = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &h.db.conn().expect("conn"),
        &toolkit_db::secure::AccessScope::allow_all(),
        h.tenant,
        PlanId::new(foreign),
        &[bss_pricing::domain::lifecycle::LifecycleState::Published],
    )
    .await
    .expect("read the foreign plan under the caller's tenant");
    assert!(
        under_callers_tenant.is_empty(),
        "the caller's tenant sees none of the foreign plan's rows even under allow_all, so the \
         tenant predicate is what the empty report below measures: {:?}",
        under_callers_tenant
            .iter()
            .map(|record| record.scope_key.to_string())
            .collect::<Vec<_>>()
    );

    assert_eq!(
        coverage(&h, absent).await,
        serde_json::json!({ "plan_id": absent, "keys": [] })
    );
    assert_eq!(
        coverage(&h, foreign).await,
        serde_json::json!({ "plan_id": foreign, "keys": [] }),
        "a foreign tenant's published, priced, windowed plan reports exactly what an \
         absent one does"
    );
}

/// A hybrid's component keys each get their own entry, and only the uncovered ones
/// read uncovered.
///
/// `inst-wc-perkey` at the surface: covering the recurring key covers nothing else,
/// so the report has to be per key or an operator cannot tell which line to fix.
#[tokio::test]
async fn every_component_key_gets_its_own_entry() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    // A second row on a different market, with no window of its own.
    let uncovered = seed_price(&h, plan_id, "US").await;
    let covered =
        rest_support::publishable_scope_key(PlanId::new(plan_id), seeded.phase, "eu").to_string();
    let uncovered = uncovered.scope_key.to_string();

    let report = coverage(&h, plan_id).await;

    // **Which** key is covered, not how many. A count of one is satisfied by a
    // defect that attached the seed's window to the other row's key, and that is
    // the exact defect `inst-wc-perkey` exists to refuse.
    assert_eq!(
        entry(&report, &covered).get("covered"),
        Some(&serde_json::json!(true)),
        "the key the seed's window is on"
    );
    assert_eq!(
        entry(&report, &uncovered).get("covered"),
        Some(&serde_json::json!(false)),
        "and covering the first covers nothing else"
    );

    // Two keys, two entries, sorted by the canonical rendering so two reads of one
    // state read identically — asserted as the whole list rather than as a length
    // plus a `contains`.
    let mut expected = vec![covered, uncovered];
    expected.sort();
    assert_eq!(keys_of(&report), expected, "{report}");
}

/// The route declares no 422 and answers no 400 of its own.
///
/// The path parameter is a bare axum `Path<Uuid>`, not `toolkit`'s
/// canonical-error-aware `extract::Path` — so axum rejects it before the
/// handler, and before this gear's own error ladder ever runs.
/// `canonical_error_middleware` still wraps the rejection into a Problem
/// (ADR-0006's ingestion-side fallback), but a generic one: RFC 9457 §4.2.1's
/// `about:blank`, with no `context.resource_type` — the tell that distinguishes
/// "the platform caught a foreign 4xx" from "this gear answered". The 422 half
/// is `module_test.rs`'s property for every route; this asserts the shape of
/// the one refusal a caller can provoke here.
#[tokio::test]
async fn a_malformed_plan_id_never_reaches_the_handler() {
    let h = Harness::new().await;
    let response = h
        .allowed()
        .send(request(
            "GET",
            "/bss-pricing/v1/plans/not-a-uuid/coverage",
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a readable body");
    let problem: serde_json::Value =
        serde_json::from_slice(&body).expect("the middleware fallback always wraps a Problem");
    assert_eq!(
        problem,
        serde_json::json!({
            "type": "about:blank",
            "title": "Bad Request",
            "status": 400,
            "detail": "Bad Request",
            "instance": "/bss-pricing/v1/plans/not-a-uuid/coverage",
            "context": {},
        }),
        "a gear-authored refusal would name its own resource_type in context, not \
         arrive as the generic about:blank fallback: {problem}"
    );
}

/// **A malformed query parameter *does* reach the handler, and is refused as a
/// problem document.**
///
/// The deliberate contrast with the case above, and the reason both sit here. A
/// path segment is the router's to reject and this gear never sees it; a query
/// parameter is not, so a `Query<T>` member typed `Option<u64>` handed the refusal
/// to axum's extractor and answered the same bodiless 400 — against a registration
/// whose declared 400 has `Problem` as its schema.
///
/// `?limit=abc` on the nine paginated reads answered that until 2026-08-17.
/// `?price_id=` is now a retired named key and is refused as an extra query
/// parameter. `cursor::parse_limit` still refuses a non-numeric limit.
/// `module_test`'s `no_query_struct_lets_the_extractor_answer` is the derivation;
/// this is the wire proof that the derivation is about something real.
#[tokio::test]
async fn a_malformed_query_parameter_is_refused_as_a_problem_document() {
    let h = Harness::new().await;

    for (what, uri, named) in [
        (
            "a limit that is not a number",
            "/bss-pricing/v1/price-windows?limit=abc",
            "limit",
        ),
        (
            "a retired named price filter",
            "/bss-pricing/v1/price-windows?price_id=nope",
            "price_id",
        ),
    ] {
        let response = h.allowed().send(request("GET", uri, None)).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{what} is refused"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a readable body");
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|_| {
            panic!(
                "{what}: the refusal is this gear's, so it is a problem document; got {:?}",
                String::from_utf8_lossy(&body)
            )
        });
        assert_eq!(
            problem["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "{what}: {problem}"
        );
        // The OData extractor names the parameter in `field_violations`, not
        // always in `detail`. Search both, never `instance` - that echoes the
        // request path and would satisfy this for free.
        let detail = problem["detail"].as_str().unwrap_or_default().to_owned();
        let violations = problem["context"]["field_violations"].to_string();
        assert!(
            detail.contains(named) || violations.contains(named),
            "{what}: the refusal names `{named}`, its own parameter, and not merely \
             some parameter of the request: {problem}"
        );
    }
}

/// The report ranges over the publish check's row set, so a `draft` row's key is
/// in it.
///
/// The remediation surface has to answer about the rows a publish would produce,
/// not only the ones already published — otherwise an operator whose publish was
/// refused on a draft row's key finds nothing here about it.
#[tokio::test]
async fn a_draft_rows_key_is_in_the_report() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    seed_draft_plan(&h, plan_id).await;
    h.attach_shape(plan_id, 0).await;
    let row = seed_price(&h, plan_id, "EU").await;

    let report = coverage(&h, plan_id).await;
    assert_eq!(keys_of(&report).len(), 1);
    assert_eq!(
        row.lifecycle_state,
        bss_pricing::domain::lifecycle::LifecycleState::Draft,
        "the row really is a draft, which is the whole premise"
    );
    assert_eq!(
        entry(&report, &keys_of(&report)[0]).get("required"),
        Some(&serde_json::json!(true))
    );
}

/// **The route's stated reason for existing, asserted: it needs no open draft
/// revision.**
///
/// `windows.rs` files this surface under "operator remediation" on exactly this
/// ground — `infra::publish::assemble` answers `NotFound` for a plan with nothing
/// to publish, so a published plan whose coverage an operator wants to inspect,
/// **the ordinary case**, cannot be pre-checked at all. The premise is true and
/// nothing checked the conclusion: every other case in this file runs against a
/// plan that still holds an open draft revision, and the one that publishes does
/// so after its last `GET`. A regression making this handler resolve through
/// `assemble`, or require a draft, would answer 404 for the entire population the
/// surface exists for with every one of those cases green.
///
/// So the plan is published for real — submit, an independent approve, commit —
/// and the absence of a draft is asserted rather than assumed before the report is
/// read again.
#[tokio::test]
async fn a_published_plan_with_no_open_draft_is_still_reported() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    let key =
        rest_support::publishable_scope_key(PlanId::new(plan_id), seeded.phase, "eu").to_string();

    let submitted = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(submitted.status(), StatusCode::ACCEPTED);
    let approval_id = units_of(&h, AuditSubjectKind::PlanRevision).await[0].approval_id;
    let approved = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(approved.status(), StatusCode::OK);
    let committed = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &seeded.etag())],
        ))
        .await;
    assert_eq!(committed.status(), StatusCode::OK, "the commit arm");

    // The premise of the whole test: there is no draft revision left to resolve
    // through, and the row this reports on is published.
    assert_eq!(
        rest_support::plan_state(&h, plan_id, seeded.revision)
            .await
            .as_deref(),
        Some("published")
    );
    assert_eq!(
        rest_support::price_rows(&h, plan_id).await[0].lifecycle_state,
        bss_pricing::domain::lifecycle::LifecycleState::Published,
        "the candidate set is now the published plane, not a draft"
    );

    let report = coverage(&h, plan_id).await;
    assert_eq!(keys_of(&report), vec![key.clone()]);
    let entry = entry(&report, &key);
    assert_eq!(entry.get("required"), Some(&serde_json::json!(true)));
    assert_eq!(
        entry.get("covered"),
        Some(&serde_json::json!(true)),
        "the published row's key still reads covered: {report}"
    );
    assert_eq!(
        entry.get("coverage_end"),
        Some(&serde_json::json!({ "kind": "ends", "at": wire(common_to()) }))
    );
}

/// A cancelled window is reported, and does not count as coverage.
///
/// Both halves matter and they pull in opposite directions: the interval is in the
/// list because "why did this key lose its successor" is the question the surface
/// answers, and it is not coverage because a cancelled window is a schedule that
/// never happened.
#[tokio::test]
async fn a_cancelled_window_is_reported_and_is_not_coverage() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    let conn = h.state.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::window_repo::transition(
        &conn,
        &h.scope(),
        h.tenant,
        common::coverage_window_id(seeded.price_id),
        bss_pricing::domain::window::WindowState::Cancelled,
        at(0),
        stamp(),
    )
    .await
    .expect("cancel the seed's window");

    let report = coverage(&h, plan_id).await;
    let keys = keys_of(&report);
    let key = entry(&report, &keys[0]);

    assert_eq!(
        key.get("intervals"),
        Some(&serde_json::json!([{
            "effective_from": wire(common_from()),
            "effective_to": wire(common_to()),
            "state": "cancelled",
        }])),
        "the cancelled interval is in the report"
    );
    assert_eq!(key.get("covered"), Some(&serde_json::json!(false)));
    assert_eq!(
        key.get("coverage_end"),
        Some(&serde_json::json!({ "kind": "uncovered", "at": null })),
        "and contributes no coverage"
    );
}

// ===========================================================================
// The three mutating routes (§5): `POST …/prices/{priceId}/windows`,
// `PATCH` and `DELETE …/price-windows/{windowId}`
// ===========================================================================
//
// # What this half of the file owns, and what already existed
//
// The **mapping** of every code below was already asserted — `error_mapping_tests`
// reads each one off the typed context and compares it by equality, and
// `rest_authz`'s census carries these three routes through the PDP-pair, deny and
// outage properties. What nothing asserted was the **raising**: that a request a
// caller can actually compose reaches the guard, and that the guard's answer reaches
// the wire under the code and status the mapping declares. Two of these codes —
// `WINDOW_NOT_CANCELLABLE` and `WINDOW_START_IN_PAST` — had **no non-test caller at
// all** before the routes were mounted, so these are the cases that make them
// reachable facts rather than declared ones.
//
// # Every world here is a **published** plan, and that is a premise
//
// `WindowService` resolves the plan's *current* revision (D-99, via
// `PlanPublishUnit::window_mutation`), so a plan that never published has no window
// surface at all and every case below would be a 404 about staging rather than a
// refusal about a rule.

/// The `If-Match` the PATCH requires. The route parses it and the **store** does the
/// comparing inside the writing transaction (D-176), so a well-formed tag is all a
/// case about a window rule needs.
const IF_MATCH: &str = "\"0\"";

fn windows_path(price_id: Uuid) -> String {
    bss_pricing::api::rest::windows::PRICE_WINDOWS.replace("{priceId}", &price_id.to_string())
}

fn window_path(window_id: Uuid) -> String {
    bss_pricing::api::rest::windows::PRICE_WINDOW.replace("{windowId}", &window_id.to_string())
}

/// The `reason` of a 409, read off the typed context and returned for an **equality**
/// comparison.
///
/// Never a `contains` over the rendered document: `error_mapping_tests`'s note
/// records the measurement — `WINDOW_OVERLAP` corrupted to `WINDOW_OVERLAPX` left a
/// containment assertion green, because the corrupted string contains the correct
/// one. This is that lesson applied on the wire side, where the field a consumer
/// branches on is `context.reason`.
async fn conflict_reason(response: axum::http::Response<axum::body::Body>) -> String {
    let body = body_json(response).await;
    body.get("context")
        .and_then(|c| c.get("reason"))
        .and_then(|r| r.as_str())
        .unwrap_or_else(|| panic!("a 409 carries context.reason: {body}"))
        .to_owned()
}

/// Every precondition violation of a 400, as `(type, subject)` pairs.
///
/// The 400 class discriminates on `context.violations[].type`, and the `subject` is
/// asserted beside it because it is what tells an operator *which field to move* —
/// D-146's line, and the difference between a refusal they can act on and one they
/// can only read.
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

/// A plan whose revision **and** price row are published, so `load_current`
/// resolves and the projected plane sees the row — **and no threshold policy**, so
/// every act on it is material.
///
/// The unconfigured half of the pair, and it is a fixture rather than an oversight:
/// `inst-mat-failsafe` is the state every tenant starts in, so this is the world in
/// which a schedule and a lengthening open a unit and write nothing.
async fn published_unconfigured(h: &Harness, plan_id: Uuid) -> rest_support::Publishable {
    let seeded = seed_publishable_plan(h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;
    seeded
}

/// The same plan, **plus an approved threshold policy on `EUR`** — the currency
/// `publishable_scope_key` puts the seeded row on.
///
/// This is what makes a schedule and a lengthening commit on one principal, and every
/// case below that is about a *mutation* rather than about a refusal needs it: a window
/// mutation moves an interval and no money, so its per-row delta is zero and the only
/// rule that can answer is whether the row's currency has an entry at all. The bar's
/// size is therefore irrelevant and is set far above anything any case moves, so that
/// no test here depends on a comparison it is not about.
async fn published(h: &Harness, plan_id: Uuid) -> rest_support::Publishable {
    let seeded = published_unconfigured(h, plan_id).await;
    rest_support::approve_threshold_policy(h, &[("EUR", 100_000)]).await;
    seeded
}

/// `POST` a window on `price_id`, under a **fresh** client key.
///
/// One key per call, because the `POST` is guarded at-most-once (D-191): a shared
/// constant would make the second schedule in any case a *replay* of the first rather
/// than the act the case is about. The cases that are about the gate name their key.
async fn post_window(
    h: &Harness,
    price_id: Uuid,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> axum::http::Response<axum::body::Body> {
    post_window_under(h, price_id, from, to, &Uuid::now_v7().to_string()).await
}

/// `POST` a window under a caller-chosen client key — the cases about the gate.
async fn post_window_under(
    h: &Harness,
    price_id: Uuid,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
    key: &str,
) -> axum::http::Response<axum::body::Body> {
    h.allowed()
        .send(with_headers(
            "POST",
            &windows_path(price_id),
            Some(serde_json::json!({
                "effective_from": wire(from),
                "effective_to": to.map(wire),
                "reason_code": "priceIncrease",
            })),
            &[("idempotency-key", key)],
        ))
        .await
}

/// `PATCH` a window's end, asserting the act sequence a fresh window stands at.
async fn patch_window(
    h: &Harness,
    window_id: Uuid,
    to: Option<OffsetDateTime>,
) -> axum::http::Response<axum::body::Body> {
    patch_window_asserting(h, window_id, to, IF_MATCH).await
}

/// `PATCH` a window's end under a caller-chosen tag — the cases that are *about* the
/// precondition rather than about a window rule.
async fn patch_window_asserting(
    h: &Harness,
    window_id: Uuid,
    to: Option<OffsetDateTime>,
    if_match: &str,
) -> axum::http::Response<axum::body::Body> {
    h.allowed()
        .send(with_headers(
            "PATCH",
            &window_path(window_id),
            Some(serde_json::json!({ "effective_to": to.map(wire) })),
            &[("if-match", if_match)],
        ))
        .await
}

/// The `ETag` a mutating window verb hands back, or `None` when it emitted none.
fn etag_of(response: &axum::http::Response<axum::body::Body>) -> Option<String> {
    response
        .headers()
        .get(axum::http::header::ETAG)
        .map(|v| v.to_str().expect("an entity tag is ASCII").to_owned())
}

/// `DELETE` a window. **No headers at all** — §5's Idempotency cell for this
/// surface is empty, which `windows.rs`'s module doc reports as a divergence rather
/// than improves on. Sending one here would assert a contract the design set does
/// not state.
async fn delete_window(h: &Harness, window_id: Uuid) -> axum::http::Response<axum::body::Body> {
    h.allowed()
        .send(request("DELETE", &window_path(window_id), None))
        .await
}

/// Every approval unit of one subject kind.
///
/// **Kind-scoped rather than "every row", and that is a strengthening.** The
/// fixture that configures a threshold policy opens and decides a `policy` unit of its
/// own, so `approval_rows(..).is_empty()` would stop meaning "this act opened no unit"
/// and start meaning "the tenant has no units at all" - false for a reason that has
/// nothing to do with the act under test. Every assertion below names the kind it is
/// about.
async fn units_of(
    h: &Harness,
    kind: AuditSubjectKind,
) -> Vec<bss_pricing::infra::storage::entity::approval::Model> {
    rest_support::approval_rows(h)
        .await
        .into_iter()
        .filter(|row| row.subject_kind == kind.as_str())
        .collect()
}

/// The stored state of one window, read past every route.
async fn window_state(h: &Harness, window_id: Uuid) -> Option<String> {
    let conn = h.state.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::window_repo::find(&conn, &h.scope(), h.tenant, window_id)
        .await
        .expect("read the window")
        .map(|w| w.state.as_str().to_owned())
}

// ---------------------------------------------------------------------------
// The 202, and what the body says
// ---------------------------------------------------------------------------

/// **`POST` answers 202 with the pending handle, and the row is really there — under a
/// configured threshold policy.**
///
/// The whole body is asserted as one object rather than field by field, so a field
/// silently dropped from `WindowMutationOutcomeView` reddens this instead of going
/// unnoticed — a `get(..)` per field passes over an absent one that nobody thought to
/// name.
///
/// The interval is **adjacent** to the seed's window (`effective_from` =
/// `COVERAGE_TO_UTC`), which is the one staging that reaches the success arm: a later
/// start would open an interior gap and an earlier one would overlap. Adjacency
/// being legal is itself the half-open interval's property, so the 202 carries that
/// too.
///
/// **The policy is what makes this the mutating arm**, and its twin
/// [`a_scheduled_window_with_no_threshold_policy_opens_a_unit_and_writes_nothing`] is
/// the same call without it. Before the evaluator was wired a schedule committed
/// unconditionally, so this case could not tell "the rule allowed it" from "no rule ran".
#[tokio::test]
async fn a_scheduled_window_answers_202_with_the_pending_version_handle() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    let response = post_window(&h, seeded.price_id, common_to(), None).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;

    let window_id = body
        .pointer("/window/window_id")
        .and_then(|v| v.as_str())
        .map_or_else(
            || panic!("the body names the window it created: {body}"),
            |s| Uuid::parse_str(s).expect("a uuid"),
        );
    assert_eq!(
        body,
        serde_json::json!({
            "outcome": "mutated",
            // No verdict and no unit on the mutating arm: the evaluator answered
            // `AutoPublishable`, so there is nothing for a reviewer to be told about.
            "materiality": null,
            "approval": null,
            "window": {
                "window_id": window_id,
                "plan_id": plan_id,
                "price_id": seeded.price_id,
                "revision": seeded.revision,
                // The registry double's first handle. Asserted rather than skipped: it
                // is the only thing in the body a caller can act on, and a 202 whose
                // handle were absent or empty would be a 202 promising nothing.
                "pending_version_ref": "pend-0",
                "effective_from": wire(common_to()),
                "effective_to": null,
                "state": "scheduled",
            },
        }),
        "the whole 202 body, so a dropped field reddens here"
    );

    // And the truth side moved, which is what makes the 202 about a mutation
    // rather than about a handler that answers 202.
    assert_eq!(
        window_state(&h, window_id).await.as_deref(),
        Some("scheduled")
    );
    assert!(
        units_of(&h, AuditSubjectKind::Window).await.is_empty(),
        "an act the threshold cleared summons nobody"
    );
}

/// **The twin: the same schedule with no threshold policy opens a unit and writes
/// nothing.**
///
/// `inst-mat-failsafe`, on the surface G4 left committing unconditionally. A schedule is
/// on nobody's trigger list, so what decides it is the tenant's policy — and a tenant
/// that has configured none has *everything* material, which is the design set's own
/// bootstrap and the state every tenant starts in.
///
/// The two assertions that make it evidence: the reason is **`noConfiguredThreshold`**,
/// so an operator is told the fact they can act on rather than D-62's, and the window
/// plane is **empty**, so the 202 is not a mutation with a mislabelled body.
#[tokio::test]
async fn a_scheduled_window_with_no_threshold_policy_opens_a_unit_and_writes_nothing() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published_unconfigured(&h, plan_id).await;

    let response = post_window(&h, seeded.price_id, common_to(), None).await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "both arms answer 202; `outcome` is what tells them apart"
    );
    let body = body_json(response).await;
    assert_eq!(body["outcome"], "submitted_for_approval");
    assert_eq!(
        body["materiality"],
        serde_json::json!({
            "material": true,
            "reason": "noConfiguredThreshold",
            "tripped": null,
            // A fail-safe is an answer about the policy, so it names no act.
            "trigger": null,
        }),
        "the evaluator's own reason, not the trigger list's: {body}"
    );
    assert_eq!(
        body["window"]["pending_version_ref"],
        serde_json::Value::Null,
        "no version was requested, because nothing was written: {body}"
    );

    // The unit is real, pending, and pinned to the window that does **not** exist —
    // which is the whole reason its `subject_ref` carries the plan.
    let opened = rest_support::approval_row(&h, opened_unit(&body)).await;
    assert_eq!(opened.subject_kind, AuditSubjectKind::Window);
    assert_eq!(opened.state, ApprovalState::Submitted);
    let window_id = body["window"]["window_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("the 202 names the window it would have created");
    // **A schedule's subject is the act and carries no window id** (D-184 clause (2)).
    // The id of a window a `POST` would create is minted per request, so a subject
    // naming it could not be reproduced by the call made after the approve — the
    // retry named a different subject and opened a second unit. Both ends are in it
    // and not the transition `act_token` builds: a window that does not exist has no
    // prior end, so the transition would render `schedule/open/open` for every
    // schedule on this row and two proposals with different starts would collide.
    assert_eq!(
        opened.subject_ref,
        format!(
            "{plan_id}/schedule/{}/{}/open",
            seeded.price_id,
            format_rfc3339(common_to())
        ),
        "the subject names the plan, the act, the price row and the proposed interval"
    );

    // Nothing was written, on the plane the act was about.
    assert_eq!(
        window_state(&h, window_id).await,
        None,
        "the refused schedule wrote no window row at all"
    );
}

/// **An approved schedule completes when the caller re-issues it** — D-184 clause (2),
/// as the owner decided it on 2026-08-05.
///
/// The completion path a materially-refused `POST` had and could not walk. The window
/// id used to be minted **per request**, so the call made after the approve named a
/// different subject from the one that was approved, found no unit, and opened a
/// second one — an approval loop with no exit. Under the decision the unit's subject
/// is the **proposed act** (the price row, the interval, the reason), so the re-issued
/// call computes the same subject, finds the approval an independent principal gave
/// it, and commits. The window id is an **outcome**, minted at the commit.
///
/// The assertion that carries the meaning is not the 202 but the pair: `outcome`
/// becomes `mutated`, and the tenant still holds exactly **one** window unit. A build
/// that opened a second unit would also answer 202, which is why the count is here.
#[tokio::test]
async fn an_approved_schedule_commits_when_the_same_call_is_re_issued() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published_unconfigured(&h, plan_id).await;

    // No threshold policy, so a schedule is material and writes nothing.
    let refused = post_window(&h, seeded.price_id, common_to(), None).await;
    assert_eq!(refused.status(), StatusCode::ACCEPTED);
    let refused = body_json(refused).await;
    assert_eq!(
        refused["outcome"], "submitted_for_approval",
        "an unconfigured tenant makes every act material: {refused}"
    );
    let unit = opened_unit(&refused);

    approve(&h, unit).await;

    // The same call again - same price row, same interval, same reason.
    let committed = post_window(&h, seeded.price_id, common_to(), None).await;
    assert_eq!(committed.status(), StatusCode::ACCEPTED);
    let committed = body_json(committed).await;

    assert_eq!(
        committed["outcome"], "mutated",
        "the re-issued call is the approved act, so it commits rather than asking again: \
         {committed}"
    );
    assert_eq!(
        units_of(&h, AuditSubjectKind::Window).await.len(),
        1,
        "and it opened no second unit - a subject the retry cannot reproduce is the defect \
         this decision closes"
    );

    // The window exists now, and it is the interval that was approved.
    let window_id = committed["window"]["window_id"]
        .as_str()
        .map(|s| Uuid::parse_str(s).expect("a uuid"))
        .expect("the commit names the window it minted");
    assert_eq!(
        window_state(&h, window_id).await.as_deref(),
        Some("scheduled"),
        "the act took effect on the plane it was about"
    );
}

/// **A reviewer of a window unit is shown the act, not just the plan** — the other
/// half of D-184 clause (2).
///
/// D-61 requires that a reviewer read what they are being asked to sign for. For a
/// window unit they could not: the pinned subject is the plan shape as it already
/// stands, so the detail surface rendered the world the act would *leave* and named
/// no operation at all. A reviewer approving a cancel and a reviewer approving a
/// lengthening were shown the same document.
///
/// The act is rendered from the record's own `subject_ref`, which is where D-184
/// clause (1) put it and which an append-only store cannot move — so the act is
/// authenticated by the record rather than by the pin, and the pin keeps answering
/// the question it exists for (has the plan's content moved since the reviewer
/// looked). Folding the act into the digest would re-freeze a pin for a fact
/// already immutable.
#[tokio::test]
async fn a_window_units_detail_renders_the_act_being_signed_for() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (seeded, first) = cancellable(&h, plan_id).await;
    let _ = &seeded;

    let opened = body_json(delete_window(&h, first).await).await;
    assert_eq!(opened["outcome"], "submitted_for_approval", "{opened}");
    let unit = opened_unit(&opened);

    let detail = h
        .allowed_as(APPROVER)
        .send(request(
            "GET",
            &format!("/bss-pricing/v1/approvals/{unit}"),
            None,
        ))
        .await;
    assert_eq!(detail.status(), StatusCode::OK);
    let detail = body_json(detail).await;

    assert_eq!(
        detail["proposed_act"]["operation"], "cancel",
        "the reviewer is told which act they are signing for: {detail}"
    );
    assert_eq!(
        detail["proposed_act"]["window_id"],
        first.to_string(),
        "and on which window: {detail}"
    );
    assert_eq!(
        detail["proposed_act"]["current_effective_to"],
        // The view echoes the act token's own spelling (`interval_end`), so the
        // literal is the gear's one rendering. It was chrono's `to_rfc3339()`
        // form here, kept so a retry across the type change still found its unit;
        // that premise went with the third renderer, and the token now reads the
        // way every other instant on this document does.
        "2099-09-01T00:00:00.000000Z",
        "and the end the key loses if it commits - the fixture window's own: {detail}"
    );
}

/// Publish `plan_id` through the mounted surfaces: submit, an **independent**
/// approve, then the commit call.
///
/// Both calls carry the same tag, which is the publish route's own two-arm shape:
/// the submit freezes nothing, so the revision's version has not moved when the
/// commit arrives.
async fn publish_through_the_routes(h: &Harness, plan_id: Uuid, etag: &str) {
    let submitted = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", etag)],
        ))
        .await;
    assert_eq!(
        submitted.status(),
        StatusCode::ACCEPTED,
        "the submit arm opens the unit"
    );
    let approval_id = units_of(h, AuditSubjectKind::PlanRevision).await[0].approval_id;
    let approved = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        approved.status(),
        StatusCode::OK,
        "a second principal signs"
    );
    let committed = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", etag)],
        ))
        .await;
    assert_eq!(
        committed.status(),
        StatusCode::OK,
        "the commit arm freezes the revision"
    );
}

/// **D-332: a priced plan publishes on its FIRST call**, and the window it needed
/// is the one the publish opened.
///
/// The test beside this one records the order that used to be forced — empty
/// publish, then the row and its window, then publish again — and why it was
/// reachable. This one records that it is no longer *required*: the author files
/// a row and publishes, once.
///
/// What it pins is the pair. The plan reaches `published` **and** the key holds
/// a live window whose start is the commit instant, because either alone would
/// pass on a bug: a publish that skipped the coverage rule would satisfy the
/// first, and a window written without the row being frozen would satisfy the
/// second.
#[tokio::test]
async fn a_priced_plan_publishes_on_its_first_call_and_the_publish_opens_the_window() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let shape = rest_support::seed_publishable_shape(&h, plan_id).await;
    // `seed_priced_row`, not `seed_price`: the latter leaves the row
    // unpublishable for five other reasons, and a probe that reddened on those
    // would say nothing about coverage.
    let row = rest_support::seed_priced_row_on_phase(&h, plan_id, "EU", 1_500, shape.phase).await;

    // Submitted by hand rather than through the helper, so a refusal names the
    // rule that produced it instead of a bare status.
    let probe = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &shape.etag())],
        ))
        .await;
    let status = probe.status();
    let seen = rest_support::body_json(probe).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "the submit was refused: {seen}"
    );

    let approval_id = units_of(&h, AuditSubjectKind::PlanRevision).await[0].approval_id;
    let approved = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(approved.status(), StatusCode::OK, "the approve");
    let committed = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &publish_path(plan_id),
            None,
            &[("if-match", &shape.etag())],
        ))
        .await;
    let committed_status = committed.status();
    let committed_body = rest_support::body_json(committed).await;
    assert_eq!(
        committed_status,
        StatusCode::OK,
        "the commit was refused: {committed_body}"
    );

    assert_eq!(
        rest_support::plan_state(&h, plan_id, shape.revision)
            .await
            .as_deref(),
        Some("published"),
        "a priced plan's first publish is refused only by the coverage rule, and the \
         publish now satisfies it by opening the window itself"
    );

    let conn = h.state.db.conn().expect("conn");
    let windows = bss_pricing::infra::storage::repo::window_repo::list_for_plan(
        &conn,
        &h.scope(),
        h.tenant,
        PlanId::new(plan_id),
    )
    .await
    .expect("read the plane");
    let _ = row;
    assert_eq!(
        windows.len(),
        1,
        "exactly one window, opened by the publish: {windows:?}"
    );
    assert_eq!(
        windows[0].state,
        bss_pricing::domain::window::WindowState::Scheduled,
        "the publish writes `scheduled` and leaves activation to the sweep, which is \
         the one thing that decides it"
    );
    assert!(
        windows[0].effective_to.is_none(),
        "open-ended: the publish opens coverage, it does not end it"
    );
}

/// **A plan's first billable row and its first window are authorable through the
/// mounted surfaces — it takes two publishes, and this executes both.**
///
/// The case that settles what `infra::window`'s divergence section is allowed to
/// claim. The report it replaced said *"a plan cannot make its first publish through
/// the mounted surfaces at all"*, and this is the counterexample: every step through a
/// mounted route, no fixture reaching past them, ending in a **mutating** 202 on a
/// window of a key that held none. It grew a step when the schedule began consulting
/// the threshold policy — six calls now rather than three — and the step is D-10's own
/// two-person configuration act, so the sequence an operator performs is longer and
/// still contains no deadlock.
///
/// **Why the empty publish is not a trick.** `coverage::check` ranges over the
/// **billable** set, so a plan with no price row presents no key for
/// `inst-wc-required` to find uncovered, and `run_publish_rules` has no
/// minimum-row rule — a plan whose shape is sound publishes with an empty row set.
/// That is the state the sequence starts from, and it is asserted (`published`, with
/// `price_rows` empty — no row was frozen) rather than assumed, because the whole
/// argument rests on it.
///
/// This block sat above the `#[tokio::test]` of the test *beside* this one until
/// 2026-08-20 — a second doc attribute on a function that executes **one** publish
/// and makes no such assertion — so rustdoc showed two contradictory descriptions of
/// that case and none of this one.
///
/// What is **true**, and is the narrow statement the module doc now carries: the
/// billable row cannot ride the *first* publish, because at that moment its key holds
/// no window and no window can be scheduled on a plan with no current revision. So
/// the order is forced — publish, author, schedule, and the row rides the next
/// publish — and nothing about it is a deadlock.
#[tokio::test]
async fn a_plans_first_window_is_authorable_through_the_routes_after_an_empty_publish() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let shape = rest_support::seed_publishable_shape(&h, plan_id).await;

    // 1. The empty first publish, through submit → independent approve → commit.
    publish_through_the_routes(&h, plan_id, &shape.etag()).await;
    assert_eq!(
        rest_support::plan_state(&h, plan_id, shape.revision)
            .await
            .as_deref(),
        Some("published"),
        "a plan with no price row publishes: the coverage rule ranges over the billable set"
    );
    assert!(
        rest_support::price_rows(&h, plan_id).await.is_empty(),
        "and it froze no rows, which is what makes the next step the first authoring"
    );

    // 2. The billable row, authored through `POST /plans/{planId}/prices`.
    let created = h
        .allowed_as(SUBMITTER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/plans/{plan_id}/prices"),
            Some(serde_json::json!({
                "scope_key": {
                    "currency": "EUR",
                    "region": "eu",
                    "phase": shape.phase.get().to_string(),
                    "price_eligibility": "all_subscriptions",
                    "charge_kind": "recurring",
                    "cohort": null
                },
                "content": {
                    "model_kind": "flat",
                    "amount_minor": 9_900,
                    "tax_inclusive": false
                }
            })),
            &[("idempotency-key", "first-row-of-a-published-plan")],
        ))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let price_id = body_json(created)
        .await
        .get("price_id")
        .and_then(|v| v.as_str())
        .map(|s| Uuid::parse_str(s).expect("a uuid"))
        .expect("the create names the row it authored");

    // 3. The threshold policy, which is the step the sequence grew when the schedule
    // began consulting the evaluator. It is not a detour: a schedule is not on
    // `inst-mat-registered`'s list, so an unconfigured tenant gets a unit and no
    // window — and configuring the policy is itself a two-person act (D-10), which is
    // the same shape as step 1. The plan has **no published price row** at this point,
    // so the change set carries none and no bar is consulted at all; the entry is
    // authored on the row's own currency because that is what an operator would do.
    rest_support::approve_threshold_policy(&h, &[("EUR", 100_000)]).await;

    // 4. The window on it, through `POST /prices/{priceId}/windows` — 202, mutating.
    let scheduled = post_window(&h, price_id, at(0), None).await;
    assert_eq!(
        scheduled.status(),
        StatusCode::ACCEPTED,
        "the plan's first window, through the route it is declared on"
    );
    let body = body_json(scheduled).await;
    assert_eq!(
        body["outcome"], "mutated",
        "the window is written, not merely proposed: {body}"
    );
    assert_eq!(body["window"]["plan_id"], plan_id.to_string());
    assert_eq!(body["window"]["price_id"], price_id.to_string());
    assert_eq!(
        body["window"]["revision"], shape.revision,
        "the mutation freezes the plan's current revision, which is the one just published"
    );
    assert_eq!(body["window"]["state"], "scheduled");
    assert!(
        body["window"]["pending_version_ref"]
            .as_str()
            .is_some_and(|handle| !handle.is_empty()),
        "and it carries the pending handle a publish unit answers with: {body}"
    );
}

/// **A lengthening `PATCH` answers 202 on one principal** — under a configured
/// threshold policy — and the stored end really moved.
///
/// An **extension**, and both halves of that are load-bearing. It is the adjust that
/// reaches the success arm on a key whose margin is known — the floor is evaluated
/// against the interval set the mutation produces, and an extension can only move the
/// coverage end later — **and** it is the side of D-62's split that is not on
/// `inst-mat-registered`'s trigger list, since `window::shortens` answers `false`.
///
/// **"Not on the trigger list" is not "commits on one principal", and this test used to
/// read as though it were.** What decides an extension is the threshold policy, so the
/// fixture configures one; its twin
/// [`an_adjusted_window_with_no_threshold_policy_opens_a_unit_and_the_end_stays_put`]
/// is the same call without it. `outcome` is asserted in both, because a lengthening
/// that summoned a reviewer under a configured policy would be a reviewer summoned for
/// an act that removes nothing.
#[tokio::test]
async fn an_adjusted_window_answers_202_and_the_stored_end_moves() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);

    let response = patch_window(&h, window_id, Some(at(30))).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(
        body,
        serde_json::json!({
            "outcome": "mutated",
            "materiality": null,
            "approval": null,
            "window": {
                "window_id": window_id,
                "plan_id": plan_id,
                "price_id": seeded.price_id,
                "revision": seeded.revision,
                "pending_version_ref": "pend-0",
                "effective_from": wire(common_from()),
                "effective_to": wire(at(30)),
                "state": "scheduled",
            },
        })
    );

    let conn = h.state.db.conn().expect("conn");
    let stored = bss_pricing::infra::storage::repo::window_repo::find(
        &conn,
        &h.scope(),
        h.tenant,
        window_id,
    )
    .await
    .expect("read the window")
    .expect("it is there");
    assert_eq!(
        stored.effective_to,
        Some(at(30)),
        "the store carries the new end, not merely the response"
    );
}

/// **A window this gear scheduled through its own route can then be adjusted
/// through it.**
///
/// The case no suite staged, and the reason it was not staged is the reason it
/// broke: every other window in this crate is the fixture's, scheduled straight
/// through `window_repo::schedule`, which requests **no** `CatalogVersion` and so
/// burns no registry handle. A window that arrived through `POST` has one, and the
/// second mutation of that window is the first call that has to ask for another.
///
/// `unit_request_id` names the registry handle by `(window, plan, revision)` — and a
/// window mutation deliberately does **not** advance the revision, so a `POST` and
/// every later `PATCH`/`DELETE` on that window rendered the *same* handle. The
/// registry is contractually idempotent on `request_id`, so it answered with the
/// pending ref the `POST` already recorded, and `record_pending`'s plain `INSERT`
/// met `PRIMARY KEY (tenant_id, pending_ref)` and came back a **500**.
///
/// So the assertion that matters is not merely the 202: it is that the second
/// mutation carries a **different** handle. Recording the same one twice is the
/// failure, and answering with the same one would mean this act's re-projection
/// rides a `CatalogVersion` that was assigned to a different act — a mutation that
/// commits locally and never becomes consumer-visible, which is worse than the 500
/// it replaced.
#[tokio::test]
async fn a_window_scheduled_through_the_route_can_then_be_adjusted_through_it() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    // Adjacent to the fixture's `[COVERAGE_FROM, COVERAGE_TO)`, so the key holds no
    // overlap and the schedule is decided by nothing but the rule under test.
    let scheduled = post_window(&h, seeded.price_id, common_to(), Some(at(0))).await;
    assert_eq!(
        scheduled.status(),
        StatusCode::ACCEPTED,
        "the successor lands"
    );
    let first = body_json(scheduled).await;
    assert_eq!(first["outcome"], "mutated", "and it was written: {first}");
    let window_id = first["window"]["window_id"]
        .as_str()
        .map(|s| Uuid::parse_str(s).expect("a uuid"))
        .expect("the schedule names the window it wrote");
    let first_handle = first["window"]["pending_version_ref"]
        .as_str()
        .expect("a schedule that committed carries its pending handle")
        .to_owned();

    // The second mutation of that same window, under the same plan revision. A
    // lengthening, so that under the fixture's configured policy it is not material
    // and takes the commit arm — which is the arm that asks the registry.
    let adjusted = patch_window(&h, window_id, Some(at(30))).await;

    let status = adjusted.status();
    let second = body_json(adjusted).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "the second mutation of a route-scheduled window is an ordinary act: {second}"
    );
    assert_eq!(second["outcome"], "mutated", "and it committed: {second}");
    assert_ne!(
        second["window"]["pending_version_ref"], first_handle,
        "this act needs a CatalogVersion of its own; re-using the schedule's would \
         re-project under a version assigned to a different act: {second}"
    );

    let conn = h.state.db.conn().expect("conn");
    let stored = bss_pricing::infra::storage::repo::window_repo::find(
        &conn,
        &h.scope(),
        h.tenant,
        window_id,
    )
    .await
    .expect("read the window")
    .expect("it is there");
    assert_eq!(
        stored.effective_to,
        Some(at(30)),
        "and the store carries the moved end, so the 202 was not an empty answer"
    );
}

/// **The twin: the same lengthening with no threshold policy opens a unit and the
/// stored end stays put.**
///
/// The half of D-62's split that had no test at all, because the act it is about
/// committed unconditionally. `inst-mat-failsafe` answers here, and the two assertions
/// are the ones that make it evidence: the reason is `noConfiguredThreshold` — an
/// extension is not on the trigger list, so borrowing `alwaysMaterialTrigger` would tell
/// an operator to look for an act that is not there — and the **stored** end is
/// unmoved, which is what says the 202 wrote nothing.
#[tokio::test]
async fn an_adjusted_window_with_no_threshold_policy_opens_a_unit_and_the_end_stays_put() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published_unconfigured(&h, plan_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);

    let response = patch_window(&h, window_id, Some(at(30))).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(body["outcome"], "submitted_for_approval");
    assert_eq!(
        body["materiality"],
        serde_json::json!({
            "material": true,
            "reason": "noConfiguredThreshold",
            "tripped": null,
            // A fail-safe is an answer about the policy, so it names no act.
            "trigger": null,
        }),
        "an extension is refused by the fail-safe, not by the trigger list: {body}"
    );
    assert_eq!(
        body["window"]["effective_to"],
        serde_json::json!(wire(common_to())),
        "the 202 echoes the end as it **still** stands, never the one that was asked for"
    );

    let conn = h.state.db.conn().expect("conn");
    let stored = bss_pricing::infra::storage::repo::window_repo::find(
        &conn,
        &h.scope(),
        h.tenant,
        window_id,
    )
    .await
    .expect("read the window")
    .expect("it is there");
    assert_eq!(
        stored.effective_to,
        Some(common_to()),
        "and the store did not move either"
    );
}

/// A cancellable window on a key whose coverage survives it.
///
/// The only staging a cancel can pass on: the key's coverage is **open-ended through
/// a second window**, so cancelling the first removes no coverage end at all.
/// Cancelling the sole window is `WINDOW_TRAILING_VOID`, which is a case of its own,
/// and the pair is why this staging is what it is rather than the simplest thing that
/// compiles.
async fn cancellable(h: &Harness, plan_id: Uuid) -> (rest_support::Publishable, Uuid) {
    let seeded = published(h, plan_id).await;
    let first = common::coverage_window_id(seeded.price_id);
    let scheduled = post_window(h, seeded.price_id, common_to(), None).await;
    assert_eq!(
        scheduled.status(),
        StatusCode::ACCEPTED,
        "the successor lands"
    );
    (seeded, first)
}

/// **A foreign caller moves no window and reads no sellability.**
///
/// The gap this closes: `other_tenant()` appeared nowhere in this file, so the
/// three mutating window routes and `GET /plans/{planId}/sellability` had no
/// cross-tenant probe at all. Every "denied" case here drives `denied()` - a
/// caller the PDP refuses outright - which exercises the gate and says nothing
/// about a caller who is legitimately authorized, in a tenant of their own,
/// reaching for this tenant's row.
///
/// The owner's own `DELETE` at the end is the positive control: without it the
/// three refusals above are satisfied by a route that answers 404 to everybody.
#[tokio::test]
async fn a_foreign_tenants_caller_moves_no_window_and_reads_no_sellability() {
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (seeded, first) = cancellable(&h, plan_id).await;
    let before = window_state(&h, first).await;
    // The owner's world carries a committed version for this plan; the foreign
    // caller's does not. Without this the two worlds are indistinguishable.
    project_and_pin(
        &h,
        plan_id,
        7,
        &delta_of(
            plan_id,
            vec![WindowInterval::new(at(-1), None, WindowState::Active)],
        ),
    )
    .await;

    let deleted = h
        .other_tenant()
        .send(request("DELETE", &window_path(first), None))
        .await;
    assert_eq!(
        deleted.status(),
        StatusCode::NOT_FOUND,
        "a window of another tenant is not a window this caller can cancel"
    );

    let patched = h
        .other_tenant()
        .send(with_headers(
            "PATCH",
            &window_path(first),
            Some(serde_json::json!({ "effective_to": wire(common_to()) })),
            &[("if-match", IF_MATCH)],
        ))
        .await;
    assert_eq!(patched.status(), StatusCode::NOT_FOUND);

    let posted = h
        .other_tenant()
        .send(with_headers(
            "POST",
            &windows_path(seeded.price_id),
            Some(serde_json::json!({ "effective_from": wire(common_to()) })),
            &[],
        ))
        .await;
    assert_eq!(
        posted.status(),
        StatusCode::NOT_FOUND,
        "and no window can be scheduled onto another tenant's row"
    );

    let foreign_read = h
        .other_tenant()
        .send(request(
            "GET",
            &sellability_path(plan_id, &market_query(0)),
            None,
        ))
        .await;
    // Not a 404: the gate's surface answers **the caller's own world**, and in the
    // foreign caller's world this plan's subject is carried by no committed catalog
    // version. So the read succeeds and predicate (2) is what refuses.
    //
    // **The owner's version has to exist first**, or this proves nothing. `publish`
    // projects no delta and `cancellable` pins no version, so without the
    // `project_and_pin` above the foreign caller's document is the one
    // `a_plan_no_version_carries_and_an_absent_plan_read_alike` already asserts for
    // `h.allowed()` - identical in all four facts - and a read that ignored the
    // caller's tenant would be green here. The owner's own read closes it.
    assert_eq!(foreign_read.status(), StatusCode::OK);
    let doc = body_json(foreign_read).await;
    assert_eq!(doc["verdict"], "not_sellable", "{doc}");
    assert_eq!(
        doc["catalog_version"],
        serde_json::Value::Null,
        "no pin resolves for a caller whose tenant carries none of this: {doc}"
    );
    assert_eq!(doc["keys"].as_array().map(Vec::len), Some(0), "{doc}");
    assert_eq!(
        answer_of(&doc, 2),
        "failed",
        "predicate (2) is the one that refuses, and it refuses rather than \
         being skipped: {doc}"
    );

    // The other route this suite owns and the one the coverage finding is about:
    // `GET /plans/{planId}/coverage`, driven by the same caller.
    let foreign_coverage = h
        .other_tenant()
        .send(request("GET", &coverage_path(plan_id), None))
        .await;
    assert_eq!(foreign_coverage.status(), StatusCode::OK);
    assert_eq!(
        body_json(foreign_coverage).await,
        serde_json::json!({ "plan_id": plan_id, "keys": [] }),
        "a foreign caller's coverage report over this plan is the one an absent plan gives"
    );
    assert!(
        !coverage(&h, plan_id).await["keys"]
            .as_array()
            .expect("the owner's report carries keys")
            .is_empty(),
        "while the owner's is not, so the empty report above is the caller's tenant \
         and not an empty plan"
    );

    let owners = sellability(&h, plan_id, &market_query(0)).await;
    assert_eq!(
        owners["catalog_version"],
        serde_json::json!(7),
        "the owner reads the version off its own frontier, so what refused above is \
         the caller's tenant and not an unprojected plan: {owners}"
    );

    assert_eq!(
        window_state(&h, first).await,
        before,
        "and nothing the foreign caller sent moved the window"
    );

    // The positive control.
    let own = delete_window(&h, first).await;
    assert_eq!(
        own.status(),
        StatusCode::ACCEPTED,
        "the owner's own cancel still reaches the door, so the refusals above are \
         about the caller and not about the route being dead"
    );
}

/// The approval unit a 202 opened, by id.
fn opened_unit(body: &serde_json::Value) -> Uuid {
    body.get("approval")
        .and_then(|a| a.get("approval_id"))
        .and_then(|v| v.as_str())
        .map_or_else(
            || panic!("the controlled arm names the unit it opened: {body}"),
            |s| Uuid::parse_str(s).expect("a uuid"),
        )
}

/// Approve `approval_id` as a principal who is not the submitter.
///
/// `delete_window` and `patch_window` call as `h.allowed()`, which mints a fresh
/// subject per call, so the submitter is never `APPROVER` — `inst-tp-distinct` is about
/// identity, and a helper that reused one principal could not stage the positive arm at
/// all.
async fn approve(h: &Harness, approval_id: Uuid) {
    let approved = h
        .allowed_as(APPROVER)
        .send(with_headers(
            "POST",
            &format!("/bss-pricing/v1/approvals/{approval_id}/approve"),
            None,
            &[],
        ))
        .await;
    assert_eq!(
        approved.status(),
        StatusCode::OK,
        "the approve must land for the mutation under test to be about the commit"
    );
}

/// **D-62's whole point: a `DELETE` answers 202 and the window is still
/// `scheduled`.**
///
/// The act D-62's Problem paragraph exists to stop — one `plan × write` holder
/// cancelling the two-person-approved scheduled successor — and until this test the
/// gear performed it on one principal: the caller got 202 with `window_state` already
/// `cancelled`, no `pricing_approval` row, no second principal, no materiality
/// evaluated.
///
/// **Nothing at all was written**, which is asserted on every store a mutation
/// touches rather than on the window row alone: the plane, the pending-ref table (a
/// handle requested here would be an assignment nothing ever commits), the outbox and
/// the audit log. A refusal-to-mutate that left one of those behind would be the
/// interim state `infra::window`'s module doc says this transaction never has.
///
/// **The statement only this guard can refuse.** Every neighbour is silent on this
/// input by construction: the state check passes (the window is `scheduled`), the
/// interior check passes (the key's remaining interval set is contiguous), the
/// trailing floor passes (the successor is open-ended, so cancelling the first removes
/// no coverage end), and the register passes (no unit is pending). Remove the D-62
/// gate and this is the only assertion that changes — the window flips on the first
/// call.
#[tokio::test]
async fn a_cancel_opens_a_unit_and_the_window_is_still_scheduled() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (_seeded, first) = cancellable(&h, plan_id).await;
    let before_refs = rest_support::pending_version_refs(&h).await.len();
    let before_outbox = rest_support::outbox_correlations(&h).await.len();
    let before_audit = rest_support::audit_rows(&h).await.len();

    let response = delete_window(&h, first).await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(
        body["outcome"], "submitted_for_approval",
        "the controlled arm, not the one that mutates: {body}"
    );
    assert_eq!(
        body["window"]["state"], "scheduled",
        "the answer describes the window as the caller will still find it: {body}"
    );
    assert_eq!(
        body["window"]["pending_version_ref"],
        serde_json::Value::Null,
        "no version was requested, because nothing was written: {body}"
    );
    assert_eq!(
        body["materiality"],
        serde_json::json!({
            "material": true,
            "reason": "alwaysMaterialTrigger",
            "tripped": null,
            // §6's trigger source: the record names D-62's act, not merely that
            // some registered act fired.
            "trigger": "windowCancellation",
        }),
        "D-62's own verdict, not the evaluator's three G1 arms: {body}"
    );

    // The unit is real, pinned to this window, and pending.
    let unit = rest_support::approval_row(&h, opened_unit(&body)).await;
    assert_eq!(unit.subject_kind, AuditSubjectKind::Window);
    assert!(
        unit.subject_ref
            .starts_with(&bss_pricing::infra::storage::repo::audit_repo::window_ref(
                PlanId::new(plan_id),
                first
            )),
        "the unit is pinned to this window, and the ref carries the plan: a controlled act writes \
         no window row, so the aggregate has to be readable off the ref itself. The tail beyond \
         the window is the **act**, which is what keeps this approval from answering for a \
         different act on the same window: {}",
        unit.subject_ref
    );
    assert!(
        unit.subject_ref.ends_with(&format!(
            "/cancel/0/{bound}/{bound}",
            bound = format_rfc3339(common_to())
        )),
        "and the act is named in full - the operation, the **act sequence the window \
         was read at** (D-190) and the transition it makes. The sequence is what tells \
         a repeated oscillation from its own first occurrence, which the transition \
         alone cannot: {}",
        unit.subject_ref
    );
    assert_eq!(unit.state, ApprovalState::Submitted);

    // And the four stores a mutation touches are exactly as it found them.
    assert_eq!(
        window_state(&h, first).await.as_deref(),
        Some("scheduled"),
        "the window a two-person control is pending over must not have moved"
    );
    assert_eq!(
        rest_support::pending_version_refs(&h).await.len(),
        before_refs,
        "no pending version ref: a handle here would name an assignment nothing commits"
    );
    assert_eq!(
        rest_support::outbox_correlations(&h).await.len(),
        before_outbox,
        "no event, because nothing happened to publish"
    );
    assert_eq!(
        rest_support::audit_rows(&h).await.len(),
        before_audit + 1,
        "exactly one record: the submit's own, and no `publish` record of a mutation \
         that did not run"
    );
}

/// **And the window flips only after an independent approve** — the other half, and
/// the one that makes the first half a control rather than a refusal.
///
/// The commit arm: the same `DELETE`, re-issued after a second principal has approved
/// the unit, answers `outcome = mutated`, flips the row to `cancelled` and carries the
/// pending handle a publish unit answers with. It is a **state flip, never a
/// deletion** — the row is still there, because a cancelled schedule is a fact an
/// auditor is entitled to see — and its audit record names the unit that authorized
/// it, which is what an auditor reads to find the second principal.
///
/// **The statement only the commit half can refuse.** Make `authorizing_unit` answer
/// `None` and this reddens alone: the first call still opens its unit, every neighbour
/// still passes, and the only thing that changes is that no re-issue ever commits —
/// the window is permanently uncancellable through the surface that owns it.
#[tokio::test]
async fn a_cancel_commits_after_an_independent_approve_and_is_a_state_flip() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (_seeded, first) = cancellable(&h, plan_id).await;

    let opened = body_json(delete_window(&h, first).await).await;
    let approval_id = opened_unit(&opened);
    approve(&h, approval_id).await;

    let response = delete_window(&h, first).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = body_json(response).await;
    assert_eq!(
        body["outcome"], "mutated",
        "the second call commits under the approved unit: {body}"
    );
    assert_eq!(body["window"]["state"], "cancelled");
    assert_eq!(body["window"]["window_id"], first.to_string());
    assert!(
        body["window"]["pending_version_ref"]
            .as_str()
            .is_some_and(|v| v.starts_with("pend-")),
        "a cancel that ran is a publish unit and carries a handle: {body}"
    );

    assert_eq!(
        window_state(&h, first).await.as_deref(),
        Some("cancelled"),
        "a state flip: the row is still there, because a cancelled schedule is a \
         fact an auditor is entitled to see"
    );

    // The trail names the unit, which is what an auditor follows to the second
    // principal. Asserted off the record rather than off the response, because the
    // response is written by the same handler.
    let publishes: Vec<_> = rest_support::audit_rows(&h)
        .await
        .into_iter()
        .filter(|row| {
            row.subject_ref
                == bss_pricing::infra::storage::repo::audit_repo::window_ref(
                    PlanId::new(plan_id),
                    first,
                )
        })
        .filter(|row| row.action == "publish")
        .collect();
    assert_eq!(publishes.len(), 1, "one record for one mutation");
    assert_eq!(
        publishes[0].approval_ref,
        Some(approval_id),
        "the record names the unit that authorized the cancel"
    );
}

/// **An approved unit authorizes the act it was opened for, and no other act on
/// the same window.**
///
/// D-62's content is that a second principal reviews *the act*. What the unit
/// stored was `(window_ref, content_hash(plan shape))` — the world **before** the
/// mutation — and `authorizing_unit` looked it back up by exactly that pair. Since
/// no act has committed between the submit and the retry, the shape digest is
/// identical for every act on that window, so an approval taken for one act
/// answered for any other: a reviewer signed a lengthening and the submitter
/// cancelled the window under it. The reviewer could not have caught it either —
/// `re_derive`'s `Window` arm renders the plan as it already stands and names no
/// operation.
///
/// The staging is forced and worth reading, because only two acts reach D-62's
/// gate at all. Every **shortening** is refused by the coverage rules first
/// ([`a_shortening_patch_is_refused_by_the_coverage_rules_before_it_can_open_a_unit`]),
/// so the pair has to be a **lengthening** and a **cancel** — and those want
/// opposite things from the key. A lengthening needs room after the window; a
/// cancel needs a successor that keeps the key covered. So the successor is placed
/// far enough ahead to leave room for both, and the tenant is left **unconfigured**
/// so that the lengthening is material at all (`inst-mat-failsafe`) — a lengthening
/// is not on `inst-mat-registered`'s list, so under a configured policy it would
/// commit on one principal and open no unit to misuse.
///
/// The assertion is that the cancel opens a unit of its **own**. Asserting only
/// that the window is still `scheduled` would pass on a build where the cancel was
/// refused outright, which is a different answer from the one D-62 asks for.
#[tokio::test]
async fn an_approved_unit_does_not_authorize_a_different_act_on_the_same_window() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published_unconfigured(&h, plan_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);
    // The successor that keeps the key covered once the fixture window is cancelled,
    // written through the repository so that staging it opens no unit of its own.
    // Far enough ahead that the lengthening below still fits before it.
    window(&h, seeded.price_id, 0x_b2, 30, None).await;

    // Act one: a lengthening. Material here only because the tenant has no policy.
    // Exactly onto the successor's start: the rules evaluate the interval set the
    // mutation *produces*, and any daylight between the two would be a `WINDOW_GAP`
    // refusal before the gate this case is about.
    let lengthened = patch_window(&h, window_id, Some(at(30))).await;
    let status = lengthened.status();
    let lengthened = body_json(lengthened).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{lengthened}");
    assert_eq!(
        lengthened["outcome"], "submitted_for_approval",
        "the lengthening is material and writes nothing: {lengthened}"
    );
    let lengthening_unit = opened_unit(&lengthened);

    // An independent principal signs *that* act.
    approve(&h, lengthening_unit).await;

    // Act two: a cancel of the same window, which nobody reviewed.
    let cancelled = delete_window(&h, window_id).await;
    assert_eq!(cancelled.status(), StatusCode::ACCEPTED);
    let cancelled = body_json(cancelled).await;

    assert_eq!(
        cancelled["outcome"], "submitted_for_approval",
        "a cancel is not what the reviewer approved, so it must ask for itself: {cancelled}"
    );
    assert_ne!(
        opened_unit(&cancelled),
        lengthening_unit,
        "and it must be a unit of its own, not the lengthening's"
    );
    assert_eq!(
        window_state(&h, window_id).await.as_deref(),
        Some("scheduled"),
        "the window is untouched: an unreviewed cancel froze nothing"
    );
}

/// **A shortening `PATCH` never reaches D-62's gate today, because the coverage rules
/// refuse it first** — and that shadowing is a fact about the rule set, pinned so the
/// group that changes it looks at the gate.
///
/// Named for the pair rather than for the floor alone, which is what the body checks:
/// the staging here is the friendliest shorten there is and the **interior** check is
/// what answers it. The floor answers the other half of the space, and
/// [`a_shortening_below_the_floor_is_refused_as_a_trailing_void`] carries that one with
/// the same zero-units assertion.
///
/// `inst-mat-registered` names two always-material acts and this gear now controls
/// both, but only one of them is *reachable*. Every shorten a caller can compose is
/// refused before the control is consulted, and the two refusals between them cover
/// the whole space:
///
/// * shorten the key's **last** interval and the coverage end moves earlier than
///   `max(current coverage end, now + the longest cycle sold)` — `WINDOW_TRAILING_VOID`,
///   because that floor is `>= the current coverage end` by construction (D-182,
///   fail-closed with no exemption);
/// * shorten an interval that has a **successor** and the hole between them is an
///   interior gap — `WINDOW_GAP`.
///
/// So the classification in `Op::is_always_material` answers `true` for a shorten and
/// nothing downstream ever asks: the guard is shadowed rather than absent, which is
/// the distinction the phase's discipline is about. It is not dead code either — the
/// arm is one branch of a function the transaction evaluates on every adjust, and the
/// day the D-79 lane lands D-182's exemption, or a successor-first sequence makes a
/// shorten legal, this test reddens and the gate is what answers instead.
///
/// **Zero approval units** is the load-bearing assertion: it is what says the refusal
/// came from the floor rather than from the control, on an input the control would
/// otherwise have answered.
#[tokio::test]
async fn a_shortening_patch_is_refused_by_the_coverage_rules_before_it_can_open_a_unit() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let first = common::coverage_window_id(seeded.price_id);

    // The key's coverage runs to the successor's open end, so this is the friendliest
    // shorten there is: it removes no coverage from the key at all.
    let scheduled = post_window(&h, seeded.price_id, common_to(), None).await;
    assert_eq!(scheduled.status(), StatusCode::ACCEPTED);

    let response = patch_window(&h, first, Some(common_to() - time::Duration::days(10))).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let codes: Vec<String> = precondition_violations(response)
        .await
        .into_iter()
        .map(|(code, _)| code)
        .collect();
    assert_eq!(
        codes,
        vec!["WINDOW_GAP".to_owned()],
        "the interior check answers this one: the shorten opens a hole before the successor"
    );
    assert!(
        units_of(&h, AuditSubjectKind::Window).await.is_empty(),
        "and no unit was opened, so the refusal is the rule set's and not the control's"
    );
}

// ---------------------------------------------------------------------------
// WINDOW_TRAILING_VOID — D-182, on both surfaces it names
// ---------------------------------------------------------------------------

/// **A cancel that would leave the key uncovered is `WINDOW_TRAILING_VOID`** (400),
/// naming `effective_to` and the floor.
///
/// D-182's refusal on the first of the two surfaces `inst-fg-trailing` names. The
/// window cancelled here is the key's **only** coverage, so the cancel leaves a void
/// with no successor — the case `inst-fg-detect` cannot see by construction, which
/// is `inst-fg-trailing`'s whole reason for existing.
///
/// **There is no exemption path to stage**, and that is D-182: the D-79 lane that
/// would decide one has no client, no contract type and no counterpart gear here, so
/// this refusal fires for every key including the ones the design set exempts.
#[tokio::test]
async fn a_cancel_that_would_empty_the_key_is_refused_as_a_trailing_void() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let only = common::coverage_window_id(seeded.price_id);

    let response = delete_window(&h, only).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        precondition_violations(response).await,
        vec![("WINDOW_TRAILING_VOID".to_owned(), "effective_to".to_owned())],
        "one violation, its code by equality, and the field the operator can move"
    );

    // Nothing moved: a refusal is not a mutation that then rolled back visibly.
    assert_eq!(
        window_state(&h, only).await.as_deref(),
        Some("scheduled"),
        "the window the refusal was about is untouched"
    );
    // And D-62's gate never ran: the floor answers before the control is consulted,
    // which is the ordering `a_shortening_patch_is_refused_by_the_coverage_rules_before_it_can_open_a_unit`
    // pins from the interior check's side.
    assert!(
        units_of(&h, AuditSubjectKind::Window).await.is_empty(),
        "a mutation that cannot commit must not be put in front of a reviewer"
    );
}

/// **An `effectiveTo` shortening below the floor is `WINDOW_TRAILING_VOID`** — the
/// second surface, and the one that makes this a rule about coverage rather than a
/// rule about the `DELETE` verb.
///
/// **This is the statement only the trailing rule can refuse**, and it is D-182's
/// own owed proof. After the shorten the key holds exactly **one** live interval, so
/// there is no pair for `inst-fg-detect` to compare and no interior hole to find:
/// the interior check is silent on this input by construction. What is wrong with it
/// is that the coverage *ends earlier than the floor* — `max(current coverage end,
/// now + the longest billing cycle sold on the key)` — and nothing but
/// `inst-fg-trailing` evaluates that.
#[tokio::test]
async fn a_shortening_below_the_floor_is_refused_as_a_trailing_void() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);

    // Ten days inside the stored end, which is below the floor because the stored
    // end *is* the floor here: `now + one month` is 2026 and the coverage end is
    // 2099, so `max(..)` is the coverage end.
    let target = common_to() - time::Duration::days(10);
    let response = patch_window(&h, window_id, Some(target)).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    let violations = body
        .get("context")
        .and_then(|c| c.get("violations"))
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("a 400 carries violations: {body}"));
    assert_eq!(
        violations
            .iter()
            .map(|v| (
                v.get("type").and_then(|t| t.as_str()).unwrap_or_default(),
                v.get("subject")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
            ))
            .collect::<Vec<_>>(),
        vec![("WINDOW_TRAILING_VOID", "effective_to")]
    );
    // The floor is **named**, which is what an operator acts on: the detail says
    // through which instant the key must stay covered.
    let detail = format!("{body}");
    assert!(
        detail.contains(&format_rfc3339(common_to())),
        "the refusal names the instant the key must stay covered through: {detail}"
    );

    let conn = h.state.db.conn().expect("conn");
    assert_eq!(
        bss_pricing::infra::storage::repo::window_repo::find(
            &conn,
            &h.scope(),
            h.tenant,
            window_id
        )
        .await
        .expect("read")
        .expect("there")
        .effective_to,
        Some(common_to()),
        "the stored end did not move"
    );
}

/// **The accepted side of the same comparison**: an end restated *at* the floor is
/// not a trailing void.
///
/// `refuse_trailing_void` compares `at >= until` (`infra/window.rs`), and no case in
/// this file exercised the boundary until 2026-08-20 — the sibling above shortens ten
/// days inside the floor, the lengthening cases end strictly past it, and the cancel
/// cases resolve through the `OpenEnded` arm. So flipping `>=` to `>` left every case
/// in the suite green while a caller who merely **restated a window's current end**
/// would be refused `WINDOW_TRAILING_VOID` — a refusal with no remedy, since the
/// instant it demands is the instant the request already names.
///
/// Same fixture, same window, same verb as the refusal above: the only variable is
/// ten days.
#[tokio::test]
async fn an_end_restated_at_the_floor_is_accepted() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);

    let response = patch_window(&h, window_id, Some(common_to())).await;
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "`at == until` clears the floor: the key stays covered through exactly the \
         instant the floor names, so there is no void to refuse: {body}"
    );
    assert!(
        !format!("{body}").contains("WINDOW_TRAILING_VOID"),
        "and it is not the trailing rule that answered: {body}"
    );
}

// ---------------------------------------------------------------------------
// The interior gap, kept distinguishable from the trailing void
// ---------------------------------------------------------------------------

/// **A schedule that opens an interior gap is `WINDOW_GAP`** (400), naming
/// `[gapStart, gapEnd)`.
///
/// **The statement only the interior rule can refuse**, and the other half of
/// D-182's owed distinguishing proof. The two rules overlap on most of what each
/// refuses; these two tests are the pair that separates them, and the separation is
/// in the *operation* as much as in the shape:
///
/// * a **schedule** adds coverage, so `Op::may_remove_coverage` is `false` and the
///   trailing floor is **never evaluated** on this path at all. The trailing rule
///   cannot refuse this input even in principle.
/// * the sibling case above shortens the key's **sole** interval, so there is no
///   pair for the interior check to compare. The interior rule cannot refuse that
///   one even in principle.
///
/// So removing either guard reddens its own case and leaves the other green, which
/// is what "distinguishable" has to mean here.
#[tokio::test]
async fn a_schedule_that_opens_an_interior_gap_is_refused_as_a_window_gap() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    // Ten days after the seed's window ends: a hole between two live windows.
    let gap_start = common_to();
    let gap_end = common_to() + time::Duration::days(10);
    let response = post_window(&h, seeded.price_id, gap_end, None).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    let violations = body
        .get("context")
        .and_then(|c| c.get("violations"))
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("a 400 carries violations: {body}"));
    assert_eq!(
        violations
            .iter()
            .map(|v| v.get("type").and_then(|t| t.as_str()).unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["WINDOW_GAP"],
        "the interior code, by equality, and exactly one of it"
    );
    // The subject is the **key**, not a field: a gap is a fact about a key's
    // interval set, and `WINDOW_GAP` is the same constant the publish rule raises
    // so an operator reads one sentence about one fault whichever surface refused.
    let key =
        rest_support::publishable_scope_key(PlanId::new(plan_id), seeded.phase, "eu").to_string();
    assert_eq!(
        violations[0].get("subject").and_then(|s| s.as_str()),
        Some(key.as_str())
    );
    let rendered = format!("{body}");
    assert!(
        rendered.contains(&format_rfc3339(gap_start))
            && rendered.contains(&format_rfc3339(gap_end)),
        "the hole is named `[gapStart, gapEnd)`: {rendered}"
    );

    // And nothing landed: the plan still holds exactly the seed's one window.
    let conn = h.state.db.conn().expect("conn");
    assert_eq!(
        bss_pricing::infra::storage::repo::window_repo::list_for_plan(
            &conn,
            &h.scope(),
            h.tenant,
            PlanId::new(plan_id)
        )
        .await
        .expect("read the plane")
        .len(),
        1,
        "the refused schedule wrote no row"
    );
}

// ---------------------------------------------------------------------------
// WINDOW_NOT_CANCELLABLE and WINDOW_START_IN_PAST — until now unreachable
// ---------------------------------------------------------------------------

/// **A second `DELETE` of one window is `WINDOW_NOT_CANCELLABLE`** (409).
///
/// §4 gives a window one edge out of `scheduled`, so a retried cancel is refused
/// rather than replayed — which is what `windows.rs` reports as standing in for the
/// idempotency header §5 does not give this verb. Before the routes were mounted
/// `window::check_cancellation` had **no non-test caller**, so this is the case that
/// makes the code a reachable fact.
///
/// **No neighbour can refuse this input.** The trailing floor is silent: the
/// cancelled window contributes no coverage either before or after, so the interval
/// set the mutation produces is the set it started from and the floor is satisfied
/// identically. The register is silent — the unit that authorized the first cancel is
/// `approved`, and only a `submitted` unit holds a key. D-62's gate is silent for the
/// same reason: that approved unit still covers this window and this content, so a
/// third call would find its authorization rather than open a second unit. And the
/// state check runs inside the operation's own closure, ahead of all three.
///
/// The staging is therefore the **whole** D-62 sequence and not a single `DELETE`: the
/// first call opens the unit, an independent principal approves it, the second commits
/// the flip, and it is the *third* that this test is about.
#[tokio::test]
async fn a_second_cancel_of_one_window_is_refused_as_not_cancellable() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (_seeded, first) = cancellable(&h, plan_id).await;

    let opened = body_json(delete_window(&h, first).await).await;
    approve(&h, opened_unit(&opened)).await;
    assert_eq!(
        delete_window(&h, first).await.status(),
        StatusCode::ACCEPTED,
        "the cancel lands on the call after the approve"
    );
    assert_eq!(
        window_state(&h, first).await.as_deref(),
        Some("cancelled"),
        "and the premise of the case below is that it really flipped"
    );

    let response = delete_window(&h, first).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        conflict_reason(response).await,
        "WINDOW_NOT_CANCELLABLE",
        "the code by equality off context.reason"
    );
    assert_eq!(window_state(&h, first).await.as_deref(), Some("cancelled"));
}

/// **A `POST` whose `effectiveFrom` is not strictly future is
/// `WINDOW_START_IN_PAST`** (400), naming `effective_from`.
///
/// `inst-ws-future-start` (D-63). The other half of the debt the mounted routes paid:
/// `window::check_creation` had no non-test caller either, so the code was declared
/// and unreachable.
///
/// **No neighbour can refuse this input.** The instant is 2020, so it overlaps no
/// sibling (every seeded interval is 2099) and the guard runs *before* the store, so
/// the trigger's own past-start refusal — which would arrive as a 500 about a
/// constraint the caller never named — is not what answers. A schedule evaluates no
/// trailing floor, and an interval before every other one opens no interior gap on
/// its own side.
#[tokio::test]
async fn a_window_starting_in_the_past_is_refused_before_the_store_sees_it() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    let past = utc_ymd_hms(2020, 1, 1, 0, 0, 0);
    let response = post_window(&h, seeded.price_id, past, Some(at(5))).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        precondition_violations(response).await,
        vec![(
            "WINDOW_START_IN_PAST".to_owned(),
            "effective_from".to_owned()
        )],
        "the code by equality, and the field it is about"
    );

    let conn = h.state.db.conn().expect("conn");
    assert_eq!(
        bss_pricing::infra::storage::repo::window_repo::list_for_plan(
            &conn,
            &h.scope(),
            h.tenant,
            PlanId::new(plan_id)
        )
        .await
        .expect("read the plane")
        .len(),
        1,
        "nothing was written, and no trigger was reached to write it"
    );
    let _ = seeded;
}

// ---------------------------------------------------------------------------
// The `None` margin: unknown is not zero
// ---------------------------------------------------------------------------

/// A published plan that **sells recurring on the key and authors no frequency**,
/// so W6's margin has *no value* rather than the value zero.
///
/// `seed_draft_plan` authors no frequency and `seed_price` creates a `recurring`
/// row, which is exactly the pair `coverage::longest_cycle_sold` answers `None` for:
/// it answers `Some(zero)` when the market sells **no** recurring row, and `None`
/// when it sells one and the plan has no frequency to measure. The plan is published
/// through the repository, which is the only way to publish one the rule set would
/// refuse — and refusing it is not this case's subject.
async fn margin_with_no_value(h: &Harness, plan_id: Uuid) -> (Uuid, Uuid) {
    seed_draft_plan(h, plan_id).await;
    h.attach_shape(plan_id, 0).await;
    let row = seed_price(h, plan_id, "EU").await;
    let window_id = rest_support::seed_window(h, row.price_id).await;
    h.publish(plan_id, 0).await;
    h.publish_price(plan_id, row.price_id).await;
    (row.price_id, window_id)
}

/// **The no-value margin answers a different *sentence* from the floor**, and the
/// sentence is what this checks.
///
/// Named for what it measures rather than for what it looks like it measures. The
/// **refusal** here is shadowed: the key's coverage is open-ended, so bounding it is
/// refused by the floor's open-ended arm whatever the margin is — folding `None`
/// into `Some(zero)` still answers 400 with `WINDOW_TRAILING_VOID`, and only the
/// detail changes. Found by proving the guard by removal: this case reddened under
/// that fold **on the message**, which is the shadowed-guard pattern the discipline
/// names, so the name and this doc were corrected rather than the red being taken as
/// evidence about the arm.
///
/// It is kept because the sentence is not decoration. D-146's line is that two
/// refusals sharing a code must still ask the operator for different things, and
/// these do: *author a frequency* is not *move an end*, and an operator handed the
/// floor's sentence on a plan that has no cycle at all would go looking for an
/// instant to change. The case that actually separates `None` from `Some(zero)` is
/// [`an_unmeasurable_margin_refuses_even_a_no_op_end`], and it is the only staging
/// that can: on a key whose coverage is open-ended nothing but an open-ended result
/// clears the floor, and on a bounded key every shorten falls below it — so no
/// *shorten* anywhere distinguishes the two readings.
#[tokio::test]
async fn the_no_value_margin_answers_a_different_sentence_from_the_floor() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (_price_id, window_id) = margin_with_no_value(&h, plan_id).await;

    let response = patch_window(&h, window_id, Some(at(40))).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(
        body.get("context")
            .and_then(|c| c.get("violations"))
            .and_then(|v| v.as_array())
            .map(|v| v
                .iter()
                .map(|e| e.get("type").and_then(|t| t.as_str()).unwrap_or_default())
                .collect::<Vec<_>>()),
        Some(vec!["WINDOW_TRAILING_VOID"]),
        "the declared code, and no second one minted for the absent term: {body}"
    );
    let rendered = format!("{body}");
    assert!(
        rendered.contains("authors no frequency") && rendered.contains("has no value"),
        "the sentence must say the term has no value, not that a floor was crossed: \
         {rendered}"
    );
}

/// **An unknown margin refuses even an `effectiveTo` the key already has** — which
/// is the assertion that separates `None` from `Some(zero)`.
///
/// Under a margin of **zero** this request passes: the floor is
/// `max(current coverage end, now + 0)`, the stored end already equals the current
/// coverage end, so nothing is below anything. Under **no margin at all** it is
/// refused, because a caller enforcing a floor with no value must refuse rather than
/// read the absence as nought — the direction a fail-closed floor may never round
/// in.
///
/// So folding `None` into `Some(time::Duration::ZERO)` in `longest_cycle_sold` turns
/// this 400 into a **202** — measured, not argued — while the sibling above keeps
/// answering 400 and moves only its detail. **This is the one case in the file whose
/// reddening is about the two readings** rather than about a message, which is why
/// the sibling's doc points here.
///
/// A consequence worth naming rather than hiding: on such a key the fail-closed arm
/// refuses a mutation that removes no coverage at all. That is
/// `Op::may_remove_coverage`'s stated scope — an adjust is included whichever
/// direction it moves — meeting the `None` arm, and it is safe in direction.
#[tokio::test]
async fn an_unmeasurable_margin_refuses_even_a_no_op_end() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let (_price_id, window_id) = margin_with_no_value(&h, plan_id).await;

    // The end the window already carries: `seed_window` leaves it open-ended, so
    // the request restates exactly that and removes nothing.
    let response = patch_window(&h, window_id, None).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an unknown margin refuses; a zero margin would have answered 202"
    );
    let rendered = format!("{}", body_json(response).await);
    assert!(
        rendered.contains("WINDOW_TRAILING_VOID") && rendered.contains("has no value"),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------
// `GET /plans/{planId}/sellability` — the gate's surface
// ---------------------------------------------------------------------------
//
// The delta is projected **directly** rather than driven through a publish. This
// suite is about the read surface, and the pairing between the projector's renderer
// and the reader is asserted where both live, in
// `read_model_repo_tests::every_field_the_reader_reads_round_trips_through_the_payload`.
// Reaching the projector from here would put the warm job's scheduling between the
// facts and the assertion.

/// A market every case below shares, spelled once so a query string and a scope key
/// cannot disagree about it.
const MARKET_CURRENCY: &str = "EUR";
const MARKET_REGION: &str = "eu";

fn sellability_path(plan_id: Uuid, query: &str) -> String {
    format!(
        "{}?{query}",
        bss_pricing::api::rest::windows::PLAN_SELLABILITY.replace("{planId}", &plan_id.to_string())
    )
}

/// The three parameters §5 names, all present and well-formed.
///
/// `at` is rendered with the **`Z`** designator rather than through `to_rfc3339`,
/// whose `+00:00` offset form is not safe in a query string: `+` decodes to a space
/// under form-urlencoding, so the value reaching the handler would be
/// `2099-10-01T00:00:00 00:00` — not an instant. Found by writing this helper the
/// obvious way and watching four cases answer 400. The route's own doc carries the
/// hazard for a client author, and
/// `an_unencoded_offset_instant_is_refused_rather_than_guessed_at` pins it.
fn market_query(at_day: i64) -> String {
    format!(
        "at={}&currency={MARKET_CURRENCY}&region={MARKET_REGION}",
        format_rfc3339(at(at_day))
    )
}

fn sellability_key(
    plan_id: Uuid,
    charge_kind: bss_pricing::domain::scope_key::ChargeKind,
) -> bss_pricing::domain::scope_key::ScopeKey {
    use bss_pricing::domain::money::CurrencyCode;
    use bss_pricing::domain::scope_key::{Cohort, PriceEligibility, Region, ScopeKey};
    ScopeKey::new(
        PlanId::new(plan_id),
        CurrencyCode::new(MARKET_CURRENCY).expect("three letters"),
        Region::new(MARKET_REGION).expect("a non-blank region"),
        rest_support::seeded_phase(),
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

/// A published, monthly plan whose one recurring key carries `intervals`.
fn delta_of(
    plan_id: Uuid,
    intervals: Vec<bss_pricing::domain::window::WindowInterval>,
) -> bss_pricing::domain::projection::PlanSubjectDelta {
    use bss_pricing::domain::concurrency::RowVersion;
    use bss_pricing::domain::lifecycle::LifecycleState;
    use bss_pricing::domain::money::MinorAmount;
    use bss_pricing::domain::plan_shape::Frequency;
    use bss_pricing::domain::price_record::PriceRecord;
    use bss_pricing::domain::price_row::{ModelKind, PriceRow};
    use bss_pricing::domain::projection::PlanSubjectDelta;
    use bss_pricing::domain::scope_key::ChargeKind;
    use bss_pricing::domain::window::KeyWindows;

    let key = sellability_key(plan_id, ChargeKind::Recurring);
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(1_200).expect("a non-negative amount"));
    PlanSubjectDelta {
        entitlement_grants: EntitlementGrants::default(),
        // Empty: no predicate this fixture drives reads a derived meter, and a
        // populated set here would be a seed no assertion reads back.
        composites: Vec::new(),
        change_contract: PlanChangeContract::default(),
        plan_id: PlanId::new(plan_id),
        revision: 0,
        lifecycle_state: LifecycleState::Published,
        sku_id: None,
        plan_tier: Some("gold".to_owned()),
        plan_tier_override: false,
        billing_cycle: None,
        frequency: Some(Frequency::Monthly),
        available_from: Some(at(-10)),
        available_to: None,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: None,
        phases: Vec::new(),
        addon_rules: Vec::new(),
        descriptor_set: None,
        period_floor_caps: Vec::new(),
        prices: vec![PriceRecord {
            price_id: Uuid::from_u128(0xb_0001),
            scope_key: key.clone(),
            row,
            tax_inclusive: false,
            tax_category_ref: None,
            billing_timing: None,
            proration_contract: None,
            rounding_policy_ref: None,
            grandfather_until: None,
            supersedes_price_id: None,
            lifecycle_state: LifecycleState::Published,
            created_by: SUBMITTER,
            created_at_utc: at(-10),
            row_version: RowVersion::new(0),
        }],
        tax_projection: BTreeMap::new(),
        windows: vec![KeyWindows {
            scope_key: key,
            intervals,
        }],
    }
}

/// Freeze one plan-subject delta at `version` and make that version pin-eligible.
async fn project_and_pin(
    h: &Harness,
    plan_id: Uuid,
    version: u64,
    delta: &bss_pricing::domain::projection::PlanSubjectDelta,
) {
    use bss_pricing::domain::read_model::SubjectRef;
    use bss_pricing::infra::storage::repo::{NewDelta, pin_frontier_repo, read_model_repo};
    use bss_pricing_sdk::CatalogVersion;

    let conn = h.db.conn().expect("conn");
    read_model_repo::project_subject(
        &conn,
        &h.scope(),
        NewDelta {
            tenant_id: h.tenant,
            catalog_version: CatalogVersion::new(version),
            subject: SubjectRef::Plan(plan_id),
            payload: delta.to_value(),
            projected_at: at(0),
        },
    )
    .await
    .expect("project the plan subject");
    pin_frontier_repo::advance(
        &conn,
        &h.scope(),
        h.tenant,
        CatalogVersion::new(version),
        at(0),
    )
    .await
    .expect("make the version pin-eligible");
}

async fn sellability(h: &Harness, plan_id: Uuid, query: &str) -> serde_json::Value {
    let response = h
        .allowed()
        .send(request("GET", &sellability_path(plan_id, query), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

/// The answer a numbered predicate gave, plan-level or per key.
fn predicate(document: &serde_json::Value, ordinal: u64) -> &serde_json::Value {
    let plan_level = document
        .get("predicates")
        .and_then(|p| p.as_array())
        .expect("the document carries the plan-level predicates");
    let per_key = document
        .get("keys")
        .and_then(|k| k.as_array())
        .expect("the document carries a key list")
        .iter()
        .filter_map(|key| key.get("predicates").and_then(|p| p.as_array()))
        .flatten();
    plan_level
        .iter()
        .chain(per_key)
        .find(|entry| entry.get("ordinal").and_then(serde_json::Value::as_u64) == Some(ordinal))
        .unwrap_or_else(|| panic!("no answer for predicate ({ordinal}) in {document}"))
}

fn answer_of(document: &serde_json::Value, ordinal: u64) -> &str {
    predicate(document, ordinal)
        .get("answer")
        .and_then(|a| a.as_str())
        .expect("every predicate carries a token")
}

/// **The property this surface exists for, and the one it must never break.**
///
/// The payload carries per-key intervals, their states and a derived coverage end,
/// and **no boolean anywhere in the document under any spelling**. A boolean would
/// freeze into an INSERT-only answer to a question about the reader's clock, and it
/// would make every activation and expiry owe a re-projection of a store whose
/// contract is that a completed version never changes (D-99).
///
/// The sweep is over the **whole** rendered document rather than over the field a
/// reader expects one in: `the_payload_carries_intervals_and_a_coverage_end_and_no_active_flag`
/// is the precedent, and the failure it guards against is a boolean arriving
/// somewhere nobody thought to look.
#[tokio::test]
async fn the_document_carries_intervals_and_a_coverage_end_and_no_boolean_anywhere() {
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    project_and_pin(
        &h,
        plan_id,
        4,
        &delta_of(
            plan_id,
            vec![
                WindowInterval::new(at(-10), Some(at(-1)), WindowState::Expired),
                WindowInterval::new(at(-1), None, WindowState::Active),
            ],
        ),
    )
    .await;

    let document = sellability(&h, plan_id, &market_query(0)).await;

    // The intervals, states included, exactly as the version froze them.
    let key = &document["keys"][0];
    assert_eq!(
        key["intervals"],
        serde_json::json!([
            { "effective_from": wire(at(-10)), "effective_to": wire(at(-1)), "state": "expired" },
            { "effective_from": wire(at(-1)), "effective_to": serde_json::Value::Null, "state": "active" },
        ])
    );
    // The derived end, as the discriminated object: a bare null would have to serve
    // "covered forever" and "covered nowhere" at once.
    assert_eq!(
        key["coverage_end"],
        serde_json::json!({ "kind": "open_ended", "at": serde_json::Value::Null })
    );
    // The key's rendering is the coverage report's own, so an operator matches the
    // two surfaces without transcribing ten axes.
    assert_eq!(
        key["scope_key"].as_str(),
        Some(
            sellability_key(
                plan_id,
                bss_pricing::domain::scope_key::ChargeKind::Recurring
            )
            .to_string()
            .as_str()
        )
    );

    // No boolean value anywhere in the document, at any depth.
    let mut booleans: Vec<String> = Vec::new();
    collect_booleans(&document, String::new(), &mut booleans);
    assert!(
        booleans.is_empty(),
        "the sellability document carries a boolean at {booleans:?}: {document}"
    );

    // And no point-in-time answer under a spelling that is not a boolean either.
    let rendered = document.to_string().to_ascii_lowercase();
    for forbidden in [
        "activenow",
        "isactive",
        "activeat",
        "sellablenow",
        "iscovered",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "{forbidden} is in {rendered}"
        );
    }
}

/// **`inst-sg-renewal`, discharged on what the surface carries rather than on what
/// it takes.**
///
/// The instruction is a statement about the **consumer** — a renewal is never
/// gate-checked — and nothing this gear does can break it, so there is no refusal
/// here to get wrong. The half that *is* ours is the one the plan asked for: the
/// surface answers about a **purchase**, so it carries nothing a caller could
/// mistake for a renewal decision — no subscription id, no in-flight state, no
/// entitlement. That was true by inspection and asserted nowhere; the only case
/// behind the instruction asserted the surface's *inputs*.
///
/// Held as the document's **whole member set, by equality and at every depth**,
/// rather than as an absence of the names somebody thought to forbid. A
/// `subscription_id` added anywhere in the view chain reddens here, and so does any
/// other member nobody meant to publish — which is the property, since the risk is
/// a field arriving somewhere nobody thought to look.
/// `the_document_carries_intervals_and_a_coverage_end_and_no_boolean_anywhere` is
/// the precedent for sweeping the whole document rather than the field a reader
/// expects.
///
/// The world stages a **failed**, three **satisfied** and two **unevaluable**
/// predicates, so every optional member `PredicateAnswerView` can carry — `detail`
/// on one arm, `owed_to` on another — is populated somewhere in the document.
#[tokio::test]
async fn the_document_carries_no_member_a_caller_could_read_as_a_renewal_check() {
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    // Coverage that reaches `at` and stops inside the monthly horizon: predicate (1)
    // is `failed` with a `detail`, (2)(3)(4) are satisfied, (5) and (6) are
    // `not_evaluable` with an `owed_to`.
    project_and_pin(
        &h,
        plan_id,
        4,
        &delta_of(
            plan_id,
            vec![WindowInterval::new(
                at(-1),
                Some(at(2)),
                WindowState::Active,
            )],
        ),
    )
    .await;

    let document = sellability(&h, plan_id, &market_query(0)).await;
    assert_eq!(answer_of(&document, 1), "failed");
    assert_eq!(answer_of(&document, 5), "not_evaluable");

    let mut members: Vec<String> = Vec::new();
    collect_members(&document, &mut members);
    members.sort();
    members.dedup();

    assert_eq!(
        members,
        vec![
            "answer",
            "at",
            "catalog_version",
            "coverage_end",
            "currency",
            "detail",
            "effective_from",
            "effective_to",
            "intervals",
            "keys",
            "kind",
            "ordinal",
            "owed_to",
            "plan_id",
            "predicate",
            "predicates",
            "region",
            "scope_key",
            "state",
            "verdict",
        ],
        "the sellability document publishes exactly these members: nothing naming a \
         subscription, an entitlement or a renewal, and nothing unaccounted for"
    );
}

/// Every member name in `value`, at every depth.
fn collect_members(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_members(item, found);
            }
        }
        serde_json::Value::Object(members) => {
            for (key, member) in members {
                found.push(key.clone());
                collect_members(member, found);
            }
        }
        _ => {}
    }
}

/// Every JSON boolean in `value`, by the path it sits at.
fn collect_booleans(value: &serde_json::Value, path: String, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Bool(_) => found.push(path),
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_booleans(item, format!("{path}[{i}]"), found);
            }
        }
        serde_json::Value::Object(members) => {
            for (key, member) in members {
                collect_booleans(member, format!("{path}.{key}"), found);
            }
        }
        _ => {}
    }
}

/// §9's own case at the wire: a scheduled-but-not-active key is **not sellable**,
/// and the same key becomes sellable at an instant its interval covers.
///
/// One delta, two instants, so what changed is the question and not the world —
/// which is the whole of what a frozen version plus an `at` parameter buys.
#[tokio::test]
async fn a_scheduled_only_key_is_not_sellable_and_the_same_version_answers_otherwise_later() {
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    project_and_pin(
        &h,
        plan_id,
        4,
        &delta_of(
            plan_id,
            vec![WindowInterval::new(at(5), None, WindowState::Scheduled)],
        ),
    )
    .await;

    let before = sellability(&h, plan_id, &market_query(0)).await;
    assert_eq!(answer_of(&before, 1), "failed");
    assert_eq!(before["verdict"], serde_json::json!("not_sellable"));

    let after = sellability(&h, plan_id, &market_query(6)).await;
    assert_eq!(
        answer_of(&after, 1),
        "satisfied",
        "the interval covers the later instant, and no re-projection happened in between"
    );
}

/// D-167 clause (3) at the wire: a consumer can tell a **false** predicate from an
/// **unanswerable** one, and each unanswerable one names what owes it.
///
/// The two fields are separate for exactly this reason — one merged message would
/// have made the difference a matter of reading English.
#[tokio::test]
async fn the_wire_tells_a_false_predicate_from_an_unanswerable_one() {
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    project_and_pin(
        &h,
        plan_id,
        4,
        &delta_of(
            plan_id,
            vec![WindowInterval::new(
                at(-1),
                Some(at(2)),
                WindowState::Active,
            )],
        ),
    )
    .await;

    let document = sellability(&h, plan_id, &market_query(0)).await;

    // (1) is FALSE - covered at the instant, but its coverage ends inside the
    // monthly horizon - and carries a detail and no owed_to.
    let horizon = predicate(&document, 1);
    assert_eq!(horizon["answer"], serde_json::json!("failed"));
    assert!(
        horizon["detail"]
            .as_str()
            .is_some_and(|d| d.contains("horizon")),
        "{horizon}"
    );
    assert_eq!(horizon["owed_to"], serde_json::Value::Null);

    // (5) and (6) are UNANSWERABLE and each names its slice, with no detail.
    for (ordinal, owed) in [(5_u64, "Slice 4"), (6, "registry")] {
        let entry = predicate(&document, ordinal);
        assert_eq!(entry["answer"], serde_json::json!("not_evaluable"));
        assert_eq!(entry["detail"], serde_json::Value::Null);
        assert!(
            entry["owed_to"].as_str().is_some_and(|o| o.contains(owed)),
            "predicate ({ordinal}) must name what owes it: {entry}"
        );
    }

    // And the verdict is the failure rather than the unevaluability: a gate holding
    // a definite refusal must not report "cannot decide".
    assert_eq!(document["verdict"], serde_json::json!("not_sellable"));
}

/// With nothing failed, the verdict is **`not_evaluable` and never `sellable`** —
/// `inst-sg-pinned` is a claim about the finished gear and this build is not it.
#[tokio::test]
async fn a_fully_covered_plan_answers_not_evaluable_rather_than_sellable() {
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    project_and_pin(
        &h,
        plan_id,
        4,
        &delta_of(
            plan_id,
            vec![WindowInterval::new(at(-1), None, WindowState::Active)],
        ),
    )
    .await;

    let document = sellability(&h, plan_id, &market_query(0)).await;

    for ordinal in [1_u64, 2, 3, 4] {
        assert_eq!(
            answer_of(&document, ordinal),
            "satisfied",
            "predicate ({ordinal}) in {document}"
        );
    }
    // One assertion, not two: the equality already excludes `sellable`. The
    // `assert_ne!` that stood beside it until 2026-08-20 compared the *same* value
    // to a different literal on the line below, so it was true by definition
    // whenever the line above passed — it could never redden independently, and it
    // read as a second guarantee about `inst-sg-pinned` that was not there.
    assert_eq!(document["verdict"], serde_json::json!("not_evaluable"));
}

/// A plan no pin-eligible version carries answers **200 with predicate (2)
/// failed**, and a plan that does not exist reads identically.
///
/// Not a 404, for `get_plan_coverage`'s reason one route over: the difference
/// between "no such plan" and "not in your scope" is what a 404 would leak, and the
/// answer here is a *state* rather than an absent resource. The remaining predicates
/// are `not_evaluable` and not `failed`, because with no version there is no fact
/// for them to read.
#[tokio::test]
async fn a_plan_no_version_carries_and_an_absent_plan_read_alike() {
    let h = Harness::new().await;
    let unprojected = Uuid::now_v7();
    seed_draft_plan(&h, unprojected).await;

    let existing = sellability(&h, unprojected, &market_query(0)).await;
    let absent = sellability(&h, Uuid::now_v7(), &market_query(0)).await;

    for document in [&existing, &absent] {
        assert_eq!(answer_of(document, 2), "failed");
        assert_eq!(document["catalog_version"], serde_json::Value::Null);
        assert_eq!(document["keys"], serde_json::json!([]));
        assert_eq!(document["verdict"], serde_json::json!("not_sellable"));
        for ordinal in [3_u64, 4] {
            assert_eq!(
                answer_of(document, ordinal),
                "not_evaluable",
                "predicate ({ordinal}) has no operand without a version: {document}"
            );
        }
    }
    // The two answers differ only in the plan id they echo, which is what "read
    // alike" has to mean for the leak to be closed.
    assert_eq!(existing["plan_id"], serde_json::json!(unprojected));
    assert_ne!(existing["plan_id"], absent["plan_id"]);
}

/// The same answer where the tenant **has** a live frontier and this plan is simply
/// not in it — the arm the case above cannot reach.
///
/// Above, the tenant has no frontier at all, so the answer comes from that arm and
/// the "no delta at the frontier" arm is never exercised. The two absences are
/// deliberately one answer to a consumer, and this is the second of them: a tenant
/// with a published catalog, asked about a plan no pin-eligible version carries.
///
/// Found by replacing this arm with a 404 and watching nothing redden. A 404 here
/// would tell an unauthorized caller which plan ids exist, which is the leak
/// `get_plan_coverage` refuses one route over.
#[tokio::test]
async fn a_plan_absent_from_a_live_frontier_answers_the_same_way() {
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let h = Harness::new().await;
    // Another plan holds the frontier, so version 4 is pin-eligible for the tenant.
    let published = Uuid::now_v7();
    project_and_pin(
        &h,
        published,
        4,
        &delta_of(
            published,
            vec![WindowInterval::new(at(-1), None, WindowState::Active)],
        ),
    )
    .await;

    let stranger = Uuid::now_v7();
    let document = sellability(&h, stranger, &market_query(0)).await;

    assert_eq!(
        answer_of(&document, 2),
        "failed",
        "the frontier exists and carries no delta for this plan: {document}"
    );
    assert_eq!(document["catalog_version"], serde_json::Value::Null);
    assert_eq!(document["keys"], serde_json::json!([]));
    assert_eq!(document["verdict"], serde_json::json!("not_sellable"));

    // And the plan that *is* in the frontier answers off it, so what refused above
    // is this plan's absence and not a frontier the tenant does not have.
    let live = sellability(&h, published, &market_query(0)).await;
    assert_eq!(live["catalog_version"], serde_json::json!(4));
}

/// All three query parameters are required, and an absent or unparseable one is a
/// **problem document naming it** rather than axum's bare rejection.
#[tokio::test]
async fn each_missing_or_malformed_query_parameter_is_named_in_a_problem_document() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();

    let cases = [
        ("currency=EUR&region=eu", "at"),
        ("at=2099-10-01T00:00:00.000000Z&region=eu", "currency"),
        ("at=2099-10-01T00:00:00.000000Z&currency=EUR", "region"),
        ("at=whenever&currency=EUR&region=eu", "at"),
    ];
    for (query, named) in cases {
        let response = h
            .allowed()
            .send(request("GET", &sellability_path(plan_id, query), None))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "`{query}` must be refused"
        );
        let rendered = format!("{}", body_json(response).await);
        assert!(
            rendered.contains(named),
            "the refusal must name `{named}`: {rendered}"
        );
    }
}

/// An RFC 3339 instant whose offset is written `+00:00` and **not** percent-encoded
/// arrives with a space where the `+` was, and is refused rather than guessed at.
///
/// `+` is a space under form-urlencoding, so this is a property of query strings
/// rather than of this surface — but it is *this* surface's only instant-valued
/// parameter, so it is where a client meets it. Accepting the mangled value would
/// mean accepting a string that is not an instant in any format, and the guess would
/// be silent: the caller would get an answer about a moment they did not name.
#[tokio::test]
async fn an_unencoded_offset_instant_is_refused_rather_than_guessed_at() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();

    // Chrono's `DateTime::to_rfc3339` (and some clients) write UTC as `+00:00`.
    // `+` is a space under form-urlencoded query strings, so pasting that form
    // unencoded is not an instant. `format_rfc3339` uses `Z` and would not
    // provoke this refusal — the case is the offset form.
    let raw = format!("{}+00:00", wire(at(0)).trim_end_matches('Z'));
    assert!(
        raw.ends_with("+00:00"),
        "the offset form is what is at issue: {raw}"
    );
    let response = h
        .allowed()
        .send(request(
            "GET",
            &sellability_path(
                plan_id,
                &format!("at={raw}&currency={MARKET_CURRENCY}&region={MARKET_REGION}"),
            ),
            None,
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let rendered = format!("{}", body_json(response).await);
    assert!(
        rendered.contains("at"),
        "the refusal names the parameter: {rendered}"
    );

    // And the same instant with the `Z` designator is accepted, so what is refused
    // is the encoding and not the value.
    let response = h
        .allowed()
        .send(request(
            "GET",
            &sellability_path(plan_id, &market_query(0)),
            None,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// **A warm delta a tenant with no frontier at all cannot pin is not readable
/// here** — the absent-frontier arm, staged with content sitting in the store.
///
/// This is *not* the version-bound case below, and it was named for that one until
/// a review found the version comparison was never reached: with no
/// `pricing_pin_frontier` row the first arm of `sellability_facts` answers and the
/// frontier's version is never read. What this world does hold, and the case it is
/// the twin of does not, is that **a row being present is not what authorizes a
/// read**: `a_plan_no_version_carries_and_an_absent_plan_read_alike` stages two
/// empty worlds, so it stays green under an arm that fell through to whatever warm
/// row it found.
///
/// Proved by replacing the `NotAddressable` return on the absent frontier with a
/// fall-through resolving at `u64::MAX >> 1`: this case reddens and the two-empty-
/// worlds case does not.
#[tokio::test]
async fn a_delta_projected_before_any_frontier_exists_is_not_addressable() {
    use bss_pricing::domain::read_model::SubjectRef;
    use bss_pricing::domain::window::{WindowInterval, WindowState};
    use bss_pricing::infra::storage::repo::{NewDelta, read_model_repo};
    use bss_pricing_sdk::CatalogVersion;

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let delta = delta_of(
        plan_id,
        vec![WindowInterval::new(at(-1), None, WindowState::Active)],
    );
    // Projected, and the tenant's frontier deliberately never created.
    let conn = h.db.conn().expect("conn");
    read_model_repo::project_subject(
        &conn,
        &h.scope(),
        NewDelta {
            tenant_id: h.tenant,
            catalog_version: CatalogVersion::new(4),
            subject: SubjectRef::Plan(plan_id),
            payload: delta.to_value(),
            projected_at: at(0),
        },
    )
    .await
    .expect("project the plan subject");

    let document = sellability(&h, plan_id, &market_query(0)).await;

    assert_eq!(
        answer_of(&document, 2),
        "failed",
        "a tenant with no frontier can pin nothing, warm row or not: {document}"
    );
    assert_eq!(document["catalog_version"], serde_json::Value::Null);
    assert_eq!(document["verdict"], serde_json::json!("not_sellable"));

    // And the same delta becomes readable the moment the tenant has a frontier
    // carrying it, so what refused above is the absent frontier and not the delta.
    project_and_pin(&h, plan_id, 5, &delta).await;
    let document = sellability(&h, plan_id, &market_query(0)).await;
    assert_eq!(answer_of(&document, 2), "satisfied");
    assert_eq!(document["catalog_version"], serde_json::json!(5));
}

/// **A projected delta whose version is past the tenant's pin-eligible frontier is
/// not readable here**, and that is D-101 at this surface rather than a detail of
/// the frontier read.
///
/// The world: the tenant's frontier stands at V4, held by a **different** plan's
/// subject, and this plan's only delta was projected at V5 — the degraded publish,
/// whose version D-101 holds un-eligible so consumers keep pinning the previous one.
/// If this surface resolved at anything other than the frontier it would answer off
/// content nobody may pin yet, and a storefront would sell against a version whose
/// siblings' warms are still outstanding.
///
/// **The frontier has to exist, and be held by somebody else, for the version bound
/// to be the thing answering.** The case above stages no frontier at all, so it
/// reaches the absent-frontier arm and the comparison is never made: against *that*
/// world, replacing `frontier.catalog_version` with `u64::MAX >> 1` — deleting
/// "answer only at the frontier" while keeping the frontier read — left the whole
/// suite green. Against this one the same removal reddens exactly here.
#[tokio::test]
async fn a_delta_whose_version_is_not_pin_eligible_is_not_addressable() {
    use bss_pricing::domain::read_model::SubjectRef;
    use bss_pricing::domain::window::{WindowInterval, WindowState};
    use bss_pricing::infra::storage::repo::{NewDelta, pin_frontier_repo, read_model_repo};
    use bss_pricing_sdk::CatalogVersion;

    let h = Harness::new().await;
    // Another plan's subject holds the frontier at 4: the tenant has a live
    // pin-eligible version, and it is not the one carrying the plan under test.
    let holder = Uuid::now_v7();
    project_and_pin(
        &h,
        holder,
        4,
        &delta_of(
            holder,
            vec![WindowInterval::new(at(-1), None, WindowState::Active)],
        ),
    )
    .await;

    let plan_id = Uuid::now_v7();
    let delta = delta_of(
        plan_id,
        vec![WindowInterval::new(at(-1), None, WindowState::Active)],
    );
    // This plan's only delta, one version **past** the frontier.
    let conn = h.db.conn().expect("conn");
    read_model_repo::project_subject(
        &conn,
        &h.scope(),
        NewDelta {
            tenant_id: h.tenant,
            catalog_version: CatalogVersion::new(5),
            subject: SubjectRef::Plan(plan_id),
            payload: delta.to_value(),
            projected_at: at(0),
        },
    )
    .await
    .expect("project the plan subject");

    let document = sellability(&h, plan_id, &market_query(0)).await;

    assert_eq!(
        answer_of(&document, 2),
        "failed",
        "a version past the frontier is not addressable: {document}"
    );
    assert_eq!(document["catalog_version"], serde_json::Value::Null);
    assert_eq!(document["verdict"], serde_json::json!("not_sellable"));

    // The frontier is live and answers for the plan that holds it, so what refused
    // above is the version bound and not an absent frontier.
    let holding = sellability(&h, holder, &market_query(0)).await;
    assert_eq!(holding["catalog_version"], serde_json::json!(4));

    // And the same delta becomes readable the moment the frontier reaches its
    // version, so what refused is the bound and not the delta.
    pin_frontier_repo::advance(&conn, &h.scope(), h.tenant, CatalogVersion::new(5), at(0))
        .await
        .expect("advance the frontier onto the delta's version");
    let document = sellability(&h, plan_id, &market_query(0)).await;
    assert_eq!(answer_of(&document, 2), "satisfied");
    assert_eq!(document["catalog_version"], serde_json::json!(5));
}

/// **The seventh unasserted `repo_failure` arm: `WindowOverlap` → 409
/// `WINDOW_OVERLAP`, over HTTP.**
///
/// `window_repo::refuse_overlap` is executed by `tests/sqlite_window_repo.rs`, which
/// asserts the **`RepoError`** and stops there. Nothing observed what `repo_failure`
/// turns it into: corrupting that arm to `Internal` would have left the whole suite
/// green while an operator scheduling a colliding interval got a 500 telling them to
/// page somebody. `conflict_reason` was written for this code — its own doc names
/// `WINDOW_OVERLAP` as the measurement that proved a containment assertion useless —
/// and had no caller asserting it.
///
/// **And the ordering this exposes is reported rather than changed.** The overlap guard
/// is the *store's*, at step 5 of the mutation, while the materiality gate is step 3a —
/// so under an **unset** threshold policy an overlapping schedule answers 202 with a
/// unit and only meets the collision on the approved retry. That contradicts
/// `infra::window`'s own ordering rule ("a mutation that cannot commit must not be put
/// in front of a reviewer") for exactly the refusals the store owns rather than the
/// domain: the interior gap and the trailing void run before the gate, the overlap and
/// the frozen-end refusals do not. Moving them means the domain re-implementing
/// `refuse_overlap`, which is a second owner for one rule — so it is written down here.
/// This case therefore stages the **configured** world, where the gate declines and the
/// store answers.
#[tokio::test]
async fn an_overlapping_schedule_is_refused_by_its_code_and_writes_nothing() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    // Inside the fixture window's own interval, so the two collide. Adjacency at
    // `common_to()` is the legal case and is `a_scheduled_window_answers_202_...`'s.
    let response = post_window(
        &h,
        seeded.price_id,
        common_from() + time::Duration::days(1),
        Some(common_to() + time::Duration::days(1)),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        conflict_reason(response).await,
        "WINDOW_OVERLAP",
        "read off `context.reason` and compared by equality: the field a consumer branches on"
    );

    // And nothing landed: the collision is refused inside the transaction, so the
    // window plane still holds exactly the fixture's one interval.
    let conn = h.state.db.conn().expect("conn");
    let plane = bss_pricing::infra::storage::repo::window_repo::list_for_plan(
        &conn,
        &h.scope(),
        h.tenant,
        PlanId::new(plan_id),
    )
    .await
    .expect("read the plane");
    assert_eq!(
        plane.len(),
        1,
        "the refused schedule wrote no second window: {plane:?}"
    );
}

// ---------------------------------------------------------------------------
// The precondition on the PATCH (D-191), over the act sequence D-190 added
// ---------------------------------------------------------------------------

/// **The `If-Match` this route has always required is now compared.**
///
/// Until D-191 the header was parsed for its 400 and dropped, which is worse than not
/// requiring it: a caller who sent it believed they were protected. What the tag names
/// is the window's **act sequence**, the one monotonic thing a window row carries
/// (D-190) — there is no row version on this table and no `GET` on a window, so the
/// tag is minted by the mutating verbs themselves and this is the loop it closes.
///
/// The three acts are the whole argument: the first lands under the tag the window
/// stands at, the second is refused for asserting a sequence the first consumed, and
/// the third — the same act under the tag the refusal implies — lands. Without the
/// third this case could not tell "refuses a stale tag" from "refuses every second
/// act".
#[tokio::test]
async fn an_adjustment_asserting_a_consumed_act_sequence_is_refused_as_stale() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);

    let first = patch_window_asserting(&h, window_id, Some(at(30)), "\"0\"").await;
    assert_eq!(
        first.status(),
        StatusCode::ACCEPTED,
        "a lengthening under a configured policy commits on one principal"
    );

    let stale = patch_window_asserting(&h, window_id, Some(at(40)), "\"0\"").await;
    assert_eq!(
        stale.status(),
        StatusCode::CONFLICT,
        "the act sequence the caller asserted has been consumed"
    );
    assert_eq!(
        conflict_reason(stale).await,
        "STALE_VERSION",
        "a moved premise is the Foundation's own code and no new one is minted"
    );

    let conn = h.state.db.conn().expect("conn");
    let stored = bss_pricing::infra::storage::repo::window_repo::find(
        &conn,
        &h.scope(),
        h.tenant,
        window_id,
    )
    .await
    .expect("read the window")
    .expect("it is there");
    assert_eq!(
        stored.effective_to,
        Some(at(30)),
        "and the refused act wrote nothing"
    );

    let fresh = patch_window_asserting(&h, window_id, Some(at(40)), "\"1\"").await;
    assert_eq!(
        fresh.status(),
        StatusCode::ACCEPTED,
        "the same act under the sequence the window now stands at is ordinary"
    );
}

/// **The tag has a producer, which is what makes the precondition satisfiable.**
///
/// D-191 clause (2) records that a window has no `GET` and therefore no representation
/// that could carry an entity tag. That is still true and is not worked around here:
/// the tag is handed back by the **mutating** verbs, so a caller who schedules a
/// window holds the tag its next act must assert, and each act hands back the next
/// one. A surface that demanded a precondition no response ever supplied would be
/// requiring a header the caller cannot obtain.
#[tokio::test]
async fn a_committed_window_mutation_hands_back_the_tag_the_next_act_must_assert() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    let scheduled = post_window(&h, seeded.price_id, common_to(), Some(at(0))).await;
    assert_eq!(scheduled.status(), StatusCode::ACCEPTED);
    assert_eq!(
        etag_of(&scheduled).as_deref(),
        Some("\"0\""),
        "a window is born at act zero, and the schedule says so"
    );
    let body = body_json(scheduled).await;
    let window_id = body["window"]["window_id"]
        .as_str()
        .map(|s| Uuid::parse_str(s).expect("a uuid"))
        .expect("the schedule names the window it wrote");

    let adjusted = patch_window_asserting(&h, window_id, Some(at(30)), "\"0\"").await;
    assert_eq!(adjusted.status(), StatusCode::ACCEPTED);
    assert_eq!(
        etag_of(&adjusted).as_deref(),
        Some("\"1\""),
        "and the act that consumed act zero hands back the next tag"
    );
}

// ---------------------------------------------------------------------------
// The POST's at-most-once guarantee (D-191), and what a key identifies
// ---------------------------------------------------------------------------

/// **A retried schedule replays the first answer and mints no second window.**
///
/// The `Idempotency-Key` was required on this route from the day it was mounted and
/// **discarded**, which is worse than not requiring it: a caller who sent it believed
/// they were protected, and what they actually got was a second window id per attempt —
/// so a retry either scheduled a second window, was refused as an overlap, or opened a
/// second approval unit. There was no gate on this surface at all, unlike the plan and
/// price authoring routes.
///
/// The assertion that carries it is the **window id**: a replay hands back the stored
/// body verbatim, so the second answer names the window the first one minted. A build
/// that ran the mutation again would answer 202 as well, which is why the id and the
/// row count are both here.
#[tokio::test]
async fn a_retried_schedule_replays_the_first_answer_and_mints_no_second_window() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let key = "one-attempt-at-scheduling";

    let first = post_window_under(&h, seeded.price_id, common_to(), Some(at(0)), key).await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first = body_json(first).await;
    assert_eq!(first["outcome"], "mutated", "{first}");

    let replay = post_window_under(&h, seeded.price_id, common_to(), Some(at(0)), key).await;
    assert_eq!(
        replay.status(),
        StatusCode::ACCEPTED,
        "a replay answers what the first attempt was told"
    );
    let replay = body_json(replay).await;
    assert_eq!(
        replay, first,
        "the stored body comes back verbatim, window id and all"
    );

    let conn = h.state.db.conn().expect("conn");
    let windows = bss_pricing::infra::storage::repo::window_repo::list_for_plan(
        &conn,
        &h.scope(),
        h.tenant,
        PlanId::new(plan_id),
    )
    .await
    .expect("list the plan's windows");
    assert_eq!(
        windows.len(),
        2,
        "the fixture's window and exactly one scheduled by these two attempts: {windows:#?}"
    );
}

/// One key, two different intents — the refusal the gate exists to make visible.
///
/// Reached through the same ladder every other guarded route reaches it through, so
/// this route mints no status and no code of its own.
#[tokio::test]
async fn a_key_reused_for_a_different_schedule_is_refused_as_a_payload_mismatch() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    let key = "one-key-two-intents";

    let first = post_window_under(&h, seeded.price_id, common_to(), Some(at(0)), key).await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);

    let mismatch = post_window_under(&h, seeded.price_id, common_to(), Some(at(30)), key).await;
    assert_eq!(mismatch.status(), StatusCode::CONFLICT);
    assert_eq!(
        conflict_reason(mismatch).await,
        "IDEMPOTENCY_PAYLOAD_MISMATCH",
        "the key is held by a different request, which is the caller's mistake and not \
         a moved premise"
    );
}

/// **A retried *refused* schedule replays its unit rather than opening a second.**
///
/// The arm that matters most, and the reason the unit is opened **inside** the guarded
/// transaction: the recorded body has to be the body that was actually sent, and on the
/// refused arm that body names the approval. Without the gate the second attempt met
/// `PENDING_CHANGE_UNIT_EXISTS` — the first attempt's own unit holding the key — so a
/// caller retrying a request they had no confirmation of was told their act conflicted
/// with itself.
#[tokio::test]
async fn a_retried_refused_schedule_replays_its_unit_rather_than_opening_a_second() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published_unconfigured(&h, plan_id).await;
    let key = "one-attempt-at-a-controlled-act";

    let refused = post_window_under(&h, seeded.price_id, common_to(), None, key).await;
    assert_eq!(refused.status(), StatusCode::ACCEPTED);
    let refused = body_json(refused).await;
    assert_eq!(refused["outcome"], "submitted_for_approval", "{refused}");

    let replay = post_window_under(&h, seeded.price_id, common_to(), None, key).await;
    assert_eq!(
        replay.status(),
        StatusCode::ACCEPTED,
        "the retry is the same attempt, so it is answered rather than re-run"
    );
    let replay = body_json(replay).await;
    assert_eq!(
        replay, refused,
        "including the unit: a second attempt must not put a second reviewer to work"
    );
    assert_eq!(
        units_of(&h, AuditSubjectKind::Window).await.len(),
        1,
        "exactly one unit, and no PENDING_CHANGE_UNIT_EXISTS about the caller's own act"
    );
}

/// **A schedule with no `Idempotency-Key` is refused and writes nothing.**
///
/// `tests/rest_plans.rs` has carried this case for the plan create since that route was
/// guarded; the window schedule had no equivalent while its key was parsed and dropped,
/// which is the state D-191 closed. Now that the header actually buys the at-most-once
/// guarantee its own description promises, the refusal is worth pinning at the route:
/// executing unguarded when the header is absent would mean the promise held only for
/// callers who happened to opt in — and the caller who most needs it is the one whose
/// retry logic sent the request twice without thinking about it.
///
/// It is a **400** and no new code, which is `preconditions`' own reading of D-141: an
/// absent precondition is a malformed request under the Foundation validation envelope.
#[tokio::test]
async fn a_schedule_without_an_idempotency_key_is_refused_and_writes_nothing() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    let response = h
        .allowed()
        .send(request(
            "POST",
            &windows_path(seeded.price_id),
            Some(serde_json::json!({
                "effective_from": wire(common_to()),
                "effective_to": wire(at(0)),
                "reason_code": "priceIncrease",
            })),
        ))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // **And the refusal names the header the caller must send.** The status alone
    // was the whole of this half until 2026-08-20, so any future 400 on this route
    // — a body rule, a parameter rule — satisfied it, and the test would have kept
    // passing with the `Idempotency-Key` requirement removed and something else
    // refusing first. Read the way
    // `rest_threshold_policy::a_proposal_without_an_if_match_is_refused_and_writes_nothing`
    // reads its own missing precondition: this refusal carries no wire code (an
    // absent precondition is a malformed request, per the doc above), so the
    // document naming the header is the only thing a caller can act on.
    let problem = body_json(response).await;
    assert!(
        problem["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Idempotency-Key")),
        "the caller is told which header is missing: {problem}"
    );

    let conn = h.state.db.conn().expect("conn");
    let windows = bss_pricing::infra::storage::repo::window_repo::list_for_plan(
        &conn,
        &h.scope(),
        h.tenant,
        PlanId::new(plan_id),
    )
    .await
    .expect("list the plan's windows");
    assert_eq!(
        windows.len(),
        1,
        "the fixture's window and nothing this request wrote: {windows:#?}"
    );
}

// ---------------------------------------------------------------------------
// The SQL tenant predicate on the three window mutations.
//
// `rest_authz.rs`'s census cannot reach these: its seed leaves a draft plan and
// all three answer their **owner** a `404 current plan revision … not found`
// there, so they are listed in `BY_ID_WRITES_THIS_FIXTURE_CANNOT_STAGE`. This
// suite's fixture publishes, so here the owner's identical call is accepted and a
// refusal of the foreign caller means tenancy rather than a missing subject.
//
// `Harness::denied` drives a PDP that refuses everything and
// `Harness::scope_mismatch` exercises `access_scope`'s write-target membership
// assertion; neither hands a caller-supplied id of another tenant's row to the
// repository, which is the only way the predicate is exercised at all.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_foreign_tenant_cannot_schedule_a_window_on_this_tenants_row() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;

    let schedule = |price_id: Uuid, key: &str| {
        with_headers(
            "POST",
            &windows_path(price_id),
            Some(serde_json::json!({
                "effective_from": wire(common_to()),
                "effective_to": serde_json::Value::Null,
                "reason_code": "priceIncrease",
            })),
            &[("idempotency-key", key)],
        )
    };
    rest_support::foreign_is_indistinguishable(
        &h,
        schedule(seeded.price_id, "foreign-key"),
        schedule(Uuid::now_v7(), "absent-key"),
    )
    .await;

    // The control, last because it succeeds and a success writes. Without it a
    // route refusing every caller — a stale tag, a body the rules reject — reads
    // as tenant isolation.
    let owner = post_window(&h, seeded.price_id, common_to(), None).await;
    assert_eq!(
        owner.status(),
        StatusCode::ACCEPTED,
        "the owner's identical schedule must be accepted, or the refusals above are about the \
         request rather than about the tenant"
    );
}

#[tokio::test]
async fn a_foreign_tenant_cannot_shorten_this_tenants_window() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = published(&h, plan_id).await;
    // The seed's own window and a lengthening, which is
    // `an_adjusted_window_answers_202_and_the_stored_end_moves`'s fixture: it is the
    // one shape of this verb that commits on a single principal, so the control
    // below is a success rather than a unit.
    let window_id = common::coverage_window_id(seeded.price_id);

    let shorten = |id: Uuid| {
        with_headers(
            "PATCH",
            &window_path(id),
            Some(serde_json::json!({ "effective_to": wire(at(30)) })),
            &[("if-match", IF_MATCH)],
        )
    };
    rest_support::foreign_is_indistinguishable(&h, shorten(window_id), shorten(Uuid::now_v7()))
        .await;

    let owner = patch_window(&h, window_id, Some(at(30))).await;
    assert_eq!(
        owner.status(),
        StatusCode::ACCEPTED,
        "the owner's identical shortening must be accepted"
    );
}

#[tokio::test]
async fn a_foreign_tenant_cannot_cancel_this_tenants_window() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    // `cancellable` leaves an open-ended successor, so cancelling the first window
    // removes no coverage end and the trailing-void floor does not answer first —
    // without it the owner's control would be the `WINDOW_TRAILING_VOID` refusal
    // rather than an acceptance.
    let (_seeded, window_id) = cancellable(&h, plan_id).await;

    rest_support::foreign_is_indistinguishable(
        &h,
        request("DELETE", &window_path(window_id), None),
        request("DELETE", &window_path(Uuid::now_v7()), None),
    )
    .await;

    let owner = delete_window(&h, window_id).await;
    assert_eq!(
        owner.status(),
        StatusCode::ACCEPTED,
        "the owner's identical cancellation must be accepted"
    );
    // A cancel is D-62 controlled, so the accepted call opens a unit rather than
    // flipping the window — and asserting the unit is what stops the control being
    // "the route answers 202 and does nothing".
    assert_eq!(
        units_of(&h, AuditSubjectKind::Window).await.len(),
        1,
        "and it is the call that puts the cancellation in front of a reviewer"
    );
    assert_eq!(
        window_state(&h, window_id).await.as_deref(),
        Some("scheduled"),
        "which the window is still waiting on"
    );
}
