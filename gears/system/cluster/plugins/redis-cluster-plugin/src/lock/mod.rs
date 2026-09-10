//! [`RedisLock`] — the native `DistributedLockBackend` (DESIGN.md §5) — and the
//! standalone lock-only plugin of DESIGN.md §3.5.
//!
//! ## The whole mutual-exclusion mechanism is one command
//!
//! `SET <prefix>:l:<name> <holder_token> NX PX <ttl_ms>`. `OK` is the lock,
//! `nil` is contention. `NX` is atomic on a single primary and `PX` makes the
//! entry its own reaper, so there is no liveness proxy, no reclamation sweep,
//! and no in-process registry of what this instance holds. Nothing has to
//! distinguish "the holder crashed" from "the lease lapsed", because a crashed
//! holder stops renewing and the key evaporates on its own deadline with
//! nothing having to notice.
//!
//! The token is a fresh v4 UUID per acquisition, and it is what makes `renew`
//! and `release` safe against a successor: both are Lua scripts that compare
//! `GET KEYS[1]` against the token before acting (DESIGN.md §5.2). A bare `DEL`
//! on release is the classic bug in this pattern — a holder whose lease already
//! lapsed deletes its successor's lock — and `RD-LOCK-006` is the regression
//! test for it.
//!
//! ## A held lock consumes no connection
//!
//! DESIGN.md §3.3, and `RD-LOCK-010` holds twelve locks on a pool of two to say
//! so. What a held lock *does* consume is one task: the SDK hands the consumer a
//! [`LockGuard`] built by `LockGuard::channel` and the backend owns the paired
//! `LockCommandReceiver`, so something has to be selecting on it to service
//! `Renew` and `Release` (DESIGN.md §5). A task is not a checked-out connection: it
//! borrows one from the pool only for the round trip a command actually needs.
//!
//! Those tasks are the detached-task class `shutdown.rs` exists to warn about,
//! so they are spawned onto a [`TaskTracker`] the handle drains under a bound —
//! not with `tokio::spawn`, which would leave `stop()` unable to say whether
//! anything it started is still running.
//!
//! ## Blocking `lock()` waits in process, and the reason is a missing feature
//!
//! Redis has no "wait for this key" command this plugin can reach: `i-lists` is
//! absent from `fred`'s feature list (DESIGN.md §3.1) so `BLPOP` and
//! the rest of the blocking family do not compile, and a companion-list wait
//! would leak a list per lock name anyway. The loop instead re-attempts the
//! `SET NX` — always the source of truth — woken by whichever comes first of the
//! holder's release publish, the blocking lease's own `PTTL`, and a jittered
//! heartbeat. See [`waiters`] for the registry and the delay policy.

pub mod waiters;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cluster_sdk::observability::{ResourceId, spans};
use cluster_sdk::{
    ClusterError, DistributedLockBackend, LockCommandReceiver, LockFeatures, LockGuard,
    LockRequest, ProviderErrorKind,
};
use fred::clients::{Pool, SubscriberClient};
use fred::interfaces::{KeysInterface, PubsubInterface};
use fred::types::{ConnectHandle, Expiration, SetOptions, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{Instrument, warn};
use uuid::Uuid;

use crate::cache::scan::escape_glob;
use crate::config::RedisLockConfig;
use crate::connect::{ConnectSpec, Connected, connect};
use crate::lock::waiters::{ReleaseWait, ReleaseWaiters, wake_delay};
use crate::observability::{RedisSignals, logs, spawn_connection_state_observer};
use crate::preflight::{EVICTION_KEYSPACE_FLAGS, PreflightRequest, run_preflight};
use crate::provider::PROVIDER_NAME;
use crate::redis_error::map_redis_error;
use crate::scripts::{
    LOCK_RELEASE, LOCK_RENEW, LOCK_SCRIPTS, PoolScriptExecutor, ScriptCache, eval,
};
use crate::shutdown::{
    DropDiagnosis, abandon_subscriber, cancel_and_diagnose_drop, close_pool, drain_tracked_tasks,
};
use crate::subscriber::{
    FanOutRoutes, KeyspaceNames, LockRoute, confirm_subscriptions, quit_subscriber,
    spawn_connection_watchdog, spawn_fan_out,
};
use crate::wait::{WaitPolicy, wait_for_replicas};

/// The in-flight command buffer of each guard's channel.
///
/// The SDK notes that 1 suffices when the consumer awaits each `renew` before
/// issuing the next, and to size it larger only if a guard is shared across
/// tasks that may renew concurrently. 4 takes the second option: a guard behind
/// an `Arc` is an ordinary thing for a consumer to build, and a buffer of 1
/// would serialize those renewals on the channel rather than on the server,
/// which is the wrong place to discover the contention.
const GUARD_COMMAND_BUFFER: usize = 4;

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// The key and channel names the lock owns, all derived from the operator's
/// `key_prefix` (DESIGN.md §2.1).
///
/// One place, because the publisher and the subscriber have to agree exactly:
/// `lock_release` builds `<prefix>:e:l:<name>` from an argument this type
/// produced, and the fan-out parses the name back out of the channel it arrives
/// on. A mismatch would be a blocked `lock()` that silently never gets its wake.
#[derive(Debug, Clone)]
pub struct LockNames {
    /// `<key_prefix>:l:`.
    lease_prefix: String,
    /// `<key_prefix>:e:l:`.
    release_prefix: String,
}

impl LockNames {
    /// Builds the naming scheme for one lock backend.
    #[must_use]
    pub fn new(key_prefix: &str) -> Self {
        Self {
            lease_prefix: format!("{key_prefix}:l:"),
            release_prefix: format!("{key_prefix}:e:l:"),
        }
    }

    /// The Redis key holding `name`'s lease: `<prefix>:l:<name>`.
    ///
    /// `name` arrives already scope-prefixed by the SDK's
    /// `ScopedDistributedLockBackend`, and this never inspects the consumer's
    /// portion of it.
    #[must_use]
    pub fn lease_key(&self, name: &str) -> String {
        format!("{}{name}", self.lease_prefix)
    }

    /// The channel `name`'s release is published on: `<prefix>:e:l:<name>`.
    ///
    /// No key exists at this name — it is a channel. It is passed to
    /// `lock_release` as an `ARGV` rather than a second `KEYS` entry, because
    /// `PUBLISH` is not slot-routed and a second key would make the script
    /// multi-key for nothing (DESIGN.md §6).
    #[must_use]
    pub fn release_channel(&self, name: &str) -> String {
        format!("{}{name}", self.release_prefix)
    }

    /// The one blanket pattern carrying every release under this prefix.
    ///
    /// One always-on pattern rather than a `SUBSCRIBE` per contended name, and
    /// the reason is the shape of the acquisition loop: a waiter's interest
    /// lasts one iteration, so per-name subscriptions would put a
    /// `SUBSCRIBE`/`UNSUBSCRIBE` round trip either side of every retry and
    /// re-introduce exactly the ordering race the cache's registry needs a mutex
    /// to exclude. Releases are bounded by critical sections rather than by
    /// write rate, so the traffic a blanket pattern delivers to an uninterested
    /// instance is a `DashMap` miss (see [`ReleaseWaiters::notify`]).
    ///
    /// The operator's prefix is glob-escaped for the same reason `scan_prefix`
    /// escapes the consumer's: nothing rules out a `[` in it, and unescaped it
    /// would be a character class subscribing to something else entirely.
    #[must_use]
    pub fn release_pattern(&self) -> String {
        format!("{}*", escape_glob(&self.release_prefix))
    }

    /// Recovers the lock name from a release channel, or `None` when the channel
    /// is not one of this backend's.
    ///
    /// The counterpart to `ChannelNames::key_from_event_channel`, and
    /// deliberately a separate function rather than a widened one: the two
    /// families differ by one segment (`:e:l:` against `:e:c:`), and a parser
    /// that accepted both would route a cache event into the waiter registry the
    /// first time a prefix was mistyped.
    #[must_use]
    pub fn name_from_release_channel(&self, channel: &str) -> Option<String> {
        channel
            .strip_prefix(&self.release_prefix)
            .map(str::to_owned)
    }

    /// Recovers the lock name from a *Redis key* — what a keyspace notification
    /// reports — or `None` when the key is not one of this backend's leases.
    ///
    /// The counterpart to `ChannelNames::key_from_entry_key`, and what lets the
    /// eviction signal of DESIGN.md §3.7 name an evicted lease. It matches
    /// `<prefix>:l:` and not `<prefix>:e:l:`, which is a channel no key ever
    /// exists at, so the two cannot be confused however a notification arrives.
    #[must_use]
    pub fn name_from_lease_key(&self, entry_key: &str) -> Option<String> {
        entry_key
            .strip_prefix(&self.lease_prefix)
            .map(str::to_owned)
    }
}

// ---------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------

/// Renders a TTL as the `PX` / `PEXPIRE` argument: whole milliseconds, floored
/// at 1.
///
/// The floor is not defensive. `PX 0` is an error reply (`invalid expire time`)
/// and `PEXPIRE k 0` deletes the key outright, so rounding a sub-millisecond TTL
/// down to zero would turn "expires almost immediately" into either a failed
/// acquisition or a lock that was released the instant it was taken — and the
/// caller's next `renew` would report `LockExpired` for a lease it was never
/// given.
#[must_use]
pub fn px_millis(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX).max(1)
}

