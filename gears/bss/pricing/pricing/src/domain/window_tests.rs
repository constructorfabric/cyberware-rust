//! The window state token, §4's edge set, the interval arithmetic and the five
//! checked forms a surface calls before it attempts a mutation.

use super::{
    CoverageEnd, KeyWindows, WINDOW_HISTORICAL_IMMUTABLE, WINDOW_NOT_CANCELLABLE, WINDOW_OVERLAP,
    WINDOW_START_IN_PAST, WindowInterval, WindowRefusal, WindowState, check_cancellation,
    check_creation, check_effective_to_adjustment, group_by_key, group_by_key_seeded, transition,
};
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::instant::utc_ymd_hms;
use time::OffsetDateTime;

/// `2026-09-01T00:00:00Z` plus `day` days — the instant vocabulary the window
/// suites share. Days rather than hours so every instant in a test is legibly
/// ordered, and an offset rather than a calendar day so `t(200)` is still an
/// instant.
fn t(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2026, 9, 1, 0, 0, 0) + time::Duration::days(day)
}

/// The code a refusal reached the caller with, so an assertion names the code
/// rather than the variant.
fn code_of(err: &DomainError) -> &'static str {
    WindowRefusal::ALL
        .iter()
        .copied()
        .find(|r| std::mem::discriminant(&r.refuse(String::new())) == std::mem::discriminant(err))
        .map_or("<not a window refusal>", WindowRefusal::code)
}

/// The four literals, spelled out here on purpose.
///
/// They are the enumeration `chk_pricing_price_window_state` admits, and the two
/// sides are checked against each other by nothing at compile time: a rename here
/// leaves a store that can neither write nor read the token, and the Postgres and
/// `SQLite` suites would then fail for a reason that reads as a schema problem.
/// Restating them is the cheapest thing that makes the rename itself the failure.
#[test]
fn the_state_tokens_are_the_four_the_check_admits_in_section_fours_order() {
    let tokens: Vec<&str> = WindowState::ALL
        .iter()
        .copied()
        .map(WindowState::as_str)
        .collect();
    assert_eq!(tokens, ["scheduled", "active", "expired", "cancelled"]);
}

#[test]
fn every_state_round_trips_through_its_token() {
    for state in WindowState::ALL {
        assert_eq!(WindowState::from_token(state.as_str()), Some(*state));
    }
}

/// A token outside the enumeration is refused rather than defaulted — the row
/// was written around the CHECK, and `scheduled` is the worst possible guess: it
/// would read as a window that is about to take effect.
#[test]
fn a_token_outside_the_enumeration_is_not_a_state() {
    for alien in ["", "paused", "Scheduled", "cancelled "] {
        assert_eq!(WindowState::from_token(alien), None, "{alien:?}");
    }
}

/// The whole product, not the three edges alone. A table that only asserted its
/// members would stay green if a fourth edge appeared beside them.
#[test]
fn the_machine_has_exactly_the_three_edges_section_four_names() {
    use WindowState::{Active, Cancelled, Expired, Scheduled};
    let sanctioned = [
        (Scheduled, Active),
        (Scheduled, Cancelled),
        (Active, Expired),
    ];
    for from in WindowState::ALL {
        for to in WindowState::ALL {
            let expected = sanctioned.contains(&(*from, *to));
            assert_eq!(
                from.may_move_to(*to),
                expected,
                "{from} -> {to} must be {}",
                if expected { "sanctioned" } else { "refused" }
            );
        }
    }
}

/// Restated as the property rather than as three rows of the table above: a
/// window that is history goes nowhere, which is what `inst-ws-immutable` says
/// and what the migration's second trigger arm enforces physically.
#[test]
fn a_terminal_window_has_no_outbound_edge() {
    for from in WindowState::ALL.iter().filter(|s| s.is_terminal()) {
        for to in WindowState::ALL {
            assert!(!from.may_move_to(*to), "{from} -> {to}");
        }
    }
    assert!(WindowState::Expired.is_terminal());
    assert!(WindowState::Cancelled.is_terminal());
    assert!(!WindowState::Scheduled.is_terminal());
    assert!(!WindowState::Active.is_terminal());
}

