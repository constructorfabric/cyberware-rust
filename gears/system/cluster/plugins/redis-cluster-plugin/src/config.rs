//! Configuration for the Redis cluster plugin (DESIGN.md §8).
//!
//! Two config types exist because the combined cache+lock plugin and the
//! standalone lock-only provider (DESIGN.md §3.5) need different field sets:
//! [`RedisClusterConfig`] carries `watch_mode` and
//! `manage_keyspace_notifications`, neither of which
//! mean anything without the cache half, and [`RedisLockConfig`] omits them.
//!
//! ## Why the shared fields are duplicated rather than flattened
//!
//! DESIGN.md §8 proposes factoring the shared subset into one inner struct so
//! the two types cannot drift. That is not implementable: it needs
//! `#[serde(flatten)]`, and serde refuses to derive `flatten` together with the
//! `#[serde(deny_unknown_fields)]` that DESIGN.md §8 and TESTING.md §2 both
//! require. Flattening is implemented by buffering every unmatched key and
//! replaying it into the inner type, so the outer type can no longer tell an
//! inner field from an operator's typo — serde rejects the combination at
//! compile time rather than silently picking one.
//!
//! `deny_unknown_fields` is the more valuable half: it is what turns
//! `pool_sise: 8` into a startup error instead of a silently-ignored key, and
//! it is also what makes a misplaced `allow_weak_consistency` on a *native*
//! binding fail loudly (DESIGN.md §7). So the fields are duplicated, exactly as
//! `postgres-cluster-plugin/src/config.rs` does and for the same reason, and two
//! things hold the copies together: every default
//! lives in one `default_*` function shared by both types, and
//! `config_tests.rs`'s drift guard compares the two types' accepted field sets
//! mechanically, so adding a field to one and not the other fails a test.

use std::fmt;
use std::time::Duration;

use cluster_sdk::ClusterError;
use serde::Deserialize;

/// The masked stand-in rendered for `url` in `Debug` output, so a `{:?}` of a
/// config (in a log line or a panic message) never leaks the password the
/// expanded connection URL embeds. Both config types hand-write `Debug` rather
/// than `#[derive]`ing it for this reason (DESIGN.md §8).
const REDACTED_URL: &str = "<redacted>";

/// Default command-pool size. DESIGN.md §8 / §3.3.
///
/// About pipelining headroom rather than resource thrift: a Redis connection
/// costs ~20 KB (ADR-001) and a held lock consumes none, so the pool never has
/// to be sized against the workload's concurrency.
#[must_use]
pub fn default_pool_size() -> u32 {
    4
}

/// Default per-command timeout, in milliseconds. DESIGN.md §8.
#[must_use]
pub fn default_command_timeout() -> u64 {
    5_000
}

/// Default prefix for every key and channel the plugin owns. DESIGN.md §2.1.
#[must_use]
pub fn default_key_prefix() -> String {
    "cluster".to_owned()
}

/// Default `WAIT` timeout, in milliseconds, used when `wait_replicas` is set.
///
/// DESIGN.md §8 declares the field with a default but does not name a value.
/// 1 s is chosen because `WAIT` sits on the tail of a lock or CAS write that
/// already carries a 5 s command timeout: a replica that has not acknowledged
/// within a second is not about to, and blocking the write for the whole
/// command budget would turn a narrowed failover window into a latency
/// regression. A `WAIT` that times out is surfaced rather than swallowed
/// (DESIGN.md §3.6), so the shorter bound loses no information.
#[must_use]
pub fn default_wait_timeout() -> u64 {
    1_000
}

/// Operator hint for the server topology (DESIGN.md §8).
///
/// If omitted, the plugin detects the topology from `INFO replication` at
/// startup (DESIGN.md §3.4). If present, the plugin **trusts it and skips the
/// corresponding detection** — the escape hatch for a locked-down managed
/// instance whose operator knows an answer the plugin cannot read. A hint is
/// therefore never *verified*, which is why a `Linearizable` declaration
/// resting on one is flagged as asserted-not-verified (DESIGN.md §3.6,
/// [`crate::ConsistencyDecision`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// A single Redis server with no replicas. The only topology that can
    /// reach `Linearizable`, and only when paired with durable writes.
    Standalone,
    /// A Sentinel-managed primary with asynchronous replicas.
    Sentinel,
    /// Redis Cluster.
    Cluster,
}

