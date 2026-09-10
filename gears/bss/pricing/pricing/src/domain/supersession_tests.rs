//! The changeover instant's two floors, and the two window operations composed
//! against it.
//!
//! Every case is driven from **fixed** instants rather than `OffsetDateTime::now_utc()`, and the
//! reason is the one D-194's case records one plane over: a rule about the distance
//! between two instants, tested against a clock that moves, passes or fails by how
//! long the test took. `now` is a parameter here precisely so it can be a fact.
//!
//! The compose cases below carry more assertions than their line count suggests in
//! one place: the **adjacency** case reconstructs the shortened predecessor and asks
//! `covers` of both intervals on either side of the changeover. Asserting only the
//! two instants — that the end equals the successor's start — would pass equally
//! under an inclusive end, which is exactly the false positive §9 names and would
//! double-cover the changeover instant.

use uuid::Uuid;

use super::{
    ChangeoverMoment, MAX_BATCHING_DELAY, NamedWindow, check_changeover_instant, compose_windows,
};
use crate::domain::error::DomainError;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::window::{WindowInterval, WindowState};
use time::Duration;
use time::OffsetDateTime;

/// A fixed reading of "now" for every case below.
fn now() -> OffsetDateTime {
    utc_ymd_hms(2099, 3, 1, 12, 0, 0)
}

#[test]
fn at_submit_the_instant_must_only_be_strictly_future() {
    // `inst-su-instant`'s first floor. A submit is not a commit: the batching delay
    // has not started running, and holding a submit to the commit floor would
    // refuse a unit that will be perfectly legal by the time it is approved.
    check_changeover_instant(now() + MAX_BATCHING_DELAY, now(), ChangeoverMoment::Submit)
        .expect("well clear of both floors");
    check_changeover_instant(
        now() + time::Duration::milliseconds(1),
        now(),
        ChangeoverMoment::Submit,
    )
    .expect("one quantum into the future is strictly future, which is all submit asks");
}

#[test]
fn at_submit_now_itself_and_anything_behind_it_is_refused() {
    // Strictly future, so the boundary is exclusive: an instant equal to `now` is
    // an instant that has arrived.
    for behind in [
        time::Duration::ZERO,
        time::Duration::milliseconds(1),
        time::Duration::days(30),
    ] {
        let err = check_changeover_instant(now() - behind, now(), ChangeoverMoment::Submit)
            .expect_err("an instant at or behind now has passed");
        let DomainError::SupersessionInstantPassed(detail) = err else {
            panic!("the refusal is SUPERSESSION_INSTANT_PASSED, got: {err:?}");
        };
        assert!(
            detail.contains("changeover"),
            "the refusal names the field, got: {detail}"
        );
    }
}

#[test]
fn at_commit_the_instant_must_clear_the_whole_batching_delay() {
    // `inst-su-instant`'s second floor, and the money reason for it: an instant
    // inside the batching lag activates the successor's window while its row is not
    // yet addressable at any completed `CatalogVersion`, so renewals and arrears
    // fail closed for up to the whole delay.
    check_changeover_instant(
        now() + MAX_BATCHING_DELAY + time::Duration::milliseconds(1),
        now(),
        ChangeoverMoment::Commit,
    )
    .expect("clear of the delay");

    // The instant a submit would have accepted, refused at commit — which is the
    // whole point of there being two moments.
    let err = check_changeover_instant(
        now() + time::Duration::milliseconds(1),
        now(),
        ChangeoverMoment::Commit,
    )
    .expect_err("strictly future is not enough at commit");
    let DomainError::SupersessionInstantPassed(detail) = err else {
        panic!("the refusal is SUPERSESSION_INSTANT_PASSED, got: {err:?}");
    };
    assert!(
        detail.contains("recompose"),
        "the refusal names the remedy the design set gives it, got: {detail}"
    );
}

#[test]
fn the_commit_floor_is_exclusive_at_exactly_the_batching_delay() {
    // "**at least** the max batching delay in the future" against a delay that is
    // itself a maximum: an instant exactly one delay away is one the last batch of
    // that window can still miss. Refused, and the boundary is pinned because an
    // off-by-one here is a fail-closed renewal rather than a refused request.
    let err = check_changeover_instant(now() + MAX_BATCHING_DELAY, now(), ChangeoverMoment::Commit)
        .expect_err("exactly one delay away does not clear the delay");
    assert!(matches!(err, DomainError::SupersessionInstantPassed(_)));
}

