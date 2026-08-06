//! The ordinal vocabulary of the event log.
//!
//! A sequence is a *position*, not a measure. The space is assigned contiguously
//! by storage but populated sparsely, because retention removes prefixes and one
//! partition carries many tenants' events while a single subscription sees a
//! subset. Two consequences follow, and this module exists to make both of them
//! the compiler's business rather than review's:
//!
//! - The distance between two sequences is not a count of anything. A quantity
//!   comes from counting real events.
//! - The value after a position is not that position plus one. Nothing steps to
//!   a neighbour.
//!
//! So [`Sequence`] implements no arithmetic at all - no `Add`, no `Sub`, no
//! `next`, no `previous`.
//!
//! Exclusivity is not modelled here, because it is not a per-call property:
//! **every** read of the log returns positions strictly greater than the one it
//! was given, since the next populated sequence is unknowable. That rule is
//! stated once on the backend contract rather than carried by a type at each
//! call site.
//!
//! The producer chain's numbering (`meta.previous` / `meta.sequence`) is a
//! different space with its own contiguity rules and deliberately does not use
//! this type.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A position in one partition's sequence space.
///
/// Ordered and comparable, never subtractable. Sequences an event can hold start
/// at 1, so [`Sequence::NONE`] - zero - is never an event's sequence and is
/// therefore available to mean "no position yet", which is what makes it usable
/// as the initial cursor.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Sequence(i64);

impl Sequence {
    /// No position. Zero is never assigned to an event, so a cursor holding it
    /// means nothing has been processed and delivery begins at the partition's
    /// lowest stored sequence.
    pub const NONE: Self = Self(0);

    /// Wraps a value that storage has already assigned.
    ///
    /// **For storage backends.** A sequence comes into existence when the
    /// component that persists an event assigns it, and this is the seam where
    /// that value enters the type - both when it is minted and when a stored row
    /// is read back. Backends live in their own crates, so no visibility
    /// modifier can restrict this; the name is the fence. Anywhere else, a
    /// sequence should be compared, carried, or handed back, never constructed.
    ///
    /// Deserialization is the other construction path, and legitimately so: a
    /// sequence arriving on the wire was assigned by the broker that sent it.
    #[must_use]
    pub const fn assigned(value: i64) -> Self {
        Self(value)
    }

    /// The underlying value, for a storage or wire boundary that must speak
    /// integers. Deliberately explicit: every crossing states which side it is
    /// on rather than relying on the two being the same type.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
    }

    /// Whether this is [`Sequence::NONE`] - no position rather than a position.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == Self::NONE.0
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
