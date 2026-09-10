//! Storage-only enumeration vocabularies for the managed-state schema.
//!
//! `database.sql` stores every enumeration as `smallint`, because no native enum
//! representation is shared by `SQLite`, `PostgreSQL` and `MySQL`. These types are
//! the typed Rust side of that mapping and nothing more: **the integers are
//! storage-only, and the SDK and REST expose the string vocabulary.** No
//! `Serialize` / `Deserialize` / `ToSchema` is derived here, so the numbers
//! cannot leak onto the wire by accident.
//!
//! # Why one file rather than one per column
//!
//! `database.sql` fixes three rules about the numbering, and all three are one
//! concern:
//!
//! * numbering is **append-only** after the first release, because renumbering is a
//!   data migration — a new value takes the next free number, a retired number is
//!   never reused;
//! * numbering is **per column and deliberately not aligned** between columns. `3`
//!   is `completed` in `operation.status` and `succeeded` in
//!   `operation_item.status`; where two columns agree that is coincidence, and it
//!   MUST NOT be turned into a contract;
//! * CHECK constraints **enumerate** the allowed values, so every variant here has
//!   a counterpart in the DDL.
//!
//! One place for the numbering makes the round-trip test — which asserts the exact
//! integers from `database.sql` — a single guard instead of six scattered ones.
//!
//! [`OwnershipScope`] and [`OperationKind`] are each shared by two columns
//! deliberately: `entity.ownership_scope` copies `version_family.ownership_scope`
//! under the family lock, and `operation_item.kind` copies `operation.kind` under
//! `fk_tr_operation_item_operation`. One type per *vocabulary*, not per column.

use sea_orm::entity::prelude::*;

/// `version_family.ownership_scope`, `entity.ownership_scope` — 1 global,
/// 2 tenant (ADR-0009).
///
/// P0 writes `Global` only; `Tenant` exists because the column and its CHECK are
/// created now so P1 tenancy needs no schema migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum OwnershipScope {
    #[sea_orm(num_value = 1)]
    Global,
    #[sea_orm(num_value = 2)]
    Tenant,
}

/// `entity.entity_kind` — 1 `type_schema`, 2 instance.
///
/// Follows from the identifier's trailing `~`, and is materialized because
/// suffix predicates are not portably indexable and the value drives
/// kind-specific constraints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum EntityKind {
    #[sea_orm(num_value = 1)]
    TypeSchema,
    #[sea_orm(num_value = 2)]
    Instance,
}

/// `entity.lifecycle_status` — 1 active, 2 deleted (ADR-0008).
///
/// P0 has no managed `deprecated` state. `Deleted` rows remain as tombstones so
/// issued Registry References stay reverse-resolvable until purge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum LifecycleStatus {
    #[sea_orm(num_value = 1)]
    Active,
    #[sea_orm(num_value = 2)]
    Deleted,
}

/// `operation.kind`, `operation_item.kind` — 1 registration, 2 deletion
/// (ADR-0012).
///
/// Dry run is **not** a kind: it is orthogonal, carried by the `dry_run` column,
/// and included in the request fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum OperationKind {
    #[sea_orm(num_value = 1)]
    Registration,
    #[sea_orm(num_value = 2)]
    Deletion,
}

/// `operation.plane` — 1 platform, 2 tenant.
///
/// Every P0 operation is `Platform`. The plane is expressed by the contract and
/// the data, not enforced by the transport (ceiling C8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum Plane {
    #[sea_orm(num_value = 1)]
    Platform,
    #[sea_orm(num_value = 2)]
    Tenant,
}

/// `operation.status` — 1 pending, 2 running, 3 completed.
///
/// **Progress only.** `Completed` means every item is terminal; outcomes live on
/// `operation_item` and are not aggregated here. There is no cancellation or
/// expiry state: outbox redelivery retries idempotently, and after exhaustion
/// existing terminal outcomes stay intact while unfinished items fail with an
/// `error_payload`.
///
/// Its `3` is *not* [`OperationItemStatus`]'s `3`. See the module header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum OperationStatus {
    #[sea_orm(num_value = 1)]
    Pending,
    #[sea_orm(num_value = 2)]
    Running,
    #[sea_orm(num_value = 3)]
    Completed,
}

/// `operation_item.status` — 1 pending, 2 running, 3 succeeded, 4 unchanged,
/// 5 failed.
///
/// Status distinguishes *effects*; `error_payload` distinguishes causes. For a
/// committed item `Succeeded` changed entity state; under dry run it means every
/// check passed and nothing was written. `Unchanged` proved redundancy — equal
/// canonical authored content — and creates no revision.
///
/// There is no `blocked` status: it has the same stored effect as failure and
/// uses a `blocked_by_dependency` error reason instead. Dry-run success is
/// `Succeeded` rather than a separate "would succeed", because the operation
/// already exposes the mode and restating it per item would be a second
/// vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum OperationItemStatus {
    #[sea_orm(num_value = 1)]
    Pending,
    #[sea_orm(num_value = 2)]
    Running,
    #[sea_orm(num_value = 3)]
    Succeeded,
    #[sea_orm(num_value = 4)]
    Unchanged,
    #[sea_orm(num_value = 5)]
    Failed,
}

/// `dependency.kind` — 1 `schema_ref` (`$ref`), 2 derivation (immediate base),
/// 3 `instance_of` (conforming Type Schema).
///
/// `x-gts-ref` is not represented: it constrains an instance value without resolving
/// or inlining a target. The numbering is append-only after the first release, and
/// `ck_tr_dependency_kind` admits exactly these three values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum DependencyKind {
    #[sea_orm(num_value = 1)]
    SchemaRef,
    #[sea_orm(num_value = 2)]
    Derivation,
    #[sea_orm(num_value = 3)]
    InstanceOf,
}

// ---------------------------------------------------------------------------
// Storage <-> domain conversions
// ---------------------------------------------------------------------------
//
// The domain has its own copy of every vocabulary here (`domain::enums`), free of
// the storage numbering. These conversions are the only bridge, and they are
// exhaustive matches on both sides: adding a variant to either enum fails to
// compile until its counterpart and the DDL's CHECK agree. That is the property
// that makes the duplication safe rather than a place for the two to drift.

use crate::domain::enums as domain;

/// One `From` in each direction, variant for variant. The variant list is written
/// once and both matches are generated from it, so the two directions cannot
/// disagree about the pairing.
macro_rules! bridge_enum {
    ($name:ident, $($variant:ident),+ $(,)?) => {
        impl From<domain::$name> for $name {
            fn from(value: domain::$name) -> Self {
                match value {
                    $(domain::$name::$variant => Self::$variant,)+
                }
            }
        }

        impl From<$name> for domain::$name {
            fn from(value: $name) -> Self {
                match value {
                    $($name::$variant => Self::$variant,)+
                }
            }
        }
    };
}

bridge_enum!(OwnershipScope, Global, Tenant);
bridge_enum!(EntityKind, TypeSchema, Instance);
bridge_enum!(LifecycleStatus, Active, Deleted);
bridge_enum!(OperationKind, Registration, Deletion);
bridge_enum!(Plane, Platform, Tenant);
bridge_enum!(OperationStatus, Pending, Running, Completed);
bridge_enum!(
    OperationItemStatus,
    Pending,
    Running,
    Succeeded,
    Unchanged,
    Failed
);
bridge_enum!(DependencyKind, SchemaRef, Derivation, InstanceOf);

#[cfg(test)]
#[path = "enums_tests.rs"]
mod enums_tests;
