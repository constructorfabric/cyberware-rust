//! Unit cases for [`super`] — §4's four edges, D-34's rescoped cancel, D-49's
//! notice floor, `inst-mg-target`'s target predicate and D-39's entry phase.

use uuid::Uuid;

use crate::domain::plan_shape::PhaseKind;

use super::{
    MigrationState, NOTICE_FLOOR_DAYS, NoticePeriod, ensure_distinct_plans,
    ensure_target_publishable, entry_phase_index,
};
use crate::domain::error::DomainError;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use time::Duration;
use time::OffsetDateTime;

fn at(year: i32, month: u32, day: u32) -> OffsetDateTime {
    utc_ymd_hms(year, month, day, 0, 0, 0)
}

// ---------------------------------------------------------------------------
// §4's state machine.
// ---------------------------------------------------------------------------

#[test]
fn the_four_sanctioned_edges_are_legal_and_nothing_else_is() {
    let legal = [
        (MigrationState::Scheduled, MigrationState::InProgress),
        (MigrationState::Scheduled, MigrationState::Cancelled),
        (MigrationState::InProgress, MigrationState::Completed),
        (MigrationState::InProgress, MigrationState::Cancelled),
    ];
    for (from, to) in legal {
        assert!(from.can_transition(to), "{from} -> {to} should be legal");
        assert!(from.transition(to).is_ok());
    }

    // Every other ordered pair, including all four self-edges.
    for &from in MigrationState::ALL {
        for &to in MigrationState::ALL {
            if legal.contains(&(from, to)) {
                continue;
            }
            assert!(
                !from.can_transition(to),
                "{from} -> {to} should not be legal"
            );
            assert!(from.transition(to).is_err());
        }
    }
}

#[test]
fn a_repeat_start_is_not_a_self_edge_the_machine_accepts() {
    // D-65: a second `start` is a **replay** that returns the persisted exclusion
    // set, and the machine must not quietly accept the flip a second time -
    // otherwise the replay path could be skipped without anything noticing.
    assert!(
        MigrationState::InProgress
            .transition(MigrationState::InProgress)
            .is_err()
    );
}

#[test]
fn cancelling_a_completed_run_answers_migration_completed_and_not_a_bare_lifecycle_refusal() {
    // D-34's one named refusal. The code deliberately is not the pre-D-34
    // `MIGRATION_ALREADY_EFFECTIVE`.
    let err = MigrationState::Completed.ensure_cancellable().unwrap_err();
    assert!(
        matches!(err, DomainError::MigrationCompleted(_)),
        "expected MigrationCompleted, got {err:?}"
    );
}

#[test]
fn an_in_progress_run_is_cancellable_which_is_the_whole_of_what_d34_changed() {
    assert!(MigrationState::InProgress.ensure_cancellable().is_ok());
    assert!(MigrationState::Scheduled.ensure_cancellable().is_ok());
}

#[test]
fn cancelling_an_already_cancelled_run_is_a_lifecycle_refusal_not_a_completion_one() {
    // The two say different things to an operator: one finished doing work, the
    // other was already stopped.
    let err = MigrationState::Cancelled.ensure_cancellable().unwrap_err();
    assert!(
        matches!(err, DomainError::LifecycleForbidden(_)),
        "expected LifecycleForbidden, got {err:?}"
    );
}

#[test]
fn only_the_two_live_states_admit_further_processing() {
    // `inst-mg-cancel`'s state handshake: the predicate Subscriptions asks before
    // execution and per batch.
    assert!(MigrationState::Scheduled.admits_further_processing());
    assert!(MigrationState::InProgress.admits_further_processing());
    assert!(!MigrationState::Completed.admits_further_processing());
    assert!(!MigrationState::Cancelled.admits_further_processing());
}

#[test]
fn terminality_and_the_processing_predicate_agree_on_every_state() {
    for &state in MigrationState::ALL {
        assert_eq!(state.is_terminal(), !state.admits_further_processing());
    }
}

#[test]
fn every_state_round_trips_through_its_token_and_an_unknown_one_does_not_resolve() {
    for &state in MigrationState::ALL {
        assert_eq!(MigrationState::parse(state.as_str()), Some(state));
    }
    // A fifth token is a corrupt row, not a default: resolving it to `scheduled`
    // would hide it behind the one state that still admits every transition.
    assert_eq!(MigrationState::parse("in-progress"), None);
    assert_eq!(MigrationState::parse("effective"), None);
    assert_eq!(MigrationState::parse(""), None);
}

