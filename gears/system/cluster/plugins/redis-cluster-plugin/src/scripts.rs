//! The Lua script catalog (DESIGN.md §6): the sources, the SHA cache that
//! `SCRIPT LOAD` fills, and the `EVALSHA` driver with its single `NOSCRIPT`
//! reload-and-retry.
//!
//! ## Every script takes exactly one key
//!
//! That is what makes the catalog Redis-Cluster-correct: Redis routes an
//! `EVALSHA` by its declared keys, so a single-key script always lands on the
//! node owning that key and `CROSSSLOT` is unreachable. The published channel
//! name is passed as an `ARGV` rather than as a second key — `ARGV[3]` for
//! `cache_put`, `ARGV[4]` for `cache_cas`, `ARGV[2]` for `lock_release` —
//! because `PUBLISH` is not slot-routed and passing the channel as a key would
//! make the script multi-key for no gain.
//!
//! The channel is therefore built Rust-side, by
//! [`RedisCache::event_channel`](crate::cache::RedisCache) and
//! [`LockNames::release_channel`](crate::lock::LockNames), and that is
//! load-bearing rather than incidental: `watch_mode: disabled` works by
//! `event_channel` answering the empty string and every mutation script
//! skipping its `PUBLISH` on `ARGV[n] ~= ''`. A reader who believes the channel
//! is derived server-side from `KEYS[1]` cannot reconstruct how `disabled`
//! reaches the write path at all.
//!
//! The invariant is asserted structurally rather than trusted:
//! [`ScriptSpec::declared_key_indices`] reads the `KEYS[n]` references back out
//! of the Lua source, and a unit test requires every catalogued script to
//! declare exactly `{1}`. A future two-key script therefore fails the build's
//! tests instead of production's `CROSSSLOT`. The single-key rule is also
//! encoded in [`ScriptExecutor::evalsha`]'s signature, which takes one `key`
//! and cannot express a second.
//!
//! ## Why the SHA cache needs no interior mutability
//!
//! A script's SHA is a pure function of its source, so it never changes for the
//! life of the process. [`ScriptCache`] is built once by [`load_catalog`] and
//! shared immutably, with no lock on the hot path — and the `NOSCRIPT` recovery
//! below repopulates the *server's* script cache, not this one, so it has
//! nothing to write back.
//!
//! ## `NOSCRIPT` recovers with `EVAL`, not with a second `SCRIPT LOAD`
//!
//! The startup load is one `SCRIPT LOAD` per script and is deliberately not
//! broadcast to every primary, which is not reachable here anyway: `fred`'s
//! `script_load_cluster` — its only broadcasting API — is gated behind the `sha-1`
//! feature, because it hashes the script client-side to have something to return,
//! and this workspace keeps a SHA-1 implementation out of its dependency graph.
//!
//! The recovery path answers the same need better than a broadcast would. On
//! `NOSCRIPT`, [`eval`] retries with `EVAL <source>` rather than reloading and
//! re-`EVALSHA`ing:
//!
//! - **It is routed by the key**, so in cluster mode it necessarily reaches the
//!   node that just reported the miss. A `SCRIPT LOAD` carries no key and is
//!   routed to whichever node the client picks, so the reload could easily warm
//!   a node other than the one about to be retried.
//! - **It is one round trip instead of two**, and it both executes the call and
//!   populates that node's script cache, so subsequent `EVALSHA`s on the same
//!   shard hit.
//! - **It self-heals**, which a startup broadcast cannot: a primary added by a
//!   reshard, or one restarted mid-life, has an empty script cache no
//!   startup-time load ever reached.
//!
//! The cost is one extra round trip the first time each shard sees each script,
//! amortized to nothing thereafter, and it is why `load_catalog` still issues
//! `SCRIPT LOAD` at startup: that is where the SHA comes from, and it is also
//! the check that the server accepts the script at all rather than discovering a
//! syntax error on the first write.
//!
//! ## Why a `ScriptExecutor` seam
//!
//! The three round-trips sit behind [`ScriptExecutor`] so the recovery policy —
//! the part with a real failure mode, an unbounded retry loop against a server
//! that answers `NOSCRIPT` to everything — is testable with no Redis at all.

use std::collections::{BTreeSet, HashMap};

