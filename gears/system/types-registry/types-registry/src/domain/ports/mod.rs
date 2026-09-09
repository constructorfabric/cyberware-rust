//! The persistence ports the domain calls, and the row and input types that cross
//! them.
//!
//! SPEC §8 makes acceptance and admission each one transaction, so the transaction
//! boundary is a business rule and the domain orchestrates it: the transaction
//! crosses the boundary as `&DbTx<'_>`. What the ports hide is every `SeaORM` type
//! — entities, active models, column enums — so the domain names `toolkit_db` and
//! nothing below it. The row and input types below are what the repositories in
//! `infra::storage::repo` themselves take and return, which is why `store.rs`
//! forwards without translating.
//!
//! # Why every port takes `&DbTx<'_>` and not a runner
//!
//! Three candidates, and the first two are unavailable rather than unattractive:
//!
//! - `&impl DBRunner` on a trait method makes the trait non-dyn-safe, which would
//!   push a type parameter through every domain function into the gear's wiring.
//!   `Arc<dyn Stores>` would be impossible.
//! - `&dyn DBRunner` *can be built* — sealing prevents implementing the trait, not
//!   coercing to it — but cannot be *executed on*: the secure query API spells its
//!   parameter `&impl DBRunner` (`toolkit-db/src/secure/select.rs`), whose implicit
//!   `Sized` bound an unsized `dyn` runner fails. `toolkit_db::outbox` opted out
//!   with `&(impl DBRunner + Sync + ?Sized)`; the secure API has not.
//! - `&DbTx<'_>` is concrete, so the traits stay dyn-safe. Its cost is that
//!   **every** port call runs inside a transaction, reads included — the
//!   deliberate choice, because a read that consults two tables must not straddle
//!   a concurrent commit (see [`snapshot_read`]).
//!
//! The repositories underneath keep `runner: &impl DBRunner` as
//! `11_database_patterns.md` prescribes, so they stay usable outside a
//! transaction. That is the whole difference between a repository and a port here.
//!
//! # Rows mirror their tables
//!
//! Each `*Row` carries every column of its table rather than the subset today's
//! callers read, so a rule that starts consulting one more column needs no port
//! change. The mapping is a field-for-field move the compiler checks.

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, ScopeError, TxAccessMode, TxConfig, TxIsolationLevel};
use toolkit_db::{Db, DbTx};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::admission::Precondition;
use crate::domain::admission::fingerprint::{RequestFingerprint, ScopeHash};
use crate::domain::enums::{
    DependencyKind, EntityKind, LifecycleStatus, OperationItemStatus, OperationKind,
    OperationStatus, OwnershipScope, Plane,
};
use crate::domain::family::FamilyKey;

// The output port the admission path's instruments cross (T16).
pub mod metrics;

// ---------------------------------------------------------------------------
// Read transactions
// ---------------------------------------------------------------------------

/// The configuration a **multi-statement read** must run under.
///
/// A transaction alone is not enough on every backend. `PostgreSQL` defaults to
/// `READ COMMITTED`, where every statement takes a fresh snapshot — so two reads
/// inside one such transaction can still straddle a concurrent commit and compose a
/// state that never existed. `RepeatableRead` is snapshot isolation there;
/// `MySQL`/`InnoDB` is already at that level, and asking makes the requirement
/// explicit rather than inherited from a server default. `ReadOnly` is an
/// assertion, not an optimisation: both engines reject a write inside such a
/// transaction.
///
/// **`SQLite` is asked for nothing, deliberately.** Its transactions are
/// serializable by construction — a reader holds a WAL snapshot or a shared lock
/// for the duration — and `SeaORM` does not translate the request anyway:
/// `sqlx_sqlite`'s `set_transaction_config` logs one `WARN` per unsupported setting
/// (observed: two lines per read on the backend `quickstart.yaml` binds).
///
/// A **single**-statement read needs none of this — one statement is atomic on its
/// own — so those paths use a plain transaction, which the ports still require.
#[must_use]
pub fn snapshot_read(db: &Db) -> TxConfig {
    snapshot_read_for(db.db_engine())
}

