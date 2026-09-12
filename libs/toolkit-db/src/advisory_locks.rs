//! Advisory locks with namespacing and retry policies.
//!
//! Backends:
//! - **`PostgreSQL`** (`pg`): session-level `pg_try_advisory_lock` on a pinned pool connection
//! - **`MySQL`** (`mysql`): session-level `GET_LOCK(name, 0)` on a pinned pool connection
//! - **`SQLite` / fallback**: file marker locks under the OS cache dir
//!
//! ## Public API semantics
//!
//! - [`LockManager::lock`] — exactly one non-blocking acquisition attempt.
//!   Contention → [`DbLockError::AlreadyHeld`]. On PG/MySQL, SQL/protocol errors map to
//!   `DbLockError::Database`.
//! - [`LockManager::try_lock`] — owns retry/backoff; returns `Ok(None)` when limits are exhausted.
//!   `max_wait` bounds retry scheduling and time between **completed** acquisition attempts. It
//!   does **not** currently impose a cancellation-safe timeout on pool acquisition or an
//!   in-flight database query.
//! - Native backends never use blocking `pg_advisory_lock` or `GET_LOCK` with a positive timeout.
//!
//! ## Single-session model (native backends)
//!
//! Each PG/MySQL [`LockManager`] owns a **dedicated pool sized `max=1`/`min=1`** with idle and
//! lifetime reaping disabled. One database session therefore holds **all** advisory keys for the
//! process. A held lock does **not** pin the connection: `pg_try_advisory_lock` /
//! `GET_LOCK(_, 0)` return immediately and the session merely remembers the key, so the single
//! connection is free to service the next lock/unlock op. Guards carry only a cheap
//! `Arc<..Session>` handle plus the key — never a `PoolConnection`.
//!
//! The lock session is established **lazily**, on the first `.lock()` / `.try_lock()` call (see
//! `LockSource`). A `DbHandle` that never takes an advisory lock opens no
//! extra connection and spawns no keepalive task — important where many gears point at the same
//! connection-limited database. The cost is that connection failures for the lock session surface
//! from the first lock attempt (as `DbLockError::Database`) rather than at `connect()` time.
//!
//! ### Reconnect accounting (generation epoch)
//!
//! If the single connection dies, sqlx re-establishes a new backend and **all** advisory locks
//! held by the old session are gone server-side. To avoid acting on stale "I hold key X"
//! beliefs, the pool's `after_connect` callback bumps a shared [`AtomicU64`] generation on every
//! newly established physical connection (initial + each reconnect). Each [`DbLockGuard`] records
//! the generation observed right after its acquire; on release, if the current generation no
//! longer matches, the lock was already released by the disconnect and the unlock SQL is skipped.
//!
//! ### Keepalive + unlock draining
//!
//! A background maintenance task keeps the session warm with a periodic `SELECT 1` (defeating
//! proxy/PG idle-killers and bounding dead-session detection latency) and drains a pending-unlock
//! queue. [`DbLockGuard`] `Drop` performs a non-blocking, runtime-free send of `(key, generation)`
//! into that queue instead of running unlock SQL inline; the task performs the generation-checked
//! `pg_advisory_unlock` / `RELEASE_LOCK`. This makes `Drop` panic-free even after the runtime has
//! shut down and removes the previous `close_on_drop` / `mem::forget` connection-fate machinery.
//! The task stops when the last [`LockManager`] / `DbHandle` clone drops the shared `Arc<..Session>`.
//!
//! Prefer awaiting [`DbLockGuard::release`] on the normal path for deterministic unlock; `Drop`
//! is best-effort via the queue.
//!
//! ### In-process exclusivity (held-key registry)
//!
//! Session advisory locks are **re-entrant**: `pg_try_advisory_lock` / `GET_LOCK` grant a key the
//! same session already holds and just bump the server's hold count. Since one session now serves
//! the whole process, the database can no longer express in-process exclusivity on its own, so each
//! session keeps a registry of claimed keys (see `HeldKeys`) that is consulted before the SQL runs.
//!
//! A claim is released only where the server is **known** to no longer hold the key: a confirmed
//! unlock, a lost session, or a generation bump. When an unlock fails on a live session the key
//! stays claimed, because PG still holds it and re-acquiring would raise the hold count to two —
//! a leak no release could balance. Claims are recorded per generation, so such a claim is not
//! permanent: the next reconnect drops every server-side lock and frees the key.
//!
//! One consequence for native backends: after `Drop`, re-acquiring the same key in the same process
//! fails until the maintenance task has drained that unlock. Await [`DbLockGuard::release`] when the
//! key is re-acquired immediately.
//!
//! **Connection pooling caveat:** session advisory locks require a real session. A
//! transaction-pooling proxy (e.g. `PgBouncer` in `transaction` mode) breaks them entirely — use
//! session pooling or a direct connection for the lock pool.
//!
//! ### File-marker backend limits
//!
//! The SQLite/fallback backend uses an exclusive create of a marker file (not `fs2` kernel
//! locks). Process termination or cancellation during filesystem acquisition may leave a stale
//! marker — the file backend does not provide kernel-owned lock cleanup.
//!
//! After `open(...).await` returns successfully, ownership is transferred to the guard without
//! another await point. Cancellation while the filesystem open itself is in flight may still
//! leave a marker, depending on Tokio blocking-filesystem cancellation behavior.
//!
//! Implicit [`DbLockGuard`] Drop removes the marker **synchronously** (file cleanup does not
//! depend on a spawned Tokio task). Explicit [`DbLockGuard::release`] for the file backend is
//! likewise synchronous after taking ownership.
//!
//! #### Recovering a stale marker
//!
//! There is **no automatic reclaim**: a marker left by `SIGKILL`, a power loss, or cancellation
//! mid-`open` keeps the key un-acquirable until it is deleted. This is deliberate — the two
//! automatic policies on offer are both unsound here. A TTL cannot be chosen safely because the
//! library does not know how long a legitimate holder may hold a key, and PID liveness checks
//! misfire once PIDs are reused; either one can reclaim a *live* lock, which is a worse failure
//! than a stuck one. The file backend is only ever selected for `SQLite`, so the participants are
//! processes on one machine sharing one database file — a human (or the service's own start-up
//! script) can see the whole picture that a TTL cannot.
//!
//! The operational fallback is to remove the marker while no holder is running:
//!
//! ```text
//! <cache_dir>/cf-gears/locks/{database_scope:016x}/{xxh3_64(canonical_lock_input):016x}.lock
//! ```
//!
//! `<cache_dir>` is `dirs::cache_dir()` (falling back to the temp dir), e.g.
//! `%LOCALAPPDATA%` on Windows and `~/.cache` on Linux. Deleting the whole
//! `cf-gears/locks/{database_scope:016x}` directory clears every key for one database and is the
//! usual recovery step; deleting `cf-gears/locks` clears all of them. Doing this while a holder
//! is alive drops that holder's mutual exclusion without telling it, so scope the deletion to a
//! window where the participating processes are down.
//!
//! If crash recovery ever needs to be automatic, the sound mechanism is kernel-owned locks
//! (`flock` / `LockFileEx`), which the OS releases on process death — not a TTL.
//!
//! ## Semver note
//!
//! This module introduces a **breaking** public API change versus prior `0.8.x` releases:
//! [`DbLockGuard::release`] now returns [`Result`], and `DbLockError` (including
//! `Database(sqlx::Error)` when `pg`/`mysql` features are enabled) is publicly re-exported.
//! Additionally, the public `lock_keepalive: Option<Duration>` field was added to both
//! [`crate::ConnectOpts`] and [`crate::config::DbConnConfig`]; neither type is `#[non_exhaustive]`,
//! so this is source-breaking for callers that build them with an exhaustive struct literal
//! (config deserialization stays compatible via `#[serde(default)]`). Bump the crate major/minor
//! appropriately when publishing (intended: `0.9.0`). Local path/patch validation against consumers
//! pinned to `0.8.4` may keep the package version at `0.8.4` until a coordinated Gears release.
//!
//! ## Stable lock namespace
//!
//! ```text
//! canonical_lock_input =
//!   "cf-gears-toolkit-db:v2:{database_scope:016x}:g{gear_utf8_len}:{gear}:k{key_utf8_len}:{key}"
//! ```
//!
//! Length prefixes are UTF-8 byte lengths so `gear`/`key` values that contain `:` cannot collide
//! (e.g. `("a:b","c")` ≠ `("a","b:c")`). The `v2` prefix replaces the ambiguous `v1` colon-joined
//! encoding.
//!
//! `database_scope` is a cross-pod stable fingerprint of host + port + database name (no password,
//! no pod/PID, no implicit `PostgreSQL` `search_path`). File lock paths are derived from the same
//! scope + canonical input (raw DSN does not independently participate in the final path).

#![cfg_attr(
    not(any(feature = "pg", feature = "mysql", feature = "sqlite")),
    allow(unused_imports, unused_variables, dead_code, unreachable_code)
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::fs::File;
use xxhash_rust::xxh3::xxh3_64;

#[cfg(any(feature = "pg", feature = "mysql"))]
use std::sync::Arc;

/// Default keepalive ping interval for the dedicated lock session.
///
/// Matches the reference production value; overridable per connection via config.
pub const DEFAULT_LOCK_KEEPALIVE: Duration = Duration::from_secs(5);

/// Consecutive keepalive-ping failures before the maintenance task escalates from `debug` to
/// `warn`. The lock session holds every advisory key for the process, so sustained ping failure
/// signals the whole lock subsystem is degraded and must be visible at normal log levels.
#[cfg(any(feature = "pg", feature = "mysql"))]
const KEEPALIVE_WARN_THRESHOLD: u32 = 3;

/// Reject a zero keepalive interval before it reaches `tokio::time::interval` (which panics on
/// zero). The public default is resolved once by the caller via `unwrap_or(DEFAULT_LOCK_KEEPALIVE)`;
/// a zero reaching here is an explicit misconfiguration, not a request for the default.
#[cfg(any(feature = "pg", feature = "mysql"))]
fn validate_keepalive(keepalive: Duration) -> Result<Duration, DbLockError> {
    if keepalive.is_zero() {
        return Err(DbLockError::InvalidConfig {
            message: "lock keepalive interval must be greater than zero".to_owned(),
        });
    }
    Ok(keepalive)
}

// --------------------------- Config ------------------------------------------

/// Configuration for lock acquisition attempts.
///
/// The backoff/jitter fields use the workspace-wide retry vocabulary and map
/// straight onto [`tokio_retry::strategy::ExponentialBackoff`]. `max_wait` and
/// `max_retries` are lock-specific termination knobs layered on top.
#[derive(Debug, Clone)]
pub struct LockConfig {
    /// Bounds retry scheduling and time between completed acquisition attempts (`None` = unlimited).
    ///
    /// Does **not** cancel an in-flight `pool.acquire()` or database query; those are limited by
    /// the sqlx/pool/database timeouts instead.
    pub max_wait: Option<Duration>,
    /// Maximum retries after the first attempt (`None` = unlimited, bounded
    /// only by `max_wait`).
    pub max_retries: Option<u32>,
    /// [`ExponentialBackoff`](tokio_retry::strategy::ExponentialBackoff) base —
    /// the growth ratio between retry delays.
    pub backoff_base_ms: u64,
    /// Multiplicative factor applied to every retry delay.
    pub backoff_factor: u64,
    /// Upper bound on any single retry delay (cap for exponential backoff).
    pub max_backoff: Duration,
    /// Apply full jitter to each retry delay.
    pub jitter: bool,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            max_wait: Some(Duration::from_secs(30)),
            max_retries: None,
            // base 2 × factor 25 → 50ms, 100ms, 200ms, … (doubling), capped at
            // `max_backoff`.
            backoff_base_ms: 2,
            backoff_factor: 25,
            max_backoff: Duration::from_secs(5),
            jitter: true,
        }
    }
}

