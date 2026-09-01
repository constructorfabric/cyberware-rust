#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::needless_pass_by_value,
    dead_code
)]
//! Shared test fixtures for foundation integration tests.

use std::collections::HashMap;
use std::sync::Arc;

pub use authz_resolver_plugin::test_support::{
    InMemoryRbacServiceClient, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
    RecordingTypesRegistry,
};
use authz_resolver_sdk::plugin_api::AuthZResolverPluginClient;
use rbac_sdk::RbacServiceClientV1;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
use serde_json::{Value, json};
use tenant_resolver_sdk::api::TenantResolverClient;
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit::config::ConfigProvider;
use toolkit::context::GearCtx;
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

pub const MODULE_NAME: &str = "authz-resolver-plugin";

/// Minimal `ConfigProvider` returning a single in-memory module config.
pub struct MapConfigProvider {
    modules: HashMap<String, Value>,
}

impl MapConfigProvider {
    pub fn empty() -> Self {
        Self {
            modules: HashMap::new(),
        }
    }

    pub fn with_module(mut self, name: &str, config: Value) -> Self {
        self.modules
            .insert(name.to_owned(), json!({ "config": config }));
        self
    }
}

impl ConfigProvider for MapConfigProvider {
    fn get_gear_config(&self, gear_name: &str) -> Option<&Value> {
        self.modules.get(gear_name)
    }
}

/// Which downstream client to leave out of the `ClientHub`. `None` means
/// register all four — the happy path. Each integration test binary uses
/// only a subset of the variants, hence the `dead_code` allowance.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum MissingDependency {
    None,
    Rbac,
    TenantResolver,
    ResourceGroup,
    TypesRegistry,
}

type CtxTuple = (
    GearCtx,
    Arc<ClientHub>,
    Arc<RecordingTypesRegistry>,
    Arc<InMemoryRbacServiceClient>,
    Arc<InMemoryTenantResolverClient>,
    Arc<InMemoryResourceGroupClient>,
);

type ExternalRbacCtxTuple = (
    GearCtx,
    Arc<ClientHub>,
    Arc<RecordingTypesRegistry>,
    Arc<InMemoryTenantResolverClient>,
    Arc<InMemoryResourceGroupClient>,
);

/// Build a fully-populated `GearCtx` using default fakes for every
/// downstream client. Tests that don't reach the post-validation pipeline
/// (or only reach the scope step) use this shortcut.
pub fn build_ctx(missing: MissingDependency) -> CtxTuple {
    build_ctx_with(
        missing,
        Arc::new(InMemoryRbacServiceClient::default()),
        Arc::new(InMemoryTenantResolverClient::default()),
        Arc::new(InMemoryResourceGroupClient::default()),
    )
}

/// Build a fully-populated `GearCtx` and inject caller-supplied fakes
/// for every downstream client. Tests that script specific behaviours
/// (RBAC Allowed/Denied/Err, tenant subtree, RG memberships) use this.
pub fn build_ctx_with(
    missing: MissingDependency,
    rbac: Arc<InMemoryRbacServiceClient>,
    tenant_resolver: Arc<InMemoryTenantResolverClient>,
    resource_group: Arc<InMemoryResourceGroupClient>,
) -> CtxTuple {
    build_ctx_with_overrides(
        missing,
        rbac,
        tenant_resolver,
        resource_group,
        None,
        None,
        None,
    )
}

/// Plugin-config overrides for [`build_ctx_with_config`]. `Default` leaves
/// every field at the plugin's own config default; tests set only the knobs
/// they exercise.
#[allow(dead_code)]
#[derive(Default)]
pub struct CtxOverrides {
    /// Pin `capability_degradation.max_expansion_ids` (e.g.
    /// `tests/group_combined_constraints.rs`, to trip the threshold
    /// without `10_001` IDs).
    pub max_expansion_ids: Option<usize>,
    /// Pin `gts_validation.mode` (`tests/gts_type_validation.rs` Strict/Off).
    pub gts_validation_mode: Option<String>,
    /// Pin `audit.enabled` (`tests/orchestration_audit.rs`). `None` leaves it
    /// at the plugin default, which is ON — so a test that wants it OFF has to
    /// say `Some(false)` rather than rely on a falsy default.
    pub audit_enabled: Option<bool>,
}

/// Build a `GearCtx` with caller-supplied fakes and selective plugin-config
/// overrides. All dependencies registered (no `MissingDependency`).
#[allow(dead_code)]
pub fn build_ctx_with_config(
    rbac: Arc<InMemoryRbacServiceClient>,
    tenant_resolver: Arc<InMemoryTenantResolverClient>,
    resource_group: Arc<InMemoryResourceGroupClient>,
    overrides: CtxOverrides,
) -> CtxTuple {
    build_ctx_with_overrides(
        MissingDependency::None,
        rbac,
        tenant_resolver,
        resource_group,
        overrides.max_expansion_ids,
        overrides.gts_validation_mode,
        overrides.audit_enabled,
    )
}