/// The engine-keyed half of [`snapshot_read`], split out so the mapping is testable
/// without a database.
fn snapshot_read_for(engine: &str) -> TxConfig {
    if engine == "sqlite" {
        return TxConfig::default();
    }
    TxConfig {
        isolation: Some(TxIsolationLevel::RepeatableRead),
        access_mode: Some(TxAccessMode::ReadOnly),
    }
}

/// The configuration a **commit transaction** must run under: the mirror image of
/// [`snapshot_read`], and for the same reason — a server default is not a contract.
///
/// A commit transaction rechecks and writes, so it wants the *latest* committed
/// state: every recheck in SPEC §8.1 step 4 exists to see what another admission
/// just did. `PostgreSQL` gives that by default; `MySQL`/`InnoDB` defaults to
/// `REPEATABLE READ`, where the re-read that recovers from an absorbed unique
/// conflict (`repo::conflict_do_nothing`) would still see the transaction's opening
/// snapshot and miss the winner's row. `READ COMMITTED` makes the two backends
/// agree.
///
/// No `access_mode`: this transaction writes. `SQLite` is asked for nothing, as in
/// [`snapshot_read`].
#[must_use]
pub fn commit_write(db: &Db) -> TxConfig {
    commit_write_for(db.db_engine())
}

/// The engine-keyed half of [`commit_write`].
fn commit_write_for(engine: &str) -> TxConfig {
    if engine == "sqlite" {
        return TxConfig::default();
    }
    TxConfig {
        isolation: Some(TxIsolationLevel::ReadCommitted),
        access_mode: None,
    }
}

#[cfg(test)]
mod snapshot_read_tests {
    use super::{TxAccessMode, TxIsolationLevel, commit_write_for, snapshot_read_for};

    /// A commit transaction asks for the opposite of a snapshot: the latest state,
    /// and no read-only assertion. `MySQL` is the one that needs the request — its
    /// default would hide the winner's row from the loser's recovering re-read.
    #[test]
    fn a_commit_transaction_asks_for_read_committed_and_may_write() {
        for engine in ["postgres", "mysql"] {
            let cfg = commit_write_for(engine);
            assert_eq!(
                cfg.isolation,
                Some(TxIsolationLevel::ReadCommitted),
                "{engine}: a recheck must see what another admission committed",
            );
            assert_eq!(cfg.access_mode, None, "{engine}: this transaction writes");
        }
        let sqlite = commit_write_for("sqlite");
        assert!(sqlite.isolation.is_none() && sqlite.access_mode.is_none());
    }

    #[test]
    fn postgres_and_mysql_get_a_read_only_snapshot() {
        for engine in ["postgres", "mysql"] {
            let cfg = snapshot_read_for(engine);
            assert_eq!(
                cfg.isolation,
                Some(TxIsolationLevel::RepeatableRead),
                "{engine} defaults are not enough for a multi-statement read",
            );
            assert_eq!(cfg.access_mode, Some(TxAccessMode::ReadOnly));
        }
    }

    #[test]
    fn sqlite_is_asked_for_nothing() {
        let cfg = snapshot_read_for("sqlite");
        assert!(
            cfg.isolation.is_none() && cfg.access_mode.is_none(),
            "SeaORM warns rather than translating, and SQLite is serializable anyway",
        );
    }