#[test]
fn the_delay_is_the_ratified_five_minutes() {
    // D-47's ratified maximum, asserted against the literal rather than against
    // itself: the two floors are the only place this number is a *rule* rather than an
    // alarm threshold, and a silent change to it moves a money boundary.
    //
    // The message half of this case moved to
    // `the_commit_remedy_states_the_delay_from_the_constant_and_not_from_a_copy`, which
    // derives the expected number instead of accepting either of two spellings — the
    // `||` that used to be here would have stayed green with the constant changed
    // (review, 2026-08-05).
    assert_eq!(MAX_BATCHING_DELAY, time::Duration::minutes(5));
}

// ---------------------------------------------------------------------------
// Composing the two window operations (`inst-su-compose`).
// ---------------------------------------------------------------------------

fn window_id(n: u128) -> Uuid {
    Uuid::from_u128(0x00d0_0000 ^ n)
}

/// The changeover every case below composes against: well clear of both floors.
fn changeover() -> OffsetDateTime {
    now() + Duration::days(30)
}

fn named(n: u128, interval: WindowInterval) -> NamedWindow {
    NamedWindow {
        window_id: window_id(n),
        interval,
    }
}

/// The ordinary world: one active open-ended window carrying the key's coverage.
fn live_open_ended() -> NamedWindow {
    named(
        1,
        WindowInterval::new(now() - Duration::days(90), None, WindowState::Active),
    )
}

#[test]
fn compose_shortens_the_covering_window_and_opens_the_successor_at_the_changeover() {
    let composed = compose_windows(&[live_open_ended()], changeover())
        .expect("an open-ended active window is the ordinary case");

    assert_eq!(composed.shorten.window_id, window_id(1));
    assert_eq!(composed.shorten.effective_to, changeover());
    assert_eq!(composed.successor.effective_from, changeover());
    assert_eq!(
        composed.successor.effective_to, None,
        "`inst-su-compose` schedules the successor as open-ended from the changeover"
    );
    assert_eq!(composed.successor.state, WindowState::Scheduled);
}

#[test]
fn the_two_intervals_meet_exactly_so_no_instant_is_covered_twice_or_not_at_all() {
    // The half-open interval is the whole of the adjacency rule: a window does not
    // cover its own `effectiveTo`, so `effectiveTo = successor.effectiveFrom` leaves
    // the changeover instant covered exactly once. This is the property
    // `inst-su-compose` promises — neither WINDOW_OVERLAP nor WINDOW_TRAILING_VOID
    // can arise from a committed unit — asserted rather than assumed.
    let composed = compose_windows(&[live_open_ended()], changeover()).expect("compose");
    let predecessor = WindowInterval::new(
        live_open_ended().interval.effective_from,
        Some(composed.shorten.effective_to),
        WindowState::Active,
    );

    assert!(!predecessor.covers(changeover()), "the old row stops here");
    assert!(
        composed.successor.covers(changeover()),
        "the new one starts"
    );
    let one_before = changeover() - Duration::milliseconds(1);
    assert!(predecessor.covers(one_before));
    assert!(!composed.successor.covers(one_before));
}

#[test]
fn a_window_with_a_future_end_is_shortened_to_the_changeover_and_keeps_its_start() {
    let bounded = named(
        1,
        WindowInterval::new(
            now() - Duration::days(90),
            Some(now() + Duration::days(365)),
            WindowState::Active,
        ),
    );
    let composed = compose_windows(&[bounded], changeover()).expect("compose");

    assert_eq!(composed.shorten.effective_to, changeover());
    assert_eq!(
        composed.successor.effective_to, None,
        "the successor is open-ended even where its predecessor was not: the \
         predecessor's end was a fact about the old price, not about the key"
    );
}

