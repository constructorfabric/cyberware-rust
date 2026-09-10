//! The domain's enumeration vocabularies.
//!
//! These carry **no** storage representation: no `smallint` mapping, no `sea_orm`
//! derive, nothing tying a variant to a column. Their persisted counterparts live
//! in `infra::storage::entity::enums`, which owns the integers `database.sql` fixes
//! and converts in both directions.
//!
//! # Why the same variants exist twice
//!
//! The storage enums are constrained by an append-only numbering, by CHECK
//! constraints that enumerate values, and by the rule that numbering is
//! deliberately *not* aligned between columns — none of which is a domain concern.
//! The domain enums are constrained only by what the business rules distinguish.
//! The duplication costs two edits per new variant; what keeps it honest is that
//! the conversions in `infra::storage::entity::enums` are exhaustive matches, so a
//! new variant fails to compile until the other side and the DDL agree. A single
//! shared type let a variant reach the database with no CHECK to admit it.
//!
//! No `Serialize` / `Deserialize` / `ToSchema` here either: the wire vocabulary is
//! REST's business (`api/rest/dto.rs`), and deriving serde on a domain enum is how
//! the storage numbering leaked onto the wire in the first place.

use toolkit_macros::domain_model;

/// `version_family.ownership_scope`, `entity.ownership_scope` — global or tenant
/// ownership (ADR-0009).
///
/// P0 admits `Global` only; `Tenant` exists because the distinction is already
/// part of the model P1 tenancy fills in.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnershipScope {
    Global,
    Tenant,
}

/// What kind of entity an identifier names.
///
/// Follows from the identifier's trailing `~`; it is carried explicitly because
/// several rules branch on it and re-deriving it at each branch would let the two
/// readings drift.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityKind {
    TypeSchema,
    Instance,
}

impl EntityKind {
    /// The public spelling, for a message a caller reads.
    ///
    /// `Display` rather than `{:?}` because a refusal naming `TypeSchema` is a
    /// wire-visible string: `clippy::use_debug` is denied precisely so a derive's
    /// output cannot become an API by accident.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TypeSchema => "Type Schema",
            Self::Instance => "Registered Instance",
        }
    }
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether an entity is live or a tombstone (ADR-0008).
///
/// P0 has no managed `deprecated` state. `Deleted` entities remain readable so
/// issued Registry References stay reverse-resolvable until purge.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LifecycleStatus {
    Active,
    Deleted,
}

/// What an operation was asked to do (ADR-0012).
///
/// Dry run is **not** a kind: it is orthogonal, carried alongside, and part of the
/// request fingerprint — so one `Idempotency-Key` cannot replay a dry run as a
/// commit.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperationKind {
    Registration,
    Deletion,
}

/// Which plane an operation belongs to.
///
/// Every P0 operation is `Platform`. The plane is expressed by the contract and
/// the data, not enforced by the transport (ceiling C8).
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Plane {
    Platform,
    Tenant,
}

/// An operation's **progress**, not its outcome.
///
/// `Completed` means every item is terminal; the outcomes themselves live on the
/// items and are deliberately not aggregated here. There is no cancellation or
/// expiry state: redelivery retries idempotently, and after exhaustion existing
/// terminal outcomes stay intact while unfinished items fail with a reason.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperationStatus {
    Pending,
    Running,
    Completed,
}

/// One candidate's outcome.
///
/// Status distinguishes *effects*; the error payload distinguishes causes. For a
/// committed item `Succeeded` changed entity state; under dry run it means every
/// check passed and nothing was written. `Unchanged` proved redundancy — equal
/// canonical authored content — and creates no revision.
///
/// There is no `Blocked`: it has the same stored effect as failure and uses a
/// `blocked_by_dependency` reason instead. Dry-run success is `Succeeded` rather
/// than a separate "would succeed", because the operation already exposes the mode
/// and restating it per item would be a second vocabulary.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperationItemStatus {
    Pending,
    Running,
    Succeeded,
    Unchanged,
    Failed,
}

/// Why one entity depends on another.
///
/// `x-gts-ref` is excluded because it validates identifier syntax without reading a target.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DependencyKind {
    SchemaRef,
    Derivation,
    InstanceOf,
}
