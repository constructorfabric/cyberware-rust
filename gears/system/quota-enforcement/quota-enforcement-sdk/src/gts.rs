//! GTS definitions owned by quota-enforcement.
//!
//! Three concerns live here:
//!
//! 1. The **plugin specs**. Plugin gears register an instance of one spec
//!    (vendor + priority). The gear discovers the active plugin through the
//!    types registry and resolves its scoped `ClientHub` client under the
//!    instance id.
//! 2. The **resource identifiers** the canonical error envelope carries in
//!    `Problem.context.resource_type` (DESIGN section 3.3, "Error Model").
//! 3. The **projection contract bases** of ADR-0007: four abstract bases, the
//!    concrete scope-discriminator type, and its two P1 well-known instances.
//!    The documents are the crate's `schemas/` copies of the reviewed JSON
//!    Schema files under `docs/schemas`: the copies ship inside the published
//!    crate, and a test holds them byte-identical to the reviewed files, so
//!    the registry receives exactly what was reviewed.
//!    They reach `types-registry` two ways: through the link-time inventory
//!    the registry seeds itself from in the embedded profile, and through the
//!    gear's bootstrap, which re-asserts them idempotently for the deployed
//!    profile ([`owned_definitions`]).

use serde_json::{Value, json};
use toolkit_gts::{InventoryInstance, InventoryTypeSchema, PluginV1, gts_id, gts_type_schema};

/// GTS resource type for Quota records (declarative caps).
pub const QUOTA_RESOURCE: &str = gts_id!("cf.qe.resource.quota.v1~");

/// GTS resource type for Quota Resolution Policy records and their versions.
pub const POLICY_RESOURCE: &str = gts_id!("cf.qe.resource.policy.v1~");

/// GTS resource type for two-phase capacity leases.
pub const LEASE_RESOURCE: &str = gts_id!("cf.qe.resource.lease.v1~");

/// GTS resource type for operation-log records.
pub const OPERATION_RESOURCE: &str = gts_id!("cf.qe.resource.operation.v1~");

// ---------------------------------------------------------------------------
// Projection contract bases (ADR-0007)
// ---------------------------------------------------------------------------

// The bases an owner derives its contracts from: one subject projection per
// scope (`inst-pub-author`), one request contract per metric with its attached
// constraint contract (`inst-pub-attrs`), and an optional resource projection
// (`inst-pub-res`). The reviewed llm_gateway examples under
// `docs/schemas/examples` are the worked owner side of each.

// @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-author
/// Abstract base of the owner-published subject projections. Its required
/// traits are `scope` and `admitted_metrics`.
pub const SUBJECT_BASE: &str = gts_id!("cf.core.qe.subj.v1~");
// @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-author

// @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-res
/// Abstract base of the owner-published resource projections.
pub const RESOURCE_BASE: &str = gts_id!("cf.core.qe.res.v1~");
// @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-res

// @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-attrs
/// Abstract base of the per-metric request contracts. Its required traits are
/// `metric` and `constraint_contract`.
pub const REQUEST_BASE: &str = gts_id!("cf.core.qe.request.v1~");

/// Abstract base of the operator-authored arbitration constraint contracts.
pub const CONSTRAINT_BASE: &str = gts_id!("cf.core.qe.constraint.v1~");
// @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-attrs

/// The concrete scope-discriminator type. Its well-known instances classify
/// caller-supplied subject references.
pub const SCOPE_TYPE: &str = gts_id!("cf.core.qe.scope.v1~");

/// The `user` scope instance.
pub const SCOPE_USER: &str = gts_id!("cf.core.qe.scope.v1~cf.core.qe.user.v1");

/// The `tenant` scope instance. The gear materializes one subject of this
/// scope from every request's `tenant_id`.
pub const SCOPE_TENANT: &str = gts_id!("cf.core.qe.scope.v1~cf.core.qe.tenant.v1");

/// The platform metric base every admitted metric is an instance of.
///
/// Provisional: PRD section 3.2 names this id pending the platform-wide
/// metric naming decision (PRD section 13). The QE base schemas pin the same
/// id in their `x-gts-ref` narrowing, so the two must change together. Metrics
/// are registry-owned; the gear never registers this base, it only checks
/// admitted metrics against it at bootstrap.
pub const METRIC_BASE_TYPE: &str = gts_id!("cf.qe.metric.type.v1~");

