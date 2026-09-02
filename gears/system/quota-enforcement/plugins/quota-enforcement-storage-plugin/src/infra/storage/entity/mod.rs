//! `SeaORM` entities of the foundation tables.
//!
//! All four tables hold operator configuration or schema metadata, not tenant
//! data. They are declared `no_tenant, no_resource, no_owner, no_type` and the
//! plugin reads them under `AccessScope::allow_all()`. Tenant data tables
//! (quotas, counters, leases, records) land with their features and carry a
//! `tenant_col`.

pub mod contention_timeout_config;
pub mod idempotency_retention_config;
pub mod lease_capacity_config;
pub mod schema_meta;

/// Sentinel key of the platform-default row in the configuration tables.
pub const DEFAULT_KEY: &str = "*";
