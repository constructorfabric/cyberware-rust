//! Tests for the grandfathered-row eligibility machine's two authoring edges.
//!
//! Both directions of one comparison, and the two accepting arms are here as
//! **positive controls** rather than as coverage: a refusal that nobody has shown
//! accepting anything is indistinguishable from a function that refuses
//! everything, which is the shape a guard clause takes when its operand is wrong.

use super::check_tightening;
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use time::OffsetDateTime;

fn at(day: i64) -> OffsetDateTime {
    crate::domain::instant::from_unix(1_800_000_000 + day * 86_400, 0)
        .expect("a representable instant")
}

/// `inst-gs-bound`: `active_indefinite → active_bounded`.
#[test]
fn setting_a_horizon_on_an_indefinite_generation_is_a_tightening() {
    check_tightening(None, at(30)).expect("setting a bound on an indefinite generation");
}

/// `inst-gs-tighten`: `active_bounded → active_bounded`.
#[test]
fn moving_a_horizon_earlier_is_a_tightening() {
    check_tightening(Some(at(30)), at(10)).expect("moving the bound earlier");
}

#[test]
fn moving_a_horizon_later_is_refused() {
    let err = check_tightening(Some(at(10)), at(30))
        .expect_err("a horizon moved outwards must be refused");
    let DomainError::GrandfatherLoosenForbidden(detail) = err else {
        panic!("a loosened horizon must carry GRANDFATHER_LOOSEN_FORBIDDEN, got: {err:?}");
    };
    // Both instants, because the author corrects one value: a message naming only
    // the submitted one leaves them guessing what it had to beat.
    assert!(
        detail.contains(&format_rfc3339(at(10))) && detail.contains(&format_rfc3339(at(30))),
        "the refusal must name the published horizon and the submitted one: {detail}"
    );
}

/// The arm the module doc argues about by name: an equal instant is a transition
/// that moves nothing, and the act is always material, so accepting it would open
/// an approval unit over `T → T`.
#[test]
fn an_unchanged_horizon_is_refused_rather_than_treated_as_a_no_op() {
    let err = check_tightening(Some(at(10)), at(10))
        .expect_err("a horizon that does not move must be refused");
    assert!(
        matches!(err, DomainError::GrandfatherLoosenForbidden(_)),
        "got: {err:?}"
    );
}

/// One millisecond earlier is a tightening. Armed against the off-by-one an
/// inclusive comparison (`<=`) would introduce: that spelling would refuse the
/// smallest real tightening while still accepting everything else, so the two
/// cases above cannot tell it from the correct one.
#[test]
fn the_smallest_real_tightening_is_accepted() {
    let published = at(10);
    let proposed = published - time::Duration::milliseconds(1);
    check_tightening(Some(published), proposed).expect("one millisecond earlier is earlier");
}
