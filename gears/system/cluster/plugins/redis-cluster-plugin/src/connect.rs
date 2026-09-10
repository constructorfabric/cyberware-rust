//! Opening the `fred` command pool and the subscriber client — step 1 of both
//! plugins' `build_and_start` (DESIGN.md §3.2, §3.3).
//!
//! Shared between `RedisClusterHandle` and `RedisLockHandle` for the same
//! reason `shutdown.rs` is (DESIGN.md §3.1): the two plugins open
//! exactly this pair of connections, and everything decided here is a policy
//! neither of them may differ on. The client-side command timeout is what makes
//! `stop()` finite (DESIGN.md §11, §12); awaiting the initial connect is what
//! turns an unreachable server into a startup error rather than a pool that
//! reconnects in the background while every command fails. A second copy of
//! this file would be free to drift on either, which is what happened to the
//! Postgres plugin's two shutdown paths.

use std::time::Duration;

use cluster_sdk::ClusterError;
use fred::clients::{Pool, SubscriberClient};
use fred::interfaces::ClientLike;
use fred::types::config::{Config, ReconnectPolicy, ServerConfig};
use fred::types::{Builder, ConnectHandle};

use crate::config::Topology;
use crate::redis_error::map_redis_error;
use crate::shutdown::{abandon_subscriber, close_pool};

/// Upper bound on the initial connect, so an unreachable server fails startup
/// promptly instead of spending [`RECONNECT_ATTEMPTS`]' whole schedule on it.
///
/// Needed the moment a reconnect policy exists: `init()` retries the *initial*
/// connect under the same policy, so without this bound a plugin pointed at a
/// closed port would sit in `build_and_start` for minutes rather than reporting
/// `Provider { ConnectionLost }` (`RD-LIFE-005`). Ten seconds is long enough to
/// ride out a container that is still binding its port and short enough to stay
/// inside any supervisor's start budget.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many times a connection is re-established before the client gives up.
///
/// `fred` defaults to **no policy at all** (`Builder::from_config` leaves
/// `policy: None`), which is not a viable default for this plugin: a single
/// dropped TCP connection would end the client permanently, so every subsequent
/// command would fail forever and every watcher would be closed on the first
/// blip. Everything DESIGN.md §4.3 and §10 say about recovery — `fred` replaying
/// the subscription set, the `Reset` a watcher then receives, the terminal close
/// "once the reconnect policy is exhausted" — presumes a policy that this file
/// has to supply.
///
/// **Bounded, not unlimited**, and that is the load-bearing half. Unlimited
/// retries would make `spawn_connection_watchdog` dead code — its signal is the
/// `ConnectHandle` resolving, which only ever happens when the policy gives up —
/// so a watcher waiting on a server that is never coming back would wait
/// forever, told nothing. With the schedule below the client rides out roughly
/// six minutes of outage, which covers a rolling restart or a Sentinel failover,
/// and then tells its consumers.
pub const RECONNECT_ATTEMPTS: u32 = 20;
/// The first reconnect delay, in milliseconds.
const RECONNECT_MIN_DELAY_MS: u32 = 100;
/// The ceiling on the exponential backoff, in milliseconds.
const RECONNECT_MAX_DELAY_MS: u32 = 30_000;
/// The backoff base: each attempt waits twice as long as the last, to the
/// ceiling above.
const RECONNECT_BASE: u32 = 2;

/// The policy both clients reconnect under. See [`RECONNECT_ATTEMPTS`].
#[must_use]
pub fn reconnect_policy() -> ReconnectPolicy {
    ReconnectPolicy::new_exponential(
        RECONNECT_ATTEMPTS,
        RECONNECT_MIN_DELAY_MS,
        RECONNECT_MAX_DELAY_MS,
        RECONNECT_BASE,
    )
}

/// What a plugin wants opened. A struct rather than five positional parameters
/// because three of the five are integers whose order nothing would catch.
pub struct ConnectSpec<'a> {
    /// The operator's connection URL, already `${VAR}`-expanded.
    pub url: &'a str,
    /// The logical database. Applied only when non-zero — the URL may carry one
    /// in its path, and 0 is both the default and the only legal value in
    /// cluster mode, where `fred` ignores it.
    pub database: u8,
    /// Command-pool size (DESIGN.md §3.3).
    pub pool_size: u32,
    /// The per-command client-side bound.
    pub command_timeout: Duration,
}

