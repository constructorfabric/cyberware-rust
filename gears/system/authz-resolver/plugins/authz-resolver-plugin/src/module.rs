//! toolkit module registration for the `AuthZ` resolver plugin.
//!
//! Sequence (mirrors `keycloak-authn-plugin` and `openbao-credstore-gateway`):
//! 1. Read `AuthZResolverPluginConfig` from `server.yaml` — fail fast on
//!    deserialization error (`deny_unknown_fields` catches typos). Empty
//!    `vendor` is also a hard `init()` error.
//! 2. Resolve four `ClientHub` dependencies — `dyn RbacServiceClientV1`,
//!    `dyn TenantResolverClient`, `dyn ResourceGroupReadHierarchy`, and
//!    `dyn TypesRegistryClient`. Missing any one fails `init()` with a
//!    descriptive `anyhow::Error`.
//! 3. Build the `AuthZResolverPlugin`.
//! 4. Register the GTS instance with the types-registry (fail-fast — done
//!    before `ClientHub` registration so a registry failure does not leave
//!    an orphaned hub entry). `register_scoped` itself is infallible
//!    (returns `()`), so once the registry call succeeds no half-state is
//!    possible.
//! 5. Register `Arc<AuthZResolverPlugin>` in `ClientHub` as
//!    `dyn AuthZResolverPluginClient` scoped by the GTS instance ID.
//!
//! Test-support feature: enabling `test-support` on this crate from a
//! downstream consumer turns on `types-registry-sdk/test-util`
//! and pulls in `opentelemetry_sdk`. The consumer's binary then carries the
//! test-util surface, not just its tests — by design, so integration test
//! crates can share fakes. Production builds leave the feature off.

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::plugin_api::AuthZResolverPluginClient;
use rbac_sdk::RbacServiceClientV1;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
use tenant_resolver_sdk::api::TenantResolverClient;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::config::ConfigError;
use toolkit::context::GearCtx;
use tracing::{info, warn};
use types_registry_sdk::TypesRegistryClient;

use crate::config::AuthZResolverPluginConfig;
use crate::domain::evaluate::AuthZResolverPlugin;
use crate::infra::gts_registration::register_plugin_instance;

#[toolkit::gear(
    name = "authz-resolver-plugin",
    deps = [types_registry, authz_resolver, rbac, tenant_resolver, resource_group]
)]
#[derive(Default)]
pub struct AuthZResolverPluginGear;

#[async_trait]
impl Gear for AuthZResolverPluginGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Convention (mirrors `rbac`): a compiled-in module with no entry in
        // the `gears:` block is intentionally disabled — skip init rather
        // than fail. `vendor` stays fail-fast when the section IS present
        // (handled below), so a misconfigured-but-listed plugin still aborts.
        let config: AuthZResolverPluginConfig = match ctx.config() {
            Ok(cfg) => cfg,
            Err(ConfigError::GearNotFound { .. }) => {
                info!(
                    "authz-resolver-plugin: not present in the `gears:` config block, \
                     skipping init() (compiled in but unconfigured = disabled)"
                );
                return Ok(());
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "authz-resolver-plugin: failed to deserialize config (vendor is required, \
                     and `deny_unknown_fields` rejects typos): {err}"
                ));
            }
        };

        // `vendor` is required: the AuthZ Resolver Gateway selects this plugin
        // by its `vendor`, so a missing/empty value would silently register
        // the plugin under no vendor and leave it unselectable. Fail fast
        // rather than guessing a default.
        if config.vendor.is_empty() {
            return Err(anyhow::anyhow!(
                "authz-resolver-plugin: `vendor` is required and must be non-empty - \
                 the gateway selects plugins by vendor; set \
                 `modules.authz-resolver-plugin.config.vendor`"
            ));
        }
        let vendor = config.vendor.clone();

        let hub = ctx.client_hub();

        let rbac: Arc<dyn RbacServiceClientV1> =
            hub.get::<dyn RbacServiceClientV1>().map_err(|err| {
                anyhow::anyhow!(
                    "authz-resolver-plugin: missing dependency RbacServiceClientV1 in ClientHub; \
                 the `rbac` module must register before this plugin: {err:?}"
                )
            })?;

        let tenant_resolver: Arc<dyn TenantResolverClient> =
            hub.get::<dyn TenantResolverClient>().map_err(|err| {
                anyhow::anyhow!(
                    "authz-resolver-plugin: missing dependency TenantResolverClient in ClientHub; \
                     the `tenant-resolver` module must register before this plugin: {err:?}"
                )
            })?;

        // Deliberately the unscoped, PEP-bypassing read contract. As the PDP,
        // the plugin's resource-group reads MUST NOT route through the
        // PolicyEnforcer: doing so would re-enter the PDP and recurse.
        let resource_group: Arc<dyn ResourceGroupReadHierarchy> =
            hub.get::<dyn ResourceGroupReadHierarchy>().map_err(|err| {
                anyhow::anyhow!(
                    "authz-resolver-plugin: missing dependency ResourceGroupReadHierarchy in ClientHub; \
                     the `resource-group` module must register before this plugin: {err:?}"
                )
            })?;

        let registry: Arc<dyn TypesRegistryClient> =
            hub.get::<dyn TypesRegistryClient>().map_err(|err| {
                anyhow::anyhow!(
                    "authz-resolver-plugin: missing dependency TypesRegistryClient in ClientHub; \
                     the `types-registry` module must register before this plugin: {err:?}"
                )
            })?;

        let config = Arc::new(config);
        let plugin = Arc::new(AuthZResolverPlugin::new(
            Arc::clone(&config),
            rbac,
            tenant_resolver,
            resource_group,
            Arc::clone(&registry),
        ));

        // GTS registration first — a registry failure must not leave an
        // orphaned ClientHub entry.
        let instance_id = register_plugin_instance(&*registry, &vendor, config.priority).await?;

        let scoped: Arc<dyn AuthZResolverPluginClient> = plugin;
        hub.register_scoped::<dyn AuthZResolverPluginClient>(
            ClientScope::gts_id(&instance_id),
            scoped,
        );

        // Surface the trusted-actor bypass at startup: it is a privilege
        // bypass, so an operator should see its size in the boot log without
        // reading config back. Zero is the default and the safe state.
        let trusted_actors = crate::domain::subject_type::TrustedSystemActors::from_config(
            &config.trusted_system_actors,
        )
        .len();
        info!(
            instance_id = %instance_id,
            vendor = %vendor,
            priority = config.priority,
            gts_validation_mode = ?config.gts_validation.mode,
            audit_enabled = config.audit.enabled,
            trusted_system_actors = trusted_actors,
            "authz-resolver-plugin registered"
        );

        // Both of the following are deliberate weakenings of a control. Neither
        // is an error, but neither should be discoverable only by reading the
        // config back, so each gets its own line in the boot log.
        if !config.audit.enabled {
            warn!(
                "authz-resolver-plugin: audit.enabled = false - authorization decision records \
                 are DROPPED. The PDP will decide without leaving an audit trail; unset the flag \
                 to restore it."
            );
        }
        if trusted_actors > 0 {
            warn!(
                trusted_system_actors = trusted_actors,
                "authz-resolver-plugin: trusted_system_actors is configured - these subject ids \
                 short-circuit policy evaluation to an allow. Intended only for unforgeable \
                 in-process system actors."
            );
        }

        Ok(())
    }
}
