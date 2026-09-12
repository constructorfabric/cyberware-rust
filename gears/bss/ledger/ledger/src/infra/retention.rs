//! `CheckpointWriter` + [`DetachGate`] — the dormant retention seam (Slice 6
//! design §4.8, Variant 2).
//!
//! Partitioning / rotation of the journal is Foundation (Slice-1) debt: nothing
//! in the MVP rotates a partition yet. This module ships the INTERFACE that
//! Foundation's rotation will call once it exists:
//!
//! - [`CheckpointWriter`] records a contiguous range of a tenant's
//!   tamper-evidence hash chain into `bss.chain_checkpoint` (so a detached
//!   partition can be proven anchored by a checkpoint). Signing / WORM storage
//!   is post-MVP (Bucket A) — an MVP checkpoint is written UNSIGNED
//!   (`signature` NULL).
//!
//! - [`DetachGate`] is the §4.8/E-5 detach gate. The fuller normative rule is
//!   "a period MAY be detached only when it is COVERED by a signed checkpoint
//!   AND retired". The checkpoint-coverage half is dormant until rotation
//!   exists; the MVP gate enforces the half that has LIVE data today: the
//!   period's chain must be fully sealed (every `journal_entry.row_hash` set).
//!   An unsealed entry means the tamper chain for that period is not closed, so
//!   detaching it would orphan an un-anchored row — the gate blocks it.
//!
//! The §4.8/E-7 "restore-event chain re-anchor" (re-link a restored partition
//! back onto the live chain) is a documented FUTURE interface; it is not built
//! here.
//!
//! A blocked detach MAPS to
//! [`AlarmCategory::PartitionDetachBlocked`](crate::infra::events::payloads::AlarmCategory::PartitionDetachBlocked).
//! This module does NOT emit that alarm — emission is wired by Foundation's
//! rotation when it calls [`DetachGate::may_detach`] and gets an `Err`; here the
//! gate only reports the blocking condition.

use std::collections::HashSet;

use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{AccessScope, DbConn, SecureEntityExt, SecureInsertExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::model::RepoError;
use crate::infra::storage::entity::{chain_checkpoint, journal_entry};
use time::OffsetDateTime;

/// Writer over `bss.chain_checkpoint`. Stateless over one [`DBProvider`]
/// (mirrors [`crate::infra::audit::retrieval::AuditRetrievalReader`]).
#[derive(Clone)]
pub struct CheckpointWriter {
    db: DBProvider<DbError>,
}

impl CheckpointWriter {
    /// Build the writer over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// The underlying provider.
    #[must_use]
    pub fn db(&self) -> &DBProvider<DbError> {
        &self.db
    }

    /// Insert ONE `chain_checkpoint` row under `scope`, recording the contiguous
    /// chain range `from_row_hash` .. `to_row_hash`. Returns the new
    /// `checkpoint_id` ([`Uuid::now_v7`]-generated). SQL-level tenant isolation
    /// via the secure ORM insert.
    ///
    /// `covered_entry_count` is **derived**: the writer walks the
    /// chain from `to_row_hash` back to `from_row_hash` via `prev_hash`, counts
    /// the entries, and verifies the range is contiguous. A caller-supplied count
    /// is never trusted — a checkpoint that claims coverage it doesn't have would
    /// let a partition detach over an un-anchored gap.
    ///
    /// `signature` is written NULL: signing / WORM storage is post-MVP
    /// (Bucket A). In the MVP a checkpoint just records a contiguous range of
    /// the chain, unsigned.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure, or when the range is not
    /// contiguous (`to` does not reach `from` by following `prev_hash`, or a link
    /// names a missing/unsealed entry, or a cycle is detected).
    pub async fn write_checkpoint(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        from_row_hash: Vec<u8>,
        to_row_hash: Vec<u8>,
    ) -> Result<Uuid, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;

        // Derive + verify the covered count by walking the chain range.
        let covered_entry_count =
            count_chain_range(&conn, scope, tenant_id, &from_row_hash, &to_row_hash).await?;

        let checkpoint_id = Uuid::now_v7();
        let am = chain_checkpoint::ActiveModel {
            checkpoint_id: Set(checkpoint_id),
            tenant_id: Set(tenant_id),
            from_row_hash: Set(from_row_hash),
            to_row_hash: Set(to_row_hash),
            covered_entry_count: Set(covered_entry_count),
            // Signing / WORM is post-MVP (Bucket A) — an MVP checkpoint is unsigned.
            signature: Set(None),
            created_at_utc: Set(OffsetDateTime::now_utc()),
        };

        chain_checkpoint::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("write_checkpoint scope: {e}")))?
            .exec(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("write_checkpoint: {e}")))?;

        Ok(checkpoint_id)
    }
}

