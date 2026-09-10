//! The revision vocabulary shared by the two paths that can end in an
//! *unchanged* outcome: full evaluation ([`super::unit`]) and the pre-evaluation
//! probe (the private `unchanged` module).
//!
//! It holds no policy of its own — the commit result types, the current-content
//! read, the precondition guard and the terminal `unchanged` write. Both callers
//! depend on this module and neither on the other, so the fast path cannot drift
//! from the outcome the slow path records.

use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::secure::AccessScope;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::errors::{ItemFailure, WorkerError};
use crate::domain::admission::AdmissionFailureReason;
use crate::domain::enums::{EntityKind, LifecycleStatus};
use crate::domain::ports::{CurrentSchemaCas, EntityRow, Stores};

/// The commit's result for one item that wrote a revision.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommittedUnit {
    pub gts_uuid: Uuid,
    pub revision_no: i32,
    pub resource_version: i64,
}

/// What committing a *revision* produced. The outcome is the variant: an
/// `unchanged` candidate allocates no revision number (ADR-0005), and
/// [`super::unit::commit_creation`] cannot reach that outcome at all.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionCommit {
    /// A new immutable revision, the current pointer moved onto it, and
    /// `resource_version` advanced.
    Admitted(CommittedUnit),
    /// The authored content already equalled the current revision: no revision, no
    /// version move. `resource_version` is the one that did not move.
    Unchanged {
        gts_uuid: Uuid,
        resource_version: i64,
    },
}

/// Current authored content; Type Schemas also carry their mandatory artifact CAS token.
#[domain_model]
#[derive(Clone, Debug)]
pub(super) enum CurrentContent {
    TypeSchema {
        revision_no: i32,
        body: String,
        hash: Vec<u8>,
        /// The compare-and-swap token for the artifact write the caller makes next.
        cas: CurrentSchemaCas,
    },
    Instance {
        revision_no: i32,
        body: String,
        hash: Vec<u8>,
    },
}

impl CurrentContent {
    /// The current revision's number, whichever kind the candidate is: the next
    /// revision is allocated from it before the outcome kind picks the write.
    pub(super) fn revision_no(&self) -> i32 {
        match self {
            Self::TypeSchema { revision_no, .. } | Self::Instance { revision_no, .. } => {
                *revision_no
            }
        }
    }

    /// Whether the current content still equals the candidate's authored content —
    /// the `unchanged` test.
    pub(super) fn matches_authored(&self, hash: &[u8], body: &str) -> bool {
        let (current_hash, current_body) = match self {
            Self::TypeSchema { hash, body, .. } | Self::Instance { hash, body, .. } => (hash, body),
        };
        current_hash == hash && current_body == body
    }
}

/// The current revision's number, authored content and digest, whichever kind the
/// candidate is.
///
/// The **authored** content, never the effective artifacts: those move when a
/// dependency moves while the authored document stands still, so including them
/// would report a revision for a document nobody edited.
///
/// # Errors
/// [`WorkerError::CurrentStateMissing`] when the entity row has no current-state
/// row of its kind — corruption, since one transaction writes both (D3).
pub(super) async fn read_current_content(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    gts_id: &str,
    kind: EntityKind,
    entity_id: i64,
) -> Result<CurrentContent, WorkerError> {
    let missing = || WorkerError::CurrentStateMissing {
        gts_id: gts_id.to_owned(),
        entity_id,
    };
    Ok(match kind {
        EntityKind::TypeSchema => {
            let current = stores
                .current_documents(tx, scope, &[entity_id])
                .await?
                .pop()
                .ok_or_else(missing)?;
            CurrentContent::TypeSchema {
                revision_no: current.revision_no,
                body: current.raw_schema,
                hash: current.content_hash,
                cas: current.projection,
            }
        }
        EntityKind::Instance => {
            let current = stores
                .current_values(tx, scope, &[entity_id])
                .await?
                .pop()
                .ok_or_else(missing)?;
            CurrentContent::Instance {
                revision_no: current.revision_no,
                body: current.canonical_value,
                hash: current.content_hash,
            }
        }
    })
}

/// Check the revision precondition after acquiring the entity write order.
pub(super) async fn revision_entity(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    gts_id: &str,
    expected_resource_version: i64,
) -> Result<Result<EntityRow, ItemFailure>, WorkerError> {
    let Some(entity) = stores.find_by_gts_id(tx, scope, gts_id).await? else {
        return Ok(Err(ItemFailure::new(
            AdmissionFailureReason::PreconditionFailed,
            format!(
                "'{gts_id}' does not exist; expected_resource_version {expected_resource_version} \
                 requires it to exist at that version"
            ),
        )));
    };
    // Before the version, because a tombstone is not a stale version: reporting
    // `precondition_failed` would send the caller to retry against a row that will
    // never accept a revision. Both revision paths use this check; the lookup
    // returns tombstones because the family rules need them.
    if entity.lifecycle_status == LifecycleStatus::Deleted {
        return Ok(Err(ItemFailure::new(
            AdmissionFailureReason::EntityDeleted,
            format!("'{gts_id}' is deleted; a revision cannot be admitted onto a withdrawn entity"),
        )));
    }
    if entity.resource_version != expected_resource_version {
        return Ok(Err(stale_precondition(
            gts_id,
            expected_resource_version,
            entity.resource_version,
        )));
    }

    Ok(Ok(entity))
}

/// Record the shared terminal outcome after the caller's entity guards pass.
pub(super) async fn terminalize_unchanged(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    entity: &EntityRow,
    operation_item_id: i64,
    now: OffsetDateTime,
) -> Result<Result<RevisionCommit, ItemFailure>, WorkerError> {
    if !stores
        .mark_item_unchanged(tx, scope, operation_item_id, entity.resource_version, now)
        .await?
    {
        return Err(WorkerError::ItemAlreadyTerminal {
            item_id: operation_item_id,
        });
    }
    Ok(Ok(RevisionCommit::Unchanged {
        gts_uuid: entity.gts_uuid,
        resource_version: entity.resource_version,
    }))
}

/// The refusal for a precondition that was already wrong when read — the entry
/// check and the `unchanged` re-read. The lost compare-and-swap words it
/// differently on purpose, knowing only that the version *moved*, not what to; both
/// carry the same `reason`, so a client branching on it sees one outcome.
pub(super) fn stale_precondition(gts_id: &str, expected: i64, found: i64) -> ItemFailure {
    ItemFailure::new(
        AdmissionFailureReason::PreconditionFailed,
        format!(
            "'{gts_id}' is at resource_version {found}, not the expected {expected}; a revision \
             is never rebased onto the current version"
        ),
    )
}
