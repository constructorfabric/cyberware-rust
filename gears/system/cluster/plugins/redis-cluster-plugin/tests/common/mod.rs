//! Shared testcontainers fixtures for the Redis cluster plugin's Layer 2/3 test
//! suites (docs/TESTING.md §4.1). Not a test binary itself — every `tests/*.rs`
//! file that needs it declares `mod common;`.
//!
//! Gated behind `--features integration` end to end (docs/TESTING.md §7): this
//! whole module is compiled out of a default `cargo test`, so it never requires a
//! Docker daemon unless the feature is explicitly enabled.
//!
//! # Why a parameterized recipe rather than TESTING §4.1's two functions
//!
//! TESTING §4.1 names four fixtures. The *scenario* set needs more container
//! shapes than that, because several scenarios are precisely about server
//! configuration rather than about plugin logic:
//!
//! | Shape | Needed by |
//! |---|---|
//! | stock (`EventuallyConsistent`) | most of §4.2–§4.5, `RD-SPEC-001` |
//! | durable single node (`Linearizable`) | the leader conformance suite (§3), `RD-SPEC-003` |
//! | no keyspace notifications | `RD-SPEC-005`, `RD-SPEC-005b`, `RD-LOCK-009` |
//! | unsafe `maxmemory-policy`, no `maxmemory` | `RD-SPEC-006` |
//! | tiny `maxmemory` + `allkeys-lru` | `RD-SPEC-007`, `RD-SPEC-007b`, `RD-LOCK-015` |
//! | `appendfsync everysec` | `RD-SPEC-011` |
//! | Redis 6 | `RD-SPEC-014`'s negative half |
//!
//! Named wrappers over one [`RedisRecipe`] rather than one copy each of the
//! start-and-retry sequence: the shapes differ only in server flags, and a
//! duplicated retry loop is the kind of thing that gets fixed in one copy.
//!
//! # The two multi-node topologies are built differently, and have to be
//!
//! [`start_redis_sentinel`] and [`start_redis_cluster`] do not go through
//! [`RedisRecipe`]. Each runs every node as a *process inside one container*
//! with host ports mapped 1:1, because both topologies advertise an address to
//! the client — Sentinel the primary's, a cluster node its `MOVED` target — and
//! only 1:1 mapping makes one address correct for both the nodes' own gossip
//! and a client outside Docker. [`free_port_base`] carries the full reasoning.
//!
//! # The keyspace flags are `Kxe`, and they are set by container flag
//!
//! TESTING §4.1 writes `Kgx$e`; the correct minimal set is **`Kxe`**
//! (DESIGN.md §4.3). `K` plus `x` yields `expired` and `K`
//! plus `e` yields `evicted` — the two events no plugin code can publish for
//! itself — while `g` and `$` would add a notification for every generic and
//! every string command **server-wide**, which on a shared Redis is a cost paid
//! by unrelated tenants for nothing this plugin reads.
//!
//! Set by container flag rather than by `CONFIG SET` so the *default* test
//! posture matches a well-configured production server: a plugin that only works
//! after mutating a server-wide setting is not the thing TESTING §4 means to
//! exercise. The fixtures that deliberately omit the flags
//! ([`start_redis_no_notifications`]) are how the degradation path gets covered.

#![cfg(feature = "integration")]
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test harness: a fixture setup failure IS the test failure, and not every helper \
              below is used by every test binary that `mod common;`s this file"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fred::clients::Client;
use fred::interfaces::ClientLike;
use fred::types::Builder;
use fred::types::config::Config;
use redis_cluster_plugin::{RedisClusterConfig, RedisLockConfig};
use serde_json::json;
use testcontainers::core::ContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::redis::{REDIS_PORT, Redis};

/// The Redis image tag every per-PR fixture runs, pinned rather than `latest` so
/// a scenario asserting on server *behaviour* (keyspace notification wording,
/// eviction accounting, `PTTL` sentinels) cannot start failing because Docker Hub
/// moved a tag. `alpine` for start-up time: the fixtures are per-PR and TESTING
/// §7 budgets the whole suite at 20–30 s.
///
/// The `testcontainers_modules::redis::Redis` image defaults to tag `5.0`, which
/// predates sharded pub/sub and several `INFO` fields, so every fixture below
/// overrides it explicitly.
pub const REDIS_TAG: &str = "7-alpine";

/// The Redis 6 tag, for `RD-SPEC-014`'s negative half — a server that supports
/// neither `SPUBLISH` nor `SSUBSCRIBE`, so the plugin must log no
/// `sharded_pubsub_available` and issue no sharded command.
pub const REDIS_6_TAG: &str = "6-alpine";

/// The minimal correct `notify-keyspace-events` set — see this module's header
/// and DESIGN.md §4.3.
///
/// Deliberately not read from `redis_cluster_plugin::REQUIRED_KEYSPACE_FLAGS`:
/// a fixture that configures the server from the same constant the plugin checks
/// against would agree with the plugin by construction, and `RD-SPEC-005`'s whole
/// subject is what happens when the two disagree.
pub const KEYSPACE_FLAGS: &str = "Kxe";

/// A container shape: the server flags that distinguish one fixture from
/// another.
///
/// Every field maps to one `redis-server` command-line flag, and every fixture
/// below is one of these plus a name. Constructed through the `stock`/`durable`
/// constructors and the `with_*` builders rather than by literal, so a new field
/// gets a default at every existing call site instead of a compile error at seven
/// of them.
#[derive(Debug, Clone)]
pub struct RedisRecipe {
    /// Image tag. [`REDIS_TAG`] for everything except `RD-SPEC-014`'s negative
    /// half.
    pub tag: &'static str,
    /// `--notify-keyspace-events`. `None` leaves the server at its default of
    /// *no* notifications, which is what `RD-SPEC-005`/`005b` and `RD-LOCK-009`
    /// need.
    pub keyspace_flags: Option<&'static str>,
    /// `--appendonly yes --appendfsync <this>`. `None` means no AOF at all, the
    /// stock posture. `Some("always")` is the only value that can reach a
    /// `Linearizable` declaration (DESIGN.md §3.6).
    pub appendfsync: Option<&'static str>,
    /// `--maxmemory`. `None` means unlimited, so nothing is ever evicted.
    pub maxmemory: Option<&'static str>,
    /// `--maxmemory-policy`. `None` leaves the server default (`noeviction`),
    /// which is the one policy this plugin rates safe.
    pub maxmemory_policy: Option<&'static str>,
    /// An extra ACL user, as `(username, password)`, granted everything **except**
    /// `CONFIG` — see [`RedisRecipe::with_config_denied_user`].
    ///
    /// The password is owned rather than `&'static str` because it is generated
    /// per container rather than written here as a literal.
    pub config_denied_user: Option<(&'static str, String)>,
}

impl RedisRecipe {
    /// Stock Redis: no AOF, no memory ceiling, keyspace notifications on.
    ///
    /// `--save ""` disables RDB snapshotting. Not thrift: a background save
    /// forks the server and can add tens of milliseconds of latency at an
    /// arbitrary moment, which would show up as flake in the scenarios that
    /// assert a wake landed in single-digit milliseconds (`RD-LOCK-003`).
    #[must_use]
    pub fn stock() -> Self {
        Self {
            tag: REDIS_TAG,
            keyspace_flags: Some(KEYSPACE_FLAGS),
            appendfsync: None,
            maxmemory: None,
            maxmemory_policy: None,
            config_denied_user: None,
        }
    }

