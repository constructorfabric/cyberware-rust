//! Domain models — pure value / data types with no behavior.

pub(crate) mod actions;
pub(crate) mod resource_types;
pub mod role_assignment;
pub mod role_definition;
#[cfg(test)]
pub(crate) mod scope_fakes;

pub use role_assignment::RoleAssignmentModel;
pub use role_definition::RoleDefinitionModel;
