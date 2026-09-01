//! Role-assignment aggregate.
//!
//! Owns [`service::RoleAssignmentService`] — the canonical orchestration
//! entry point over [`crate::domain::role_assignment_repo::RoleAssignmentRepository`]
//! and the supporting domain ports — plus
//! [`hydration::PrincipalNameHydrator`], the read-path decoration that
//! turns principal and author ids into display names.

pub mod hydration;
pub mod service;

pub use hydration::PrincipalNameHydrator;
pub use service::{
    CreateRoleAssignmentRequest, HydratedRoleAssignment, ListRoleAssignmentsRequest,
    RoleAssignmentService,
};
