//! Infrastructure layer: storage, migrations, ClientHub adapters, the
//! built-in role seeder, and the platform-admin bootstrap.
//!
//! Anything that talks SeaORM, toolkit-db, an upstream ClientHub trait, or
//! the network/HTTP transport layer lives here — not in `domain/`. The
//! `domain/` layer hosts pure types and pure functions; this module is
//! the bridge to the outside world.

/// Display-name reads for `User` principals, over the account-management
/// SDK. Not a gear `deps` edge — see the module docs.
pub(crate) mod am_user_name_reader;
pub mod bootstrap;
pub(crate) mod canonical_mapping;
pub(crate) mod error_conv;
pub mod metrics;
pub(crate) mod odata_err;
/// Coerces caller-supplied `$filter` literals into the shape each field
/// declares, so an id filter does not depend on the column's SQL type.
pub(crate) mod odata_normalize;
pub(crate) mod rg_adapter;
pub mod seeder;
pub mod storage;
pub(crate) mod types_registry_permission_catalog;
pub(crate) mod types_registry_target_type_validator;

// Test-only re-export. Lets integration tests under `tests/postgres_*.rs`
// pass a real PG-emitted `DbErr` through the production classifier and
// assert the resulting `DomainError`. Gated behind `test-support` so the
// symbol cannot leak into a release artifact.
#[cfg(any(test, feature = "test-support"))]
pub use canonical_mapping::classify_db_err_to_domain;