    /// The one configuration ADR-009 rates safe: a single node with
    /// `appendonly yes` and `appendfsync always` and no replicas, so nothing is
    /// acknowledged before it is fsynced and the cache declares `Linearizable`.
    #[must_use]
    pub fn durable() -> Self {
        Self {
            appendfsync: Some("always"),
            ..Self::stock()
        }
    }

    /// No keyspace notifications at all — the degradation path.
    #[must_use]
    pub fn without_keyspace_notifications(mut self) -> Self {
        self.keyspace_flags = None;
        self
    }

    /// A `maxmemory-policy` that can evict this plugin's keys.
    #[must_use]
    pub fn with_maxmemory_policy(mut self, policy: &'static str) -> Self {
        self.maxmemory_policy = Some(policy);
        self
    }

    /// A memory ceiling low enough that writing past it forces real eviction.
    #[must_use]
    pub fn with_maxmemory(mut self, maxmemory: &'static str) -> Self {
        self.maxmemory = Some(maxmemory);
        self
    }

    /// A different `appendfsync` — `RD-SPEC-011` needs `everysec` specifically,
    /// to contradict a `durability: fsync_always` hint.
    #[must_use]
    pub fn with_appendfsync(mut self, appendfsync: &'static str) -> Self {
        self.appendfsync = Some(appendfsync);
        self
    }

    /// A different image tag.
    #[must_use]
    pub fn with_tag(mut self, tag: &'static str) -> Self {
        self.tag = tag;
        self
    }

    /// Adds an ACL user granted every command **except** `CONFIG`, so a plugin
    /// connecting as it can read `INFO` but cannot read `CONFIG GET`.
    ///
    /// The local stand-in for a managed Redis (`ElastiCache`, `MemoryDB`) that hides
    /// `CONFIG` from its tenants — the environment DESIGN.md §3.6's
    /// asserted-not-verified branch exists for, and which `RD-SPEC-011`'s second
    /// half needs. TESTING §8 already records that this is an approximation of a
    /// managed instance rather than the thing itself; it is close because all the
    /// plugin cares about is that the command errors.
    ///
    /// Declared as a **server flag** rather than set with `ACL SETUSER`, which
    /// matters for more than tidiness: `ACL` sits on `fred`'s `i-acl` interface,
    /// which DESIGN.md §3.1 deliberately leaves out of the feature list, so no build of this
    /// plugin — or of its tests — can issue one.
    ///
    /// `password` is owned and expected to be generated per container — see
    /// [`throwaway_password`], and [`start_redis_config_denied_with`] for the one
    /// caller.
    #[must_use]
    pub fn with_config_denied_user(mut self, username: &'static str, password: String) -> Self {
        self.config_denied_user = Some((username, password));
        self
    }

    /// The `redis-server` argument vector this recipe describes.
    fn command(&self) -> Vec<String> {
        // The official image's entrypoint is `docker-entrypoint.sh`, so the
        // command has to name `redis-server` itself rather than only its flags.
        let mut command = vec![
            "redis-server".to_owned(),
            "--save".to_owned(),
            String::new(),
        ];
        if let Some(flags) = self.keyspace_flags {
            command.push("--notify-keyspace-events".to_owned());
            command.push(flags.to_owned());
        }
        if let Some(appendfsync) = self.appendfsync {
            command.push("--appendonly".to_owned());
            command.push("yes".to_owned());
            command.push("--appendfsync".to_owned());
            command.push(appendfsync.to_owned());
        }
        if let Some(maxmemory) = self.maxmemory {
            command.push("--maxmemory".to_owned());
            command.push(maxmemory.to_owned());
        }
        if let Some(policy) = self.maxmemory_policy {
            command.push("--maxmemory-policy".to_owned());
            command.push(policy.to_owned());
        }
        if let Some((username, password)) = &self.config_denied_user {
            // `~*` all keys, `&*` all pub/sub channels, `+@all` every command,
            // then `-config` to take that one back. `&*` is not optional here:
            // without it the user cannot subscribe, and this plugin opens a
            // subscriber before it ever reaches the durability check.
            command.extend([
                "--user".to_owned(),
                (*username).to_owned(),
                "on".to_owned(),
                format!(">{password}"),
                "~*".to_owned(),
                "&*".to_owned(),
                "+@all".to_owned(),
                "-config".to_owned(),
            ]);
        }
        command
    }
}

/// Starts a container from `recipe` and returns it with a `redis://` URL pointed
/// at its mapped host port.
///
/// Retries the whole create → `.start()` → `get_host_port_ipv4` sequence up to 5
/// times with backoff, for the reason the Postgres plugin's fixture records: under
/// parallel test load the container's own wait strategy passes but the follow-up
/// Docker port inspect transiently fails, and a fresh container per attempt
/// side-steps that flake. Every fixture below goes through here, so all of them
/// inherit the retry rather than one of them having it.
///
/// The container is returned rather than kept alive internally because the caller
/// has to outlive it: dropping a `ContainerAsync` removes the container, so a
/// fixture that dropped it would hand back a URL pointing at nothing.
pub async fn start(recipe: &RedisRecipe) -> (ContainerAsync<Redis>, String) {
    // 250ms, 500ms, 1s, 2s between the 5 attempts (the last has no sleep).
    let backoffs = [
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ];
    let attempts = backoffs.len() + 1;
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        let request = Redis::default()
            .with_tag(recipe.tag)
            .with_cmd(recipe.command());
        let container = match request.start().await {
            Ok(container) => container,
            Err(error) => {
                last_error = format!("container start: {error}");
                if let Some(backoff) = backoffs.get(attempt - 1) {
                    tokio::time::sleep(*backoff).await;
                }
                continue;
            }
        };
        match container.get_host_port_ipv4(REDIS_PORT).await {
            Ok(port) => return (container, format!("redis://127.0.0.1:{port}")),
            Err(error) => {
                last_error = format!("mapped host port: {error}");
                // Drop the failed container before backing off, so a retry does
                // not leave one running behind it.
                drop(container);
                if let Some(backoff) = backoffs.get(attempt - 1) {
                    tokio::time::sleep(*backoff).await;
                }
            }
        }
    }
    panic!(
        "Redis container acquisition failed after {attempts} attempts; last error: {last_error}"
    );
}