impl LockConfig {
    /// Validate configuration before the first acquisition attempt.
    ///
    /// # Errors
    /// Returns [`DbLockError::InvalidConfig`] when values would panic or produce nonsense backoff.
    pub fn validate(&self) -> Result<(), DbLockError> {
        // Each of these collapses every `ExponentialBackoff` delay to zero, turning the retry
        // loop into a busy spin until `max_wait` expires. `tokio-retry` does not reject them,
        // so catch them before the first attempt.
        if self.backoff_base_ms == 0 {
            return Err(DbLockError::InvalidConfig {
                message: "backoff_base_ms must be greater than zero".to_owned(),
            });
        }
        if self.backoff_factor == 0 {
            return Err(DbLockError::InvalidConfig {
                message: "backoff_factor must be greater than zero".to_owned(),
            });
        }
        if self.max_backoff.is_zero() {
            return Err(DbLockError::InvalidConfig {
                message: "max_backoff must be greater than zero".to_owned(),
            });
        }
        // `max_retries: Some(0)` is valid: one attempt, no retries.
        Ok(())
    }
}

/// Outcome of a single `try_acquire_once` used to drive the retry loop:
/// `Pending` is the retryable "held elsewhere, try again" signal.
enum TryLockError {
    Pending,
    Fatal(DbLockError),
}

// --------------------------- Scope / key helpers -----------------------------

const CANONICAL_PREFIX: &str = "cf-gears-toolkit-db:v2";

/// Canonical lock input shared by all backends.
///
/// Uses UTF-8 length-prefixed `gear`/`key` fields so values containing `:` cannot collide.
#[must_use]
pub(crate) fn canonical_lock_input(database_scope: u64, gear: &str, key: &str) -> String {
    format!(
        "{CANONICAL_PREFIX}:{database_scope:016x}:g{}:{gear}:k{}:{key}",
        gear.len(),
        key.len(),
    )
}

/// Build a cross-pod-stable database scope fingerprint.
///
/// Identity must be identical for all peers coordinating on the same logical database.
/// Do **not** include pod hostname, PID, `instance_id`, passwords, or `PostgreSQL` `search_path`.
#[must_use]
pub(crate) fn database_scope_from_identity(identity: &str) -> u64 {
    xxh3_64(identity.as_bytes())
}

/// Server-style identity: `scheme://host:port/database` (no credentials).
///
/// Normalizes the two ways equivalent DSNs differ textually so peers still land on one scope:
/// hostnames are case-insensitive per DNS, and a trailing `/` on the database path is not part of
/// the name. Normalizing here rather than in [`database_scope_from_dsn`] keeps the DSN-parsed and
/// typed-options entry points (which pass `opts.get_host()` straight through) in agreement.
///
/// TODO(application-namespace): optionally append an explicit application namespace from
/// `DbOptions` when present, so independent apps sharing one database do not collide on
/// advisory locks. Until then, peers on the same host/port/database share one lock space.
#[must_use]
pub(crate) fn server_database_identity(
    scheme: &str,
    host: &str,
    port: u16,
    database: &str,
) -> String {
    let host = host.to_ascii_lowercase();
    let database = database.trim_end_matches('/');
    format!("{scheme}://{host}:{port}/{database}")
}

/// Parse a DSN into a non-secret scope identity string when possible.
#[must_use]
pub(crate) fn database_scope_from_dsn(dsn: &str) -> u64 {
    let trimmed = dsn.trim_start();
    if let Ok(url) = url::Url::parse(trimmed) {
        let scheme = url.scheme();
        if scheme == "postgres" || scheme == "postgresql" || scheme == "mysql" {
            let host = url.host_str().unwrap_or("");
            let port =
                url.port_or_known_default()
                    .unwrap_or(if scheme == "mysql" { 3306 } else { 5432 });
            let database = url.path().trim_start_matches('/');
            // Normalize postgres/postgresql so both DSNs share one scope.
            let normalized_scheme = if scheme == "postgresql" {
                "postgres"
            } else {
                scheme
            };
            return database_scope_from_identity(&server_database_identity(
                normalized_scheme,
                host,
                port,
                database,
            ));
        }
        #[cfg(feature = "sqlite")]
        if scheme == "sqlite" {
            return database_scope_from_identity(&crate::sqlite::path::sqlite_scope_identity(
                trimmed,
            ));
        }
    }
    #[cfg(feature = "sqlite")]
    if trimmed.starts_with("sqlite:") {
        return database_scope_from_identity(&crate::sqlite::path::sqlite_scope_identity(trimmed));
    }
    // Unrecognized: fingerprint the DSN string (no password stripping possible).
    database_scope_from_identity(&format!("dsn:{trimmed}"))
}

/// `PostgreSQL` advisory key: XXH3-64 bit pattern as signed `i64`.
#[must_use]
#[cfg_attr(not(feature = "pg"), allow(dead_code))]
pub(crate) fn stable_lock_key(canonical: &str) -> i64 {
    xxh3_64(canonical.as_bytes()).cast_signed()
}

/// `MySQL` `GET_LOCK` name: `cf:` + lowercase zero-padded 16-digit hex XXH3-64.
#[must_use]
#[cfg_attr(not(feature = "mysql"), allow(dead_code))]
pub(crate) fn mysql_lock_name(canonical: &str) -> String {
    format!("cf:{:016x}", xxh3_64(canonical.as_bytes()))
}

#[must_use]
#[cfg_attr(not(any(feature = "pg", feature = "mysql")), allow(dead_code))]
fn key_fingerprint(canonical: &str) -> String {
    format!("{:016x}", xxh3_64(canonical.as_bytes()))
}

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

fn new_instance_id() -> u64 {
    let counter = NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
    let seed = format!(
        "{}:{}:{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        counter,
    );
    xxh3_64(seed.as_bytes())
}

// --------------------------- Lock session ------------------------------------

/// Keys currently claimed by a live guard on one shared native session.
///
/// Both `pg_try_advisory_lock` and `GET_LOCK` are **session-scoped and re-entrant**: a second
/// acquire of a key the *same session* already holds succeeds and merely bumps the server's hold
/// count. Because every guard of a [`LockManager`] shares one session, the database alone can no
/// longer express in-process exclusivity — without this registry a second `lock()` for a held key
/// would hand out a second guard instead of [`DbLockError::AlreadyHeld`], and the first release
/// would only decrement the count, leaving the key held server-side with no owner.
///
/// The claim is the map insert: it happens under the mutex with no `await` in between, so two tasks
/// racing for the same key cannot both reach the SQL. This restores the exclusive semantics the
/// file backend has always had.
///
/// ### Why claims outlive their guard
///
/// A claim is dropped only where the server-side hold is *known* to be gone. If the unlock cannot
/// be confirmed while the session is still alive — the SQL errored, or `RELEASE_LOCK` did not
/// report ownership — the database still holds the key, so the key stays claimed. Releasing it
/// early would let this process re-acquire re-entrantly and bump the server's hold count to two,
/// turning a recoverable failed unlock into a hold no release can ever balance.
///
/// That is why each claim records the session generation it was made under. A retained claim would
/// otherwise be permanent; instead it is scoped to one physical connection. When the session
/// reconnects every lock it held is gone server-side, so a claim from an older generation is stale
/// and the next claimant takes it over.
#[cfg(any(feature = "pg", feature = "mysql"))]
#[derive(Debug, Default)]
struct HeldKeys<K: Eq + std::hash::Hash>(std::sync::Mutex<std::collections::HashMap<K, u64>>);