/// Walk a tenant's tamper-evidence chain from `to_row_hash` back to
/// `from_row_hash` (inclusive) via `prev_hash`, returning the number of entries
/// in the range. Verifies contiguity: every link must resolve to a sealed
/// `journal_entry`, and `from_row_hash` must be reached before genesis. Used to
/// derive a checkpoint's `covered_entry_count` so it can never
/// over-claim coverage.
///
/// # Errors
/// [`RepoError::Db`] if a link names a missing/unsealed entry, the walk reaches
/// genesis without hitting `from_row_hash` (non-contiguous range), or a cycle is
/// detected.
async fn count_chain_range(
    conn: &DbConn<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    from_row_hash: &[u8],
    to_row_hash: &[u8],
) -> Result<i64, RepoError> {
    let mut current: Option<Vec<u8>> = Some(to_row_hash.to_vec());
    let mut count: i64 = 0;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();

    while let Some(row_hash) = current {
        if !seen.insert(row_hash.clone()) {
            return Err(RepoError::Db(format!(
                "checkpoint range walk hit a cycle at row_hash {} (tenant {tenant_id})",
                hex(&row_hash)
            )));
        }

        let entry = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::TenantId.eq(tenant_id))
                    .add(journal_entry::Column::RowHash.eq(row_hash.clone())),
            )
            .one(conn)
            .await
            .map_err(|e| RepoError::Db(format!("checkpoint range read: {e}")))?
            .ok_or_else(|| {
                RepoError::Db(format!(
                    "checkpoint range not contiguous: no sealed entry for row_hash {} \
                     (tenant {tenant_id})",
                    hex(&row_hash)
                ))
            })?;

        count += 1;
        if row_hash.as_slice() == from_row_hash {
            return Ok(count);
        }
        current = entry.prev_hash;
    }

    Err(RepoError::Db(format!(
        "checkpoint range not contiguous: reached genesis without hitting from_row_hash {} \
         (tenant {tenant_id})",
        hex(from_row_hash)
    )))
}

/// Lowercase hex of a byte slice, for diagnostics only.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The §4.8/E-5 partition detach gate. Stateless over one [`DBProvider`].
#[derive(Clone)]
pub struct DetachGate {
    db: DBProvider<DbError>,
}

impl DetachGate {
    /// Build the gate over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// The underlying provider.
    #[must_use]
    pub fn db(&self) -> &DBProvider<DbError> {
        &self.db
    }

    /// Decide whether the period `(tenant_id, period_id)` MAY be detached.
    ///
    /// Both halves of §4.8/E-5 are enforced:
    ///
    /// 1. **Fully sealed** — EVERY `journal_entry` in the period under `scope`
    ///    must be sealed (non-NULL `row_hash`). An unsealed entry means the
    ///    tamper chain for the period is not closed → [`DetachBlocked`] naming the
    ///    unsealed count.
    /// 2. **Checkpoint-covered** — every sealed entry in the period must fall
    ///    within the `created_seq` range of a recorded `chain_checkpoint` for the
    ///    tenant. A period with no covering checkpoint is un-anchored: detaching
    ///    it would orphan rows whose coverage was never recorded → blocked,
    ///    naming the uncovered count. (Checkpoint *signing* / WORM is still
    ///    post-MVP — coverage here requires a checkpoint, not yet a signed one.)
    ///
    /// A blocked detach maps to
    /// [`AlarmCategory::PartitionDetachBlocked`](crate::infra::events::payloads::AlarmCategory::PartitionDetachBlocked);
    /// alarm emission is wired by the caller (rotation), not here.
    ///
    /// # Errors
    /// [`DetachBlocked`] when the period has unsealed entries OR entries not
    /// covered by any checkpoint. A storage / scope fault is FAIL-SAFE: the gate
    /// cannot prove the period detachable, so it BLOCKS (the underlying error is
    /// logged and the counts are reported as `0` — "unknown, blocked anyway").
    /// Never returns `Ok` on an unverified period.
    pub async fn may_detach(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        period_id: &str,
    ) -> Result<(), DetachBlocked> {
        let blocked = || DetachBlocked {
            tenant_id,
            period_id: period_id.to_owned(),
            unsealed_count: 0,
            uncovered_count: 0,
        };

        let conn = match self.db.conn() {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!(
                    tenant_id = %tenant_id,
                    period_id,
                    error = %e,
                    "bss-ledger: detach-gate conn failed; blocking detach (fail-safe)"
                );
                return Err(blocked());
            }
        };