/// A self-edge is not a transition. The activation job's idempotence is a
/// question about the row ("it is already there"), not about the machine ("it may
/// move there"), and folding the two would let a re-driven sweep re-stamp an
/// `activated_at` that already recorded when the price took effect.
#[test]
fn a_self_edge_is_not_a_transition() {
    for state in WindowState::ALL {
        assert!(!state.may_move_to(*state), "{state} -> itself");
    }
}

/// The code strings, pinned against the design set's spelling.
///
/// Four literals restated from §5's problem-response list. They are what a
/// consumer branches on, nothing in the toolchain checks this crate's spelling
/// against the document, and one character is the whole difference between a code
/// a client matches and one it does not — which this crate has measured:
/// `WINDOW_OVERLAP` corrupted to `WINDOW_OVERLAPX` left the entire error suite
/// green.
#[test]
fn the_four_codes_are_the_ones_section_five_declares() {
    assert_eq!(WINDOW_OVERLAP, "WINDOW_OVERLAP");
    assert_eq!(WINDOW_HISTORICAL_IMMUTABLE, "WINDOW_HISTORICAL_IMMUTABLE");
    assert_eq!(WINDOW_NOT_CANCELLABLE, "WINDOW_NOT_CANCELLABLE");
    assert_eq!(WINDOW_START_IN_PAST, "WINDOW_START_IN_PAST");
}

/// Every refusal carries a distinct code, and the enumeration is what closes the
/// set. A fifth arm without a code of its own fails here.
#[test]
fn each_refusal_carries_its_own_code() {
    let codes: Vec<&str> = WindowRefusal::ALL
        .iter()
        .copied()
        .map(WindowRefusal::code)
        .collect();
    assert_eq!(
        codes,
        [
            WINDOW_OVERLAP,
            WINDOW_HISTORICAL_IMMUTABLE,
            WINDOW_NOT_CANCELLABLE,
            WINDOW_START_IN_PAST
        ]
    );
    for (i, code) in codes.iter().enumerate() {
        assert!(
            !codes[i + 1..].contains(code),
            "{code} is claimed by two refusals"
        );
    }
}

// ---------------------------------------------------------------------------
// The interval, and the half-openness every consumer re-derives from it
// ---------------------------------------------------------------------------

/// Half-open, and this is the one that keeps adjacency legal.
///
/// A window covers its `effectiveFrom` and does **not** cover its `effectiveTo`,
/// so `effectiveTo = next.effectiveFrom` leaves the boundary instant covered
/// exactly once. §9 names the false positive an inclusive end would produce, and
/// it would refuse the shape a supersession and a cutover both produce.
#[test]
fn a_window_does_not_cover_its_own_effective_to() {
    let window = WindowInterval::new(t(10), Some(t(20)), WindowState::Active);

    assert!(window.covers(t(10)), "the start is covered");
    assert!(window.covers(t(19)));
    assert!(!window.covers(t(20)), "the end is NOT covered");
    assert!(!window.covers(t(9)));

    // And the adjacency that buys: the successor covers exactly the instant its
    // predecessor does not.
    let successor = WindowInterval::new(t(20), Some(t(30)), WindowState::Scheduled);
    assert!(successor.covers(t(20)));
}

/// An open-ended window covers every instant from its start onward, and covering
/// is asked of the interval rather than of the state — predicate (1) is a
/// conjunction and this is one of its terms.
#[test]
fn an_open_ended_window_covers_everything_after_its_start() {
    let window = WindowInterval::new(t(10), None, WindowState::Active);

    assert!(window.is_open_ended());
    assert!(window.covers(t(10)));
    assert!(window.covers(t(200)));
    assert!(!window.covers(t(9)));
}