#[cfg(any(feature = "pg", feature = "mysql"))]
impl<K: Eq + std::hash::Hash> HeldKeys<K> {
    fn new() -> Self {
        Self(std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    /// Claim `key` under `generation`.
    ///
    /// Returns `false` only when a claim made under the *current* generation is still standing. A
    /// claim from an older generation belongs to a connection that has since died — its locks are
    /// gone server-side — so it is stale and gets taken over.
    fn claim(&self, key: K, generation: u64) -> bool {
        match self.guard().entry(key) {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                if *held.get() == generation {
                    false
                } else {
                    held.insert(generation);
                    true
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(generation);
                true
            }
        }
    }

    /// Give up a claim **only if** the entry is still held under `generation`.
    ///
    /// After a reconnect a newer generation may have taken the entry over (see [`claim`]). Cleanup
    /// paths run on behalf of the generation that made the claim, and that generation may already be
    /// stale by the time they execute (guard dropped, queued unlock drained, or session reconnected
    /// mid-release). Removing the entry unconditionally would drop a *newer*, still-live claim and
    /// let a third acquisition succeed re-entrantly on the shared session, breaking mutual exclusion.
    /// Only the generation that owns the entry may retract it.
    fn unclaim_if_generation<Q>(&self, key: &Q, generation: u64)
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + std::hash::Hash + ?Sized,
    {
        let mut map = self.guard();
        if map.get(key) == Some(&generation) {
            map.remove(key);
        }
    }

    /// A poisoned registry only means some other caller panicked; the map itself stays consistent,
    /// so recover the data rather than propagating the panic into every later lock attempt.
    fn guard(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<K, u64>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// RAII retraction for a claim taken *before* the acquire query is awaited.
///
/// `try_lock_generic` claims the key *before* awaiting the backend's acquire SQL (`pg_try_advisory_lock` /
/// `GET_LOCK`), so that in-process exclusivity on the shared session is decided synchronously. If the
/// enclosing future is cancelled at that await — ordinary usage such as `tokio::time::timeout` or
/// the `tokio::select!` cooperative-shutdown pattern the crate's own docs recommend — no
/// [`DbLockGuard`] is ever produced to retract the claim, so the key would be stranded until the next
/// reconnect (a self-inflicted, undetectable lockout). Because Rust drops all live locals when a
/// future is cancelled at any `.await`, holding the claim through this guard makes retraction
/// unconditional on cancellation. It is [`disarm`](Self::disarm)ed only once ownership of the key is
/// handed to the returned `DbLockGuard`.
#[cfg(any(feature = "pg", feature = "mysql"))]
struct ClaimGuard<'a, K: Eq + std::hash::Hash> {
    held_keys: &'a HeldKeys<K>,
    generation: u64,
    key: Option<K>,
}

#[cfg(any(feature = "pg", feature = "mysql"))]
impl<'a, K: Eq + std::hash::Hash> ClaimGuard<'a, K> {
    /// Arm retraction for `key`, already claimed under `generation`.
    fn new(held_keys: &'a HeldKeys<K>, key: K, generation: u64) -> Self {
        Self {
            held_keys,
            generation,
            key: Some(key),
        }
    }

    /// Ownership of the key has passed to the new [`DbLockGuard`]: cancel the retraction so drop is a
    /// no-op.
    fn disarm(mut self) {
        self.key = None;
    }
}

#[cfg(any(feature = "pg", feature = "mysql"))]
impl<K: Eq + std::hash::Hash> Drop for ClaimGuard<'_, K> {
    fn drop(&mut self) {
        // Generation-checked so a cancellation that races a reconnect cannot drop a newer claim.
        if let Some(key) = self.key.take() {
            self.held_keys.unclaim_if_generation(&key, self.generation);
        }
    }
}

/// Classify an `sqlx` error as "the session is gone" vs a genuine database error.
///
/// A dropped session cannot still hold an advisory lock, so on a connection-lost error the caller
/// treats the key as already released. Everything else is a real error the server answered with.
///
/// Deliberately **not** connection-lost: [`sqlx::Error::PoolTimedOut`]. With `max_connections(1)`
/// a busy lock session makes `acquire()` time out routinely while the session is alive and still
/// holding the key. Treating that as "released" would skip the unlock and report success, leaking
/// the key server-side until the session closes.
#[cfg(any(feature = "pg", feature = "mysql"))]
fn is_connection_lost(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolClosed => true,
        // PostgreSQL: connection_exception class `08*`; admin shutdown / crash `57P01/02/03`.
        sqlx::Error::Database(db) => db.code().as_deref().is_some_and(|code| {
            code.starts_with("08") || matches!(code, "57P01" | "57P02" | "57P03")
        }),
        _ => false,
    }
}

// --- Native backend (generic core) --------------------------------------------
//
// PostgreSQL and MySQL share one generic session/pool/drain/maintenance/release/acquire core.
// Everything genuinely per-DB — the SQL strings, the lock-key type, and the result parsing —
// lives behind the [`NativeBackend`] trait, in the two marker impls (`PgBackend`, `MySqlBackend`).
// Keeping every concrete `query_scalar`/`bind` inside those impls (where `Db` is a concrete type)
// is what keeps sqlx's `Encode`/`Type`/`Executor` bounds out of the generic core.

/// Shared result of an acquire attempt, mapped from each backend's native return shape.
#[cfg(any(feature = "pg", feature = "mysql"))]
enum TryOutcome {
    Acquired,  // PG `Ok(true)`  / MySQL `Ok(Some(1))`
    Contended, // PG `Ok(false)` / MySQL `Ok(Some(0))`
    // Only MySQL's `GET_LOCK` can return a value the acquire path cannot interpret.
    #[cfg_attr(not(feature = "mysql"), allow(dead_code))]
    Unexpected(String), // MySQL `Ok(None)` / `Ok(Some(other))`
}

/// Shared result of a release attempt, mapped from each backend's native return shape.
#[cfg(any(feature = "pg", feature = "mysql"))]
enum ReleaseOutcome {
    Released, // PG `Ok(true)`  / MySQL `Ok(Some(1))`         -> unclaim, Ok(())
    NotHeld,  // PG `Ok(false)` / MySQL `Ok(Some(0))`|`None`  -> unclaim, Err(NotHeld)
    // Only MySQL's `RELEASE_LOCK` can return a value the release path cannot interpret.
    #[cfg_attr(not(feature = "mysql"), allow(dead_code))]
    Unexpected(String), // MySQL `Ok(Some(other))`                      -> keep claim, Err(Unexpected)
}

/// Per-DB specialization for the native advisory-lock backends.
///
/// The generic core drives connection acquisition, the generation epoch, the held-key registry, and
/// error classification; the impl supplies the SQL and interprets its result into the shared
/// [`TryOutcome`] / [`ReleaseOutcome`] enums. Every `query_scalar`/`bind` stays in the impl so the
/// core never has to name a value type's sqlx bounds.
#[cfg(any(feature = "pg", feature = "mysql"))]
trait NativeBackend: Sized + Send + Sync + 'static {
    type Db: sqlx::Database;
    type Key: Eq + std::hash::Hash + Clone + Send + Sync + std::fmt::Debug + 'static;
    type ConnectOptions: sqlx::ConnectOptions<Connection = <Self::Db as sqlx::Database>::Connection>
        + Clone
        + std::fmt::Debug;

    /// Structured `backend=` log field.
    const BACKEND: &'static str;
    /// Acquire SQL function name, used to reconstruct log messages in the generic core.
    const ACQUIRE_FN: &'static str;
    /// Unlock SQL function name, used to reconstruct log messages in the generic core.
    const UNLOCK_FN: &'static str;

    /// Derive the backend-native lock key from the canonical lock input.
    fn lock_key(canonical: &str) -> Self::Key;

    /// Bridge into the concrete [`GuardInner`] enum (the one non-generic seam).
    fn build_guard_inner(
        session: Arc<Session<Self>>,
        key: Self::Key,
        generation: u64,
        key_fingerprint: String,
    ) -> GuardInner;

    /// Dedicated single-connection lock pool: no reaping, generation-bumping `after_connect`, and a
    /// liveness ping before hand-out so a dead session forces a reconnect. DB-agnostic, hence a
    /// provided default both backends inherit unchanged.
    fn pool_options(generation: Arc<AtomicU64>) -> sqlx::pool::PoolOptions<Self::Db> {
        sqlx::pool::PoolOptions::<Self::Db>::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .test_before_acquire(true)
            .after_connect(move |_conn, _meta| {
                let generation = Arc::clone(&generation);
                Box::pin(async move {
                    // Release-ordered so a subsequent Acquire-load after `acquire()` observes the bump.
                    generation.fetch_add(1, Ordering::Release);
                    Ok(())
                })
            })
    }

    /// Classify an `sqlx` error as "the session is gone". Defaults to the shared classifier; a
    /// backend may override to recognize its own transport codes.
    // TODO: MySQL could override to also treat `08S01` / `CR_SERVER_LOST` (2006/2013) as
    // connection-lost. Left as default for now — changing it changes when a key is silently dropped.
    fn is_connection_lost(error: &sqlx::Error) -> bool {
        is_connection_lost(error)
    }

