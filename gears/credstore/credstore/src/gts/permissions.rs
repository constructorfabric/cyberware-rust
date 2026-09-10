//! Credstore authorization permissions catalog.
//!
//! Declares every permission credstore can grant as a well-known GTS instance
//! of [`AuthzPermissionV1`] via [`gts_instance!`]. Each invocation submits an
//! [`InventoryInstance`] to the process-wide `toolkit-gts` inventory;
//! `types-registry::init()` aggregates them at boot.
//!
//! `resource_type` for all six is [`credstore_sdk::CREDENTIAL_RESOURCE_TYPE`],
//! the base credential type id (with its trailing `~`) — per
//! `docs/arch/authorization/PERMISSION_GTS_TYPE.md`'s `resource_type`
//! semantics, a concrete type id covers everything derived from it, so one
//! permission per action spans every built-in and customer credential type
//! (`generic`, `api_key`, …, and any custom plugin-defined type). The PEP
//! still evaluates each request against the concrete resolved type
//! ([`crate::domain::authz::credential_type_resource`]), so type-scoped roles
//! remain possible; this catalog only names the actions a role can be granted.
//!
//! `action` values come from `crate::domain::authz::actions` — the same
//! constants [`crate::domain::secret::service::Service`] passes to the PEP —
//! so the catalog cannot drift from what the REST surface actually enforces.
//!
//! No permission is declared for `run_gc`: that is an in-process call made
//! through the SDK trait from a system security context, not an operation
//! roles are granted.
//!
//! Instance id layout:
//! `gts.cf.toolkit.authz.permission.v1~cf.core.credstore.<name>.v1`.
//!
//! [`AuthzPermissionV1`]: toolkit_gts::AuthzPermissionV1
//! [`InventoryInstance`]: toolkit_gts::InventoryInstance
//! [`gts_instance!`]: toolkit_gts::gts_instance

use credstore_sdk::CREDENTIAL_RESOURCE_TYPE;
use toolkit_gts::{AuthzPermissionV1, gts_instance};

use crate::domain::authz::actions;

// ---- credential record (gts.cf.core.credstore.credential.v1~) -------------

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_list.v1"),
        resource_type: CREDENTIAL_RESOURCE_TYPE.to_owned(),
        action: actions::LIST.to_owned(),
        display_name: "List credential records".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_read.v1"),
        resource_type: CREDENTIAL_RESOURCE_TYPE.to_owned(),
        action: actions::READ.to_owned(),
        display_name: "Read credential record".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_write.v1"),
        resource_type: CREDENTIAL_RESOURCE_TYPE.to_owned(),
        action: actions::WRITE.to_owned(),
        display_name: "Write credential record".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_delete.v1"),
        resource_type: CREDENTIAL_RESOURCE_TYPE.to_owned(),
        action: actions::DELETE.to_owned(),
        display_name: "Delete credential".to_owned(),
    }
}

// ---- secret value (same resource type; value-scoped actions) --------------

gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.secret_read.v1"),
        resource_type: CREDENTIAL_RESOURCE_TYPE.to_owned(),
        action: actions::READ_SECRET.to_owned(),
        display_name: "Read secret value".to_owned(),
    }
}
gts_instance! {
    AuthzPermissionV1 {
        id: gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.secret_write.v1"),
        resource_type: CREDENTIAL_RESOURCE_TYPE.to_owned(),
        action: actions::WRITE_SECRET.to_owned(),
        display_name: "Write secret value".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use toolkit_gts::{GtsId, InventoryInstance, gts_id};

    use super::*;

    const PERMISSION_TYPE_ID: &str = gts_id!("cf.toolkit.authz.permission.v1~");
    /// Credstore instance-segment coordinates (`cf.core.credstore`) — the
    /// vendor / package / namespace every credstore permission instance's
    /// concrete segment carries. Matched structurally against the parsed GTS
    /// segment (not by raw-string prefix), so a lookalike namespace cannot
    /// slip through.
    const CREDSTORE_VENDOR: &str = "cf";
    const CREDSTORE_PACKAGE: &str = "core";
    const CREDSTORE_NAMESPACE: &str = "credstore";

    /// One per `(resource_type, action)` the credstore REST/PEP surface
    /// enforces (ADR-0004's six actions; `list` is Phase 3 but its
    /// permission is declared now per the addendum so the vocabulary is
    /// complete and stable).
    const EXPECTED_PERMISSION_IDS: &[&str] = &[
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_list.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_read.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_write.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_delete.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.secret_read.v1"),
        gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.secret_write.v1"),
    ];

    fn credstore_permission_instances() -> Vec<&'static InventoryInstance> {
        inventory::iter::<InventoryInstance>
            .into_iter()
            .filter(|e| {
                // Parse the instance id through the GTS grammar rather than
                // slicing the raw string: select concrete permission
                // instances (type id == `PERMISSION_TYPE_ID`) whose
                // derivation segment sits in the credstore namespace.
                let Ok(parsed) = GtsId::try_new(e.instance_id) else {
                    return false;
                };
                parsed.get_type_id().as_deref() == Some(PERMISSION_TYPE_ID)
                    && parsed.segments().last().is_some_and(|seg| {
                        seg.vendor() == CREDSTORE_VENDOR
                            && seg.package() == CREDSTORE_PACKAGE
                            && seg.namespace() == CREDSTORE_NAMESPACE
                    })
            })
            .collect()
    }

    #[test]
    fn all_credstore_permissions_registered_in_inventory() {
        let entries = credstore_permission_instances();
        assert_eq!(
            entries.len(),
            EXPECTED_PERMISSION_IDS.len(),
            "expected {} credstore permission instances; found {}: {:?}",
            EXPECTED_PERMISSION_IDS.len(),
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
    fn credstore_permission_inventory_covers_every_expected_id() {
        let actual: std::collections::BTreeSet<&str> = credstore_permission_instances()
            .iter()
            .map(|e| e.instance_id)
            .collect();
        for expected in EXPECTED_PERMISSION_IDS {
            assert!(
                actual.contains(expected),
                "missing permission id: {expected}"
            );
        }
        assert_eq!(actual.len(), EXPECTED_PERMISSION_IDS.len());
    }

    /// Every declared instance carries the action and resource type the
    /// addendum specifies, keyed by its own instance id so a copy/paste
    /// mistake between two entries is caught even when the total count is
    /// still six.
    #[test]
    fn each_permission_carries_its_expected_action_and_resource_type() {
        let expected: &[(&str, &str)] = &[
            (
                gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_list.v1"),
                actions::LIST,
            ),
            (
                gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_read.v1"),
                actions::READ,
            ),
            (
                gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_write.v1"),
                actions::WRITE,
            ),
            (
                gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.credential_delete.v1"),
                actions::DELETE,
            ),
            (
                gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.secret_read.v1"),
                actions::READ_SECRET,
            ),
            (
                gts_id!("cf.toolkit.authz.permission.v1~cf.core.credstore.secret_write.v1"),
                actions::WRITE_SECRET,
            ),
        ];
        let entries = credstore_permission_instances();
        for (id, action) in expected {
            let entry = entries
                .iter()
                .find(|e| e.instance_id == *id)
                .unwrap_or_else(|| panic!("missing permission id: {id}"));
            let payload = (entry.payload_fn)();
            assert_eq!(
                payload["action"], *action,
                "instance {id} carries the wrong action"
            );
            assert_eq!(
                payload["resource_type"], CREDENTIAL_RESOURCE_TYPE,
                "instance {id} carries the wrong resource_type"
            );
        }
    }
}
