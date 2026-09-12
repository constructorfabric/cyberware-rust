//! What the coverage rules refuse, and the worlds in which each refusal is the
//! thing that answers.
//!
//! Every instant here is a **fixed** one — `at(day)` is 2026-09-01 plus a whole
//! number of days — and none of these rules reads a clock: the coverage walk is
//! pure over an interval set, so no case can start passing or failing because
//! today's date moved past it.
//!
//! The pipeline under test is [`window_coverage_rules`], Slice 7's own set,
//! rather than `run_publish_rules`. The aggregate's *registration* of this set is
//! a separate fact with its own test in `publish/rules_tests.rs`, because a test
//! that ran the aggregate would redden both when a rule is wrong and when the
//! registration is missing, and those are two defects with two different fixes.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    AVAILABILITY_OUTSIDE_COVERAGE, WINDOW_COVERAGE_MISSING, WINDOW_GAP, check_shape,
    longest_cycle_sold, window_coverage_rules,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::format_rfc3339;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan_shape::{
    BillingCycle, CustomIntervalUnit, Frequency, PhaseGraph, PhaseKind, PlanPhase, PlanShape,
};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::ValidationReport;
use crate::domain::window::{CoverageEnd, KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A window-scale instant: 2026-09-01 plus `day` whole days.
fn at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2026, 9, 1, 0, 0, 0) + time::Duration::days(day)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xf1))
}

fn key(charge_kind: ChargeKind, currency: &str, region: &str) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new(currency).expect("three letters"),
        Region::new(region).expect("non-blank"),
        phase(),
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

/// A row on `scope_key`, publishable as far as every rule outside this module is
/// concerned.
///
fn row_on(price_id: u128, scope_key: ScopeKey) -> PriceRecord {
    PriceRecord {
        price_id: Uuid::from_u128(price_id),
        scope_key,
        row: {
            let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
            row.amount_minor = Some(MinorAmount::new(9_900).expect("non-negative"));
            row
        },
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(0),
        row_version: RowVersion::new(1),
    }
}

/// A group of windows on one key.
fn group(scope_key: ScopeKey, intervals: Vec<WindowInterval>) -> KeyWindows {
    KeyWindows {
        scope_key,
        intervals,
    }
}

/// `[from, to)` in `state`.
fn interval(from: i64, to: Option<i64>, state: WindowState) -> WindowInterval {
    WindowInterval::new(at(from), to.map(at), state)
}

/// A plan with one recurring EUR/eu row and whatever windows the case gives it.
fn one_row_plan(windows: Vec<KeyWindows>) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 1, at(0));
    shape.billing_cycle = Some(BillingCycle::Recurring);
    shape.frequency = Some(Frequency::Monthly);
    shape.plan_tier = Some("standard".to_owned());
    shape.phases = PhaseGraph::new(vec![PlanPhase {
        phase_id: phase(),
        kind: PhaseKind::Evergreen,
        display_name: None,
        ordinal: 0,
        converts_to_phase_id: None,
        phase_duration_days: None,
        display_trial_days: None,
    }]);
    shape.rows = vec![row_on(0xb001, key(ChargeKind::Recurring, "EUR", "eu"))];
    shape.windows = windows;
    shape
}

fn verdict(shape: &PlanShape) -> ValidationReport {
    // `false`: the contract of every caller except the publish. D-332 lets the
    // publish skip a key it is about to cover, and this module's subject is the
    // rule itself — so the default is what it runs under, and the exemption has
    // its own case below.
    window_coverage_rules(false).run(shape)
}