/// Reads a `PTTL` reply as the blocking lease's remaining life (DESIGN.md §5.3).
///
/// Redis overloads the reply with two negative sentinels and they mean opposite
/// things here:
///
/// - **`-2`, the key does not exist** — the lease lapsed or was released between
///   the `SET NX` that just failed and this read, so the name is free *now* and
///   the waiter should re-attempt immediately rather than sleep.
/// - **`-1`, the key exists with no TTL** — not a lease this plugin wrote (every
///   acquisition carries `PX`), so there is no deadline to schedule against and
///   the heartbeat is the honest answer.
#[must_use]
pub fn lease_remaining(pttl: i64) -> Option<Duration> {
    match pttl {
        -2 => Some(Duration::ZERO),
        negative if negative < 0 => None,
        millis => Some(Duration::from_millis(
            u64::try_from(millis).unwrap_or(u64::MAX),
        )),
    }
}

/// What one acquisition attempt left the blocking loop with.
enum Attempt {
    /// The `SET NX` took the lock.
    Acquired(LockGuard),
    /// Redis answered, and the answer was that someone else holds the name.
    Contended,
    /// The attempt never reached a Redis that could answer. Retried inside the
    /// caller's budget, and reported instead of `LockTimeout` if the budget runs
    /// out while it is still the last thing that happened.
    Unreachable(ClusterError),
    /// End the loop now and report this.
    Fatal(ClusterError),
    /// The caller's `timeout` is spent.
    BudgetSpent,
}

/// Decides whether an attempt's error is worth waiting out (DESIGN.md §5.3).
///
/// Only the two kinds that describe an *unreachable server* are retried:
/// `fred`'s reconnect is what carries a `lock()` through a Sentinel failover, and
/// a caller that asked for thirty seconds of patience should get it rather than
/// a failure ten milliseconds in. Everything else — a config fault, a script the
/// server rejected, an `OOM`, a shutdown — either will not clear by waiting or
/// is already the caller's answer, and retrying it would spend the whole budget
/// to report the same thing later.
fn classify_attempt(err: ClusterError) -> Attempt {
    match err {
        ClusterError::Provider {
            kind: ProviderErrorKind::ConnectionLost | ProviderErrorKind::Timeout,
            ..
        } => Attempt::Unreachable(err),
        other => Attempt::Fatal(other),
    }
}

