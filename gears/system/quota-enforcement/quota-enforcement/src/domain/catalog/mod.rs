//! The projection contract catalogue (`features/projection-contracts.md`,
//! ADR-0007): the immutable, process-local snapshot of the configured owner
//! projections and per-metric contracts that the Gateway validates against
//! without a registry call.
//!
//! - [`model`]: the catalogue and the compiled contracts it holds.
//! - [`builder`]: the bootstrap consistency set that produces it, and the
//!   compatibility check against active Quotas.
//! - [`membership`]: the reference check Quota and Policy writes run.

pub mod builder;
pub mod membership;
pub mod model;

pub use builder::{CatalogBuilder, CatalogConfig};
pub use membership::check_projection_reference;
pub use model::{
    CatalogMiss, CompiledContract, ConstraintContract, MetricRequestContract,
    ProjectionContractCatalog, ResourceProjectionContract, SubjectProjectionContract,
    parse_metric_under_base,
};