fn codes(report: &ValidationReport) -> Vec<String> {
    report
        .violations
        .iter()
        .map(|violation| violation.code.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// inst-wc-required — the key must hold a live window
// ---------------------------------------------------------------------------

/// `inst-wc-required`: a billable row whose key has no active/scheduled window
/// fails publish. No silent fallback — Tariffs step 2 would resolve nothing.
#[test]
/// **D-332's exemption, and the two halves that make it narrow.**
///
/// The publish opens coverage for a key it is freezing, so the rule must not
/// refuse that key — while a key carrying a row that is **already published**
/// has had coverage and lost it, which is an author's mistake and stays refused.
/// Both are asserted from one fixture pair, because the exemption is only sound
/// if the second half holds: a rule that skipped every uncovered key would pass
/// this test's first assertion and delete itself.
fn the_publish_exemption_reaches_a_draft_key_and_not_a_published_one() {
    // `one_row_plan` with no windows: one billable key, uncovered.
    let draft = one_row_plan(vec![]);
    assert!(
        window_coverage_rules(true).run(&draft).is_publishable(),
        "the publish opens this key's first window itself, so refusing it would \
         refuse a plan the same call is about to cover"
    );
    assert!(
        !window_coverage_rules(false).run(&draft).is_publishable(),
        "and every other caller of this set - the repricing apply's aggregate \
         pass - opens nothing, so there the same key must still fail"
    );

    let mut lost = one_row_plan(vec![]);
    for row in &mut lost.rows {
        row.lifecycle_state = LifecycleState::Published;
    }
    assert!(
        !window_coverage_rules(true).run(&lost).is_publishable(),
        "a key whose row is already frozen had coverage and lost it: the \
         exemption must not reach it even from the publish"
    );
}

#[test]
fn a_billable_row_with_no_window_fails_publish() {
    let report = verdict(&one_row_plan(Vec::new()));

    assert_eq!(codes(&report), vec![WINDOW_COVERAGE_MISSING.to_owned()]);
    assert!(!report.is_publishable());
    assert_eq!(
        report.violations[0].subject,
        key(ChargeKind::Recurring, "EUR", "eu").to_string(),
        "the finding names the key, because scheduling a window on it is the edit"
    );
}

/// The world in which the above is observable, and without it the refusal test
/// proves nothing: a row WITH a window publishes.
///
/// A rule that refused every plan would pass the test above and fail this one.
#[test]
fn a_billable_row_with_a_scheduled_window_publishes() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let report = verdict(&one_row_plan(vec![group(
        scope_key,
        vec![interval(1, None, WindowState::Scheduled)],
    )]));

    assert_eq!(codes(&report), Vec::<String>::new());
    assert!(report.is_publishable());
}

/// A key whose only window has **expired** holds no live coverage, even though
/// its coverage end is a real instant.
///
/// The distinction the rule rests on: `coverage_end()` counts expired intervals
/// on purpose — a dormant key's coverage ended, it did not fail to exist — so a
/// rule spelled as `coverage_end() != Uncovered` would let this plan publish.
#[test]
fn a_key_whose_only_window_expired_has_no_live_coverage() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![interval(1, Some(2), WindowState::Expired)],
    )]);

    let coverage = check_shape(&shape);
    let entry = coverage.find(&scope_key).expect("the key is in the report");
    assert_eq!(
        entry.coverage_end(),
        CoverageEnd::Ends(at(2)),
        "the coverage end is where the expired window ended, not nowhere"
    );
    assert!(!entry.has_live_window());
    assert_eq!(
        codes(&verdict(&shape)),
        vec![WINDOW_COVERAGE_MISSING.to_owned()]
    );
}

/// A cancelled window is a schedule that never happened, so it covers nothing.
#[test]
fn a_cancelled_window_is_not_coverage() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![interval(1, None, WindowState::Cancelled)],
    )]);

    let coverage = check_shape(&shape);
    let entry = coverage.find(&scope_key).expect("the key is in the report");
    assert_eq!(entry.coverage_end(), CoverageEnd::Uncovered);
    assert!(!entry.has_live_window());
    assert_eq!(
        codes(&verdict(&shape)),
        vec![WINDOW_COVERAGE_MISSING.to_owned()]
    );
}