/// Retries container acquisition for the two multi-node fixtures the way [`start`]
/// does for the single-node ones — kept in one copy for the same reason, and
/// carrying one hazard the single-node path does not: the ports come from
/// [`free_port_base`], whose probe-then-release leaves a window a parallel process
/// can claim a port in (documented there), and the 1:1 mapping turns a lost race
/// into a failed `request.start()` rather than a slow reconnect. A fresh probe per
/// attempt makes that recoverable, so neither multi-node fixture is a single roll
/// of the port dice — which matters more here than for the single-node shapes,
/// because these fixtures **bypassed** `start`'s retry entirely before.
///
/// `build(base)` returns the container's shell command and the host ports to map
/// 1:1 for that base; the caller runs its own topology wait on the returned
/// container and derives its ports from the returned base the same way `build`
/// did. `port_count` is what to hand [`free_port_base`], not the length of the
/// mapped-port list (the Cluster fixture maps two ports per node).
async fn start_multinode(
    port_count: u16,
    build: impl Fn(u16) -> (String, Vec<u16>),
) -> (ContainerAsync<Redis>, u16) {
    let backoffs = [
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ];
    let attempts = backoffs.len() + 1;
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        // A fresh base per attempt: retrying the same ports would just lose the
        // same race, so re-probing is the whole point.
        let base = free_port_base(port_count);
        let (script, ports) = build(base);
        let mut request = Redis::default()
            .with_tag(REDIS_TAG)
            // The official image's `docker-entrypoint.sh` prepends `redis-server`
            // only when the first argument is a flag or a `.conf`, and `exec "$@"`s
            // anything else — so a shell command passes through untouched.
            .with_cmd(["sh".to_owned(), "-c".to_owned(), script])
            .with_startup_timeout(Duration::from_secs(90));
        for port in ports {
            request = request.with_mapped_port(port, ContainerPort::Tcp(port));
        }
        match request.start().await {
            Ok(container) => return (container, base),
            Err(error) => {
                last_error = format!("multi-node container start: {error}");
                if let Some(backoff) = backoffs.get(attempt - 1) {
                    tokio::time::sleep(*backoff).await;
                }
            }
        }
    }
    panic!(
        "multi-node Redis container acquisition failed after {attempts} attempts; \
         last error: {last_error}"
    );
}

/// Builds a [`RedisClusterConfig`] from `url` plus `overrides` (a JSON object
/// merged over the minimal required fields).
///
/// Goes through `serde_json` rather than a `Default` impl because
/// `RedisClusterConfig` deliberately does not derive `Default` — `url` has no
/// sensible default — so this is the test-only equivalent of the
/// `..RedisClusterConfig::default()` spread TESTING §4.1 sketches. It also means
/// every fixture config travels the same `serde` path an operator's YAML does,
/// including `deny_unknown_fields`: a typo in an override below fails here rather
/// than being silently ignored.
///
/// `pool_size` is left at the plugin's own default of 4. Nothing in this plugin
/// holds a pool connection for the life of anything — a held lock is a key with a
/// TTL, not a pinned connection (DESIGN.md §3.3) — so unlike the Postgres
/// fixture there is no per-scenario connection arithmetic to do.
pub fn cluster_config_json(url: &str, overrides: serde_json::Value) -> RedisClusterConfig {
    let mut base = json!({ "url": url });
    merge(&mut base, overrides);
    serde_json::from_value(base).expect("valid RedisClusterConfig")
}

/// A [`RedisLockConfig`] against `url` with `overrides` merged over the shared
/// defaults, for the standalone lock-only path (DESIGN.md §3.5).
pub fn lock_config_json(url: &str, overrides: serde_json::Value) -> RedisLockConfig {
    let mut base = json!({ "url": url });
    merge(&mut base, overrides);
    serde_json::from_value(base).expect("valid RedisLockConfig")
}

/// Shallow object merge — sufficient for the flat config shapes here.
fn merge(base: &mut serde_json::Value, overrides: serde_json::Value) {
    let (serde_json::Value::Object(base_map), serde_json::Value::Object(override_map)) =
        (base, overrides)
    else {
        return;
    };
    for (key, value) in override_map {
        base_map.insert(key, value);
    }
}

/// The default fixture: stock Redis, no AOF. Declares `EventuallyConsistent`
/// (DESIGN.md §3.6), which makes it the fixture most of §4.2–§4.5 runs on and the
/// one `RD-SPEC-001` asserts the weak declaration against.
pub async fn start_redis() -> (ContainerAsync<Redis>, RedisClusterConfig) {
    start_redis_with(json!({})).await
}

/// [`start_redis`] with `RedisClusterConfig` field overrides, for scenarios that
/// need a non-default value (`command_timeout_ms`, `watch_mode`, `database`, …).
pub async fn start_redis_with(
    overrides: serde_json::Value,
) -> (ContainerAsync<Redis>, RedisClusterConfig) {
    let (container, url) = start(&RedisRecipe::stock()).await;
    let config = cluster_config_json(&url, overrides);
    (container, config)
}

/// Single node, `--appendonly yes --appendfsync always`, no replicas — the one
/// configuration ADR-009 rates safe, so the cache declares `Linearizable`.
///
/// Required by the leader conformance suite (TESTING §3) and by `RD-SPEC-003`:
/// `CasBasedLeaderElectionBackend::new` is the strict constructor and refuses an
/// `EventuallyConsistent` cache, so on the default fixture the leader suite would
/// fail to *construct* rather than fail a scenario.
pub async fn start_redis_durable() -> (ContainerAsync<Redis>, RedisClusterConfig) {
    start_redis_durable_with(json!({})).await
}

/// [`start_redis_durable`] with field overrides.
pub async fn start_redis_durable_with(
    overrides: serde_json::Value,
) -> (ContainerAsync<Redis>, RedisClusterConfig) {
    let (container, url) = start(&RedisRecipe::durable()).await;
    let config = cluster_config_json(&url, overrides);
    (container, config)
}

/// Stock Redis with `notify-keyspace-events` left empty — the degradation path
/// (`RD-SPEC-005`, `RD-SPEC-005b`, `RD-LOCK-009`).
///
/// **`RD-SPEC-005b` must not share this container with `RD-SPEC-005`.** It is the
/// one scenario that issues `CONFIG SET notify-keyspace-events`, and that setting
/// is server-wide and outlives the test — so a shared container would make the
/// pair order-dependent, with `RD-SPEC-005`'s "the flags are absent" assertion
/// passing or failing depending on which ran first. Each takes its own.
pub async fn start_redis_no_notifications() -> (ContainerAsync<Redis>, RedisClusterConfig) {
    start_redis_no_notifications_with(json!({})).await
}

/// [`start_redis_no_notifications`] with field overrides — `RD-SPEC-005b` needs
/// `manage_keyspace_notifications: true`.
pub async fn start_redis_no_notifications_with(
    overrides: serde_json::Value,
) -> (ContainerAsync<Redis>, RedisClusterConfig) {
    let (container, url) = start(&RedisRecipe::stock().without_keyspace_notifications()).await;
    let config = cluster_config_json(&url, overrides);
    (container, config)
}

/// Stock Redis with `--maxmemory-policy allkeys-lru` and **no** `maxmemory`, so
/// the policy is unsafe but nothing is ever actually evicted (`RD-SPEC-006`).
///
/// The two halves are deliberately separate fixtures: `RD-SPEC-006` is about the
/// startup WARN firing on a policy that *could* evict, and keeping the ceiling
/// off means it asserts that without also being at the mercy of a real eviction
/// landing mid-test.
pub async fn start_redis_unsafe_policy() -> (ContainerAsync<Redis>, RedisClusterConfig) {
    let (container, url) = start(&RedisRecipe::stock().with_maxmemory_policy("allkeys-lru")).await;
    let config = cluster_config_json(&url, json!({}));
    (container, config)
}

/// Stock Redis with a 3 MiB ceiling and `allkeys-lru`, so writing past it forces
/// a real eviction of this plugin's keys (`RD-SPEC-007`).
///
/// 3 MiB rather than something tighter because Redis needs headroom for its own
/// overhead before it will accept any write at all; verified against a container
/// that ~16 KiB of filler values reliably pushes a small watched key out.
pub async fn start_redis_evicting() -> (ContainerAsync<Redis>, RedisClusterConfig) {
    let (container, url) = start(
        &RedisRecipe::stock()
            .with_maxmemory("3mb")
            .with_maxmemory_policy("allkeys-lru"),
    )
    .await;
    let config = cluster_config_json(&url, json!({}));
    (container, config)
}

