//! `SeaORM` entity types for the RBAC module.
//!
//! * `role_definition` — backs the built-in role seeder and the
//!   role-definition repository adapter.
//! * `role_assignment` — backs the platform-admin bootstrap and the
//!   role-assignment repository adapter.

pub mod role_assignment;
pub mod role_definition;
