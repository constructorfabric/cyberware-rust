#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! Verifies that `Gear::init()` fails fast with a descriptive error when
//! any one of the four required `ClientHub` dependencies is absent.

mod common;

use authz_resolver_plugin::AuthZResolverPluginGear;
use toolkit::Gear;

async fn assert_init_fails_mentioning(missing: common::MissingDependency, needle: &str) {
    let (ctx, _hub, _registry, _rbac, _tr, _rg) = common::build_ctx(missing);
    let err = AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect_err("init must fail when a required dependency is missing");
    let msg = format!("{err:#}");
    assert!(
        msg.contains(needle),
        "error message must name `{needle}`; got: {msg}"
    );
}

#[tokio::test]
async fn missing_rbac_service_client() {
    assert_init_fails_mentioning(common::MissingDependency::Rbac, "RbacServiceClientV1").await;
}

#[tokio::test]
async fn missing_tenant_resolver_client() {
    assert_init_fails_mentioning(
        common::MissingDependency::TenantResolver,
        "TenantResolverClient",
    )
    .await;
}

#[tokio::test]
async fn missing_resource_group_client() {
    assert_init_fails_mentioning(
        common::MissingDependency::ResourceGroup,
        "ResourceGroupReadHierarchy",
    )
    .await;
}

#[tokio::test]
async fn missing_types_registry_client() {
    assert_init_fails_mentioning(
        common::MissingDependency::TypesRegistry,
        "TypesRegistryClient",
    )
    .await;
}