use cluster_sdk::{ClusterError, ProviderErrorKind};
use fred::clients::Pool;
use fred::error::Error;
use fred::interfaces::LuaInterface;
use fred::types::Value;

use crate::observability::RedisSignals;
use crate::redis_error::{is_noscript, map_redis_error};

/// One entry in the catalog: a name to look its SHA up by, and the Lua source
/// `SCRIPT LOAD` is given.
///
/// `source` is `&'static str` so a script can never be assembled at runtime
/// from a key, a name, or anything else a caller supplies — the catalog is
/// fixed at compile time, and Lua injection has nowhere to enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptSpec {
    /// The catalog name, used as the [`ScriptCache`] lookup key and in logs.
    pub name: &'static str,
    /// The Lua source, verbatim as sent to `SCRIPT LOAD`.
    pub source: &'static str,
}

impl ScriptSpec {
    /// Returns the distinct `KEYS[n]` indices this script's source references.
    ///
    /// The mechanical form of DESIGN.md §6's single-key rule: the answer for
    /// every catalogued script must be exactly `{1}`. Reading it out of the
    /// source rather than storing a hand-maintained count is the point — a
    /// count can be copied along with the script it no longer describes.
    ///
    /// Deliberately naive: it looks for the literal `KEYS[` followed by
    /// decimal digits and `]`, which is the only form the catalog uses. A
    /// computed index (`KEYS[i]`) would not be counted, and that is the
    /// conservative direction only because nothing in the catalog computes
    /// one — a script that did would need this to grow with it.
    #[must_use]
    pub fn declared_key_indices(&self) -> BTreeSet<usize> {
        let mut indices = BTreeSet::new();
        let mut rest = self.source;
        while let Some(start) = rest.find("KEYS[") {
            rest = &rest[start + "KEYS[".len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Some(stripped) = rest.strip_prefix(digits.as_str())
                && stripped.starts_with(']')
                && let Ok(index) = digits.parse::<usize>()
            {
                indices.insert(index);
            }
        }
        indices
    }
}

/// `cache_put` — unconditional write. DESIGN.md §6.
///
/// `HINCRBY` rather than a Lua `ver + 1` because Redis's embedded Lua is 5.1,
/// whose numbers are IEEE doubles: a version past 2^53 would silently lose
/// precision and start producing duplicates (DESIGN.md §2.2). `HINCRBY` is
/// 64-bit integer arithmetic in C, and no number ever enters Lua.
pub const CACHE_PUT: ScriptSpec = ScriptSpec {
    name: "cache_put",
    source: r"
-- KEYS[1]=entry  ARGV[1]=value  ARGV[2]=px_or_-1  ARGV[3]=channel
-- Returns: the new version, as an integer
local ver = redis.call('HINCRBY', KEYS[1], 'ver', 1)
redis.call('HSET', KEYS[1], 'v', ARGV[1])
if ARGV[2] == '-1' then redis.call('PERSIST', KEYS[1])
else redis.call('PEXPIRE', KEYS[1], ARGV[2]) end
if ARGV[3] ~= '' then redis.call('PUBLISH', ARGV[3], 'C') end
return ver
",
};

/// `cache_put_if_absent` — create-only write. DESIGN.md §6.
///
/// Needs Lua rather than a bare `SET NX` because the entry is a two-field hash
/// and the version has to be seeded to 1 atomically with the value
/// (DESIGN.md §2.2, §2.3).
pub const CACHE_PUT_IF_ABSENT: ScriptSpec = ScriptSpec {
    name: "cache_put_if_absent",
    source: r"
-- KEYS[1]=entry  ARGV[1]=value  ARGV[2]=px_or_-1  ARGV[3]=channel
-- Returns: 1 when created (version 1), or false when the key already existed
if redis.call('EXISTS', KEYS[1]) == 1 then return false end
redis.call('HSET', KEYS[1], 'v', ARGV[1], 'ver', 1)
if ARGV[2] ~= '-1' then redis.call('PEXPIRE', KEYS[1], ARGV[2]) end
if ARGV[3] ~= '' then redis.call('PUBLISH', ARGV[3], 'C') end
return 1
",
};

/// `cache_cas` — version-guarded write. DESIGN.md §6.
///
/// The version comparison is a *string* compare: `HGET` returns a string,
/// `ARGV[1]` is a string, and `==` between them is exact for every `u64`
/// (DESIGN.md §2.2). The mismatch reply carries the current version and value
/// so the caller can populate `CasConflict { current }` without a second round
/// trip.
pub const CACHE_CAS: ScriptSpec = ScriptSpec {
    name: "cache_cas",
    source: r"
-- KEYS[1]=entry  ARGV[1]=expected_version  ARGV[2]=new_value
--                ARGV[3]=px_or_-1          ARGV[4]=channel
-- Returns: {1, new_version} on success
--          {0, current_version, current_value} on version mismatch
--          {0} when the key is absent (caller reports CasConflict{current: None})
local cur = redis.call('HGET', KEYS[1], 'ver')
if not cur then return {0} end
if cur ~= ARGV[1] then
    return {0, cur, redis.call('HGET', KEYS[1], 'v')}
end
local ver = redis.call('HINCRBY', KEYS[1], 'ver', 1)
redis.call('HSET', KEYS[1], 'v', ARGV[2])
if ARGV[3] == '-1' then redis.call('PERSIST', KEYS[1])
else redis.call('PEXPIRE', KEYS[1], ARGV[3]) end
if ARGV[4] ~= '' then redis.call('PUBLISH', ARGV[4], 'C') end
return {1, ver}
",
};

/// `cache_compare_and_delete` — value-guarded delete. DESIGN.md §6.
///
/// Guarded on the **value**, not the version, and that is the whole point:
/// a version resets to 1 when a key is deleted and recreated
/// (`cluster-cache-version-reset-caveat`), so a version guard could match a
/// successor's fresh entry and wipe its claim. A successor that re-claimed the
/// key after a TTL lapse wrote a different value, so the value guard is a safe
/// no-op against it.
pub const CACHE_COMPARE_AND_DELETE: ScriptSpec = ScriptSpec {
    name: "cache_compare_and_delete",
    source: r"
-- KEYS[1]=entry  ARGV[1]=expected_value  ARGV[2]=channel
-- Returns: 1 when deleted, 0 on value mismatch or absent key (never an error)
if redis.call('HGET', KEYS[1], 'v') ~= ARGV[1] then return 0 end
redis.call('DEL', KEYS[1])
if ARGV[2] ~= '' then redis.call('PUBLISH', ARGV[2], 'D') end
return 1
",
};

/// `cache_delete` — unconditional delete. DESIGN.md §6.
///
/// The `DEL` return value gates the `PUBLISH` so a delete of an absent key
/// emits no event, keeping the "one event per logical mutation" property
/// DESIGN.md §4.3 rests on.
pub const CACHE_DELETE: ScriptSpec = ScriptSpec {
    name: "cache_delete",
    source: r"
-- KEYS[1]=entry  ARGV[1]=channel
-- Returns: 1 when the key existed, else 0
if redis.call('DEL', KEYS[1]) == 0 then return 0 end
if ARGV[1] ~= '' then redis.call('PUBLISH', ARGV[1], 'D') end
return 1
",
};

/// `lock_renew` — token-fenced lease extension. DESIGN.md §6, §5.2.
///
/// The `GET`-then-`PEXPIRE` pair has to be one script: between a client-side
/// read and a bare `PEXPIRE`, the lease can lapse and a successor can claim it,
/// and the renewal would then extend *their* lease.
pub const LOCK_RENEW: ScriptSpec = ScriptSpec {
    name: "lock_renew",
    source: r"
-- KEYS[1]=lock  ARGV[1]=holder_token  ARGV[2]=px
-- Returns: 1 when renewed, 0 when the token does not match or the key is gone
if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end
return redis.call('PEXPIRE', KEYS[1], ARGV[2])
",
};

/// `lock_release` — token-fenced release. DESIGN.md §6, §5.2.
///
/// The classic bug this exists to prevent: a bare `DEL` on release deletes
/// whatever is under the key, so a holder whose lease already lapsed releases
/// its *successor's* lock. `RD-LOCK-006` is the regression test.
pub const LOCK_RELEASE: ScriptSpec = ScriptSpec {
    name: "lock_release",
    source: r"
-- KEYS[1]=lock  ARGV[1]=holder_token  ARGV[2]=channel
-- Returns: 1 when released, 0 when we are no longer the holder
if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end
redis.call('DEL', KEYS[1])
redis.call('PUBLISH', ARGV[2], 'R')
return 1
",
};

/// The cache half of the catalog.
///
/// Acquisition and the read path are absent because neither needs a script:
/// `SET K token NX PX ttl` is already atomic (DESIGN.md §5.1), and
/// `get`/`contains`/`scan_prefix` are plain commands (DESIGN.md §4.1).
pub const CACHE_SCRIPTS: &[ScriptSpec] = &[
    CACHE_PUT,
    CACHE_PUT_IF_ABSENT,
    CACHE_CAS,
    CACHE_COMPARE_AND_DELETE,
    CACHE_DELETE,
];

/// The lock half of the catalog — the only two the standalone
/// `RedisLockPlugin` loads (DESIGN.md §3.5).
pub const LOCK_SCRIPTS: &[ScriptSpec] = &[LOCK_RENEW, LOCK_RELEASE];

/// Every script, for the combined cache+lock plugin.
pub const ALL_SCRIPTS: &[ScriptSpec] = &[
    CACHE_PUT,
    CACHE_PUT_IF_ABSENT,
    CACHE_CAS,
    CACHE_COMPARE_AND_DELETE,
    CACHE_DELETE,
    LOCK_RENEW,
    LOCK_RELEASE,
];

/// The two server round-trips the script layer needs, behind a seam so the
/// reload-and-retry policy is testable without a Redis.
///
/// Not dyn-compatible — the methods return `impl Future` — and deliberately so:
/// there is exactly one production implementation (the `fred` pool) and one test
/// double, both known statically, so the seam costs no allocation and no vtable.
pub trait ScriptExecutor: Sync {
    /// `SCRIPT LOAD <source>`, returning the SHA **the server computed**.
    ///
    /// The plugin never hashes a script itself: `fred`'s `Script::from_lua`
    /// would, but it is gated behind the `sha-1` feature, and this workspace
    /// keeps a SHA-1 implementation out of its dependency graph for what is only
    /// script-cache addressing (`dylint.toml` DE0708 records the FIPS rule this
    /// follows).
    ///
    /// Issued once per script at startup, and never again: it is where the SHA
    /// comes from, not the recovery path (see the module docs).
    fn script_load(
        &self,
        source: &'static str,
    ) -> impl Future<Output = Result<String, Error>> + Send;