/// The error a `lock()` reports when its budget runs out, as a function of how
/// long the caller waited and what the last attempt saw (DESIGN.md §5.3).
///
/// The distinction is the point. `LockTimeout` tells a caller "someone else
/// holds this name", which is a fact about the fleet and usually means back off
/// and try later. A retained `Provider` tells it "Redis was unreachable for your
/// whole budget", which is a fact about the deployment and usually means alert.
/// Collapsing the second into the first — which is what a loop that discarded
/// `last` would do — leaves a caller unable to tell contention from an outage.
///
/// `last` is cleared by every attempt that gets a *real* answer, so a blip early
/// in a long wait cannot masquerade as the cause of a timeout that was in fact
/// ordinary contention.
fn lock_failure(name: &str, waited: Duration, last: Option<ClusterError>) -> ClusterError {
    match last {
        Some(unreachable) => unreachable,
        None => ClusterError::LockTimeout {
            name: name.to_owned(),
            waited,
        },
    }
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// Everything [`RedisLock::new`] needs, bundled so the call site names each
/// field rather than relying on the order of two `Arc`s and a `bool`.
pub struct LockInit {
    /// The connected command pool.
    pub pool: Pool,
    /// The SHAs `SCRIPT LOAD` returned for [`LOCK_SCRIPTS`] at startup.
    pub scripts: Arc<ScriptCache>,
    /// The key and channel naming scheme, shared with the subscriber fan-out.
    pub names: LockNames,
    /// The declaration, derived from the preflight exactly as the cache's is.
    pub linearizable: bool,
    /// The operator's `WAIT` policy, if any.
    pub wait: WaitPolicy,
    /// The release-waiter registry the fan-out feeds.
    pub waiters: Arc<ReleaseWaiters>,
    /// The handle's shutdown token.
    pub shutdown: CancellationToken,
    /// The plugin's signal sink.
    ///
    /// The lock emits its ADR-004 signals *natively* rather than through a
    /// decorator, because there is no `InstrumentedLock` in the SDK to wrap it
    /// in — the lock's surface includes a guard whose `renew`/`release` arrive
    /// on a channel long after the call that produced it, which a decorator
    /// around the backend trait cannot see (DESIGN.md §9).
    pub signals: Arc<RedisSignals>,
}

/// The native Redis distributed-lock backend.
pub struct RedisLock {
    pool: Pool,
    executor: PoolScriptExecutor,
    scripts: Arc<ScriptCache>,
    names: LockNames,
    /// Mirrors the cache's declared consistency (DESIGN.md §5.1): `true` only
    /// under the verified single-node durable topology, so a consumer requiring
    /// `LockCapability::Linearizable` against a Sentinel or Cluster deployment
    /// fails startup rather than receiving a lock a failover can hand to two
    /// holders.
    linearizable: bool,
    wait: WaitPolicy,
    waiters: Arc<ReleaseWaiters>,
    shutdown: CancellationToken,
    /// The per-guard tasks, tracked rather than detached so `stop()` can say
    /// whether they have finished. See the module docs.
    guards: TaskTracker,
    /// The sink every `cluster.lock.*` span, counter, and histogram goes
    /// through, mirroring `CasBasedDistributedLockBackend::record_lock` so a
    /// dashboard cannot tell which of the two locks a profile is bound to.
    signals: Arc<RedisSignals>,
}

impl RedisLock {
    /// Builds the lock over an already-connected pool and an already-loaded
    /// script catalog.
    #[must_use]
    pub fn new(init: LockInit) -> Self {
        let executor = PoolScriptExecutor::new(init.pool.clone());
        Self {
            pool: init.pool,
            executor,
            scripts: init.scripts,
            names: init.names,
            linearizable: init.linearizable,
            wait: init.wait,
            waiters: init.waiters,
            shutdown: init.shutdown,
            guards: TaskTracker::new(),
            signals: init.signals,
        }
    }

    /// Closes the guard-task tracker and waits for the tasks under a bound —
    /// step 1 of either handle's `stop()`.
    ///
    /// The caller must have cancelled the shared token first: that is what makes
    /// this finite, since each guard task selects on it and every command it
    /// could be mid-flight on is bounded client-side.
    pub async fn drain_guards(&self) {
        drain_tracked_tasks(&self.guards, "held redis lock guards").await;
    }

    /// The context each guard task carries: everything a held lock needs to
    /// answer `Renew` and `Release`, and nothing else. Cheap to clone — a pool
    /// handle, two `Arc`s, two `String`s, and a token clone.
    fn guard_context(&self) -> GuardContext {
        GuardContext {
            executor: self.executor.clone(),
            scripts: Arc::clone(&self.scripts),
            names: self.names.clone(),
            shutdown: self.shutdown.clone(),
            signals: Arc::clone(&self.signals),
        }
    }

    /// One `SET NX PX`, plus everything that has to be true before a guard is
    /// handed out. `Ok(None)` is contention.
    ///
    /// # Errors
    /// [`ClusterError::Shutdown`] when `stop()` has run or races this, and
    /// whatever [`map_redis_error`] makes of a failing `SET` or `WAIT`.
    async fn try_acquire(
        &self,
        name: &str,
        ttl: Duration,
    ) -> Result<Option<LockGuard>, ClusterError> {
        // Checked before any lock work, so an acquisition arriving after
        // `stop()` answers immediately instead of writing a lease nothing will
        // ever hold (DESIGN.md §5.3). `lock()` reaches this through the same
        // path, which is what keeps the two in agreement.
        if self.shutdown.is_cancelled() {
            return Err(ClusterError::Shutdown);
        }

        let token = Uuid::new_v4().to_string();
        let acquired: Value = self
            .pool
            .set(
                self.names.lease_key(name),
                token.clone(),
                Some(Expiration::PX(px_millis(ttl))),
                Some(SetOptions::NX),
                false,
            )
            .await
            .map_err(map_redis_error)?;
        if acquired.is_null() {
            return Ok(None);
        }

        // `WAIT` on acquisition and on nothing else in this file, which is
        // narrower than the cache's rule and deliberately so. Losing an
        // *acquisition* to a promotion is the failure `WAIT` exists for: the new
        // primary has no lease key, so a second instance takes the name while
        // this one believes it holds it. Losing a *renewal* fails the other way
        // — the lease reverts to its earlier, shorter deadline — and losing a
        // *release* only leaves a name unacquirable until its TTL. Neither can
        // produce two holders, so neither is worth a round trip on every call.
        if let Err(short) = wait_for_replicas(&self.pool, self.wait).await {
            self.abandon(name, &token).await;
            return Err(short);
        }

        // `stop()` may have run while that `SET` was in flight. Nobody holds
        // this lease — no guard has been handed out — so releasing it is not the
        // remote cleanup `cpt-cf-clst-fr-shutdown-ttl-cleanup` forbids: that
        // rule is about leases a consumer *is* holding, which `stop()` leaves to
        // expire (`RD-LOCK-013`). Wedging a name nobody took, for a full TTL,
        // would be the worse answer.
        if self.shutdown.is_cancelled() {
            self.abandon(name, &token).await;
            return Err(ClusterError::Shutdown);
        }

        // DEBUG and a *log line*, never a metric: a holder token is unbounded
        // and would explode the label cardinality of anything it touched
        // (ADR-004's cardinality rule). As a line it is what makes a token read
        // out of Redis by an operator traceable back to the instance holding it
        // (DESIGN.md §5.5).
        tracing::debug!(
            name: logs::LOCK_ACQUIRED,
            provider = self.signals.provider(),
            lock = %name,
            holder = %token,
            "cluster.lock.acquired: this instance now holds the lease under this token"
        );

        let (commands, guard) = LockGuard::channel(name.to_owned(), GUARD_COMMAND_BUFFER);
        self.guards.spawn(run_guard_task(
            name.to_owned(),
            token,
            self.guard_context(),
            commands,
        ));
        Ok(Some(guard))
    }

    /// Gives back a lease this instance took but never handed to a consumer.
    ///
    /// Token-fenced like every other release, which is what makes it safe to
    /// call unconditionally: if the lease has already lapsed and been re-taken,
    /// this matches nothing rather than deleting the successor's key. Failures
    /// are swallowed because the caller is already returning an error and the
    /// TTL is the backstop either way.
    async fn abandon(&self, name: &str, token: &str) {
        if let Err(err) = release_lease(&self.guard_context(), name, token).await {
            tracing::debug!(
                error = %err,
                "could not give back a redis lease that was taken but never handed out; it will \
                 lapse at its TTL"
            );
            // Swallowed for the *caller*, who is already being handed an error
            // and for whom the TTL is the backstop either way - but not for the
            // deployment: this is a command the backend refused, and it is
            // wrapped by no catalogued op, so without this the only trace of a
            // Redis that is failing every release would be a DEBUG line.
            self.signals
                .provider_error("abandon", ResourceId::Lock(name), &err);
        }
    }

    /// Runs one acquisition attempt inside whatever is left of the caller's
    /// budget.
    ///
    /// The bound matters because `try_acquire`'s own bound is `fred`'s
    /// `command_timeout_ms` (5 s by default), which can be far longer than a
    /// caller's `timeout`: without it a `lock(name, ttl, 1s)` against an
    /// unresponsive server returns after five seconds. An attempt cancelled
    /// after its `SET` committed leaves a lease nobody holds, which is exactly
    /// the case DESIGN.md §5.1 already accounts for — it expires at its TTL like
    /// any other, with nothing local claiming to own it.
    ///
    /// The *first* attempt always runs, even with a spent or zero budget, so
    /// `lock(free_name, ttl, Duration::ZERO)` acquires instead of reporting a
    /// timeout without ever having tried — matching the SDK default backend's
    /// attempt-before-budget-check ordering.
    async fn attempt(
        &self,
        name: &str,
        ttl: Duration,
        deadline: tokio::time::Instant,
        first: bool,
    ) -> Attempt {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() && !first {
            return Attempt::BudgetSpent;
        }
        let acquire = self.try_acquire(name, ttl);
        let outcome = if remaining.is_zero() {
            acquire.await
        } else {
            match tokio::time::timeout(remaining, acquire).await {
                Ok(outcome) => outcome,
                Err(_elapsed) => return Attempt::BudgetSpent,
            }
        };
        match outcome {
            Ok(Some(guard)) => Attempt::Acquired(guard),
            Ok(None) => Attempt::Contended,
            Err(err) => classify_attempt(err),
        }
    }

    /// Waits for whichever comes first of the holder's release, the blocking
    /// lease's own deadline, and a jittered heartbeat (DESIGN.md §5.3).
    ///
    /// `released` is registered by the caller *before* its attempt rather than
    /// here, and that ordering is the one deviation from §5.3's pseudo-code. A
    /// release landing between a failed `SET NX` and the registration would
    /// otherwise be missed, and the waiter would sit out a full delay for a lock
    /// that is already free — which is precisely the "the notification was
    /// missed, not slow" outcome `RD-LOCK-003` is built to catch.
    ///
    /// # Errors
    /// [`ClusterError::Shutdown`] if `stop()` fires while parked, so a blocked
    /// caller is told the backend is gone rather than waiting out its whole
    /// budget for a `LockTimeout` that would read as contention (DESIGN.md §11
    /// step 1).
    async fn park(
        &self,
        name: &str,
        released: ReleaseWait,
        deadline: tokio::time::Instant,
    ) -> Result<(), ClusterError> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        // Read on the attempt that just failed, so the sleep is sized against
        // the lease actually blocking us. An unreadable reply is not worth
        // failing over: the heartbeat covers it, and the attempt that follows
        // the sleep reports the outage properly if it is still there - so this
        // is a DEBUG rather than another provider error, which would otherwise
        // count once per loop iteration for a single unreachable server.
        //
        // Bounded by the same `remaining` that bounds the sleep it sizes, and by
        // the same rule DESIGN.md §5.3's third bound states: the caller's
        // `timeout` is the budget for the whole `lock()` call, not for each
        // command inside it. Unbounded, this round trip is capped only by
        // `fred`'s 5 s default command timeout, so a Redis that stalls here — a
        // `BGSAVE` fork pause, a slow `SCAN` from another tenant — overruns a
        // 50 ms budget by seconds. The acquisition attempt on the same loop is
        // already wrapped this way (see `attempt`); this is the read between
        // them.
        let read = self.pool.pttl(self.names.lease_key(name));
        let pttl: Option<i64> = match tokio::time::timeout(remaining, read).await {
            Ok(Ok(pttl)) => Some(pttl),
            // The budget ran out while reading. Sizing the sleep is moot at that
            // point: the loop's next `attempt` finds no budget left and reports
            // the timeout.
            Err(_elapsed) => None,
            Ok(Err(err)) => {
                tracing::debug!(
                    lock = name,
                    error = %err,
                    "could not read the blocking lease's PTTL; falling back to the heartbeat"
                );
                None
            }
        };
        // Re-read rather than reusing the `remaining` above, because the read
        // just spent some of it. Bounding the read and then sizing the sleep
        // against the pre-read budget would leave the two bounds serialized —
        // worst case a stalled read consumes the whole budget and the sleep
        // spends it a second time, which is the same overrun one layer down.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let delay = wake_delay(pttl.and_then(lease_remaining), remaining);
        tokio::select! {
            () = self.shutdown.cancelled() => return Err(ClusterError::Shutdown),
            _woken = released => {}
            () = tokio::time::sleep(delay) => {}
        }
        Ok(())
    }
}