#[test]
fn a_key_whose_coverage_has_already_ended_is_not_supersedable() {
    // `inst-su-compose`: a key with no window covering the changeover is dormant,
    // and the unit presupposes current coverage. The design set's remedy is a plain
    // publish plus a window schedule — a revival, not a supersession.
    let expired = named(
        1,
        WindowInterval::new(
            now() - Duration::days(90),
            Some(now() - Duration::days(1)),
            WindowState::Expired,
        ),
    );

    let err = compose_windows(&[expired], changeover())
        .expect_err("coverage ended yesterday; there is nothing to shorten");
    let DomainError::SupersessionKeyDormant(detail) = err else {
        panic!(
            "a dormant key is its own refusal — it names a remedy, so D-146 keeps it out of \
                LifecycleForbidden — got: {err:?}"
        );
    };
    assert!(detail.contains("dormant"), "got: {detail}");
    assert!(
        detail.contains("publish"),
        "the refusal names the revival path the design set gives it, got: {detail}"
    );
}

#[test]
fn a_key_with_no_windows_at_all_is_dormant_too_and_says_the_same_thing() {
    let err = compose_windows(&[], changeover()).expect_err("no coverage at all");
    assert!(
        matches!(err, DomainError::SupersessionKeyDormant(_)),
        "got: {err:?}"
    );
}

#[test]
fn a_cancelled_window_is_not_coverage_even_where_its_interval_would_cover() {
    // A cancelled window is a schedule that never happened, which is why D-121
    // keeps it out of the read model and why `OCCUPYING_STATES` excludes it. Its
    // interval still spans the changeover, so a compose reading intervals without
    // states would shorten a window that carries nothing.
    let cancelled = named(
        1,
        WindowInterval::new(now() - Duration::days(90), None, WindowState::Cancelled),
    );

    let err = compose_windows(&[cancelled], changeover())
        .expect_err("a cancelled window is not this key's coverage");
    assert!(
        matches!(err, DomainError::SupersessionKeyDormant(_)),
        "got: {err:?}"
    );
}

#[test]
fn a_scheduled_window_that_covers_the_changeover_is_coverage() {
    // The key's coverage has not started yet and the changeover falls inside it.
    // Repricing before the window opens is a supersession like any other: the
    // covering window is `scheduled`, its start is untouched, its end moves.
    let ahead = named(
        1,
        WindowInterval::new(now() + Duration::days(7), None, WindowState::Scheduled),
    );
    let composed = compose_windows(&[ahead], changeover()).expect("scheduled coverage counts");

    assert_eq!(composed.shorten.window_id, window_id(1));
    assert_eq!(composed.shorten.effective_to, changeover());
}

#[test]
fn a_window_beginning_at_or_after_the_changeover_would_collide_with_the_successor() {
    // The successor is open-ended, so anything on the key starting at or after the
    // changeover is inside it. `inst-su-compose` promises a committed unit can
    // never produce WINDOW_OVERLAP, which means compose is where it is produced.
    let later = named(
        2,
        WindowInterval::new(
            changeover() + Duration::days(1),
            None,
            WindowState::Scheduled,
        ),
    );

    let err = compose_windows(&[live_open_ended(), later], changeover())
        .expect_err("the successor's open-ended interval would swallow the later window");
    let DomainError::WindowOverlap(detail) = err else {
        panic!("a collision is WINDOW_OVERLAP, got: {err:?}");
    };
    assert!(
        detail.contains(&window_id(2).to_string()),
        "the refusal names the window it would collide with, got: {detail}"
    );
}

#[test]
fn a_window_beginning_exactly_at_the_changeover_collides_too() {
    // The boundary case of the one above, and the direction matters: the successor
    // covers its own `effectiveFrom`, so a sibling starting on that instant is a
    // genuine double cover rather than an adjacency.
    let coincident = named(
        2,
        WindowInterval::new(changeover(), None, WindowState::Scheduled),
    );

    let err = compose_windows(&[live_open_ended(), coincident], changeover())
        .expect_err("two windows claiming the changeover instant");
    assert!(matches!(err, DomainError::WindowOverlap(_)));
}

#[test]
fn history_behind_the_changeover_is_left_alone() {
    // Expired windows earlier on the key are the key's history and are immutable;
    // they neither supply coverage at the changeover nor collide with the
    // successor, so compose must ignore them rather than refuse over them.
    let history = named(
        3,
        WindowInterval::new(
            now() - Duration::days(400),
            Some(now() - Duration::days(90)),
            WindowState::Expired,
        ),
    );
    let composed = compose_windows(&[history, live_open_ended()], changeover())
        .expect("history does not block a supersession");

    assert_eq!(composed.shorten.window_id, window_id(1));
}