    /// `EVALSHA <sha> 1 <key> <args…>` — the steady-state call.
    ///
    /// One key, not a slice: the single-key invariant of DESIGN.md §6 is a
    /// property of the whole catalog, so encoding it in the signature means a
    /// multi-key call cannot be written at all.
    fn evalsha(
        &self,
        sha: &str,
        key: &str,
        args: &[Value],
    ) -> impl Future<Output = Result<Value, Error>> + Send;

    /// `EVAL <source> 1 <key> <args…>` — the `NOSCRIPT` recovery.
    ///
    /// Carries the same key as the [`evalsha`](Self::evalsha) it replaces, which
    /// is the whole point: the retry is routed to the node that reported the
    /// miss, and caches the script there on the way through.
    fn eval_source(
        &self,
        source: &'static str,
        key: &str,
        args: &[Value],
    ) -> impl Future<Output = Result<Value, Error>> + Send;
}

/// The production [`ScriptExecutor`]: a connected `fred` command pool.
///
/// Carries no cluster flag, and that is a consequence of the recovery design
/// rather than an omission — every method here is either key-routed (`EVALSHA`,
/// `EVAL`) or indifferent to which node serves it (`SCRIPT LOAD`, whose only
/// job is to return the SHA). See the module docs.
#[derive(Debug, Clone)]
pub struct PoolScriptExecutor {
    pool: Pool,
}

impl PoolScriptExecutor {
    /// Wraps a connected pool.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl ScriptExecutor for PoolScriptExecutor {
    async fn script_load(&self, source: &'static str) -> Result<String, Error> {
        self.pool.script_load(source).await
    }

