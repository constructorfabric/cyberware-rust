//! The three `Cluster*Provider` impls, each returning `provider() -> "k8s"`
//! (DESIGN.md §3.5), and [`PROVIDER_NAME`].
//!
//! Each provider deserializes the flattened, provider-specific options map into its
//! per-primitive config, then builds a **fully independent** backend instance plus
//! its [`StopHook`] via the matching standalone plugin — its own `kube::Client`, its
//! own watcher/reaper tasks, its own shutdown. Two profiles binding two primitives
//! to `provider: k8s` therefore get two clients, never a shared one (§3.5): the
//! wiring crate routes each primitive independently, and a native provider never
//! receives the cache backend (SDK `provider.rs` contract).
//!
//! `${VAR}` expansion happens inside each plugin builder's `build_and_start`, so the
//! providers here only deserialize — an unknown option key is rejected by the
//! config's `deny_unknown_fields`, and an unresolvable var surfaces from the builder;
//! both map to [`ClusterError::InvalidConfig`].

use std::sync::Arc;

use async_trait::async_trait;
use cluster_sdk::{
    ClusterCacheBackend, ClusterCacheProvider, ClusterError, ClusterLeaderElectionProvider,
    ClusterLockProvider, DistributedLockBackend, LeaderElectionBackend, StopHook,
};

use crate::config::{K8sCacheConfig, K8sLeaderElectionConfig, K8sLockConfig};
use crate::plugin::{K8sCachePlugin, K8sLeaderElectionPlugin, K8sLockPlugin};

/// The provider name every `Cluster*Provider` and backend reports (§3.5).
pub const PROVIDER_NAME: &str = "k8s";

/// Deserializes `options` into the per-primitive config `T`, mapping a malformed
/// map (an unknown key via `deny_unknown_fields`, a wrong type) to
/// [`ClusterError::InvalidConfig`]. `${VAR}` expansion is deferred to the builder.
fn deserialize<T>(options: &serde_json::Map<String, serde_json::Value>) -> Result<T, ClusterError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::Value::Object(options.clone())).map_err(|err| {
        ClusterError::InvalidConfig {
            reason: format!("k8s: invalid options: {err}"),
        }
    })
}

/// Builds the native Kubernetes cache backend from operator config (§3.5).
pub struct K8sCacheProvider;

#[async_trait]
impl ClusterCacheProvider for K8sCacheProvider {
    fn provider(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn build_cache(
        &self,
        options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn ClusterCacheBackend>, StopHook), ClusterError> {
        let config: K8sCacheConfig = deserialize(options)?;
        let handle = K8sCachePlugin::builder(config).build_and_start().await?;
        let cache = handle.cache();
        let stop: StopHook = Box::new(move || Box::pin(async move { handle.stop().await }));
        Ok((cache, stop))
    }
}

/// Builds the native Kubernetes leader-election backend from operator config (§3.5).
/// This is the first native `with_leader_election_provider` in the tree — a plugin
/// whose leader election is a purpose-built Lease primitive, not the SDK default
/// over a cache, so it never receives a cache backend (SDK `provider.rs` contract).
pub struct K8sLeaderElectionProvider;

#[async_trait]
impl ClusterLeaderElectionProvider for K8sLeaderElectionProvider {
    fn provider(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn build_leader_election(
        &self,
        options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn LeaderElectionBackend>, StopHook), ClusterError> {
        let config: K8sLeaderElectionConfig = deserialize(options)?;
        let handle = K8sLeaderElectionPlugin::builder(config)
            .build_and_start()
            .await?;
        let leader = handle.leader_election();
        let stop: StopHook = Box::new(move || Box::pin(async move { handle.stop().await }));
        Ok((leader, stop))
    }
}

/// Builds the native Kubernetes distributed-lock backend from operator config
/// (§3.5). Standalone — never receives or depends on a cache backend argument (SDK
/// `provider.rs` contract).
pub struct K8sLockProvider;

#[async_trait]
impl ClusterLockProvider for K8sLockProvider {
    fn provider(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn build_lock(
        &self,
        options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn DistributedLockBackend>, StopHook), ClusterError> {
        let config: K8sLockConfig = deserialize(options)?;
        let handle = K8sLockPlugin::builder(config).build_and_start().await?;
        let lock = handle.lock();
        let stop: StopHook = Box::new(move || Box::pin(async move { handle.stop().await }));
        Ok((lock, stop))
    }
}

// Layer-1 unit tests (TESTING.md §2, provider.rs row). The validation cases fail at
// config deserialization — *before* any client is built — so they need no API
// server. Structural coverage (each trait implemented, each `provider()` == "k8s",
// non-cache providers take no cache) is by the impls above compiling and the
// `provider()` assertions here.
#[cfg(test)]
mod provider_tests {
    use super::{
        K8sCacheProvider, K8sLeaderElectionProvider, K8sLockProvider, PROVIDER_NAME, deserialize,
    };
    use crate::config::{K8sCacheConfig, K8sLeaderElectionConfig, K8sLockConfig};
    use cluster_sdk::{
        ClusterCacheProvider, ClusterError, ClusterLeaderElectionProvider, ClusterLockProvider,
    };
    use serde_json::json;

