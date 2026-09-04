//! Zone-based throttling middleware.
//!
//! Two spec-driven maps are built from the registered operations:
//!
//! - [`ThrottlingMapNoAuth`] — operations whose `ThrottlingSpec` has
//!   `require_security_context = false`. Enforced *before* authentication and
//!   restricted to IP-keyed zones (identity keying is unavailable pre-auth).
//! - [`ThrottlingMap`] — operations with `require_security_context = true`.
//!   Enforced *after* authentication; identity-keyed zones use the subject id
//!   from the request's `SecurityContext`.
//!
//! Each `(method, path)` lands in exactly one map (decided by the per-operation
//! flag).
//!
//! On a served request, the rate-limit zone's `RateLimit-*` (and legacy
//! `X-RateLimit-*`) metadata headers are attached to the response.
//!
//! When an operation's `ThrottlingSpec` sets `dry_run = true`, limits are
//! observed but not enforced: a request that would have been rejected is served
//! instead, counted in the `throttling.dry_run_rejections` counter (attributes
//! `zone`/`kind`) and logged at `warn`.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashmap::mapref::entry::Entry;
use dashmap::{DashMap, DashSet};
use governor::clock::{Clock, DefaultClock};
use governor::middleware::StateInformationMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use toolkit::api::{OperationSpec, ThrottlingSpec};
use toolkit_security::SecurityContext;

use crate::config::{ApiGatewayConfig, InFlightLimitZone, KeyType, RateLimitZone, RetryAfter};
use crate::middleware::common;
use crate::middleware::errors::ApiGatewayGatewayError;

type ThrottleKey = (Method, String);

/// Floor for the `Retry-After` hint on in-flight rejections (seconds).
const DEFAULT_IN_FLIGHT_RETRY_AFTER_SECS: u64 = 5;

/// Interval between background prunes of throttling keyed stores.
///
/// Both keyed stores create one entry per distinct key and never evict on the
/// request path, so a periodic off-hot-path sweep reclaims stale entries —
/// fully-replenished rate-limit buckets and idle in-flight gates — bounding
/// memory even when keys are attacker-influenced (e.g. per-IP zones).
const KEY_PRUNE_INTERVAL: Duration = Duration::from_secs(10);

/// Keyed token-bucket limiter (one entry per identity/IP key).
type KeyedRateLimiter =
    RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock, StateInformationMiddleware>;

/// A resolved rate-limit zone: config + shared keyed limiter state.
struct RateZone {
    /// Zone name, used as the low-cardinality `zone` metric/log attribute.
    name: String,
    cfg: RateLimitZone,
    limiter: KeyedRateLimiter,
    policy: HeaderValue,
    /// Keys currently admitted into the limiter's keyed store, capped at
    /// `cfg.max_keys` (see [`RateZone::admit`]).
    admitted: DashSet<String>,
    /// Number of keys in `admitted`, kept as a counter so the hot path never
    /// walks every shard (`DashSet::len`) to check the cap.
    admitted_len: AtomicU64,
}