    /// An engine this function has not been taught about must get the safe
    /// configuration, not the permissive one.
    #[test]
    fn an_unknown_engine_gets_the_snapshot() {
        let cfg = snapshot_read_for("unknown");
        assert_eq!(cfg.isolation, Some(TxIsolationLevel::RepeatableRead));
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One `entity` row.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRow {
    pub id: i64,
    pub gts_uuid: Uuid,
    pub gts_id: String,
    pub entity_kind: EntityKind,
    pub family_id: i64,
    pub ownership_scope: OwnershipScope,
    pub owner_tenant_id: Option<Uuid>,
    pub owning_gear: Option<String>,
    pub lifecycle_status: LifecycleStatus,
    pub resource_version: i64,
    pub deleted_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One `version_family` row.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionFamilyRow {
    pub id: i64,
    pub family_key: FamilyKey,
    pub ownership_scope: OwnershipScope,
    pub owner_tenant_id: Option<Uuid>,
    pub created_at: OffsetDateTime,
}

/// One `operation` row.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRow {
    pub id: Uuid,
    pub kind: OperationKind,
    pub dry_run: bool,
    pub plane: Plane,
    pub tenant_id: Option<Uuid>,
    pub principal_id: Uuid,
    pub idempotency_key: String,
    pub idempotency_scope_hash: ScopeHash,
    pub request_fingerprint: RequestFingerprint,
    pub status: OperationStatus,
    pub created_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
}

/// One `operation_item` row.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationItemRow {
    pub id: i64,
    pub operation_id: Uuid,
    pub item_no: i32,
    pub gts_id: String,
    pub dry_run: bool,
    pub kind: OperationKind,
    pub precondition: Precondition,
    pub status: OperationItemStatus,
    pub request_payload: Option<String>,
    pub result_revision_no: Option<i32>,
    pub result_resource_version: Option<i64>,
    pub error_payload: Option<String>,
    pub created_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
}

/// One `type_schema` current-state row: the revision pointer plus D3's
/// materialized artifacts.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentTypeSchemaRow {
    pub entity_id: i64,
    pub revision_no: i32,
    pub resolved_schema: String,
    pub effective_traits: String,
    pub effective_traits_schema: String,
    pub resolution_fingerprint: Vec<u8>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The current authored document of one entity.
///
/// This is the *authored* document on the revision, never the materialized
/// artifacts: those are outputs of the very resolution the store performs (D3), so
/// feeding them back in would compose an already-composed document.
#[domain_model]
#[derive(Clone, Debug)]
pub struct CurrentDocument {
    pub entity_id: i64,
    pub revision_no: i32,
    /// The authored document as submitted, canonical UTF-8 text. Parsing it is the
    /// caller's job: this port moves bytes, and the layer that knows what a
    /// malformed document means is the one that names the entity in the error.
    pub raw_schema: String,
    /// The digest of [`Self::raw_schema`], so the `unchanged` decision can reject an
    /// inequality without comparing whole documents. A **prefilter only**
    /// (ADR-0012): equality proposes redundancy, the bytes confirm it — which is why
    /// the text travels beside the digest rather than instead of it.
    pub content_hash: Vec<u8>,
    /// The projection state to use when writing artifacts derived from this document.
    pub projection: CurrentSchemaCas,
}

/// The result of a reverse-impact read.
#[domain_model]
#[derive(Clone, Debug)]
pub enum ReverseImpact {
    /// Every dependent, `gts_id`-sorted, roots excluded.
    Within(Vec<EntityRow>),
    /// More dependents than the bound admits.
    OverBound { at_least: usize, bound: usize },
}

/// The result of a dependency-closure read.
#[domain_model]
#[derive(Clone, Debug)]
pub struct DependencyClosure {
    /// The resolved roots plus everything they transitively consume, `gts_id`
    /// sorted. Tombstones are **included**: a deleted entity remains the
    /// compatibility baseline until purge, so omitting it would let an ordinary
    /// deletion move the baseline.
    pub entities: Vec<EntityRow>,
    /// Candidate identifiers with no entity row, sorted and deduplicated. A first
    /// admission's own candidate is always here, which is why this is a reported
    /// outcome rather than an error.
    pub missing_roots: Vec<String>,
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Everything an entity needs at first admission. `resource_version`,
/// `lifecycle_status` and `deleted_at` are not parameters: a new entity is always
/// active at version 1 with no tombstone.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewEntity {
    /// The `UUIDv5` Registry Reference derived from `gts_id` by the caller, which
    /// owns the derivation so no layer below re-derives it.
    pub gts_uuid: Uuid,
    pub gts_id: String,
    pub entity_kind: EntityKind,
    pub family_id: i64,
    pub ownership_scope: OwnershipScope,
    pub owner_tenant_id: Option<Uuid>,
    pub owning_gear: Option<String>,
    pub now: OffsetDateTime,
}

/// One immutable authored revision.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewRevision {
    pub entity_id: i64,
    pub revision_no: i32,
    pub raw_schema: String,
    pub content_hash: Vec<u8>,
    pub gts_spec_version: String,
    pub gts_impl_version: String,
    pub compat_forced: bool,
    pub operation_item_id: i64,
    pub now: OffsetDateTime,
}