        // 1. Fully-sealed half: any NULL `row_hash` in the period blocks.
        let unsealed_count = match journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::TenantId.eq(tenant_id))
                    .add(journal_entry::Column::PeriodId.eq(period_id))
                    .add(journal_entry::Column::RowHash.is_null()),
            )
            .count(&conn)
            .await
        {
            Ok(count) => count,
            Err(e) => {
                tracing::error!(
                    tenant_id = %tenant_id,
                    period_id,
                    error = %e,
                    "bss-ledger: detach-gate unsealed-count read failed; blocking detach (fail-safe)"
                );
                return Err(blocked());
            }
        };

        if unsealed_count > 0 {
            return Err(DetachBlocked {
                tenant_id,
                period_id: period_id.to_owned(),
                unsealed_count,
                uncovered_count: 0,
            });
        }

        // 2. Checkpoint-coverage half: every sealed entry's `created_seq` must
        //    fall inside some checkpoint's [from_seq, to_seq] range.
        let uncovered_count = match self
            .uncovered_entry_count(&conn, scope, tenant_id, period_id)
            .await
        {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(
                    tenant_id = %tenant_id,
                    period_id,
                    error = %e,
                    "bss-ledger: detach-gate coverage read failed; blocking detach (fail-safe)"
                );
                return Err(blocked());
            }
        };

        if uncovered_count > 0 {
            return Err(DetachBlocked {
                tenant_id,
                period_id: period_id.to_owned(),
                unsealed_count: 0,
                uncovered_count,
            });
        }
        Ok(())
    }

    /// Count the period's sealed entries NOT covered by any `chain_checkpoint`
    /// range (by `created_seq`). Resolves each checkpoint's `from`/`to` row hash
    /// to a `created_seq` to build the covered intervals, then counts period
    /// entries whose seq falls in no interval. A period with zero entries is
    /// trivially covered (returns 0). Unresolvable checkpoint endpoints (no
    /// matching sealed entry) are skipped — they cover nothing.
    async fn uncovered_entry_count(
        &self,
        conn: &DbConn<'_>,
        scope: &AccessScope,
        tenant_id: Uuid,
        period_id: &str,
    ) -> Result<u64, RepoError> {
        // The period's sealed entries (seq list).
        let entries = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::TenantId.eq(tenant_id))
                    .add(journal_entry::Column::PeriodId.eq(period_id))
                    .add(journal_entry::Column::RowHash.is_not_null()),
            )
            .all(conn)
            .await
            .map_err(|e| RepoError::Db(format!("coverage: read period entries: {e}")))?;
        if entries.is_empty() {
            return Ok(0);
        }

        // All checkpoints for the tenant → covered `created_seq` intervals.
        let checkpoints = chain_checkpoint::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(chain_checkpoint::Column::TenantId.eq(tenant_id)))
            .all(conn)
            .await
            .map_err(|e| RepoError::Db(format!("coverage: read checkpoints: {e}")))?;

        let mut intervals: Vec<(i64, i64)> = Vec::with_capacity(checkpoints.len());
        for cp in &checkpoints {
            let (Some(from_seq), Some(to_seq)) = (
                resolve_seq(conn, scope, tenant_id, &cp.from_row_hash).await?,
                resolve_seq(conn, scope, tenant_id, &cp.to_row_hash).await?,
            ) else {
                // An endpoint with no matching sealed entry covers nothing — skip.
                continue;
            };
            intervals.push((from_seq.min(to_seq), from_seq.max(to_seq)));
        }

        let uncovered = entries
            .iter()
            .filter(|e| {
                !intervals
                    .iter()
                    .any(|(lo, hi)| e.created_seq >= *lo && e.created_seq <= *hi)
            })
            .count();
        Ok(uncovered as u64)
    }
}

/// Resolve a sealed entry's `created_seq` by its `row_hash` under `scope`, or
/// `None` when no sealed entry matches (an unresolvable checkpoint endpoint).
async fn resolve_seq(
    conn: &DbConn<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    row_hash: &[u8],
) -> Result<Option<i64>, RepoError> {
    let row = journal_entry::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(journal_entry::Column::TenantId.eq(tenant_id))
                .add(journal_entry::Column::RowHash.eq(row_hash.to_vec())),
        )
        .one(conn)
        .await
        .map_err(|e| RepoError::Db(format!("coverage: resolve seq: {e}")))?;
    Ok(row.map(|r| r.created_seq))
}

/// Why a period detach was blocked (§4.8/E-5). Maps to the
/// [`AlarmCategory::PartitionDetachBlocked`](crate::infra::events::payloads::AlarmCategory::PartitionDetachBlocked)
/// alarm the caller (Foundation's rotation) raises; the gate itself only reports
/// the condition.
///
/// `unsealed_count` is the number of `journal_entry` rows in the period that
/// still have a NULL `row_hash` (the chain for that period is not closed).
/// `uncovered_count` is the number of sealed entries not covered by any
/// `chain_checkpoint` range. Both are `0` when the gate blocked
/// fail-safe on an infrastructure fault (it could not prove the period
/// detachable); the underlying error is logged, not carried here.
// The message leads with the `PARTITION_DETACH_BLOCKED` alarm token the caller
// routes this to; the `entr{y|ies}` suffix pluralizes on the combined count.
#[derive(Debug, thiserror::Error)]
#[error(
    "PARTITION_DETACH_BLOCKED: tenant {tenant_id} period {period_id} has \
     {unsealed_count} unsealed and {uncovered_count} checkpoint-uncovered entr{}",
    if *unsealed_count + *uncovered_count == 1 { "y" } else { "ies" }
)]
pub struct DetachBlocked {
    pub tenant_id: Uuid,
    pub period_id: String,
    pub unsealed_count: u64,
    pub uncovered_count: u64,
}