    /// Run the non-blocking acquire SQL and map its native result to [`TryOutcome`].
    fn try_acquire(
        conn: &mut <Self::Db as sqlx::Database>::Connection,
        key: &Self::Key,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<TryOutcome, sqlx::Error>> + Send;

    /// Run the unlock SQL for deterministic release and map its native result to [`ReleaseOutcome`].
    fn run_release(
        conn: &mut <Self::Db as sqlx::Database>::Connection,
        key: &Self::Key,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<ReleaseOutcome, sqlx::Error>> + Send;

    /// Run the unlock SQL for a queued (drop-path) release. `Ok(())` means the server no longer holds
    /// it for us (confirmed unlock, not-owned, or no-such-lock), so the core drops the claim.
    fn run_drain_unlock(
        conn: &mut <Self::Db as sqlx::Database>::Connection,
        key: &Self::Key,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<(), sqlx::Error>> + Send;

    /// Keepalive `SELECT 1` on the lock pool. Lives on the trait (not the generic core) because
    /// `&Pool<DB>: Executor` carries bounds that don't hold for a generic `DB`; here `Db` is concrete.
    fn ping(
        pool: &sqlx::Pool<Self::Db>,
    ) -> impl std::future::Future<Output = Result<(), sqlx::Error>> + Send;
}

/// Queued unlock request produced by [`DbLockGuard`] `Drop`.
#[cfg(any(feature = "pg", feature = "mysql"))]
#[derive(Debug)]
struct PendingUnlock<B: NativeBackend> {
    key: B::Key,
    generation: u64,
    key_fingerprint: String,
}

/// Dedicated single-connection lock session shared by all guards of one [`LockManager`].
///
/// The single session holds **all** advisory keys for the process; a held lock does not pin the
/// connection (the acquire SQL returns immediately and the session just remembers the key), so the
/// one connection is free to service the next op. Guards carry only a cheap `Arc<Session<B>>` plus
/// the key.
#[cfg(any(feature = "pg", feature = "mysql"))]
#[derive(Debug)]
struct Session<B: NativeBackend> {
    pool: sqlx::Pool<B::Db>,
    generation: Arc<AtomicU64>,
    unlock_tx: tokio::sync::mpsc::UnboundedSender<PendingUnlock<B>>,
    shutdown: Arc<tokio::sync::Notify>,
    /// Keys claimed on this session — see [`HeldKeys`]. Shared with the maintenance task, which
    /// unclaims a key once its queued unlock is confirmed.
    held_keys: Arc<HeldKeys<B::Key>>,
    // Only ever accessed from `Drop` under `&mut self`, so no shared-mutability wrapper is needed.
    task: Option<tokio::task::JoinHandle<()>>,
}

#[cfg(any(feature = "pg", feature = "mysql"))]
impl<B: NativeBackend> Drop for Session<B> {
    fn drop(&mut self) {
        // Last shared clone is going away: signal the maintenance task to shut down. It drains any
        // remaining queued unlocks, then exits and drops its pool clone; dropping our own `pool`
        // clone afterwards closes the physical session, which releases every advisory lock
        // server-side. We deliberately do not `abort()` the task — a hard abort would cut the
        // graceful drain short, and connection close already guarantees server-side cleanup.
        self.shutdown.notify_one();
        drop(self.task.take());
    }
}

/// How to reach the database for the dedicated lock connection.
#[cfg(any(feature = "pg", feature = "mysql"))]
#[derive(Debug)]
enum ConnectSpec<B: NativeBackend> {
    Dsn(String),
    Options(Box<B::ConnectOptions>),
}

/// Lazily-established lock session.
///
/// The dedicated single-connection lock pool and its maintenance task are expensive — one extra
/// connection permanently reserved from the (often connection-limited) target database plus a
/// keepalive ping — and most handles never take an advisory lock. So establishment is deferred to
/// the first [`session`](Self::session) call (i.e. the first `.lock()`/`.try_lock()`) via a
/// [`OnceCell`]; a handle that never locks pays nothing. A failed connect leaves the cell empty so
/// the next attempt retries rather than caching the error. The cell lives behind an `Arc` shared by
/// every [`LockManager`] clone, so at most one session is ever built.
#[cfg(any(feature = "pg", feature = "mysql"))]
#[derive(Debug)]
struct LockSource<B: NativeBackend> {
    spec: ConnectSpec<B>,
    keepalive: Duration,
    cell: tokio::sync::OnceCell<Arc<Session<B>>>,
}

#[cfg(any(feature = "pg", feature = "mysql"))]
impl<B: NativeBackend> LockSource<B>
where
    // Ties the associated options type to the one `PoolOptions::connect_with` expects.
    <B::Db as sqlx::Database>::Connection: sqlx::Connection<Options = B::ConnectOptions>,
{
    async fn session(&self) -> Result<&Arc<Session<B>>, DbLockError> {
        self.cell
            .get_or_try_init(|| async {
                let generation = Arc::new(AtomicU64::new(0));
                let pool = match &self.spec {
                    ConnectSpec::Dsn(dsn) => {
                        B::pool_options(Arc::clone(&generation))
                            .connect(dsn)
                            .await?
                    }
                    ConnectSpec::Options(opts) => {
                        B::pool_options(Arc::clone(&generation))
                            .connect_with((**opts).clone())
                            .await?
                    }
                };
                Ok(Arc::new(build_session::<B>(
                    pool,
                    generation,
                    self.keepalive,
                )))
            })
            .await
    }
}

/// Assemble a [`Session`] (channel + maintenance task) from an already-connected lock pool.
#[cfg(any(feature = "pg", feature = "mysql"))]
fn build_session<B: NativeBackend>(
    pool: sqlx::Pool<B::Db>,
    generation: Arc<AtomicU64>,
    keepalive: Duration,
) -> Session<B> {
    let (unlock_tx, unlock_rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let held_keys = Arc::new(HeldKeys::new());
    let task = spawn_maintenance::<B>(
        pool.clone(),
        Arc::clone(&generation),
        Arc::clone(&held_keys),
        unlock_rx,
        Arc::clone(&shutdown),
        keepalive,
    );
    Session {
        pool,
        generation,
        unlock_tx,
        shutdown,
        held_keys,
        task: Some(task),
    }
}

/// Generation-checked drain of one queued unlock (best-effort; runs on the maintenance task).
///
/// Owns the in-process claim released by [`DbLockGuard`] `Drop`: it is dropped here, once the key is
/// known to be free server-side. Every early return below is a path where the lock is gone
/// (reconnect / lost session); the two paths that leave the key still held keep the claim, so a
/// pending or failed cleanup cannot be re-acquired re-entrantly. See [`HeldKeys`].
#[cfg(any(feature = "pg", feature = "mysql"))]
async fn drain_unlock<B: NativeBackend>(
    pool: &sqlx::Pool<B::Db>,
    generation: &AtomicU64,
    held_keys: &HeldKeys<B::Key>,
    pending: PendingUnlock<B>,
) {
    if generation.load(Ordering::Acquire) != pending.generation {
        held_keys.unclaim_if_generation(&pending.key, pending.generation);
        return; // session reconnected → lock already gone.
    }
    let mut conn = match pool.acquire().await {
        Ok(conn) => conn,
        Err(error) if B::is_connection_lost(&error) => {
            held_keys.unclaim_if_generation(&pending.key, pending.generation);
            return;
        }
        Err(error) => {
            // Session is alive and still holds the key: keep the claim (scoped to this generation).
            tracing::warn!(backend = B::BACKEND, %error, "queued advisory unlock: pool acquire failed");
            return;
        }
    };
    if generation.load(Ordering::Acquire) != pending.generation {
        held_keys.unclaim_if_generation(&pending.key, pending.generation);
        return;
    }
    match B::run_drain_unlock(&mut conn, &pending.key, &pending.key_fingerprint).await {
        Ok(()) => held_keys.unclaim_if_generation(&pending.key, pending.generation),
        Err(error) if B::is_connection_lost(&error) => {
            held_keys.unclaim_if_generation(&pending.key, pending.generation);
        }
        // Unlock unconfirmed on a live session → keep the key claimed.
        Err(error) => tracing::warn!(
            backend = B::BACKEND,
            key_fingerprint = %pending.key_fingerprint,
            %error,
            "queued {} failed; key stays claimed until this session reconnects",
            B::UNLOCK_FN
        ),
    }
}

/// Spawn the maintenance task: keepalive ping + pending-unlock drain until shutdown.
#[cfg(any(feature = "pg", feature = "mysql"))]
fn spawn_maintenance<B: NativeBackend>(
    pool: sqlx::Pool<B::Db>,
    generation: Arc<AtomicU64>,
    held_keys: Arc<HeldKeys<B::Key>>,
    mut unlock_rx: tokio::sync::mpsc::UnboundedReceiver<PendingUnlock<B>>,
    shutdown: Arc<tokio::sync::Notify>,
    keepalive: Duration,
) -> tokio::task::JoinHandle<()> {
    // `keepalive` is already validated non-zero by the session constructor.
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(keepalive);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // consume the immediate first tick.
        let mut ping_failures: u32 = 0;
        loop {
            tokio::select! {
                () = shutdown.notified() => {
                    // Graceful shutdown: drain already-queued unlocks so guards dropped just
                    // before teardown still release their keys before the session connection closes.
                    while let Ok(pending) = unlock_rx.try_recv() {
                        drain_unlock::<B>(&pool, &generation, &held_keys, pending).await;
                    }
                    break;
                }
                // `None` = all senders gone; keep pinging until shutdown.
                maybe = unlock_rx.recv() => if let Some(pending) = maybe {
                    drain_unlock::<B>(&pool, &generation, &held_keys, pending).await;
                },
                _ = ticker.tick() => match B::ping(&pool).await {
                    Ok(()) => ping_failures = 0,
                    Err(error) => {
                        ping_failures = ping_failures.saturating_add(1);
                        if ping_failures >= KEEPALIVE_WARN_THRESHOLD {
                            tracing::warn!(
                                backend = B::BACKEND,
                                %error,
                                consecutive_failures = ping_failures,
                                "advisory-lock keepalive ping failing repeatedly; lock session may be dead"
                            );
                        } else {
                            tracing::debug!(backend = B::BACKEND, %error, "advisory-lock keepalive ping failed");
                        }
                    }
                }
            }
        }
    })
}

/// Single non-blocking acquire on the shared session. A held lock does not pin the connection — it
/// returns to the pool immediately while the session remembers the key.
#[cfg(any(feature = "pg", feature = "mysql"))]
async fn try_lock_generic<B: NativeBackend>(
    session: &Arc<Session<B>>,
    display_key: &str,
    canonical: &str,
) -> Result<Option<DbLockGuard>, DbLockError> {
    let mut conn = match session.pool.acquire().await {
        Ok(conn) => conn,
        // With `max_connections(1)` a busy session makes `acquire()` time out while the connection is
        // alive and serving another lock/unlock op — that is contention, not a fatal error, so report
        // "not acquired" (retryable via `try_lock`).
        Err(sqlx::Error::PoolTimedOut) => return Ok(None),
        Err(error) => return Err(DbLockError::Database(error)),
    };
    // Read AFTER acquire so a reconnect during acquire is reflected in the recorded generation.
    let generation = session.generation.load(Ordering::Acquire);
    let key = B::lock_key(canonical);
    let fingerprint = key_fingerprint(canonical);

    // The session's advisory locks are re-entrant, so in-process exclusivity is decided here, before
    // the SQL. Contended → `None`, exactly like a key held by another process.
    if !session.held_keys.claim(key.clone(), generation) {
        return Ok(None);
    }
    // Own the claim across the await: if this future is cancelled before a `DbLockGuard` takes over,
    // `ClaimGuard`'s Drop retracts it instead of stranding the key until reconnect.
    let claim = ClaimGuard::new(&session.held_keys, key.clone(), generation);

    match B::try_acquire(&mut conn, &key, &fingerprint).await {
        Ok(TryOutcome::Acquired) => {
            claim.disarm(); // ownership passes to the guard
            Ok(Some(DbLockGuard {
                namespaced_key: display_key.to_owned(),
                inner: Some(B::build_guard_inner(
                    Arc::clone(session),
                    key,
                    generation,
                    fingerprint,
                )),
            }))
        }
        // No guard is produced on either path: `claim` retracts on drop.
        Ok(TryOutcome::Contended) => Ok(None),
        Ok(TryOutcome::Unexpected(message)) => {
            Err(DbLockError::UnexpectedDatabaseResult { message })
        }
        Err(error) => {
            tracing::warn!(
                backend = B::BACKEND,
                key_fingerprint = %fingerprint,
                %error,
                "{} failed",
                B::ACQUIRE_FN
            );
            Err(DbLockError::Database(error))
        }
    }
}

/// Generation-checked unlock on the shared session (used by [`DbLockGuard::release`]).
#[cfg(any(feature = "pg", feature = "mysql"))]
async fn release_native_generic<B: NativeBackend>(
    session: &Session<B>,
    key: &B::Key,
    generation: u64,
    key_fingerprint: &str,
) -> Result<(), DbLockError> {
    // The in-process claim is given up only where the server is known to no longer hold the key. If
    // the unlock cannot be confirmed on a live session the key stays claimed — see [`HeldKeys`].
    if session.generation.load(Ordering::Acquire) != generation {
        session.held_keys.unclaim_if_generation(key, generation);
        tracing::debug!(
            backend = B::BACKEND,
            key_fingerprint = %key_fingerprint,
            "release skipped: session reconnected, lock already gone"
        );
        return Ok(());
    }
    let mut conn = match session.pool.acquire().await {
        Ok(conn) => conn,
        Err(error) if B::is_connection_lost(&error) => {
            session.held_keys.unclaim_if_generation(key, generation);
            return Ok(());
        }
        Err(error) => return Err(DbLockError::Database(error)),
    };
    // Re-check after acquire: a reconnect during acquire bumps the generation.
    if session.generation.load(Ordering::Acquire) != generation {
        session.held_keys.unclaim_if_generation(key, generation);
        return Ok(());
    }
    match B::run_release(&mut conn, key, key_fingerprint).await {
        Ok(ReleaseOutcome::Released) => {
            session.held_keys.unclaim_if_generation(key, generation);
            Ok(())
        }
        Ok(ReleaseOutcome::NotHeld) => {
            session.held_keys.unclaim_if_generation(key, generation);
            Err(DbLockError::NotHeld)
        }
        // Unexpected value: cannot conclude the key is free, so keep the claim.
        Ok(ReleaseOutcome::Unexpected(message)) => {
            Err(DbLockError::UnexpectedDatabaseResult { message })
        }
        Err(error) if B::is_connection_lost(&error) => {
            session.held_keys.unclaim_if_generation(key, generation);
            tracing::debug!(
                backend = B::BACKEND,
                key_fingerprint = %key_fingerprint,
                %error,
                "release: session lost; treating lock as released"
            );
            Ok(())
        }
        Err(error) => {
            tracing::warn!(
                backend = B::BACKEND,
                key_fingerprint = %key_fingerprint,
                %error,
                "{} failed; key stays claimed until this session reconnects",
                B::UNLOCK_FN
            );
            Err(DbLockError::Database(error))
        }
    }
}

/// Queue a native guard's unlock on `Drop`: a non-blocking send that needs no Tokio runtime.
///
/// The maintenance task drains the queue and runs the generation-checked unlock SQL, and it is that
/// drain — not this function — that gives up the in-process claim, once the key is known to be free.
/// The claim is only dropped here on the paths where no drain will ever run: the lock is already gone
/// (reconnect), or the queue is closed because the session is being torn down (which closes the
/// connection and releases everything server-side anyway).
#[cfg(any(feature = "pg", feature = "mysql"))]
fn enqueue_native_generic<B: NativeBackend>(
    session: &Arc<Session<B>>,
    key: B::Key,
    generation: u64,
    key_fingerprint: String,
) {
    if session.generation.load(Ordering::Acquire) != generation {
        session.held_keys.unclaim_if_generation(&key, generation);
        return; // reconnected → lock already gone.
    }
    if let Err(err) = session.unlock_tx.send(PendingUnlock {
        key,
        generation,
        key_fingerprint,
    }) {
        session
            .held_keys
            .unclaim_if_generation(&err.0.key, generation);
        tracing::debug!(backend = B::BACKEND, "advisory unlock queue closed on drop");
    }
}

// --- PostgreSQL backend ---

#[cfg(feature = "pg")]
#[derive(Debug, Clone, Copy)]
struct PgBackend;

#[cfg(feature = "pg")]
impl NativeBackend for PgBackend {
    type Db = sqlx::Postgres;
    type Key = i64;
    type ConnectOptions = sqlx::postgres::PgConnectOptions;

    const BACKEND: &'static str = "postgres";
    const ACQUIRE_FN: &'static str = "pg_try_advisory_lock";
    const UNLOCK_FN: &'static str = "pg_advisory_unlock";

    fn lock_key(canonical: &str) -> i64 {
        stable_lock_key(canonical)
    }

    fn build_guard_inner(
        session: Arc<Session<Self>>,
        key: i64,
        generation: u64,
        key_fingerprint: String,
    ) -> GuardInner {
        GuardInner::Postgres {
            session,
            key,
            generation,
            key_fingerprint,
        }
    }

    async fn try_acquire(
        conn: &mut sqlx::PgConnection,
        key: &i64,
        _fingerprint: &str,
    ) -> Result<TryOutcome, sqlx::Error> {
        // `pg_try_advisory_lock` is re-entrant on the session every guard shares.
        if sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(*key)
            .fetch_one(&mut *conn)
            .await?
        {
            Ok(TryOutcome::Acquired)
        } else {
            Ok(TryOutcome::Contended)
        }
    }

    async fn run_release(
        conn: &mut sqlx::PgConnection,
        key: &i64,
        _fingerprint: &str,
    ) -> Result<ReleaseOutcome, sqlx::Error> {
        // `false` means PG was not holding it, so there is no stale hold to guard against.
        if sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(*key)
            .fetch_one(&mut *conn)
            .await?
        {
            Ok(ReleaseOutcome::Released)
        } else {
            Ok(ReleaseOutcome::NotHeld)
        }
    }

    async fn run_drain_unlock(
        conn: &mut sqlx::PgConnection,
        key: &i64,
        fingerprint: &str,
    ) -> Result<(), sqlx::Error> {
        if !sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(*key)
            .fetch_one(&mut *conn)
            .await?
        {
            // `false` means the server was not holding it, so there is nothing left to protect.
            tracing::debug!(
                backend = "postgres",
                key_fingerprint = %fingerprint,
                "queued pg_advisory_unlock returned false"
            );
        }
        Ok(())
    }

    async fn ping(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(pool).await?;
        Ok(())
    }
}

// --- MySQL backend ---

#[cfg(feature = "mysql")]
#[derive(Debug, Clone, Copy)]
struct MySqlBackend;

#[cfg(feature = "mysql")]
impl NativeBackend for MySqlBackend {
    type Db = sqlx::MySql;
    type Key = String;
    type ConnectOptions = sqlx::mysql::MySqlConnectOptions;

    const BACKEND: &'static str = "mysql";
    const ACQUIRE_FN: &'static str = "GET_LOCK";
    const UNLOCK_FN: &'static str = "RELEASE_LOCK";

    fn lock_key(canonical: &str) -> String {
        mysql_lock_name(canonical)
    }

    fn build_guard_inner(
        session: Arc<Session<Self>>,
        key: String,
        generation: u64,
        key_fingerprint: String,
    ) -> GuardInner {
        GuardInner::MySql {
            session,
            key,
            generation,
            key_fingerprint,
        }
    }

    async fn try_acquire(
        conn: &mut sqlx::MySqlConnection,
        key: &String,
        fingerprint: &str,
    ) -> Result<TryOutcome, sqlx::Error> {
        // `GET_LOCK` is re-entrant on the session every guard shares (MySQL 5.7.5+ grants the same
        // name to the same session repeatedly). Timeout 0 — non-blocking only.
        match sqlx::query_scalar::<_, Option<i64>>("SELECT GET_LOCK(?, 0)")
            .bind(key.as_str())
            .fetch_one(&mut *conn)
            .await?
        {
            Some(1) => Ok(TryOutcome::Acquired),
            Some(0) => Ok(TryOutcome::Contended),
            None => {
                tracing::warn!(
                    backend = "mysql",
                    key_fingerprint = %fingerprint,
                    "GET_LOCK returned NULL"
                );
                Ok(TryOutcome::Unexpected("GET_LOCK returned NULL".to_owned()))
            }
            Some(other) => Ok(TryOutcome::Unexpected(format!("GET_LOCK returned {other}"))),
        }
    }

    async fn run_release(
        conn: &mut sqlx::MySqlConnection,
        key: &String,
        fingerprint: &str,
    ) -> Result<ReleaseOutcome, sqlx::Error> {
        match sqlx::query_scalar::<_, Option<i64>>("SELECT RELEASE_LOCK(?)")
            .bind(key.as_str())
            .fetch_one(&mut *conn)
            .await?
        {
            Some(1) => Ok(ReleaseOutcome::Released),
            // `0` = held by another session, `NULL` = no such lock. Either way this session is not
            // the holder, so there is no stale hold of ours to guard against.
            Some(0) => {
                tracing::debug!(
                    backend = "mysql",
                    key_fingerprint = %fingerprint,
                    reason = "not_owned_by_session",
                    "RELEASE_LOCK returned 0"
                );
                Ok(ReleaseOutcome::NotHeld)
            }
            None => {
                tracing::debug!(
                    backend = "mysql",
                    key_fingerprint = %fingerprint,
                    reason = "not_found",
                    "RELEASE_LOCK returned NULL"
                );
                Ok(ReleaseOutcome::NotHeld)
            }
            Some(other) => Ok(ReleaseOutcome::Unexpected(format!(
                "RELEASE_LOCK returned {other}"
            ))),
        }
    }

    async fn run_drain_unlock(
        conn: &mut sqlx::MySqlConnection,
        key: &String,
        fingerprint: &str,
    ) -> Result<(), sqlx::Error> {
        match sqlx::query_scalar::<_, Option<i64>>("SELECT RELEASE_LOCK(?)")
            .bind(key.as_str())
            .fetch_one(&mut *conn)
            .await?
        {
            Some(1) => {}
            // `0` = held by another session, `NULL` = no such lock. Either way *this* session is not
            // the holder, so there is no stale hold of ours to guard against.
            other => tracing::debug!(
                backend = "mysql",
                key_fingerprint = %fingerprint,
                ?other,
                "queued RELEASE_LOCK did not confirm ownership"
            ),
        }
        Ok(())
    }

    async fn ping(pool: &sqlx::MySqlPool) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(pool).await?;
        Ok(())
    }
}

// --------------------------- Guard -------------------------------------------

#[derive(Debug)]
enum GuardInner {
    File {
        path: PathBuf,
        file: File,
    },
    #[cfg(feature = "pg")]
    Postgres {
        session: Arc<Session<PgBackend>>,
        key: i64,
        /// Session generation observed at acquire; a mismatch at release means reconnect-loss.
        generation: u64,
        key_fingerprint: String,
    },
    #[cfg(feature = "mysql")]
    MySql {
        session: Arc<Session<MySqlBackend>>,
        key: String,
        /// Session generation observed at acquire; a mismatch at release means reconnect-loss.
        generation: u64,
        key_fingerprint: String,
    },
}

/// Database lock guard. Prefer [`DbLockGuard::release`]; `Drop` is best-effort only.
#[derive(Debug)]
pub struct DbLockGuard {
    /// Human display key (`"{gear}:{key}"`) — unchanged from prior API.
    namespaced_key: String,
    inner: Option<GuardInner>,
}

impl DbLockGuard {
    /// Lock key with gear namespace (`"gear:key"`).
    #[must_use]
    pub fn key(&self) -> &str {
        &self.namespaced_key
    }

