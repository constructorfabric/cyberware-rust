//! `AnnotationService` — the typed controlled non-financial annotation overlay
//! (Slice 6 Phase 2 Group 2B, Variant C remodel). The ONLY mutating path over an
//! otherwise append-only ledger: it sets the CURRENT `description` note on a
//! journal entry / line, recording every change as an append-only
//! `metadata-change` secured-audit record — in ONE `SERIALIZABLE` transaction.
//! It NEVER touches `journal_entry` / `journal_line` (financial truth stays
//! byte-identical), and authoritative workflow state (dispute, reconciliation)
//! stays owned by its own resource — this overlay carries only the free-text
//! note.
//!
//! ## Pre-write screen (before any write)
//!
//! The `description` is screened for raw customer PII via [`PiiMinimizer`]: the
//! secured-audit record is append-only, so a value carrying an email / phone /
//! payment number (or a prohibited key) is rejected up front
//! ([`DomainError::PiiInMetadataValue`], HTTP 400) and never sealed into the
//! chain. The target entry / line must also exist in-tenant (a tenant-scoped,
//! race-free read over the append-only journal) so a change is never logged
//! against a dangling id.

use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{AccessScope, DbConn, DbTx, SecureEntityExt, SecureInsertExt, TxConfig};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::ports::metrics::{LedgerMetricsPort, NoopLedgerMetrics};
use crate::infra::audit::event_type::AuditEventType;
use crate::infra::audit::store::SecuredAuditStore;
use crate::infra::pii::PiiMinimizer;
use crate::infra::posting::service::infra;
use crate::infra::storage::entity::{entry_annotation, journal_entry, journal_line};
use time::OffsetDateTime;

/// `'ENTRY'` / `'LINE'` literals for the `target_kind` column (CHECK-constrained
/// in migration 000015).
const KIND_ENTRY: &str = "ENTRY";
const KIND_LINE: &str = "LINE";

/// The `attribute` label kept on the §9 metric + the audit record for
/// continuity with the metric token (`ledger_metadata_change_total{attribute}`).
const ATTRIBUTE_DESCRIPTION: &str = "description";

/// The typed annotation overlay service. Holds the append-only
/// [`SecuredAuditStore`] (stateless); every write runs in its own
/// `SERIALIZABLE` transaction.
#[derive(Clone)]
pub struct AnnotationService {
    audit: SecuredAuditStore,
    metrics: Arc<dyn LedgerMetricsPort>,
}

impl Default for AnnotationService {
    fn default() -> Self {
        Self::new()
    }
}

