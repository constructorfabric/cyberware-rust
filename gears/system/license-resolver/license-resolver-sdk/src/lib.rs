//! License Resolver SDK
//!
//! This crate provides the public, transport-agnostic contract for the
//! `license_resolver` gear — a read-only, plugin-delegating resolver that
//! answers exactly one question: *is this resource licensed to this subject
//! right now?*
//!
//! - [`LicenseResolverClient`] — public API trait for consumers (the single
//!   `is_licensed` check).
//! - [`LicenseResolverPluginClient`] — plugin API trait for backend
//!   implementations (mirrors the public signature).
//! - [`LicenseCheckRequest`], [`LicenseDecision`] — the check input/output.
//! - [`Subject`], [`Resource`], [`LicenseCheckContext`] — the contract objects
//!   bundled into a request.
//! - [`LicenseResolverError`] — the typed error enum, plus its co-located
//!   violation vocabulary ([`field`]).
//! - [`LicenseResolverPluginSpecV1`] — GTS plugin spec used for discovery.
//! - [`gts`] — the licensing base types (`gts.cf.core.lic.subj.v1~` /
//!   `…res.v1~`) that consuming Gears derive their contract types from.
//!
//! There is intentionally **no** listing or enumeration method: enumerating a
//! platform's licensing surface is served natively by the types registry
//! (every licensing contract derives from the base types), not by this API.
//!
//! ## Usage
//!
//! ```ignore
//! use license_resolver_sdk::LicenseResolverClient;
//!
//! let resolver = hub.get::<dyn LicenseResolverClient>()?;
//! let decision = resolver.is_licensed(request).await?;
//! if decision.granted { /* allow */ } else { /* deny */ }
//! ```
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod api;
pub mod error;
pub mod field;
pub mod gts;
pub mod models;
pub mod plugin_api;

pub use api::LicenseResolverClient;
pub use error::{FieldViolation, LicenseResolverError};
pub use gts::{LicenseResolverPluginSpecV1, LicenseResourceV1, LicenseSubjectV1};
pub use models::{
    LicenseCheckContext, LicenseCheckContextBuildError, LicenseCheckContextBuilder,
    LicenseCheckRequest, LicenseDecision, Resource, Subject,
};
pub use plugin_api::LicenseResolverPluginClient;
