//! Pure: the frame types and the one invariant `Position` enforces.

use crate::domain::model::Sequence;
use toolkit_gts::GtsInstanceId;

use super::frames::{CloseReason, Position};

fn topic() -> GtsInstanceId {
    GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
        .expect("static gts id is valid")
}

#[test]
fn a_position_reports_what_it_was_given() {
    let position = Position::builder(topic(), 3)
        .offset(Sequence::assigned(100))
        .last_examined(Sequence::assigned(140))
        .build();

    assert_eq!(position.partition, 3);
    assert_eq!(position.offset, Sequence::assigned(100));
    assert_eq!(position.last_examined, Sequence::assigned(140));
}

#[test]
fn a_frontier_behind_the_cursor_is_normalised_forward() {
    // Everything delivered was examined, so a frontier behind the cursor is
    // incoherent. Normalised rather than trusted, so a caller that sets only
    // `offset` still reports a consistent pair.
    let position = Position::builder(topic(), 0)
        .offset(Sequence::assigned(500))
        .last_examined(Sequence::assigned(10))
        .build();

    assert_eq!(position.last_examined, Sequence::assigned(500));
}

#[test]
fn setting_only_the_cursor_reports_the_frontier_with_it() {
    let position = Position::builder(topic(), 0)
        .offset(Sequence::assigned(42))
        .build();

    assert_eq!(position.offset, Sequence::assigned(42));
    assert_eq!(position.last_examined, Sequence::assigned(42));
}

#[test]
fn a_saturating_filter_leaves_the_frontier_far_ahead() {
    // The case the progress frame exists for: one match in a hundred thousand.
    let position = Position::builder(topic(), 0)
        .offset(Sequence::assigned(8))
        .last_examined(Sequence::assigned(100_000))
        .build();

    assert!(position.last_examined > position.offset);
}

#[test]
fn close_reasons_have_stable_wire_spellings() {
    // Kept beside the variants so a rename cannot silently change the wire.
    assert_eq!(CloseReason::Rebalanced.as_wire(), "rebalanced");
    assert_eq!(CloseReason::LoseAll.as_wire(), "lose_all");
    assert_eq!(CloseReason::Teardown.as_wire(), "teardown");
}