/// Operator hint for write durability at acknowledgement time (DESIGN.md §8).
///
/// If omitted, the plugin detects durability from
/// `CONFIG GET appendonly|appendfsync`. Unlike [`Topology`], a
/// [`Durability::FsyncAlways`] hint is *cross-checked* whenever `CONFIG GET` is
/// readable, and a hint the server contradicts fails startup naming both values
/// (DESIGN.md §3.6) — claiming the one setting that unlocks `Linearizable` is
/// the claim worth checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// `appendonly yes` with `appendfsync always`: nothing is acknowledged
    /// before it is fsynced.
    FsyncAlways,
    /// `appendonly yes` with `appendfsync everysec` — the Redis default. Up to
    /// one second of accepted writes is lost on a crash.
    FsyncEverysec,
    /// No append-only file. Acknowledged writes survive only in memory.
    None,
}

/// How cache watches are sourced (DESIGN.md §4.3).
///
/// Two variants, not three: DESIGN.md §13 D4 considered and rejected a raw
/// keyspace-notification mode, so there is no `todo!()` arm here waiting to be
/// filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    /// The default: each mutation publishes its own event from inside the Lua
    /// script, and Redis `expired`/`evicted` keyspace notifications supply the
    /// two events no plugin code can observe.
    #[default]
    Publish,
    /// No publish and no cache-event subscriber. `watch`/`watch_prefix` return
    /// `Unsupported`. The honest answer for a managed
    /// Redis where neither `CONFIG` nor the publish overhead is acceptable.
    Disabled,
}

/// Configuration for the combined `RedisClusterPlugin` (cache + lock,
/// DESIGN.md §3.2).
#[derive(Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct RedisClusterConfig {
    /// Connection URL. `redis://`, `rediss://`, `redis-sentinel://`, or
    /// `redis-cluster://`.
    ///
    /// Supports `${VAR}` / `${VAR:-default}` expansion via
    /// `toolkit_utils::var_expand`, resolved through `ctx.config_expanded()` —
    /// the same mechanism `libs/toolkit-db` uses for DB DSNs. A credstore-backed
    /// (`secret_ref`) path is deferred (DESIGN.md §13 D5).
    ///
    /// The expanded value embeds a password, which is why `Debug` is
    /// hand-written below and masks this field.
    #[expand_vars]
    pub url: String,

    /// Command-pool size. Default: 4. See [`default_pool_size`].
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,

    /// Per-command timeout, in milliseconds. Default: 5000.
    ///
    /// Enforced client-side by `fred`, so no command can block indefinitely
    /// once issued — the bound that `stop()` and `RD-LIFE-009` rely on
    /// (DESIGN.md §11, §12). Zero is rejected by [`Self::validate`]: `fred`
    /// reads a zero `default_command_timeout` as "no timeout at all", so it
    /// would silently remove that bound rather than shorten it.
    #[serde(default = "default_command_timeout")]
    pub command_timeout_ms: u64,

    /// Prefix for every key and channel this plugin owns. Default: `"cluster"`.
    /// DESIGN.md §2.1.
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Logical database index. Ignored in Cluster mode, which has only db 0.
    /// Default: 0.
    #[serde(default)]
    pub database: u8,

    /// Operator hint for topology. Omitted → detected via `INFO replication`
    /// (DESIGN.md §3.4). Drives the consistency declaration (DESIGN.md §3.6).
    #[serde(default)]
    pub topology: Option<Topology>,

    /// Operator hint for write durability. Omitted → detected via
    /// `CONFIG GET appendonly|appendfsync`. A hint contradicted by a readable
    /// server config fails startup with `InvalidConfig` (DESIGN.md §3.6).
    #[serde(default)]
    pub durability: Option<Durability>,

    /// Append `WAIT <n> <wait_timeout_ms>` to lock and CAS writes. Narrows the
    /// Sentinel failover window; per ADR-009 it does **not** upgrade the
    /// declared consistency (DESIGN.md §3.6). Default: none.
    ///
    /// `Some(0)` is accepted and means what Redis means by it — `WAIT 0`
    /// returns immediately — so it is a no-op rather than an error.
    #[serde(default)]
    pub wait_replicas: Option<u32>,

    /// Timeout for the `WAIT` above, in milliseconds. Default: 1000.
    /// See [`default_wait_timeout`].
    ///
    /// Bounded above as well as below: `WAIT`'s timeout argument is signed, so a
    /// value past `i64::MAX` fails startup with `InvalidConfig`
    /// ([`WaitPolicy::from_config`](crate::WaitPolicy::from_config)) rather than
    /// being clamped into a deadline no operator asked for.
    #[serde(default = "default_wait_timeout")]
    pub wait_timeout_ms: u64,

    /// How cache watches are sourced (DESIGN.md §4.3). Default: `publish`.
    #[serde(default)]
    pub watch_mode: WatchMode,

    /// When `true`, and the server's `notify-keyspace-events` lacks the flags
    /// `Expired` events need, issue one `CONFIG SET` adding them and re-read to
    /// confirm (DESIGN.md §3.4).
    ///
    /// Default `false`: this writes a **server-wide** setting that unrelated
    /// tenants of the same Redis share, so it is exactly the kind of surprise
    /// an operator should have to opt into.
    #[serde(default)]
    pub manage_keyspace_notifications: bool,
}

