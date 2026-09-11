//! The one predicate everything that reasons about a reader and a span goes
//! through.

use super::span::AccountedSpan;
use crate::domain::model::Sequence;

fn span(from: i64, through: i64) -> AccountedSpan {
    AccountedSpan::builder(Sequence::assigned(from))
        .through(Sequence::assigned(through))
        .build()
}

#[test]
fn a_span_contains_its_own_ends() {
    let resident = span(100, 200);

    assert!(resident.contains(Sequence::assigned(100)));
    assert!(resident.contains(Sequence::assigned(200)));
    assert!(!resident.contains(Sequence::assigned(99)));
    assert!(!resident.contains(Sequence::assigned(201)));
}

#[test]
fn serving_a_reader_is_about_the_sequence_after_its_offset() {
    let resident = span(100, 200);

    // Offsets are exclusive: a reader at 99 has consumed 99 and wants 100.
    assert!(resident.serves(Sequence::assigned(99)));
    assert!(
        !resident.serves(Sequence::assigned(200)),
        "a reader at the end of the span has had all of it"
    );
    assert!(
        !resident.serves(Sequence::assigned(98)),
        "99 is not in this span, so it cannot answer a reader at 98"
    );
}

#[test]
fn a_single_sequence_span_serves_exactly_one_position() {
    let resident = span(500, 500);

    assert!(resident.serves(Sequence::assigned(499)));
    assert!(!resident.serves(Sequence::assigned(500)));
    assert!(!resident.serves(Sequence::assigned(498)));
}

#[test]
fn an_inverted_span_is_normalised_rather_than_inverting() {
    let resident = AccountedSpan::builder(Sequence::assigned(200))
        .through(Sequence::assigned(100))
        .build();

    assert_eq!(resident.from(), Sequence::assigned(200));
    assert_eq!(
        resident.through(),
        Sequence::assigned(200),
        "a span cannot end before it begins, even if one is asked for"
    );
}

#[test]
fn adjacency_is_exact() {
    assert!(span(100, 200).is_adjacent_to(span(201, 300)));
    assert!(
        !span(100, 200).is_adjacent_to(span(202, 300)),
        "201 is unaccounted for, so a read may not cross"
    );
    assert!(
        !span(100, 200).is_adjacent_to(span(200, 300)),
        "overlapping is not adjacent"
    );
}

#[test]
fn serving_and_adjacency_agree_at_a_boundary() {
    let left = span(100, 200);
    let right = span(201, 300);

    // The reader that the left span has just finished with is exactly the reader
    // the right span serves next. That agreement is what lets a walk cross the
    // boundary without a reader ever being stranded between the two.
    assert!(!left.serves(Sequence::assigned(200)));
    assert!(right.serves(Sequence::assigned(200)));
    assert!(left.is_adjacent_to(right));
}