/// `inst-wc-perkey`: a hybrid's recurring / usage / `one_time_setup` components
/// carry their own coverage. Covering the recurring key does not cover the
/// usage key.
#[test]
fn a_hybrid_needs_coverage_on_every_charge_kind_key() {
    let recurring = key(ChargeKind::Recurring, "EUR", "eu");
    let usage = key(ChargeKind::Usage, "EUR", "eu");
    let setup = key(ChargeKind::OneTimeSetup, "EUR", "eu");

    let mut shape = one_row_plan(vec![group(
        recurring.clone(),
        vec![interval(1, None, WindowState::Scheduled)],
    )]);
    shape.billing_cycle = Some(BillingCycle::Hybrid);
    shape.rows = vec![
        row_on(0xb001, recurring.clone()),
        row_on(0xb002, usage.clone()),
        row_on(0xb003, setup.clone()),
    ];

    let report = verdict(&shape);
    let named: Vec<String> = report
        .violations
        .iter()
        .map(|violation| violation.subject.clone())
        .collect();

    assert_eq!(
        codes(&report),
        vec![
            WINDOW_COVERAGE_MISSING.to_owned(),
            WINDOW_COVERAGE_MISSING.to_owned()
        ],
        "the two uncovered component keys, and not the covered one"
    );
    assert!(named.contains(&setup.to_string()));
    assert!(named.contains(&usage.to_string()));
    assert!(
        !named.contains(&recurring.to_string()),
        "the recurring key is covered and must not be named"
    );
}

/// An uncovered key held by two rows is named **once**.
///
/// The finding is per key because the remedy is per key: two lines would tell the
/// author to schedule one window twice.
#[test]
fn an_uncovered_key_is_named_once_however_many_rows_hold_it() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let mut shape = one_row_plan(Vec::new());
    shape.rows = vec![row_on(0xb001, scope_key.clone()), row_on(0xb002, scope_key)];

    assert_eq!(
        codes(&verdict(&shape)),
        vec![WINDOW_COVERAGE_MISSING.to_owned()]
    );
}

/// A key only a non-candidate row holds appears in the report and is not
/// required.
///
/// The report is the operator's view of the whole plane; `inst-wc-required` is a
/// statement about rows that are publishing. A key with windows and no candidate
/// row has no row whose publish could be refused.
#[test]
fn a_key_no_candidate_row_holds_is_reported_but_not_required() {
    let held = key(ChargeKind::Recurring, "EUR", "eu");
    let historical = key(ChargeKind::Usage, "USD", "us");
    let shape = one_row_plan(vec![
        group(
            held.clone(),
            vec![interval(1, None, WindowState::Scheduled)],
        ),
        group(
            historical.clone(),
            vec![interval(1, Some(2), WindowState::Expired)],
        ),
    ]);

    let coverage = check_shape(&shape);
    assert!(coverage.find(&held).expect("held").required);
    assert!(
        !coverage.find(&historical).expect("historical").required,
        "no candidate row sits on it"
    );
    assert!(verdict(&shape).is_publishable());
}

// ---------------------------------------------------------------------------
// inst-fg-detect — the interior gap
// ---------------------------------------------------------------------------

/// `inst-fg-detect`: an interior gap is named, with `[gapStart, gapEnd)` and
/// the key.
#[test]
fn an_interior_gap_between_two_scheduled_windows_is_named() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![
            interval(1, Some(3), WindowState::Scheduled),
            interval(6, Some(9), WindowState::Scheduled),
        ],
    )]);

    let coverage = check_shape(&shape);
    assert_eq!(
        coverage
            .find(&scope_key)
            .expect("the key is in the report")
            .interior_gaps(),
        vec![(at(3), at(6))],
        "the gap is the half-open interval between the end and the next start"
    );

    let report = verdict(&shape);
    assert_eq!(codes(&report), vec![WINDOW_GAP.to_owned()]);
    assert_eq!(report.violations[0].subject, scope_key.to_string());
    assert!(
        report.violations[0].detail.contains(&format_rfc3339(at(3)))
            && report.violations[0].detail.contains(&format_rfc3339(at(6))),
        "the detail names [gapStart, gapEnd): {}",
        report.violations[0].detail
    );
}