/// A single node with `appendonly yes` but `appendfsync everysec` — durable
/// enough to look it, weak enough that a `durability: fsync_always` hint is a
/// falsehood the plugin must refuse (`RD-SPEC-011`).
pub async fn start_redis_everysec_with(
    overrides: serde_json::Value,
) -> (ContainerAsync<Redis>, RedisClusterConfig) {
    let (container, url) = start(&RedisRecipe::durable().with_appendfsync("everysec")).await;
    let config = cluster_config_json(&url, overrides);
    (container, config)
}

/// A fresh credential for one throwaway container's ACL user.
///
/// Generated rather than written as a literal, and the reason is tooling as much
/// as hygiene. Nothing depends on the value — it is created for a container that
/// is removed when the test's `ContainerAsync` drops, and it never leaves the
/// Docker bridge — but a string literal assigned to something named `PASSWORD` is
/// indistinguishable, to `CodeQL`'s hard-coded-credential rule and to every secret
/// scanner, from a real credential committed to the repository. Generating it
/// removes the finding truthfully instead of suppressing it, and costs one UUID
/// per container.
///
/// Hex, via `Uuid::simple`, so the value needs no escaping in either place it is
/// interpolated: a `redis://user:password@host` URL and a `>password` argument to
/// the server's `--user` ACL flag.
fn throwaway_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A single node with `appendfsync everysec` whose `CONFIG` is denied to the
/// connecting user — `RD-SPEC-011`'s second half.
///
/// The URL carries the restricted user's credentials, so the plugin connects as
/// it and finds `CONFIG GET` refused. With durability unreadable, a
/// `durability: fsync_always` hint cannot be contradicted, so it is trusted and
/// the declaration is flagged as asserted-not-verified rather than failing
/// startup (DESIGN.md §3.6).
pub async fn start_redis_config_denied_with(
    overrides: serde_json::Value,
) -> (ContainerAsync<Redis>, RedisClusterConfig) {
    const USER: &str = "limited";
    let password = throwaway_password();
    let (container, url) = start(
        &RedisRecipe::durable()
            .with_appendfsync("everysec")
            .with_config_denied_user(USER, password.clone()),
    )
    .await;
    // `redis://user:password@host:port` — the plugin connects as the restricted
    // user rather than as `default`, which is the whole point.
    let authed = url.replace("redis://", &format!("redis://{USER}:{password}@"));
    let config = cluster_config_json(&authed, overrides);
    (container, config)
}

/// A Redis 6 container — `RD-SPEC-014`'s negative half, where neither the
/// sharded-pub/sub DEBUG nor any `SPUBLISH`/`SSUBSCRIBE` may appear.
pub async fn start_redis_6() -> (ContainerAsync<Redis>, RedisClusterConfig) {
    let (container, url) = start(&RedisRecipe::stock().with_tag(REDIS_6_TAG)).await;
    let config = cluster_config_json(&url, json!({}));
    (container, config)
}

/// A stock container returning a [`RedisLockConfig`], for the standalone
/// lock-only plugin (DESIGN.md §3.5).
pub async fn start_redis_lock_only() -> (ContainerAsync<Redis>, RedisLockConfig) {
    start_redis_lock_only_with(json!({})).await
}

/// [`start_redis_lock_only`] with field overrides.
pub async fn start_redis_lock_only_with(
    overrides: serde_json::Value,
) -> (ContainerAsync<Redis>, RedisLockConfig) {
    let (container, url) = start(&RedisRecipe::stock()).await;
    let config = lock_config_json(&url, overrides);
    (container, config)
}

/// A memory-capped, evicting container returning a [`RedisLockConfig`] **and its
/// URL** — `RD-LOCK-015`'s fixture.
///
/// The URL comes back because this plugin owns no cache to write filler through:
/// the memory pressure that forces a real eviction has to come from a raw client
/// rather than from the thing under test.
pub async fn start_redis_evicting_lock_only() -> (ContainerAsync<Redis>, RedisLockConfig, String) {
    let (container, url) = start(
        &RedisRecipe::stock()
            .with_maxmemory("3mb")
            .with_maxmemory_policy("allkeys-lru"),
    )
    .await;
    let config = lock_config_json(&url, json!({}));
    (container, config, url)
}

/// A stock container with **no** keyspace notifications, returning a
/// [`RedisLockConfig`] — `RD-LOCK-009`'s subject: the standalone lock needs none
/// of them, because a release is a `PUBLISH` the plugin issues itself rather than
/// a notification the server emits.
pub async fn start_redis_lock_only_no_notifications() -> (ContainerAsync<Redis>, RedisLockConfig) {
    let (container, url) = start(&RedisRecipe::stock().without_keyspace_notifications()).await;
    let config = lock_config_json(&url, json!({}));
    (container, config)
}

/// A per-scenario [`RedisClusterConfig`] on a shared container, isolated **both**
/// ways TESTING §3 describes: its own logical database and its own `key_prefix`.
///
/// §3 prefers database isolation and calls prefix isolation the fallback, on the
/// grounds that a shared prefix space leaves a scenario able to see another's
/// keys through `scan_prefix`. Doing both is strictly stronger than either and
/// costs nothing, and it is what makes the two suites that need more than 15
/// scenarios work on one container rather than one container per scenario.
///
/// The database cycles `1..=15` rather than starting at 0 so nothing lands in the
/// database a stray client would connect to by default, and so a scenario's
/// `expired` notifications arrive on `__keyspace@<n>__` with `n` non-zero — the
/// off-by-one-database bug `RD-SPEC-012` exists to catch would otherwise be
/// invisible here.
pub fn cluster_config_for_scenario(
    url: &str,
    suite: &str,
    index: usize,
    overrides: serde_json::Value,
) -> RedisClusterConfig {
    let mut base = json!({
        "database": database_for_scenario(index),
        "key_prefix": format!("{suite}{index}"),
    });
    merge(&mut base, overrides);
    cluster_config_json(url, base)
}

/// A per-scenario [`RedisLockConfig`] on a shared container, isolated the same
/// two ways as [`cluster_config_for_scenario`].
///
/// Both halves matter for the lock suite specifically: the conformance scenarios
/// reuse a handful of lock names (`"res"`, `"m"`), so a lease still held past one
/// scenario's teardown would collide with the next — and `stop()` deliberately
/// *leaves* held leases to expire (`cpt-cf-clst-fr-shutdown-ttl-cleanup`,
/// `RD-LOCK-013`), so there is no handback to rely on the way the Postgres plugin
/// now can.
pub fn lock_config_for_scenario(
    url: &str,
    suite: &str,
    index: usize,
    overrides: serde_json::Value,
) -> RedisLockConfig {
    let mut base = json!({
        "database": database_for_scenario(index),
        "key_prefix": format!("{suite}{index}"),
    });
    merge(&mut base, overrides);
    lock_config_json(url, base)
}

/// Cycles a scenario index over the 15 non-default logical databases a stock
/// Redis provides (`databases 16`, indices 0–15).
fn database_for_scenario(index: usize) -> u8 {
    u8::try_from(index % 15 + 1).expect("a value in 1..=15 fits a u8")
}