/// An open-ended window never expires (`inst-ws-expire`).
///
/// The instruction's own words, and not a convenience: no fallback pricing
/// exists, so a key whose coverage merely stopped fails closed downstream rather
/// than falling back to anything. A job that expired open-ended windows would
/// take every such key out of service on its first pass.
#[test]
fn an_open_ended_window_is_never_due_to_expire() {
    let open = WindowInterval::new(t(10), None, WindowState::Active);
    let bounded = WindowInterval::new(t(10), Some(t(20)), WindowState::Active);

    for now in [t(10), t(20), t(200)] {
        assert!(!open.is_due_to_expire(now), "at {now}");
    }
    assert!(!bounded.is_due_to_expire(t(19)));
    assert!(
        bounded.is_due_to_expire(t(20)),
        "expiry is at the exclusive end, which the window does not cover"
    );
}

/// `inst-ws-activate` fires **at** `effectiveFrom`, not after it — the same
/// boundary `covers` uses, so the instant a window starts covering is the instant
/// the job may flip it.
#[test]
fn activation_is_due_at_the_start_instant_itself() {
    let window = WindowInterval::new(t(10), Some(t(20)), WindowState::Scheduled);

    assert!(!window.has_started(t(9)));
    assert!(window.has_started(t(10)));
    assert_eq!(
        window.has_started(t(10)),
        window.covers(t(10)),
        "the flip condition and the containment condition must agree at the boundary"
    );
}

/// `effectiveTo = effectiveFrom` is not a zero-length window, it is a mistake.
#[test]
fn an_interval_that_ends_where_it_starts_is_not_an_interval() {
    assert!(!WindowInterval::new(t(10), Some(t(10)), WindowState::Scheduled).is_non_empty());
    assert!(!WindowInterval::new(t(10), Some(t(9)), WindowState::Scheduled).is_non_empty());
    assert!(WindowInterval::new(t(10), Some(t(11)), WindowState::Scheduled).is_non_empty());
    assert!(
        WindowInterval::new(t(10), None, WindowState::Scheduled).is_non_empty(),
        "open-ended is the widest interval there is, not an empty one"
    );
}

// ---------------------------------------------------------------------------
// Creation — `inst-ws-future-start`
// ---------------------------------------------------------------------------

/// `inst-ws-future-start` (D-63): a start in the past is refused at creation.
///
/// This is not an aesthetic rule — it is what stops a `plan × write` holder
/// `POSTing` a window that started 60 days ago, having the job activate it, and
/// repricing open arrears periods behind the `BackdateGrant`.
#[test]
fn a_window_starting_in_the_past_is_refused() {
    let now = t(10);

    let refusal = check_creation(t(9), Some(t(20)), now).expect_err("a past start is refused");
    assert_eq!(code_of(&refusal), WINDOW_START_IN_PAST);
    assert!(
        refusal.to_string().contains(&format_rfc3339(t(9))),
        "the refusal must name the instant an author has to correct: {refusal}"
    );
}

/// Strictly future, so the boundary itself is refused.
///
/// A window created at the instant it takes effect is one the
/// `WindowActivationJob` may flip before the publish unit carrying it reaches a
/// committed `CatalogVersion` — the unwarmed-changeover exposure D-101 raises
/// `pricing.window.changeover_unwarmed` for. `>=` here would admit exactly that.
#[test]
fn a_window_starting_at_this_very_instant_is_refused_too() {
    let now = t(10);

    assert_eq!(
        code_of(&check_creation(now, Some(t(20)), now).expect_err("now is not the future")),
        WINDOW_START_IN_PAST
    );
    check_creation(now + time::Duration::milliseconds(1), Some(t(20)), now)
        .expect("one quantum into the future is the future");
}

