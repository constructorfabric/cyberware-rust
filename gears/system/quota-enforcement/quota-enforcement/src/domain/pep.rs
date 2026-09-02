//! PEP vocabulary: the resource types and actions QE names in PDP requests.
//! Grouped in two inner modules so call sites read `resources::QUOTA` and
//! `actions::CREATE`.
//!
//! `supported_properties` declares which PDP constraint properties the PEP
//! may compile into `AccessScope`. Tenant-scoped resources support the
//! tenant and resource-id properties. Policies are platform-wide operator
//! entities and support no row property (PRD section 5.12).

use authz_resolver_sdk::pep::ResourceType;
use toolkit_security::pep_properties;

/// Resource types.
pub mod resources {
    use super::{ResourceType, pep_properties};

    /// Quota records.
    pub const QUOTA: ResourceType = ResourceType::from_static(
        "quota_enforcement.quota",
        &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
    );
    /// Consumer evaluation operations (debit, credit, rollback, preview, batch).
    pub const OPERATION: ResourceType = ResourceType::from_static(
        "quota_enforcement.operation",
        &[pep_properties::OWNER_TENANT_ID],
    );
    /// Two-phase leases.
    pub const LEASE: ResourceType = ResourceType::from_static(
        "quota_enforcement.lease",
        &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
    );
    /// Snapshot reads.
    pub const SNAPSHOT: ResourceType = ResourceType::from_static(
        "quota_enforcement.snapshot",
        &[pep_properties::OWNER_TENANT_ID],
    );
    /// Quota Resolution Policies (operator scope, no row property).
    pub const POLICY: ResourceType = ResourceType::from_static("quota_enforcement.policy", &[]);
}

/// Actions.
pub mod actions {
    /// Create a record.
    pub const CREATE: &str = "create";
    /// Read one record.
    pub const GET: &str = "get";
    /// List records.
    pub const LIST: &str = "list";
    /// Update a record.
    pub const UPDATE: &str = "update";
    /// Deactivate a Quota.
    pub const DEACTIVATE: &str = "deactivate";
    /// Debit.
    pub const DEBIT: &str = "debit";
    /// Credit.
    pub const CREDIT: &str = "credit";
    /// Rollback.
    pub const ROLLBACK: &str = "rollback";
    /// Read-only preview.
    pub const PREVIEW: &str = "preview";
    /// Batch debit.
    pub const BATCH_DEBIT: &str = "batch_debit";
    /// Lease acquisition.
    pub const RESERVE: &str = "reserve";
    /// Lease commit.
    pub const COMMIT: &str = "commit";
    /// Lease release.
    pub const RELEASE: &str = "release";
    /// Snapshot read.
    pub const READ: &str = "read";
    /// Policy rollback.
    pub const POLICY_ROLLBACK: &str = "rollback";
    /// Policy soft-delete.
    pub const DELETE: &str = "delete";
}
