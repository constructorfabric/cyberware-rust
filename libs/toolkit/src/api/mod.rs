//! Type-safe API operation builder with compile-time guarantees
//!
//! This gear provides a type-state builder pattern that enforces at compile time
//! that API operations cannot be registered unless both a handler and at least one
//! response are specified.

pub mod api_dto;
pub mod canonical_error_layer;
pub mod error_layer;
pub mod odata;
pub mod openapi_registry;
pub mod operation_builder;
pub mod response;
pub mod rest;
pub mod select;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod odata_policy_tests;

pub use canonical_error_layer::canonical_error_middleware;
pub use error_layer::{
    IntoCanonical, error_mapping_middleware, extract_trace_id, map_error_to_canonical,
};
pub use openapi_registry::{OpenApiInfo, OpenApiRegistry, OpenApiRegistryImpl, ensure_schema};
pub use operation_builder::{
    Missing, OperationBuilder, OperationSpec, ParamLocation, ParamSpec, Present,
    ResponseHeaderSpec, ResponseHeaderType, ResponseSpec, ThrottlingSpec, state,
};
pub use select::{apply_select, page_to_projected_json, project_json};

/// Prelude that re-exports the canonical error types and common API utilities.
pub mod canonical_prelude {
    // Canonical error types
    pub use toolkit_canonical_errors::{CanonicalError, Problem, resource_error};

    /// Result type alias for handlers using the canonical error catalog.
    ///
    /// Returns [`CanonicalError`] (not [`Problem`]) so handler `?` chains
    /// resolve through `From<DomainError> for CanonicalError` — the
    /// long-lived per-gear mapping. The canonical error middleware
    /// (`toolkit::api::canonical_error_middleware`) converts the
    /// `CanonicalError` to a wire `Problem` and fills `instance` /
    /// `trace_id` on the way out, so handlers never need to construct a
    /// `Problem` themselves.
    pub type ApiResult<T = ()> = std::result::Result<T, CanonicalError>;

    // Same response sugar / OData / axum re-exports as the legacy prelude
    pub use super::odata::OData;
    pub use super::response::{JsonBody, JsonPage, created_json, no_content, ok_json};
    pub use super::rest::extract;
    pub use super::select::apply_select;
    pub use axum::{Json, http::StatusCode, response::IntoResponse};
}