#[test]
fn a_cancelled_window_after_the_changeover_does_not_collide_with_the_successor() {
    // The state filter's *other* load-bearing case, and the one a removal probe over
    // the covering half does not reach. A cancelled window is a schedule that never
    // happened, so a cancelled future interval is not coverage this supersession
    // would be replacing — refusing over it would make a key unsupersedable because
    // of an act somebody already took back.
    let cancelled_ahead = named(
        2,
        WindowInterval::new(
            changeover() + Duration::days(1),
            None,
            WindowState::Cancelled,
        ),
    );
    let composed = compose_windows(&[live_open_ended(), cancelled_ahead], changeover())
        .expect("a cancelled future window is not a collision");

    assert_eq!(composed.shorten.window_id, window_id(1));
}

// ---------------------------------------------------------------------------
// The whole compose-time judgement, and the order it answers in.
// ---------------------------------------------------------------------------

use super::{SupersessionPlan, plan_supersession};
use crate::domain::money::RateMinor;
use crate::domain::price_row::{
    BillingGranularity, ModelKind, PriceRow, TierAggregationWindow, TierBand,
};
use crate::domain::scope_key::ChargeKind;

/// A tiered usage predecessor — the shape with unit fields for the guard to have
/// an opinion about.
fn usage_row(amount: i64, granularity: BillingGranularity) -> PriceRow {
    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    row.bands = vec![
        TierBand::closed(0, 100, RateMinor::from_minor_units(0).unwrap()),
        TierBand::open(100, RateMinor::from_minor_units(amount).unwrap()),
    ];
    row.meter = Some("api_calls".to_owned());
    "region:eu".clone_into(&mut row.dimension_key);
    row.billing_granularity = Some(granularity);
    // **A reset window, because `plan_supersession` now runs the row-local set too.**
    // These fixtures were built for the unit *guard*, which compares two rows and has no
    // opinion about whether either is publishable — so they carried only the fields the
    // guard reads. `inst-su-compose` clause (a) says the successor is judged by the S3
    // rules as well, and `inst-tb-window` is one of them: a tiered usage row without a
    // reset window has a counter that never resets. The fixtures reddening when that set
    // was wired in is the rule working, not the fixtures going stale (2026-08-06).
    row.tier_aggregation_window = Some(TierAggregationWindow::CalendarMonth);
    row
}

fn predecessor() -> PriceRow {
    usage_row(25, BillingGranularity::PerHour)
}

/// A pure price change: new band rate, every unit field preserved.
fn priced_differently() -> PriceRow {
    usage_row(30, BillingGranularity::PerHour)
}

/// The ×24 class: the same accumulated counter re-read in a different unit.
fn denominated_differently() -> PriceRow {
    usage_row(30, BillingGranularity::PerDay)
}

#[test]
fn a_pure_price_change_on_a_live_key_plans() {
    let plan = plan_supersession(
        &predecessor(),
        &priced_differently(),
        &[live_open_ended()],
        changeover(),
        now(),
        ChangeoverMoment::Submit,
    )
    .expect("a new band rate on a covered key is the ordinary supersession");

    assert_eq!(plan.changeover, changeover());
    assert_eq!(plan.windows.shorten.window_id, window_id(1));
    assert_eq!(plan.windows.successor.effective_from, changeover());
}

#[test]
fn a_successor_that_re_denominates_the_counter_is_refused_by_the_unit_guard() {
    let err = plan_supersession(
        &predecessor(),
        &denominated_differently(),
        &[live_open_ended()],
        changeover(),
        now(),
        ChangeoverMoment::Submit,
    )
    .expect_err("per_hour -> per_day applies an hours-denominated Q to day bands");

    let DomainError::ValidationFailed(report) = err else {
        panic!("the guard reports violations, got: {err:?}");
    };
    assert!(!report.is_publishable());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.code == crate::domain::rules::SUPERSESSION_UNIT_MISMATCH),
        "got: {:?}",
        report.violations
    );
}

