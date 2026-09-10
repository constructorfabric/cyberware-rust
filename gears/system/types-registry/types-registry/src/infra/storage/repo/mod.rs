//! Repositories over the managed-state schema. One file per repository
//! (`02_gear_layout_and_sdk_pattern.md`); the shared `IN_CHUNK` and the re-exports
//! live here.
//!
//! # These repositories speak the domain's row types, not `SeaORM`'s
//!
//! Every method takes and returns the types in [`crate::domain::ports`], mapping
//! its own `SeaORM` models at the edge — as `credstore` and `mini-chat` do.
//! [`super::store`] therefore holds no mapping, only the `&DbTx` port signatures
//! the domain's dyn-safe traits need. [`EntityPage`] and [`PageRequest`] stay here
//! because paging is not a port yet.
//!
//! Every method takes `runner: &impl DBRunner`, never `&SecureConn`, so one body
//! serves both a pooled connection and a transaction — which is how the admission
//! worker runs the same read inside and outside its commit transaction.
//!
//! # GTS matching is never translated into SQL
//!
//! `constraint-gts-implementation` makes `gts-rust` the sole source of GTS
//! semantics, and compiling the pattern grammar into `LIKE` or a regex would be
//! exactly the local approximation it forbids. So SQL only ever **narrows**:
//! [`EntityRepo::list_page`] applies a prefix range over `gts_id` — exact on all
//! three backends, because the column carries binary collation — and then
//! [`gts::GtsId::matches_pattern`] decides. The range is deliberately wider than
//! the pattern (`prefilter_prefix` in [`entity_repo`]): too tight would silently
//! *drop* real matches.
//!
//! Dependency walks use `ToolKit`'s scoped recursive CTE builder, without raw SQL.

pub mod coordination_state_repo;
pub mod dependency_repo;
pub mod entity_repo;
pub mod instance_repo;
pub mod operation_repo;
pub mod type_schema_repo;
pub mod version_family_repo;

pub use coordination_state_repo::CoordinationStateRepo;
pub use dependency_repo::DependencyRepo;
pub use entity_repo::{EntityPage, EntityRepo, PageRequest};
pub use instance_repo::InstanceRepo;
pub use operation_repo::OperationRepo;
pub use type_schema_repo::TypeSchemaRepo;
pub use version_family_repo::VersionFamilyRepo;

/// `ON CONFLICT DO NOTHING`, spelled portably, for the two inserts that race.
///
/// # Why not catch the unique violation and re-read
///
/// A raised unique violation **aborts the transaction** on `PostgreSQL`, so the
/// recovering re-read fails for a second, unrelated reason. Both racing inserts run
/// inside the admission commit transaction ([`crate::domain::admission::unit`]);
/// `SQLite` and `MySQL` tolerate the recovery, which is why a pooled-connection test
/// passes while the production path does not. An insert whose conflict writes
/// nothing *succeeded* on every backend, and the caller decides what the absence
/// means.
///
/// # Why the column argument
///
/// Untargeted on `PostgreSQL` and `SQLite` — plain `ON CONFLICT DO NOTHING` covers
/// every unique key on the table. `MySQL` has no `DO NOTHING`; `sea-query` polyfills
/// it with an `ON DUPLICATE KEY UPDATE` that assigns a column to itself, which needs
/// a column whose self-assignment changes nothing: the primary key.
fn conflict_do_nothing<C>(pk: C) -> sea_orm::sea_query::OnConflict
where
    C: sea_orm::sea_query::IntoIden,
{
    sea_orm::sea_query::OnConflict::new()
        .do_nothing_on([pk])
        .to_owned()
}

/// Chunk size for `IN (…)` lists.
///
/// `SQLite`'s default `SQLITE_MAX_VARIABLE_NUMBER` is 999 on older builds, and the
/// statement carries the scope predicate's parameters too. 200 is inside every
/// backend's limit and large enough that a realistic closure needs a handful of
/// round trips, not hundreds.
const IN_CHUNK: usize = 200;