/// A bare `fred` client against the same container, for assertions on
/// Redis-level state (`HGETALL`, `PTTL`, `INFO commandstats`, `PUBSUB NUMPAT`,
/// `CONFIG GET`) rather than only through the plugin's own API.
///
/// Carries the same short command timeout the plugin's own pool does, so a raw
/// assertion against a paused container fails the test instead of hanging it.
/// Callers that assert on *connection counts* (`RD-LIFE-003`) must
/// [`quit`](ClientLike::quit) theirs first, since this one is a connection too.
pub async fn raw_client(url: &str) -> Client {
    raw_client_on(url, 0).await
}

/// [`raw_client`], on a specific logical database — needed by every scenario
/// whose config is a [`cluster_config_for_scenario`], since those do not run on
/// database 0.
pub async fn raw_client_on(url: &str, database: u8) -> Client {
    let mut config = Config::from_url(url).expect("the fixture url parses");
    if database != 0 {
        config.database = Some(database);
    }
    let client = Builder::from_config(config)
        .with_performance_config(|perf| {
            perf.default_command_timeout = Duration::from_secs(5);
        })
        .build()
        .expect("a raw client builds from the fixture url");
    client.init().await.expect("the raw client connects");
    client
}

/// The server's current `notify-keyspace-events` value.
///
/// Read back through `CONFIG GET` rather than assumed from the recipe, because
/// two scenarios turn on the difference: `RD-SPEC-005` asserts the flags are
/// absent and `RD-SPEC-005b` asserts the plugin's one `CONFIG SET` put them
/// there.
pub async fn keyspace_flags(client: &Client) -> String {
    use fred::interfaces::ConfigInterface;
    let reply: Vec<String> = client
        .config_get("notify-keyspace-events")
        .await
        .expect("CONFIG GET notify-keyspace-events succeeds");
    reply.get(1).cloned().unwrap_or_default()
}

/// How many times `command` has been dispatched on this server, from
/// `INFO commandstats`.
///
/// The mechanism behind every "this command was never issued" assertion
/// (`RD-CACHE-006`'s no-`KEYS`, `RD-WATCH-010`'s no-`PUBLISH`, `RD-SPEC-014`'s
/// no-`SPUBLISH`). `0` covers both "the counter exists and is zero" and "the
/// command has never been called, so there is no line for it", which are the same
/// statement.
///
/// `INFO commandstats` lines are `cmdstat_<command>:calls=N,usec=…`, and the
/// command name is lower-cased with multi-word commands joined by `|`
/// (`cmdstat_config|set`). Counting is server-wide and cumulative, so a caller
/// asserting an *increase* must read a baseline first — asserting an absolute
/// count would also be counting the plugin's own startup traffic.
pub async fn command_calls(client: &Client, command: &str) -> u64 {
    use fred::types::InfoKind;
    let info: String = client
        .info(Some(InfoKind::CommandStats))
        .await
        .expect("INFO commandstats succeeds");
    let needle = format!("cmdstat_{}:", command.to_lowercase());
    for line in info.lines() {
        let Some(rest) = line.trim().strip_prefix(&needle) else {
            continue;
        };
        for field in rest.split(',') {
            if let Some(calls) = field.strip_prefix("calls=") {
                return calls.parse().unwrap_or(0);
            }
        }
    }
    0
}

/// Polls `condition` until it returns `true` or `timeout` elapses, sleeping
/// `interval` between attempts.
///
/// Used instead of a single fixed sleep wherever the assertion is about state
/// that changes asynchronously — a pub/sub delivery, a TTL lapse, an eviction —
/// so a scenario neither hard-codes a latency bound tighter than it is about nor
/// pays a worst-case sleep on every run.
///
/// The whole wait, including each in-flight `condition().await`, is bounded by
/// `timeout`: checking the deadline only between attempts would let one stalled
/// command inside `condition` overrun it indefinitely.
pub async fn wait_until<F>(timeout: Duration, interval: Duration, mut condition: F) -> bool
where
    F: AsyncFnMut() -> bool,
{
    tokio::time::timeout(timeout, async {
        loop {
            if condition().await {
                return true;
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// An in-memory OpenTelemetry reader plus the [`Meter`] to inject into a plugin
/// builder's `__with_meter` seam.
///
/// `__with_meter` routes **both** halves of the plugin's telemetry through one
/// meter — the ADR-004 catalog signals via `OtelClusterMetrics::new` and the four
/// plugin-local instruments directly — so a single reader observes everything the
/// plugin emits. That is what lets `RD-WATCH-008`, `RD-SPEC-007` and `RD-LIFE-008`
/// assert on counters by their contracted names instead of by eye.
///
/// [`Meter`]: opentelemetry::metrics::Meter
pub fn in_memory_meter() -> (opentelemetry::metrics::Meter, MetricReadback) {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let meter = provider.meter("redis-cluster-plugin-integration");
    (meter, MetricReadback { provider, exporter })
}

/// Reads the plugin's instruments back out of an in-process exporter.
///
/// The crate's own `src/test_support.rs` has a Layer-1 equivalent, but it is
/// `#[cfg(test)]` on the library and an integration test binary links the library
/// as a normal dependency — so it cannot see it, and the readback is re-expressed
/// here.
///
/// # Read the last batch, never the sum of all of them
///
/// `InMemoryMetricExporter` **accumulates** one `ResourceMetrics` batch per flush
/// and each batch carries the counter's full cumulative value. So summing data
/// points across batches multiplies the answer by the number of times it has been
/// read: a counter genuinely at 1, read three times, reports 6. Both readers below
/// therefore take the *last* batch that mentions the instrument.
pub struct MetricReadback {
    provider: opentelemetry_sdk::metrics::SdkMeterProvider,
    exporter: opentelemetry_sdk::metrics::InMemoryMetricExporter,
}

impl MetricReadback {
    /// The current value of the `u64` counter whose *contract* name is `name`.
    ///
    /// The `_total` suffix is stripped before matching: the Prometheus exporter
    /// appends it when it renders a counter, so the instrument is registered
    /// without it and `cluster_cache_ops_total` is the instrument
    /// `cluster_cache_ops`. Passing either form works, which keeps a call site
    /// free to quote the name a dashboard would.
    ///
    /// Flushes first, so the readback is deterministic rather than dependent on
    /// the periodic reader's timer.
    pub fn counter(&self, name: &str) -> u64 {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        let name = name.strip_suffix("_total").unwrap_or(name);
        let _flushed = self.provider.force_flush();
        let Ok(collected) = self.exporter.get_finished_metrics() else {
            return 0;
        };
        let mut latest = 0;
        for resource in &collected {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() == name
                        && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                    {
                        // Within one batch, summing across data points is right:
                        // they are distinct label sets, not repeated readings.
                        latest = sum
                            .data_points()
                            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                            .sum::<u64>();
                    }
                }
            }
        }
        latest
    }

    /// The most recent value of the `u64` gauge named `name`, or `None` if it has
    /// never been recorded.
    ///
    /// `None` and `Some(0)` are genuinely different here:
    /// `cluster_redis_connection_state` records `0` on the way out of `stop()`, so
    /// "never sampled" and "sampled as disconnected" are not the same fact.
    pub fn gauge(&self, name: &str) -> Option<u64> {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        let _flushed = self.provider.force_flush();
        let collected = self.exporter.get_finished_metrics().ok()?;
        let mut latest = None;
        for resource in &collected {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() == name
                        && let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data()
                        && let Some(point) = gauge.data_points().last()
                    {
                        latest = Some(point.value());
                    }
                }
            }
        }
        latest
    }

    /// Every instrument name the exporter has seen, for the "nothing is emitted
    /// under an uncontracted name" half of ADR-004's naming rule.
    pub fn instrument_names(&self) -> std::collections::BTreeSet<String> {
        let _flushed = self.provider.force_flush();
        let mut names = std::collections::BTreeSet::new();
        if let Ok(collected) = self.exporter.get_finished_metrics() {
            for resource in &collected {
                for scope in resource.scope_metrics() {
                    for metric in scope.metrics() {
                        names.insert(metric.name().to_owned());
                    }
                }
            }
        }
        names
    }
}

/// A `tracing` writer that appends to a shared buffer, so a test can grep what
/// was logged.
#[derive(Clone)]
pub struct SharedWriter(pub Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("the log capture mutex is never poisoned in tests")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs a **process-global** capture at `level` once per test binary and
/// returns its buffer.
///
/// Global rather than thread-local, deliberately and for a reason
/// `tracing-test` cannot serve: several of the events these suites assert on are
/// emitted from **spawned tasks** — `cluster.provider.eviction_observed` and
/// `cluster.watch.reset` come off the subscriber fan-out, `pool_close_timeout`
/// off a shutdown path — and a thread-local subscriber never sees those. The
/// Postgres plugin's `install_global_warn_capture` is the precedent.
///
/// The cost is that the buffer is shared by every test in the binary, so it can
/// only support "this event appeared" assertions, never "this event did not".
/// Use [`scoped_capture`] for the absence half, and give it its own container.
pub fn install_global_capture(level: tracing::Level) -> Arc<Mutex<Vec<u8>>> {
    use std::sync::OnceLock;
    static BUF: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    Arc::clone(BUF.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(SharedWriter(Arc::clone(&buf)))
            .with_max_level(level)
            // No ANSI: this buffer is searched with `count_occurrences`, not
            // read by a terminal, and the styling wraps *field names* in escape
            // codes. `primitive="lock"` then stops being a contiguous substring
            // while still looking like one in any failure message that prints
            // the buffer, which is a false negative that reads as a real one.
            .with_ansi(false)
            .finish();
        // Ignore an already-installed global: nothing else in these binaries
        // sets one, and losing the race would only mean a second caller reuses
        // the first's buffer, which is what `OnceLock` already arranges.
        let _installed = tracing::subscriber::set_global_default(subscriber);
        buf
    }))
}