/// The revision and fingerprint a current-schema write expects to replace.
#[domain_model]
#[derive(Clone, Debug)]
pub struct CurrentSchemaCas {
    pub revision_no: i32,
    pub resolution_fingerprint: Vec<u8>,
}

/// The current-state row to write: the revision pointer plus D3's materialized
/// artifacts.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewCurrentTypeSchema {
    pub entity_id: i64,
    pub revision_no: i32,
    pub resolved_schema: String,
    pub effective_traits: String,
    pub effective_traits_schema: String,
    pub resolution_fingerprint: Vec<u8>,
    pub now: OffsetDateTime,
}

/// The current-state row of one Registered Instance.
///
/// Thinner than [`CurrentTypeSchemaRow`]: an Instance has no artifact and no
/// fingerprint — nothing about it is derived from other entities.
#[domain_model]
#[derive(Clone, Debug)]
pub struct CurrentInstanceRow {
    pub entity_id: i64,
    pub revision_no: i32,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The current authored value of one Instance, with the schema revision it was
/// validated against.
///
/// The schema pair travels with the value: knowing *why* it is valid needs the exact
/// revision, and that schema's current revision may already have moved.
#[domain_model]
#[derive(Clone, Debug)]
pub struct CurrentInstanceValue {
    pub entity_id: i64,
    pub revision_no: i32,
    /// The authored value as submitted, canonical UTF-8 text. Parsing it is the
    /// caller's job, as on [`CurrentDocument`].
    pub canonical_value: String,
    /// The digest of [`Self::canonical_value`] — a prefilter, as on
    /// [`CurrentDocument::content_hash`].
    pub content_hash: Vec<u8>,
    pub type_schema_entity_id: i64,
    pub type_schema_revision_no: i32,
}

/// An immutable Instance revision to insert.
///
/// No `compat_forced` counterpart to [`NewRevision`]: an Instance is either valid
/// against its schema revision or refused, so `force` has nothing to waive.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewInstanceRevision {
    pub entity_id: i64,
    pub revision_no: i32,
    pub canonical_value: String,
    pub content_hash: Vec<u8>,
    /// The revision that validated this value; `ON DELETE RESTRICT` pins it.
    pub type_schema_entity_id: i64,
    pub type_schema_revision_no: i32,
    pub gts_spec_version: String,
    pub gts_impl_version: String,
    pub operation_item_id: i64,
    pub now: OffsetDateTime,
}

/// The current-revision pointer to write. Carries no artifact — there is none.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewCurrentInstance {
    pub entity_id: i64,
    pub revision_no: i32,
    pub now: OffsetDateTime,
}

/// Everything an operation needs at acceptance.
///
/// `status`, `started_at` and `completed_at` are not parameters: an accepted
/// operation is always pending with neither timestamp, and the stored CHECK
/// enforces that pairing.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewOperation {
    pub id: Uuid,
    pub kind: OperationKind,
    pub dry_run: bool,
    pub plane: Plane,
    pub tenant_id: Option<Uuid>,
    pub principal_id: Uuid,
    pub idempotency_key: String,
    /// Digest of (plane, `tenant_id`, `principal_id`) — see
    /// [`crate::domain::admission::fingerprint`] for why this is digested rather
    /// than carried as three columns.
    pub idempotency_scope_hash: ScopeHash,
    pub request_fingerprint: RequestFingerprint,
    pub now: OffsetDateTime,
}

