//! The database-backed registry service: the domain surface every transport
//! adapter calls.
//!
//! SPEC §8.4 requires that the REST layer be a mapping step only, and that this
//! surface *already* be sufficient for a future `api/grpc` adapter without adding
//! domain methods. That is why the three methods here take and return domain
//! values — no `StatusCode`, no `HeaderMap`, no `Json` — and why the
//! `Idempotency-Key` arrives as an ordinary field rather than being read from a
//! header somewhere inside.
//!
//! # Two interim shapes, both marked
//!
//! **Admission runs inline.** T21 starts the outbox worker in `init()` and makes
//! the dispatcher enqueue; until then `submit` accepts and then admits in the same
//! call, which is what makes a registration observable end to end at Checkpoint 1.
//! Seeding does this permanently (SPEC §8.1: *"types-registry accepts and admits it
//! itself, inline, with no outbox"*), so the code path is not a throwaway — only
//! its use for API traffic is.
//!
//! **The scope is `allow_all`.** ponytail: ceiling C6 — there is no PDP, the
//! managed entities are `#[secure(unrestricted)]`, and no `SecurityContext` reaches
//! this layer yet. `allow_all` is a legitimate authorization outcome with no
//! row-level filtering, not a bypass; `AccessScope::default()` is deny-all. The P1
//! upgrade is a request-scoped `AccessScope` derived from the `SecurityContext`,
//! which is why every repository method already takes one.

use std::sync::Arc;

use serde_json::Value;
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, ScopeError};
use toolkit_db::{DBProvider, Db, DbError};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::TypesRegistryConfig;
use crate::domain::admission::acceptance::{AcceptanceContext, AcceptanceError, accept};
use crate::domain::admission::worker::{Tuning, WorkerError, run_operation};
use crate::domain::admission::{Accepted, OperationDispatch, SubmitRequest};
use crate::domain::enums::{
    EntityKind, LifecycleStatus, OperationItemStatus, OperationKind, OperationStatus,
};
use crate::domain::policy::RegistrationPolicy;
use crate::domain::ports::metrics::AdmissionMetrics;
use crate::domain::ports::{
    CurrentDocument, CurrentInstanceValue, CurrentTypeSchemaRow, Stores, snapshot_read,
};

/// Either key an entity can be read by.
///
/// Both name the same row: the Registry Reference is `GtsId::to_uuid()`, a
/// deterministic derivation of the identifier. Parsing which one the caller meant
/// is a domain decision, not a transport one, so it lives in [`EntityKey::parse`].
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityKey {
    GtsId(String),
    Uuid(Uuid),
}

impl EntityKey {
    /// Classify a path segment. A value that parses as a UUID is a Registry
    /// Reference; anything else is treated as an identifier and validated by the
    /// read.
    ///
    /// The order matters and is not arbitrary: a GTS identifier can never be a
    /// bare UUID, because every segment carries dots and a version.
    #[must_use]
    pub fn parse(key: &str) -> Self {
        match Uuid::parse_str(key) {
            Ok(uuid) => Self::Uuid(uuid),
            Err(_) => Self::GtsId(key.to_owned()),
        }
    }
}

/// One operation and its per-candidate outcomes, as a caller polls it.
#[domain_model]
#[derive(Clone, Debug)]
pub struct OperationRecord {
    pub operation_id: Uuid,
    pub kind: OperationKind,
    pub dry_run: bool,
    pub status: OperationStatus,
    pub created_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
    pub items: Vec<OperationItemRecord>,
}

/// One candidate's durable outcome.
#[domain_model]
#[derive(Clone, Debug)]
pub struct OperationItemRecord {
    pub gts_id: String,
    pub status: OperationItemStatus,
    pub resource_version: Option<i64>,
    /// The stored structured reason, verbatim.
    pub error: Option<String>,
}

