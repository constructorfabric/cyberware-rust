//! Storage layer: `SeaORM` migrations, entity types, and the repository
//! adapters used by the seeder and the REST handlers.

pub(crate) mod entity;
pub(crate) mod like_escape;
pub mod migrations;
pub(crate) mod odata_mapping;
pub mod repo;

// Re-exported flat at `infra::storage::*`, and `pub` so integration tests can
// construct the repos directly.
pub use repo::{role_assignment_repo, role_definition_repo};