impl RateZone {
    /// Admit `key` into the zone, enforcing the `max_keys` cap.
    ///
    /// Returns `true` when the key is already admitted or the admission set is
    /// under `cfg.max_keys` (the key is then inserted); `false` when the set is
    /// saturated — the request should be rejected rather than allowed to grow
    /// the limiter's keyed store.
    ///
    /// The cap is approximate rather than exact, deliberately: the
    /// check-then-insert is not atomic (a concurrent race can overshoot by a
    /// few entries, and the counter may drift by that many across one prune
    /// interval), and the background prune resets the set while the limiter
    /// retains still-replenishing keys — a retained key that is not re-admitted
    /// gets no further `check_key` calls, replenishes untouched, and is dropped
    /// by the first sweep after the zone's replenish window
    /// `W = (burst_limit + 1) / rps`, so limiter size stays bounded by
    /// `max_keys x (1 + ceil(W / prune_interval))`. Making the cap exact would
    /// require locking the request hot path; the goal here is bounding memory
    /// growth, not a precise count. Unadmitted keys never reach the limiter —
    /// in enforce mode they are rejected, in dry-run mode logged and served
    /// without consulting it.
    fn admit(&self, key: &str) -> bool {
        if self.admitted.contains(key) {
            return true;
        }
        if self.admitted_len.load(Ordering::Relaxed) >= self.cfg.max_keys {
            return false;
        }
        if self.admitted.insert(key.to_owned()) {
            self.admitted_len.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// Reset admission tracking; called by the background prune right after the
    /// limiter's own stale-key sweep so freed capacity becomes admittable again.
    /// Keys that survived the limiter sweep are re-admitted on their next
    /// request. Not atomic with concurrent admits: the counter may lag the
    /// set by their number for one interval; the next reset resynchronises it.
    fn reset_admitted(&self) {
        self.admitted.clear();
        self.admitted.shrink_to_fit();
        self.admitted_len.store(0, Ordering::Relaxed);
    }
}

/// Per-key concurrency gate for an in-flight zone.
struct KeyGate {
    inflight: Arc<Semaphore>,
    backlog: Arc<Semaphore>,
}

impl KeyGate {
    /// Acquire an in-flight permit, optionally waiting in the backlog.
    ///
    /// Returns `None` when the request should be rejected (no in-flight slot and
    /// either no backlog capacity or the backlog wait timed out).
    async fn acquire(&self, backlog_timeout: Duration) -> Option<OwnedSemaphorePermit> {
        if let Ok(permit) = Arc::clone(&self.inflight).try_acquire_owned() {
            return Some(permit);
        }
        // No free slot: take a backlog slot and wait for one to free up.
        let _backlog_slot = Arc::clone(&self.backlog).try_acquire_owned().ok()?;
        if let Ok(Ok(permit)) =
            tokio::time::timeout(backlog_timeout, Arc::clone(&self.inflight).acquire_owned()).await
        {
            Some(permit)
        } else {
            None
        }
        // `_backlog_slot` is released here, before the in-flight permit is held.
    }
}

/// A resolved in-flight (concurrency) zone with per-key gates.
struct InFlightZone {
    /// Zone name, used as the low-cardinality `zone` metric/log attribute.
    name: String,
    cfg: InFlightLimitZone,
    keys: DashMap<String, Arc<KeyGate>>,
    /// Number of gates in `keys`, kept as a counter so the hot path never
    /// walks every shard (`DashMap::len`) to check the cap.
    tracked: AtomicU64,
    excluded: HashSet<String>,
}

impl InFlightZone {
    /// Resolve the per-key gate, enforcing the `max_keys` cap on admission.
    ///
    /// Returns the existing gate, or a new one while the zone tracks fewer than
    /// `cfg.max_keys` keys; `None` when the cap is reached, so a flood of
    /// distinct keys cannot grow the map until the periodic prune frees
    /// capacity. Like [`RateZone::admit`], the cap is approximate: concurrent
    /// first-time keys can overshoot it by their number.
    fn gate(&self, key: &str) -> Option<Arc<KeyGate>> {
        if let Some(existing) = self.keys.get(key) {
            return Some(Arc::clone(&existing));
        }
        if self.tracked.load(Ordering::Relaxed) >= self.cfg.max_keys {
            return None;
        }
        // Count only real insertions: `entry` may find a concurrent insert.
        let gate = match self.keys.entry(key.to_owned()) {
            Entry::Occupied(e) => Arc::clone(e.get()),
            Entry::Vacant(v) => {
                self.tracked.fetch_add(1, Ordering::Relaxed);
                Arc::clone(&v.insert(Arc::new(KeyGate {
                    inflight: Arc::new(Semaphore::new(self.cfg.in_flight_limit as usize)),
                    backlog: Arc::new(Semaphore::new(self.cfg.backlog_limit as usize)),
                })))
            }
        };
        Some(gate)
    }

    /// Drop gates no longer referenced by an in-flight request and reopen
    /// `max_keys` admission. Runs off the request hot path (periodic background
    /// sweep) so a flood of distinct keys cannot turn every request into an
    /// all-shard write-locking `DashMap::retain` scan. The counter check skips
    /// the scan entirely while the map is under its cap. A gate held by a live
    /// request is never evicted, so a cap filled by live requests stays closed
    /// until they finish. Not atomic with concurrent inserts: the counter may
    /// lag the map by their number for one interval; the next sweep
    /// resynchronises it.
    fn prune_idle_keys(&self) {
        if self.tracked.load(Ordering::Relaxed) >= self.cfg.max_keys {
            self.keys.retain(|_, v| Arc::strong_count(v) > 1);
            self.tracked
                .store(self.keys.len() as u64, Ordering::Relaxed);
        }
    }
}

/// A per-operation throttling entry.
struct ThrottlingEntry {
    spec: ThrottlingSpec,
    rate_zone: Option<Arc<RateZone>>,
    inflight_zone: Option<Arc<InFlightZone>>,
}

/// Shared inner state for both throttling maps.
///
/// Each [`ThrottlingEntry`] holds `Arc` handles to its resolved zones, so the
/// zone runtimes stay alive for as long as the routing table; no separate
/// zone registry is needed at request time.
#[derive(Default)]
struct ThrottlingInner {
    routes: HashMap<ThrottleKey, ThrottlingEntry>,
    /// Number of trusted reverse-proxy hops used when deriving the client IP for
    /// IP-keyed zones (see [`client_ip`]).
    trusted_proxy_hops: usize,
    /// Counter of enforced throttling rejections, labeled by `zone`/`kind`.
    /// `None` only for `Default`-constructed (empty) maps, which never reject.
    rejections: Option<Counter<u64>>,
    /// Counter of would-be rejections served because the operation is in
    /// dry-run mode, labeled by `zone`/`kind` like `rejections`. This is the
    /// graphable signal for tuning limits before enforcing them.
    dry_run: Option<Counter<u64>>,
}

/// Post-auth throttling map (identity-keyed zones allowed).
#[derive(Clone, Default)]
pub struct ThrottlingMap {
    inner: Arc<ThrottlingInner>,
}

/// Pre-auth throttling map (IP-keyed zones only).
#[derive(Clone, Default)]
pub struct ThrottlingMapNoAuth {
    inner: Arc<ThrottlingInner>,
}

impl ThrottlingMap {
    /// Build the post-auth (`require_security_context = true`) throttling map.
    ///
    /// Prefer [`build_maps`] when constructing both partitions so that a zone
    /// referenced from both shares a single limiter instance. This constructor
    /// builds an isolated partition (its zone runtimes are not shared with any
    /// pre-auth map) and is intended for standalone use such as tests.
    ///
    /// # Errors
    /// Returns an error if an entry references an undefined zone or an invalid
    /// (e.g. zero-limit) zone.
    pub fn from_specs(specs: &[OperationSpec], cfg: &ApiGatewayConfig) -> Result<Self> {
        let mut rate_zones = HashMap::new();
        let mut inflight_zones = HashMap::new();
        Ok(Self {
            inner: Arc::new(build(
                specs,
                cfg,
                true,
                &mut rate_zones,
                &mut inflight_zones,
            )?),
        })
    }
}

impl ThrottlingMapNoAuth {
    /// Build the pre-auth (`require_security_context = false`) throttling map.
    ///
    /// Prefer [`build_maps`] when constructing both partitions so that a zone
    /// referenced from both shares a single limiter instance. This constructor
    /// builds an isolated partition and is intended for standalone use such as
    /// tests.
    ///
    /// # Errors
    /// Returns an error if an entry references an undefined zone, an invalid
    /// zone, or an identity-keyed zone (forbidden before authentication).
    pub fn from_specs(specs: &[OperationSpec], cfg: &ApiGatewayConfig) -> Result<Self> {
        let mut rate_zones = HashMap::new();
        let mut inflight_zones = HashMap::new();
        Ok(Self {
            inner: Arc::new(build(
                specs,
                cfg,
                false,
                &mut rate_zones,
                &mut inflight_zones,
            )?),
        })
    }
}

/// Build both throttling partitions, sharing zone runtimes across them.
///
/// A single set of zone caches is populated across both the post-auth and
/// pre-auth passes, so any operation referencing the same zone name shares the
/// same `Arc<RateZone>` / `Arc<InFlightZone>` regardless of
/// `require_security_context`. This guarantees a zone's token bucket / in-flight
/// gate is a single instance rather than one per auth partition.
///
/// # Errors
/// Returns an error if any entry references an undefined or invalid zone, or an
/// identity-keyed zone from a pre-auth operation.
pub fn build_maps(
    specs: &[OperationSpec],
    cfg: &ApiGatewayConfig,
) -> Result<(ThrottlingMap, ThrottlingMapNoAuth, ThrottleKeyPruner)> {
    let mut rate_zones: HashMap<String, Arc<RateZone>> = HashMap::new();
    let mut inflight_zones: HashMap<String, Arc<InFlightZone>> = HashMap::new();
    let auth = build(specs, cfg, true, &mut rate_zones, &mut inflight_zones)?;
    let noauth = build(specs, cfg, false, &mut rate_zones, &mut inflight_zones)?;
    // These caches hold one deduplicated `Arc` per named zone shared across both
    // partitions, so they are exactly the sets the pruner must sweep.
    let pruner = ThrottleKeyPruner {
        rate_zones: rate_zones.into_values().collect(),
        inflight_zones: inflight_zones.into_values().collect(),
    };
    Ok((
        ThrottlingMap {
            inner: Arc::new(auth),
        },
        ThrottlingMapNoAuth {
            inner: Arc::new(noauth),
        },
        pruner,
    ))
}

/// Owns the throttling zones whose keyed stores require periodic pruning.
///
/// Neither keyed store evicts on the request path: both zone kinds cap growth
/// on admission (`max_keys`, see [`RateZone::admit`] and [`InFlightZone::gate`])
/// and rely on this sweep to drop stale buckets / idle gates and reopen
/// admission. Doing the prune off the hot path avoids
/// turning a distinct-key flood into a per-request all-shard write-locking
/// scan. Call [`ThrottleKeyPruner::spawn`] once the gear's lifecycle token is
/// available.
pub struct ThrottleKeyPruner {
    rate_zones: Vec<Arc<RateZone>>,
    inflight_zones: Vec<Arc<InFlightZone>>,
}

impl ThrottleKeyPruner {
    /// Spawn a background task that periodically prunes stale keys from every
    /// throttling zone's keyed store, keeping memory bounded. The task runs
    /// until `cancel` is triggered (gear shutdown) and then exits.
    ///
    /// Returns `None` (spawning nothing) when there are no throttling zones.
    #[must_use]
    pub fn spawn(self, cancel: CancellationToken) -> Option<tokio::task::JoinHandle<()>> {
        if self.rate_zones.is_empty() && self.inflight_zones.is_empty() {
            return None;
        }
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(KEY_PRUNE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Consume the immediate first tick so pruning starts one interval in.
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        for zone in &self.rate_zones {
                            // Drop buckets that have fully replenished
                            // (indistinguishable from never-seen keys), then
                            // release the reclaimed capacity and reopen
                            // `max_keys` admission.
                            zone.limiter.retain_recent();
                            zone.limiter.shrink_to_fit();
                            zone.reset_admitted();
                        }
                        for zone in &self.inflight_zones {
                            // Drop gates no longer held by an in-flight request
                            // and reopen `max_keys` admission.
                            zone.prune_idle_keys();
                        }
                    }
                }
            }
        }))
    }
}

