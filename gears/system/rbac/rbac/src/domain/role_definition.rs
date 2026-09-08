//! Role-definition aggregate.
//!
//! Owns [`service::RoleDefinitionService`] — the canonical orchestration
//! entry point over [`crate::domain::role_definition_repo::RoleDefinitionRepository`]
//! and the supporting domain ports.

pub mod service;

pub use service::{
    CallerScope, CountedRoleDefinition, CreateRoleDefinitionRequest, ListRoleDefinitionsRequest,
    RoleDefinitionService, UpdateRoleDefinitionRequest,
};
