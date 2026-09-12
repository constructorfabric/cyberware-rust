//! Unit cases for [`super`] — D-51's kept-vs-cancelled split, D-131's per-price-id
//! map, D-182's absent lane, and `inst-re-references`' two weights.

use uuid::Uuid;

use super::{
    BlockingReferenceKind, GenerationCoverage, KeptReason, PlanReference, PresenceMap,
    ReferenceReport, ScheduledWindow, WarningReferenceKind, WindowDisposition, WindowVerdict,
    dispose_windows, strand_free_disposition,
};
use crate::domain::error::DomainError;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::money::CurrencyCode;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::window::{WindowInterval, WindowState};
use time::OffsetDateTime;

fn window(price_id: Uuid) -> ScheduledWindow {
    ScheduledWindow {
        window_id: Uuid::now_v7(),
        price_id,
    }
}

fn reference(label: &str) -> PlanReference {
    PlanReference {
        referrer_id: Uuid::now_v7(),
        referrer_label: label.to_owned(),
    }
}

#[test]
fn a_key_with_no_in_flight_subscribers_has_its_scheduled_window_cancelled() {
    let unoccupied = Uuid::now_v7();
    let verdicts = dispose_windows(&[window(unoccupied)], &PresenceMap::resolved([]));

    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].disposition, WindowDisposition::Cancelled);
    assert!(verdicts[0].is_cancelled());
}

#[test]
fn a_key_with_in_flight_subscribers_keeps_its_continuing_coverage_window() {
    // D-51: cancelling it opens the trailing void no gap check can see.
    let occupied = Uuid::now_v7();
    let verdicts = dispose_windows(&[window(occupied)], &PresenceMap::resolved([occupied]));

    assert_eq!(
        verdicts[0].disposition,
        WindowDisposition::Kept(KeptReason::InFlightSubscribers)
    );
    assert!(!verdicts[0].is_cancelled());
}

#[test]
fn a_plan_whose_keys_are_mixed_cancels_exactly_the_unoccupied_ones() {
    // D-131's whole argument: a union **count** answers only "does this plan have
    // any subscriber at all", under which this case cancels nothing.
    let occupied = Uuid::now_v7();
    let unoccupied_a = Uuid::now_v7();
    let unoccupied_b = Uuid::now_v7();
    let scheduled = [
        window(occupied),
        window(unoccupied_a),
        window(unoccupied_b),
        // A second window on the occupied key - the map is keyed by price id, so
        // both of its windows are kept.
        window(occupied),
    ];

    let verdicts = dispose_windows(&scheduled, &PresenceMap::resolved([occupied]));

    let cancelled: Vec<_> = verdicts
        .iter()
        .filter(|v| v.is_cancelled())
        .map(|v| v.price_id)
        .collect();
    assert_eq!(cancelled, vec![unoccupied_a, unoccupied_b]);
    assert_eq!(verdicts.iter().filter(|v| !v.is_cancelled()).count(), 2);
}

#[test]
fn an_unanswerable_lane_keeps_every_window_and_says_which_kind_of_kept() {
    // D-131's fail-closed clause, which D-182 makes this system's only case: the
    // D-79 lane has no client, so every key reads occupied and retirement
    // declines to cancel rather than cancelling blind.
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let verdicts = dispose_windows(&[window(a), window(b)], &PresenceMap::fail_closed());

    assert!(
        verdicts
            .iter()
            .all(|v| v.disposition == WindowDisposition::Kept(KeptReason::PresenceUnresolved))
    );
    assert_eq!(verdicts.iter().filter(|v| v.is_cancelled()).count(), 0);
}

#[test]
fn the_fail_closed_map_reports_itself_so_the_preview_can_label_why() {
    // `inst-re-warn`: "kept because nobody could tell us" and "kept because a
    // subscriber is on it" are different facts for the operator confirming.
    assert!(PresenceMap::fail_closed().is_fail_closed());
    assert!(!PresenceMap::resolved([]).is_fail_closed());
    assert!(PresenceMap::fail_closed().is_occupied(Uuid::now_v7()));
    assert!(!PresenceMap::resolved([]).is_occupied(Uuid::now_v7()));
}

#[test]
fn no_scheduled_windows_is_no_verdicts_rather_than_a_refusal() {
    assert!(dispose_windows(&[], &PresenceMap::fail_closed()).is_empty());
}

#[test]
fn a_bundle_component_reference_blocks_the_retirement_and_is_enumerated() {
    let plan_id = Uuid::now_v7();
    let bundle = reference("Starter Suite");
    let report = ReferenceReport {
        blocking: vec![(BlockingReferenceKind::BundleComponent, bundle.clone())],
        warnings: Vec::new(),
    };

    let err = report.ensure_retirable(plan_id).unwrap_err();
    let DomainError::RetirePlanReferenced(detail) = err else {
        panic!("expected RetirePlanReferenced, got {err:?}");
    };
    assert!(detail.contains("bundle component"), "{detail}");
    assert!(detail.contains("Starter Suite"), "{detail}");
    assert!(detail.contains(&bundle.referrer_id.to_string()), "{detail}");
}