/// Shared builder used by both maps, selecting specs by `require_ctx`.
///
/// `rate_zones` / `inflight_zones` are caches shared across partitions so the
/// same named zone resolves to a single `Arc` instance regardless of which
/// partition first builds it.
fn build(
    specs: &[OperationSpec],
    cfg: &ApiGatewayConfig,
    require_ctx: bool,
    rate_zones: &mut HashMap<String, Arc<RateZone>>,
    inflight_zones: &mut HashMap<String, Arc<InFlightZone>>,
) -> Result<ThrottlingInner> {
    let mut routes: HashMap<ThrottleKey, ThrottlingEntry> = HashMap::new();

    for spec in specs {
        let Some(thr) = spec.throttling.as_ref() else {
            continue;
        };
        if thr.require_security_context != require_ctx {
            continue;
        }

        let rate_zone = if let Some(zone_name) = thr.rate_limit_zone.as_deref() {
            let zcfg = cfg.rate_limit_zones.get(zone_name).ok_or_else(|| {
                anyhow!(
                    "throttling: operation {} {} references undefined rate_limit zone '{}'",
                    spec.method,
                    spec.path,
                    zone_name
                )
            })?;
            check_key_type(require_ctx, zone_name, zcfg.key.key_type)?;
            Some(get_or_build_rate_zone(rate_zones, zone_name, zcfg)?)
        } else {
            None
        };

        let inflight_zone = if let Some(zone_name) = thr.in_flight_limit_zone.as_deref() {
            let zcfg = cfg.in_flight_limit_zones.get(zone_name).ok_or_else(|| {
                anyhow!(
                    "throttling: operation {} {} references undefined in_flight_limit zone '{}'",
                    spec.method,
                    spec.path,
                    zone_name
                )
            })?;
            check_key_type(require_ctx, zone_name, zcfg.key.key_type)?;
            Some(get_or_build_inflight_zone(inflight_zones, zone_name, zcfg))
        } else {
            None
        };

        let key = (spec.method.clone(), spec.path.clone());
        routes.insert(
            key,
            ThrottlingEntry {
                spec: thr.clone(),
                rate_zone,
                inflight_zone,
            },
        );
    }

    Ok(ThrottlingInner {
        routes,
        trusted_proxy_hops: cfg.trusted_proxy_hops,
        rejections: Some(build_counter(
            cfg,
            "throttling.rejections",
            "Number of requests rejected by enforced throttling (429)",
        )),
        dry_run: Some(build_counter(
            cfg,
            "throttling.dry_run_rejections",
            "Number of requests that exceeded a throttling limit but were served (dry-run)",
        )),
    })
}