    async fn evalsha(&self, sha: &str, key: &str, args: &[Value]) -> Result<Value, Error> {
        // One key, always — the catalog's single-key invariant (see the module
        // docs) is what makes this routable in cluster mode at all.
        self.pool.evalsha(sha, key, args.to_vec()).await
    }

    async fn eval_source(
        &self,
        source: &'static str,
        key: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        self.pool.eval(source, key, args.to_vec()).await
    }
}

/// The catalog's SHAs, keyed by [`ScriptSpec::name`].
///
/// Immutable after [`load_catalog`] builds it — see the module docs for why a
/// `NOSCRIPT` reload does not have to write back into it.
#[derive(Debug, Clone, Default)]
pub struct ScriptCache {
    shas: HashMap<&'static str, String>,
}

impl ScriptCache {
    /// The SHA `SCRIPT LOAD` returned for `name`.
    ///
    /// # Errors
    /// [`ClusterError::Provider`] with [`ProviderErrorKind::Other`] when the
    /// name is not in the cache. That is a plugin bug rather than an operator
    /// or server condition — it means a script was evaluated that its plugin
    /// never loaded — so it is reported as unretryable and says so.
    pub fn sha(&self, name: &str) -> Result<&str, ClusterError> {
        self.shas
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| ClusterError::Provider {
                kind: ProviderErrorKind::Other,
                message: format!(
                    "script `{name}` was evaluated but is not in this plugin's loaded catalog \
                     (a plugin bug: the catalog loaded at startup does not contain it)"
                ),
            })
    }
}