// ---------------------------------------------------------------------------
// D-49's notice period.
// ---------------------------------------------------------------------------

#[test]
fn a_configured_period_below_the_floor_is_raised_to_it_rather_than_honoured() {
    // Fail-closed: lowering is the direction that costs a subscriber their warning.
    assert_eq!(NoticePeriod::resolved(0).days(), NOTICE_FLOOR_DAYS);
    assert_eq!(NoticePeriod::resolved(-5).days(), NOTICE_FLOOR_DAYS);
    assert_eq!(NoticePeriod::resolved(59).days(), NOTICE_FLOOR_DAYS);
    assert_eq!(NoticePeriod::floor().days(), NOTICE_FLOOR_DAYS);
}

#[test]
fn a_tenant_may_raise_the_period_and_the_raised_value_is_what_binds() {
    assert_eq!(NoticePeriod::resolved(90).days(), 90);
    let announced = at(2026, 1, 1);
    assert_eq!(
        NoticePeriod::resolved(90)
            .earliest_effective(announced)
            .unwrap(),
        announced + Duration::days(90)
    );
}

#[test]
fn a_migration_exactly_one_notice_period_out_has_given_the_whole_notice() {
    // The bound is inclusive.
    let announced = at(2026, 1, 1);
    let period = NoticePeriod::floor();
    assert!(
        period
            .ensure_honoured(announced, announced + Duration::days(NOTICE_FLOOR_DAYS))
            .is_ok()
    );
}

#[test]
fn a_migration_inside_the_notice_period_is_refused_naming_the_earliest_instant() {
    let announced = at(2026, 1, 1);
    let period = NoticePeriod::floor();
    let err = period
        .ensure_honoured(announced, announced + Duration::days(NOTICE_FLOOR_DAYS - 1))
        .unwrap_err();

    let DomainError::MigrationNoticeTooShort(detail) = err else {
        panic!("expected MigrationNoticeTooShort, got {err:?}");
    };
    // The operator's next act is to pick a later date or change the policy, and
    // neither is reachable from a refusal that only says "too short".
    assert!(detail.contains("60"), "{detail}");
    assert!(detail.contains("earliest admissible"), "{detail}");
    assert!(detail.contains("no override"), "{detail}");
}

#[test]
fn a_day_count_that_overflows_try_days_itself_is_refused_not_panicked() {
    // F-11, site 1 of 2. The column has no upper CHECK, only `>= 60`, so a
    // corrupt or maliciously-large configured period must not reach the
    // unchecked date addition `earliest_effective` used to do. `i64::MAX`
    // overflows `Duration::try_days`'s own internal `checked_mul` (days *
    // 86_400 seconds), so `try_days` returns `None` and the `.and_then`
    // closure that would call `checked_add_signed` never runs.
    //
    // This proves only the *first* overflow site is guarded — the pre-fix
    // panic this reproduces is `Duration::days`'s own "out of bounds"
    // panic, not a `DateTime` add overflow. See the sibling test below for
    // the second site, which this value cannot reach: `try_days`'s ceiling
    // (~1.07e14 days) is six orders of magnitude past chrono's date range
    // (~9.6e7 days), so anything that overflows the add already overflowed
    // the multiply first, at a far smaller magnitude on this axis alone.
    let announced = at(2026, 1, 1);
    let period = NoticePeriod::resolved(i64::MAX);
    let err = period
        .ensure_honoured(announced, announced)
        .expect_err("an unrepresentable notice period must be refused, not panic");

    let DomainError::MigrationNoticeTooShort(detail) = err else {
        panic!("expected MigrationNoticeTooShort, got {err:?}");
    };
    assert!(detail.contains(&i64::MAX.to_string()), "{detail}");
    assert!(detail.contains("no override"), "{detail}");
}