/// The per-guard context: everything a held lock needs beyond its own name and
/// token. Grouped into one value rather than threaded as separate parameters,
/// which keeps `run_guard_task` and the two operations below within a sane
/// argument count.
#[derive(Clone)]
struct GuardContext {
    executor: PoolScriptExecutor,
    scripts: Arc<ScriptCache>,
    names: LockNames,
    shutdown: CancellationToken,
    /// Carried into the per-guard task so `renew` and `release` emit the same
    /// signals `try_lock` and `lock` do. Without it the two halves of the lock's
    /// surface would be observable and unobservable respectively, and the
    /// unobservable half is the one that runs for the whole critical section.
    signals: Arc<RedisSignals>,
}

/// Drives one held lock's [`LockCommandReceiver`] until `Release`, until the
/// consumer drops its guard without releasing, or until `stop()`.
///
/// All three endings leave the lease alone, and that is the TTL safety net
/// rather than an omission: a dropped guard is documented by the SDK as "no I/O
/// in `Drop`, the entry lapses via TTL", and a `stop()` is
/// `cpt-cf-clst-fr-shutdown-ttl-cleanup`, which `RD-LOCK-013` pins by asserting
/// the keys are **still there** afterwards. Once this task returns, the
/// consumer's `renew` sees a closed channel and reports `LockExpired` while its
/// `release` reports the best-effort `Ok` the SDK specifies for a backend that
/// has gone away.
async fn run_guard_task(
    name: String,
    token: String,
    ctx: GuardContext,
    mut commands: LockCommandReceiver,
) {
    loop {
        tokio::select! {
            // Exit promptly rather than waiting on a consumer that may never
            // act. This is what keeps `drain_guards` bounded by the in-flight
            // command rather than by the critical section.
            () = ctx.shutdown.cancelled() => return,
            request = commands.recv() => {
                let Some(request) = request else {
                    // The consumer dropped the guard without releasing.
                    return;
                };
                match request {
                    LockRequest::Renew { new_ttl, responder } => {
                        let span = tracing::info_span!(
                            spans::LOCK_RENEW,
                            provider = %ctx.signals.provider(),
                            lock = %name
                        );
                        let started = std::time::Instant::now();
                        let out = renew_lease(&ctx, &name, &token, new_ttl)
                            .instrument(span)
                            .await;
                        ctx.signals.record_lock("renew", &name, started, &out);
                        responder.respond(out);
                    }
                    LockRequest::Release { responder } => {
                        let span = tracing::info_span!(
                            spans::LOCK_RELEASE,
                            provider = %ctx.signals.provider(),
                            lock = %name
                        );
                        let started = std::time::Instant::now();
                        let out = release_lease(&ctx, &name, &token).instrument(span).await;
                        ctx.signals.record_lock("release", &name, started, &out);
                        responder.respond(out);
                        // Release consumes the guard, so there is nothing left
                        // to service.
                        return;
                    }
                }
            }
        }
    }
}

