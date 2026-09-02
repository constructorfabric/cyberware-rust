//! API surface: the REST mount point and the readiness health check.
//!
//! Operational routes land with their features. Every one of them is
//! registered through [`rest::routes::register_routes`] and admitted through
//! [`crate::domain::Admission`], so REST and the in-process SDK client share
//! one authorization boundary.

pub mod healthcheck;
pub mod rest;
