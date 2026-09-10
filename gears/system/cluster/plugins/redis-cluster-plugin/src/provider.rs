//! The two provider implementations the wiring crate registers for the Redis
//! backend (DESIGN.md §1.1, §3.5).
//!
//! Both answer `provider() -> "redis"`, and they are independent by the SDK's
//! design: `ClusterCacheProvider` and `ClusterLockProvider` are separate traits
//! and a non-cache provider never receives the cache backend, so
//! `lock: { provider: redis }` resolves whether or not `cache` also points at
//! Redis. The cost is a second pool when both do, which DESIGN.md §3.5 accepts
//! rather than invent a lifecycle-ownership story for a shared one.

use std::sync::Arc;

use async_trait::async_trait;
use cluster_sdk::{
    ClusterCacheBackend, ClusterCacheProvider, ClusterError, ClusterLockProvider,
    DistributedLockBackend, StopHook,
};
use toolkit::var_expand::ExpandVars;

use crate::config::{RedisClusterConfig, RedisLockConfig};
use crate::lock::RedisLockPlugin;
use crate::plugin::RedisClusterPlugin;

/// The operator config `provider` name that selects the Redis backend, for both
/// the `cache` and the `lock` primitive bindings.
pub const PROVIDER_NAME: &str = "redis";

/// Deserializes `options` into `T` and applies `#[derive(ExpandVars)]`
/// expansion, mapping both failure modes to [`ClusterError::InvalidConfig`].
///
/// Both are operator errors rather than backend faults, and neither may become
/// a silent fallback: a misspelled key must not leave a default in place
/// (`deny_unknown_fields` is what turns it into the error this reports), and an
/// unresolved `${REDIS_PASSWORD}` must not be sent to the server as a literal
/// password, which would surface as an auth failure pointing at the wrong thing
/// entirely.
fn deserialize_and_expand<T>(
    options: &serde_json::Map<String, serde_json::Value>,
) -> Result<T, ClusterError>
where
    T: serde::de::DeserializeOwned + ExpandVars,
{
    let mut config: T = serde_json::from_value(serde_json::Value::Object(options.clone()))
        .map_err(|err| ClusterError::InvalidConfig {
            reason: format!("redis: invalid options: {err}"),
        })?;
    config
        .expand_vars()
        .map_err(|err| ClusterError::InvalidConfig {
            reason: format!("redis: `url` env-var expansion failed: {err}"),
        })?;
    Ok(config)
}

/// Builds the Redis cache backend from operator config.
pub struct RedisCacheProvider;