impl AnnotationService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            audit: SecuredAuditStore::new(),
            metrics: Arc::new(NoopLedgerMetrics),
        }
    }

    /// Bind the §9 metrics sink (`ledger_metadata_change_total{attribute}` is
    /// emitted on each committed change). Defaults to no-op until wired.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn LedgerMetricsPort>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Reject a `description` that carries raw customer PII BEFORE any write
    /// (fail fast). The secured-audit record is append-only, so PII must be
    /// screened out up front — once a record is sealed into the chain it can
    /// never be redacted. Catches PII both as object KEYS and embedded in
    /// free-text string VALUES (see [`PiiMinimizer`]).
    fn screen_description_for_pii(description: &serde_json::Value) -> Result<(), DomainError> {
        let mut categories = PiiMinimizer::prohibited_fields(description);
        for category in PiiMinimizer::prohibited_in_values(description) {
            if !categories.contains(&category) {
                categories.push(category);
            }
        }
        if categories.is_empty() {
            return Ok(());
        }
        Err(DomainError::PiiInMetadataValue(format!(
            "annotation carries prohibited PII categories {categories:?}; the audit \
             chain is append-only — remove the PII before retrying"
        )))
    }

    /// Assert the `target_id` is a real journal object in `tenant` BEFORE any
    /// write (fail fast). Journal rows are append-only (never deleted), so a
    /// pre-transaction read is race-free; this stops a typo / dangling
    /// `target_id` from logging an annotation + audit record that point at
    /// nothing. The lookup is tenant-scoped (`SecureORM`), so it also cannot
    /// probe another tenant's ids.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when no entry / line matches;
    /// [`DomainError::Internal`] on a storage / scope failure.
    async fn assert_target_exists(
        db: &DBProvider<DbError>,
        scope: &AccessScope,
        tenant: Uuid,
        target_id: Uuid,
        target_period_id: &str,
        target: AnnotationTarget,
    ) -> Result<(), DomainError> {
        let conn: DbConn<'_> = db
            .conn()
            .map_err(|e| DomainError::Internal(format!("annotation target-check conn: {e}")))?;
        let exists = match target {
            AnnotationTarget::Entry => journal_entry::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(
                    Condition::all()
                        .add(journal_entry::Column::TenantId.eq(tenant))
                        .add(journal_entry::Column::PeriodId.eq(target_period_id.to_owned()))
                        .add(journal_entry::Column::EntryId.eq(target_id)),
                )
                .one(&conn)
                .await
                .map_err(|e| DomainError::Internal(format!("annotation target read: {e}")))?
                .is_some(),
            AnnotationTarget::Line => journal_line::Entity::find()
                .secure()
                .scope_with(scope)
                .filter(
                    Condition::all()
                        .add(journal_line::Column::TenantId.eq(tenant))
                        .add(journal_line::Column::PeriodId.eq(target_period_id.to_owned()))
                        .add(journal_line::Column::LineId.eq(target_id)),
                )
                .one(&conn)
                .await
                .map_err(|e| DomainError::Internal(format!("annotation target read: {e}")))?
                .is_some(),
        };
        if exists {
            Ok(())
        } else {
            Err(DomainError::InvalidRequest(format!(
                "annotation target {} {target_id} not found in tenant period {target_period_id}",
                target.as_str()
            )))
        }
    }

    /// Set the controlled `description` annotation on a journal entry / line.
    ///
    /// Pre-write screen (no writes): PII screen on `description`; the target must
    /// exist in-tenant. Then ONE `SERIALIZABLE` transaction reads the current
    /// annotation row (for the before/after audit payload), UPSERTs the typed
    /// `entry_annotation` row (current-state), and appends a `metadata-change`
    /// secured-audit record onto the tenant's audit chain — all in the SAME
    /// transaction. NO journal table is touched.
    ///
    /// # Errors
    /// [`DomainError::PiiInMetadataValue`] (description carries raw customer PII)
    /// / [`DomainError::InvalidRequest`] (`target_id` does not exist) — both
    /// pre-write, no writes; [`DomainError::Internal`] on a storage / scope /
    /// audit failure (rolls the transaction back).
    #[allow(
        clippy::too_many_arguments,
        reason = "one annotation's full identity (target/period/kind) + description + actor/reason over the caller's ctx/scope"
    )]
    pub async fn set(
        &self,
        db: &DBProvider<DbError>,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        target_id: Uuid,
        target_period_id: String,
        target: AnnotationTarget,
        description: Option<String>,
        actor_ref: String,
        reason: Option<String>,
        correlation_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        // --- PRE-WRITE screen (fail fast, no writes) ---
        let description_json = description.as_ref().map_or(serde_json::Value::Null, |d| {
            serde_json::Value::String(d.clone())
        });
        Self::screen_description_for_pii(&description_json)?;
        Self::assert_target_exists(db, scope, tenant, target_id, &target_period_id, target).await?;

        // `ctx` authorized the call at the REST seam (the PEP gate ran there);
        // the in-txn writes are tenant-scoped by `scope`, and the audit append
        // generates its own ids, so the txn body does not re-thread `ctx`.
        let _ = ctx;
        let audit = self.audit.clone();
        let scope = scope.clone();
        let kind = target.as_str().to_owned();

        // --- TRANSACTION (SERIALIZABLE + retry) ---
        let result: Result<(), DbError> = db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let audit = audit.clone();
                let scope = scope.clone();
                let kind = kind.clone();
                let description = description.clone();
                let description_json = description_json.clone();
                let actor_ref = actor_ref.clone();
                let reason = reason.clone();
                let target_period_id = target_period_id.clone();
                Box::pin(async move {
                    set_in_txn(
                        txn,
                        &audit,
                        &scope,
                        tenant,
                        target_id,
                        &target_period_id,
                        &kind,
                        description,
                        &description_json,
                        &actor_ref,
                        reason.as_deref(),
                        correlation_id,
                    )
                    .await
                })
            })
            .await;

        let mapped = result.map_err(|e| DomainError::Internal(format!("annotation set txn: {e}")));
        if mapped.is_ok() {
            self.metrics.metadata_change(ATTRIBUTE_DESCRIPTION);
        }
        mapped
    }
}

/// In-transaction body: read the current annotation, UPSERT the typed row,
/// append the secured-audit record. All under the caller's `SERIALIZABLE`
/// transaction.
#[allow(
    clippy::too_many_arguments,
    reason = "the full annotation tuple threaded into one txn body"
)]
async fn set_in_txn(
    txn: &DbTx<'_>,
    audit: &SecuredAuditStore,
    scope: &AccessScope,
    tenant: Uuid,
    target_id: Uuid,
    target_period_id: &str,
    target_kind: &str,
    description: Option<String>,
    description_json: &serde_json::Value,
    actor_ref: &str,
    reason: Option<&str>,
    correlation_id: Option<Uuid>,
) -> Result<(), DbError> {
    // 1. Read the current annotation (for the before/after audit payload).
    let current = entry_annotation::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(entry_annotation::Column::TenantId.eq(tenant))
                .add(entry_annotation::Column::TargetId.eq(target_id))
                .add(entry_annotation::Column::TargetKind.eq(target_kind.to_owned())),
        )
        .one(txn)
        .await
        .map_err(|e| infra(format!("annotation read current: {e}")))?;

    let before = current
        .and_then(|row| row.description)
        .map_or(serde_json::Value::Null, serde_json::Value::String);
    let before_after = serde_json::json!({ "before": before, "after": description_json });

    // 2. UPSERT the typed current-state row.
    let am = entry_annotation::ActiveModel {
        tenant_id: Set(tenant),
        target_id: Set(target_id),
        target_kind: Set(target_kind.to_owned()),
        target_period_id: Set(target_period_id.to_owned()),
        description: Set(description),
        actor_ref: Set(actor_ref.to_owned()),
        updated_at: Set(OffsetDateTime::now_utc()),
    };
    let on_conflict = OnConflict::columns([
        entry_annotation::Column::TenantId,
        entry_annotation::Column::TargetId,
        entry_annotation::Column::TargetKind,
    ])
    .update_columns([
        entry_annotation::Column::Description,
        entry_annotation::Column::ActorRef,
        entry_annotation::Column::UpdatedAt,
        entry_annotation::Column::TargetPeriodId,
    ])
    .to_owned();

    entry_annotation::Entity::insert(am.clone())
        .secure()
        .scope_with_model(scope, &am)
        .map_err(|e| infra(format!("annotation upsert scope: {e}")))?
        .on_conflict_raw(on_conflict)
        .exec(txn)
        .await
        .map_err(|e| infra(format!("annotation upsert: {e}")))?;

    // 3. Append the secured-audit record in the SAME transaction.
    audit
        .append(
            txn,
            scope,
            tenant,
            AuditEventType::MetadataChange,
            Some(actor_ref),
            reason,
            &before_after,
            correlation_id,
            None,
        )
        .await?;

    Ok(())
}

