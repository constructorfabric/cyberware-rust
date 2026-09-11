//! What the loader needs from a backend, and nothing more.
//!
//! Deliberately narrower than the storage backend trait: no security context,
//! no query surface, and domain events rather than the SDK projection. The
//! loader is generic over this rather than holding a `dyn`, so an implementation
//! costs no allocation per fetch and a test double is a plain struct.

use std::future::Future;

use crate::domain::model::{Event, Sequence};
use crate::domain::streaming::source::PartitionKey;

/// Why a fetch could not be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The backend is reachable again later. The demand survives.
    Unavailable,
    /// The read itself failed.
    Failed(String),
}

/// One partition's events, read forward from an exclusive offset.
pub trait EventSource: Send + Sync {
    /// Events with `sequence > after`, at most `max_events` of them, ascending.
    ///
    /// An empty result is not an error and does not mean the partition is
    /// finished: a notification can arrive before the backend has assigned the
    /// sequence, so the events may simply not exist yet. Distinguishing "there
    /// is nothing" from "there is nothing *yet*" is not this trait's job - the
    /// poller's backoff is what handles it.
    fn read(
        &self,
        key: &PartitionKey,
        after: Sequence,
        max_events: usize,
    ) -> impl Future<Output = Result<Vec<Event>, SourceError>> + Send;
}
