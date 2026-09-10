//! [`RedisCache`] — the native `ClusterCacheBackend` (DESIGN.md §4).
//!
//! ## The entry is a two-field hash, and every mutation is a script
//!
//! An entry is `HSET <prefix>:c:<key> v <bytes> ver <decimal>`, and DESIGN.md
//! §2.2 records why it is not the obvious framed string: Redis's embedded Lua is
//! 5.1, whose numbers are IEEE doubles, so a version past 2^53 compared or
//! incremented *in Lua* silently loses precision and starts producing
//! duplicates. With a hash, comparison is a string compare (`HGET` returns a
//! string, `ARGV[1]` is a string, `==` is exact for every `u64`) and increment
//! is `HINCRBY`, 64-bit integer arithmetic in C. No number ever enters Lua, and
//! [`decode_entry`] below is the Rust-side half of the same rule — it parses the
//! version out of the decimal string rather than through any float.
//!
//! `get`, `contains`, and `scan_prefix` are the only operations that avoid Lua,
//! and that is not an oversight to fix later: every mutation needs an atomic
//! read-modify-write over two hash fields plus a conditional TTL plus a publish,
//! which no single Redis command expresses (DESIGN.md §4.1).
//!
//! ## Atomicity is not the same as the consistency declaration
//!
//! Each operation here is atomic — Redis runs a script to completion without
//! interleaving, so a `compare_and_swap` cannot lose to a concurrent CAS and
//! `put_if_absent` cannot admit two winners. The weakness ADR-009 names, and
//! that [`consistency`](RedisCache::consistency) reports, is entirely about
//! *losing an acknowledged write* to an fsync gap or a failover. Conflating the
//! two would either overstate the backend or understate it (DESIGN.md §4.5).

use std::sync::Arc;

use async_trait::async_trait;
use cluster_sdk::cache::{PutRequest, Ttl};
use cluster_sdk::{
    CacheConsistency, CacheEntry, CacheFeatures, CacheWatch, ClusterCacheBackend, ClusterError,
    ProviderErrorKind,
};
use fred::clients::Pool;
use fred::interfaces::{HashesInterface, KeysInterface};
use fred::types::Value;

use crate::cache::watch::{ChannelNames, WatchRegistry};
use crate::config::WatchMode;
use crate::observability::RedisSignals;
use crate::redis_error::map_redis_error;
use crate::scripts::{
    CACHE_CAS, CACHE_COMPARE_AND_DELETE, CACHE_DELETE, CACHE_PUT, CACHE_PUT_IF_ABSENT,
    PoolScriptExecutor, ScriptCache, eval,
};
use crate::wait::{self, WaitPolicy};

pub mod scan;
pub mod watch;

/// The `ARGV` sentinel meaning "no TTL" — the scripts branch on it to `PERSIST`
/// rather than `PEXPIRE` (DESIGN.md §6).
///
/// A sentinel rather than an omitted argument because a script's `ARGV` is
/// positional: leaving the slot out would shift every argument after it, and the
/// scripts that carry a channel name after the TTL would publish to whatever the
/// TTL used to be.
const NO_TTL: &str = "-1";

/// The native Redis cache backend.
pub struct RedisCache {
    pool: Pool,
    executor: PoolScriptExecutor,
    scripts: Arc<ScriptCache>,
    /// The operator's `key_prefix`, already combined with this cache's `:c:`
    /// primitive segment so the hot path concatenates once instead of twice.
    entry_prefix: String,
    /// The channel prefix, `<key_prefix>:e:c:`.
    channel_prefix: String,
    consistency: CacheConsistency,
    watch_mode: WatchMode,
    /// Whether the client is in Redis Cluster mode, which changes how
    /// [`scan::scan_prefix`] enumerates keys and whether prefix watch is
    /// offered at all.
    clustered: bool,
    wait: WaitPolicy,
    /// The channel and pattern names, shared with the subscriber fan-out so the
    /// publisher and the subscriber cannot disagree on them.
    names: ChannelNames,
    /// The watcher registry, or `None` under `watch_mode: disabled`.
    watchers: Option<Arc<WatchRegistry>>,
    /// The plugin's signal sink.
    ///
    /// The cache's *own* ADR-004 signals do not come from here: the handle wraps
    /// this backend in the SDK's `InstrumentedCache`, which emits every
    /// `cluster.cache.*` span, the ops counter, the duration histogram, and
    /// `cluster.provider.error` around each call (DESIGN.md §9). What this field
    /// carries is the one thing a decorator cannot see — a `NOSCRIPT` recovery,
    /// which happens *inside* an otherwise successful operation.
    signals: Arc<RedisSignals>,
}