/// The interval check runs **before** the start check, and the order is what an
/// author can act on: told the start is in the past they move one instant, and
/// then discover the interval was inverted all along.
///
/// # The second half is the unshadowed proof, and the first half cannot be
///
/// The ordering case violates **both** rules, so removing the empty-interval arm
/// reddens it on the *variant* — the start rule refuses the same input with a
/// different code — rather than on the property. So the statement only this arm can
/// refuse is asserted too: a **strictly future** start with an inverted interval.
/// The start rule has nothing to say about it (`t(20) > t(10)` passes), the
/// millisecond quantum is not this function's, and nothing else in `check_creation`
/// looks at the end at all — so with the arm gone that call returns `Ok`.
#[test]
fn an_empty_interval_is_refused_ahead_of_the_start_rule() {
    // First, the statement nothing but this arm can refuse — asserted first so that
    // removing the arm reddens this test on the property rather than on the variant
    // the assertion below would report.
    let unshadowed = check_creation(t(20), Some(t(19)), t(10))
        .expect_err("an end before a strictly future start is still not an interval");
    assert!(
        matches!(unshadowed, DomainError::InvalidRequest(_)),
        "{unshadowed:?}"
    );
    assert_eq!(
        code_of(&unshadowed),
        "<not a window refusal>",
        "and emphatically not the start rule, which has nothing to object to here"
    );

    // Then the order, on an input that violates both rules: the start is past AND
    // the interval is inverted.
    let refusal = check_creation(t(9), Some(t(9)), t(10)).expect_err("refused");

    assert!(
        matches!(refusal, DomainError::InvalidRequest(_)),
        "an interval that is not an interval is a malformed value, not a state \
         conflict: {refusal:?}"
    );
    assert!(refusal.to_string().contains("effectiveTo"), "{refusal}");
}

/// An open-ended future window is the ordinary case and passes both rules.
#[test]
fn a_future_open_ended_window_is_creatable() {
    check_creation(t(20), None, t(10)).expect("the ordinary schedule");
    check_creation(t(20), Some(t(30)), t(10)).expect("and the bounded one");
}

// ---------------------------------------------------------------------------
// Cancellation — `inst-ws-cancel`
// ---------------------------------------------------------------------------

/// `inst-ws-cancel`: only a not-yet-active window cancels.
///
/// **Staged against a genuinely `active` window**, which is the whole of what
/// this test has to establish before it asserts anything: `scheduled` is the one
/// state where the edge is legal, so a refusal observed against any other state
/// would be observed against a world where several rules disagree with the
/// request and the one under test may not be what answered.
#[test]
fn cancelling_an_active_window_is_refused() {
    let refusal =
        check_cancellation(WindowState::Active).expect_err("an active window is never cancelled");

    assert_eq!(code_of(&refusal), WINDOW_NOT_CANCELLABLE);
    assert!(
        refusal.to_string().contains("effectiveTo"),
        "the refusal must name the operation §4 leaves the operator — shortening — \
         because that is their next action: {refusal}"
    );
}

/// An `expired` window is history and does not cancel either. It reaches
/// `WINDOW_NOT_CANCELLABLE` rather than `WINDOW_HISTORICAL_IMMUTABLE` because the
/// **operation** selects the code: §5 gives the cancel path its own refusal, and
/// an operator told "not cancellable" learns which operation to reach for instead.
#[test]
fn cancelling_an_expired_window_is_refused() {
    assert_eq!(
        code_of(&check_cancellation(WindowState::Expired).expect_err("history does not cancel")),
        WINDOW_NOT_CANCELLABLE
    );
}

/// A second cancellation is refused rather than answered `Ok`.
///
/// Not pedantry: a window cancellation is a **publish unit** (D-99) with its own
/// `CatalogVersion` request and its own materiality, so an idempotent `Ok` here
/// would let a whole unit run over a window that cannot change.
#[test]
fn cancelling_an_already_cancelled_window_is_not_a_no_op() {
    assert_eq!(
        code_of(&check_cancellation(WindowState::Cancelled).expect_err("refused")),
        WINDOW_NOT_CANCELLABLE
    );
}

/// And the one state that does cancel, so the four assertions above are not a
/// function that refuses everything.
#[test]
fn a_scheduled_window_cancels() {
    check_cancellation(WindowState::Scheduled).expect("section 4's third edge");
}

// ---------------------------------------------------------------------------
// Moving the end — `inst-ws-immutable`
// ---------------------------------------------------------------------------