/// Build a throttling counter, honoring the configured metrics prefix.
///
/// Instruments are deduplicated by name within a meter, so building this once
/// per partition yields a single time series. Attributes are limited to the
/// low-cardinality `zone`/`kind`; the per-client bucket key is never a label.
fn build_counter(cfg: &ApiGatewayConfig, suffix: &str, description: &str) -> Counter<u64> {
    let prefix = cfg.metrics.prefix.trim().trim_end_matches('.');
    let name = if prefix.is_empty() {
        suffix.to_owned()
    } else {
        format!("{prefix}.{suffix}")
    };
    let scope = opentelemetry::InstrumentationScope::builder("api-gateway").build();
    let meter = opentelemetry::global::meter_with_scope(scope);
    meter
        .u64_counter(name)
        .with_description(description.to_owned())
        .build()
}

/// Identity keying is only valid after authentication.
fn check_key_type(require_ctx: bool, zone: &str, kt: KeyType) -> Result<()> {
    if !require_ctx && matches!(kt, KeyType::Identity) {
        bail!(
            "throttling: zone '{zone}' is identity-keyed but is referenced by a pre-auth \
             (require_security_context=false) operation; identity keying requires authentication"
        );
    }
    Ok(())
}

fn get_or_build_rate_zone(
    zones: &mut HashMap<String, Arc<RateZone>>,
    name: &str,
    cfg: &RateLimitZone,
) -> Result<Arc<RateZone>> {
    if let Some(existing) = zones.get(name) {
        return Ok(Arc::clone(existing));
    }
    let rps = NonZeroU32::new(cfg.rate_limit.rps)
        .ok_or_else(|| anyhow!("throttling: rate_limit zone '{name}' has rps = 0"))?;
    let burst = NonZeroU32::new(cfg.burst_limit)
        .ok_or_else(|| anyhow!("throttling: rate_limit zone '{name}' has burst_limit = 0"))?;
    let limiter = RateLimiter::keyed(Quota::per_second(rps).allow_burst(burst))
        .with_middleware::<StateInformationMiddleware>();
    let policy = HeaderValue::from_str(&format!(
        "\"burst\";q={};w={}",
        cfg.burst_limit, cfg.rate_limit.rps
    ))
    .context("throttling: failed to build RateLimit-Policy header")?;
    let zone = Arc::new(RateZone {
        name: name.to_owned(),
        cfg: cfg.clone(),
        limiter,
        policy,
        admitted: DashSet::new(),
        admitted_len: AtomicU64::new(0),
    });
    zones.insert(name.to_owned(), Arc::clone(&zone));
    Ok(zone)
}