/// Everything [`RedisCache::new`] needs, bundled so the call site names each
/// field — five of these are `String`s and `bool`s that would otherwise be
/// positional and interchangeable.
pub struct CacheInit {
    /// The connected command pool.
    pub pool: Pool,
    /// The SHAs `SCRIPT LOAD` returned at startup.
    pub scripts: Arc<ScriptCache>,
    /// The operator's `key_prefix` (DESIGN.md §2.1).
    pub key_prefix: String,
    /// The declaration the preflight computed (DESIGN.md §3.6).
    pub consistency: CacheConsistency,
    /// The operator's `watch_mode`.
    pub watch_mode: WatchMode,
    /// Whether the client is clustered.
    pub clustered: bool,
    /// The operator's `WAIT` policy, if any.
    pub wait: WaitPolicy,
    /// The logical database, which scopes the keyspace-notification pattern.
    pub database: u8,
    /// The watcher registry, or `None` under `watch_mode: disabled`.
    pub watchers: Option<Arc<WatchRegistry>>,
    /// The plugin's signal sink, shared with the lock and the fan-out.
    pub signals: Arc<RedisSignals>,
}

impl RedisCache {
    /// Builds the cache over an already-connected pool and an already-loaded
    /// script catalog.
    #[must_use]
    pub fn new(init: CacheInit) -> Self {
        let executor = PoolScriptExecutor::new(init.pool.clone());
        let names = ChannelNames::new(&init.key_prefix, init.database);
        Self {
            pool: init.pool,
            executor,
            scripts: init.scripts,
            entry_prefix: format!("{}:c:", init.key_prefix),
            channel_prefix: format!("{}:e:c:", init.key_prefix),
            consistency: init.consistency,
            watch_mode: init.watch_mode,
            clustered: init.clustered,
            wait: init.wait,
            names,
            watchers: init.watchers,
            signals: init.signals,
        }
    }

    /// The watcher registry, so the handle can close every watch at shutdown
    /// and the fan-out task can dispatch into it.
    #[must_use]
    pub fn watch_registry(&self) -> Option<Arc<WatchRegistry>> {
        self.watchers.clone()
    }

    /// The channel naming scheme, shared with the subscriber fan-out.
    #[must_use]
    pub fn channel_names(&self) -> ChannelNames {
        self.names.clone()
    }

    /// Whether this cache offers a native prefix watch (DESIGN.md §4.3).
    ///
    /// `false` under `watch_mode: disabled`, and `false` in Redis Cluster: plain
    /// `PUBLISH` is broadcast cluster-wide so the plugin's own events do reach a
    /// subscriber on any node, but `expired`/`evicted` keyspace notifications
    /// are emitted only by the node owning the key, so a clustered prefix watch
    /// would silently never report an expiry outside its own shard. Declaring
    /// `true` on the strength of that untested, half-working path is precisely
    /// the dishonest declaration ADR-009 forbids; DESIGN.md §13 D2 records the
    /// decision and designates `RD-SPEC-010` as the gate on lifting it.
    fn offers_prefix_watch(&self) -> bool {
        self.watchers.is_some() && !self.clustered && self.watch_mode == WatchMode::Publish
    }

    /// The Redis key holding `key`'s entry: `<prefix>:c:<key>`.
    ///
    /// `key` arrives already scope-prefixed by the SDK's `ScopedCacheBackend`,
    /// and this never inspects the consumer's portion of it (DESIGN.md §2.1).
    #[must_use]
    pub fn entry_key(&self, key: &str) -> String {
        format!("{}{key}", self.entry_prefix)
    }