const SUBJECT_BASE_JSON: &str = include_str!("../schemas/gts.cf.core.qe.subj.v1~.schema.json");
const RESOURCE_BASE_JSON: &str = include_str!("../schemas/gts.cf.core.qe.res.v1~.schema.json");
const REQUEST_BASE_JSON: &str = include_str!("../schemas/gts.cf.core.qe.request.v1~.schema.json");
const CONSTRAINT_BASE_JSON: &str =
    include_str!("../schemas/gts.cf.core.qe.constraint.v1~.schema.json");
const SCOPE_TYPE_JSON: &str = include_str!("../schemas/gts.cf.core.qe.scope.v1~.schema.json");

/// One QE-owned GTS definition as the registry receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedDefinition {
    /// The GTS id: a type id (trailing `~`) or an instance id.
    pub id: &'static str,
    /// The document registered under `id`.
    pub document: Value,
}

/// The QE-owned GTS definitions in registration order: the four abstract
/// bases, the scope-discriminator type, then its two well-known instances.
///
/// Registration touches only these definitions. Concrete owner projections
/// are published by their owning gears, and the gear seeds no platform-wide
/// subject instances (ADR-0007).
///
/// # Errors
///
/// Returns the parse error when an embedded schema file is not valid JSON.
/// The files are reviewed documents checked by the crate's tests, so this is
/// a build defect rather than a runtime condition.
// @cpt-dod:cpt-cf-quota-enforcement-dod-base-registration:p1
pub fn owned_definitions() -> Result<Vec<OwnedDefinition>, serde_json::Error> {
    let mut out = Vec::with_capacity(7);
    for (id, raw) in OWNED_TYPE_SCHEMAS {
        out.push(OwnedDefinition {
            id,
            document: serde_json::from_str(raw)?,
        });
    }
    for id in [SCOPE_USER, SCOPE_TENANT] {
        out.push(OwnedDefinition {
            id,
            document: scope_instance(id),
        });
    }
    Ok(out)
}

/// The five QE-owned type schemas, bases before the concrete scope type.
const OWNED_TYPE_SCHEMAS: [(&str, &str); 5] = [
    (SUBJECT_BASE, SUBJECT_BASE_JSON),
    (RESOURCE_BASE, RESOURCE_BASE_JSON),
    (REQUEST_BASE, REQUEST_BASE_JSON),
    (CONSTRAINT_BASE, CONSTRAINT_BASE_JSON),
    (SCOPE_TYPE, SCOPE_TYPE_JSON),
];

/// The identity-only document of a well-known scope instance.
fn scope_instance(id: &str) -> Value {
    json!({ "id": id, "type": SCOPE_TYPE })
}

inventory::submit! {
    InventoryTypeSchema { type_id: SUBJECT_BASE, schema_fn: || SUBJECT_BASE_JSON.to_owned() }
}
inventory::submit! {
    InventoryTypeSchema { type_id: RESOURCE_BASE, schema_fn: || RESOURCE_BASE_JSON.to_owned() }
}
inventory::submit! {
    InventoryTypeSchema { type_id: REQUEST_BASE, schema_fn: || REQUEST_BASE_JSON.to_owned() }
}
inventory::submit! {
    InventoryTypeSchema { type_id: CONSTRAINT_BASE, schema_fn: || CONSTRAINT_BASE_JSON.to_owned() }
}
inventory::submit! {
    InventoryTypeSchema { type_id: SCOPE_TYPE, schema_fn: || SCOPE_TYPE_JSON.to_owned() }
}
inventory::submit! {
    InventoryInstance {
        type_id: SCOPE_TYPE,
        instance_id: SCOPE_USER,
        payload_fn: || scope_instance(SCOPE_USER),
    }
}
inventory::submit! {
    InventoryInstance {
        type_id: SCOPE_TYPE,
        instance_id: SCOPE_TENANT,
        payload_fn: || scope_instance(SCOPE_TENANT),
    }
}

// ---------------------------------------------------------------------------
// Plugin specs
// ---------------------------------------------------------------------------

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
