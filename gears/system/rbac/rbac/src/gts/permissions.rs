//! RBAC authorization permissions catalog.
//!
//! Declares every RBAC-grantable permission as an [`AuthzPermissionV1`] GTS
//! instance via [`gts_instance!`]. Each invocation submits an
//! [`InventoryInstance`] entry; `types-registry::init()` aggregates and
//! validates them at startup.
//!
//! `action` values come from `crate::domain::actions` — the same constants
//! the REST handlers' `PolicyEnforcer::enforce(...)` calls pass through.
//! `resource_type` values are concrete GTS type IDs (not wildcards) since
//! RBAC's resources are platform-level and not subject to derivation in v1.
//!
//! Instance ID layout (`vendor.package.namespace.type.v1` — exactly four
//! name tokens before the version per the GTS parser):
//!
//! ```text
//! gts.cf.toolkit.authz.permission.v1~cf.core.rbac.<permission_name>.v1
//! ```
//!
//! [`AuthzPermissionV1`]: toolkit_gts::AuthzPermissionV1
//! [`InventoryInstance`]: toolkit_gts::InventoryInstance
//! [`gts_instance!`]: toolkit_gts::gts_instance

use crate::domain::actions;
use crate::domain::resource_types::{
    ROLE_ASSIGNMENT as ROLE_ASSIGNMENT_RESOURCE_TYPE,
    ROLE_DEFINITION as ROLE_DEFINITION_RESOURCE_TYPE,
};
use toolkit_gts::{AuthzPermissionV1, gts_instance};

// role-definition resource permissions

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_definition_read.v1"),
        resource_type: ROLE_DEFINITION_RESOURCE_TYPE.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read role definition".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_definition_write.v1"),
        resource_type: ROLE_DEFINITION_RESOURCE_TYPE.to_owned(),
        action: actions::WRITE.to_owned(),
        display_name: "Write role definition".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_definition_delete.v1"),
        resource_type: ROLE_DEFINITION_RESOURCE_TYPE.to_owned(),
        action: actions::DELETE.to_owned(),
        display_name: "Delete role definition".to_owned(),
    }
}

// role-assignment resource permissions

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_assignment_read.v1"),
        resource_type: ROLE_ASSIGNMENT_RESOURCE_TYPE.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read role assignment".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_assignment_write.v1"),
        resource_type: ROLE_ASSIGNMENT_RESOURCE_TYPE.to_owned(),
        action: actions::WRITE.to_owned(),
        display_name: "Write role assignment".to_owned(),
    }
}

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_assignment_delete.v1"),
        resource_type: ROLE_ASSIGNMENT_RESOURCE_TYPE.to_owned(),
        action: actions::DELETE.to_owned(),
        display_name: "Delete role assignment".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use toolkit_gts::{InventoryInstance, gts_id};

    const PERMISSION_TYPE_ID: &str = gts_id!("cf.toolkit.authz.permission.v1~");
    /// RBAC's instance-id namespace segment, appended after
    /// [`PERMISSION_TYPE_ID`]. Kept as a bare fragment (not a `gts.`-prefixed
    /// literal) so it is composed with the valid type id at the filter site
    /// rather than spelled as a malformed standalone GTS string.
    const RBAC_INSTANCE_NS: &str = "cf.core.rbac.";

    /// Expected RBAC permission instance ids — one per `(action,
    /// resource_type)` pair enforced by the REST surface.
    const EXPECTED_PERMISSION_IDS: &[&str] = &[
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_definition_read.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_definition_write.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_definition_delete.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_assignment_read.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_assignment_write.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.rbac.role_assignment_delete.v1"),
    ];

    fn rbac_permission_instances() -> Vec<&'static InventoryInstance> {
        inventory::iter::<InventoryInstance>
            .into_iter()
            .filter(|e| {
                e.instance_id
                    .strip_prefix(PERMISSION_TYPE_ID)
                    .is_some_and(|seg| seg.starts_with(RBAC_INSTANCE_NS))
            })
            .collect()
    }

    /// All six RBAC permission instances are emitted into the global
    /// inventory and surface under the expected type id.
    #[test]
    fn all_rbac_permissions_are_registered_in_inventory() {
        let entries = rbac_permission_instances();
        assert_eq!(
            entries.len(),
            6,
            "expected 6 RBAC permission instances; found {}: {:?}",
            entries.len(),
            entries.iter().map(|e| e.instance_id).collect::<Vec<_>>()
        );
        for entry in &entries {
            assert_eq!(
                entry.type_id, PERMISSION_TYPE_ID,
                "instance {} derived wrong type_id",
                entry.instance_id
            );
        }
    }

    #[test]
    fn rbac_permission_inventory_covers_every_expected_id() {
        let entries = rbac_permission_instances();
        let actual: std::collections::BTreeSet<&str> =
            entries.iter().map(|e| e.instance_id).collect();
        for expected in EXPECTED_PERMISSION_IDS {
            assert!(
                actual.contains(expected),
                "missing expected permission id: {expected}; got {actual:?}"
            );
        }
        assert_eq!(
            actual.len(),
            EXPECTED_PERMISSION_IDS.len(),
            "inventory contains permission ids not in the expected set"
        );
    }
}
