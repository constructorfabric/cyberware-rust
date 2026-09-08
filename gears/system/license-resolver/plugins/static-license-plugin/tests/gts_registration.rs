//! Reproduces the server's startup GTS commit in-process.
//!
//! At boot types-registry seeds every linked crate's link-time inventory, adds
//! whatever gears register during `init()`, then commits the lot in one
//! all-or-nothing pass. That pass is process-global: a licensing type or a
//! plugin instance that fails it does not degrade licensing, it stops the whole
//! server from binding. This test runs that same sequence over the licensing
//! surface alone, so the failure shows up here rather than as an unexplained
//! boot timeout in every E2E suite.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use gts::GtsSchema;
use license_resolver_sdk::LicenseResolverPluginSpecV1;
use license_resolver_sdk::gts::{LicenseResourceV1, LicenseSubjectV1};
use static_license_plugin::gear::INSTANCE_SUFFIX;
use toolkit::gts::PluginV1;
use toolkit_gts::{all_inventory_instances, all_inventory_type_schemas};
use types_registry::config::TypesRegistryConfig;
use types_registry::domain::service::TypesRegistryService;
use types_registry::infra::InMemoryGtsRepository;
use types_registry_sdk::RegisterResult;

const VENDOR: &str = "constructorfabric";
const PRIORITY: i16 = 100;

fn create_service() -> Arc<TypesRegistryService> {
    let repo = Arc::new(InMemoryGtsRepository::new(
        TypesRegistryConfig::default().to_gts_config(),
    ));
    Arc::new(TypesRegistryService::new(
        repo,
        TypesRegistryConfig::default(),
    ))
}

fn plugin_instance() -> (gts::GtsInstanceId, serde_json::Value) {
    PluginV1::<LicenseResolverPluginSpecV1>::build_registration(
        INSTANCE_SUFFIX,
        VENDOR.to_owned(),
        PRIORITY,
    )
    .expect("plugin registration payload builds")
}

#[test]
fn the_licensing_surface_survives_the_startup_commit() {
    let service = create_service();

    let mut entities = all_inventory_type_schemas().expect("collect inventory type schemas");
    entities.extend(all_inventory_instances().expect("collect inventory instances"));
    let (instance_id, instance_json) = plugin_instance();
    entities.push(instance_json);

    let results = service.register(entities);
    RegisterResult::ensure_all_ok(&results).expect("every entity registers");

    service
        .switch_to_ready()
        .expect("the ready commit must succeed, or the server cannot boot");

    for type_id in [
        <LicenseSubjectV1<()> as GtsSchema>::TYPE_ID,
        <LicenseResourceV1<()> as GtsSchema>::TYPE_ID,
        LicenseResolverPluginSpecV1::TYPE_ID,
    ] {
        assert!(
            service.get(type_id).is_ok(),
            "{type_id} must be registered after the commit"
        );
    }
    assert!(
        service.get(instance_id.as_ref()).is_ok(),
        "the plugin instance {instance_id} must be registered after the commit"
    );
}

/// The instance carries what the gateway selects on: without a matching vendor
/// the gateway finds no backend and every check fails closed.
#[test]
fn the_plugin_instance_advertises_its_vendor_and_priority() {
    let (instance_id, instance_json) = plugin_instance();

    assert!(
        instance_id
            .as_ref()
            .starts_with(LicenseResolverPluginSpecV1::TYPE_ID),
        "the instance must be declared from the license resolver plugin spec: {instance_id}"
    );
    assert_eq!(
        instance_json.get("vendor").and_then(|v| v.as_str()),
        Some(VENDOR)
    );
    assert_eq!(
        instance_json
            .get("priority")
            .and_then(serde_json::Value::as_i64),
        Some(i64::from(PRIORITY))
    );
}

/// The Resource base must reach the registry carrying its trait schema — the
/// gateway reads `admitted_subjects` off the resolved chain, so a base that
/// commits without it would make every check inadmissible.
#[test]
fn the_registered_resource_base_carries_the_admitted_subjects_trait() {
    let service = create_service();
    let entities = all_inventory_type_schemas().expect("collect inventory type schemas");
    RegisterResult::ensure_all_ok(&service.register(entities)).expect("every schema registers");
    service
        .switch_to_ready()
        .expect("the ready commit succeeds");

    let base = service
        .get(<LicenseResourceV1<()> as GtsSchema>::TYPE_ID)
        .expect("the Resource licensing base is registered");
    assert!(
        base.content
            .pointer("/x-gts-traits-schema/properties/admitted_subjects")
            .is_some(),
        "the registered Resource base must declare admitted_subjects: {:#}",
        base.content
    );
}
