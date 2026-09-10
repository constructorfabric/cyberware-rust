//! What the sweep decides without a database.
//!
//! The split is `readmodel_warm_tests.rs`'s: everything else this pass does is a
//! statement about rows in two tables and is proved in
//! `tests/sqlite_window_activation.rs`. What is left here is
//!
//! 1. the alarm string the design set names — an operator's runbook greps for it
//!    and, the alarms having no sink, the string *is* the artifact;
//! 2. the changeover order, which is a rule about which of two events a consumer
//!    reads first and would otherwise only be observable through two committed
//!    outbox rows;
//! 3. how much of one pass's page an activation read may take, which is the other
//!    half of that same changeover rule — the sort orders the pair, the budget is
//!    what guarantees both halves were read.
//!
//! A fourth used to be here: *that a pass which did nothing reports nothing*. It
//! constructed no job and never called `run`, so what it asserted was
//! `#[derive(Default)]` — and of six fields it named five. The property it was named
//! for belongs to `run` and is asserted of `run` twice, by
//! `a_second_pass_over_the_same_instant_is_a_no_op` and
//! `a_cancelled_window_is_due_for_nothing`, both against a store.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{ALARM_WINDOW_ACTIVATION_OVERDUE, DUE_WINDOWS_PER_PASS, activation_budget, sort_key};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::scope_key::PlanId;
use crate::infra::storage::repo::window_repo::{DueBoundary, DueWindow};

fn due(boundary: DueBoundary, window: u128, at_day: u32) -> DueWindow {
    let at = utc_ymd_hms(2099, 9, at_day, 0, 0, 0);
    DueWindow {
        window_id: Uuid::from_u128(window),
        tenant_id: Uuid::from_u128(0x7e_11),
        price_id: Uuid::from_u128(0xa0_01),
        plan_id: PlanId::new(Uuid::from_u128(0x91_a1)),
        effective_from: at,
        effective_to: None,
        boundary,
        at,
    }
}

#[test]
fn the_alarm_name_is_the_one_the_design_set_spells() {
    // Asserted against a literal rather than derived, for `CatalogEvent::as_str`'s
    // reason: this is the string an operator's runbook matches on.
    //
    // The second half of that sentence used to read "and this gear has no alarm
    // facility for a second spelling to fail against". It was the same falsified
    // premise the module doc carried, and it is now checkable rather than
    // asserted: there **are** two spellings, and they must agree.
    assert_eq!(
        ALARM_WINDOW_ACTIVATION_OVERDUE,
        "pricing.window.activation_overdue"
    );
    assert_eq!(
        crate::domain::ports::metrics::PricingAlarm::WindowActivationOverdue.as_str(),
        ALARM_WINDOW_ACTIVATION_OVERDUE,
        "the log line's string and the alarm plane's label are one name"
    );
}

/// On one instant an **expiry** is ordered before an **activation**.
///
/// The changeover: the predecessor's window ends where the successor's begins, so
/// the two boundaries are the same instant and the outbox sequence is the only
/// thing that says which happened first. A consumer told the successor took
/// effect before being told the predecessor was over reads, for one event, a key
/// held by two windows at once.
#[test]
fn an_expiry_is_ordered_before_an_activation_on_the_same_instant() {
    // The activation carries the LOWER window id, so an ordering that fell
    // through to the id would put it first.
    let activation = due(DueBoundary::Activation, 0x01, 20);
    let expiry = due(DueBoundary::Expiry, 0x02, 20);

    let mut work = [activation, expiry];
    work.sort_unstable_by_key(sort_key);

    assert_eq!(
        work.iter().map(|w| w.boundary).collect::<Vec<_>>(),
        [DueBoundary::Expiry, DueBoundary::Activation]
    );
}

/// An earlier boundary is ordered first whichever kind it is — a backlog drains
/// in the order it accumulated.
#[test]
fn an_earlier_boundary_is_swept_before_a_later_one() {
    let later = due(DueBoundary::Expiry, 0x01, 25);
    let earlier = due(DueBoundary::Activation, 0x02, 20);

    let mut work = [later, earlier];
    work.sort_unstable_by_key(sort_key);

    assert_eq!(
        work.iter().map(|w| w.window_id).collect::<Vec<_>>(),
        [Uuid::from_u128(0x02), Uuid::from_u128(0x01)],
        "the boundary instant outranks both the kind and the id"
    );
}

/// A **saturated** expiry read leaves an activation read no budget at all.
///
/// The zero is the whole guarantee. An activation is read only when the expiry read
/// came back short of its bound, and a capped read that comes back short returned
/// everything that matched — so "an activation is in the page" implies "every due
/// expiry is in the page", which is what stops a changeover from being emitted in
/// halves. A budget that merely shrank would leave the pair's fate to how much room
/// happened to be left.
#[test]
fn a_saturated_expiry_read_leaves_no_room_for_an_activation() {
    assert_eq!(
        activation_budget(usize::try_from(DUE_WINDOWS_PER_PASS).expect("the bound is a usize")),
        0,
        "a full expiry page spends the whole pass"
    );
    assert_eq!(
        activation_budget(usize::MAX),
        0,
        "and a page somehow over its own bound saturates rather than wrapping to a bigger one"
    );

    let short = DUE_WINDOWS_PER_PASS - 1;
    assert_eq!(
        activation_budget(usize::try_from(short).expect("the bound is a usize")),
        1,
        "one row short of the bound leaves room for exactly one activation"
    );
    assert_eq!(
        activation_budget(0),
        DUE_WINDOWS_PER_PASS,
        "a quiet expiry read leaves the activation read the whole page"
    );
}
