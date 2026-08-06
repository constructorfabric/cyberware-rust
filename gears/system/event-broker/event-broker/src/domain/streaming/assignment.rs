//! Classifying an assignment change into the one thing a session does about it.
//!
//! The coordinator computes assignments and publishes them; it does not decide
//! frames. This module is the whole decision, and it exists as its own type
//! because the mapping from a change to a frame has to happen in exactly one
//! place - five cases, one emission rule each - rather than being re-derived
//! wherever a generation arrives.
//!
//! Pure: no clock, no channel, no frame. What a case *means* is the session's.

use crate::domain::model::Assignment;

/// One published view of a member's assignment.
///
/// Public fields, because this is a data carrier published on a `watch` channel
/// rather than a value with invariants to protect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub topology_version: i64,
    pub assigned: Vec<Assignment>,
}

impl Generation {
    /// Two arguments, but a version and a list of assignments cannot be
    /// transposed.
    #[must_use]
    pub fn new(topology_version: i64, assigned: Vec<Assignment>) -> Self {
        Self {
            topology_version,
            assigned,
        }
    }

    /// Whether this generation holds `other`'s partition.
    ///
    /// Compared on `(topic, partition)` only. `offset` and `last_examined` are
    /// deliberately excluded: they still exist on the struct for the SDK's sake
    /// and the broker no longer reads them - the starting position comes from
    /// the cursor store and the frontier from the session's own accounting.
    /// Comparing whole structs would report a topology change whenever a stale
    /// offset differed.
    fn holds(&self, other: &Assignment) -> bool {
        self.assigned
            .iter()
            .any(|held| held.topic == other.topic && held.partition == other.partition)
    }
}

/// What changed between two generations, and therefore what the session emits.
///
/// One variant per emission rule: `Unchanged` emits nothing; `VersionOnly` and
/// `Loss` emit a non-terminal `topology` frame and the stream continues; `Gain`
/// and `LoseAll` emit a `terminal` control frame and the stream then closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignmentDelta {
    /// Same partitions, same version. Nothing to say.
    Unchanged,
    /// Same partitions, new version - a rebalance that did not move this
    /// member. The consumer still needs the version, because it is what makes a
    /// later position report attributable to a topology.
    VersionOnly { topology_version: i64 },
    /// Partitions were taken away and some remain. The stream continues on what
    /// is left.
    Loss {
        topology_version: i64,
        retained: Vec<Assignment>,
    },
    /// Partitions were added.
    ///
    /// Terminal, which is counter-intuitive until the reason is stated: a gained
    /// partition has no cursor in this session, and its correct starting offset
    /// is whatever the group committed. Continuing the stream would mean either
    /// replaying from zero or guessing, so the session hands its frontier back
    /// and the consumer re-JOINs and re-SEEKs.
    Gain,
    /// Every partition taken away. There is nothing left to stream.
    LoseAll,
}

impl AssignmentDelta {
    /// Classifies the move from `current` to `next`.
    ///
    /// Order is load-bearing. `LoseAll` is checked before `Gain` because they
    /// cannot both hold - losing everything gains nothing - and before `Loss`
    /// because it is the case where nothing is retained. `Gain` is then checked
    /// before `Loss`, which is what "gain dominates a simultaneous
    /// gain-plus-loss" means: a rebalance that both adds and removes partitions
    /// terminates, because the added ones still have no cursor here.
    ///
    /// Not for seeding. A session's first assignment is not a delta, and
    /// classifying it against an empty generation would report `Gain` and
    /// terminate a stream that had not started.
    #[must_use]
    pub fn classify(current: &Generation, next: &Generation) -> Self {
        let gained = next.assigned.iter().any(|held| !current.holds(held));
        let lost = current.assigned.iter().any(|held| !next.holds(held));

        if lost && next.assigned.is_empty() {
            return Self::LoseAll;
        }
        if gained {
            return Self::Gain;
        }
        if lost {
            return Self::Loss {
                topology_version: next.topology_version,
                retained: next.assigned.clone(),
            };
        }
        if next.topology_version == current.topology_version {
            return Self::Unchanged;
        }
        Self::VersionOnly {
            topology_version: next.topology_version,
        }
    }

    /// Whether this delta ends the stream. The session's single branch point
    /// between a `topology` frame and a `terminal` one.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Gain | Self::LoseAll)
    }
}