/// `lock_renew`: `PEXPIRE` the lease, but only while the token still matches
/// (DESIGN.md §5.2).
///
/// `PEXPIRE` is absolute-from-now on Redis's own clock, so a lease deadline
/// never depends on the client's wall clock and a skewed instance cannot hold a
/// lock longer than it was granted.
///
/// # Errors
/// [`ClusterError::LockExpired`] when the script reports zero — the lease
/// lapsed, or a successor took it. The consumer's response is the same in both
/// cases (abort the critical section), which is why the SDK models it as one
/// error.
async fn renew_lease(
    ctx: &GuardContext,
    name: &str,
    token: &str,
    new_ttl: Duration,
) -> Result<(), ClusterError> {
    let args = [
        Value::String(token.into()),
        Value::String(px_millis(new_ttl).to_string().into()),
    ];
    let renewed = eval(
        &ctx.executor,
        &ctx.scripts,
        &LOCK_RENEW,
        &ctx.names.lease_key(name),
        &args,
        &ctx.signals,
    )
    .await?;
    if renewed.as_i64() == Some(1) {
        return Ok(());
    }
    Err(ClusterError::LockExpired {
        name: name.to_owned(),
    })
}

/// `lock_release`: delete the lease and publish the wake, but only while the
/// token still matches (DESIGN.md §5.2).
///
/// **A zero reply is not an error.** It is the SDK's release-if-still-holder
/// contract (`cpt-cf-clst-algo-distributed-lock-release-if-holder`): this
/// holder's lease had already lapsed and someone else's entry is under the key,
/// so the script left it alone. From this holder's view the lease is gone, which
/// is what it asked for. `RD-LOCK-006` is the test that fails if the script is
/// ever reduced to a bare `DEL`.
///
/// # Errors
/// Whatever [`eval`] makes of a failing `EVALSHA`.
async fn release_lease(ctx: &GuardContext, name: &str, token: &str) -> Result<(), ClusterError> {
    let args = [
        Value::String(token.into()),
        Value::String(ctx.names.release_channel(name).into()),
    ];
    let released = eval(
        &ctx.executor,
        &ctx.scripts,
        &LOCK_RELEASE,
        &ctx.names.lease_key(name),
        &args,
        &ctx.signals,
    )
    .await?;
    if released.as_i64() != Some(1) {
        tracing::debug!(
            lock = name,
            "released a redis lock this instance no longer holds; the lease had already lapsed \
             and another holder's entry was left intact"
        );
    }
    Ok(())
}

#[async_trait]
impl DistributedLockBackend for RedisLock {
    fn features(&self) -> LockFeatures {
        LockFeatures::new(self.linearizable)
    }

