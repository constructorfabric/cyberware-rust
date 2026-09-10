//! `AuditRetrievalReader` — scoped reads backing the audit-retrieval REST
//! surface (Slice 6 Phase 2 Group 2C, architecture AC #8): the who/when/source/
//! correlation dims of a posted entry, a document's full posting history, and a
//! scope's tamper-status (active freezes + a derived `verified` flag).
//!
//! Stateless over one [`DBProvider`]; every read goes through the `SecureORM`
//! layer (`.secure().scope_with(scope)`), so the scope the caller passes IS the
//! SQL-level BOLA filter — a row outside it is simply not returned. The
//! tamper-status read may run under a TARGET-tenant scope handed back by the
//! [`crate::infra::authz::cross_tenant::CrossTenantGateway`]; the per-row reads
//! run under the caller's HOME scope.
//!
//! **§10 NFR target (ratified):** audit retrieval p95 ≤ 2 s. The bounded,
//! index-backed scoped reads here meet it; the §9 latency is observed via the
//! shared posting/inquiry latency instruments (`infra::metrics`).

use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::model::RepoError;
use crate::infra::storage::entity::{journal_entry, scope_freeze};
use time::OffsetDateTime;

/// The audit who/when/source/correlation dims of one posted journal entry
/// (AC #8). A pure read projection of `journal_entry`; carries no lines.
#[derive(Clone, Debug)]
pub struct AuditEntryRecord {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub period_id: String,
    pub posted_by_actor_id: Uuid,
    pub origin: String,
    pub posted_at_utc: OffsetDateTime,
    pub source_doc_type: String,
    pub source_business_id: String,
    pub correlation_id: Uuid,
    pub reverses_entry_id: Option<Uuid>,
    pub created_seq: i64,
}

impl From<journal_entry::Model> for AuditEntryRecord {
    fn from(m: journal_entry::Model) -> Self {
        Self {
            entry_id: m.entry_id,
            tenant_id: m.tenant_id,
            period_id: m.period_id,
            posted_by_actor_id: m.posted_by_actor_id,
            origin: m.origin,
            posted_at_utc: m.posted_at_utc,
            source_doc_type: m.source_doc_type,
            source_business_id: m.source_business_id,
            correlation_id: m.correlation_id,
            reverses_entry_id: m.reverses_entry_id,
            created_seq: m.created_seq,
        }
    }
}

/// One active-or-historical scope-freeze row in a tamper-status read.
#[derive(Clone, Debug)]
pub struct FreezeRecord {
    pub scope: String,
    pub period_id: String,
    pub reason: String,
    pub frozen_at: OffsetDateTime,
    pub set_by: String,
    pub cleared_by: Option<String>,
    pub cleared_at: Option<OffsetDateTime>,
}

impl From<scope_freeze::Model> for FreezeRecord {
    fn from(m: scope_freeze::Model) -> Self {
        Self {
            scope: m.scope,
            period_id: m.period_id,
            reason: m.reason,
            frozen_at: m.frozen_at,
            set_by: m.set_by,
            cleared_by: m.cleared_by,
            cleared_at: m.cleared_at,
        }
    }
}

/// The tamper-status of a resolved scope: every freeze row for the tenant, a
/// `scope_frozen` flag (any ACTIVE freeze), and a derived `verified` flag.
#[derive(Clone, Debug)]
pub struct TamperStatusRecord {
    pub scope_frozen: bool,
    pub freezes: Vec<FreezeRecord>,
    pub verified: bool,
}

/// Scoped reader over one [`DBProvider`]. Stateless.
#[derive(Clone)]
pub struct AuditRetrievalReader {
    db: DBProvider<DbError>,
}

impl AuditRetrievalReader {
    /// Build the reader over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// The underlying provider (the audit REST surface opens its own elevation
    /// transaction on it for the tamper-status path).
    #[must_use]
    pub fn db(&self) -> &DBProvider<DbError> {
        &self.db
    }