/// `inst-ws-immutable`: an `active` window's only permitted mutation is a FUTURE
/// `effective_to`.
///
/// **The world-state requirement is the test.** It runs against a window that is
/// genuinely `active` with a genuinely past `effective_from`; staged against a
/// `scheduled` window it would pass for the wrong reason, because nothing in this
/// function refuses a scheduled window's adjustment at all and the assertion
/// would be measuring the target instant alone.
#[test]
fn shortening_an_active_window_into_the_past_is_refused() {
    let now = t(15);
    let active = WindowInterval::new(t(10), Some(t(20)), WindowState::Active);
    assert!(
        active.has_started(now) && !active.state.is_terminal(),
        "the world: in force, started, not history, so the only rule left to \
         refuse the move is the one under test"
    );

    let refusal = check_effective_to_adjustment(&active, Some(t(12)), now)
        .expect_err("an end may not be moved into the past");

    assert_eq!(code_of(&refusal), WINDOW_HISTORICAL_IMMUTABLE);
    assert!(
        refusal.to_string().contains(&format_rfc3339(t(12))),
        "{refusal}"
    );
}

/// The permitted half, from the same world: a future end moves both ways.
#[test]
fn extending_an_active_windows_future_end_is_permitted() {
    let now = t(15);
    let active = WindowInterval::new(t(10), Some(t(20)), WindowState::Active);

    check_effective_to_adjustment(&active, Some(t(30)), now).expect("an extension");
    check_effective_to_adjustment(&active, Some(t(16)), now).expect("a shorten, still future");
    check_effective_to_adjustment(&active, None, now)
        .expect("open-ended removes a bound rather than any coverage");
}

/// An end that has **already passed** cannot be moved even forward.
///
/// The third case, and the one a naive "is the target future?" check lets
/// through: the stored end is what a consumer resolved a price against, so moving
/// it forward re-opens a window that already closed and rewrites what was
/// chargeable. It is reachable in a `active` state whenever the activation job is
/// behind its SLO.
#[test]
fn an_end_that_has_already_passed_cannot_be_moved_forward() {
    let now = t(25);
    let overdue = WindowInterval::new(t(10), Some(t(20)), WindowState::Active);
    assert!(
        overdue.is_due_to_expire(now) && overdue.state == WindowState::Active,
        "the world: still `active` because the job is behind, with an end in the past"
    );

    let refusal = check_effective_to_adjustment(&overdue, Some(t(30)), now)
        .expect_err("a passed end is history whichever way it is moved");
    assert_eq!(code_of(&refusal), WINDOW_HISTORICAL_IMMUTABLE);
}

/// A terminal window moves nothing. Both terminal states, because "expired" and
/// "cancelled" are one rule with two tokens and a check written against only one
/// of them would leave the other mutable.
#[test]
fn a_terminal_windows_end_cannot_move() {
    for state in [WindowState::Expired, WindowState::Cancelled] {
        let terminal = WindowInterval::new(t(10), Some(t(20)), state);
        let refusal = check_effective_to_adjustment(&terminal, Some(t(30)), t(15))
            .expect_err("history is immutable");
        assert_eq!(code_of(&refusal), WINDOW_HISTORICAL_IMMUTABLE, "{state}");
        assert!(refusal.to_string().contains(state.as_str()), "{refusal}");
    }
}

/// An adjustment cannot invert the interval either, and it is the same refusal
/// the creation path gives — one rule, one answer, whichever door the value
/// arrives through.
#[test]
fn an_adjustment_may_not_invert_the_interval() {
    let scheduled = WindowInterval::new(t(20), Some(t(30)), WindowState::Scheduled);

    let refusal = check_effective_to_adjustment(&scheduled, Some(t(19)), t(10))
        .expect_err("an end before the start is not an interval");
    assert!(
        matches!(refusal, DomainError::InvalidRequest(_)),
        "{refusal:?}"
    );
}

// ---------------------------------------------------------------------------
// The checked edge set
// ---------------------------------------------------------------------------

