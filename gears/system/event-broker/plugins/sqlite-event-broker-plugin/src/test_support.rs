//! A backend over a private event log, shared by this crate's tests.

use crate::backend::SqliteEventBackend;
use crate::options::EventLogPath;

/// A backend over an event log of its own, with this backend's tables applied
/// by the same code path a deployment runs.
///
/// In memory, so nothing a test writes reaches a file, and - the point of these
/// tests - nothing reaches whatever database a host gear happens to own.
pub async fn test_backend() -> SqliteEventBackend {
    SqliteEventBackend::open(&EventLogPath::InMemory)
        .await
        .expect("an in-memory event log must open")
}
