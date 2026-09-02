//! Coordination plugin contract: [`CoordinationPluginV1`].
//!
//! A small lock-based contract for singleton coordination of the gear's
//! background tasks (DESIGN section 3.3, "Coordination Plugin Trait").
//! Consumers are the `LeaseSweeper` and `RetentionSweeper` singletons. The
//! notification dispatcher does not use it; the `toolkit-db` outbox lease
//! fences that path.
//!
//! Contract guarantees every implementation upholds:
//!
//! - **TTL-bounded locks.** A lock never outlives its TTL, even when the
//!   holder process crashed silently. Auto-release at expiry is the
//!   authoritative cleanup path.
//! - **`renew` on or before TTL/3.** A holder that misses the window sees
//!   [`CoordinationError::LockExpired`] on the next call and drops to
//!   follower mode.
//! - **`release` is best-effort.** It is a handoff hint so a cooperating peer
//!   can re-acquire without a wait for the TTL. It never fails the holder.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Closed set of coordination scopes. Exactly one holder per scope at a time.
///
/// The closed shape rules out free-form string keys and gives the
/// implementation a deterministic key namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockScope {
    /// Physical reclamation of expired leases.
    LeaseSweeper,
    /// Idempotency-record and operation-log retention.
    RetentionSweeper,
}

impl LockScope {
    /// Every scope value. Bootstrap probes each one with `try_lock` + `release`.
    pub const ALL: [Self; 2] = [Self::LeaseSweeper, Self::RetentionSweeper];

    /// Stable key an implementation may use to namespace its lock rows.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::LeaseSweeper => "lease_sweeper",
            Self::RetentionSweeper => "retention_sweeper",
        }
    }
}

impl fmt::Display for LockScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// Opaque holder token returned by [`CoordinationPluginV1::try_lock`].
///
/// Holders treat the value as opaque. Only the implementation that minted it
/// interprets the fields. It is distinct from the domain `Lease` (a quota
/// capacity hold managed by the storage plugin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    scope: LockScope,
    holder_id: Uuid,
    ttl: Duration,
    acquired_at: OffsetDateTime,
}

impl Lock {
    /// Mint a lock token. Called by plugin implementations only.
    #[must_use]
    pub const fn new(
        scope: LockScope,
        holder_id: Uuid,
        ttl: Duration,
        acquired_at: OffsetDateTime,
    ) -> Self {
        Self {
            scope,
            holder_id,
            ttl,
            acquired_at,
        }
    }

    /// The scope this lock holds.
    #[must_use]
    pub const fn scope(&self) -> LockScope {
        self.scope
    }

    /// Holder identity minted per acquisition cycle (`UUIDv7`).
    #[must_use]
    pub const fn holder_id(&self) -> Uuid {
        self.holder_id
    }

    /// TTL granted at acquisition.
    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Acquisition time on the holder's clock.
    #[must_use]
    pub const fn acquired_at(&self) -> OffsetDateTime {
        self.acquired_at
    }

    /// Latest renewal point that keeps the contract: TTL/3 after acquisition.
    #[must_use]
    pub fn renew_by(&self) -> OffsetDateTime {
        self.acquired_at + self.ttl / 3
    }
}

/// Closed error set of [`CoordinationPluginV1`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoordinationError {
    /// Another holder owns the scope.
    #[error("lock scope {scope} is held by another holder")]
    Conflict {
        /// The contended scope.
        scope: LockScope,
    },

    /// `renew` or `release` was issued on a lock whose TTL has elapsed.
    #[error("lock on scope {scope} expired before the operation")]
    LockExpired {
        /// The scope of the expired lock.
        scope: LockScope,
    },

    /// Transport or backend reachability failure.
    #[error("coordination backend unavailable: {0}")]
    BackendUnavailable(String),

    /// Last-resort opaque failure.
    #[error("coordination plugin internal error: {0}")]
    Internal(String),
}

/// Backend-agnostic singleton coordination for the gear's background tasks.
///
/// Registered by a plugin gear as a scoped `ClientHub` client under its GTS
/// instance id. The gear resolves it by vendor through the types registry.
// @cpt-dod:cpt-cf-quota-enforcement-dod-sdk-contracts:p1
#[async_trait]
pub trait CoordinationPluginV1: Send + Sync + 'static {
    /// Grant `scope` to exactly one holder for `ttl`.
    ///
    /// # Errors
    ///
    /// - [`CoordinationError::Conflict`] when another live holder owns the scope.
    /// - [`CoordinationError::BackendUnavailable`] when the backend cannot answer.
    async fn try_lock(&self, scope: LockScope, ttl: Duration) -> Result<Lock, CoordinationError>;

    /// Extend a live hold by its TTL.
    ///
    /// # Errors
    ///
    /// - [`CoordinationError::LockExpired`] when the hold already elapsed or a
    ///   peer took the scope over. The holder must drop to follower mode.
    /// - [`CoordinationError::BackendUnavailable`] when the backend cannot answer.
    async fn renew(&self, lock: &Lock) -> Result<(), CoordinationError>;

    /// Hand the scope back before the TTL elapses. Best-effort: a hold that a
    /// peer already took over is not an error.
    ///
    /// # Errors
    ///
    /// - [`CoordinationError::BackendUnavailable`] when the backend cannot answer.
    async fn release(&self, lock: Lock) -> Result<(), CoordinationError>;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "coordination_plugin_tests.rs"]
mod coordination_plugin_tests;