/// The checked form agrees with the table it delegates to, on the **whole**
/// product rather than on the three sanctioned rows.
///
/// This is what stops the two from drifting: `transition` states no table of its
/// own, and a table it restated would be a fourth answer to which edges exist —
/// beside `may_move_to`, the store's `state = <expected>` predicate and the
/// migration's transition arm.
#[test]
fn the_checked_transition_admits_exactly_what_the_table_does() {
    for from in WindowState::ALL {
        for to in WindowState::ALL {
            assert_eq!(
                transition(*from, *to).is_ok(),
                from.may_move_to(*to),
                "{from} -> {to}"
            );
        }
    }
}

/// A refused edge mints **no window code**: §5 names a code for every window
/// refusal an operator can provoke, and an unsanctioned edge is not one of them —
/// what reaches it is a background sweep whose ordering assumption broke.
#[test]
fn a_refused_edge_is_a_lifecycle_refusal_and_not_a_window_code() {
    let refusal =
        transition(WindowState::Scheduled, WindowState::Expired).expect_err("not an edge");

    assert!(
        matches!(refusal, DomainError::LifecycleForbidden(_)),
        "{refusal:?}"
    );
    assert_eq!(code_of(&refusal), "<not a window refusal>");
}

// ---------------------------------------------------------------------------
// Per-key grouping and the derived coverage end
// ---------------------------------------------------------------------------

fn key(charge_kind: &str) -> crate::domain::scope_key::ScopeKey {
    use crate::domain::money::CurrencyCode;
    use crate::domain::scope_key::{
        ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
    };
    use uuid::Uuid;

    ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x91_a1)),
        CurrencyCode::new("USD").expect("iso currency"),
        Region::new("us-east").expect("region"),
        PhaseId::new(Uuid::from_u128(0x40_a5)),
        PriceEligibility::AllSubscriptions,
        match charge_kind {
            "recurring" => ChargeKind::Recurring,
            _ => ChargeKind::OneTimeSetup,
        },
        Cohort::None,
    )
    .expect("a valid canonical scope key")
}

/// **`first_uncovered_from` orders the set itself**, so a producer that built one
/// by hand out of order cannot make it report a false first-uncovered instant.
///
/// The two intervals below are a contiguous run `[10, 20) [20, 30)` written
/// bottom-last. Walked in authored order the first one starts after `from` and
/// the walk `break`s at once, answering `t(10)` — an instant the key covers, on a
/// bound that decides money: both consumers of this walk
/// (`infra::window::refuse_horizon_span_uncovered` and
/// `domain::retirement::strand_free_disposition`) enforce D-04's horizon with it,
/// and under-reported coverage there refuses a retirement or a window act that
/// should stand.
///
/// It is a hand-built set on purpose. `KeyWindows::intervals` is a plain `pub`
/// field, and `infra::window`'s `PlanContext::before` collects one straight out of
/// a repository read with no sort of its own — correct only because that query
/// happens to `ORDER BY effective_from` three layers away.
#[test]
fn an_unordered_interval_set_does_not_produce_a_false_first_uncovered_instant() {
    let group = KeyWindows {
        scope_key: key("recurring"),
        intervals: vec![
            WindowInterval::new(t(20), Some(t(30)), WindowState::Scheduled),
            WindowInterval::new(t(10), Some(t(20)), WindowState::Active),
        ],
    };

    assert_eq!(
        group.first_uncovered_from(t(10), None),
        Some(t(30)),
        "the run is contiguous to t(30); anything earlier is the authored order leaking in"
    );
    assert_eq!(
        group.first_uncovered_from(t(10), Some(t(25))),
        None,
        "and the bounded question is answered from the same walk"
    );
}

