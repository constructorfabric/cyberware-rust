//! License Resolver Gear
//!
//! Realizes the single `is_licensed` check of
//! [`LicenseResolverClient`](license_resolver_sdk::LicenseResolverClient): it
//! validates the request against the licensing contracts it declares, discovers
//! a backend plugin via types-registry (vendor + priority), and delegates.
//!
//! The gear owns no grant store and makes no licensing decision — it validates
//! *shape and compatibility* and routes. Every cannot-determine condition fails
//! closed: a non-conforming request, a missing plugin, and an unreachable
//! backend all yield an error, never a granted decision.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod test_contracts;