/// The false positive the design set names explicitly (§9): adjacency.
///
/// `effectiveTo == next.effectiveFrom` leaves every instant covered exactly once
/// under the half-open reading, and it is the shape every supersession and every
/// cutover produces.
///
/// **This case alone does not prove the containment test load-bearing, and that
/// was measured rather than assumed.** With exactly two adjacent windows,
/// dropping the `!covers(end)` filter from the walk changes nothing: the strict
/// `>` in `next_start_after` refuses the boundary too, because the successor does
/// not start *after* the instant it starts at. The cases that do prove it are
/// [`adjacency_is_not_a_gap_when_a_later_window_follows`] and
/// [`overlapping_windows_do_not_open_a_gap_at_the_inner_end`] — a third window
/// gives the false gap somewhere to end. This one stays because it is §9's
/// criterion verbatim.
#[test]
fn adjacent_windows_are_not_a_gap() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![
            interval(1, Some(3), WindowState::Active),
            interval(3, Some(9), WindowState::Scheduled),
        ],
    )]);

    let coverage = check_shape(&shape);
    let entry = coverage.find(&scope_key).expect("the key is in the report");
    assert!(
        entry.covers(at(3)),
        "the boundary instant belongs to the successor, not to the predecessor"
    );
    assert_eq!(entry.interior_gaps(), Vec::new());
    assert!(verdict(&shape).is_publishable());
}

/// Adjacency is not a gap **even when a later window follows it**.
///
/// The case that makes the containment test in the walk load-bearing: with
/// `[1,3) [3,5) [8,10)` the only uncovered interval is `[5,8)`. Drop
/// `!covers(end)` and the end at `3` also becomes a gap start, reported as
/// `[3,8)` — a hole named across an instant two windows cover between them.
#[test]
fn adjacency_is_not_a_gap_when_a_later_window_follows() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![
            interval(1, Some(3), WindowState::Active),
            interval(3, Some(5), WindowState::Scheduled),
            interval(8, Some(10), WindowState::Scheduled),
        ],
    )]);

    assert_eq!(
        check_shape(&shape)
            .find(&scope_key)
            .expect("the key")
            .interior_gaps(),
        vec![(at(5), at(8))],
        "the only hole is after the adjacent pair, not inside it"
    );
    assert_eq!(codes(&verdict(&shape)), vec![WINDOW_GAP.to_owned()]);
}

/// Two overlapping windows open no gap at the **inner** end.
///
/// `[1,5) [3,7) [8,10)`: the first window's end at `5` sits inside the second, so
/// coverage runs unbroken to `7`. The only hole is `[7,8)`. Drop `!covers(end)`
/// and `[5,8)` is reported as well — a gap whose start is an instant the plan
/// covers.
#[test]
fn overlapping_windows_do_not_open_a_gap_at_the_inner_end() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![
            interval(1, Some(5), WindowState::Active),
            interval(3, Some(7), WindowState::Scheduled),
            interval(8, Some(10), WindowState::Scheduled),
        ],
    )]);

    let entry = check_shape(&shape);
    let entry = entry.find(&scope_key).expect("the key");
    assert!(entry.covers(at(5)), "the inner end is inside the overlap");
    assert_eq!(entry.interior_gaps(), vec![(at(7), at(8))]);
}

/// A window nested inside another opens no gap.
#[test]
fn nested_windows_are_not_a_gap() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![
            interval(1, Some(9), WindowState::Active),
            interval(3, Some(5), WindowState::Scheduled),
        ],
    )]);

    assert_eq!(
        check_shape(&shape)
            .find(&scope_key)
            .expect("the key")
            .interior_gaps(),
        Vec::new()
    );
}

/// An open-ended window swallows every later end, so nothing after it is a gap.
#[test]
fn an_open_ended_window_leaves_no_interior_gap() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![
            interval(1, None, WindowState::Active),
            interval(6, Some(9), WindowState::Scheduled),
        ],
    )]);

    assert_eq!(
        check_shape(&shape)
            .find(&scope_key)
            .expect("the key")
            .interior_gaps(),
        Vec::new()
    );
}

