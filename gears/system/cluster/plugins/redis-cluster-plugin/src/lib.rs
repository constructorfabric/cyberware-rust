//! # Redis cluster plugin
//!
//! `redis_cluster_plugin` is the Redis backend plugin for the cluster gear
//! (DESIGN.md §1). It provides a native `ClusterCacheBackend` over a `fred`
//! connection pool and a native `DistributedLockBackend` over a `SET NX PX`
//! lease key with Lua-fenced renew and release (DESIGN.md §5). Leader election
//! is derived from the SDK default backend over the
//! Redis cache — no additional keys or connections are required for that
//! primitive (DESIGN.md §7).
//!
//! This is the recommended cache and lock backend for the **K8s +
//! high-throughput cache** deployment shape (`../../../docs/DESIGN.md` §4.2),
//! where Redis serves cache and lock and K8s Lease serves leader election.
//! ADR-001 puts Redis 10–100× above every other backend on
//! cache and lock throughput, which is what makes the OAGW requirement of
//! 10 000+ counter updates per second reachable at all.
//!
//! ## Consistency: read this before binding a profile to Redis
//!
//! This is the **first `EventuallyConsistent` cache backend in the workspace** —
//! the Postgres and standalone caches both declare `Linearizable`. ADR-009 rates
//! Redis unsafe for CAS-based leader election in *every* replicated
//! configuration, and this plugin declares that rather than papering over it
//! (DESIGN.md §3.6): `consistency()` is `EventuallyConsistent` unless the
//! preflight *verifies* a single node with `appendonly yes` and
//! `appendfsync always` and no replicas.
//!
//! The practical consequence is that a profile binding `cache: { provider: redis }`
//! and omitting `leader_election` or `lock` **fails startup**, because both SDK
//! default backends reject a weak cache. That is correct behaviour, not a bug.
//! The three ways out are documented in DESIGN.md §7; the intended one is to
//! route leader election to a linearizable backend, and to bind
//! `lock: { provider: redis }` explicitly so the native lease-based lock is used
//! instead of the CAS default.
//!
//! ## Status
//!
//! **Feature-complete and reachable.** Both primitives are here and both provider
//! impls are implemented: the cache with its native TTL, watch, and `scan_prefix`
//! ([`RedisCache`]), the lock with its `SET NX PX` lease, Lua-fenced renew and
//! release, and publish-driven blocking acquisition ([`RedisLock`]), the two
//! lifecycle shapes ([`RedisClusterPlugin`], [`RedisLockPlugin`]), and the two
//! providers the wiring matches on ([`RedisCacheProvider`],
//! [`RedisLockProvider`]), with the ADR-004 observability catalog wired through
//! [`RedisSignals`] — the cache via the SDK's `InstrumentedCache` decorator, the
//! lock natively at each site, and the four plugin-local metrics of DESIGN.md §9
//! through a meter this plugin owns.
//!
//! **`cluster/src/gear.rs` registers both providers**, so `provider: redis` under
//! either `cache` or `lock` resolves in any build of the cluster gear — there is
//! no cargo feature an operator has to know about. The accepted cost is that every
//! cluster-gear build links `fred` and its rustls stack whether or not a profile
//! binds Redis.
//!
//! Tests: Layer 1 unit tests (no Docker, sub-second), the four wireable
//! conformance suites, and the Layer 3 scenarios of `docs/TESTING.md` §4.2–§4.6,
//! run by `make test-cluster-redis` in CI's `integration` job. Both multi-node
//! fixtures are built and run on every PR: `RD-SPEC-002` on Sentinel,
//! `RD-SPEC-008`/`009`/`010` and `RD-LOCK-014` on a 3-node Cluster.
//! `docs/TESTING.md` is the register — counts live there and nowhere else,
//! because a number repeated across four files goes stale in three of them.
//!
//! **What is not verified**: the `RD-FAULT-*` fault-injection scenarios, which
//! need a Toxiproxy layer that does not exist. `docs/TESTING.md` §8 is the full
//! register.
//!
//! ## What this crate exports, and why some of it is wider than a consumer needs
//!
//! Some of the re-exports below — the preflight parsers, the script catalog —
//! are not things a consumer of the plugin ever calls. They are public because
//! the modules themselves are private (the Postgres plugin's shape) and each is
//! a testable unit in its own right. The consumer-facing surface is much
//! narrower: the two config types, the two plugins and their handles, and the
//! two providers.
//!
//! [`docs/DESIGN.md`]: https://github.com/constructorfabric/gears-rust/blob/main/gears/system/cluster/plugins/redis-cluster-plugin/docs/DESIGN.md
//! [`docs/TESTING.md`]: https://github.com/constructorfabric/gears-rust/blob/main/gears/system/cluster/plugins/redis-cluster-plugin/docs/TESTING.md

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

mod cache;
mod config;
mod connect;
mod lock;
mod observability;
mod plugin;
mod preflight;
mod provider;
mod redis_error;
mod scripts;
mod shutdown;
mod subscriber;
mod wait;

/// The recording [`ClusterMetrics`](cluster_sdk::ClusterMetrics) double every
/// module's Layer-1 signal tests share.
#[cfg(test)]
mod test_support;

/// TESTING.md §6's mechanical source checks — the commands this plugin must
/// never issue. Crate-level rather than per-module because the rule is about the
/// whole of `src/`, and a check scoped to one module would go stale the moment a
/// new one was added.
#[cfg(test)]
#[path = "static_analysis_tests.rs"]
mod static_analysis_tests;

pub use cache::{CacheInit, RedisCache};
pub use config::{
    Durability, RedisClusterConfig, RedisLockConfig, Topology, WatchMode, default_command_timeout,
    default_key_prefix, default_pool_size, default_wait_timeout,
};
pub use lock::waiters::{HEARTBEAT, ReleaseWait, ReleaseWaiters, wake_cap, wake_delay};
pub use lock::{
    LockInit, LockNames, RedisLock, RedisLockBuilder, RedisLockHandle, RedisLockPlugin,
    lease_remaining, px_millis,
};
pub use observability::{
    EvictionReporter, PLUGIN_SCOPE, RedisSignals, logs, plugin_meter,
    spawn_connection_state_observer,
};
pub use plugin::{RedisClusterBuilder, RedisClusterHandle, RedisClusterPlugin};
pub use preflight::{
    Appendfsync, ConsistencyDecision, DurabilityReading, EVICTION_KEYSPACE_FLAGS, PreflightOutcome,
    PreflightRequest, REQUIRED_KEYSPACE_FLAGS, ReplicationInfo, ReplicationRole,
    SAFE_MAXMEMORY_POLICY, SHARDED_PUBSUB_MAJOR, TopologyFinding, decide_consistency,
    maxmemory_policy_is_safe, merge_keyspace_flags, missing_keyspace_flags, parse_config_get,
    parse_info, parse_info_replication, resolve_topology, run_preflight, supports_sharded_pubsub,
    topology_from_replication,
};
pub use provider::{PROVIDER_NAME, RedisCacheProvider, RedisLockProvider};
pub use redis_error::{is_noscript, map_redis_error};
pub use scripts::{
    ALL_SCRIPTS, CACHE_SCRIPTS, LOCK_SCRIPTS, PoolScriptExecutor, ScriptCache, ScriptExecutor,
    ScriptSpec, eval, load_catalog,
};
pub use shutdown::{
    DropDiagnosis, GUARD_DRAIN_TIMEOUT, POOL_CLOSE_TIMEOUT, cancel_and_diagnose_drop,
};
pub use wait::{WaitPolicy, WaitTarget};