    /// Deterministically release the lock (preferred path).
    ///
    /// # Errors
    /// Returns [`DbLockError`] if unlock fails or the lock was not held.
    // With no native backend the only arm is the synchronous file cleanup, so there is no await.
    #[cfg_attr(
        not(any(feature = "pg", feature = "mysql")),
        allow(clippy::unused_async)
    )]
    pub async fn release(mut self) -> Result<(), DbLockError> {
        let Some(inner) = self.inner.take() else {
            return Ok(());
        };
        match inner {
            // Synchronous: no await after taking ownership, so cancelling `release()` cannot leave a
            // stale marker the way async `remove_file` could.
            GuardInner::File { path, file } => {
                drop(file);
                remove_file_lock_marker_result(&path)?;
            }
            // Native backends run unlock SQL across `.await`s. Hold `inner` in a cleanup token so a
            // cancellation mid-release still enqueues the same runtime-free unlock `Drop` would —
            // otherwise the owned `GuardInner` would just be dropped (it has no cleanup of its own),
            // stranding the server lock and the claim until the next reconnect.
            #[cfg(any(feature = "pg", feature = "mysql"))]
            native => {
                let mut cleanup = ReleaseCleanup {
                    inner: Some(native),
                };
                // `cleanup.inner` is `Some` here by construction; the `None` arm is unreachable but
                // avoids an `expect`. The borrow is held across the await, so a cancellation drops
                // `cleanup` with `inner` still present and its `Drop` enqueues the fallback unlock.
                let result = match cleanup.inner.as_ref() {
                    Some(inner) => release_native(inner).await,
                    None => Ok(()),
                };
                // Reached only if not cancelled: the unlock resolved, so the claim state is already
                // decided — disarm the fallback and surface the real result.
                cleanup.inner = None;
                result?;
            }
        }
        tracing::debug!(key = %self.namespaced_key, "advisory lock released");
        Ok(())
    }
}

/// Cancellation-safety token for [`DbLockGuard::release`] on native backends.
///
/// Holds the guard's `inner` across the release awaits. On normal completion `release` sets `inner`
/// to `None` (disarm); if the release future is cancelled instead, this token is dropped with `inner`
/// still present and its `Drop` performs the same best-effort, runtime-free enqueue as
/// [`DbLockGuard`] `Drop`.
#[cfg(any(feature = "pg", feature = "mysql"))]
struct ReleaseCleanup {
    inner: Option<GuardInner>,
}

#[cfg(any(feature = "pg", feature = "mysql"))]
impl Drop for ReleaseCleanup {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            enqueue_native_unlock(inner);
        }
    }
}

impl Drop for DbLockGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };

        match inner {
            // File markers do not need async cleanup: remove synchronously so runtime shutdown /
            // never-polled / aborted tasks cannot leave an orphan marker.
            GuardInner::File { path, file } => {
                drop(file);
                remove_file_lock_marker(&path);
            }
            // Native backends: non-blocking, runtime-free hand-off to the maintenance task.
            #[cfg(any(feature = "pg", feature = "mysql"))]
            native => enqueue_native_unlock(native),
        }
    }
}

/// Queue a native guard's unlock on `Drop`: a non-blocking send that needs no Tokio runtime.
///
/// The session's maintenance task drains the queue and runs the generation-checked unlock SQL, and
/// it is that drain — not this function — that gives up the in-process claim, once the key is known
/// to be free. The claim is only dropped here on the paths where no drain will ever run: the lock is
/// already gone (reconnect), or the queue is closed because the session is being torn down (which
/// closes the connection and releases everything server-side anyway).
#[cfg(any(feature = "pg", feature = "mysql"))]
fn enqueue_native_unlock(inner: GuardInner) {
    match inner {
        GuardInner::File { path, file } => {
            drop(file);
            remove_file_lock_marker(&path);
        }
        #[cfg(feature = "pg")]
        GuardInner::Postgres {
            session,
            key,
            generation,
            key_fingerprint,
        } => enqueue_native_generic::<PgBackend>(&session, key, generation, key_fingerprint),
        #[cfg(feature = "mysql")]
        GuardInner::MySql {
            session,
            key,
            generation,
            key_fingerprint,
        } => enqueue_native_generic::<MySqlBackend>(&session, key, generation, key_fingerprint),
    }
}