/// Coverage runs to the far end of the key's windows, and the arithmetic is over
/// the **key** rather than over one price row — which is the whole reason this
/// grouping exists: a supersession leaves a `superseded` predecessor and a
/// `published` successor on one key, and their two windows are one coverage run.
#[test]
fn a_keys_coverage_ends_where_its_last_window_ends() {
    let group = KeyWindows {
        scope_key: key("recurring"),
        intervals: vec![
            WindowInterval::new(t(10), Some(t(20)), WindowState::Expired),
            WindowInterval::new(t(20), Some(t(30)), WindowState::Active),
        ],
    };

    assert_eq!(group.coverage_end(), CoverageEnd::Ends(t(30)));
    assert_eq!(group.coverage_end().at(), Some(t(30)));
    assert_eq!(group.coverage_end().as_str(), "ends");
}

/// One open-ended window makes the key's coverage open-ended, whatever else is on
/// it — an open end satisfies the D-80 horizon trivially.
#[test]
fn one_open_ended_window_makes_the_key_open_ended() {
    let group = KeyWindows {
        scope_key: key("recurring"),
        intervals: vec![
            WindowInterval::new(t(10), Some(t(20)), WindowState::Expired),
            WindowInterval::new(t(20), None, WindowState::Active),
        ],
    };

    assert_eq!(group.coverage_end(), CoverageEnd::OpenEnded);
    assert_eq!(
        group.coverage_end().at(),
        None,
        "and `at` is None here for a reason opposite to `Uncovered`'s, which is why \
         the arms are not one"
    );
}

/// A cancelled window contributes no coverage — it is a schedule that never
/// happened.
///
/// **This is the arm that must not round the wrong way.** A key whose only window
/// was cancelled is uncovered, and `Uncovered` is what the D-80 horizon fails on;
/// were it `OpenEnded` — the answer an `Option<DateTime>` would have to share with
/// it — the gate would read that key as sellable forever.
#[test]
fn a_cancelled_window_contributes_no_coverage_and_uncovered_is_not_open_ended() {
    let group = KeyWindows {
        scope_key: key("recurring"),
        intervals: vec![WindowInterval::new(t(10), None, WindowState::Cancelled)],
    };

    assert_eq!(group.coverage_end(), CoverageEnd::Uncovered);
    assert_ne!(group.coverage_end(), CoverageEnd::OpenEnded);
    assert_eq!(group.coverage_end().as_str(), "uncovered");
}

/// `covers_at` folds containment over a key's set — and a cancelled interval
/// contributes nothing to it either.
///
/// The containment half of sellability predicate (1). Two properties in one case,
/// because they are one sentence: the fold is over
/// [`WindowInterval::covers`]'s half-open reading, and a schedule that never
/// happened covers no instant. Were the cancelled arm to answer `true`, a key
/// whose successor an operator withdrew would read as effective over the span
/// nothing was ever effective over — the trailing void arriving from the read
/// side.
#[test]
fn covers_at_folds_the_key_and_a_cancelled_interval_covers_nothing() {
    let live = KeyWindows {
        scope_key: key("recurring"),
        intervals: vec![
            WindowInterval::new(t(10), Some(t(20)), WindowState::Expired),
            WindowInterval::new(t(30), Some(t(40)), WindowState::Scheduled),
        ],
    };

    assert!(live.covers_at(t(15)), "the first interval contains it");
    assert!(live.covers_at(t(35)), "so does the second");
    assert!(
        !live.covers_at(t(25)),
        "and the hole between them is covered by neither"
    );
    assert!(
        !live.covers_at(t(20)),
        "the exclusive end is outside its own interval"
    );

    let withdrawn = KeyWindows {
        scope_key: key("recurring"),
        intervals: vec![WindowInterval::new(t(10), None, WindowState::Cancelled)],
    };
    assert!(
        !withdrawn.covers_at(t(15)),
        "a cancelled window is a schedule that never happened"
    );
}