#[test]
fn a_day_count_that_builds_but_cannot_be_added_is_refused_not_panicked() {
    // F-11, site 2 of 2 — the one the test above cannot reach.
    //
    // 10_000_000_000 days (~27.4 million years) sits strictly between the two
    // ceilings: comfortably under `try_days`'s ~1.07e14-day bound (so
    // `Duration::try_days` succeeds and returns `Some`), but far past
    // chrono's ~9.6e7-day representable range from `announced_at` (so
    // `checked_add_signed` is what returns `None`). This is the case that
    // was reddened by hand against a bare `+` in place of
    // `checked_add_signed`, to confirm this test — and not its sibling —
    // is the one that actually exercises the add.
    let announced = at(2026, 1, 1);
    let period = NoticePeriod::resolved(10_000_000_000);
    let err = period
        .ensure_honoured(announced, announced)
        .expect_err("a period that overflows the add, but not try_days, must be refused");

    let DomainError::MigrationNoticeTooShort(detail) = err else {
        panic!("expected MigrationNoticeTooShort, got {err:?}");
    };
    assert!(detail.contains("10000000000"), "{detail}");
    assert!(detail.contains("no override"), "{detail}");
}

#[test]
fn a_large_but_representable_notice_period_still_resolves_and_is_honoured() {
    // The fix must not have narrowed the accepted range — only closed the
    // two panics. 400 days is far past the floor and nowhere near either
    // ceiling, and must still round-trip exactly as it did before F-11.
    let announced = at(2026, 1, 1);
    let period = NoticePeriod::resolved(400);
    assert_eq!(period.days(), 400);

    let earliest = period.earliest_effective(announced).unwrap();
    assert_eq!(earliest, announced + Duration::days(400));

    assert!(period.ensure_honoured(announced, earliest).is_ok());
    assert!(
        period
            .ensure_honoured(announced, earliest - Duration::days(1))
            .is_err()
    );
}

#[test]
fn a_raised_period_refuses_a_date_the_floor_would_have_admitted() {
    // The raise is not decorative: it moves the boundary.
    let announced = at(2026, 1, 1);
    let effective = announced + Duration::days(75);
    assert!(
        NoticePeriod::floor()
            .ensure_honoured(announced, effective)
            .is_ok()
    );
    assert!(
        NoticePeriod::resolved(90)
            .ensure_honoured(announced, effective)
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// `inst-mg-target`.
// ---------------------------------------------------------------------------

#[test]
fn only_a_published_target_admits_a_migration() {
    let target = Uuid::now_v7();
    assert!(ensure_target_publishable(target, LifecycleState::Published).is_ok());

    for state in [
        LifecycleState::Draft,
        LifecycleState::Abandoned,
        LifecycleState::Superseded,
        LifecycleState::Retired,
    ] {
        let err = ensure_target_publishable(target, state).unwrap_err();
        let DomainError::MigrationTargetInvalid(detail) = err else {
            panic!("expected MigrationTargetInvalid for {state}, got {err:?}");
        };
        // The refusal names the state the target is actually in.
        assert!(detail.contains(state.as_str()), "{detail}");
    }
}

#[test]
fn a_migration_onto_the_source_plan_itself_is_refused() {
    let plan = Uuid::now_v7();
    assert!(ensure_distinct_plans(plan, Uuid::now_v7()).is_ok());
    assert!(matches!(
        ensure_distinct_plans(plan, plan).unwrap_err(),
        DomainError::MigrationTargetInvalid(_)
    ));
}

// ---------------------------------------------------------------------------
// D-39.
// ---------------------------------------------------------------------------

#[test]
fn a_migrated_subscription_enters_the_first_non_trial_phase() {
    // D-39, CONFIRMED 2026-08-07: a migration never grants a new `trial`.
    assert_eq!(
        entry_phase_index(&[PhaseKind::Trial, PhaseKind::Interim, PhaseKind::Evergreen]),
        Some(1)
    );
    // ...and **does** grant `interim`, which is the half that is easy to lose.
    assert_eq!(
        entry_phase_index(&[PhaseKind::Interim, PhaseKind::Evergreen]),
        Some(0)
    );
    assert_eq!(entry_phase_index(&[PhaseKind::Evergreen]), Some(0));
    // Consecutive trials are all skipped, not just the first.
    assert_eq!(
        entry_phase_index(&[PhaseKind::Trial, PhaseKind::Trial, PhaseKind::Evergreen]),
        Some(2)
    );
}

#[test]
fn a_target_that_is_entirely_trial_offers_no_entry_phase() {
    // Not decided here: the caller surfaces it as a boundary delta.
    assert_eq!(entry_phase_index(&[PhaseKind::Trial]), None);
    assert_eq!(entry_phase_index(&[]), None);
}