/// Synchronous removal of a file-backend lock marker.
///
/// `NotFound` is treated as success (already released / cleaned up).
fn remove_file_lock_marker_result(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Best-effort synchronous removal used from [`DbLockGuard`] Drop.
fn remove_file_lock_marker(path: &std::path::Path) {
    if let Err(error) = remove_file_lock_marker_result(path) {
        tracing::warn!(
            %error,
            path = %path.display(),
            "failed to remove file lock marker on Drop"
        );
    }
}

// Native unlock SQL for [`DbLockGuard::release`]. Takes `&GuardInner` (not ownership) so the caller
// can keep it inside a [`ReleaseCleanup`] token that survives a cancellation of this future. The
// file backend is handled synchronously by the caller and never reaches here.
#[cfg(any(feature = "pg", feature = "mysql"))]
async fn release_native(inner: &GuardInner) -> Result<(), DbLockError> {
    match inner {
        #[cfg(feature = "pg")]
        GuardInner::Postgres {
            session,
            key,
            generation,
            key_fingerprint,
        } => release_native_generic::<PgBackend>(session, key, *generation, key_fingerprint).await,
        #[cfg(feature = "mysql")]
        GuardInner::MySql {
            session,
            key,
            generation,
            key_fingerprint,
        } => {
            release_native_generic::<MySqlBackend>(session, key, *generation, key_fingerprint).await
        }
        GuardInner::File { .. } => Ok(()),
    }
}

// --------------------------- Lock Manager ------------------------------------

#[derive(Debug, Clone)]
enum LockBackend {
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    File,
    #[cfg(feature = "pg")]
    Postgres(Arc<LockSource<PgBackend>>),
    #[cfg(feature = "mysql")]
    MySql(Arc<LockSource<MySqlBackend>>),
}

/// Internal lock manager handling different database backends.
#[derive(Debug, Clone)]
pub(crate) struct LockManager {
    backend: LockBackend,
    instance_id: u64,
    database_scope: u64,
}

impl LockManager {
    #[must_use]
    #[cfg_attr(not(any(feature = "sqlite", test)), allow(dead_code))]
    pub fn file(database_scope: u64) -> Self {
        Self {
            backend: LockBackend::File,
            instance_id: new_instance_id(),
            database_scope,
        }
    }

    /// Build a PG lock manager from typed options. The dedicated lock connection and its keepalive
    /// task are **not** opened here — they are established lazily on the first `.lock()`/`.try_lock()`
    /// so handles that never lock cost nothing (see [`LockSource`]).
    ///
    /// # Errors
    /// Returns [`DbLockError::InvalidConfig`] if `keepalive` is zero. Connection failures surface
    /// later, from the first lock attempt, as [`DbLockError::Database`].
    #[cfg(feature = "pg")]
    pub fn postgres_lazy(
        opts: sqlx::postgres::PgConnectOptions,
        database_scope: u64,
        keepalive: Duration,
    ) -> Result<Self, DbLockError> {
        Ok(Self::from_pg_source(
            ConnectSpec::Options(Box::new(opts)),
            database_scope,
            validate_keepalive(keepalive)?,
        ))
    }

    /// Build a PG lock manager from a DSN. Lazy — see [`postgres_lazy`](Self::postgres_lazy).
    ///
    /// # Errors
    /// Returns [`DbLockError::InvalidConfig`] if `keepalive` is zero.
    #[cfg(feature = "pg")]
    pub fn postgres_lazy_dsn(
        dsn: &str,
        database_scope: u64,
        keepalive: Duration,
    ) -> Result<Self, DbLockError> {
        Ok(Self::from_pg_source(
            ConnectSpec::Dsn(dsn.to_owned()),
            database_scope,
            validate_keepalive(keepalive)?,
        ))
    }

    #[cfg(feature = "pg")]
    fn from_pg_source(
        spec: ConnectSpec<PgBackend>,
        database_scope: u64,
        keepalive: Duration,
    ) -> Self {
        Self {
            backend: LockBackend::Postgres(Arc::new(LockSource {
                spec,
                keepalive,
                cell: tokio::sync::OnceCell::new(),
            })),
            instance_id: new_instance_id(),
            database_scope,
        }
    }

    /// Build a `MySQL` lock manager from typed options. Lazy — see [`LockSource`].
    ///
    /// # Errors
    /// Returns [`DbLockError::InvalidConfig`] if `keepalive` is zero. Connection failures surface
    /// later, from the first lock attempt, as [`DbLockError::Database`].
    #[cfg(feature = "mysql")]
    pub fn mysql_lazy(
        opts: sqlx::mysql::MySqlConnectOptions,
        database_scope: u64,
        keepalive: Duration,
    ) -> Result<Self, DbLockError> {
        Ok(Self::from_mysql_source(
            ConnectSpec::Options(Box::new(opts)),
            database_scope,
            validate_keepalive(keepalive)?,
        ))
    }

    /// Build a `MySQL` lock manager from a DSN. Lazy — see [`mysql_lazy`](Self::mysql_lazy).
    ///
    /// # Errors
    /// Returns [`DbLockError::InvalidConfig`] if `keepalive` is zero.
    #[cfg(feature = "mysql")]
    pub fn mysql_lazy_dsn(
        dsn: &str,
        database_scope: u64,
        keepalive: Duration,
    ) -> Result<Self, DbLockError> {
        Ok(Self::from_mysql_source(
            ConnectSpec::Dsn(dsn.to_owned()),
            database_scope,
            validate_keepalive(keepalive)?,
        ))
    }

    #[cfg(feature = "mysql")]
    fn from_mysql_source(
        spec: ConnectSpec<MySqlBackend>,
        database_scope: u64,
        keepalive: Duration,
    ) -> Self {
        Self {
            backend: LockBackend::MySql(Arc::new(LockSource {
                spec,
                keepalive,
                cell: tokio::sync::OnceCell::new(),
            })),
            instance_id: new_instance_id(),
            database_scope,
        }
    }

    #[must_use]
    #[allow(dead_code)] // diagnostics / tests
    pub fn database_scope(&self) -> u64 {
        self.database_scope
    }

    #[must_use]
    #[allow(dead_code)] // diagnostics / tests
    pub fn instance_id(&self) -> u64 {
        self.instance_id
    }

    /// Acquire an advisory lock for `{gear}:{key}` with a single non-blocking attempt.
    ///
    /// # Errors
    /// Returns [`DbLockError::AlreadyHeld`] on contention. On PG/MySQL, SQL errors map to
    /// `DbLockError::Database`.
    pub async fn lock(&self, gear: &str, key: &str) -> Result<DbLockGuard, DbLockError> {
        let display_key = format!("{gear}:{key}");
        let canonical = canonical_lock_input(self.database_scope, gear, key);
        match self.try_acquire_once(&display_key, &canonical).await? {
            Some(guard) => {
                tracing::debug!(key = %display_key, "advisory lock acquired");
                Ok(guard)
            }
            None => Err(DbLockError::AlreadyHeld {
                lock_name: display_key,
            }),
        }
    }

    /// Remove the marker a dead holder left behind for `key`, so the key can be
    /// acquired again. Only the file backend keeps markers; `PostgreSQL` and
    /// `MySQL` drop a dead session's locks themselves, so this is a no-op there.
    /// Returns whether a marker was removed.
    ///
    /// No liveness check is made, deliberately: this is for a service's own
    /// start-up path, where the caller knows no holder of `key` is running
    /// (module docs, "Recovering a stale marker").
    ///
    /// # Errors
    /// `DbLockError::Io` when a marker exists but cannot be removed.
    pub async fn break_stale(&self, gear: &str, key: &str) -> Result<bool, DbLockError> {
        if !matches!(self.backend, LockBackend::File) {
            return Ok(false);
        }
        let canonical = canonical_lock_input(self.database_scope, gear, key);
        let path = self.get_lock_file_path(&canonical);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Try to acquire an advisory lock with retry/backoff policy.
    ///
    /// Returns:
    /// - `Ok(Some(guard))` if lock acquired
    /// - `Ok(None)` if timed out or attempts exceeded
    /// - `Err(e)` on unrecoverable error (including invalid config)
    ///
    /// `config.max_wait` bounds retry scheduling between completed attempts; it does not cancel
    /// an in-flight pool acquire or advisory-lock SQL query.
    ///
    /// # Cancellation safety
    ///
    /// This future is cancellation-safe. Dropping it will not leak a lock, whether it is cancelled
    /// between attempts or mid-acquire:
    /// - a guard already produced cleans up on `Drop` — file markers synchronously, native guards by
    ///   queuing a generation-checked unlock drained by the session maintenance task;
    /// - a native acquire cancelled *at* the `pg_try_advisory_lock` / `GET_LOCK` await, before any
    ///   guard exists, retracts its in-process claim via a `ClaimGuard` so the key is not stranded
    ///   until the next reconnect.
    ///
    /// Callers that need cooperative shutdown may wrap the call in `tokio::select!`:
    ///
    /// ```ignore
    /// tokio::select! {
    ///     result = manager.try_lock(gear, key, config) => { /* handle */ }
    ///     _ = cancellation_token.cancelled() => { /* shutdown */ }
    /// }
    /// ```
    pub async fn try_lock(
        &self,
        gear: &str,
        key: &str,
        config: LockConfig,
    ) -> Result<Option<DbLockGuard>, DbLockError> {
        use tokio_retry::RetryIf;
        use tokio_retry::strategy::{ExponentialBackoff, jitter};

        config.validate()?;

        let display_key = format!("{gear}:{key}");
        let canonical = canonical_lock_input(self.database_scope, gear, key);
        let start = Instant::now();

        // Exponential backoff in the shared retry vocabulary, capped at
        // `max_backoff`.
        let jitter_on = config.jitter;
        let max_wait = config.max_wait;
        let strategy = ExponentialBackoff::from_millis(config.backoff_base_ms)
            .factor(config.backoff_factor)
            .max_delay(config.max_backoff)
            // Stop yielding delays once the wall-clock budget is spent — this is
            // what bounds total wait when `max_wait` is set. Pulled between
            // attempts, exactly where the previous loop checked the deadline.
            .take_while(move |_| max_wait.is_none_or(|mw| start.elapsed() < mw))
            // Cap each delay by the remaining budget, then optionally jitter.
            .map(move |d| {
                let capped = max_wait.map_or(d, |mw| d.min(mw.saturating_sub(start.elapsed())));
                if jitter_on { jitter(capped) } else { capped }
            })
            // Retries after the first attempt (unlimited when `None`, bounded
            // only by `max_wait`).
            .take(config.max_retries.map_or(usize::MAX, |r| r as usize));

        // `try_acquire_once` returns `Ok(None)` while the lock is held
        // elsewhere; model that as a retryable sentinel so tokio-retry drives
        // the backoff, and fold the exhausted sentinel back into `Ok(None)`.
        //
        // The attempt counter is an atomic, not a `Cell`, so the returned future stays `Send`
        // and callers can keep spawning `try_lock`.
        let attempts = AtomicU32::new(0);
        let action = || async {
            attempts.fetch_add(1, Ordering::Relaxed);
            match self.try_acquire_once(&display_key, &canonical).await {
                Ok(Some(guard)) => Ok(guard),
                Ok(None) => Err(TryLockError::Pending),
                Err(e) => Err(TryLockError::Fatal(e)),
            }
        };
        let retryable = |e: &TryLockError| matches!(e, TryLockError::Pending);

        match RetryIf::start(strategy, action, retryable).await {
            Ok(guard) => {
                tracing::debug!(
                    key = %display_key,
                    attempt = attempts.load(Ordering::Relaxed),
                    elapsed = ?start.elapsed(),
                    "advisory lock acquired via try_lock"
                );
                Ok(Some(guard))
            }
            Err(TryLockError::Pending) => Ok(None),
            Err(TryLockError::Fatal(e)) => Err(e),
        }
    }

    async fn try_acquire_once(
        &self,
        display_key: &str,
        canonical: &str,
    ) -> Result<Option<DbLockGuard>, DbLockError> {
        match &self.backend {
            LockBackend::File => self.try_lock_file(display_key, canonical).await,
            // Establishes the dedicated lock connection + keepalive on first use, then reuses it.
            #[cfg(feature = "pg")]
            LockBackend::Postgres(source) => {
                let session = source.session().await?;
                try_lock_generic::<PgBackend>(session, display_key, canonical).await
            }
            #[cfg(feature = "mysql")]
            LockBackend::MySql(source) => {
                let session = source.session().await?;
                try_lock_generic::<MySqlBackend>(session, display_key, canonical).await
            }
        }
    }

    async fn try_lock_file(
        &self,
        display_key: &str,
        canonical: &str,
    ) -> Result<Option<DbLockGuard>, DbLockError> {
        let path = self.get_lock_file_path(canonical);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            // Marker creation defines acquisition. Transfer ownership to the guard immediately
            // after a successful open result — no further await in this path. (Cancellation
            // while the open itself is in flight may still leave a marker; see module docs.)
            Ok(file) => Ok(Some(DbLockGuard {
                namespaced_key: display_key.to_owned(),
                inner: Some(GuardInner::File { path, file }),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// File path derived from `database_scope` + hash(canonical). Raw DSN does not participate.
    fn get_lock_file_path(&self, canonical: &str) -> PathBuf {
        let base_dir = if cfg!(test) {
            std::env::temp_dir().join("cf_gears_test_locks")
        } else {
            let cache = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
            cache.join("cf-gears").join("locks")
        };

        let scope_dir = format!("{:016x}", self.database_scope);
        let key_hash = format!("{:016x}", xxh3_64(canonical.as_bytes()));
        base_dir.join(scope_dir).join(format!("{key_hash}.lock"))
    }
}

// --------------------------- Errors ------------------------------------------

#[derive(Error, Debug)]
pub enum DbLockError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[error("Database advisory-lock operation failed: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Lock already held: {lock_name}")]
    AlreadyHeld { lock_name: String },

    #[error("Advisory lock was not held during release")]
    NotHeld,

    #[error("Unexpected database advisory-lock result: {message}")]
    UnexpectedDatabaseResult { message: String },

    #[error("Lock configuration is invalid: {message}")]
    InvalidConfig { message: String },
}

// --------------------------- Tests -------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::sync::Arc;

    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn is_connection_lost_classification() {
        use std::io;

        // Transport-level failures mean the session is gone → treat lock as released.
        assert!(is_connection_lost(&sqlx::Error::Io(io::Error::from(
            io::ErrorKind::BrokenPipe
        ))));
        assert!(is_connection_lost(&sqlx::Error::PoolClosed));

        // A pool timeout does NOT mean the session died: with `max_connections(1)` it can fire
        // while the session is alive and still holding the key, so the unlock must not be skipped.
        assert!(!is_connection_lost(&sqlx::Error::PoolTimedOut));

        // Non-connection sqlx errors are NOT connection-lost.
        assert!(!is_connection_lost(&sqlx::Error::RowNotFound));
    }

    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn validate_keepalive_rejects_zero_and_passes_positive() {
        // Zero would panic `tokio::time::interval`; it must be rejected, not silently defaulted.
        assert!(matches!(
            validate_keepalive(Duration::ZERO),
            Err(DbLockError::InvalidConfig { .. })
        ));
        // A positive interval passes through unchanged (no default re-resolution here).
        let keepalive = Duration::from_millis(250);
        assert_eq!(validate_keepalive(keepalive).unwrap(), keepalive);
    }

    #[test]
    fn stable_lock_key_is_stable() {
        // database_scope = 1 → "0000000000000001"
        // input: UTF-8 of full canonical; XXH3-64; PG key = bit pattern as i64
        let canonical = canonical_lock_input(1, "zoveon", "phone-case");
        assert_eq!(
            canonical,
            "cf-gears-toolkit-db:v2:0000000000000001:g6:zoveon:k10:phone-case"
        );
        assert_eq!(stable_lock_key(&canonical), 7_193_862_067_539_650_702_i64);
        assert_eq!(mysql_lock_name(&canonical), "cf:63d5bb6f8adba88e");
    }

    #[test]
    fn canonical_input_has_no_component_boundary_collisions() {
        let a = canonical_lock_input(1, "a:b", "c");
        let b = canonical_lock_input(1, "a", "b:c");

        assert_ne!(a, b);
        assert_eq!(a, "cf-gears-toolkit-db:v2:0000000000000001:g3:a:b:k1:c");
        assert_eq!(b, "cf-gears-toolkit-db:v2:0000000000000001:g1:a:k3:b:c");
        assert_ne!(stable_lock_key(&a), stable_lock_key(&b));
        assert_ne!(mysql_lock_name(&a), mysql_lock_name(&b));
    }

    #[test]
    fn canonical_has_no_case_normalization() {
        let a = canonical_lock_input(1, "Gear", "Key");
        let b = canonical_lock_input(1, "gear", "key");
        assert_ne!(a, b);
        assert_ne!(stable_lock_key(&a), stable_lock_key(&b));
    }

    /// Hostnames are case-insensitive per DNS and a trailing `/` is not part of the database name,
    /// so peers writing the DSN either way must coordinate on one lock scope.
    #[test]
    fn database_scope_normalizes_host_case_and_trailing_slash() {
        let canonical = database_scope_from_dsn("postgres://db.example:5432/zoveon");
        assert_eq!(
            database_scope_from_dsn("postgres://DB.Example:5432/zoveon"),
            canonical
        );
        assert_eq!(
            database_scope_from_dsn("postgres://db.example:5432/zoveon/"),
            canonical
        );
        // Normalization lives in `server_database_identity`, so the typed-options entry point
        // (which passes `opts.get_host()` straight through) agrees with the parsed-DSN one.
        assert_eq!(
            database_scope_from_identity(&server_database_identity(
                "postgres",
                "DB.Example",
                5432,
                "zoveon/"
            )),
            canonical
        );
    }

    /// Normalization must not blur genuinely different databases.
    #[test]
    fn database_scope_still_separates_distinct_databases() {
        assert_ne!(
            database_scope_from_dsn("postgres://db.example:5432/zoveon"),
            database_scope_from_dsn("postgres://db.example:5432/zoveonx")
        );
    }

    /// The shared native session makes the database's own advisory locks re-entrant, so
    /// [`HeldKeys`] is what preserves exclusivity within one process.
    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn held_keys_claim_is_exclusive_until_unclaimed() {
        let held: HeldKeys<i64> = HeldKeys::new();

        assert!(held.claim(42, 1), "first claim must succeed");
        assert!(
            !held.claim(42, 1),
            "second claim of a held key must be refused"
        );
        // Distinct keys stay independent — one held key must not block the rest.
        assert!(held.claim(43, 1));

        held.unclaim_if_generation(&42, 1);
        assert!(held.claim(42, 1), "key is claimable again after unclaim");
    }

    /// A claim retained because an unlock could not be confirmed must not be permanent: once the
    /// session reconnects, the lock it protected is gone server-side and the key frees up.
    ///
    /// This is what keeps the fail-closed rule in [`HeldKeys`] from deadlocking a key forever.
    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn held_keys_claim_from_a_dead_generation_is_stale() {
        let held: HeldKeys<i64> = HeldKeys::new();

        // Generation 1 holds the key and its unlock failed, so the claim was deliberately kept.
        assert!(held.claim(42, 1));
        assert!(!held.claim(42, 1), "still held while generation 1 is live");

        // Reconnect: generation 2 is a new physical connection, so PG/MySQL dropped every lock the
        // old one held.
        assert!(held.claim(42, 2), "stale claim must be taken over");
        // The key now belongs to generation 2 and excludes again.
        assert!(!held.claim(42, 2));
    }

    /// `MySQL` keys are `String`s; unclaiming borrows as `&str` (no allocation on the release path).
    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn held_keys_unclaims_string_keys_by_borrow() {
        let held: HeldKeys<String> = HeldKeys::new();

        assert!(held.claim("cf:dead".to_owned(), 1));
        assert!(!held.claim("cf:dead".to_owned(), 1));

        held.unclaim_if_generation("cf:dead", 1);
        assert!(held.claim("cf:dead".to_owned(), 1));
    }

    /// Regression for the generation-unaware cleanup race: a stale generation's retraction must not
    /// evict a newer generation's still-live claim, or a third acquire could succeed re-entrantly on
    /// the shared session and break mutual exclusion.
    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn unclaim_if_generation_preserves_a_newer_claim() {
        let held: HeldKeys<i64> = HeldKeys::new();

        // Generation 1 claims, then a reconnect lets generation 2 take the stale entry over.
        assert!(held.claim(42, 1));
        assert!(held.claim(42, 2), "stale gen-1 claim taken over by gen 2");

        // Generation 1's belated cleanup (dropped guard / drained unlock / reconnect branch) must be
        // a no-op: the entry now belongs to generation 2.
        held.unclaim_if_generation(&42, 1);
        assert!(
            !held.claim(42, 2),
            "generation 2 must still own the key after generation 1's stale cleanup"
        );

        // Generation 2's own cleanup does free it.
        held.unclaim_if_generation(&42, 2);
        assert!(
            held.claim(42, 2),
            "key is claimable again after the owner unclaims"
        );
    }

    /// Models the cancellation gap in `try_lock_*`: the claim is taken before the acquire query is
    /// awaited, so if the future is cancelled at that await the `ClaimGuard` must retract it — unless
    /// a `DbLockGuard` took ownership (disarm).
    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn claim_guard_retracts_on_drop_unless_disarmed() {
        let held: HeldKeys<i64> = HeldKeys::new();

        // Armed guard dropped (query await cancelled before a guard exists) → claim retracted.
        assert!(held.claim(7, 1));
        drop(ClaimGuard::new(&held, 7, 1));
        assert!(
            held.claim(7, 1),
            "armed ClaimGuard drop must retract the claim"
        );

        // Disarmed guard dropped (ownership handed to a `DbLockGuard`) → claim stands.
        let guard = ClaimGuard::new(&held, 7, 1);
        guard.disarm();
        assert!(
            !held.claim(7, 1),
            "disarmed ClaimGuard must leave the claim standing"
        );
    }

    /// A cancellation that races a reconnect must not let the stale generation's `ClaimGuard` evict
    /// the newer generation's live claim (ties [`ClaimGuard`] to the generation-aware retraction).
    #[cfg(any(feature = "pg", feature = "mysql"))]
    #[test]
    fn claim_guard_drop_does_not_evict_a_newer_generation() {
        let held: HeldKeys<i64> = HeldKeys::new();

        assert!(held.claim(9, 1));
        let stale = ClaimGuard::new(&held, 9, 1);
        // Reconnect: generation 2 takes the key over while generation 1's guard is still alive.
        assert!(held.claim(9, 2));

        drop(stale); // generation 1's belated retraction must be a no-op
        assert!(
            !held.claim(9, 2),
            "generation 2's claim must survive generation 1's ClaimGuard drop"
        );
    }

    #[test]
    fn database_scope_ignores_credentials_in_dsn() {
        let a = database_scope_from_dsn("postgres://alice:secret@db.example:5432/zoveon");
        let b = database_scope_from_dsn("postgres://bob:other@db.example:5432/zoveon");
        assert_eq!(a, b);
    }

    #[test]
    fn database_scope_normalizes_postgres_scheme() {
        let a = database_scope_from_dsn("postgres://db.example:5432/app");
        let b = database_scope_from_dsn("postgresql://db.example:5432/app");
        assert_eq!(a, b);
    }

    #[test]
    #[cfg(feature = "sqlite")]
    fn sqlite_relative_dot_paths_share_scope() {
        let a = database_scope_from_dsn("sqlite:./test.db");
        let b = database_scope_from_dsn("sqlite:test.db");
        assert_eq!(a, b);
    }

    #[test]
    #[cfg(feature = "sqlite")]
    fn sqlite_dotdot_paths_share_scope_lexically() {
        let a = database_scope_from_dsn("sqlite:./data/../data/app.db");
        let b = database_scope_from_dsn("sqlite:data/app.db");
        assert_eq!(a, b);
    }

    /// Pins the contract that lets the two lock-scope entry points agree: for a file-backed
    /// database the scope depends **only** on the resolved file path, never on DSN query
    /// parameters (`extract_file_path_from_dsn` strips the query).
    ///
    /// This matters because the two callers see different DSN text for the same database.
    /// `DbHandle::connect` hashes `clean_dsn`, which still carries non-PRAGMA parameters, while
    /// `DbConnectOptions::Sqlite` rebuilds `sqlite://{filename}` from a path that
    /// `parse_sqlite_path_from_dsn` already stripped. They land on one scope only as long as
    /// parameters are scope-irrelevant.
    ///
    /// If parameters ever start influencing *which* database is opened — e.g. passing
    /// `mode=memory&cache=shared` through to `connect` instead of dropping it — this assertion
    /// must be revisited: the scope would then have to encode them, or the same logical
    /// shared-memory database would be split across two lock namespaces.
    #[test]
    #[cfg(feature = "sqlite")]
    fn sqlite_scope_ignores_dsn_query_parameters() {
        let plain = database_scope_from_dsn("sqlite:data/app.db");

        assert_eq!(
            database_scope_from_dsn("sqlite:data/app.db?mode=rwc&cache=shared"),
            plain
        );
        assert_eq!(
            database_scope_from_dsn("sqlite:data/app.db?_pragma=busy_timeout(5000)"),
            plain
        );

        // The file path itself still separates databases.
        assert_ne!(database_scope_from_dsn("sqlite:data/other.db"), plain);
    }

    #[test]
    fn database_scope_differs_by_database_name() {
        let a = database_scope_from_dsn("postgres://db.example:5432/zoveon");
        let b = database_scope_from_dsn("postgres://db.example:5432/other");
        assert_ne!(a, b);
    }

    /// Every rejected value would collapse the exponential backoff to zero delay, turning the
    /// retry loop into a busy spin until `max_wait` expires.
    #[test]
    fn lock_config_rejects_invalid_values() {
        assert!(matches!(
            LockConfig {
                backoff_base_ms: 0,
                ..Default::default()
            }
            .validate(),
            Err(DbLockError::InvalidConfig { .. })
        ));

        assert!(matches!(
            LockConfig {
                backoff_factor: 0,
                ..Default::default()
            }
            .validate(),
            Err(DbLockError::InvalidConfig { .. })
        ));

        assert!(matches!(
            LockConfig {
                max_backoff: Duration::ZERO,
                ..Default::default()
            }
            .validate(),
            Err(DbLockError::InvalidConfig { .. })
        ));
    }

    /// `max_retries` counts retries *after* the first attempt, so zero means "one attempt, no
    /// retries" — valid, unlike the old `max_attempts: Some(0)`.
    #[test]
    fn lock_config_allows_zero_retries() {
        let cfg = LockConfig {
            max_retries: Some(0),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn lock_config_allows_unlimited_wait() {
        let cfg = LockConfig {
            max_wait: None,
            max_retries: None,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn clone_preserves_instance_id_and_scope() {
        let a = LockManager::file(0xdead_beef);
        let b = a.clone();
        assert_eq!(a.instance_id(), b.instance_id());
        assert_eq!(a.database_scope(), b.database_scope());

        let c = LockManager::file(0xdead_beef);
        assert_ne!(a.instance_id(), c.instance_id());
        assert_eq!(a.database_scope(), c.database_scope());
    }

    #[tokio::test]
    async fn test_namespaced_locks() -> Result<()> {
        let lock_manager = LockManager::file(0x11);
        let test_id = format!(
            "test_ns_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let guard1 = lock_manager
            .lock("gear1", &format!("{test_id}_key"))
            .await?;
        let guard2 = lock_manager
            .lock("gear2", &format!("{test_id}_key"))
            .await?;

        assert!(!guard1.key().is_empty());
        assert!(!guard2.key().is_empty());

        guard1.release().await?;
        guard2.release().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_try_lock_different_key_succeeds() -> Result<()> {
        let lock_manager = Arc::new(LockManager::file(0x22));
        let test_id = format!(
            "test_diff_key_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let _guard1 = lock_manager
            .lock("test_gear", &format!("{test_id}_key"))
            .await?;

        let config = LockConfig {
            max_wait: Some(Duration::from_millis(200)),
            max_retries: Some(3),
            ..Default::default()
        };

        let result = lock_manager
            .try_lock("test_gear", &format!("{test_id}_different_key"), config)
            .await?;
        assert!(result.is_some(), "expected successful lock acquisition");
        Ok(())
    }

    #[tokio::test]
    async fn test_try_lock_exhausted_attempts_returns_none_without_extra_sleep() -> Result<()> {
        let lock_manager = LockManager::file(0x66);
        let key = format!(
            "test_no_extra_sleep_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let _guard = lock_manager.lock("gear", &key).await?;
        let config = LockConfig {
            max_wait: Some(Duration::from_secs(30)),
            // Pin every delay to ~100ms so the elapsed-time assertion below is meaningful.
            max_backoff: Duration::from_millis(100),
            // One retry after the first attempt = two attempts total.
            max_retries: Some(1),
            ..Default::default()
        };

        let start = Instant::now();
        let res = lock_manager.try_lock("gear", &key, config).await?;
        assert!(res.is_none());
        // Two failed attempts + one inter-attempt sleep (~100ms), not a second post-final sleep.
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "unexpected long wait: {:?}",
            start.elapsed()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_try_lock_success() -> Result<()> {
        let lock_manager = LockManager::file(0x33);
        let test_id = format!(
            "test_success_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let result = lock_manager
            .try_lock(
                "test_gear",
                &format!("{test_id}_key"),
                LockConfig::default(),
            )
            .await?;
        assert!(result.is_some(), "expected lock acquisition");
        if let Some(g) = result {
            g.release().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_double_lock_same_key_errors() -> Result<()> {
        let lock_manager = LockManager::file(0x44);
        let test_id = format!(
            "test_double_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let guard = lock_manager.lock("test_gear", &test_id).await?;
        let err = lock_manager.lock("test_gear", &test_id).await.unwrap_err();
        match err {
            DbLockError::AlreadyHeld { lock_name } => {
                assert!(lock_name.contains(&test_id));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        guard.release().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_try_lock_conflict_returns_none() -> Result<()> {
        let lock_manager = LockManager::file(0x55);
        let key = format!(
            "test_conflict_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let _guard = lock_manager.lock("gear", &key).await?;
        let config = LockConfig {
            max_wait: Some(Duration::from_millis(100)),
            max_retries: Some(2),
            ..Default::default()
        };
        let res = lock_manager.try_lock("gear", &key, config).await?;
        assert!(res.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn file_explicit_release_allows_immediate_reacquire() -> Result<()> {
        let manager = LockManager::file(0x71);
        let key = format!(
            "reacquire_release_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let guard = manager.lock("gear", &key).await?;
        guard.release().await?;

        let guard = manager.lock("gear", &key).await?;
        guard.release().await?;
        Ok(())
    }

    #[tokio::test]
    async fn file_drop_allows_immediate_reacquire() -> Result<()> {
        let manager = LockManager::file(0x72);
        let key = format!(
            "reacquire_drop_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        {
            let _guard = manager.lock("gear", &key).await?;
        }

        let guard = manager.lock("gear", &key).await?;
        guard.release().await?;
        Ok(())
    }

    #[test]
    fn file_drop_after_runtime_shutdown_does_not_panic() {
        // File cleanup is synchronous and must not depend on a live runtime / spawned task.
        let manager = LockManager::file(0x73);
        let key = format!(
            "drop_after_shutdown_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let guard = runtime
            .block_on(async { manager.lock("gear", &key).await })
            .expect("lock");
        drop(runtime);

        // No current Tokio handle: file Drop must still remove the marker without panicking.
        drop(guard);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let guard = manager.lock("gear", &key).await.expect("reacquire");
            guard.release().await.expect("release");
        });
    }

    #[tokio::test]
    async fn file_path_uses_scope_not_independent_dsn() -> Result<()> {
        let a = LockManager::file(0xabc);
        let b = LockManager::file(0xabc);
        let path_a = a.get_lock_file_path(&canonical_lock_input(0xabc, "g", "k"));
        let path_b = b.get_lock_file_path(&canonical_lock_input(0xabc, "g", "k"));
        assert_eq!(path_a, path_b);

        let c = LockManager::file(0xdef);
        let path_c = c.get_lock_file_path(&canonical_lock_input(0xdef, "g", "k"));
        assert_ne!(path_a, path_c);
        Ok(())
    }

    /// A `max_backoff` below the first delay is legitimate now: it clamps every delay rather
    /// than being a misconfiguration (the old `initial_backoff` field it was compared against
    /// no longer exists).
    #[test]
    fn lock_config_allows_max_backoff_below_first_delay() {
        let cfg = LockConfig {
            backoff_base_ms: 2,
            backoff_factor: 25,
            max_backoff: Duration::from_millis(10),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn database_scope_fallback_for_unrecognized_dsn() {
        let scope = database_scope_from_dsn("custom://foo/bar");
        assert_ne!(scope, 0);
        assert_eq!(scope, database_scope_from_dsn("custom://foo/bar"));
        assert_ne!(scope, database_scope_from_dsn("postgres://foo:5432/bar"));
    }
}
