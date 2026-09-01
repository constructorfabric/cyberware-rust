//! GTS plugin instance registration with the types-registry.
//!
//! The gateway discovers `AuthZ` resolver plugins by querying the
//! types-registry for instances of `AuthZResolverPluginSpecV1`. This module
//! builds the instance payload and calls `register` exactly the way
//! `keycloak-authn-plugin` does for `AuthNResolverPluginSpecV1`. Performed
//! before `ClientHub` registration in `Gear::init` so a registry failure
//! does not leave the in-memory hub with an orphaned entry.

use authz_resolver_sdk::AuthZResolverPluginSpecV1;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

/// Vendor used in registration tests. Production reads `vendor` from config
/// (required — see `module.rs`); there is no built-in default, so this const
/// exists only for tests.
#[cfg(test)]
const PLUGIN_VENDOR: &str = "cf";

/// Suffix appended to the `AuthZ` plugin schema's base ID to build this
/// plugin's unique GTS instance identifier. The base schema is fixed by the
/// SDK (`gts.cf.toolkit.plugins.plugin.v1~cf.core.authz_resolver.plugin.v1~`);
/// this is the trailing instance segment. Follows the GTS convention
/// `<vendor>.<package>.authz_resolver.plugin.v1`.
pub(crate) const PLUGIN_INSTANCE_SUFFIX: &str = "cf.builtin.authz_resolver.plugin.v1";

/// Register this plugin's instance with the types-registry.
///
/// Returns the canonical GTS instance ID on success so callers can use it
/// as the `ClientHub` scope.
pub(crate) async fn register_plugin_instance(
    registry: &dyn TypesRegistryClient,
    vendor: &str,
    priority: i16,
) -> anyhow::Result<String> {
    // Use the canonical toolkit helper (same path as static-authz-plugin and
    // the other built-in plugins) so the instance id, vendor, and priority
    // are assembled consistently with the rest of the platform.
    let (instance_id_typed, instance_json) =
        PluginV1::<AuthZResolverPluginSpecV1>::build_registration(
            PLUGIN_INSTANCE_SUFFIX,
            vendor,
            priority,
        )?;
    let instance_id = instance_id_typed.to_string();

    let results = registry.register(vec![instance_json]).await?;
    RegisterResult::ensure_all_ok(&results)?;

    // Note: the single operational "registered" log line lives in `module.rs`
    // (it has the full context); avoid duplicating it here.
    Ok(instance_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingTypesRegistry;

    #[tokio::test]
    async fn registers_with_expected_instance_id() {
        let registry = RecordingTypesRegistry::new();
        let instance_id = register_plugin_instance(&registry, PLUGIN_VENDOR, 100)
            .await
            .expect("registration should succeed");

        assert!(
            instance_id.contains("authz_resolver.plugin.v1"),
            "instance id should contain the plugin type segment: {instance_id}"
        );
        assert!(
            instance_id.ends_with(PLUGIN_INSTANCE_SUFFIX),
            "instance id should end with the configured suffix: {instance_id}"
        );

        let calls = registry.calls();
        assert_eq!(calls.len(), 1, "exactly one register call expected");
        let payload = &calls[0][0];
        assert_eq!(
            payload.get("id").and_then(|v| v.as_str()),
            Some(instance_id.as_str()),
            "the registered payload should carry the same instance id"
        );
        assert_eq!(payload.get("vendor").and_then(|v| v.as_str()), Some("cf"));
        assert_eq!(
            payload.get("priority").and_then(serde_json::Value::as_i64),
            Some(100)
        );
    }

    #[tokio::test]
    async fn idempotent_re_registration_succeeds() {
        let registry = RecordingTypesRegistry::new();
        let id1 = register_plugin_instance(&registry, PLUGIN_VENDOR, 100)
            .await
            .expect("first registration");
        let id2 = register_plugin_instance(&registry, PLUGIN_VENDOR, 100)
            .await
            .expect("re-registration should also succeed (registry handles idempotency)");
        assert_eq!(id1, id2, "instance id is deterministic for the same suffix");
        assert_eq!(registry.calls().len(), 2, "both registrations recorded");
    }

    #[tokio::test]
    async fn registry_error_propagates() {
        let registry = RecordingTypesRegistry::failing("simulated registry failure");
        let err = register_plugin_instance(&registry, PLUGIN_VENDOR, 100)
            .await
            .expect_err("registry failure must surface");
        assert!(
            err.to_string().contains("simulated registry failure"),
            "error must propagate registry message: {err}"
        );
    }
}