    fn provider_name(&self) -> &'static str {
        // Overridden rather than left to the default `type_name`, because this
        // string reaches operators through `CapabilityNotMet { provider }` and
        // `redis` is the name they wrote in their YAML.
        PROVIDER_NAME
    }

    async fn try_lock(&self, name: &str, ttl: Duration) -> Result<LockGuard, ClusterError> {
        let span = tracing::info_span!(
            spans::LOCK_TRY_LOCK,
            provider = %self.signals.provider(),
            lock = %name
        );
        let started = std::time::Instant::now();
        let out = async {
            match self.try_acquire(name, ttl).await? {
                Some(guard) => Ok(guard),
                None => Err(ClusterError::LockContended {
                    name: name.to_owned(),
                }),
            }
        }
        .instrument(span)
        .await;
        // `LockContended` lands on the bounded `result` label as `contended` and
        // *not* on the provider-error counter: someone else holding the name is
        // this call's answer, not a fault (`result::label`).
        self.signals.record_lock("try_lock", name, started, &out);
        out
    }

    async fn lock(
        &self,
        name: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        // Instrumented once *around* the loop rather than inside it, following
        // the postgres plugin's shape: a blocking acquisition is one operation
        // however many `SET NX`s it takes, so one span covering the whole wait
        // and one duration measuring it is what a consumer asked for. Recording
        // per attempt would report a 30 s wait as a hundred fast operations.
        let span = tracing::info_span!(
            spans::LOCK_LOCK,
            provider = %self.signals.provider(),
            lock = %name
        );
        let op_started = std::time::Instant::now();
        let out = async {
            let started = tokio::time::Instant::now();
            let deadline = started + timeout;
            // The last attempt that never reached Redis, cleared by every
            // attempt that got a real answer. See [`lock_failure`] for why it
            // survives at all.
            let mut last: Option<ClusterError> = None;
            let mut first = true;
            loop {
                // Registered before the attempt, not after — see [`park`].
                let released = self.waiters.wait_for(name);
                match self.attempt(name, ttl, deadline, first).await {
                    Attempt::Acquired(guard) => return Ok(guard),
                    Attempt::Contended => last = None,
                    Attempt::Unreachable(err) => last = Some(err),
                    Attempt::Fatal(err) => return Err(err),
                    Attempt::BudgetSpent => {
                        return Err(lock_failure(name, started.elapsed(), last));
                    }
                }
                first = false;
                if tokio::time::Instant::now() >= deadline {
                    return Err(lock_failure(name, started.elapsed(), last));
                }
                self.park(name, released, deadline).await?;
            }
        }
        .instrument(span)
        .await;
        self.signals.record_lock("lock", name, op_started, &out);
        out
    }
}

// ---------------------------------------------------------------------------
// The standalone lock-only plugin (DESIGN.md §3.5)
// ---------------------------------------------------------------------------

/// Entry point for the standalone lock-only plugin, which lets an operator bind
/// `lock: { provider: redis }` alongside a cache of any other kind.
///
/// It opens its own pool and its own subscriber rather than borrowing the
/// combined plugin's — the SDK's provider contract is that non-cache providers
/// do not receive the cache backend, and sharing would need a
/// lifecycle-ownership story (whose `stop()` closes the pool?) for two providers
/// the SDK deliberately made independent. At ~20 KB per connection that is
/// cheaper than the coupling it avoids.
///
/// ```no_run
/// # async fn doc(config: redis_cluster_plugin::RedisLockConfig) -> Result<(), cluster_sdk::ClusterError> {
/// use redis_cluster_plugin::RedisLockPlugin;
/// let handle = RedisLockPlugin::builder(config).build_and_start().await?;
/// let _lock = handle.lock();
/// handle.stop().await;
/// # Ok(())
/// # }
/// ```
pub struct RedisLockPlugin;

impl RedisLockPlugin {
    // No `#[must_use]` here: `RedisLockBuilder` already carries a
    // `#[must_use = "..."]` message, so a bare attribute on this function would
    // be a `clippy::double_must_use` no-op.
    /// Starts building the plugin from operator config.
    pub fn builder(config: RedisLockConfig) -> RedisLockBuilder {
        RedisLockBuilder {
            config,
            meter: None,
        }
    }
}

/// Fluent builder for [`RedisLockPlugin`].
#[must_use = "a builder starts nothing until `.build_and_start()` is called"]
pub struct RedisLockBuilder {
    config: RedisLockConfig,
    /// Optional override for the meter every signal is emitted through. `None`
    /// in production (the process-global provider). See
    /// [`__with_meter`](Self::__with_meter).
    meter: Option<opentelemetry::metrics::Meter>,
}

impl RedisLockBuilder {
    /// Test-only: routes both the ADR-004 catalog signals and this plugin's four
    /// local metrics through `meter` instead of the process-global provider, so
    /// a test can attach an in-memory reader and read every signal back by name
    /// rather than by eye.
    ///
    /// Gated behind `--features integration` so the seam is compiled out of
    /// release builds entirely, mirroring
    /// `PostgresLockBuilder::__with_reaper_meter`.
    #[cfg(feature = "integration")]
    #[doc(hidden)]
    pub fn __with_meter(mut self, meter: opentelemetry::metrics::Meter) -> Self {
        self.meter = Some(meter);
        self
    }