/// What [`connect`] established.
pub struct Connected {
    /// The connected command pool.
    pub pool: Pool,
    /// The subscriber client and the `ConnectHandle` that resolves when its
    /// reconnect policy is exhausted.
    ///
    /// Not optional, and deliberately not skipped under `watch_mode: disabled`.
    /// The subscriber is a plugin-level connection carrying three families
    /// (DESIGN.md §3.3), and the third is the lock-release wake a blocked
    /// `lock()` rides — so a *cache* setting must not close it, or every blocked
    /// acquisition is silently pushed onto the heartbeat fallback. What
    /// `watch_mode: disabled` does save is every subscription and the whole
    /// watcher registry.
    pub subscriber: (SubscriberClient, ConnectHandle),
    /// Whether the client is in Redis Cluster mode.
    pub clustered: bool,
    /// What the URL scheme itself said about the topology.
    pub url_topology: Option<Topology>,
}

/// Builds the pool from the operator's URL and awaits the initial connect, so a
/// bad DSN or an unreachable server fails here rather than at first use.
///
/// Also reports what the URL itself said about the topology, which both the
/// preflight and `scan_prefix` need: a `redis-cluster://` URL puts `fred` in
/// cluster mode, and that is a fact about the client rather than an inference
/// from any one server's `INFO`.
///
/// # Errors
/// [`ClusterError::InvalidConfig`] for a URL `fred` cannot parse, and
/// [`ClusterError::Provider`] for a failing connect. A subscriber that fails to
/// connect abandons its own router task *and* closes the pool on the way out, so
/// a half-open startup leaves nothing behind.
pub async fn connect(spec: ConnectSpec<'_>) -> Result<Connected, ClusterError> {
    let mut client_config = Config::from_url(spec.url).map_err(map_redis_error)?;
    if spec.database != 0 {
        client_config.database = Some(spec.database);
    }
    let clustered = client_config.server.is_clustered();
    let url_topology = topology_from_server_config(&client_config.server);
    let subscriber_config = client_config.clone();

    let mut builder = Builder::from_config(client_config);
    builder.set_policy(reconnect_policy());
    let pool = builder
        .with_performance_config(|perf| {
            // The client-side bound every other timing guarantee rests on: with
            // it, no command can block indefinitely once issued, which is what
            // makes `stop()`'s pool drain finite (DESIGN.md §11, §12). Each
            // config's `validate` has already rejected the zero that would
            // disable it rather than shorten it.
            perf.default_command_timeout = spec.command_timeout;
        })
        .build_pool(usize::try_from(spec.pool_size).unwrap_or(usize::MAX))
        .map_err(map_redis_error)?;

    // `init` awaits the first successful connect and surfaces the failure to the
    // caller, rather than returning a pool that reconnects in the background
    // while every command fails.
    //
    // Bounded by [`CONNECT_TIMEOUT`], because the reconnect policy applies to
    // the initial connect too: without the bound, a URL pointing at a closed
    // port would retry for the policy's whole schedule before `build_and_start`
    // returned (`RD-LIFE-005`).
    match tokio::time::timeout(CONNECT_TIMEOUT, pool.init()).await {
        Ok(Ok(_connection)) => {}
        Ok(Err(err)) => {
            close_pool(&pool).await;
            return Err(map_redis_error(err));
        }
        Err(_elapsed) => {
            close_pool(&pool).await;
            return Err(unreachable_within_budget());
        }
    }

    // Built from the same config but outside the pool, because a connection in
    // subscribe mode accepts only subscribe-family commands (DESIGN.md §3.3).
    let mut subscriber_builder = Builder::from_config(subscriber_config);
    subscriber_builder.set_policy(reconnect_policy());
    let client = match subscriber_builder
        .with_performance_config(|perf| {
            // The same bound the pool gets, and for the same reason: the
            // subscribe-family commands this client carries — the `SUBSCRIBE`
            // behind every `watch()`, the `PSUBSCRIBE` of the keyspace pattern,
            // the `PING` that confirms them — are commands like any other, and
            // without this they are the one path in either plugin that can
            // block indefinitely. That would leave `watch()` unbounded against
            // a server that accepted the connection and then stopped answering,
            // which is exactly the state DESIGN.md §11 and §12 rest on not
            // being reachable.
            perf.default_command_timeout = spec.command_timeout;
        })
        .build_subscriber_client()
    {
        Ok(client) => client,
        Err(err) => {
            close_pool(&pool).await;
            return Err(map_redis_error(err));
        }
    };
    // `connect()` + `wait_for_connect()` rather than the `init()` shorthand,
    // because the shorthand only hands back the [`ConnectHandle`] on success and
    // this function needs it on the failure paths: `connect()` has already
    // spawned the router task by then, and [`abandon_subscriber`] documents why
    // dropping the client does not end it in this build of `fred`. `init()`
    // resets the task itself when the *connect* fails, but a timeout drops the
    // `init()` future before it can, which leaks one connection and one task per
    // `build_and_start` attempt — the same leak `RD-LIFE-010` covers for the
    // startup steps after this one.
    let connection = client.connect();
    let subscriber = match tokio::time::timeout(CONNECT_TIMEOUT, client.wait_for_connect()).await {
        // The handle stays pending while the client is connected or retrying,
        // and resolves when the reconnect policy gives up — the signal each
        // plugin's watchdog acts on.
        Ok(Ok(())) => (client, connection),
        Ok(Err(err)) => {
            abandon_subscriber(&client, &connection).await;
            close_pool(&pool).await;
            return Err(map_redis_error(err));
        }
        Err(_elapsed) => {
            abandon_subscriber(&client, &connection).await;
            close_pool(&pool).await;
            return Err(unreachable_within_budget());
        }
    };

    Ok(Connected {
        pool,
        subscriber,
        clustered,
        url_topology,
    })
}