    /// Read the audit dims of one entry by `(tenant_id, entry_id)` under `scope`.
    /// SQL-level BOLA: a foreign-tenant scope (or an absent entry) yields `None`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn audit_entry(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        entry_id: Uuid,
    ) -> Result<Option<AuditEntryRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let header = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::EntryId.eq(entry_id))
                    .add(journal_entry::Column::TenantId.eq(tenant_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("find audit journal_entry: {e}")))?;
        Ok(header.map(AuditEntryRecord::from))
    }

    /// Read a document's full posting history under `scope`: every
    /// `journal_entry` row for `tenant_id` with that `(source_doc_type,
    /// source_business_id)`, PLUS any entries that `reverses_entry_id`-link to
    /// them (reversals / mapping-corrections of the same document), ordered by
    /// `created_seq`. SQL-level BOLA via the scoped select.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn document_history(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        source_doc_type: &str,
        source_business_id: &str,
    ) -> Result<Vec<AuditEntryRecord>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;

        // 1. The document's own entries (by source key).
        let base = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_entry::Column::TenantId.eq(tenant_id))
                    .add(journal_entry::Column::SourceDocType.eq(source_doc_type.to_owned()))
                    .add(journal_entry::Column::SourceBusinessId.eq(source_business_id.to_owned())),
            )
            .order_by(journal_entry::Column::CreatedSeq, Order::Asc)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("find document history (base): {e}")))?;

        // 2. Any entries that reverse one of the base entries (reversals /
        //    mapping-corrections link back via `reverses_entry_id`). Tenant-
        //    scoped like the base read.
        let base_ids: Vec<Uuid> = base.iter().map(|e| e.entry_id).collect();
        let linked = if base_ids.is_empty() {
            Vec::new()
        } else {
            journal_entry::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(
                    Condition::all()
                        .add(journal_entry::Column::TenantId.eq(tenant_id))
                        .add(journal_entry::Column::ReversesEntryId.is_in(base_ids.clone())),
                )
                .order_by(journal_entry::Column::CreatedSeq, Order::Asc)
                .all(&conn)
                .await
                .map_err(|e| RepoError::Db(format!("find document history (linked): {e}")))?
        };

        // Merge + de-dup (a linked entry sharing the source key already appears
        // in `base`), then order by `created_seq`.
        let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        let mut out: Vec<AuditEntryRecord> = Vec::with_capacity(base.len() + linked.len());
        for m in base.into_iter().chain(linked) {
            if seen.insert(m.entry_id) {
                out.push(AuditEntryRecord::from(m));
            }
        }
        out.sort_by_key(|e| e.created_seq);
        Ok(out)
    }

    /// Read the tamper-status of `tenant` under `scope` inside `txn` (the audit
    /// surface runs this in the elevation transaction so the forensic record and
    /// the read share one transaction). Returns every freeze row for the tenant,
    /// a `scope_frozen` flag (any ACTIVE freeze — `cleared_at IS NULL`), and a
    /// derived `verified = !scope_frozen`.
    ///
    /// `verified` is an MVP derivation: the gear persists no per-run verifier
    /// result, so "not currently frozen" is the only chain-health signal
    /// available at read time. The daily chain verifier is what SETS a freeze on
    /// a broken chain, so an unfrozen scope is one the verifier has not faulted.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage / scope failure.
    pub async fn tamper_status_in_txn(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<TamperStatusRecord, RepoError> {
        let rows = scope_freeze::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(scope_freeze::Column::TenantId.eq(tenant)))
            .order_by(scope_freeze::Column::FrozenAt, Order::Asc)
            .all(txn)
            .await
            .map_err(|e| RepoError::Db(format!("read tamper-status scope_freeze: {e}")))?;

        let scope_frozen = rows.iter().any(|r| r.cleared_at.is_none());
        let freezes = rows.into_iter().map(FreezeRecord::from).collect();
        Ok(TamperStatusRecord {
            scope_frozen,
            freezes,
            // MVP: no per-run verifier result is persisted, so a scope that is
            // not currently frozen is treated as verified.
            verified: !scope_frozen,
        })
    }
}