/// One entity with its content and D3's materialized artifacts.
#[domain_model]
#[derive(Clone, Debug)]
pub struct EntityRecord {
    pub gts_id: String,
    pub gts_uuid: Uuid,
    pub kind: EntityKind,
    pub lifecycle_status: LifecycleStatus,
    pub resource_version: i64,
    pub owning_gear: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// The authored document under the public resource name. Absent only if the
    /// current-state row is missing, which is a corrupt row rather than a state a
    /// reader should expect.
    pub content: Option<Value>,
    pub resolved_schema: Option<Value>,
    pub effective_traits: Option<Value>,
    pub effective_traits_schema: Option<Value>,
}

/// One entity's current state as its own kind stores it.
///
/// An enum rather than four `Option`s beside a kind discriminant, for the reason
/// `EvaluatedOutcome` is one on the write side: the pairing of a kind with the state
/// that kind actually has is then a compile-time fact, and "an Instance carrying a
/// resolved schema" is not a representable value.
enum CurrentState {
    /// The authored document and D3's materialized artifacts.
    TypeSchema {
        current: Option<CurrentTypeSchemaRow>,
        document: Option<CurrentDocument>,
    },
    /// The authored value. No artifact, because an Instance has none.
    Instance { value: Option<CurrentInstanceValue> },
}

/// What the service can fail with. One layer above the two admission halves, so a
/// transport adapter maps one type.
#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Acceptance(#[from] AcceptanceError),
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error("storage failure: {0}")]
    Storage(#[from] ScopeError),
    #[error("database failure: {0}")]
    Db(#[from] DbError),
    #[error("a stored document could not be read as JSON: {0}")]
    CorruptDocument(String),
}

/// How accepted operations are driven after their acceptance transaction commits.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionMode {
    /// Drive the worker before returning. Used by P0 API traffic and seeding.
    Inline,
    /// Leave the durable operation for the outbox worker.
    Outbox,
}

/// The database-backed registry service.
#[domain_model]
pub struct RegistryService {
    db: Db,
    /// The persistence ports. Injected rather than named concretely, which is what
    /// keeps this layer free of every `SeaORM` type; the gear passes
    /// `infra::storage::Repos`.
    stores: Arc<dyn Stores>,
    policy: RegistrationPolicy,
    config: TypesRegistryConfig,
    dispatch: Arc<dyn OperationDispatch>,
    admission_mode: AdmissionMode,
    /// The admission instruments (T16).
    metrics: Arc<dyn AdmissionMetrics>,
}

impl RegistryService {
    /// `admission_mode` drives the interim behaviour described in the module
    /// header: [`AdmissionMode::Inline`] until T21 starts the outbox worker, and
    /// permanently inline for the seeding path.
    #[must_use]
    pub fn new(
        db: Db,
        stores: Arc<dyn Stores>,
        policy: RegistrationPolicy,
        config: TypesRegistryConfig,
        dispatch: Arc<dyn OperationDispatch>,
        admission_mode: AdmissionMode,
        metrics: Arc<dyn AdmissionMetrics>,
    ) -> Self {
        Self {
            db,
            stores,
            policy,
            config,
            dispatch,
            admission_mode,
            metrics,
        }
    }

    /// See the module header for why this is `allow_all` and why that is honest
    /// rather than a bypass.
    fn scope() -> AccessScope {
        AccessScope::allow_all()
    }