/// A void with **no successor** is not an interior gap, and this rule says
/// nothing about it.
///
/// `inst-fg-trailing`'s whole reason for existing (D-62). The assertion is that
/// the walk is silent, not that it is wrong: a coverage end in the future with
/// nothing after it is exactly the shape a cancelled successor leaves, and the
/// rule that refuses it needs W6's cycle and the D-79 subscriber lane.
#[test]
fn a_void_with_no_successor_is_not_an_interior_gap() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![interval(1, Some(3), WindowState::Active)],
    )]);

    let coverage = check_shape(&shape);
    let entry = coverage.find(&scope_key).expect("the key");
    assert_eq!(entry.coverage_end(), CoverageEnd::Ends(at(3)));
    assert_eq!(
        entry.interior_gaps(),
        Vec::new(),
        "there is no successor for the interior check to compare against"
    );
    assert!(
        verdict(&shape).is_publishable(),
        "the key holds a live window, so inst-wc-required is satisfied too"
    );
}

/// A gap between an expired window and a later scheduled one is **not** reported.
///
/// The state roster's own argument, executed: the uncovered interval lies partly
/// in the past, and `inst-ws-future-start` refuses a window that would fill it —
/// so reporting it would be a violation with no available remedy.
#[test]
fn a_void_behind_an_expired_window_is_not_an_interior_gap() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![
            interval(1, Some(2), WindowState::Expired),
            interval(6, Some(9), WindowState::Scheduled),
        ],
    )]);

    assert_eq!(
        check_shape(&shape)
            .find(&scope_key)
            .expect("the key")
            .interior_gaps(),
        Vec::new()
    );
    assert!(verdict(&shape).is_publishable());
}

/// A gap on a key no candidate row holds is still named.
///
/// Which row is current on a key says nothing about whether its interval set is
/// sound: rating resolves an instant inside the hole and finds nothing.
#[test]
fn a_gap_on_a_key_no_candidate_row_holds_is_still_named() {
    let held = key(ChargeKind::Recurring, "EUR", "eu");
    let other = key(ChargeKind::Usage, "USD", "us");
    let shape = one_row_plan(vec![
        group(held, vec![interval(1, None, WindowState::Scheduled)]),
        group(
            other.clone(),
            vec![
                interval(1, Some(3), WindowState::Scheduled),
                interval(6, Some(9), WindowState::Scheduled),
            ],
        ),
    ]);

    let report = verdict(&shape);
    assert_eq!(codes(&report), vec![WINDOW_GAP.to_owned()]);
    assert_eq!(report.violations[0].subject, other.to_string());
}

/// Two gaps on one key are both named, in `gapStart` order.
#[test]
fn every_interior_gap_on_a_key_is_named_in_order() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key,
        vec![
            interval(1, Some(2), WindowState::Active),
            interval(4, Some(5), WindowState::Scheduled),
            interval(7, Some(8), WindowState::Scheduled),
        ],
    )]);

    assert_eq!(
        codes(&verdict(&shape)),
        vec![WINDOW_GAP.to_owned(), WINDOW_GAP.to_owned()]
    );
    assert_eq!(
        check_shape(&shape).keys[0].interior_gaps(),
        vec![(at(2), at(4)), (at(5), at(7))]
    );
}

// ---------------------------------------------------------------------------
// inst-wc-availability (W5)
// ---------------------------------------------------------------------------

/// `inst-wc-availability` (W5): a purchasability interval reaching outside all
/// coverage fails publish. Dates gate purchase; they do not schedule publish.
#[test]
fn an_available_to_past_the_last_window_fails_publish() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let mut shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![interval(1, Some(9), WindowState::Scheduled)],
    )]);
    shape.available_from = Some(at(2));
    shape.available_to = Some(at(20));

    let report = verdict(&shape);
    assert_eq!(
        codes(&report),
        vec![AVAILABILITY_OUTSIDE_COVERAGE.to_owned()]
    );
    assert_eq!(report.violations[0].subject, scope_key.to_string());
    assert!(report.violations[0].detail.contains(&format_rfc3339(at(9))));
}