fn get_or_build_inflight_zone(
    zones: &mut HashMap<String, Arc<InFlightZone>>,
    name: &str,
    cfg: &InFlightLimitZone,
) -> Arc<InFlightZone> {
    if let Some(existing) = zones.get(name) {
        return Arc::clone(existing);
    }
    let zone = Arc::new(InFlightZone {
        name: name.to_owned(),
        cfg: cfg.clone(),
        keys: DashMap::new(),
        tracked: AtomicU64::new(0),
        excluded: cfg.excluded_keys.iter().cloned().collect(),
    });
    zones.insert(name.to_owned(), Arc::clone(&zone));
    zone
}

/// Post-auth throttling middleware (uses [`ThrottlingMap`]).
pub async fn throttling_middleware(map: ThrottlingMap, req: Request, next: Next) -> Response {
    enforce(&map.inner, req, next).await
}

/// Pre-auth throttling middleware (uses [`ThrottlingMapNoAuth`]).
pub async fn throttling_no_auth_middleware(
    map: ThrottlingMapNoAuth,
    req: Request,
    next: Next,
) -> Response {
    enforce(&map.inner, req, next).await
}

async fn enforce(inner: &ThrottlingInner, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(|| req.uri().path().to_owned(), |p| p.as_str().to_owned());
    let path = common::resolve_path(&req, path.as_str());
    let key = (method, path);

    let Some(entry) = inner.routes.get(&key) else {
        return next.run(req).await;
    };

    // Rate-limit metadata headers to attach to the *response* once we have one.
    let mut rate_headers: Option<RateHeaders> = None;

    // Rate limiting.
    if let Some(zone) = entry.rate_zone.as_ref() {
        let id = compute_key(zone.cfg.key.key_type, &req, inner.trusted_proxy_hops);
        // `max_keys` admission cap: a new key beyond the cap is rejected before
        // it can grow the limiter's keyed store (until the next prune frees
        // capacity). Existing keys are unaffected. In dry-run mode the
        // would-be rejection is logged and the limiter is deliberately NOT
        // consulted: feeding unadmitted keys to `check_key` would create
        // keyed-store state past the cap, unbounding memory in observe mode.
        if zone.admit(&id) {
            match zone.limiter.check_key(&id) {
                Ok(snapshot) => {
                    rate_headers = Some(RateHeaders {
                        policy: zone.policy.clone(),
                        burst: HeaderValue::from(zone.cfg.burst_limit),
                        remaining: HeaderValue::from(snapshot.remaining_burst_capacity()),
                    });
                }
                Err(not_until) => {
                    if entry.spec.dry_run {
                        // Dry-run: observe but don't enforce. Count, log, fall through.
                        record_dry_run(inner, &key, &zone.name, "rate_limit", &id);
                    } else {
                        let wait = not_until
                            .wait_time_from(zone.limiter.clock().now())
                            .as_secs();
                        let retry_after = match zone.cfg.response_retry_after {
                            RetryAfter::Auto => Some(wait),
                            RetryAfter::Seconds(n) => Some(n),
                        };
                        record_rejection(inner, &key, &zone.name, "rate_limit", &id);
                        return throttle_response(
                            zone.cfg.response_status_code,
                            retry_after,
                            Some((&zone.policy, zone.cfg.burst_limit)),
                        );
                    }
                }
            }
        } else if entry.spec.dry_run {
            record_dry_run(inner, &key, &zone.name, "max_keys", &id);
        } else {
            record_rejection(inner, &key, &zone.name, "max_keys", &id);
            return throttle_response(
                zone.cfg.response_status_code,
                Some(KEY_PRUNE_INTERVAL.as_secs()),
                Some((&zone.policy, zone.cfg.burst_limit)),
            );
        }
    }

    // In-flight concurrency limiting.
    if let Some(zone) = entry.inflight_zone.as_ref() {
        let id = compute_key(zone.cfg.key.key_type, &req, inner.trusted_proxy_hops);
        if !zone.excluded.contains(&id) {
            // `max_keys` admission cap, same contract as the rate zone above:
            // a never-seen key past the cap is refused before it can grow the
            // gate map; dry-run logs and serves without a gate.
            let Some(gate) = zone.gate(&id) else {
                if entry.spec.dry_run {
                    record_dry_run(inner, &key, &zone.name, "max_keys", &id);
                    let mut response = next.run(req).await;
                    apply_rate_headers(&mut response, rate_headers.as_ref());
                    return response;
                }
                record_rejection(inner, &key, &zone.name, "max_keys", &id);
                return throttle_response(
                    zone.cfg.response_status_code,
                    Some(KEY_PRUNE_INTERVAL.as_secs()),
                    None,
                );
            };
            let Some(permit) = gate.acquire(zone.cfg.backlog_timeout).await else {
                if entry.spec.dry_run {
                    // Dry-run: observe but don't enforce. Count, log and serve
                    // the request without holding an in-flight permit.
                    record_dry_run(inner, &key, &zone.name, "in_flight", &id);
                    let mut response = next.run(req).await;
                    apply_rate_headers(&mut response, rate_headers.as_ref());
                    return response;
                }
                record_rejection(inner, &key, &zone.name, "in_flight", &id);
                // Suggest a retry after roughly the backlog wait window, with a
                // sensible floor so clients always get a usable hint.
                let retry_after = zone
                    .cfg
                    .backlog_timeout
                    .as_secs()
                    .max(DEFAULT_IN_FLIGHT_RETRY_AFTER_SECS);
                return throttle_response(zone.cfg.response_status_code, Some(retry_after), None);
            };
            let mut response = next.run(req).await;
            drop(permit);
            apply_rate_headers(&mut response, rate_headers.as_ref());
            return response;
        }
    }

    let mut response = next.run(req).await;
    apply_rate_headers(&mut response, rate_headers.as_ref());
    response
}