    /// Accept a submission, and — while admission is inline — admit it.
    ///
    /// NOT cancel-safe, and cannot be: the acceptance transaction commits before
    /// inline admission starts, so a future dropped in between — the caller's
    /// connection closed, a `timeout` fired — leaves the operation `pending` with
    /// its work undone. Nothing here can undo the commit, and nothing should: the
    /// operation and its idempotency key are the record that the submission was
    /// accepted. Recovery is a replay under the same key, which re-enters the
    /// non-terminal branch below and drives the operation to completion; that is
    /// the only recovery path until T21's outbox exists.
    ///
    /// # Errors
    /// [`ServiceError::Acceptance`] for every synchronous refusal, including the
    /// fingerprint conflict; [`ServiceError::Worker`] only for an infrastructure
    /// failure during inline admission, never for a candidate that was refused on
    /// its merits.
    pub async fn submit(
        &self,
        request: &SubmitRequest,
        now: OffsetDateTime,
    ) -> Result<Accepted, ServiceError> {
        let provider: DBProvider<AcceptanceError> = DBProvider::new(self.db.clone());
        let accepted = accept(
            &self.stores,
            &provider,
            &Self::scope(),
            &AcceptanceContext {
                policy: &self.policy,
                config: &self.config,
                metrics: &self.metrics,
            },
            &self.dispatch,
            request,
            now,
        )
        .await?;

        // `!terminal`, not `!replayed`. A replay of a *non-terminal* operation is
        // the only recovery path there is until T21's outbox exists: if the process
        // died — or `run_operation` returned an infrastructure error — between the
        // acceptance commit and the inline admission, nothing else will ever drive
        // that operation, and gating on `replayed` left it `pending` with the client
        // polling forever. `run_operation` is idempotent: it returns early on
        // `completed` and skips items that are already terminal.
        let mut accepted = accepted;
        if self.admission_mode == AdmissionMode::Inline && !accepted.terminal() {
            // After the acceptance transaction committed, never inside it: the
            // worker reads the operation it is admitting.
            let worker: DBProvider<WorkerError> = DBProvider::new(self.db.clone());
            run_operation(
                &self.stores,
                &worker,
                &Self::scope(),
                Tuning {
                    limits: &self.config.limits,
                    worker: &self.config.worker,
                    metrics: &self.metrics,
                },
                accepted.operation_id,
                now,
            )
            .await?;
            // A pass that returns `Ok` leaves the operation `completed`, whether it
            // did the work or found it already terminal — `run_operation` ends with
            // `mark_completed` on the first path and returns early on the second. So
            // the receipt can carry the real status without reading the row back.
            accepted.status = OperationStatus::Completed;
        }
        Ok(accepted)
    }