    /// The pub/sub channel `key`'s events are published on:
    /// `<prefix>:e:c:<key>` — or the **empty string** under
    /// `watch_mode: disabled`, which every mutation script reads as "do not
    /// publish".
    ///
    /// No key exists at this name — it is a channel. It is passed to each
    /// mutation script as an `ARGV` rather than a second `KEYS` entry, because
    /// `PUBLISH` is not slot-routed and a second key would make the script
    /// multi-key for nothing (DESIGN.md §6).
    ///
    /// # Why the mode is enforced here rather than at each call site
    ///
    /// The write path is where the saving is. Gating only the watcher registry
    /// and the subscriptions would leave every write still issuing its `PUBLISH`,
    /// to a channel with — by construction — no subscriber: watches off, and none
    /// of the write-path cost saved that the mode exists to save. That is
    /// precisely backwards, since DESIGN.md §4.3 offers the mode for "a managed
    /// Redis where neither `CONFIG` nor the publish overhead is acceptable", and
    /// this type's own config documentation promises "no publish and no
    /// cache-event subscriber". `RD-WATCH-010` is the regression test.
    ///
    /// Routing it through this one accessor means the five mutation paths cannot
    /// disagree about it — each already calls this to build its channel argument,
    /// so none of them needs to know the mode exists.
    #[must_use]
    pub fn event_channel(&self, key: &str) -> String {
        if self.watch_mode == WatchMode::Disabled {
            return String::new();
        }
        format!("{}{key}", self.channel_prefix)
    }

    /// The prefix every entry key of this cache starts with, `<prefix>:c:` —
    /// the `MATCH` stem and the strip target for the `scan` module's
    /// `scan_prefix`.
    #[must_use]
    pub fn entry_prefix(&self) -> &str {
        &self.entry_prefix
    }

    /// Issues the operator's `WAIT` after a conditional write, if there is one.
    ///
    /// The policy itself lives in [`crate::wait`], shared with the lock: this is
    /// only the choice of *which* writes carry it. `put_if_absent`,
    /// `compare_and_swap`, and `compare_and_delete` do, following DESIGN.md
    /// §3.6's "each lock and CAS write" literally; an unconditional `put` or
    /// `delete` carries no decision made on a value that a failover could
    /// invalidate, which is what `WAIT` exists to narrow.
    async fn wait_for_replicas(&self) -> Result<(), ClusterError> {
        wait::wait_for_replicas(&self.pool, self.wait).await
    }
}

/// Renders a [`Ttl`] as the script's TTL argument: milliseconds, or the
/// [`NO_TTL`] sentinel.
///
/// A write always sets the entry's TTL explicitly and never preserves a previous
/// one implicitly, which is why `Indefinite` becomes an active `PERSIST` inside
/// the script rather than "leave it alone" — that is what the SDK's two-valued
/// `Ttl` says should happen (DESIGN.md §4.2).
///
/// A sub-millisecond `Ttl::Of` rounds up to 1 ms rather than down to 0:
/// `PEXPIRE k 0` deletes the key outright, so rounding down would turn "expires
/// almost immediately" into "was never stored", and the caller's subsequent read
/// would see an absence it could not distinguish from a failed write.
///
/// A `Duration` whose millisecond count exceeds `u64` **saturates**, and that is
/// deliberate rather than overlooked — `an_absurd_ttl_saturates_rather_than_wrapping`
/// pins it, as its sibling does for [`px_millis`](crate::px_millis). Unlike
/// `wait_timeout_ms`, which is operator YAML and so is rejected up front by
/// [`WaitPolicy::from_config`](crate::WaitPolicy::from_config), a [`Ttl`] is
/// built by a Rust caller out of a `Duration` three orders of magnitude past
/// anything a cache entry means, and the saturated value is one Redis itself
/// rejects: `PEXPIRE`'s deadline is `now + ttl`, which overflows. So the caller
/// gets an error either way, and this function stays infallible for the
/// unreachable case rather than putting a `Result` on every ordinary write.
/// What must not happen is a *wrap* into a short TTL, which would silently
/// expire an entry the caller asked to keep.
#[must_use]
pub fn ttl_argument(ttl: Ttl) -> String {
    match ttl {
        Ttl::Of(duration) => {
            let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
            millis.max(1).to_string()
        }
        Ttl::Indefinite => NO_TTL.to_owned(),
    }
}

