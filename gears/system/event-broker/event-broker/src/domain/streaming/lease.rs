//! At-most-one open stream per subscription, as ownership rather than as a
//! marker somebody has to remember to clear.
//!
//! In-process only. A consumer group is owned by exactly one delivery instance,
//! so this invariant has nothing distributed about it: a set behind a mutex, not
//! a method on a storage facade that implies a shared source of truth.
//!
//! The lease is released on `Drop`, and it is held by the session itself. That
//! is the whole point of the type. The alternative - a marker plus a separate
//! drop guard - made correctness depend on a handler remembering not to
//! destructure the returned handle, which is exactly the kind of invariant that
//! should be expressed by who owns what.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use uuid::Uuid;

/// Who may open a stream for a subscription.
pub trait StreamLeases: Send + Sync {
    /// Takes the lease, or `None` when a stream is already open for
    /// `subscription_id`.
    fn acquire(&self, subscription_id: Uuid) -> Option<StreamLease>;

    /// Whether a stream is open. Every call carrying a subscription id except
    /// `DELETE` consults this - a second `:stream`, a `:seek`, and a plain
    /// `GET` all conflict with an open stream.
    fn is_held(&self, subscription_id: Uuid) -> bool;
}

/// The set of subscriptions currently streaming.
#[derive(Debug, Default)]
pub struct InProcessStreamLeases {
    open: Arc<Mutex<HashSet<Uuid>>>,
}

impl InProcessStreamLeases {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recovers a poisoned guard rather than refusing every later stream.
    ///
    /// Nothing in the critical section can panic - it is a set insert - so
    /// poisoning here would mean something impossible happened. Treating it as
    /// a failure would be worse than continuing: the poison is sticky, so one
    /// panic would deny every subscription a stream for the life of the
    /// process, and the leases would never be released either.
    fn open(&self) -> MutexGuard<'_, HashSet<Uuid>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many streams are open. For observability and tests; the invariant
    /// itself is enforced by `acquire`.
    #[must_use]
    pub fn held_count(&self) -> usize {
        self.open().len()
    }
}

impl StreamLeases for InProcessStreamLeases {
    fn acquire(&self, subscription_id: Uuid) -> Option<StreamLease> {
        // `insert` returns false when the id was already present, which is the
        // conflict - checked and taken in one critical section, so two
        // concurrent opens cannot both see it free.
        if !self.open().insert(subscription_id) {
            return None;
        }
        Some(StreamLease {
            subscription_id,
            open: Arc::clone(&self.open),
        })
    }

    fn is_held(&self, subscription_id: Uuid) -> bool {
        self.open().contains(&subscription_id)
    }
}

/// One subscription's claim on streaming, released when this value is dropped.
///
/// Holds its own handle on the set rather than a reference to the registry, so
/// the lease can be owned by a session that outlives any borrow of it.
pub struct StreamLease {
    subscription_id: Uuid,
    open: Arc<Mutex<HashSet<Uuid>>>,
}

impl StreamLease {
    #[must_use]
    pub fn subscription_id(&self) -> Uuid {
        self.subscription_id
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        // Same reasoning as `open()`: a poisoned lock must not leak the lease,
        // or the subscription could never stream again.
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.subscription_id);
    }
}

impl std::fmt::Debug for StreamLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamLease")
            .field("subscription_id", &self.subscription_id)
            .finish_non_exhaustive()
    }
}