#[async_trait]
impl ClusterCacheProvider for RedisCacheProvider {
    fn provider(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn build_cache(
        &self,
        options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn ClusterCacheBackend>, StopHook), ClusterError> {
        let config: RedisClusterConfig = deserialize_and_expand(options)?;
        let handle = RedisClusterPlugin::builder(config)
            .build_and_start()
            .await?;
        let cache = handle.cache();
        // The hook owns the handle, so the wiring's shutdown is the only thing
        // that can drop it — which is what keeps the ADR-006 `Drop` guard from
        // firing on a perfectly ordinary shutdown.
        let stop: StopHook = Box::new(move || Box::pin(async move { handle.stop().await }));
        Ok((cache, stop))
    }
}

/// Builds the standalone Redis lock backend from operator config
/// (DESIGN.md §3.5).
///
pub struct RedisLockProvider;

#[async_trait]
impl ClusterLockProvider for RedisLockProvider {
    fn provider(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn build_lock(
        &self,
        options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn DistributedLockBackend>, StopHook), ClusterError> {
        // `RedisLockConfig`, not `RedisClusterConfig`: the three cache-only keys
        // are absent from it, so a lock binding that sets `watch_mode` fails at
        // startup with the key named rather than having it silently ignored
        // (DESIGN.md §8).
        let config: RedisLockConfig = deserialize_and_expand(options)?;
        let handle = RedisLockPlugin::builder(config).build_and_start().await?;
        let lock = handle.lock();
        // The hook owns the handle, so the wiring's shutdown is the only thing
        // that can drop it — which is what keeps the ADR-006 `Drop` guard from
        // firing on a perfectly ordinary shutdown.
        let stop: StopHook = Box::new(move || Box::pin(async move { handle.stop().await }));
        Ok((lock, stop))
    }
}

// Layer-1 unit tests (TESTING.md §2, `provider.rs` row). Every case below fails
// at deserialization or env-var expansion — before any connection is attempted —
// so none of them needs a container.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map,
            _ => panic!("options must be a JSON object"),
        }
    }

    #[test]
    fn both_provider_names_are_redis() {
        // One name, two primitives: an operator writes `provider: redis` under
        // `cache` and under `lock` and gets this plugin for both.
        assert_eq!(RedisCacheProvider.provider(), "redis");
        assert_eq!(RedisLockProvider.provider(), "redis");
    }

    #[tokio::test]
    async fn the_lock_provider_rejects_a_cache_only_option() {
        // `RedisLockConfig` omits `watch_mode` and `deny_unknown_fields` turns
        // that omission into an error rather than an ignored key: a lock-only
        // binding that sets it has misunderstood something and hears so at
        // startup (DESIGN.md §8).
        let result = RedisLockProvider
            .build_lock(&options(json!({
                "url": "redis://redis:6379/0",
                "watch_mode": "disabled",
            })))
            .await;
        let Err(ClusterError::InvalidConfig { reason }) = result else {
            panic!("a cache-only option on a lock binding must be rejected");
        };
        assert!(
            reason.contains("watch_mode"),
            "the error must name the offending key, got {reason}"
        );
    }

    #[tokio::test]
    async fn the_lock_provider_rejects_a_missing_url_before_connecting() {
        let result = RedisLockProvider
            .build_lock(&options(json!({ "pool_size": 4 })))
            .await;
        assert!(
            matches!(result, Err(ClusterError::InvalidConfig { .. })),
            "a lock options map with no url must surface as InvalidConfig"
        );
    }

    #[tokio::test]
    async fn the_lock_provider_needs_no_cache_backend_to_be_asked_for_one() {
        // The SDK's "non-cache providers do not receive the cache backend"
        // contract, asserted the only way a signature can be: `build_lock` takes
        // options and nothing else, so there is no cache for it to consult even
        // when one exists. This fails at config validation, before any
        // connection, which is what keeps it a Layer 1 test.
        let result = RedisLockProvider
            .build_lock(&options(json!({
                "url": "redis://127.0.0.1:1/0",
                "pool_size": 0,
            })))
            .await;
        let Err(ClusterError::InvalidConfig { reason }) = result else {
            panic!("a zero pool_size must be rejected as InvalidConfig");
        };
        assert!(
            reason.contains("pool_size"),
            "the error must name the offending key, got {reason}"
        );
    }

    #[tokio::test]
    async fn a_missing_url_is_invalid_config() {
        let result = RedisCacheProvider
            .build_cache(&options(json!({ "pool_size": 4 })))
            .await;
        assert!(
            matches!(result, Err(ClusterError::InvalidConfig { .. })),
            "a malformed options map must surface as InvalidConfig"
        );
    }

    #[tokio::test]
    async fn an_unknown_option_key_is_invalid_config() {
        // The `deny_unknown_fields` payoff at the provider boundary: an
        // `allow_weak_consistency` misplaced onto this native binding (see
        // DESIGN.md §13 D1) fails loudly here instead of being
        // ignored.
        let result = RedisCacheProvider
            .build_cache(&options(json!({
                "url": "redis://redis:6379/0",
                "allow_weak_consistency": true,
            })))
            .await;
        assert!(
            matches!(result, Err(ClusterError::InvalidConfig { .. })),
            "an unknown option key must surface as InvalidConfig"
        );
    }

    #[tokio::test]
    async fn an_unresolvable_env_var_is_invalid_config() {
        let result = RedisCacheProvider
            .build_cache(&options(json!({
                "url": "redis://:${REDIS_CLUSTER_PROVIDER_UNSET}@redis:6379/0",
            })))
            .await;
        assert!(
            matches!(result, Err(ClusterError::InvalidConfig { .. })),
            "an unresolvable env var must surface as InvalidConfig"
        );
    }

    #[tokio::test]
    async fn a_zero_pool_size_is_rejected_before_any_connection() {
        // `config.validate()` runs at the top of `build_and_start`, so this
        // returns without ever dialling — which is also what makes it a Layer 1
        // test rather than a Layer 3 one.
        let result = RedisCacheProvider
            .build_cache(&options(json!({
                "url": "redis://127.0.0.1:1/0",
                "pool_size": 0,
            })))
            .await;
        let Err(ClusterError::InvalidConfig { reason }) = result else {
            panic!("a zero pool_size must be rejected as InvalidConfig");
        };
        assert!(
            reason.contains("pool_size"),
            "the error must name the offending key, got {reason}"
        );
    }
}