/// Installs a **thread-local** capture at `level` for the current test, returning
/// its uninstall guard and buffer.
///
/// `set_default`, not `set_global_default`, so this test's capture is isolated
/// from every other test's plugin — which is what makes asserting the *absence*
/// of an event possible at all (`RD-SPEC-003`'s "no `weak_consistency` WARN",
/// `RD-SPEC-005b`'s "no `expiry_events_unavailable`"). `#[tokio::test]` runs a
/// current-thread runtime, so anything emitted inline by `build_and_start` lands
/// on this thread and is captured; anything emitted from a spawned task will not
/// be, which is the trade [`install_global_capture`] exists for.
///
/// # Why this installs a process-global sink first
///
/// A thread-local subscriber alone is **not** sufficient, and the way it fails is
/// silent: `tracing` caches each callsite's interest *process-wide* the first time
/// it is evaluated. In a test binary running several tests in parallel, another
/// test's plugin can reach a `warn!` callsite first, on a thread with no
/// subscriber installed — the callsite is then cached as `Interest::never` and
/// every later evaluation short-circuits **before** consulting the thread-local
/// dispatcher. The capture comes back empty and the test fails claiming the event
/// was not emitted, when it was.
///
/// This is not hypothetical: it is what `rd_life_startup_events` did on its first
/// run, passing in isolation and failing in the full binary, at every level.
/// Installing a global sink first (idempotent, and its buffer is discarded) forces
/// the callsite to be enabled process-wide — `set_global_default` rebuilds the
/// interest cache — after which the thread-local default receives events normally
/// on the thread that installed it.
pub fn scoped_capture(
    level: tracing::Level,
) -> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<u8>>>) {
    // Discarded: this exists only so the callsites are interesting. TRACE rather
    // than `level`, because the global is installed once per binary and a later
    // caller asking for a more verbose level would otherwise inherit the first
    // caller's ceiling.
    let _global = install_global_capture(tracing::Level::TRACE);
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(SharedWriter(Arc::clone(&buf)))
        .with_max_level(level)
        // See `install_global_capture`: styled field names are not searchable.
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, buf)
}

/// How many times `needle` appears in a capture buffer.
///
/// Every §9 log event carries its catalogued name **twice** — as the structural
/// `name:` field and opening the human message — so passing an event constant
/// from `redis_cluster_plugin::logs` matches the message half, which is what the
/// default `fmt` layer prints. That is why these assertions can be a substring
/// count rather than needing a layer that reads `event.metadata().name()`.
pub fn count_occurrences(buf: &Arc<Mutex<Vec<u8>>>, needle: &str) -> usize {
    let bytes = buf
        .lock()
        .expect("the log capture mutex is never poisoned in tests");
    String::from_utf8_lossy(&bytes).matches(needle).count()
}

/// The whole capture buffer as a string, for assertion messages that should show
/// what *was* logged when the expected line is missing.
pub fn captured(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = buf
        .lock()
        .expect("the log capture mutex is never poisoned in tests");
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---------------------------------------------------------------------------
// Multi-node topologies (TESTING.md §4.1)
// ---------------------------------------------------------------------------

/// How many consecutive free ports [`free_port_base`] looks for, and the offset
/// at which the Cluster bus ports live.
///
/// Redis defaults a node's bus port to `port + 10000`, and the fixtures keep
/// that convention rather than setting `cluster-port`, so a reader comparing the
/// fixture against `CLUSTER NODES` output sees the arithmetic they expect.
const CLUSTER_BUS_OFFSET: u16 = 10_000;

/// Finds `count` consecutive free TCP ports on the loopback interface, returning
/// the base.
///
/// ## Why the fixtures need this at all
///
/// Both multi-node topologies **advertise an address to the client**: Sentinel
/// answers `get-master-addr-by-name` with the primary's address, and a Cluster
/// node answers a wrong-slot command with `MOVED <slot> <addr>`. The client then
/// has to connect to that address. With testcontainers' usual random port
/// mapping the address a node knows about (its container port) is not the address
/// the host can reach (the mapped port), so every redirect points somewhere the
/// test process cannot go.
///
/// Mapping each port **1:1** removes the distinction: `127.0.0.1:7000` means the
/// same endpoint inside the container and outside it, so an advertised address is
/// correct for both audiences at once. That is why these fixtures pick their own
/// ports rather than letting Docker choose.
///
/// The ports are probed by binding and immediately dropping the listener, so
/// there is a window between the probe and the container claiming them. It is
/// small and the alternative — hard-coded ports — collides with whatever else the
/// developer is running, which is a worse failure because it is not random. A
/// lost race is no longer fatal either: [`start_multinode`] re-probes a fresh base
/// and retries, so a parallel process grabbing a port inside the window costs an
/// attempt rather than the whole fixture.
fn free_port_base(count: u16) -> u16 {
    use std::net::TcpListener;
    // A fixed window rather than an OS-assigned ephemeral port. The ephemeral
    // range starts around 49152, and every port here needs its bus port
    // `CLUSTER_BUS_OFFSET` above it, so an ephemeral base overflows `u16` far
    // more often than not.
    const WINDOW: std::ops::Range<u16> = 7_000..40_000;
    // Staggered by process id so two test binaries running side by side start
    // their search in different places rather than racing for the same base.
    let stride = count + 1;
    let span = WINDOW.end - WINDOW.start;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "deliberately reduced mod the window's span, which is a u16"
    )]
    let stagger = ((u64::from(std::process::id()) * u64::from(stride)) % u64::from(span)) as u16;
    #[expect(
        clippy::integer_division,
        reason = "a count of whole strides that fit in the window is exactly what is wanted"
    )]
    let attempts = span / stride;
    'candidate: for step in 0..attempts {
        // Widened before the sum, not after: `stagger` reaches `span - 1` and
        // `step * stride` reaches nearly the same, so the addition overflows
        // `u16` and panics in a debug build. It takes several rejected
        // candidates to get there, which is what would make it surface as an
        // intermittent fixture failure under parallel load rather than a
        // reproducible one.
        let offset = (u32::from(stagger) + u32::from(step) * u32::from(stride))
            % u32::from(span - CLUSTER_BUS_OFFSET);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "reduced mod a span that is itself a u16"
        )]
        let base = WINDOW.start + offset as u16;
        if base + CLUSTER_BUS_OFFSET + count >= WINDOW.end {
            continue;
        }
        let mut held = Vec::new();
        for offset in 0..count {
            for port in [base + offset, base + CLUSTER_BUS_OFFSET + offset] {
                match TcpListener::bind(("127.0.0.1", port)) {
                    Ok(listener) => held.push(listener),
                    Err(_) => continue 'candidate,
                }
            }
        }
        // Released here, so the container can claim them. The window between
        // this and the container binding is the fixture's one race, and it is
        // narrower than the hard-coded alternative's certainty of collision.
        drop(held);
        return base;
    }
    panic!("no run of {count} consecutive free ports was found in {WINDOW:?}");
}