/// Write port the `PATCH …/annotation` handler records changes through.
/// Abstracts the single annotation write (screen → upsert + audit in one txn)
/// so the journal-entry router tests can stub the path without a database. The
/// production implementation is [`LedgerAnnotationWriter`].
#[async_trait::async_trait]
pub trait AnnotationWriter: Send + Sync {
    /// Set one controlled annotation (see [`AnnotationService::set`]).
    ///
    /// # Errors
    /// [`DomainError::PiiInMetadataValue`] on the pre-write PII screen;
    /// [`DomainError::InvalidRequest`] on a dangling target;
    /// [`DomainError::Internal`] on a storage fault.
    #[allow(
        clippy::too_many_arguments,
        reason = "one annotation's full identity + description + actor/reason over the caller's ctx/scope"
    )]
    async fn set(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        target_id: Uuid,
        target_period_id: String,
        target: AnnotationTarget,
        description: Option<String>,
        actor_ref: String,
        reason: Option<String>,
        correlation_id: Option<Uuid>,
    ) -> Result<(), DomainError>;
}

/// The production [`AnnotationWriter`]: a stateless [`AnnotationService`] bound
/// to one [`DBProvider`].
#[derive(Clone)]
pub struct LedgerAnnotationWriter {
    service: AnnotationService,
    db: DBProvider<DbError>,
}

impl LedgerAnnotationWriter {
    /// Build the writer over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self {
            service: AnnotationService::new(),
            db,
        }
    }

    /// Bind the §9 metrics sink onto the wrapped service.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn LedgerMetricsPort>) -> Self {
        self.service = self.service.with_metrics(metrics);
        self
    }
}

#[async_trait::async_trait]
impl AnnotationWriter for LedgerAnnotationWriter {
    #[allow(
        clippy::too_many_arguments,
        reason = "delegates the full annotation tuple to the inherent service method"
    )]
    async fn set(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        target_id: Uuid,
        target_period_id: String,
        target: AnnotationTarget,
        description: Option<String>,
        actor_ref: String,
        reason: Option<String>,
        correlation_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        self.service
            .set(
                &self.db,
                ctx,
                scope,
                tenant,
                target_id,
                target_period_id,
                target,
                description,
                actor_ref,
                reason,
                correlation_id,
            )
            .await
    }
}

/// The kind of journal object an annotation targets (`ENTRY` / `LINE`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnnotationTarget {
    /// The annotation targets a journal entry header (`target_id` = `entry_id`).
    Entry,
    /// The annotation targets a journal line (`target_id` = `line_id`).
    Line,
}

impl AnnotationTarget {
    /// Stable wire token (matches the `target_kind` CHECK in migration 000015).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entry => KIND_ENTRY,
            Self::Line => KIND_LINE,
        }
    }

    /// Parse a `target_kind` literal (`ENTRY` / `LINE`); any other literal is an
    /// invalid request.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `s` is neither `ENTRY` nor `LINE`.
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        match s {
            KIND_ENTRY => Ok(Self::Entry),
            KIND_LINE => Ok(Self::Line),
            other => Err(DomainError::InvalidRequest(format!(
                "invalid target_kind {other:?} (expected ENTRY or LINE)"
            ))),
        }
    }
}

/// Retry-extractor for the `SERIALIZABLE` annotation write: a wrapped `DbErr` so
/// a serialization failure is recognised as retryable contention (mirrors
/// `infra::posting::service::as_db_err`).
fn as_db_err(e: &DbError) -> Option<&sea_orm::DbErr> {
    match e {
        DbError::Sea(db_err) => Some(db_err),
        _ => None,
    }
}

#[cfg(test)]
#[path = "annotation_tests.rs"]
mod annotation_tests;