/// One accepted candidate. `kind` and `dry_run` are copied from the parent by
/// [`OperationStore::insert_items`] rather than being fields here, because the
/// composite foreign key ties them to the parent's and letting a caller pass them
/// separately would let the two disagree.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewOperationItem {
    pub item_no: i32,
    pub gts_id: String,
    pub precondition: Precondition,
    /// The canonical request body. The stored CHECK requires it while the item is
    /// non-terminal, and the worker drops it at terminality.
    pub request_payload: String,
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// The write path's serialization point.
#[async_trait]
pub trait EntityWriteOrderStore: Send + Sync {
    /// Advance `entity_write_order` as the transaction's first statement.
    async fn claim_entity_write_order(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        now: OffsetDateTime,
    ) -> Result<(), ScopeError>;
}

/// The version family: the lock the family-wide rules are serialized by.
#[async_trait]
pub trait VersionFamilyStore: Send + Sync {
    /// Take the family, creating it if this is its first member. The `bool` is
    /// `true` when this call created it.
    async fn create_or_get(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_key: &FamilyKey,
        ownership_scope: OwnershipScope,
        owner_tenant_id: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<(VersionFamilyRow, bool), ScopeError>;
}

/// Entity identity and lifecycle.
#[async_trait]
pub trait EntityStore: Send + Sync {
    /// Exact read by GTS identifier. Tombstones are returned: a deleted entity
    /// stays reverse-resolvable until purge.
    async fn find_by_gts_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_id: &str,
    ) -> Result<Option<EntityRow>, ScopeError>;

    /// Batch exact read.
    async fn find_by_gts_ids(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_ids: &[String],
    ) -> Result<Vec<EntityRow>, ScopeError>;

    /// Exact read by Registry Reference.
    async fn find_by_gts_uuid(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_uuid: Uuid,
    ) -> Result<Option<EntityRow>, ScopeError>;

    /// The kind of one member of a family, or `None` when the family is empty.
    /// The input to T10's one-kind-per-family rule.
    async fn kind_in_family(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_id: i64,
    ) -> Result<Option<EntityKind>, ScopeError>;

    /// Named `insert_entity` rather than `insert` because [`Stores`] merges every
    /// port into one trait object, and two same-named methods on it would need
    /// fully-qualified syntax at each call.
    ///
    /// `None` means a concurrent writer already holds the identifier: the unique
    /// key is the serialization point, and its conflict is absorbed rather than
    /// raised so that the transaction this runs in stays usable on every backend.
    /// The caller's answer is the same one the existence check gives —
    /// `already_exists`.
    async fn insert_entity(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewEntity,
    ) -> Result<Option<EntityRow>, ScopeError>;

    /// Advance `resource_version` if and only if the entity is **active** and
    /// still at `expected`.
    ///
    /// The check is in the statement's `WHERE`, so there is no window between reading
    /// the version and moving it. The lifecycle is there for the same reason: a
    /// deletion can commit between the caller's read and this call, and a revision
    /// must not resurrect a tombstone. `None` is a lost race, which the caller turns
    /// into `precondition_failed`; `Some(next)` is the value the database actually
    /// committed, so the domain never reconstructs it independently.
    async fn compare_and_swap_version(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
        expected_resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<Option<i64>, ScopeError>;
}

/// Authored revisions and the current-state row.
#[async_trait]
pub trait TypeSchemaStore: Send + Sync {
    /// The current authored document of each named entity. Entities with no
    /// current row are simply absent.
    async fn current_documents(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentDocument>, ScopeError>;

    async fn find_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentTypeSchemaRow>, ScopeError>;

    /// The current-state rows of many entities in one read, `entity_id`-sorted.
    async fn current_schemas(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentTypeSchemaRow>, ScopeError>;

    async fn insert_schema_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewRevision,
    ) -> Result<(), ScopeError>;

    async fn insert_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
    ) -> Result<(), ScopeError>;