/// Rate-limit metadata headers echoed on successful (served) responses.
struct RateHeaders {
    policy: HeaderValue,
    burst: HeaderValue,
    remaining: HeaderValue,
}

/// Attach `RateLimit-*` (and legacy `X-RateLimit-*`) headers to a response.
fn apply_rate_headers(response: &mut Response, rate_headers: Option<&RateHeaders>) {
    let Some(h) = rate_headers else {
        return;
    };
    let headers = response.headers_mut();
    headers.insert("RateLimit-Policy", h.policy.clone());
    headers.insert("RateLimit-Limit", h.burst.clone());
    headers.insert("RateLimit-Remaining", h.remaining.clone());
    // Legacy `X-RateLimit-*` headers for compatibility with the pre-zone limiter.
    headers.insert("X-RateLimit-Limit", h.burst.clone());
    headers.insert("X-RateLimit-Remaining", h.remaining.clone());
}

/// Compute the throttling key for a request according to the zone key type.
fn compute_key(kind: KeyType, req: &Request, trusted_proxy_hops: usize) -> String {
    match kind {
        KeyType::Ip => client_ip(req, trusted_proxy_hops),
        KeyType::Identity => req
            .extensions()
            .get::<SecurityContext>()
            .map_or_else(|| "anonymous".to_owned(), |sc| sc.subject_id().to_string()),
    }
}