    /// Read one operation and its per-candidate outcomes.
    ///
    /// # Errors
    /// [`ServiceError::Storage`] for a read failure. An absent operation is
    /// `Ok(None)`, because "not found" is an answer rather than a fault.
    pub async fn operation(&self, id: Uuid) -> Result<Option<OperationRecord>, ServiceError> {
        let provider: DBProvider<ServiceError> = DBProvider::new(self.db.clone());
        let scope = Self::scope();
        let stores = Arc::clone(&self.stores);
        // One snapshot for both reads. The operation's status and its items'
        // outcomes are written by two different transactions (`worker`), so two
        // independently-snapshotted reads could compose a pair that no single
        // committed state ever had.
        let Some((operation, items)) = provider
            .transaction_with_config(snapshot_read(&self.db), move |tx| {
                Box::pin(async move {
                    let Some(operation) = stores.find_by_id(tx, &scope, id).await? else {
                        return Ok(None);
                    };
                    let items = stores.find_items(tx, &scope, id).await?;
                    Ok(Some((operation, items)))
                })
            })
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(OperationRecord {
            operation_id: operation.id,
            kind: operation.kind,
            dry_run: operation.dry_run,
            status: operation.status,
            created_at: operation.created_at,
            started_at: operation.started_at,
            completed_at: operation.completed_at,
            items: items
                .into_iter()
                .map(|item| OperationItemRecord {
                    gts_id: item.gts_id,
                    status: item.status,
                    resource_version: item.result_resource_version,
                    error: item.error_payload,
                })
                .collect(),
        }))
    }

    /// Read one entity by identifier or Registry Reference, with its authored
    /// content and D3's materialized artifacts.
    ///
    /// **Both kinds, and the branch is on the row rather than on the key.** A Type
    /// Schema's current state is its document plus D3's three artifacts; an
    /// Instance's is its authored value and nothing derived. Asking the Type Schema
    /// store for both is what made an admitted Instance read back with a null
    /// `content` while its operation said `succeeded` — the read path was built at
    /// T9, when only Type Schemas existed, and T10 extended admission past it.
    ///
    /// A tombstone is returned: a DELETED entity stays exact-readable as deleted
    /// and only leaves discovery.
    ///
    /// # Errors
    /// [`ServiceError::Storage`] for a read failure, or
    /// [`ServiceError::CorruptDocument`] if a stored document is not JSON.
    pub async fn entity(&self, key: &EntityKey) -> Result<Option<EntityRecord>, ServiceError> {
        let provider: DBProvider<ServiceError> = DBProvider::new(self.db.clone());
        let scope = Self::scope();
        let stores = Arc::clone(&self.stores);
        let key = key.clone();

        // One snapshot for all three reads. `entity.resource_version`, the
        // current-state artifacts and the authored document are written by one
        // transaction, so reading them under three independent snapshots is what
        // would let a concurrent revision compose version N with the artifacts of
        // N + 1. Today only creations exist, so the window is closed by there being
        // no second revision; T11 opens it, and T29's conditional reads make
        // `resource_version` a wire promise about the body beside it.
        let Some((row, current)) = provider
            .transaction_with_config(snapshot_read(&self.db), move |tx| {
                Box::pin(async move {
                    let found = match &key {
                        EntityKey::GtsId(gts_id) => {
                            stores.find_by_gts_id(tx, &scope, gts_id).await?
                        }
                        EntityKey::Uuid(uuid) => stores.find_by_gts_uuid(tx, &scope, *uuid).await?,
                    };
                    let Some(row) = found else {
                        return Ok(None);
                    };
                    let current = match row.entity_kind {
                        EntityKind::TypeSchema => CurrentState::TypeSchema {
                            current: stores.find_current_schema(tx, &scope, row.id).await?,
                            document: stores.current_documents(tx, &scope, &[row.id]).await?.pop(),
                        },
                        // One read, not two: `current_values` returns the pointer's
                        // revision number with the value it points at, so the
                        // separate pointer read `find_current_instance` offers would
                        // buy a second statement and nothing else.
                        EntityKind::Instance => CurrentState::Instance {
                            value: stores.current_values(tx, &scope, &[row.id]).await?.pop(),
                        },
                    };
                    Ok(Some((row, current)))
                })
            })
            .await?
        else {
            return Ok(None);
        };

        let (content, resolved_schema, effective_traits, effective_traits_schema) = match current {
            CurrentState::TypeSchema { current, document } => {
                let current = current.ok_or_else(|| {
                    ServiceError::CorruptDocument(format!(
                        "entity '{}' has no current Type Schema state",
                        row.gts_id
                    ))
                })?;
                let document = document.ok_or_else(|| {
                    ServiceError::CorruptDocument(format!(
                        "entity '{}' has no current Type Schema document",
                        row.gts_id
                    ))
                })?;
                (
                    Some(parse_stored(&document.raw_schema, &row.gts_id)?),
                    Some(parse_stored(&current.resolved_schema, &row.gts_id)?),
                    Some(parse_stored(&current.effective_traits, &row.gts_id)?),
                    Some(parse_stored(&current.effective_traits_schema, &row.gts_id)?),
                )
            }
            // The three artifacts are `None` by construction rather than by
            // omission: an Instance has no derived state, so there is nothing a
            // later task could materialize into them.
            CurrentState::Instance { value } => {
                let value = value.ok_or_else(|| {
                    ServiceError::CorruptDocument(format!(
                        "entity '{}' has no current Instance state",
                        row.gts_id
                    ))
                })?;
                (
                    Some(parse_stored(&value.canonical_value, &row.gts_id)?),
                    None,
                    None,
                    None,
                )
            }
        };

        Ok(Some(EntityRecord {
            gts_id: row.gts_id,
            gts_uuid: row.gts_uuid,
            kind: row.entity_kind,
            lifecycle_status: row.lifecycle_status,
            resource_version: row.resource_version,
            owning_gear: row.owning_gear,
            created_at: row.created_at,
            updated_at: row.updated_at,
            content,
            resolved_schema,
            effective_traits,
            effective_traits_schema,
        }))
    }
}

fn parse_stored(text: &str, gts_id: &str) -> Result<Value, ServiceError> {
    serde_json::from_str(text)
        .map_err(|e| ServiceError::CorruptDocument(format!("'{gts_id}': {e}")))
}
