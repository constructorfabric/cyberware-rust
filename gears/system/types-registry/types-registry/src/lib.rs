//! Types Registry Gear Implementation
//!
//! This gear provides GTS entity registration, storage, validation, and REST API endpoints.
//! The public API is defined in `types-registry-sdk` and re-exported here.
//!
//! ## Architecture
//!
//! - **Two-phase storage**: Configuration phase (no validation) → Ready phase (full validation)
//! - **gts-rust integration**: Uses the official GTS library for all operations
//! - **`ClientHub` registration**: Other gears access via `hub.get::<dyn TypesRegistryClient>()?`

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]
// The admission path nests async futures deeply — handler, service, worker,
// transaction closure, commit — and the default limit of 128 is reached laying out
// `submit_entities`'s state machine. Nothing here recurses at runtime.
#![recursion_limit = "256"]

// === PUBLIC API (from SDK) ===
pub use types_registry_sdk::{
    GtsInstance, GtsInstanceId, GtsTypeId, GtsTypeSchema, InstanceQuery, RegisterResult,
    RegisterSummary, TypeSchemaQuery, TypesRegistryClient, TypesRegistryError,
};

// === MODULE DEFINITION ===
pub mod gear;
pub use gear::TypesRegistryGear;

// === CONFIGURATION ===
pub mod config;
mod policy_config;

// === OBSERVABILITY ===
pub mod observability;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;