#[test]
fn an_add_on_price_override_target_blocks_too() {
    let report = ReferenceReport {
        blocking: vec![(
            BlockingReferenceKind::AddOnPriceOverrideTarget,
            reference("Support Add-on"),
        )],
        warnings: Vec::new(),
    };

    let err = report.ensure_retirable(Uuid::now_v7()).unwrap_err();
    let DomainError::RetirePlanReferenced(detail) = err else {
        panic!("expected RetirePlanReferenced, got {err:?}");
    };
    assert!(detail.contains("add-on price-override target"), "{detail}");
}

#[test]
fn warnings_alone_never_block_a_retirement() {
    // D-24 and D-31 both: the change-target edge goes inert (Subscriptions
    // re-checks lifecycle state at change time) and the overlay stays evaluable
    // for in-flight subscribers, so neither is a reason to refuse.
    let report = ReferenceReport {
        blocking: Vec::new(),
        warnings: vec![
            (WarningReferenceKind::AllowedChangeTarget, reference("Pro")),
            (
                WarningReferenceKind::PriceOverlayTarget,
                reference("EMEA Q3 discount"),
            ),
        ],
    };

    assert!(report.ensure_retirable(Uuid::now_v7()).is_ok());
}

#[test]
fn a_plan_nothing_refers_to_retires() {
    assert!(
        ReferenceReport::default()
            .ensure_retirable(Uuid::now_v7())
            .is_ok()
    );
}

#[test]
fn every_blocking_referrer_is_named_not_just_the_first() {
    // "1 of 3 things is in the way" is not an actionable sentence, and an
    // operator who fixes the named one comes back to the same refusal twice.
    let report = ReferenceReport {
        blocking: vec![
            (BlockingReferenceKind::BundleComponent, reference("Suite A")),
            (BlockingReferenceKind::BundleComponent, reference("Suite B")),
            (
                BlockingReferenceKind::AddOnPriceOverrideTarget,
                reference("Support"),
            ),
        ],
        warnings: Vec::new(),
    };

    let err = report.ensure_retirable(Uuid::now_v7()).unwrap_err();
    let DomainError::RetirePlanReferenced(detail) = err else {
        panic!("expected RetirePlanReferenced, got {err:?}");
    };
    for expected in ["Suite A", "Suite B", "Support"] {
        assert!(
            detail.contains(expected),
            "{expected} missing from {detail}"
        );
    }
}

// ---------------------------------------------------------------------------
// D-04's bound over what a retirement's cancellations would leave (D-316's
// residual). The two arms below have no reachable integration shape: the
// indefinite generation and the valueless margin are both authored past the
// publish door rather than through it, so a hand-built operand is the only way
// to pin them.
// ---------------------------------------------------------------------------

fn generation_key(cohort: OffsetDateTime) -> ScopeKey {
    ScopeKey::new(
        PlanId::new(Uuid::now_v7()),
        CurrencyCode::new("USD").expect("currency"),
        Region::new("us").expect("region"),
        PhaseId::new(Uuid::from_u128(0x9ba5e)),
        PriceEligibility::ExistingGrandfathered,
        ChargeKind::Recurring,
        Cohort::Generation(cohort),
    )
    .expect("scope key")
}

fn at(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 8, day, 0, 0, 0)
}

/// One condemned window on a generation, and the verdict list naming it.
fn condemned_on(window_id: Uuid) -> Vec<WindowVerdict> {
    vec![WindowVerdict {
        window_id,
        price_id: Uuid::now_v7(),
        disposition: WindowDisposition::Cancelled,
    }]
}

#[test]
fn an_indefinite_generations_window_is_kept_because_no_finite_end_answers_it() {
    // D-147: a grandfathered row with a null `grandfatherUntil` is indefinite, so
    // the walk is asked for coverage that never ends. Cancelling the only window
    // leaves the span uncovered from the anchor onward, whatever the horizon
    // would have said - and "no horizon" must never be read as "a horizon of
    // now", which is the direction a money bound cannot round in.
    let window_id = Uuid::now_v7();
    let generation = GenerationCoverage {
        scope_key: generation_key(at(4)),
        windows: vec![(
            window_id,
            WindowInterval::new(at(4), None, WindowState::Scheduled),
        )],
        grandfather_until: None,
        margin: Some(time::Duration::days(31)),
    };

    let mut verdicts = condemned_on(window_id);
    strand_free_disposition(&mut verdicts, &[generation], at(5));

    assert_eq!(
        verdicts[0].disposition,
        WindowDisposition::Kept(KeptReason::GrandfatheredCoverageBound)
    );
}

