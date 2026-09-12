//! `SeaORM` entities of the plugin's tables.
//!
//! The four foundation tables hold operator configuration or schema metadata,
//! not tenant data. They are declared `no_tenant, no_resource, no_owner,
//! no_type` and the plugin reads them under `AccessScope::allow_all()`. The
//! Quota tables are tenant data: `qe_quotas` maps `tenant_id` and its `id`,
//! its two siblings map `tenant_id` and `quota_id`, so `SecureORM` applies the
//! PDP scope to every row of every table.

pub mod contention_timeout_config;
pub mod idempotency_retention_config;
pub mod lease_capacity_config;
pub mod operation_log;
pub mod quota;
pub mod quota_allocation_counter;
pub mod schema_meta;

/// Sentinel key of the platform-default row in the configuration tables.
pub const DEFAULT_KEY: &str = "*";