#[test]
fn the_instant_floor_is_answered_before_the_world_is_read() {
    // A request whose instant has passed AND whose key is dormant AND whose
    // successor re-denominates the counter. The operator hears about the instant,
    // because it is the only one of the three that is wrong about the *request*
    // rather than about what the request would do — and the design set's own remedy
    // for it, recomposition, is what produces a request the other two can even be
    // asked of.
    let err = plan_supersession(
        &predecessor(),
        &denominated_differently(),
        &[],
        now() - Duration::days(1),
        now(),
        ChangeoverMoment::Submit,
    )
    .expect_err("everything is wrong with this one");

    assert!(
        matches!(err, DomainError::SupersessionInstantPassed(_)),
        "the instant is answered first, got: {err:?}"
    );
}

#[test]
fn the_worlds_shape_is_answered_before_the_successors_content() {
    // A dormant key and a re-denominating successor. The operator hears "dormant",
    // because dormancy decides whether a *supersession* is the right operation at
    // all — the remedy is a publish plus a window schedule, an entirely different
    // act — while the guard's answer presupposes there is a supersession to do and
    // asks only whether this one is shaped right. Validating the content of an act
    // that must not be attempted tells someone to fix a payload they should not
    // send.
    let err = plan_supersession(
        &predecessor(),
        &denominated_differently(),
        &[],
        changeover(),
        now(),
        ChangeoverMoment::Submit,
    )
    .expect_err("dormant and mis-denominated at once");

    let DomainError::SupersessionKeyDormant(detail) = err else {
        panic!("dormancy is answered before content, got: {err:?}");
    };
    assert!(detail.contains("dormant"), "got: {detail}");
}

#[test]
fn the_commit_moment_holds_the_same_plan_to_the_stricter_floor() {
    // `inst-su-compose`: validated at compose **and** re-validated at commit. The
    // world and the content are re-read identically; the only thing that differs
    // between the two moments is the floor, which is what makes a plan that
    // submitted legally refusable at commit without anything else having moved.
    let barely_ahead = now() + Duration::minutes(1);

    plan_supersession(
        &predecessor(),
        &priced_differently(),
        &[live_open_ended()],
        barely_ahead,
        now(),
        ChangeoverMoment::Submit,
    )
    .expect("a minute ahead is strictly future");

    let err = plan_supersession(
        &predecessor(),
        &priced_differently(),
        &[live_open_ended()],
        barely_ahead,
        now(),
        ChangeoverMoment::Commit,
    )
    .expect_err("a minute ahead is inside the batching delay");
    assert!(matches!(err, DomainError::SupersessionInstantPassed(_)));
}

#[test]
fn the_plan_carries_the_changeover_so_the_commit_can_recompose_from_it() {
    // The window operations are **not** written at compose — `inst-su-commit` applies
    // both at commit — so what the unit has to survive on is the instant. A plan that
    // carried only the composed intervals would leave the commit re-validating
    // against numbers it could not re-derive if the plane had moved.
    let plan: SupersessionPlan = plan_supersession(
        &predecessor(),
        &priced_differently(),
        &[live_open_ended()],
        changeover(),
        now(),
        ChangeoverMoment::Submit,
    )
    .expect("plan");

    assert_eq!(plan.changeover, changeover());
    assert_eq!(plan.windows.shorten.effective_to, plan.changeover);
    assert_eq!(plan.windows.successor.effective_from, plan.changeover);
}

#[test]
fn a_changeover_at_the_covering_windows_own_start_is_refused_on_its_own_terms() {
    // Found by review: `covers` is **inclusive** at the start, so a window beginning
    // exactly at the changeover both covers it and matches the collision predicate.
    // The covering window is therefore NOT excluded from that predicate "by
    // construction", and before this arm existed the caller was told the key "carries
    // later coverage that this supersession would not replace" — naming a sibling that
    // does not exist and sending an operator hunting for it.
    //
    // Refusing is right either way: shortening a window to its own start leaves an
    // empty interval, so there is no predecessor coverage left to hand over. What was
    // wrong was the diagnosis.
    let at_the_boundary = named(
        1,
        WindowInterval::new(changeover(), None, WindowState::Scheduled),
    );

    let err = compose_windows(&[at_the_boundary], changeover())
        .expect_err("shortening a window to its own start empties it");
    let DomainError::SupersessionWouldEmptyWindow(detail) = err else {
        panic!("this is a refused state of the key, not a collision, got: {err:?}");
    };
    assert!(
        detail.contains(&window_id(1).to_string()),
        "the refusal names the window it is about, got: {detail}"
    );
    assert!(
        !detail.contains("later coverage"),
        "and it does NOT claim a sibling that is not there, got: {detail}"
    );
}