impl fmt::Debug for RedisClusterConfig {
    /// Hand-written so `url` (which embeds the Redis password after
    /// `expand_vars`) is masked — see the `REDACTED_URL` const.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisClusterConfig")
            .field("url", &REDACTED_URL)
            .field("pool_size", &self.pool_size)
            .field("command_timeout_ms", &self.command_timeout_ms)
            .field("key_prefix", &self.key_prefix)
            .field("database", &self.database)
            .field("topology", &self.topology)
            .field("durability", &self.durability)
            .field("wait_replicas", &self.wait_replicas)
            .field("wait_timeout_ms", &self.wait_timeout_ms)
            .field("watch_mode", &self.watch_mode)
            .field(
                "manage_keyspace_notifications",
                &self.manage_keyspace_notifications,
            )
            .finish()
    }
}

/// Rejects a zero `pool_size`.
///
/// `fred::clients::Pool::new` already refuses an empty pool, but with
/// `"Pool cannot be empty."` — a message that names neither the config key nor
/// the file it came from. Failing here instead means the operator reads the
/// name of the field they have to change.
fn reject_zero_pool_size(value: u32) -> Result<(), ClusterError> {
    if value == 0 {
        return Err(ClusterError::InvalidConfig {
            reason: "pool_size must be greater than zero".to_owned(),
        });
    }
    Ok(())
}

/// Rejects a zero duration for a config key whose zero value means something
/// materially different from "very small", naming the offending key.
///
/// Both callers are that shape rather than merely defensive:
/// `command_timeout_ms: 0` disables `fred`'s command timeout outright, and
/// `wait_timeout_ms: 0` makes `WAIT` block until the replicas answer with no
/// deadline at all. Each silently removes a bound the design relies on, so
/// neither can be left to the backend to discover.
fn reject_zero_duration(value: u64, field: &str) -> Result<(), ClusterError> {
    if value == 0 {
        return Err(ClusterError::InvalidConfig {
            reason: format!("{field} must be greater than zero"),
        });
    }
    Ok(())
}