/// The same bound inside coverage publishes — the world that makes the refusal
/// above about the bound rather than about availability being set at all.
#[test]
fn an_available_to_inside_coverage_publishes() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let mut shape = one_row_plan(vec![group(
        scope_key,
        vec![interval(1, Some(9), WindowState::Scheduled)],
    )]);
    shape.available_from = Some(at(2));
    shape.available_to = Some(at(9));

    assert!(
        verdict(&shape).is_publishable(),
        "availableTo == the coverage end is inside coverage: both ends are exclusive"
    );
}

/// An `availableFrom` before all coverage fails publish too.
#[test]
fn an_available_from_before_all_coverage_fails_publish() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let mut shape = one_row_plan(vec![group(
        scope_key.clone(),
        vec![interval(5, Some(9), WindowState::Scheduled)],
    )]);
    shape.available_from = Some(at(2));
    shape.available_to = Some(at(9));

    let report = verdict(&shape);
    assert_eq!(
        codes(&report),
        vec![AVAILABILITY_OUTSIDE_COVERAGE.to_owned()]
    );
    assert_eq!(report.violations[0].subject, scope_key.to_string());
    assert!(report.violations[0].detail.contains(&format_rfc3339(at(2))));
}

/// An `availableFrom` inside a window the activation sweep has since moved to
/// `expired` publishes, because **both halves of this rule read one interval
/// set**.
///
/// The world is the ordinary one: a plan on sale since `availableFrom`, whose
/// first window ran out and whose successor is live. The author has no remedy for
/// a refusal here — `WINDOW_START_IN_PAST` (`inst-ws-future-start`, D-63) refuses
/// a window opened over a past instant, so the only edit that would satisfy a
/// `COVERING_STATES` reading of `availableFrom` is falsifying the date the plan
/// already published with. And `AvailableFromNotBackdated` (`inst-cs-availability`)
/// is written to **permit** exactly this re-publish: its qualifier exists so that
/// *"every later re-publish of a once-future-dated plan"* is not blocked the moment
/// the authored date passes. One rule of this pipeline permitting what another
/// refuses is the defect, not the date.
#[test]
fn an_available_from_inside_an_expired_window_publishes() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let mut shape = one_row_plan(vec![group(
        scope_key,
        vec![
            // Adjacent under the half-open reading, so `WINDOW_OVERLAP` admits
            // the pair and `interior_gaps` finds nothing between them.
            interval(1, Some(5), WindowState::Expired),
            interval(5, None, WindowState::Active),
        ],
    )]);
    shape.available_from = Some(at(2));

    assert_eq!(
        codes(&verdict(&shape)),
        Vec::<String>::new(),
        "at(2) is inside the expired interval: the key was covered when the plan \
         went on sale, and the successor covers it now"
    );
}

/// An open-ended window satisfies any `availableTo`.
#[test]
fn an_open_ended_window_satisfies_every_availability_bound() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let mut shape = one_row_plan(vec![group(
        scope_key,
        vec![interval(1, None, WindowState::Scheduled)],
    )]);
    shape.available_from = Some(at(2));
    shape.available_to = Some(at(9_000));

    assert!(verdict(&shape).is_publishable());
}

/// An uncovered key is named by `inst-wc-required` and **not** a second time by
/// the availability rule.
#[test]
fn an_uncovered_key_is_not_also_an_availability_violation() {
    let mut shape = one_row_plan(Vec::new());
    shape.available_from = Some(at(2));
    shape.available_to = Some(at(9));

    assert_eq!(
        codes(&verdict(&shape)),
        vec![WINDOW_COVERAGE_MISSING.to_owned()],
        "one remedy, one line"
    );
}

/// With neither bound set the availability rule judges nothing — W5's "(when
/// set)".
#[test]
fn a_plan_with_no_availability_bounds_is_not_judged_by_the_availability_rule() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let shape = one_row_plan(vec![group(
        scope_key,
        // Coverage starts a day in and ends: an unset `availableFrom` would fail
        // every containment test if the rule read it as "purchasable from
        // forever".
        vec![interval(5, Some(9), WindowState::Scheduled)],
    )]);

    assert!(verdict(&shape).is_publishable());
}

