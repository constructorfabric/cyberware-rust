//! `SeaORM` entity models for the managed-state schema — one file per table, each
//! a mirror of its `CREATE TABLE` in `docs/database.sql`.
//!
//! Every table the migration creates has an entity here. `instance` and
//! `instance_revision` arrived with Registered Instances (T10) rather than with
//! the migration, on the rule that an entity with no reader is code the compiler
//! cannot check against the DDL — which is exactly the drift these mirrors exist
//! to prevent.
//!
//! `dependency` is here from T4 rather than T13 as first planned: T13 *writes*
//! edges, but T4's dependency-closure read — what the transient `gts-rust` store
//! of T5 is built from — already walks them.
//!
//! Every entity derives `Scopable` and declares its security dimensions; none
//! omits the attribute. All are `#[secure(unrestricted)]` in P0, and each carries
//! a `ponytail:` comment recording ceiling C6 — no PDP — and its upgrade path at
//! the point where the choice is made.
//!
//! Enumeration columns map to the typed Rust enums in [`enums`]. Those integers
//! are storage-only; the SDK and REST expose the string vocabulary.

pub mod enums;

// `entity::entity` is module inception, and deliberate: these files are a DDL
// mirror, so each is named after its table and the table is
// `types_registry__entity`. Renaming the file would put the mirror out of step
// with the schema it exists to track.
pub mod coordination_state;
pub mod dependency;
#[allow(clippy::module_inception)]
pub mod entity;
pub mod instance;
pub mod instance_revision;
pub mod operation;
pub mod operation_item;
pub mod type_schema;
pub mod type_schema_revision;
pub mod version_family;

#[cfg(test)]
#[path = "columns_tests.rs"]
mod columns_tests;