    /// Move an existing current-state row onto a newly admitted revision,
    /// re-materializing D3's artifacts with it.
    ///
    /// Separate from [`Self::insert_current_schema`] rather than one upsert: an
    /// insert that finds a row and an update that finds none are different bugs, and
    /// collapsing them would silence both. `Ok(false)` means no row matched.
    ///
    /// `expected` makes every artifact update a mandatory compare-and-swap.
    async fn update_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
        expected: CurrentSchemaCas,
    ) -> Result<bool, ScopeError>;
}

/// Registered Instances: immutable revisions and the current-revision pointer.
///
/// Separate from [`TypeSchemaStore`] rather than generic over kind: the revisions
/// record different things and only one current row has artifacts. A shared trait
/// would make both halves optional on both sides.
#[async_trait]
pub trait InstanceStore: Send + Sync {
    /// The current authored value of each named entity, with its schema pair.
    /// Entities with no current row are simply absent.
    async fn current_values(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentInstanceValue>, ScopeError>;

    async fn find_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentInstanceRow>, ScopeError>;

    async fn insert_instance_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewInstanceRevision,
    ) -> Result<(), ScopeError>;

    async fn insert_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<(), ScopeError>;

    /// Move an existing current-revision pointer, for the reasons
    /// [`TypeSchemaStore::update_current_schema`] states.
    async fn update_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<bool, ScopeError>;
}

/// Operations and their per-candidate items.
#[async_trait]
pub trait OperationStore: Send + Sync {
    async fn find_by_idempotency(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        idempotency_scope_hash: &ScopeHash,
        idempotency_key: &str,
    ) -> Result<Option<OperationRow>, ScopeError>;

    async fn find_by_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<OperationRow>, ScopeError>;

    async fn insert_operation(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewOperation,
    ) -> Result<OperationRow, ScopeError>;

    /// `kind` and `dry_run` come from `parent`, never from the items.
    async fn insert_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        parent: &OperationRow,
        items: &[NewOperationItem],
    ) -> Result<(), ScopeError>;

    async fn find_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        operation_id: Uuid,
    ) -> Result<Vec<OperationItemRow>, ScopeError>;

    /// Each `mark_*` returns `false` when the row was not in the state the move
    /// requires — an ordinary concurrent-worker outcome, not a fault.
    async fn mark_running(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError>;

    async fn mark_completed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError>;

    async fn mark_item_succeeded(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        revision_no: i32,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError>;

    /// Record a candidate whose authored content already equalled the current
    /// revision. No `revision_no`, because an `unchanged` candidate allocates none
    /// (ADR-0005); the resource version is the one that did **not** move.
    async fn mark_item_unchanged(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError>;

    async fn mark_item_failed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        error_payload: String,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError>;
}

/// Dependency edges.
#[async_trait]
pub trait DependencyStore: Send + Sync {
    /// The roots plus everything they transitively consume.
    async fn closure(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[String],
    ) -> Result<DependencyClosure, ScopeError>;

    /// Everything that transitively depends on any of `roots`, roots excluded.
    async fn reverse_impact(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[i64],
        write_set_bound: usize,
    ) -> Result<ReverseImpact, ScopeError>;

    /// Replace one entity's **outgoing** edges, and only that entity's.
    async fn replace_outgoing(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        from_entity_id: i64,
        edges: &[(DependencyKind, i64)],
    ) -> Result<(), ScopeError>;
}

/// Every port in one handle, so a caller wires one value rather than seven.
///
/// Because all seven are reached through one handle, no two ports may share a method
/// name — hence `insert_schema_revision` against `insert_instance_revision`. Which
/// also puts the kind where a reader of a commit path needs it: the call site.
///
/// The blanket implementation means an adapter implementing the seven traits
/// satisfies this for free.
pub trait Stores:
    EntityWriteOrderStore
    + VersionFamilyStore
    + EntityStore
    + TypeSchemaStore
    + InstanceStore
    + OperationStore
    + DependencyStore
{
}

impl<T> Stores for T where
    T: EntityWriteOrderStore
        + VersionFamilyStore
        + EntityStore
        + TypeSchemaStore
        + InstanceStore
        + OperationStore
        + DependencyStore
{
}
