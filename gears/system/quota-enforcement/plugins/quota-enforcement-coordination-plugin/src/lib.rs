//! Quota Enforcement coordination plugin.
//!
//! Database-backed default of
//! [`CoordinationPluginV1`](quota_enforcement_sdk::CoordinationPluginV1). One
//! row per [`LockScope`](quota_enforcement_sdk::LockScope) in
//! `qe_coordination_locks`; acquisition and steal run in one serializable
//! transaction; expiry is judged on the database clock.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod gear;
pub mod infra;

pub use gear::CoordinationPlugin;
pub use infra::DbCoordination;