/// Waits for a container's topology to finish forming, by running `probe` inside
/// it until it answers `expected`.
///
/// Run **inside** the container with `docker exec` rather than over a client from
/// the host, deliberately: this is asking whether the topology is ready, and a
/// host-side client failing to connect cannot distinguish "not ready yet" from
/// "the port mapping is wrong", which is the one failure mode worth telling
/// apart when a fixture like this misbehaves.
async fn await_topology<I: testcontainers::Image>(
    container: &ContainerAsync<I>,
    probe: &[String],
    expected: &str,
    what: &str,
) {
    let ready = wait_until(
        Duration::from_mins(1),
        Duration::from_millis(250),
        async || {
            let Ok(mut output) = container
                .exec(testcontainers::core::ExecCommand::new(
                    probe.iter().cloned(),
                ))
                .await
            else {
                return false;
            };
            let Ok(stdout) = output.stdout_to_vec().await else {
                return false;
            };
            String::from_utf8_lossy(&stdout).contains(expected)
        },
    )
    .await;
    assert!(ready, "the {what} fixture never reported `{expected}`");
}

/// A **Sentinel-managed** topology: one primary, one replica, one Sentinel, all
/// in a single container (TESTING.md §4.1).
///
/// Returns the container, a `redis-sentinel://` [`RedisClusterConfig`], and the
/// primary and replica ports, so a scenario can reach past the client to the
/// servers themselves — to read the primary's config back (`RD-SPEC-002`) or to
/// take the replica away under a running plugin (`RD-LOCK-014`).
///
/// ## Why one container and not three
///
/// Three containers on a Docker network is the shape this topology has in
/// production, and it is the wrong shape for a test on this side of the network
/// boundary: Sentinel would answer `get-master-addr-by-name` with the primary's
/// *network* address, which the test process cannot reach. Putting all three
/// processes in one container with 1:1 port mapping (see [`free_port_base`])
/// makes `127.0.0.1:<port>` mean the same thing to Sentinel, to the replica, and
/// to the test — so the address Sentinel advertises is one the client can use.
///
/// What this costs is the thing a Sentinel topology is otherwise *for*: the
/// processes share a failure domain, so killing the container kills the quorum
/// along with the primary and no failover can be observed. That is the boundary
/// between this fixture and the `RD-FAULT-005..007` failover scenarios, which
/// stay deferred (§8) — they need fault injection between separately-killable
/// nodes, and this fixture deliberately does not pretend to provide it.
///
/// `appendfsync always` on the primary is not incidental. It is what makes
/// `RD-SPEC-002` a real test: the durability reading says "safe" and the topology
/// reading says "replicated", and the declaration has to come out
/// `EventuallyConsistent` on the strength of the second alone.
pub async fn start_redis_sentinel() -> (ContainerAsync<Redis>, RedisClusterConfig, u16, u16) {
    let (container, base) = start_multinode(2, |base| {
        let (primary, replica) = (base, base + 1);
        let sentinel = base + CLUSTER_BUS_OFFSET;
        // `&` rather than `--daemonize yes`: a daemonized server detaches and
        // stops writing to the container's stdout, which is where a failed
        // fixture's diagnosis has to come from.
        //
        // The `until … PONG` wait replaces a bare `sleep 1`: Sentinel's
        // `monitor` needs the primary reachable, and a fixed second is a guess
        // that a cold image or a loaded host can exceed, which then surfaces as
        // `await_topology` burning its whole budget rather than as this readiness
        // step waiting a few hundred ms more. Sentinel retries `monitor` on its
        // own, so this only tightens an already-recoverable path — but it removes
        // the guess.
        let script = format!(
            "redis-server --port {primary} --appendonly yes --appendfsync always --save '' \
               --notify-keyspace-events {KEYSPACE_FLAGS} & \
             redis-server --port {replica} --replicaof 127.0.0.1 {primary} --save '' \
               --appendonly no & \
             until redis-cli -p {primary} ping 2>/dev/null | grep -q PONG; do sleep 0.1; done && \
             printf 'port {sentinel}\\n\
sentinel monitor {SENTINEL_SERVICE} 127.0.0.1 {primary} 1\\n\
sentinel down-after-milliseconds {SENTINEL_SERVICE} 2000\\n\
sentinel failover-timeout {SENTINEL_SERVICE} 5000\\n' > /data/sentinel.conf && \
             redis-sentinel /data/sentinel.conf"
        );
        (script, vec![primary, replica, sentinel])
    })
    .await;
    let (primary, replica) = (base, base + 1);
    let sentinel = base + CLUSTER_BUS_OFFSET;
    // The replica being *online* is the precondition every scenario here rests
    // on: `RD-SPEC-002` needs the primary to report a replica for the topology to
    // read as replicated, and `RD-LOCK-014` needs one for `WAIT` to be satisfiable
    // before it is made unsatisfiable.
    await_topology(
        &container,
        &[
            "redis-cli".to_owned(),
            "-p".to_owned(),
            primary.to_string(),
            "info".to_owned(),
            "replication".to_owned(),
        ],
        "state=online",
        "sentinel",
    )
    .await;
    await_topology(
        &container,
        &[
            "redis-cli".to_owned(),
            "-p".to_owned(),
            sentinel.to_string(),
            "sentinel".to_owned(),
            "get-master-addr-by-name".to_owned(),
            SENTINEL_SERVICE.to_owned(),
        ],
        &primary.to_string(),
        "sentinel",
    )
    .await;
    let url =
        format!("redis-sentinel://127.0.0.1:{sentinel}?sentinelServiceName={SENTINEL_SERVICE}");
    let config = cluster_config_json(&url, json!({}));
    (container, config, primary, replica)
}