/// Decodes an `HMGET K v ver` reply into an entry (DESIGN.md §4.1).
///
/// Both fields `nil` is an absent key — which also covers a key that exists but
/// carries neither field, a state this plugin never writes and cannot
/// meaningfully read. Exactly one field present is a different matter: every
/// mutation writes both inside one script, so a half-populated hash means
/// something *other than this plugin* owns the key, and reporting that as an
/// absence would let the next write silently merge into a stranger's hash.
///
/// # Errors
/// [`ClusterError::Provider`] with [`ProviderErrorKind::Other`] for a reply that
/// is not a two-element array, for a half-populated entry, or for a version that
/// is not a decimal integer.
pub fn decode_entry(key: &str, reply: &Value) -> Result<Option<CacheEntry>, ClusterError> {
    let malformed = |detail: String| ClusterError::Provider {
        kind: ProviderErrorKind::Other,
        message: format!("cache entry at `{key}` is not one this plugin wrote: {detail}"),
    };

    let Value::Array(fields) = reply else {
        return Err(malformed(format!(
            "expected an HMGET array reply, got {:?}",
            reply.kind()
        )));
    };
    let [value, version] = fields.as_slice() else {
        return Err(malformed(format!(
            "expected exactly the two fields `v` and `ver`, got {} element(s)",
            fields.len()
        )));
    };

    match (value.is_null(), version.is_null()) {
        (true, true) => Ok(None),
        (false, false) => {
            let bytes = value
                .clone()
                .into_owned_bytes()
                .ok_or_else(|| malformed(format!("field `v` is {:?}, not bytes", value.kind())))?;
            Ok(Some(CacheEntry {
                value: bytes,
                version: decode_version(version).ok_or_else(|| {
                    malformed(format!("field `ver` is not a decimal integer: {version:?}"))
                })?,
            }))
        }
        // Named for what it holds — `value.is_null()` — rather than inverted:
        // `true` here means `v` is *absent*, so the field that *is* set is
        // `ver`. A binding called `present_v` would read as the opposite and
        // invite a later reader to swap the two strings.
        (value_is_null, _) => Err(malformed(format!(
            "only the `{}` field is set; every write in this plugin sets both atomically",
            if value_is_null { "ver" } else { "v" }
        ))),
    }
}

/// Parses a version out of a script or `HGET` reply, without ever going through
/// a float.
///
/// `HINCRBY` is `i64` server-side and `HGET` returns the counter as a decimal
/// string, so both shapes appear depending on which command produced the reply.
/// Both are parsed as integers: routing either through `f64` would silently
/// round any version past 2^53, which is the exact failure DESIGN.md §2.2 chose
/// the hash encoding to avoid.
#[must_use]
pub fn decode_version(value: &Value) -> Option<u64> {
    match value {
        Value::Integer(raw) => u64::try_from(*raw).ok(),
        Value::String(raw) => raw.parse::<u64>().ok(),
        Value::Bytes(raw) => std::str::from_utf8(raw).ok()?.parse::<u64>().ok(),
        _ => None,
    }
}

#[async_trait]
impl ClusterCacheBackend for RedisCache {
    fn consistency(&self) -> CacheConsistency {
        // Whatever the startup preflight computed (DESIGN.md §3.6), never
        // re-evaluated. A topology that changes under a running plugin does not
        // downgrade a live declaration — a real gap, recorded in DESIGN.md §12
        // rather than solved, because the resolution model cannot express a
        // backend whose capabilities change after consumers resolved on them.
        self.consistency
    }

    fn features(&self) -> CacheFeatures {
        CacheFeatures::new(self.offers_prefix_watch())
    }

