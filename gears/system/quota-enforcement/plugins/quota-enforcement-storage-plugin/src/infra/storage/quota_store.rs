//! [`SqlQuotaStore`]: the `toolkit-db` adapter behind [`QuotaStore`].
//!
//! Every mutation is one transaction: the Quota row, its counter row, one
//! operation-log row, and the notification events (I1, I11). Update and
//! deactivate lock the Quota row first, then its counter rows (ADR-0002 lock
//! order), decide on the merged row inside the lock (I6, I14), and write by
//! compare-and-set on the record version, so a row that moved under the lock
//! is reported, never overwritten. Reads run on a plain connection: the
//! keyset page re-applies the caller's scope on every request, and the two
//! platform-plane aggregates run unscoped, as the contract states.
//!
//! Seams for later features are marked `2.5` (consumption counters in the cap
//! guard) and `2.6` (the lease cascade in deactivation).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use quota_enforcement_sdk::{
    ActiveQuotaCounts, DeactivateOutcome, MetricId, NotificationEvent, PageRequest, PageResult,
    ProjectionBinding, Quota, QuotaDraft, QuotaFilter, QuotaId, QuotaPatch, QuotaType, TenantId,
};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, ScopeError, validate_tenant_in_scope};
use toolkit_db::{Db, DbError};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::ports::{Actor, QuotaStore, StoreError};
use crate::infra::outbox::{EnqueueError, NotificationEnqueuer};
use crate::infra::storage::cursor::{self, MAX_FILTER_IDS};
use crate::infra::storage::entity::quota;
use crate::infra::storage::quota_mapping::{
    self, MappingError, QuotaUpdate, STATUS_ACTIVE, status_name,
};
use crate::infra::storage::repo::operation_log_repo::{
    self, Entry, OP_QUOTA_CREATE, OP_QUOTA_DEACTIVATE, OP_QUOTA_UPDATE,
};
use crate::infra::storage::repo::{allocation_counter_repo, quota_repo};

const LOG_TARGET: &str = "qe.storage";

/// The Quota tables on the plugin's database.
#[derive(Clone)]
pub struct SqlQuotaStore {
    db: Db,
    enqueuer: Arc<dyn NotificationEnqueuer>,
}

