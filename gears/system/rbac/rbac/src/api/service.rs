//! API-side adapters between the domain services
//! ([`crate::domain::role_definition::RoleDefinitionService`],
//! [`crate::domain::role_assignment::RoleAssignmentService`]) and external
//! surfaces:
//!
//! - [`error_mapping`] — the `DomainError → RbacServiceError` lift applied at
//!   the SDK boundary, which is RBAC's public wire contract.
//! - [`lowering`] — domain `Model → SDK type` projections, called from REST
//!   handlers rather than from the domain services.
//! - [`local_client`] — in-process `dyn RbacServiceClientV1` implementation
//!   published in `ClientHub`. Delegates to the permission evaluator.

mod error_mapping;
/// In-process adapter that publishes `dyn RbacServiceClientV1` in
/// `ClientHub`. `#[doc(hidden)]` keeps it out of rustdoc.
#[doc(hidden)]
pub mod local_client;
pub(crate) mod lowering;