/// Resolve the client IP used as the throttling bucket key.
///
/// Client-supplied forwarding headers (`X-Forwarded-For` / `X-Real-IP`) are
/// only honored when the gateway sits behind a known number of trusted reverse
/// proxies, given by `trusted_proxy_hops`:
///
/// - `0` (the default): the headers are fully client-controlled and are
///   ignored. The peer address from `ConnectInfo` is used (else `"unknown"`),
///   so a caller cannot spoof or rotate the bucket key to bypass IP limits.
/// - `n >= 1`: the client IP is taken from the `X-Forwarded-For` entry `n`
///   positions from the right — the value appended by the outermost trusted
///   proxy, which an untrusted client cannot forge (any spoofed entries it
///   prepends only shift this index further right). When `X-Forwarded-For` is
///   absent or too short, the immediate (trusted) proxy's `X-Real-IP` is used,
///   then the peer address, then `"unknown"`.
fn client_ip(req: &Request, trusted_proxy_hops: usize) -> String {
    if trusted_proxy_hops == 0 {
        return peer_ip(req);
    }
    let headers = req.headers();
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let entries: Vec<&str> = xff
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(idx) = entries.len().checked_sub(trusted_proxy_hops)
            && let Some(candidate) = entries.get(idx)
            && let Ok(ip) = candidate.parse::<IpAddr>()
        {
            return ip.to_string();
        }
    }
    // The immediate peer is a trusted proxy, so its `X-Real-IP` is trustworthy.
    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<IpAddr>().ok())
    {
        return ip.to_string();
    }
    peer_ip(req)
}

