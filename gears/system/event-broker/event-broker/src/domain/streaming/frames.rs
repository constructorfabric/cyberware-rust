//! The four frame kinds, and the position they report.
//!
//! Every frame on a consumption stream is constructed by the session. Nothing
//! else constructs one, which is what makes the mapping from an assignment
//! change to a frame a single classification with one emission rule per case
//! rather than a decision re-made wherever a generation arrives.

use chrono::{DateTime, Utc};
use toolkit::domain_model;
use toolkit_gts::GtsInstanceId;

use crate::domain::model::{Event, Sequence};

/// Where one partition stands, as reported to the consumer.
///
/// Separate from `Assignment`, which is *membership* - which partitions a member
/// holds. This is progress, and the two were one type until it became clear that
/// carrying a position on an assignment invited reading a stale offset as the
/// starting point. A position is per session and never persisted on a member.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub topic: GtsInstanceId,
    pub partition: i32,
    /// Last sequence *delivered*. The consumer's cursor, and what a resume
    /// starts one above.
    pub offset: Sequence,
    /// Last sequence *examined*, delivered or not.
    ///
    /// The two diverge exactly when a filter rejects events, which is the case
    /// the progress frame exists for: a subscription matching one event in a
    /// million would otherwise look idle, with no way to commit the ground it
    /// has covered.
    pub last_examined: Sequence,
}

impl Position {
    /// One argument, the partition it describes; the two sequences are set
    /// through the builder so they cannot be transposed.
    #[must_use]
    pub fn builder(topic: GtsInstanceId, partition: i32) -> PositionBuilder {
        PositionBuilder {
            topic,
            partition,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        }
    }
}

pub struct PositionBuilder {
    topic: GtsInstanceId,
    partition: i32,
    offset: Sequence,
    last_examined: Sequence,
}

impl PositionBuilder {
    #[must_use]
    pub fn offset(mut self, offset: Sequence) -> Self {
        self.offset = offset;
        self
    }

    #[must_use]
    pub fn last_examined(mut self, last_examined: Sequence) -> Self {
        self.last_examined = last_examined;
        self
    }

    #[must_use]
    pub fn build(self) -> Position {
        Position {
            topic: self.topic,
            partition: self.partition,
            offset: self.offset,
            // A frontier cannot be behind what was delivered: everything
            // delivered was examined. Normalised rather than trusted, so a
            // caller that sets only `offset` still reports a coherent pair.
            last_examined: self.last_examined.max(self.offset),
        }
    }
}

/// Why a stream ended.
///
/// Named rather than a free-text string, because the consumer branches on it:
/// a rebalance means re-JOIN and re-SEEK, a teardown means the broker gave up
/// and the consumer should retry later.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The assignment changed in a way the stream cannot continue through - a
    /// gain, whose partitions have no cursor here.
    Rebalanced,
    /// Every partition was taken away.
    LoseAll,
    /// The broker is stopping this stream: a sustained read failure, or a
    /// shutdown.
    Teardown,
}

impl CloseReason {
    /// The wire spelling. Kept beside the variant so the two cannot drift.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Rebalanced => "rebalanced",
            Self::LoseAll => "lose_all",
            Self::Teardown => "teardown",
        }
    }
}

#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCode {
    Progress,
    Terminal,
}

/// One frame on the consumption stream.
#[domain_model]
#[derive(Debug, Clone)]
pub enum Frame {
    Event(Box<Event>),
    Heartbeat {
        at: DateTime<Utc>,
    },
    /// A non-terminal topology report: the stream continues.
    Topology {
        topology_version: i64,
        positions: Vec<Position>,
    },
    /// A frontier report, or the last frame before a close.
    Control {
        code: ControlCode,
        positions: Vec<Position>,
        reason: Option<CloseReason>,
    },
}