    /// Builds and starts the lock-only plugin.
    ///
    /// The same six steps as the combined plugin's `build_and_start`, minus
    /// everything the cache owns: only [`LOCK_SCRIPTS`] is loaded, only the
    /// release pattern is subscribed, and the preflight is told **not** to check
    /// `notify-keyspace-events`. That last one is DESIGN.md §3.5's third row and
    /// the reason it is worth stating: a lease lapse is discovered by the next
    /// acquire attempt, so a lock-only deployment works with keyspace
    /// notifications entirely unavailable, with no degradation at all
    /// (`RD-LOCK-009`).
    ///
    /// As in the combined plugin, the initial `PSUBSCRIBE` is awaited before
    /// this returns: Redis pub/sub does not replay for a client that subscribes
    /// late, so a release landing in the startup window would otherwise be lost
    /// with nothing to show for it.
    ///
    /// # Errors
    /// - [`ClusterError::InvalidConfig`] for a zero-valued config bound, a URL
    ///   `fred` cannot parse, or an `INFO server` the server refuses.
    /// - [`ClusterError::Provider`] if the initial connect, the `SCRIPT LOAD`,
    ///   or the initial `PSUBSCRIBE` fails.
    pub async fn build_and_start(self) -> Result<RedisLockHandle, ClusterError> {
        let config = self.config;
        config.validate()?;
        // Before the connect, so an unrepresentable `wait_timeout_ms` is a plain
        // `?` rather than an error path with a pool and a subscriber to tear
        // down. See [`WaitPolicy::from_config`].
        let wait = WaitPolicy::from_config(config.wait_replicas, config.wait_timeout_ms)?;

        // One sink for this plugin, built before anything that could emit
        // through it: the lock's own signals, the fan-out's, and the
        // connection-state gauge all share it, so `provider` is fixed once and
        // `cluster_lock_ops_total` is one instrument rather than three.
        let signals = Arc::new(match self.meter {
            Some(meter) => RedisSignals::over_meter(&meter, PROVIDER_NAME),
            None => RedisSignals::from_global_meters(PROVIDER_NAME),
        });

        let Connected {
            pool,
            subscriber: (client, connection),
            url_topology,
            ..
        } = connect(ConnectSpec {
            url: &config.url,
            database: config.database,
            pool_size: config.pool_size,
            command_timeout: config.command_timeout(),
        })
        .await?;

        // Everything past the connect has to tear the pool *and the subscriber*
        // down on the way out — both are connected, so returning the error alone
        // would leak the connections until the process exited. See
        // [`abandon_subscriber`] for why dropping the subscriber is not enough.
        let outcome = match run_preflight(
            &pool,
            PreflightRequest {
                topology_hint: config.topology,
                url_topology,
                durability_hint: config.durability,
                // The narrow set: `Ke` for the eviction signal, never `x`. A
                // lapsed lease is found by the next acquire attempt, so this
                // plugin has no use for `expired` and asking a shared server to
                // turn on a server-wide flag it never reads would be a cost
                // charged to unrelated tenants (DESIGN.md §3.5, §3.7).
                keyspace_flags: Some(EVICTION_KEYSPACE_FLAGS),
                // Never: the flags this plugin wants are an observability
                // improvement rather than something it needs to function, and a
                // server-wide `CONFIG SET` is too blunt an instrument to reach
                // for on that basis. The combined plugin exposes the opt-in
                // because its cache genuinely degrades without them.
                manage_keyspace_notifications: false,
            },
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                abandon_subscriber(&client, &connection).await;
                close_pool(&pool).await;
                return Err(err);
            }
        };

        let executor = PoolScriptExecutor::new(pool.clone());
        let scripts = match crate::scripts::load_catalog(&executor, LOCK_SCRIPTS).await {
            Ok(scripts) => Arc::new(scripts),
            Err(err) => {
                abandon_subscriber(&client, &connection).await;
                close_pool(&pool).await;
                return Err(err);
            }
        };

        let shutdown = CancellationToken::new();
        let waiters = ReleaseWaiters::new();
        let names = LockNames::new(&config.key_prefix);
        let lock = Arc::new(RedisLock::new(LockInit {
            pool: pool.clone(),
            scripts,
            names: names.clone(),
            linearizable: outcome.consistency == cluster_sdk::CacheConsistency::Linearizable,
            wait,
            waiters: Arc::clone(&waiters),
            shutdown: shutdown.clone(),
            signals: Arc::clone(&signals),
        }));

        let subscription = match start_release_subscriber(ReleaseSubscriberSetup {
            client,
            connection,
            names: &names,
            // No cache half: this plugin owns leases and nothing else, so a
            // notification for a `<prefix>:c:` key belongs to a *different*
            // deployment sharing the prefix and is classified as neither.
            keyspace: KeyspaceNames::new(&config.key_prefix, config.database, None, names.clone()),
            waiters,
            signals: Arc::clone(&signals),
            shutdown: &shutdown,
        })
        .await
        {
            Ok(subscription) => subscription,
            Err(err) => {
                close_pool(&pool).await;
                return Err(err);
            }
        };

        let connection_state =
            spawn_connection_state_observer(pool.clone(), signals, shutdown.clone());

        Ok(RedisLockHandle {
            lock,
            pool,
            subscription: Some(subscription),
            connection_state: Some(connection_state),
            shutdown,
            stopped: false,
        })
    }
}

/// What [`start_release_subscriber`] needs. A struct because six parameters,
/// three of them references, is where a positional call stops being readable.
struct ReleaseSubscriberSetup<'a> {
    client: SubscriberClient,
    connection: ConnectHandle,
    names: &'a LockNames,
    keyspace: KeyspaceNames,
    waiters: Arc<ReleaseWaiters>,
    signals: Arc<RedisSignals>,
    shutdown: &'a CancellationToken,
}

/// The subscriber client and the tasks that ride it, owned by the handle so
/// `stop()` can end them in order.
struct LockSubscription {
    client: SubscriberClient,
    fan_out: JoinHandle<()>,
    /// `fred`'s own subscription-replay task, which is what makes a reconnect
    /// recoverable at all.
    manager: JoinHandle<()>,
    /// Logs once if the reconnect policy is ever exhausted.
    watchdog: JoinHandle<()>,
}