/// The Sentinel service name the fixture monitors under, and that the URL's
/// `service_name` names.
pub const SENTINEL_SERVICE: &str = "mymaster";

/// How many primaries the Cluster fixture runs. Three is the minimum a Redis
/// Cluster accepts, and enough for the property under test: that a prefix's keys
/// land on more than one shard.
pub const CLUSTER_NODES: u16 = 3;

/// A **3-primary Redis Cluster**, all nodes in a single container
/// (TESTING.md §4.1).
///
/// Returns the container, a `redis-cluster://` [`RedisClusterConfig`], and the
/// three node ports.
///
/// ## Why one container, again
///
/// The same reason as [`start_redis_sentinel`] and more sharply: a cluster node
/// answers a wrong-slot command with `MOVED <slot> <ip:port>`, and the client is
/// required to follow it. Each node therefore advertises an address through
/// `cluster-announce-ip`/`cluster-announce-port`, and that one address has to
/// work for two audiences — the other nodes' gossip and the host's client.
/// Co-locating the nodes and mapping ports 1:1 is what lets `127.0.0.1:<port>`
/// satisfy both. Separate containers cannot: an address that reaches a peer
/// across the Docker network is not one the test process can dial.
///
/// No replicas. Every scenario on this fixture (`RD-SPEC-008`, `009`, `010`) is
/// about **slot routing** rather than about failover, and replicas would add
/// startup time to a per-PR fixture for a property nothing here asserts — the
/// failover scenarios are deferred to fault injection either way (§8).
pub async fn start_redis_cluster() -> (ContainerAsync<Redis>, RedisClusterConfig, Vec<u16>) {
    let (container, base) = start_multinode(CLUSTER_NODES, |base| {
        let ports: Vec<u16> = (0..CLUSTER_NODES).map(|offset| base + offset).collect();
        let servers = ports
            .iter()
            .map(|port| {
                format!(
                    "redis-server --port {port} --cluster-enabled yes \
                     --cluster-config-file node-{port}.conf --cluster-node-timeout 5000 \
                     --cluster-announce-ip 127.0.0.1 --cluster-announce-port {port} \
                     --cluster-announce-bus-port {bus} --save '' --appendonly no \
                     --notify-keyspace-events {KEYSPACE_FLAGS} &",
                    bus = port + CLUSTER_BUS_OFFSET
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let members = ports
            .iter()
            .map(|port| format!("127.0.0.1:{port}"))
            .collect::<Vec<_>>()
            .join(" ");
        // Wait for every node to answer `PING` before the one-shot
        // `cluster create`, then retry the create itself a bounded number of
        // times. The old `sleep 1` was a guess that a cold image or a loaded host
        // can exceed, and the create ran exactly once: a create issued a beat too
        // early errored with no retry, the cluster never formed, and the failure
        // showed up a minute later as `await_topology` timing out on a `MOVED`-less
        // cluster rather than here where it happened. Readiness is awaited rather
        // than slept, and a create that loses the first race gets a few more.
        let waits = ports
            .iter()
            .map(|port| {
                format!(
                    "until redis-cli -p {port} ping 2>/dev/null | grep -q PONG; do sleep 0.1; done"
                )
            })
            .collect::<Vec<_>>()
            .join(" && ");
        // `--cluster-yes` answers the interactive slot-allocation prompt. The
        // `wait` at the end keeps the container alive on the backgrounded servers.
        let script = format!(
            "{servers} {waits} && \
             tries=0; \
             until redis-cli --cluster create {members} --cluster-yes; do \
               tries=$((tries+1)); [ \"$tries\" -ge 10 ] && break; sleep 0.5; \
             done && wait"
        );
        let mut mapped = Vec::with_capacity(usize::from(CLUSTER_NODES) * 2);
        for port in &ports {
            mapped.push(*port);
            mapped.push(port + CLUSTER_BUS_OFFSET);
        }
        (script, mapped)
    })
    .await;
    let ports: Vec<u16> = (0..CLUSTER_NODES).map(|offset| base + offset).collect();
    // `cluster_state:ok` rather than merely "the process is up": until every one
    // of the 16384 slots is served, a command for an unassigned slot answers
    // `CLUSTERDOWN`, and a scenario starting into that would fail for a reason
    // that has nothing to do with what it tests.
    //
    // Asked of **every** node rather than of the one the URL names. A client
    // builds its slot map from whichever node answers first, so a node that has
    // not yet finished gossiping reports a partial cluster and `fred` rejects the
    // topology outright ("Invalid or missing cluster state"). Waiting on one node
    // makes that a race the fixture loses intermittently.
    for port in &ports {
        for marker in [
            "cluster_state:ok".to_owned(),
            format!("cluster_known_nodes:{CLUSTER_NODES}"),
        ] {
            await_topology(
                &container,
                &[
                    "redis-cli".to_owned(),
                    "-p".to_owned(),
                    port.to_string(),
                    "cluster".to_owned(),
                    "info".to_owned(),
                ],
                &marker,
                "cluster",
            )
            .await;
        }
    }
    // Every node in the URL, not only the first: `fred` seeds its slot map from
    // whichever of them answers, so naming all three means a single slow node
    // cannot stall startup.
    let nodes = ports
        .iter()
        .skip(1)
        .map(|port| format!("node=127.0.0.1:{port}"))
        .collect::<Vec<_>>()
        .join("&");
    let url = format!("redis-cluster://127.0.0.1:{}?{nodes}", ports[0]);
    let config = cluster_config_json(&url, json!({}));
    (container, config, ports)
}

/// Runs `args` inside `container` and returns its stdout.
///
/// The escape hatch the multi-node fixtures need: their nodes are processes
/// inside one container rather than addressable containers, so "ask this
/// specific node something" and "stop this specific node" are both `docker exec`
/// rather than anything the client can express. Used to read per-shard key counts
/// (`RD-SPEC-008`, `RD-SPEC-009`) and to take the replica away under a running
/// plugin (`RD-LOCK-014`).
pub async fn exec_in<I: testcontainers::Image>(
    container: &ContainerAsync<I>,
    args: &[&str],
) -> String {
    let mut output = container
        .exec(testcontainers::core::ExecCommand::new(
            args.iter().map(|arg| (*arg).to_owned()),
        ))
        .await
        .expect("the exec is accepted by the container");
    let stdout = output
        .stdout_to_vec()
        .await
        .expect("the exec's stdout is readable");
    String::from_utf8_lossy(&stdout).trim().to_owned()
}

/// How many keys each node of the Cluster fixture currently holds.
///
/// The evidence that a "spread across shards" scenario really spread: a test
/// that planted a thousand keys which all happened to land on one shard would
/// pass a per-shard scan assertion while proving nothing about the other two.
pub async fn keys_per_node<I: testcontainers::Image>(
    container: &ContainerAsync<I>,
    ports: &[u16],
) -> Vec<u64> {
    let mut counts = Vec::new();
    for port in ports {
        let raw = exec_in(container, &["redis-cli", "-p", &port.to_string(), "dbsize"]).await;
        counts.push(raw.trim().parse().unwrap_or(0));
    }
    counts
}