#[test]
fn a_changeover_finer_than_the_millisecond_quantum_is_refused_at_compose() {
    // Found by review. The quantum is the store's rule and both write paths do refuse
    // a non-quantized instant — but the changeover's first arrival at the store is
    // inside the commit transaction, after two people have approved. So a microsecond
    // in the payload survives compose at submit, survives it again at commit, and dies
    // mid-commit as a storage refusal: a caller mistake reported at the one moment the
    // caller can no longer fix it, on the very path whose sibling refusal
    // (`SUPERSESSION_INSTANT_PASSED`) exists to avoid wasting a recomposition.
    //
    // This adds no second owner of the rule: `check_quantum` is `is_quantized`
    // consulted, the same arrangement `check_creation` already uses for emptiness.
    let ragged = changeover() + Duration::microseconds(1);
    let err = plan_supersession(
        &predecessor(),
        &priced_differently(),
        &[live_open_ended()],
        ragged,
        now(),
        ChangeoverMoment::Submit,
    )
    .expect_err("a sub-millisecond changeover is not storable");
    assert!(
        matches!(err, DomainError::TimestampPrecisionExceeded(_)),
        "got: {err:?}"
    );

    // And it is answered before the floor, because a malformed instant is not an
    // instant whose distance is worth comparing.
    let ragged_and_passed = now() - Duration::days(1) + Duration::microseconds(1);
    let err = plan_supersession(
        &predecessor(),
        &priced_differently(),
        &[live_open_ended()],
        ragged_and_passed,
        now(),
        ChangeoverMoment::Submit,
    )
    .expect_err("both wrong");
    assert!(
        matches!(err, DomainError::TimestampPrecisionExceeded(_)),
        "the malformed value is answered before its distance, got: {err:?}"
    );
}

#[test]
fn a_key_whose_coverage_ends_exactly_at_the_changeover_is_dormant_at_it() {
    // The mirror of `a_window_beginning_exactly_at_the_changeover_collides_too`, and the
    // case the existing dormancy test cannot isolate: that one uses an `Expired` window
    // whose end is in the past, so it fails the interval test *and* the state test.
    //
    // Here the window is `Active`, its end is in the **future**, and it is adjacent-legal
    // — so the only thing deciding the answer is `covers`'s **exclusive** end. A key
    // covered right up to the changeover does not cover the changeover, so there is
    // nothing to shorten and no coverage to hand over; the successor would open at an
    // instant the predecessor had already stopped covering, which is a trailing void
    // rather than a supersession.
    let ends_at_the_changeover = named(
        1,
        WindowInterval::new(
            now() - Duration::days(90),
            Some(changeover()),
            WindowState::Active,
        ),
    );

    let err = compose_windows(&[ends_at_the_changeover], changeover())
        .expect_err("coverage that stops at the changeover does not cover it");
    let DomainError::SupersessionKeyDormant(detail) = err else {
        panic!("this is dormancy at that instant, got: {err:?}");
    };
    assert!(
        detail.contains("dormant"),
        "and it is reported as dormancy, got: {detail}"
    );
}

#[test]
fn the_commit_remedy_states_the_delay_from_the_constant_and_not_from_a_copy() {
    // The module doc argues two paragraphs above that a hand-maintained second copy of
    // the SLO is how two mechanisms come to disagree about it — and the remedy sentence
    // was a hand-maintained second copy. Worse, the case guarding it asserted
    // `contains("300s") || contains("5 min")`, an `||`, so changing the constant left the
    // message lying and the suite green. Found by review, 2026-08-05.
    //
    // Both halves are now derived: the sentence formats `MAX_BATCHING_DELAY`, and this
    // asserts the formatted number rather than either literal.
    let err = check_changeover_instant(now(), now(), ChangeoverMoment::Commit)
        .expect_err("now is not a batching delay from now");
    let DomainError::SupersessionInstantPassed(detail) = err else {
        panic!("got: {err:?}");
    };
    let seconds = MAX_BATCHING_DELAY.whole_seconds();
    assert!(
        detail.contains(&format!("{seconds}s")),
        "the operator has to be told how far ahead is far enough, from the constant \
         itself, got: {detail}"
    );
}