/// Every way a transaction body can fail, before the lift to [`StoreError`].
#[derive(Debug, thiserror::Error)]
enum TxError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Scope(#[from] ScopeError),
    #[error(transparent)]
    Map(#[from] MappingError),
    #[error(transparent)]
    Enqueue(#[from] EnqueueError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Lift a transaction failure onto the port error. Database failures are
/// logged here and reported as `Unavailable`; scope refusals are the
/// post-PDP defence (`SubjectOutOfScope`); a caller value that does not fit
/// its column is `ValueOutOfRange`; everything the schema should have made
/// impossible is `Corrupt`.
fn lift(operation: &'static str, err: TxError) -> StoreError {
    match err {
        TxError::Store(err) => err,
        TxError::Db(err) => unavailable(operation, &err),
        TxError::Scope(ScopeError::Db(err)) => unavailable(operation, &err),
        TxError::Scope(
            ScopeError::TenantNotInScope { .. } | ScopeError::Denied(_) | ScopeError::Invalid(_),
        ) => StoreError::SubjectOutOfScope,
        TxError::Map(MappingError::CapOutOfRange { cap }) => StoreError::ValueOutOfRange {
            field: "cap",
            value: cap.to_string(),
        },
        TxError::Map(MappingError::VersionOutOfRange { field, value }) => {
            StoreError::ValueOutOfRange {
                field,
                value: value.to_string(),
            }
        }
        TxError::Map(err @ MappingError::MetadataWithoutContract) => StoreError::InvalidPatch {
            detail: err.to_string(),
        },
        TxError::Map(err @ MappingError::Column { .. }) => corrupt(operation, &err),
        TxError::Enqueue(EnqueueError::NotBound) => {
            tracing::warn!(
                target: LOG_TARGET,
                operation,
                "notification outbox is not bound; the mutation was rolled back"
            );
            StoreError::Unavailable { operation }
        }
        TxError::Enqueue(EnqueueError::Outbox(toolkit_db::outbox::OutboxError::Database(err))) => {
            unavailable(operation, &err)
        }
        TxError::Enqueue(err) => corrupt(operation, &err),
    }
}

fn unavailable(operation: &'static str, err: &dyn std::fmt::Display) -> StoreError {
    tracing::warn!(target: LOG_TARGET, operation, error = %err, "storage backend call failed");
    StoreError::Unavailable { operation }
}

fn corrupt(operation: &'static str, err: &dyn std::fmt::Display) -> StoreError {
    tracing::error!(target: LOG_TARGET, operation, error = %err, "storage state is inconsistent");
    StoreError::Corrupt {
        operation,
        detail: err.to_string(),
    }
}

/// The events of a creation with the assigned id filled in.
fn with_quota_id(events: &[NotificationEvent], id: QuotaId) -> Vec<NotificationEvent> {
    events
        .iter()
        .cloned()
        .map(|mut event| {
            if event.quota_id.is_none() {
                event.quota_id = Some(id);
            }
            event
        })
        .collect()
}

impl SqlQuotaStore {
    /// Bind the store to the plugin's database and its notification outbox.
    #[must_use]
    pub fn new(db: Db, enqueuer: Arc<dyn NotificationEnqueuer>) -> Self {
        Self { db, enqueuer }
    }

    fn conn(&self, operation: &'static str) -> Result<toolkit_db::DbConn<'_>, StoreError> {
        self.db.conn().map_err(|e| unavailable(operation, &e))
    }

    /// The locked active row, or the port error that stops the mutation.
    async fn lock_active_row(
        runner: &impl DBRunner,
        scope: &AccessScope,
        quota_id: QuotaId,
    ) -> Result<quota::Model, TxError> {
        let row = quota_repo::find_by_id(runner, scope, quota_id.as_uuid(), true)
            .await?
            .ok_or(StoreError::QuotaNotFound { id: quota_id })?;
        if row.status != STATUS_ACTIVE {
            return Err(StoreError::QuotaDeactivated { id: quota_id }.into());
        }
        Ok(row)
    }

    /// What the merged row would over-commit: the counters under the row lock.
    /// Allocation Quotas read their in-flight counter; consumption Quotas read
    /// nothing until consumption-operations (2.5) adds their period counters.
    async fn consumed_under_lock(
        runner: &impl DBRunner,
        scope: &AccessScope,
        row: &quota::Model,
    ) -> Result<u64, TxError> {
        if row.quota_type != QuotaType::Allocation.as_gts_id() {
            return Ok(0);
        }
        let in_flight = allocation_counter_repo::read_in_flight_for_update(runner, scope, row.id)
            .await?
            .unwrap_or(0);
        u64::try_from(in_flight).map_err(|_| {
            MappingError::Column {
                column: "in_flight",
                detail: format!("negative in-flight amount {in_flight}"),
            }
            .into()
        })
    }

    /// Invariants I6 and I14 on the merged row.
    fn check_merged(
        row: &quota::Model,
        update: &QuotaUpdate,
        consumed: u64,
    ) -> Result<(), TxError> {
        let merged_cap = update.merged_cap(row);
        if let Some(cap) = merged_cap {
            let new_cap = u64::try_from(cap).unwrap_or(0);
            if new_cap < consumed {
                return Err(StoreError::CapBelowConsumed { new_cap, consumed }.into());
            }
        }
        let thresholds = quota_mapping::thresholds_of(update.merged_thresholds(row))?;
        if merged_cap.is_none() && !thresholds.is_empty() {
            return Err(StoreError::ThresholdsRequireBoundedCap.into());
        }
        Ok(())
    }
}

#[async_trait]
impl QuotaStore for SqlQuotaStore {
    async fn create_quota(
        &self,
        actor: &Actor,
        scope: &AccessScope,
        draft: QuotaDraft,
        events: &[NotificationEvent],
    ) -> Result<QuotaId, StoreError> {
        const OPERATION: &str = "create quota";
        validate_tenant_in_scope(draft.tenant_id.as_uuid(), scope)
            .map_err(|_| StoreError::SubjectOutOfScope)?;
        let id = QuotaId::generate();
        let now = OffsetDateTime::now_utc();
        let row =
            quota_mapping::draft_to_row(id, &draft, now).map_err(|e| lift(OPERATION, e.into()))?;
        let events = with_quota_id(events, id);
        let tenant_id = draft.tenant_id.as_uuid();
        let with_counter = draft.quota_type == QuotaType::Allocation;
        let scope = scope.clone();
        let actor = actor.clone();
        let enqueuer = Arc::clone(&self.enqueuer);
        self.db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    quota_repo::insert(tx, &scope, row).await?;
                    if with_counter {
                        allocation_counter_repo::insert_initial(
                            tx,
                            &scope,
                            id.as_uuid(),
                            tenant_id,
                            now,
                        )
                        .await?;
                    }
                    operation_log_repo::append(
                        tx,
                        &scope,
                        Entry {
                            tenant_id,
                            quota_id: id.as_uuid(),
                            operation: OP_QUOTA_CREATE,
                            actor: &actor,
                            record_version: 1,
                            detail: String::new(),
                            occurred_at: now,
                        },
                    )
                    .await?;
                    enqueuer.enqueue_all(tx, &events).await?;
                    Ok::<QuotaId, TxError>(id)
                })
            })
            .await
            .map_err(|e| lift(OPERATION, e))
    }

    async fn update_quota(
        &self,
        actor: &Actor,
        scope: &AccessScope,
        quota_id: QuotaId,
        patch: QuotaPatch,
        events: &[NotificationEvent],
    ) -> Result<Quota, StoreError> {
        const OPERATION: &str = "update quota";
        let update =
            quota_mapping::patch_to_update(&patch).map_err(|e| lift(OPERATION, e.into()))?;
        let now = OffsetDateTime::now_utc();
        let scope = scope.clone();
        let actor = actor.clone();
        let events = events.to_vec();
        let enqueuer = Arc::clone(&self.enqueuer);
        self.db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let row = Self::lock_active_row(tx, &scope, quota_id).await?;
                    let consumed = Self::consumed_under_lock(tx, &scope, &row).await?;
                    Self::check_merged(&row, &update, consumed)?;
                    let applied = quota_repo::apply_update(
                        tx,
                        &scope,
                        row.id,
                        row.record_version,
                        &update,
                        now,
                    )
                    .await?;
                    if !applied {
                        return Err(StoreError::Corrupt {
                            operation: OPERATION,
                            detail: "the locked row moved before the update".to_owned(),
                        }
                        .into());
                    }
                    let committed = quota_repo::find_by_id(tx, &scope, row.id, false)
                        .await?
                        .ok_or_else(|| StoreError::Corrupt {
                            operation: OPERATION,
                            detail: "the updated row does not read back".to_owned(),
                        })?;
                    operation_log_repo::append(
                        tx,
                        &scope,
                        Entry {
                            tenant_id: row.tenant_id,
                            quota_id: row.id,
                            operation: OP_QUOTA_UPDATE,
                            actor: &actor,
                            record_version: committed.record_version,
                            detail: patched_fields(&update),
                            occurred_at: now,
                        },
                    )
                    .await?;
                    enqueuer.enqueue_all(tx, &events).await?;
                    Ok::<Quota, TxError>(quota_mapping::row_to_quota(committed)?)
                })
            })
            .await
            .map_err(|e| lift(OPERATION, e))
    }

    async fn deactivate_quota(
        &self,
        actor: &Actor,
        scope: &AccessScope,
        quota_id: QuotaId,
        events: &[NotificationEvent],
    ) -> Result<DeactivateOutcome, StoreError> {
        const OPERATION: &str = "deactivate quota";
        let now = OffsetDateTime::now_utc();
        let scope = scope.clone();
        let actor = actor.clone();
        let events = events.to_vec();
        let enqueuer = Arc::clone(&self.enqueuer);
        self.db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let row = Self::lock_active_row(tx, &scope, quota_id).await?;
                    let flipped =
                        quota_repo::mark_deactivated(tx, &scope, row.id, row.record_version, now)
                            .await?;
                    if !flipped {
                        return Err(StoreError::Corrupt {
                            operation: OPERATION,
                            detail: "the locked row moved before the deactivation".to_owned(),
                        }
                        .into());
                    }
                    // 2.6: lock the Quota's active leases here, resolve them,
                    // return their held capacity, and append one
                    // `lease-resolved-by-deactivation` event per lease.
                    let outcome = DeactivateOutcome::default();
                    operation_log_repo::append(
                        tx,
                        &scope,
                        Entry {
                            tenant_id: row.tenant_id,
                            quota_id: row.id,
                            operation: OP_QUOTA_DEACTIVATE,
                            actor: &actor,
                            record_version: row.record_version + 1,
                            detail: format!("resolved_leases={}", outcome.resolved_leases.len()),
                            occurred_at: now,
                        },
                    )
                    .await?;
                    enqueuer.enqueue_all(tx, &events).await?;
                    Ok::<DeactivateOutcome, TxError>(outcome)
                })
            })
            .await
            .map_err(|e| lift(OPERATION, e))
    }

    async fn read_quotas(
        &self,
        scope: &AccessScope,
        filter: QuotaFilter,
        page: PageRequest,
    ) -> Result<PageResult<Quota>, StoreError> {
        const OPERATION: &str = "read quotas";
        if filter.ids.len() > MAX_FILTER_IDS {
            return Err(StoreError::InvalidFilter {
                detail: format!(
                    "{} ids named, at most {MAX_FILTER_IDS} are accepted",
                    filter.ids.len()
                ),
            });
        }
        let limit = cursor::effective_limit(page.limit);
        let after = page
            .cursor
            .as_deref()
            .map(cursor::decode)
            .transpose()?
            .map(QuotaId::as_uuid);
        let ids: Vec<Uuid> = filter.ids.iter().map(|id| id.as_uuid()).collect();
        let status = filter.status.map(status_name);
        let query = quota_repo::ListQuery {
            tenant_id: filter.tenant_id.map(TenantId::as_uuid),
            subject: filter
                .subject
                .as_ref()
                .map(|s| (s.projection_type.as_ref(), s.subject_id.as_str())),
            metric: filter.metric.as_ref().map(MetricId::as_str),
            status,
            ids: &ids,
            after,
            limit: u64::from(limit) + 1,
        };
        let conn = self.conn(OPERATION)?;
        let mut rows = quota_repo::list_page(&conn, scope, &query)
            .await
            .map_err(|e| lift(OPERATION, e.into()))?;
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next_cursor = if has_more {
            rows.last().map(|row| cursor::encode(QuotaId::new(row.id)))
        } else {
            None
        };
        let items = rows
            .into_iter()
            .map(quota_mapping::row_to_quota)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| lift(OPERATION, e.into()))?;
        Ok(PageResult { items, next_cursor })
    }

    async fn read_active_projection_bindings(
        &self,
    ) -> Result<HashSet<ProjectionBinding>, StoreError> {
        const OPERATION: &str = "read active projection bindings";
        let conn = self.conn(OPERATION)?;
        let rows = quota_repo::active_bindings(&conn)
            .await
            .map_err(|e| lift(OPERATION, e.into()))?;
        rows.into_iter()
            .map(|row| {
                Ok(ProjectionBinding {
                    metric: MetricId::parse(&row.metric).map_err(|e| MappingError::Column {
                        column: "metric",
                        detail: e.to_string(),
                    })?,
                    projection_type: gts::GtsTypeId::try_new(&row.projection_type).map_err(
                        |e| MappingError::Column {
                            column: "projection_type",
                            detail: e.to_string(),
                        },
                    )?,
                })
            })
            .collect::<Result<HashSet<_>, MappingError>>()
            .map_err(|e| lift(OPERATION, e.into()))
    }

    async fn read_active_quota_counts(&self) -> Result<ActiveQuotaCounts, StoreError> {
        const OPERATION: &str = "read active quota counts";
        let conn = self.conn(OPERATION)?;
        let cap_zero = quota_repo::count_active_cap_zero(&conn)
            .await
            .map_err(|e| lift(OPERATION, e.into()))?;
        let cap_unbounded = quota_repo::count_active_cap_unbounded(&conn)
            .await
            .map_err(|e| lift(OPERATION, e.into()))?;
        let rows = quota_repo::active_counts_by_metric(&conn)
            .await
            .map_err(|e| lift(OPERATION, e.into()))?;
        let by_metric = rows
            .into_iter()
            .map(|row| {
                let metric = MetricId::parse(&row.metric).map_err(|e| MappingError::Column {
                    column: "metric",
                    detail: e.to_string(),
                })?;
                Ok((metric, u64::try_from(row.total).unwrap_or(0)))
            })
            .collect::<Result<HashMap<_, _>, MappingError>>()
            .map_err(|e| lift(OPERATION, e.into()))?;
        Ok(ActiveQuotaCounts {
            cap_zero,
            cap_unbounded,
            by_metric,
        })
    }
}

/// The names of the columns a patch sets, for the operation log. Names only,
/// never values: metadata is opaque to QE.
fn patched_fields(update: &QuotaUpdate) -> String {
    let mut fields = Vec::new();
    if update.cap.is_some() {
        fields.push("cap");
    }
    if update.notification_thresholds.is_some() {
        fields.push("notification_thresholds");
    }
    if update.validity.is_some() {
        fields.push("validity_window");
    }
    if update.metadata.is_some() {
        fields.push("metadata,constraint_contract");
    }
    if update.enforcement_mode.is_some() {
        fields.push("enforcement_mode");
    }
    if update.fail_open_hint.is_some() {
        fields.push("fail_open_hint");
    }
    fields.join(",")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "quota_store_tests.rs"]
mod quota_store_tests;
