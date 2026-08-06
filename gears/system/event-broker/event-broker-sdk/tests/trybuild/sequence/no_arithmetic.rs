// A sequence is an ordinal, not a measure: the distance between two of them
// counts nothing, and the value after one is not it plus one. None of the
// operations below may compile.
use event_broker_sdk::Sequence;

fn main() {
    let a = Sequence::assigned(50);
    let b = Sequence::assigned(6);

    // A magnitude derived from two positions.
    let _ = a - b;

    // Stepping to a neighbour that may not exist.
    let _ = a + 1;

    // Asking for a successor the space cannot promise.
    let _ = a.next();

    // Ordering is the whole point, and must still work.
    assert!(a > b);
}