impl RedisClusterConfig {
    /// Validates the config values that can only fail at startup, before any
    /// pool, subscriber, or background task is constructed. Called at the top
    /// of `build_and_start`.
    ///
    /// # Errors
    /// [`ClusterError::InvalidConfig`] for a zero `pool_size`,
    /// `command_timeout_ms`, or `wait_timeout_ms`.
    pub fn validate(&self) -> Result<(), ClusterError> {
        reject_zero_pool_size(self.pool_size)?;
        reject_zero_duration(self.command_timeout_ms, "command_timeout_ms")?;
        reject_zero_duration(self.wait_timeout_ms, "wait_timeout_ms")?;
        Ok(())
    }

    /// The per-command timeout as a [`Duration`].
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }

    /// The `WAIT` timeout as a [`Duration`].
    #[must_use]
    pub fn wait_timeout(&self) -> Duration {
        Duration::from_millis(self.wait_timeout_ms)
    }
}

/// Configuration for the standalone `RedisLockPlugin` (DESIGN.md §3.5).
///
/// A separate, smaller type carrying only the fields the lock primitive uses.
/// The two it omits — `watch_mode` and `manage_keyspace_notifications` — are
/// cache concerns, and `deny_unknown_fields` turns
/// each of them into a startup error here rather than an ignored key: a
/// lock-only binding that sets `watch_mode` has misunderstood something, and
/// says so at startup.
#[derive(Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct RedisLockConfig {
    /// See [`RedisClusterConfig::url`].
    #[expand_vars]
    pub url: String,

    /// See [`RedisClusterConfig::pool_size`].
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,

    /// See [`RedisClusterConfig::command_timeout_ms`].
    #[serde(default = "default_command_timeout")]
    pub command_timeout_ms: u64,

    /// See [`RedisClusterConfig::key_prefix`].
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// See [`RedisClusterConfig::database`].
    #[serde(default)]
    pub database: u8,

    /// See [`RedisClusterConfig::topology`].
    #[serde(default)]
    pub topology: Option<Topology>,

    /// See [`RedisClusterConfig::durability`].
    #[serde(default)]
    pub durability: Option<Durability>,

    /// See [`RedisClusterConfig::wait_replicas`].
    #[serde(default)]
    pub wait_replicas: Option<u32>,

    /// See [`RedisClusterConfig::wait_timeout_ms`].
    #[serde(default = "default_wait_timeout")]
    pub wait_timeout_ms: u64,
}

impl fmt::Debug for RedisLockConfig {
    /// Hand-written so `url` is masked — see the `REDACTED_URL` const.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisLockConfig")
            .field("url", &REDACTED_URL)
            .field("pool_size", &self.pool_size)
            .field("command_timeout_ms", &self.command_timeout_ms)
            .field("key_prefix", &self.key_prefix)
            .field("database", &self.database)
            .field("topology", &self.topology)
            .field("durability", &self.durability)
            .field("wait_replicas", &self.wait_replicas)
            .field("wait_timeout_ms", &self.wait_timeout_ms)
            .finish()
    }
}

impl RedisLockConfig {
    /// Validates the config values that can only fail at startup, before any
    /// pool or subscriber is constructed. Called at the top of
    /// `build_and_start`.
    ///
    /// # Errors
    /// [`ClusterError::InvalidConfig`] for a zero `pool_size`,
    /// `command_timeout_ms`, or `wait_timeout_ms`.
    pub fn validate(&self) -> Result<(), ClusterError> {
        reject_zero_pool_size(self.pool_size)?;
        reject_zero_duration(self.command_timeout_ms, "command_timeout_ms")?;
        reject_zero_duration(self.wait_timeout_ms, "wait_timeout_ms")?;
        Ok(())
    }

    /// The per-command timeout as a [`Duration`].
    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }

    /// The `WAIT` timeout as a [`Duration`].
    #[must_use]
    pub fn wait_timeout(&self) -> Duration {
        Duration::from_millis(self.wait_timeout_ms)
    }
}

// Layer-1 unit tests (TESTING.md §2, `config.rs` row). Pure serde, expansion,
// and validation — no container. Out-of-line because DE1101 caps an inline test
// block at 100 lines (`dylint.toml`), the same reason the Postgres plugin's
// config tests live in their own file.
#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
