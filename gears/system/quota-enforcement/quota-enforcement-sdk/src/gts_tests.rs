use gts::GtsSchema;

use super::{
    LEASE_RESOURCE, OPERATION_RESOURCE, POLICY_RESOURCE, QUOTA_RESOURCE,
    QuotaEnforcementStoragePluginSpecV1,
};

#[test]
fn the_storage_plugin_spec_type_id_derives_from_the_toolkit_plugin_base() {
    let storage = QuotaEnforcementStoragePluginSpecV1::TYPE_ID;
    assert!(
        storage.starts_with("gts.cf.toolkit.plugins.plugin.v1~"),
        "{storage}"
    );
    assert!(storage.ends_with('~'));
}

#[test]
fn resource_ids_are_distinct_five_segment_type_ids() {
    let all = [
        QUOTA_RESOURCE,
        POLICY_RESOURCE,
        LEASE_RESOURCE,
        OPERATION_RESOURCE,
    ];
    for id in all {
        assert!(id.starts_with("gts.cf.qe.resource."), "{id}");
        assert!(id.ends_with(".v1~"), "{id}");
    }
    let mut sorted = all.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "resource ids must be unique");
}
