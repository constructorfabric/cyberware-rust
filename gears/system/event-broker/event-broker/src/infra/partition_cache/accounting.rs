//! Deriving what a fetch proved, from what it returned.
//!
//! A read asks for events after some offset, bounded by a maximum count, and
//! gets them back in ascending order. That is enough to establish, without any
//! extra call, which sequences in the range it covered are **permanently
//! absent**: within the interval from the requested offset up to the highest
//! sequence returned, the returned set is the complete set, and because
//! sequences are assigned contiguously at persist time, a sequence missing
//! from it has been deleted. Backends may delete at any time - retention
//! trimming a prefix, lawful erasure of one tenant's events mid-stream, or
//! compaction - so holes are expected rather than exceptional. See
//! `docs/DESIGN.md`'s `DeliveryService` responsibilities (requirement R09,
//! "tolerate sparse offsets") and its statement that the offset stream is a
//! sparse log.
//!
//! Nothing is concluded above the highest sequence returned. A read that
//! filled its bound stopped at the bound; a read that did not means nothing
//! further exists yet. Either way that region is the tail, not a gap - and the
//! distinction matters, because a reader may step over a proven absence but
//! must never step over an unknown.

use crate::domain::model::Sequence;

/// What one fetch established.
///
/// `accounted_through` is the highest sequence the fetch examined, so the span
/// from the requested offset up to it is now fully accounted for: every
/// sequence in it is either present or proven absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accounting {
    requested_from: Sequence,
    accounted_through: Sequence,
    saturated: bool,
}

impl Accounting {
    #[must_use]
    pub fn accounted_through(&self) -> Sequence {
        self.accounted_through
    }

    /// Whether the fetch filled its bound. A saturated fetch says nothing
    /// about what lies beyond it, so the caller must read again to find out.
    #[must_use]
    pub fn saturated(&self) -> bool {
        self.saturated
    }

    /// `true` when the fetch examined nothing, so it proved nothing. An empty
    /// result must not widen an accounted span: there is a difference between
    /// "I looked and it is gone" and "I found nothing to look at".
    #[must_use]
    pub fn proved_nothing(&self) -> bool {
        !self.saturated && self.accounted_through == self.requested_from
    }

    #[must_use]
    pub fn requested_from(&self) -> Sequence {
        self.requested_from
    }
}

/// Derives what a fetch proved from the sequences it returned.
///
/// Takes sequences rather than events because that is all it reads, which
/// keeps it exact and lets its cases be a plain table.
///
/// `returned` must be ascending and strictly greater than `offset`; anything
/// else is a backend contract violation and is ignored rather than trusted.
#[must_use]
pub fn account_for_fetch(offset: Sequence, returned: &[Sequence], max_events: usize) -> Accounting {
    let highest = returned
        .iter()
        .copied()
        .filter(|sequence| *sequence > offset)
        .max()
        .unwrap_or(offset);

    Accounting {
        requested_from: offset,
        accounted_through: highest,
        saturated: max_events > 0 && returned.len() >= max_events,
    }
}

/// A run of sequences proven absent: `[from, through]`, inclusive both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsentRun {
    pub from: Sequence,
    pub through: Sequence,
}

/// The runs of sequences that a fetch proved absent.
///
/// Every sequence strictly between `offset` and the highest returned sequence
/// that is not itself returned has been deleted. Nothing above the highest
/// returned sequence appears here, however the fetch ended.
#[must_use]
pub fn absent_runs(offset: Sequence, returned: &[Sequence]) -> Vec<AbsentRun> {
    let mut runs = Vec::new();
    let mut previous = offset;

    for sequence in returned.iter().copied().filter(|value| *value > offset) {
        // The one site that legitimately computes positions from two
        // sequences, and it is not deriving a quantity: the store returned
        // everything between these two, so everything between them is proven
        // absent. The endpoints are positions in that gap, and the arithmetic
        // is written out rather than hidden behind an operator the type
        // deliberately does not offer.
        let after_previous = Sequence::assigned(previous.as_i64().saturating_add(1));
        if sequence > after_previous {
            runs.push(AbsentRun {
                from: after_previous,
                through: Sequence::assigned(sequence.as_i64().saturating_sub(1)),
            });
        }
        previous = sequence;
    }

    runs
}
