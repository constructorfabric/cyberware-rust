//! What a caller can learn about a traced batch after the fact.
//!
//! The in-memory subscription is the normal way to hear about a completion, and
//! it dies with the process that held it. This is the durable fallback: the
//! trace row outlives both the messages it describes and the instance that
//! enqueued them, so a restarted process can still ask what became of work it
//! submitted before it died.

/// What a caller is told when its batch finishes.
///
/// Delivered once, to the instance that enqueued the batch, whichever instance
/// processed the entities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceOutcome {
    /// The trace the caller supplied.
    pub trace: String,
    /// How many entities the batch held.
    pub entities: i64,
    /// How many of them were dead-lettered rather than delivered.
    pub failures: i64,
    /// Handler attempts spent on the batch's final entity.
    pub attempts: i64,
    /// When the last entity reached a terminal state.
    pub completed_at: chrono::DateTime<chrono::Utc>,
}

impl TraceOutcome {
    /// Whether every entity was delivered rather than dead-lettered.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.failures == 0
    }
}

/// What a subscriber is told while its batch is still in flight.
///
/// Pushed only when the batch is *stuck* rather than merely slow: it appears
/// once a handler has retried one of the batch's entities, and is withdrawn
/// when the batch moves again. A batch that flows straight through produces no
/// progress at all, which is why subscribing to it costs nothing until
/// something goes wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceProgress {
    /// The trace the caller supplied.
    pub trace: String,
    /// How many entities the batch held.
    pub entities: i64,
    /// How many have not yet reached a terminal state.
    pub pending: i64,
    /// How many reached a terminal state by being dead-lettered.
    pub failures: i64,
    /// Handler attempts against the entity currently blocking the batch.
    pub attempts: i64,
    /// Why the blocking entity was last retried.
    pub last_error: Option<String>,
    /// When the batch first stopped making progress.
    pub stalled_since: chrono::DateTime<chrono::Utc>,
}

impl TraceProgress {
    /// How long the batch has been stuck, as of now.
    #[must_use]
    pub fn stalled_for(&self) -> chrono::TimeDelta {
        chrono::Utc::now() - self.stalled_since
    }
}

/// What an ack's countdown did to a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceAdvance {
    /// This advance drove the batch to zero, so it may be claimable.
    Completed,
    /// Entities remain, so there is nothing to claim.
    StillPending,
    /// The guard matched no row: somebody else's ack got there first.
    NotAffected,
    /// The dialect cannot report it without another read, so the caller must
    /// try the claim to find out.
    Unknown,
}

/// The state of one traced batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceStatus {
    /// The trace the caller supplied.
    pub trace: String,
    /// The queue the batch was enqueued to.
    pub queue: String,
    /// How many entities the batch held.
    pub entities: i64,
    /// How many have not yet reached a terminal state. Zero is completion.
    pub pending: i64,
    /// How many reached a terminal state by being dead-lettered.
    pub failures: i64,
    /// Handler attempts against the entity currently blocking the batch.
    pub attempts: i64,
    /// Why the blocking entity was last retried.
    pub last_error: Option<String>,
    /// When the batch first stopped making progress, cleared when it resumes.
    pub stalled_since: Option<chrono::DateTime<chrono::Utc>>,
    /// When the batch was enqueued.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When every entity had reached a terminal state.
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl TraceStatus {
    /// Whether every entity in the batch has reached a terminal state.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.pending == 0
    }

    /// Whether the batch is stuck rather than merely slow.
    #[must_use]
    pub const fn is_stalled(&self) -> bool {
        self.stalled_since.is_some()
    }
}