/// The immediate peer address from `ConnectInfo`, or `"unknown"` when absent.
fn peer_ip(req: &Request) -> String {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(|| "unknown".to_owned(), |ci| ci.0.ip().to_string())
}

/// Build a throttling rejection response.
///
/// `rate_headers` carries `(policy, burst_limit)` for rate-limit rejections so
/// the `RateLimit-*` / legacy `X-RateLimit-Limit` headers are echoed on the
/// error (matching the legacy rate limiter); it is `None` for in-flight
/// rejections, which have no token-bucket policy.
fn throttle_response(
    status: u16,
    retry_after_seconds: Option<u64>,
    rate_headers: Option<(&HeaderValue, u32)>,
) -> Response {
    let err = ApiGatewayGatewayError::resource_exhausted("throttling limit exceeded")
        .with_quota_violation("throttling", "limit exceeded")
        .create();
    let mut response = err.into_response();
    if let Ok(code) = StatusCode::from_u16(status) {
        *response.status_mut() = code;
    }
    let headers = response.headers_mut();
    if let Some((policy, burst_limit)) = rate_headers {
        let burst = HeaderValue::from(burst_limit);
        headers.insert("RateLimit-Policy", policy.clone());
        headers.insert("RateLimit-Limit", burst.clone());
        headers.insert("X-RateLimit-Limit", burst);
    }
    if let Some(secs) = retry_after_seconds
        && let Ok(value) = HeaderValue::from_str(&secs.to_string())
    {
        headers.insert(header::RETRY_AFTER, value);
    }
    response
}

/// Record an enforced throttling rejection: bump the `zone`/`kind` counter and
/// log at `info` so operators have production visibility at the default level.
///
/// The high-cardinality bucket key stays a structured field (`key`) — never in
/// the message body and never a metric attribute.
fn record_rejection(inner: &ThrottlingInner, key: &ThrottleKey, zone: &str, kind: &str, id: &str) {
    if let Some(counter) = inner.rejections.as_ref() {
        counter.add(
            1,
            &[
                KeyValue::new("zone", zone.to_owned()),
                KeyValue::new("kind", kind.to_owned()),
            ],
        );
    }
    tracing::info!(
        method = %key.0,
        path = %key.1,
        kind,
        zone,
        key = %id,
        "throttling limit exceeded"
    );
}

/// Record a dry-run event: the request exceeded a limit (`kind` is
/// `rate_limit`, `max_keys` or `in_flight`) but is served because the
/// operation is in dry-run mode. Bumps the `zone`/`kind` dry-run counter and
/// logs at `warn` with the same structured shape as [`record_rejection`].
///
/// For `max_keys` the keyed store is deliberately not touched, so it stays
/// capped in observe mode too.
fn record_dry_run(inner: &ThrottlingInner, key: &ThrottleKey, zone: &str, kind: &str, id: &str) {
    if let Some(counter) = inner.dry_run.as_ref() {
        counter.add(
            1,
            &[
                KeyValue::new("zone", zone.to_owned()),
                KeyValue::new("kind", kind.to_owned()),
            ],
        );
    }
    tracing::warn!(
        method = %key.0,
        path = %key.1,
        kind,
        zone,
        key = %id,
        "throttling limit exceeded, serving because of dry-run mode"
    );
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "throttling_tests.rs"]
mod tests;
