//! A range of sequences the cache has accounted for, and the questions asked of
//! one.
//!
//! Small and shared on purpose. Three separate places used to ask their own
//! version of "does this range serve a reader standing here": the wake path,
//! reclamation's retained windows, and the byte-pressure victim filter. They
//! agreed, but only by coincidence, and a change to any one of them would have
//! broken liveness silently - a reader woken for a span reclaimed in the same
//! breath parks with nothing left to wake it. There is one predicate now.

use crate::domain::model::Sequence;

/// A span `from..=through`, inclusive at both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountedSpan {
    from: Sequence,
    through: Sequence,
}

impl AccountedSpan {
    /// Built through a builder: both ends are sequences, and a positional pair
    /// would let a caller transpose them and silently invert the span.
    #[must_use]
    pub fn builder(from: Sequence) -> AccountedSpanBuilder {
        AccountedSpanBuilder {
            from,
            through: from,
        }
    }

    #[must_use]
    pub fn from(self) -> Sequence {
        self.from
    }

    #[must_use]
    pub fn through(self) -> Sequence {
        self.through
    }

    /// Whether this span accounts for `sequence` - so a reader positioned there
    /// can be answered, with an event or with the knowledge that none exists.
    #[must_use]
    pub fn contains(self, sequence: Sequence) -> bool {
        sequence >= self.from && sequence <= self.through
    }

    /// Whether this span answers what a reader at `position` wants next.
    ///
    /// **The unified predicate.** Offsets are exclusive, so the reader at
    /// `position` has consumed it and wants `position + 1`. Everything that
    /// reasons about a reader and a span goes through here: the wake path uses
    /// it to decide whom a fetch serves, and reclamation uses it to decide what
    /// must not be taken. Those two have to be the same question, or a fetch can
    /// wake a reader for a span the same pass is about to drop.
    #[must_use]
    pub fn serves(self, position: Sequence) -> bool {
        self.contains(Sequence::assigned(position.as_i64().saturating_add(1)))
    }

    /// Whether this span ends immediately before `next` begins, so a read may
    /// cross from one into the other. Exact, and indifferent to holes in either.
    #[must_use]
    pub fn is_adjacent_to(self, next: Self) -> bool {
        Sequence::assigned(self.through.as_i64().saturating_add(1)) == next.from
    }
}

pub struct AccountedSpanBuilder {
    from: Sequence,
    through: Sequence,
}

impl AccountedSpanBuilder {
    #[must_use]
    pub fn through(mut self, through: Sequence) -> Self {
        self.through = through;
        self
    }

    #[must_use]
    pub fn build(self) -> AccountedSpan {
        AccountedSpan {
            from: self.from,
            // Normalised rather than trusted, so an inverted span cannot exist
            // even if one is asked for.
            through: self.through.max(self.from),
        }
    }
}