    fn options(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map,
            _ => panic!("options must be a JSON object"),
        }
    }

    #[test]
    fn all_three_providers_report_k8s() {
        assert_eq!(K8sCacheProvider.provider(), "k8s");
        assert_eq!(K8sLeaderElectionProvider.provider(), "k8s");
        assert_eq!(K8sLockProvider.provider(), "k8s");
        // And the constant they all return is the one the wiring matches on.
        assert_eq!(PROVIDER_NAME, "k8s");
    }

    #[tokio::test]
    async fn build_cache_rejects_an_unknown_option_key_as_invalid_config() {
        // `cache_read` is a typo for `cache_reads`; `deny_unknown_fields` rejects it
        // at deserialization, before any client is built (no server needed).
        let result = K8sCacheProvider
            .build_cache(&options(json!({ "cache_read": "quorum" })))
            .await;
        assert!(
            matches!(result, Err(ClusterError::InvalidConfig { .. })),
            "an unknown option key must surface as InvalidConfig, got {:?}",
            result.as_ref().map(|_| ())
        );
    }

    #[tokio::test]
    async fn build_leader_election_rejects_an_unknown_option_key_as_invalid_config() {
        let result = K8sLeaderElectionProvider
            .build_leader_election(&options(json!({ "min_election_ttl": 5000 })))
            .await;
        assert!(
            matches!(result, Err(ClusterError::InvalidConfig { .. })),
            "an unknown option key must surface as InvalidConfig, got {:?}",
            result.as_ref().map(|_| ())
        );
    }

    #[tokio::test]
    async fn build_lock_rejects_an_unknown_option_key_as_invalid_config() {
        let result = K8sLockProvider
            .build_lock(&options(json!({ "reaper_intervall_ms": 1000 })))
            .await;
        assert!(
            matches!(result, Err(ClusterError::InvalidConfig { .. })),
            "an unknown option key must surface as InvalidConfig, got {:?}",
            result.as_ref().map(|_| ())
        );
    }

    #[tokio::test]
    async fn build_lock_rejects_a_wrong_typed_option_as_invalid_config() {
        // `request_timeout_ms` must be an integer, not a string.
        let result = K8sLockProvider
            .build_lock(&options(json!({ "request_timeout_ms": "soon" })))
            .await;
        assert!(matches!(result, Err(ClusterError::InvalidConfig { .. })));
    }

    /// A malformed `lease_prefix` is rejected by the backend's `new`
    /// (`naming::validate_lease_prefix`) — reached here as a pure config check
    /// through the shared `deserialize` + the prefix rule, without a client. The
    /// full `build_*` path proves the same rule against a server in Phase 6.
    #[test]
    fn a_bad_lease_prefix_is_a_config_error() {
        // Deserialization accepts any string; the prefix rule is what rejects it.
        let config: K8sLockConfig =
            deserialize(&options(json!({ "lease_prefix": "Not_A_Label" }))).unwrap();
        assert!(matches!(
            crate::naming::validate_lease_prefix(&config.lease_prefix),
            Err(ClusterError::InvalidConfig { .. })
        ));
        // Sanity: a legal prefix deserializes and validates for each config shape.
        let cache: K8sCacheConfig = deserialize(&options(json!({ "lease_prefix": "cf" }))).unwrap();
        let leader: K8sLeaderElectionConfig =
            deserialize(&options(json!({ "lease_prefix": "cf" }))).unwrap();
        assert!(crate::naming::validate_lease_prefix(&cache.lease_prefix).is_ok());
        assert!(crate::naming::validate_lease_prefix(&leader.lease_prefix).is_ok());
    }
}