/// The error a connect that outlived [`CONNECT_TIMEOUT`] reports.
///
/// `ConnectionLost` rather than `Timeout`, because it describes the server
/// rather than one command: the operator's URL is fine and their Redis is not
/// answering, which is the same state a mid-life outage produces and the same
/// one their retry policy should treat as retryable.
fn unreachable_within_budget() -> ClusterError {
    ClusterError::Provider {
        kind: cluster_sdk::ProviderErrorKind::ConnectionLost,
        message: format!(
            "redis did not answer within the {} ms connect budget; the server is unreachable or \
             is refusing connections",
            CONNECT_TIMEOUT.as_millis()
        ),
    }
}

/// What the connection URL's scheme says about the topology, or `None` for a
/// plain `redis://`/`rediss://` URL that says nothing.
///
/// A centralized URL is deliberately not reported as `Standalone`: it means "one
/// address", not "no replicas", and the single-node row of DESIGN.md §3.6 is the
/// one place a wrong answer weakens a guarantee. Detection via
/// `INFO replication` decides that case.
#[must_use]
pub fn topology_from_server_config(server: &ServerConfig) -> Option<Topology> {
    match server {
        ServerConfig::Clustered { .. } => Some(Topology::Cluster),
        ServerConfig::Sentinel { .. } => Some(Topology::Sentinel),
        ServerConfig::Centralized { .. } => None,
    }
}

// Layer-1 unit tests: the URL-scheme topology mapping, the one decision in this
// file that needs no server. Everything else here is a connect against a
// container (Layer 3).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clustered_url_is_reported_as_cluster() {
        let config = Config::from_url("redis-cluster://node-a:6379?node=node-b:6379")
            .expect("a clustered url parses");
        assert!(config.server.is_clustered());
        assert_eq!(
            topology_from_server_config(&config.server),
            Some(Topology::Cluster)
        );
    }

    #[test]
    fn a_plain_url_says_nothing_about_replicas() {
        // The trap this guards: reading `redis://one-host` as "standalone, so no
        // replicas" would let a Sentinel-managed primary reach the one row of
        // DESIGN.md §3.6 that declares Linearizable.
        let config = Config::from_url("redis://:pw@redis:6379/0").expect("a plain url parses");
        assert!(!config.server.is_clustered());
        assert_eq!(topology_from_server_config(&config.server), None);
    }
}