#[test]
fn a_generation_whose_margin_has_no_value_is_kept_rather_than_read_as_zero() {
    // W6's third case: the market sells recurring and the plan authored no
    // frequency, so the term has **no value**. That is not `Some(zero)` - the
    // floor cannot be computed at all, so no interval set can be *shown* to reach
    // it, and this surface answers by keeping the coverage where the window
    // surface answers by refusing the act.
    let window_id = Uuid::now_v7();
    let generation = GenerationCoverage {
        scope_key: generation_key(at(4)),
        windows: vec![(
            window_id,
            // Open-ended, so a rule that read `None` as a zero margin would find
            // the span satisfied and cancel.
            WindowInterval::new(at(4), None, WindowState::Scheduled),
        )],
        grandfather_until: Some(at(20)),
        margin: None,
    };

    let mut verdicts = condemned_on(window_id);
    strand_free_disposition(&mut verdicts, &[generation], at(5));

    assert_eq!(
        verdicts[0].disposition,
        WindowDisposition::Kept(KeptReason::GrandfatheredCoverageBound)
    );
}

/// **An unrepresentable horizon is the same absence, not a panic.**
///
/// `grandfather_until` is operator-authored and stored, and
/// `infra::storage::repo::check_authored_instant` bounds it only for millisecond
/// precision — never for range. chrono's `impl Add<time::Duration> for DateTime<Tz>` is
/// `checked_add_signed(..).expect(..)`, so `horizon + margin` aborted a **release**
/// build as readily as a debug one, reachable from `PLAN_RETIRE` through both
/// `preview` and the commit arm (review 2026-08-19). The fallible add folds into
/// `BoundSpan::Incomputable`, which the caller already reads as keep — the same
/// answer `sellability::active_window_with_horizon` gives the same operand.
///
/// Open-ended window, deliberately: a rule that treated the failed add as "no
/// bound to reach" would find the span satisfied and cancel, so keeping is a real
/// assertion here rather than the only outcome available.
#[test]
fn a_generation_whose_horizon_plus_margin_is_unrepresentable_is_kept_not_a_panic() {
    let window_id = Uuid::now_v7();
    let generation = GenerationCoverage {
        scope_key: generation_key(at(4)),
        windows: vec![(
            window_id,
            WindowInterval::new(at(4), None, WindowState::Scheduled),
        )],
        grandfather_until: Some(crate::domain::instant::max_utc()),
        margin: Some(time::Duration::days(31)),
    };

    let mut verdicts = condemned_on(window_id);
    strand_free_disposition(&mut verdicts, &[generation], at(5));

    assert_eq!(
        verdicts[0].disposition,
        WindowDisposition::Kept(KeptReason::GrandfatheredCoverageBound)
    );
}

#[test]
fn a_generation_still_covered_without_the_condemned_window_cancels_it() {
    // The bound is about the key's coverage and not about any one window: a
    // second interval carrying the whole span means the condemned one is not
    // load-bearing, and keeping it would refuse an act that strands nobody.
    // Without this case the rule is satisfied by "never cancel on a grandfathered
    // key", which is the overreach the integration controls also guard.
    let condemned_id = Uuid::now_v7();
    let generation = GenerationCoverage {
        scope_key: generation_key(at(4)),
        windows: vec![
            (
                condemned_id,
                WindowInterval::new(at(4), Some(at(10)), WindowState::Scheduled),
            ),
            (
                Uuid::now_v7(),
                WindowInterval::new(at(4), None, WindowState::Active),
            ),
        ],
        grandfather_until: Some(at(20)),
        margin: Some(time::Duration::days(31)),
    };

    let mut verdicts = condemned_on(condemned_id);
    strand_free_disposition(&mut verdicts, &[generation], at(5));

    assert_eq!(verdicts[0].disposition, WindowDisposition::Cancelled);
}

#[test]
fn a_window_the_lane_already_kept_is_never_reconsidered() {
    // The bound runs after the presence judgement and only ever moves a verdict
    // back to `kept`. A rule that rebuilt the disposition from scratch would
    // relabel D-51's keeps under D-04's reason and tell the operator the wrong
    // thing on every retirement this system can actually perform.
    let window_id = Uuid::now_v7();
    let generation = GenerationCoverage {
        scope_key: generation_key(at(4)),
        windows: vec![(
            window_id,
            WindowInterval::new(at(4), Some(at(10)), WindowState::Scheduled),
        )],
        grandfather_until: Some(at(20)),
        margin: Some(time::Duration::days(31)),
    };

    let mut verdicts = vec![WindowVerdict {
        window_id,
        price_id: Uuid::now_v7(),
        disposition: WindowDisposition::Kept(KeptReason::InFlightSubscribers),
    }];
    strand_free_disposition(&mut verdicts, &[generation], at(5));

    assert_eq!(
        verdicts[0].disposition,
        WindowDisposition::Kept(KeptReason::InFlightSubscribers),
        "D-51's reason survives; this bound adds keeps, it does not relabel them"
    );
}