/// Subscribes the release and keyspace patterns, awaits them, and spawns the
/// fan-out.
///
/// There is no reconnect observer here, unlike the combined plugin's. That task
/// exists to broadcast a cache `Reset` after a subscription gap, and a lock has
/// no equivalent to reset: a release missed during a gap costs its waiter one
/// jittered delay, after which the `SET NX` — the source of truth throughout —
/// answers correctly. Telling anyone about the gap would give them nothing to
/// do with it.
///
/// The **keyspace** pattern is here for one reason and it is not expiry: a
/// lapsed lease needs no event, but an *evicted* one hands the lock to a second
/// holder with no TTL having elapsed and nothing else in the system that could
/// notice (DESIGN.md §3.7). A lock-only deployment on a shared Redis under
/// `allkeys-lru` is exactly where that happens, so it subscribes the pattern and
/// reports what arrives. Nothing depends on the notifications arriving: a server
/// without the `Ke` flags degrades this to silence, which the preflight has
/// already warned about, and the lock keeps working unchanged.
///
/// # Errors
/// Whatever [`map_redis_error`] makes of a failing `PSUBSCRIBE`. The subscriber
/// is quit on that path, so a half-open second connection does not outlive the
/// failed startup.
async fn start_release_subscriber(
    setup: ReleaseSubscriberSetup<'_>,
) -> Result<LockSubscription, ClusterError> {
    let ReleaseSubscriberSetup {
        client,
        connection,
        names,
        keyspace,
        waiters,
        signals,
        shutdown,
    } = setup;
    // The replay task first: a reconnect between the subscribes below and this
    // spawn would otherwise lose the subscription set permanently.
    let manager = client.manage_subscriptions();
    let mut subscribed = Ok(());
    for pattern in [names.release_pattern(), keyspace.pattern().to_owned()] {
        if let Err(err) = client.psubscribe(pattern).await {
            subscribed = Err(map_redis_error(err));
            break;
        }
    }
    // `psubscribe` resolving is not the server having processed it, so the round
    // trip is what actually makes the patterns live before this returns — see
    // [`confirm_subscriptions`]. `RD-LOCK-003` is the assertion that notices
    // when it is not: a release landing in the startup window would otherwise
    // reach no subscriber, and the waiter would fall back to the heartbeat with
    // nothing to show for it.
    let subscribed = match subscribed {
        Ok(()) => confirm_subscriptions(&client).await,
        Err(err) => Err(err),
    };
    if let Err(err) = subscribed {
        manager.abort();
        connection.abort();
        quit_subscriber(&client).await;
        return Err(err);
    }

    // The same watchdog the combined plugin spawns, with nothing to close: a
    // blocked `lock()` loses only its wake path when the subscriber is
    // permanently gone, and keeps acquiring on the heartbeat. Reusing it is what
    // keeps the two plugins from growing two descriptions of one event.
    let watchdog = spawn_connection_watchdog(connection, None, shutdown.clone());
    let fan_out = spawn_fan_out(
        &client,
        FanOutRoutes {
            cache: None,
            locks: LockRoute {
                waiters,
                names: names.clone(),
            },
            // Present although `cache` is not: this plugin owns no cache
            // entries, and the keyspace route exists here purely to observe
            // evictions of its own leases.
            keyspace: Some(keyspace),
            signals,
        },
        shutdown.clone(),
    );
    Ok(LockSubscription {
        client,
        fan_out,
        manager,
        watchdog,
    })
}

/// The running standalone lock plugin.
///
/// Call [`stop`](Self::stop) on graceful shutdown. Dropping the handle without
/// it is a programming error and says so — the same ADR-006 guard the combined
/// handle carries, independently, because this handle owns its own pool and
/// subscriber.
pub struct RedisLockHandle {
    lock: Arc<RedisLock>,
    pool: Pool,
    /// `Option`, not a bare field, for the same reason the combined handle's is:
    /// this type has a `Drop` impl, so `stop` cannot move out of it and uses
    /// `.take()` to drain in place.
    subscription: Option<LockSubscription>,
    /// The task keeping `cluster_redis_connection_state` current (DESIGN.md §9).
    connection_state: Option<JoinHandle<()>>,
    shutdown: CancellationToken,
    /// Set by `stop` so the `Drop` guard can tell a graceful shutdown apart from
    /// a forgotten one (ADR-006 §Confirmation).
    stopped: bool,
}

impl RedisLockHandle {
    /// The lock backend.
    ///
    /// Handed out bare, and it stays bare: the ADR-004 lock signal set is
    /// emitted by [`RedisLock`] itself at each of its four sites rather than by
    /// a decorator around it (DESIGN.md §9), because a guard's `renew` and
    /// `release` arrive on a channel that nothing wrapping this trait can see.
    #[must_use]
    pub fn lock(&self) -> Arc<dyn DistributedLockBackend> {
        Arc::clone(&self.lock) as Arc<dyn DistributedLockBackend>
    }

    /// Shuts the plugin down (DESIGN.md §11).
    ///
    /// 1. Cancels the shared token, which unparks every blocked `lock()` waiter
    ///    — they return `ClusterError::Shutdown` rather than sitting out their
    ///    whole budget for a `LockTimeout` that would read as contention — and
    ///    ends every per-guard task.
    /// 2. Drains the guard tasks under a bound, **before** the pool closes,
    ///    since a task mid-`renew` still needs a connection.
    /// 3. Quits the subscriber, then the command pool, both bounded.
    ///
    /// **Held leases are left to expire** (`cpt-cf-clst-fr-shutdown-ttl-cleanup`,
    /// `RD-LOCK-013`): there is no best-effort remote cleanup here, and adding
    /// one would fail that test on purpose.
    pub async fn stop(mut self) {
        self.shutdown.cancel();
        self.lock.drain_guards().await;
        if let Some(observer) = self.connection_state.take() {
            let _observer_exited = observer.await;
        }
        if let Some(subscription) = self.subscription.take() {
            let _fan_out_exited = subscription.fan_out.await;
            let _watchdog_exited = subscription.watchdog.await;
            // `fred`'s replay task ends with the client rather than with the
            // token, so it is aborted rather than joined.
            subscription.manager.abort();
            quit_subscriber(&subscription.client).await;
        }
        close_pool(&self.pool).await;
        self.stopped = true;
    }
}

/// Diagnostic guard (ADR-006 §Confirmation): dropping a `RedisLockHandle`
/// without calling `stop()` leaves its pool, its subscriber, and its guard tasks
/// running, surfaced loudly rather than silently.
impl Drop for RedisLockHandle {
    fn drop(&mut self) {
        match cancel_and_diagnose_drop(self.stopped, &self.shutdown) {
            DropDiagnosis::StoppedCleanly => {}
            // not-a-catalogued-event: an ADR-006 developer diagnostic, not an
            // operator's business — this is the release-build arm of the same
            // programming error that panics in debug.
            DropDiagnosis::DuringPanic => warn!(
                "RedisLockHandle dropped during panic unwind without stop(); skipping debug panic \
                 to avoid double-panic abort"
            ),
            DropDiagnosis::Unstopped => {
                #[cfg(debug_assertions)]
                panic!("RedisLockHandle dropped without stop() - programming error");
                // not-a-catalogued-event: as above.
                #[cfg(not(debug_assertions))]
                warn!(
                    "RedisLockHandle dropped without stop() - programming error; the command pool \
                     and any background tasks may leak"
                );
            }
        }
    }
}

// Layer-1 unit tests (TESTING.md §2, `lock/mod.rs` row): key and channel
// construction, the holder token, the `SET NX PX` argument assembly, and the
// three-outcome classification of a failed `lock()`. Out-of-line per DE1101.
#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