/// The availability rule is a conjunction over the plan's billable keys.
///
/// One covered component key does not excuse another whose coverage stops
/// earlier — a purchase binds every key of the market (D-94).
#[test]
fn one_component_key_short_of_the_availability_bound_blocks_the_plan() {
    let recurring = key(ChargeKind::Recurring, "EUR", "eu");
    let usage = key(ChargeKind::Usage, "EUR", "eu");
    let mut shape = one_row_plan(vec![
        group(
            recurring.clone(),
            vec![interval(1, None, WindowState::Scheduled)],
        ),
        group(
            usage.clone(),
            vec![interval(1, Some(5), WindowState::Scheduled)],
        ),
    ]);
    shape.billing_cycle = Some(BillingCycle::Hybrid);
    shape.rows = vec![row_on(0xb001, recurring), row_on(0xb002, usage.clone())];
    shape.available_to = Some(at(9));

    let report = verdict(&shape);
    assert_eq!(
        codes(&report),
        vec![AVAILABILITY_OUTSIDE_COVERAGE.to_owned()]
    );
    assert_eq!(report.violations[0].subject, usage.to_string());
}

// ---------------------------------------------------------------------------
// W6's term
// ---------------------------------------------------------------------------

/// W6: zero on a plan with no recurring part on that market — a one-time
/// purchase needs no forward coverage.
///
/// `Some(zero)` and not `None`: the term has a value and the value is nothing.
#[test]
fn the_margin_is_zero_where_the_market_sells_no_recurring_row() {
    let mut shape = one_row_plan(Vec::new());
    shape.billing_cycle = Some(BillingCycle::Usage);
    shape.frequency = None;
    shape.rows = vec![row_on(0xb001, key(ChargeKind::Usage, "EUR", "eu"))];

    assert_eq!(
        longest_cycle_sold(
            &shape,
            &CurrencyCode::new("EUR").expect("three letters"),
            &Region::new("eu").expect("non-blank")
        ),
        Some(time::Duration::ZERO)
    );
}

/// A market the plan does not sell recurring in is zero, even on a plan that
/// sells recurring elsewhere.
///
/// The term is per `(currency, region)`, which is the only per-key thing about
/// it.
#[test]
fn the_margin_is_per_market_and_not_per_plan_alone() {
    let mut shape = one_row_plan(Vec::new());
    shape.rows = vec![
        row_on(0xb001, key(ChargeKind::Recurring, "EUR", "eu")),
        row_on(0xb002, key(ChargeKind::Usage, "USD", "us")),
    ];

    assert_eq!(
        longest_cycle_sold(
            &shape,
            &CurrencyCode::new("USD").expect("three letters"),
            &Region::new("us").expect("non-blank")
        ),
        Some(time::Duration::ZERO)
    );
    assert_eq!(
        longest_cycle_sold(
            &shape,
            &CurrencyCode::new("EUR").expect("three letters"),
            &Region::new("eu").expect("non-blank")
        ),
        Some(time::Duration::days(31)),
        "monthly rounds up to the longest calendar month, because every consumer is a margin"
    );
}

/// Each fixed frequency contributes its calendar **maximum**.
#[test]
fn every_fixed_frequency_rounds_up_to_its_calendar_maximum() {
    let market = (
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("non-blank"),
    );
    for (frequency, days) in [
        (Frequency::Monthly, 31),
        (Frequency::Quarterly, 93),
        (Frequency::Semiannual, 186),
        (Frequency::Annual, 366),
    ] {
        let mut shape = one_row_plan(Vec::new());
        shape.frequency = Some(frequency);
        assert_eq!(
            longest_cycle_sold(&shape, &market.0, &market.1),
            Some(time::Duration::days(days)),
            "{frequency}"
        );
    }
}