/// `SCRIPT LOAD`s each script in `scripts` once and returns the resulting SHA
/// cache. Step 3 of `build_and_start` (DESIGN.md §3.2).
///
/// # Errors
/// Whatever [`map_redis_error`] makes of a failing `SCRIPT LOAD` — in practice
/// a connection or auth failure, since the sources are compile-time constants
/// and cannot be rejected for syntax at runtime without having been rejected in
/// every prior deployment too.
pub async fn load_catalog<E: ScriptExecutor>(
    executor: &E,
    scripts: &'static [ScriptSpec],
) -> Result<ScriptCache, ClusterError> {
    let mut shas = HashMap::with_capacity(scripts.len());
    for script in scripts {
        let sha = executor
            .script_load(script.source)
            .await
            .map_err(map_redis_error)?;
        shas.insert(script.name, sha);
    }
    Ok(ScriptCache { shas })
}

/// Evaluates `script` against `key`, recovering **exactly once** if the server
/// reports `NOSCRIPT` (DESIGN.md §6).
///
/// The bound is the point. A server that restarts under load answers `NOSCRIPT`
/// to every in-flight command, and a policy that recovered on each failure
/// without a limit would turn one restart into an unbounded retry storm against
/// a server that is already struggling. The recovery path is entered at most
/// once per call and never re-entered, so anything that goes wrong inside it —
/// including a second `NOSCRIPT`, which `EVAL` cannot actually produce — falls
/// through [`map_redis_error`] to `Provider { Other }`.
///
/// # Errors
/// [`ClusterError::Provider`] for a failing `EVALSHA` or a failing recovery, and
/// whatever [`ScriptCache::sha`] returns if `script` was never loaded.
pub async fn eval<E: ScriptExecutor>(
    executor: &E,
    cache: &ScriptCache,
    script: &ScriptSpec,
    key: &str,
    args: &[Value],
    signals: &RedisSignals,
) -> Result<Value, ClusterError> {
    let sha = cache.sha(script.name)?;
    match executor.evalsha(sha, key, args).await {
        Ok(value) => Ok(value),
        Err(err) if is_noscript(&err) => {
            // DEBUG, not WARN: an emptied server-side script cache is a normal
            // consequence of a restart, a `SCRIPT FLUSH`, or (in cluster mode)
            // the first call to reach a given shard — all fully recovered from
            // here. What makes it alertable is the aggregate rather than the
            // line: `cluster_redis_script_reloads_total` climbing steadily means
            // something is flushing the script cache under the plugin
            // (DESIGN.md §9).
            signals.script_reloaded();
            tracing::debug!(
                script = script.name,
                "redis script cache miss; retrying with EVAL, which reaches the same node and \
                 caches the script there"
            );
            executor
                .eval_source(script.source, key, args)
                .await
                .map_err(map_redis_error)
        }
        Err(err) => Err(map_redis_error(err)),
    }
}

// Layer-1 unit tests (TESTING.md §2, `scripts.rs` row): the single-key
// structural invariant, one `SCRIPT LOAD` per script, and the bounded
// reload-and-retry. Out-of-line per DE1101.
#[cfg(test)]
#[path = "scripts_tests.rs"]
mod tests;