    async fn get(&self, key: &str) -> Result<Option<CacheEntry>, ClusterError> {
        let entry_key = self.entry_key(key);
        let reply: Value = self
            .pool
            .hmget(entry_key, vec!["v", "ver"])
            .await
            .map_err(map_redis_error)?;
        decode_entry(key, &reply)
    }

    async fn put(&self, req: PutRequest<'_>) -> Result<(), ClusterError> {
        let args = vec![
            Value::Bytes(req.value.to_vec().into()),
            Value::String(ttl_argument(req.ttl).into()),
            Value::String(self.event_channel(req.key).into()),
        ];
        eval(
            &self.executor,
            &self.scripts,
            &CACHE_PUT,
            &self.entry_key(req.key),
            &args,
            &self.signals,
        )
        .await?;
        Ok(())
    }

    async fn put_if_absent(&self, req: PutRequest<'_>) -> Result<Option<CacheEntry>, ClusterError> {
        let args = vec![
            Value::Bytes(req.value.to_vec().into()),
            Value::String(ttl_argument(req.ttl).into()),
            Value::String(self.event_channel(req.key).into()),
        ];
        let reply = eval(
            &self.executor,
            &self.scripts,
            &CACHE_PUT_IF_ABSENT,
            &self.entry_key(req.key),
            &args,
            &self.signals,
        )
        .await?;
        // The script returns `false` (a RESP nil) when the key already existed
        // and `1` when it created it at version 1.
        if reply.is_null() || reply == Value::Boolean(false) {
            return Ok(None);
        }
        self.wait_for_replicas().await?;
        Ok(Some(CacheEntry {
            value: req.value.to_vec(),
            version: 1,
        }))
    }

    async fn compare_and_swap(
        &self,
        key: &str,
        expected_version: u64,
        new_value: &[u8],
        ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        let args = vec![
            Value::String(expected_version.to_string().into()),
            Value::Bytes(new_value.to_vec().into()),
            Value::String(ttl_argument(ttl).into()),
            Value::String(self.event_channel(key).into()),
        ];
        let reply = eval(
            &self.executor,
            &self.scripts,
            &CACHE_CAS,
            &self.entry_key(key),
            &args,
            &self.signals,
        )
        .await?;
        match decode_cas_reply(key, &reply)? {
            CasOutcome::Swapped { version } => {
                self.wait_for_replicas().await?;
                Ok(CacheEntry {
                    value: new_value.to_vec(),
                    version,
                })
            }
            CasOutcome::Conflict { current } => Err(ClusterError::CasConflict {
                key: key.to_owned(),
                current,
            }),
        }
    }

    async fn compare_and_delete(
        &self,
        key: &str,
        expected_value: &[u8],
    ) -> Result<bool, ClusterError> {
        let args = vec![
            Value::Bytes(expected_value.to_vec().into()),
            Value::String(self.event_channel(key).into()),
        ];
        let reply = eval(
            &self.executor,
            &self.scripts,
            &CACHE_COMPARE_AND_DELETE,
            &self.entry_key(key),
            &args,
            &self.signals,
        )
        .await?;
        let deleted = reply.as_i64().unwrap_or(0) == 1;
        if deleted {
            self.wait_for_replicas().await?;
        }
        Ok(deleted)
    }

    async fn delete(&self, key: &str) -> Result<bool, ClusterError> {
        let args = vec![Value::String(self.event_channel(key).into())];
        let reply = eval(
            &self.executor,
            &self.scripts,
            &CACHE_DELETE,
            &self.entry_key(key),
            &args,
            &self.signals,
        )
        .await?;
        Ok(reply.as_i64().unwrap_or(0) == 1)
    }

    async fn contains(&self, key: &str) -> Result<bool, ClusterError> {
        let present: i64 = self
            .pool
            .exists(self.entry_key(key))
            .await
            .map_err(map_redis_error)?;
        Ok(present > 0)
    }