/// Grouping is per key and the order is the function's promise, because the
/// payload it feeds is INSERT-only on the seven-year horizon: a re-drive that
/// produced the same facts in another order would write a payload that differs
/// byte-for-byte from the one it is meant to be idempotent with.
#[test]
fn grouping_is_per_key_and_ordered_regardless_of_the_input_order() {
    let recurring = key("recurring");
    let one_time = key("one_time_setup");
    let late = WindowInterval::new(t(20), Some(t(30)), WindowState::Scheduled);
    let early = WindowInterval::new(t(10), Some(t(20)), WindowState::Active);

    let forwards = group_by_key([
        (recurring.clone(), early),
        (one_time.clone(), early),
        (recurring.clone(), late),
    ]);
    let backwards = group_by_key([
        (recurring.clone(), late),
        (one_time, early),
        (recurring, early),
    ]);

    assert_eq!(
        forwards, backwards,
        "the order of the input must not survive"
    );
    assert_eq!(forwards.len(), 2, "two keys, two groups");
    let recurring_group = forwards
        .iter()
        .find(|g| g.intervals.len() == 2)
        .expect("the key with two windows");
    assert_eq!(
        recurring_group.intervals,
        vec![early, late],
        "and within a group the intervals run in time order"
    );
}

/// Two windows on one key with **identical bounds** and different states — the tie
/// `(effective_from, effective_to)` cannot break.
///
/// It is a reachable world, not a contrived one: `OCCUPYING_STATES` excludes
/// `expired`, so the store's non-overlap check admits a `scheduled` window with
/// exactly the bounds of an `expired` one on the same key, and
/// `PROJECTED_WINDOW_STATES` projects both. Sorted on the two instants alone the
/// order of the pair was the **input's** — which is the store's row order, i.e.
/// precisely what the byte-stability promise says the payload does not depend on.
/// A stable sort would have preserved that dependence rather than removed it; the
/// third component is what makes the key total over the whole value.
#[test]
fn two_windows_with_one_interval_and_two_states_order_the_same_whichever_arrives_first() {
    let recurring = key("recurring");
    let expired = WindowInterval::new(t(10), Some(t(20)), WindowState::Expired);
    let scheduled = WindowInterval::new(t(10), Some(t(20)), WindowState::Scheduled);

    let forwards = group_by_key([(recurring.clone(), expired), (recurring.clone(), scheduled)]);
    let backwards = group_by_key([(recurring.clone(), scheduled), (recurring, expired)]);

    assert_eq!(
        forwards, backwards,
        "the input order must not survive into a payload that is INSERT-only"
    );
    assert_eq!(
        forwards[0].intervals,
        vec![scheduled, expired],
        "and the tie is broken by section 4's own state order, which is the order a \
         window lives through"
    );
}

/// A seeded key with no surviving window is a group, and it reads `Uncovered`.
///
/// The seed is what gives `CoverageEnd::Uncovered` a producer at all on the
/// projection path: without it "this key is uncovered" is spelled as the absence of
/// a group, which is a second and undeclared spelling of a payload fact. A seeded
/// key that *does* have windows must not be duplicated, and a pair on an unseeded
/// key must still form its own group — both are asserted here, because a seed that
/// dropped either would be a key list rather than a union.
#[test]
fn a_seeded_key_with_no_window_is_a_group_that_reads_uncovered() {
    let recurring = key("recurring");
    let one_time = key("one_time_setup");
    let interval = WindowInterval::new(t(10), Some(t(20)), WindowState::Active);

    let groups = group_by_key_seeded(
        [recurring.clone(), one_time.clone()],
        [(recurring.clone(), interval)],
    );

    assert_eq!(groups.len(), 2, "both seeded keys are present");
    let bare = groups
        .iter()
        .find(|g| g.scope_key == one_time)
        .expect("the seeded key with no window");
    assert!(bare.intervals.is_empty());
    assert_eq!(
        bare.coverage_end(),
        CoverageEnd::Uncovered,
        "and it says so rather than being absent"
    );
    let covered = groups
        .iter()
        .find(|g| g.scope_key == recurring)
        .expect("the seeded key with one window");
    assert_eq!(
        covered.intervals,
        vec![interval],
        "a seeded key that has windows is not duplicated into two groups"
    );

    // The union, from the other side: a pair on a key nobody seeded still forms its
    // own group, so the seed adds keys and never restricts them.
    let unseeded = group_by_key_seeded([one_time], [(recurring, interval)]);
    assert_eq!(unseeded.len(), 2);
}