/// Build the plugin context around an arbitrary RBAC SDK client rather than
/// the scripted RBAC fake.
///
/// This is the vertical-test seam used to connect RBAC's real local client to
/// the plugin through `ClientHub`. Returning only the remaining resolver fakes
/// prevents callers from accidentally asserting against a second scripted RBAC
/// response instead of the production evaluator path.
pub fn build_ctx_with_rbac_client(
    rbac: Arc<dyn RbacServiceClientV1>,
    tenant_resolver: Arc<InMemoryTenantResolverClient>,
    resource_group: Arc<InMemoryResourceGroupClient>,
) -> ExternalRbacCtxTuple {
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn RbacServiceClientV1>(rbac);

    let tr_dyn: Arc<dyn TenantResolverClient> = Arc::clone(&tenant_resolver) as _;
    hub.register::<dyn TenantResolverClient>(tr_dyn);
    let rg_dyn: Arc<dyn ResourceGroupReadHierarchy> = Arc::clone(&resource_group) as _;
    hub.register::<dyn ResourceGroupReadHierarchy>(rg_dyn);

    let registry = Arc::new(RecordingTypesRegistry::new());
    let registry_dyn: Arc<dyn TypesRegistryClient> = Arc::clone(&registry) as _;
    hub.register::<dyn TypesRegistryClient>(registry_dyn);
    // See `build_ctx_with_overrides`: the default mode is `strict`, so the
    // fixture type ids have to be known for the vertical path to be reachable.
    register_default_types(&registry);

    let config = json!({
        "vendor": "cf",
        "priority": 100,
    });
    let provider = Arc::new(MapConfigProvider::empty().with_module(MODULE_NAME, config));
    let ctx = GearCtx::new(
        MODULE_NAME,
        Uuid::new_v4(),
        provider,
        Arc::clone(&hub),
        CancellationToken::new(),
    );

    (ctx, hub, registry, tenant_resolver, resource_group)
}

fn build_ctx_with_overrides(
    missing: MissingDependency,
    rbac: Arc<InMemoryRbacServiceClient>,
    tenant_resolver: Arc<InMemoryTenantResolverClient>,
    resource_group: Arc<InMemoryResourceGroupClient>,
    max_expansion_ids: Option<usize>,
    gts_validation_mode: Option<String>,
    audit_enabled: Option<bool>,
) -> CtxTuple {
    let hub = Arc::new(ClientHub::new());

    if !matches!(missing, MissingDependency::Rbac) {
        let rbac_dyn: Arc<dyn RbacServiceClientV1> = Arc::clone(&rbac) as _;
        hub.register::<dyn RbacServiceClientV1>(rbac_dyn);
    }

    if !matches!(missing, MissingDependency::TenantResolver) {
        let tr_dyn: Arc<dyn TenantResolverClient> = Arc::clone(&tenant_resolver) as _;
        hub.register::<dyn TenantResolverClient>(tr_dyn);
    }

    if !matches!(missing, MissingDependency::ResourceGroup) {
        let rg_dyn: Arc<dyn ResourceGroupReadHierarchy> = Arc::clone(&resource_group) as _;
        hub.register::<dyn ResourceGroupReadHierarchy>(rg_dyn);
    }

    let registry = Arc::new(RecordingTypesRegistry::new());
    if !matches!(missing, MissingDependency::TypesRegistry) {
        let registry_dyn: Arc<dyn TypesRegistryClient> = Arc::clone(&registry) as _;
        hub.register::<dyn TypesRegistryClient>(registry_dyn);
    }

    // `gts_validation.mode` defaults to `strict`, so an unprimed registry
    // would deny every fixture request with `unknown_resource_type.v1` before
    // it reached the step the test is about. Prime the builder's default type
    // ids here so the rest of the suite runs under the SAME mode production
    // does. A test that pins the mode owns its own priming — that is exactly
    // the knob `tests/gts_type_validation.rs` uses to make a type unknown.
    if gts_validation_mode.is_none() {
        register_default_types(&registry);
    }

    let mut config = json!({
        "vendor": "cf",
        "priority": 100,
    });
    if let Some(max) = max_expansion_ids {
        config["capability_degradation"] = json!({ "max_expansion_ids": max });
    }
    if let Some(mode) = gts_validation_mode {
        config["gts_validation"] = json!({ "mode": mode });
    }
    if let Some(enabled) = audit_enabled {
        config["audit"] = json!({ "enabled": enabled });
    }
    let provider = Arc::new(MapConfigProvider::empty().with_module(MODULE_NAME, config));

    let ctx = GearCtx::new(
        MODULE_NAME,
        Uuid::new_v4(),
        provider,
        Arc::clone(&hub),
        CancellationToken::new(),
    );

    (ctx, hub, registry, rbac, tenant_resolver, resource_group)
}

/// Pre-populate the registry fake with the default subject and resource
/// type ids used by `EvaluationRequestBuilder::default()`.
///
/// Applied automatically by the `build_ctx*` helpers whenever the test has not
/// pinned `gts_validation.mode`, because the default mode is `strict`. Calling
/// it again is harmless (`add_known_type` is idempotent); a test that pins the
/// mode calls it explicitly for whichever legs it wants Known.
#[allow(dead_code)]
pub fn register_default_types(registry: &RecordingTypesRegistry) {
    use authz_resolver_plugin::test_support::request_builder::{
        DEFAULT_RESOURCE_TYPE, DEFAULT_SUBJECT_TYPE,
    };
    registry.add_known_type(DEFAULT_SUBJECT_TYPE);
    registry.add_known_type(DEFAULT_RESOURCE_TYPE);
}

/// Resolve the registered `AuthZ` plugin from `ClientHub` by GTS instance ID.
pub fn resolve_plugin(hub: &ClientHub) -> Arc<dyn AuthZResolverPluginClient> {
    use toolkit::client_hub::ClientScope;
    // The plugin's instance ID is constructed by gts_make_instance_id at
    // registration time; the canonical form is recovered from the registry
    // recording. For the foundation tests we know the suffix is fixed.
    let instance_id = authz_resolver_sdk::AuthZResolverPluginSpecV1::gts_make_instance_id(
        "cf.builtin.authz_resolver.plugin.v1",
    )
    .to_string();
    hub.get_scoped::<dyn AuthZResolverPluginClient>(&ClientScope::gts_id(&instance_id))
        .expect("plugin should be resolvable after successful init")
}
