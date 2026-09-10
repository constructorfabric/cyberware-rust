//! Recognize unchanged revisions before loading or resolving their dependencies.

use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::secure::AccessScope;
use toolkit_macros::domain_model;

use super::errors::{ItemFailure, WorkerError};
use super::revision::{
    RevisionCommit, read_current_content, revision_entity, terminalize_unchanged,
};
use crate::domain::admission::{AdmissionFailureReason, Precondition};
use crate::domain::artifacts::content_hash;
use crate::domain::enums::LifecycleStatus;
use crate::domain::ports::{OperationItemRow, Stores};

/// Proof that the submitted bytes equal the current authored document in one
/// snapshot. Private fields keep callers from constructing an unverified proof.
#[domain_model]
#[derive(Clone, Debug)]
pub struct UnchangedCandidate {
    gts_id: String,
    entity_id: i64,
    resource_version: i64,
    operation_item_id: i64,
}

/// A read-only probe, restricted to revisions. A miss falls through to ordinary
/// evaluation; it neither claims the write order nor records an item outcome.
pub(super) async fn probe(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    item: &OperationItemRow,
    payload: &str,
) -> Result<Option<UnchangedCandidate>, WorkerError> {
    let Precondition::Version(expected) = item.precondition else {
        return Ok(None);
    };
    let Some(entity) = stores.find_by_gts_id(tx, scope, &item.gts_id).await? else {
        return Ok(None);
    };
    if entity.lifecycle_status == LifecycleStatus::Deleted || entity.resource_version != expected {
        return Ok(None);
    }
    let current = read_current_content(
        stores,
        tx,
        scope,
        &item.gts_id,
        entity.entity_kind,
        entity.id,
    )
    .await?;
    // A digest is only a prefilter: collisions must not swallow edits.
    Ok(current
        .matches_authored(&content_hash(payload), payload)
        .then_some(UnchangedCandidate {
            gts_id: item.gts_id.clone(),
            entity_id: entity.id,
            resource_version: expected,
            operation_item_id: item.id,
        }))
}

/// Commit a snapshot proof only if the same live entity still has that version.
/// Every authored-content write advances `resource_version` under this same lock;
/// dependency refreshes may change artifacts, but cannot change authored content.
pub(super) async fn commit(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    candidate: &UnchangedCandidate,
    now: OffsetDateTime,
) -> Result<Result<RevisionCommit, ItemFailure>, WorkerError> {
    // Must remain the first SQL statement, including on transaction retry.
    stores.claim_entity_write_order(tx, scope, now).await?;
    let entity = match revision_entity(
        stores,
        tx,
        scope,
        &candidate.gts_id,
        candidate.resource_version,
    )
    .await?
    {
        Ok(entity) => entity,
        Err(failure) => return Ok(Err(failure)),
    };
    if entity.id != candidate.entity_id {
        return Ok(Err(ItemFailure::new(
            AdmissionFailureReason::PreconditionFailed,
            format!(
                "'{}' was replaced after the content comparison",
                candidate.gts_id
            ),
        )));
    }
    terminalize_unchanged(stores, tx, scope, &entity, candidate.operation_item_id, now).await
}
