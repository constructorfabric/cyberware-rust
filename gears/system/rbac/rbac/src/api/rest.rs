//! REST API layer for the RBAC module.
//!
//! Hosts the role-definition, role-assignment, and permissions handlers,
//! plus the shared error mapping that converts `RbacServiceError` into
//! RFC-9457 `Problem` responses.

pub(crate) mod auth_context;
pub(crate) mod canonical_json;
pub(crate) mod canonical_path;
pub mod error;
// REST DTOs with `utoipa::ToSchema` derives; mirrors `rbac_sdk::models::*`
// so the SDK crate stays infrastructure-free.
pub(crate) mod dto;
// RFC 7232 `If-Match` header parsing for optimistic-concurrency endpoints.
pub(crate) mod if_match;
pub mod permissions;
pub mod role_assignments;
pub mod role_definitions;