/// A custom interval is read off the **variant**, never off
/// `CUSTOM_INTERVAL_PLACEHOLDER`.
///
/// The placeholder is `1`, so a reader that took it would answer one day for a
/// ninety-day cycle and one day for a two-month one.
#[test]
fn a_custom_interval_is_read_from_the_variant_and_not_the_placeholder() {
    let market = (
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("non-blank"),
    );

    let mut days = one_row_plan(Vec::new());
    days.frequency = Some(Frequency::CustomEveryN {
        n: 90,
        unit: CustomIntervalUnit::Days,
    });
    assert_eq!(
        longest_cycle_sold(&days, &market.0, &market.1),
        Some(time::Duration::days(90))
    );

    let mut months = one_row_plan(Vec::new());
    months.frequency = Some(Frequency::CustomEveryN {
        n: 2,
        unit: CustomIntervalUnit::Months,
    });
    assert_eq!(
        longest_cycle_sold(&months, &market.0, &market.1),
        Some(time::Duration::days(62))
    );

    assert_ne!(
        Frequency::CUSTOM_INTERVAL_PLACEHOLDER,
        90,
        "the placeholder is not the authored interval, which is what makes this test meaningful"
    );
}

/// A recurring market with no authored frequency has **no** margin — not a zero
/// one.
///
/// Unreachable from the publish path: `CYCLE_METADATA_MISSING` refuses a
/// `recurring`/`hybrid` plan with no frequency. Reachable for a caller elsewhere,
/// which is why the answer is `None` and not a guess.
#[test]
fn the_margin_has_no_value_when_a_recurring_market_authored_no_frequency() {
    let mut shape = one_row_plan(Vec::new());
    shape.frequency = None;

    assert_eq!(
        longest_cycle_sold(
            &shape,
            &CurrencyCode::new("EUR").expect("three letters"),
            &Region::new("eu").expect("non-blank")
        ),
        None
    );
}

// ---------------------------------------------------------------------------
// The report's shape
// ---------------------------------------------------------------------------

/// Every rule's finding names the key, and the report is ordered by it.
///
/// The order is the operator's report order and a re-run's byte stability, both
/// inherited from `group_by_key_seeded`'s promise rather than re-derived.
#[test]
fn the_report_is_ordered_by_the_keys_canonical_rendering() {
    let a = key(ChargeKind::Recurring, "EUR", "eu");
    let b = key(ChargeKind::Usage, "USD", "us");
    let straight = one_row_plan(vec![
        group(a.clone(), vec![interval(1, None, WindowState::Scheduled)]),
        group(b.clone(), vec![interval(1, None, WindowState::Scheduled)]),
    ]);
    let reversed = one_row_plan(vec![
        group(b, vec![interval(1, None, WindowState::Scheduled)]),
        group(a, vec![interval(1, None, WindowState::Scheduled)]),
    ]);

    let straight: Vec<String> = check_shape(&straight)
        .keys
        .iter()
        .map(|entry| entry.scope_key().to_string())
        .collect();
    let reversed: Vec<String> = check_shape(&reversed)
        .keys
        .iter()
        .map(|entry| entry.scope_key().to_string())
        .collect();

    let mut sorted = straight.clone();
    sorted.sort();
    assert_eq!(straight, sorted);
    assert_eq!(
        straight, reversed,
        "the input order is not the report order"
    );
}

/// A billable key the window plane does not mention gets an entry, uncovered.
///
/// `check` does the seeding itself rather than trusting the caller's: if "this
/// key is uncovered" were the *absence* of an entry, a caller that forgot to seed
/// would silently lose the finding.
#[test]
fn a_billable_key_the_plane_does_not_mention_is_present_and_uncovered() {
    let scope_key = key(ChargeKind::Recurring, "EUR", "eu");
    let coverage = check_shape(&one_row_plan(Vec::new()));

    let entry = coverage.find(&scope_key).expect("seeded from the row");
    assert!(entry.required);
    assert!(entry.windows.intervals.is_empty());
    assert_eq!(entry.coverage_end(), CoverageEnd::Uncovered);
}
