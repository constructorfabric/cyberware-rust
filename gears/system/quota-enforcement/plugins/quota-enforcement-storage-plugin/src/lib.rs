//! Quota Enforcement storage plugin: foundation slice.
//!
//! Schema-version check, the three configuration tables with idempotent
//! seeding, and the migrations that create them. See the crate README for why
//! no `QuotaEnforcementStoragePluginV1` client is published yet.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use domain::{FoundationStore, SeedReport, StoragePlugin, StoreError};
pub use gear::StoragePluginGear;
pub use infra::storage::SqlFoundationStore;
