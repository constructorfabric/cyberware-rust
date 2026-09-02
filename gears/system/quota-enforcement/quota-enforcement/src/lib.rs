//! Quota Enforcement gear: foundation slice.
//!
//! - [`domain::admission`]: shape check, PDP admission, `AccessScope`
//!   pass-through.
//! - [`domain::plugins`]: storage and coordination plugin binding by vendor.
//! - [`domain::bootstrap`]: fail-closed bootstrap with per-dependency
//!   readiness.
//! - [`infra::metrics`]: the PRD 5.16 instruments on the platform meter.
//! - [`api`]: REST mount point and the readiness health check.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::QuotaEnforcementGear;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod test_support;
