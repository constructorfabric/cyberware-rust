//! GTS type definitions owned by quota-enforcement.
//!
//! Two concerns live here:
//!
//! 1. The **plugin specs**. Plugin gears register an instance of one spec
//!    (vendor + priority). The gear discovers the active plugin through the
//!    types registry and resolves its scoped `ClientHub` client under the
//!    instance id.
//! 2. The **resource identifiers** the canonical error envelope carries in
//!    `Problem.context.resource_type` (DESIGN section 3.3, "Error Model").

use toolkit_gts::{PluginV1, gts_id, gts_type_schema};

/// GTS resource type for Quota records (declarative caps).
pub const QUOTA_RESOURCE: &str = gts_id!("cf.qe.resource.quota.v1~");

/// GTS resource type for Quota Resolution Policy records and their versions.
pub const POLICY_RESOURCE: &str = gts_id!("cf.qe.resource.policy.v1~");

/// GTS resource type for two-phase capacity leases.
pub const LEASE_RESOURCE: &str = gts_id!("cf.qe.resource.lease.v1~");

/// GTS resource type for operation-log records.
pub const OPERATION_RESOURCE: &str = gts_id!("cf.qe.resource.operation.v1~");

/// GTS plugin specification for quota-enforcement storage backends.
///
/// Instance id shape:
/// `gts.cf.toolkit.plugins.plugin.v1~cf.core.qe.storage_plugin.v1~<vendor>.<pkg>.<ns>.<name>.v1`
// @cpt-dod:cpt-cf-quota-enforcement-dod-sdk-contracts:p1
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.qe.storage_plugin.v1~"),
    description = "Quota Enforcement storage plugin specification",
    properties = "",
)]
pub struct QuotaEnforcementStoragePluginSpecV1;

// Singleton coordination has no plugin spec: the gear consumes the platform
// `cluster` gear's leader election, and the operator selects its backend in the
// cluster profile YAML (ADR-0006).

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gts_tests.rs"]
mod gts_tests;