    async fn watch(&self, key: &str) -> Result<CacheWatch, ClusterError> {
        // `Unsupported` rather than a channel nothing ever sends on: a watch
        // that silently never fires is indistinguishable from a quiet key, and
        // every consumer of it would be reasoning about a guarantee it does not
        // have. Under `watch_mode: disabled` there is no subscriber at all, so
        // this is the honest answer (DESIGN.md §4.3).
        let Some(registry) = self.watchers.as_ref() else {
            return Err(ClusterError::Unsupported { feature: "watch" });
        };
        registry
            .register_key(key, &self.names.channel_for_key(key))
            .await
    }

    async fn watch_prefix(&self, prefix: &str) -> Result<CacheWatch, ClusterError> {
        // One guard, not two: `offers_prefix_watch` already requires
        // `self.watchers.is_some()`, so a separate `as_ref()` check below it can
        // never fire and would only state the invariant a second time.
        let Some(registry) = self
            .watchers
            .as_ref()
            .filter(|_| self.offers_prefix_watch())
        else {
            return Err(ClusterError::Unsupported {
                feature: "prefix_watch",
            });
        };
        registry
            .register_prefix(prefix, &self.names.pattern_for_prefix(prefix))
            .await
    }

    async fn scan_prefix(&self, prefix: &str) -> Result<Vec<String>, ClusterError> {
        scan::scan_prefix(&self.pool, self.clustered, &self.entry_prefix, prefix).await
    }
}

/// The three shapes `cache_cas` can reply with, collapsed to the two the caller
/// acts on.
#[derive(Debug, PartialEq, Eq)]
pub enum CasOutcome {
    /// The expected version matched and the write went through.
    Swapped {
        /// The version the entry now carries.
        version: u64,
    },
    /// The expected version did not match, or the key was absent.
    Conflict {
        /// The entry as it stands, when the script could supply it. `None` for
        /// an absent key, which is what `CasConflict { current: None }` means.
        current: Option<CacheEntry>,
    },
}

/// Decodes the `cache_cas` reply (DESIGN.md §6).
///
/// The script answers `{1, new_version}` on success, `{0, current_version,
/// current_value}` on a version mismatch, and `{0}` when the key is absent. The
/// mismatch arm carrying the current entry is what lets `CasConflict.current` be
/// populated without a second round trip — and a `None` there is a real answer
/// (the key is gone), not a missing one.
///
/// # Errors
/// [`ClusterError::Provider`] with [`ProviderErrorKind::Other`] for a reply
/// shape the script cannot have produced, which would mean the loaded script is
/// not the one this code was written against.
pub fn decode_cas_reply(key: &str, reply: &Value) -> Result<CasOutcome, ClusterError> {
    let malformed = |detail: String| ClusterError::Provider {
        kind: ProviderErrorKind::Other,
        message: format!(
            "cache_cas returned a reply this plugin cannot read for `{key}`: {detail}"
        ),
    };

    let Value::Array(parts) = reply else {
        return Err(malformed(format!(
            "expected an array, got {:?}",
            reply.kind()
        )));
    };
    match parts.as_slice() {
        [status, version] if status.as_i64() == Some(1) => Ok(CasOutcome::Swapped {
            version: decode_version(version)
                .ok_or_else(|| malformed(format!("new version is not an integer: {version:?}")))?,
        }),
        [_absent] => Ok(CasOutcome::Conflict { current: None }),
        [_status, current_version, current_value] => Ok(CasOutcome::Conflict {
            current: Some(CacheEntry {
                value: current_value.clone().into_owned_bytes().ok_or_else(|| {
                    malformed(format!(
                        "current value is {:?}, not bytes",
                        current_value.kind()
                    ))
                })?,
                version: decode_version(current_version).ok_or_else(|| {
                    malformed(format!(
                        "current version is not an integer: {current_version:?}"
                    ))
                })?,
            }),
        }),
        other => Err(malformed(format!(
            "expected 1, 2, or 3 elements, got {}",
            other.len()
        ))),
    }
}

// Layer-1 unit tests (TESTING.md §2, `cache/mod.rs` row): key and channel
// construction, TTL rendering, and reply decoding. Every command this file
// issues is covered at Layer 3. Out-of-line per DE1101.
#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
