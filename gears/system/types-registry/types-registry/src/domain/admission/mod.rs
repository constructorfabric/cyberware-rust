//! Admission: the synchronous acceptance half (T7) and the request identity it
//! rests on.
//!
//! The split is SPEC §8.1's: **acceptance** runs in the caller's task, reads no
//! entity state, and either refuses synchronously or commits one operation row,
//! its items and an outbox message in a single transaction. **Admission** — the
//! worker — is a separate pass driven by that outbox message.
//!
//! Everything before the transaction is a pure function of the request and the
//! configuration. That is not a style choice: SPEC §8.1's ordering invariant is
//! that the policy gate precedes any existence lookup, so a refusal cannot probe
//! the namespace, and the cheapest way to keep that true is for the refusing code
//! to have no database in scope at all.

pub mod acceptance;
mod bounds;
mod drift;
mod errors;
mod reasons;

pub mod fingerprint;
pub mod refresh;
pub mod unit;
pub mod vector;
pub mod worker;

pub use reasons::AdmissionFailureReason;

use serde_json::Value;
use toolkit_db::DbTx;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::enums::{OperationKind, OperationStatus};

/// One candidate in a submitted request.
#[domain_model]
#[derive(Clone, Debug)]
pub struct Candidate {
    /// The identifier as authored. Canonicalized through `GtsId::try_new` during
    /// acceptance; a non-canonical spelling is refused rather than rewritten.
    pub gts_id: String,
    /// The authored document. Absent for a deletion.
    pub content: Option<Value>,
    /// The optimistic precondition. **`None` means must-not-exist**; `Some(0)` is
    /// refused, because the wire vocabulary spells must-not-exist as an absent
    /// field and a literal `0` is more likely a serialization accident than an
    /// intent (`database.sql`).
    pub expected_resource_version: Option<i64>,
    /// ADR-0004 `force`: waive one cross-minor compatibility check.
    pub force: bool,
}

/// The closed optimistic-precondition vocabulary used after acceptance.
///
/// REST spells creation as an absent `expected_resource_version`; storage spells
/// it as `0`. Neither representation crosses the domain pipeline: adapters map
/// them to and from this enum at their respective boundaries.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Precondition {
    MustNotExist,
    Version(i64),
}

impl Precondition {
    /// The stable integer included in the request fingerprint and persisted in
    /// `operation_item.expected_resource_version`.
    #[must_use]
    pub const fn stored_value(self) -> i64 {
        match self {
            Self::MustNotExist => 0,
            Self::Version(version) => version,
        }
    }

    /// The REST response spelling: absence means must-not-exist.
    #[must_use]
    pub const fn expected_resource_version(self) -> Option<i64> {
        match self {
            Self::MustNotExist => None,
            Self::Version(version) => Some(version),
        }
    }

    /// Validate the persisted closed vocabulary.
    #[must_use]
    pub const fn from_stored(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::MustNotExist),
            1.. => Some(Self::Version(value)),
            _ => None,
        }
    }
}

/// A submitted request, before acceptance.
#[domain_model]
#[derive(Clone, Debug)]
pub struct SubmitRequest {
    /// Mandatory. Absence is a synchronous refusal, not a generated key: a
    /// generated one would make every retry a fresh operation.
    pub idempotency_key: String,
    pub kind: OperationKind,
    pub dry_run: bool,
    pub candidates: Vec<Candidate>,
}

/// What acceptance decided.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub operation_id: Uuid,
    /// `true` when this request resolved to an operation that already existed
    /// under its `Idempotency-Key` with a matching fingerprint.
    pub replayed: bool,
    /// The operation's status as of this call's return — `pending` for a fresh
    /// acceptance, the stored value for a replay, and `completed` once inline
    /// admission has run (T21 removes that last case along with inline admission).
    ///
    /// Carried rather than left to the caller to look up: the REST layer needs it for
    /// the receipt, and re-reading the row it has just written cost a second snapshot
    /// transaction plus a `"pending"` fallback for a `None` that cannot happen.
    pub status: OperationStatus,
}

impl Accepted {
    /// `true` when the operation will not change again. The REST layer answers `200`
    /// for a terminal replay and `202` otherwise (SPEC §8.1), and inline admission is
    /// skipped for one — derived from [`Self::status`] rather than stored beside it,
    /// so the two cannot disagree.
    #[must_use]
    pub fn terminal(&self) -> bool {
        self.status == OperationStatus::Completed
    }
}

/// How an accepted operation reaches the admission worker.
///
/// A port rather than a direct `Outbox` call, because an `Outbox` only exists
/// after `OutboxBuilder::start()` has spawned its processors — which is precisely
/// what T21 wires and what SPEC §13's *"no test may poll"* rule forbids a test
/// from doing. The transaction shape is the same either way: the message is
/// written by the same transaction as the operation, so a committed operation is
/// always dispatched and a rolled-back one never is.
///
/// The runner is the concrete [`DbTx`] rather than `&impl DBRunner`, so the trait
/// stays object-safe: acceptance always dispatches from inside its transaction,
/// so there is no second executor to be generic over.
#[async_trait::async_trait]
pub trait OperationDispatch: Send + Sync {
    /// Enqueue one operation UUID.
    ///
    /// The payload carries the UUID and nothing else — candidate content must
    /// never enter an outbox or dead-letter payload (SPEC T21).
    ///
    /// # Errors
    /// Whatever the transport fails with; acceptance turns it into a refusal and
    /// the transaction rolls back, so nothing is half-accepted.
    async fn enqueue(&self, tx: &DbTx<'_>, operation_id: Uuid) -> anyhow::Result<()>;
}

/// A dispatcher that enqueues nothing.
///
/// Used by the two paths that admit **inline**: seeding, which SPEC §8.1 makes
/// permanent (*"types-registry accepts and admits it itself, inline, with no
/// outbox"*), and API traffic until T21 starts the outbox worker. The dispatch call
/// still happens inside the acceptance transaction, so the shape T21 needs is
/// already in place and swapping the implementation is the whole change.
#[domain_model]
pub struct NullDispatch;

#[async_trait::async_trait]
impl OperationDispatch for NullDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, _operation_id: Uuid) -> anyhow::Result<()> {
        Ok(())
    }
}
