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
//! instead, and a `warn` event is logged.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
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
    /// few entries), and the background prune resets the set while the limiter
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
        if (self.admitted.len() as u64) < self.cfg.max_keys {
            self.admitted.insert(key.to_owned());
            return true;
        }
        false
    }

    /// Reset admission tracking; called by the background prune right after the
    /// limiter's own stale-key sweep so freed capacity becomes admittable again.
    /// Keys that survived the limiter sweep are re-admitted on their next
    /// request.
    fn reset_admitted(&self) {
        self.admitted.clear();
        self.admitted.shrink_to_fit();
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
    excluded: HashSet<String>,
}

impl InFlightZone {
    fn gate(&self, key: &str) -> Arc<KeyGate> {
        if let Some(existing) = self.keys.get(key) {
            return Arc::clone(&existing);
        }
        Arc::clone(&self.keys.entry(key.to_owned()).or_insert_with(|| {
            Arc::new(KeyGate {
                inflight: Arc::new(Semaphore::new(self.cfg.in_flight_limit as usize)),
                backlog: Arc::new(Semaphore::new(self.cfg.backlog_limit as usize)),
            })
        }))
    }

    /// Soft `max_keys` cap: drop gates no longer referenced by an in-flight
    /// request. Runs off the request hot path (periodic background sweep) so a
    /// flood of distinct keys cannot turn every request into an all-shard
    /// write-locking `DashMap::retain` scan. The `len` check skips the scan
    /// entirely while the map is under its cap.
    fn prune_idle_keys(&self) {
        if self.keys.len() as u64 >= self.cfg.max_keys {
            self.keys.retain(|_, v| Arc::strong_count(v) > 1);
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
/// Neither keyed store evicts on the request path: rate zones cap growth via
/// the `max_keys` admission set (see [`RateZone::admit`]) and rely on this
/// sweep to drop stale buckets and reopen admission; in-flight zones drop idle
/// gates once over their soft cap. Doing the prune off the hot path avoids
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
                            // Drop gates no longer held by an in-flight request.
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
        rejections: Some(build_rejection_counter(cfg)),
    })
}

/// Build the enforced-rejection counter, honoring the configured metrics prefix.
///
/// Instruments are deduplicated by name within a meter, so building this once
/// per partition yields a single time series. Attributes are limited to the
/// low-cardinality `zone`/`kind`; the per-client bucket key is never a label.
fn build_rejection_counter(cfg: &ApiGatewayConfig) -> Counter<u64> {
    let prefix = cfg.metrics.prefix.trim().trim_end_matches('.');
    let name = if prefix.is_empty() {
        "throttling.rejections".to_owned()
    } else {
        format!("{prefix}.throttling.rejections")
    };
    let scope = opentelemetry::InstrumentationScope::builder("api-gateway").build();
    let meter = opentelemetry::global::meter_with_scope(scope);
    meter
        .u64_counter(name)
        .with_description("Number of requests rejected by enforced throttling (429)")
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
                        // Dry-run: observe but don't enforce. Log and fall through.
                        log_dry_run_rate(&id);
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
            log_dry_run_max_keys(&id);
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
            let gate = zone.gate(&id);
            let Some(permit) = gate.acquire(zone.cfg.backlog_timeout).await else {
                if entry.spec.dry_run {
                    // Dry-run: observe but don't enforce. Log and serve the
                    // request without holding an in-flight permit.
                    log_dry_run_in_flight(&id);
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

/// Dry-run `max_keys` event: the key would have been refused admission but the
/// request is served because the operation is in dry-run mode. The limiter is
/// not consulted for such keys, so its keyed store stays capped even in
/// observe mode.
fn log_dry_run_max_keys(id: &str) {
    tracing::warn!(
        rate_limit_key = %id,
        "too many tracked keys, serving will be continued because of dry run mode"
    );
}

/// Dry-run rate-limit event: the request would have been rate-limited but is
/// served because the operation is in dry-run mode.
fn log_dry_run_rate(id: &str) {
    tracing::warn!(
        rate_limit_key = %id,
        "too many requests, serving will be continued because of dry run mode"
    );
}

/// Dry-run in-flight event: the request would have been rejected by the
/// in-flight limit but is served because the operation is in dry-run mode.
fn log_dry_run_in_flight(id: &str) {
    tracing::warn!(
        in_flight_limit_key = %id,
        "too many in-flight requests, serving will be continued because of dry run mode"
    );
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{KeyConfig, RateSpec};
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use std::time::Duration;
    use tower::ServiceExt;

    use toolkit::api::operation_builder::VendorExtensions;

    fn op(method: Method, path: &str, throttling: Option<ThrottlingSpec>) -> OperationSpec {
        OperationSpec {
            method,
            path: path.to_owned(),
            operation_id: None,
            summary: None,
            description: None,
            tags: vec![],
            params: vec![],
            request_body: None,
            responses: vec![],
            handler_id: "test".to_owned(),
            authenticated: false,
            exposed: true,
            throttling,
            allowed_request_content_types: None,
            vendor_extensions: VendorExtensions::default(),
            license_requirement: None,
        }
    }

    /// Map a test zone argument to `Option<String>`, treating `""` as "no zone".
    fn zone(name: &str) -> Option<String> {
        (!name.is_empty()).then(|| name.to_owned())
    }

    fn thr(rate_zone: &str, inflight_zone: &str, require_ctx: bool) -> ThrottlingSpec {
        ThrottlingSpec {
            rate_limit_zone: zone(rate_zone),
            in_flight_limit_zone: zone(inflight_zone),
            require_security_context: require_ctx,
            dry_run: false,
        }
    }

    fn thr_dry(rate_zone: &str, inflight_zone: &str) -> ThrottlingSpec {
        ThrottlingSpec {
            rate_limit_zone: zone(rate_zone),
            in_flight_limit_zone: zone(inflight_zone),
            require_security_context: false,
            dry_run: true,
        }
    }

    fn rate_zone_cfg(rps: u32, burst: u32, key: KeyType) -> RateLimitZone {
        RateLimitZone {
            rate_limit: RateSpec { rps },
            burst_limit: burst,
            response_status_code: 429,
            response_retry_after: RetryAfter::Auto,
            key: KeyConfig { key_type: key },
            max_keys: 1000,
        }
    }

    fn inflight_zone_cfg(in_flight: u32, key: KeyType, excluded: Vec<String>) -> InFlightLimitZone {
        InFlightLimitZone {
            in_flight_limit: in_flight,
            backlog_limit: 0,
            backlog_timeout: Duration::from_millis(50),
            response_status_code: 429,
            key: KeyConfig { key_type: key },
            max_keys: 1000,
            excluded_keys: excluded,
        }
    }

    fn cfg_with_rate(name: &str, zone: RateLimitZone) -> ApiGatewayConfig {
        let mut cfg = ApiGatewayConfig::default();
        cfg.rate_limit_zones.insert(name.to_owned(), zone);
        cfg
    }

    #[test]
    fn partitions_specs_by_require_security_context() {
        let mut cfg = ApiGatewayConfig::default();
        cfg.rate_limit_zones
            .insert("ip".to_owned(), rate_zone_cfg(10, 10, KeyType::Ip));
        cfg.rate_limit_zones
            .insert("id".to_owned(), rate_zone_cfg(10, 10, KeyType::Identity));

        let specs = vec![
            op(Method::GET, "/pre", Some(thr("ip", "", false))),
            op(Method::GET, "/post", Some(thr("id", "", true))),
            op(Method::GET, "/none", None),
        ];

        let pre = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();
        let post = ThrottlingMap::from_specs(&specs, &cfg).unwrap();

        assert_eq!(pre.inner.routes.len(), 1);
        assert!(
            pre.inner
                .routes
                .contains_key(&(Method::GET, "/pre".to_owned()))
        );
        assert_eq!(post.inner.routes.len(), 1);
        assert!(
            post.inner
                .routes
                .contains_key(&(Method::GET, "/post".to_owned()))
        );
    }

    #[test]
    fn pre_auth_identity_zone_is_rejected() {
        let cfg = cfg_with_rate("id", rate_zone_cfg(10, 10, KeyType::Identity));
        let specs = vec![op(Method::GET, "/x", Some(thr("id", "", false)))];
        let err = ThrottlingMapNoAuth::from_specs(&specs, &cfg)
            .err()
            .expect("should error")
            .to_string();
        assert!(
            err.contains("identity keying requires authentication"),
            "{err}"
        );
    }

    #[test]
    fn undefined_zone_is_rejected() {
        let cfg = ApiGatewayConfig::default();
        let specs = vec![op(Method::GET, "/x", Some(thr("missing", "", false)))];
        let err = ThrottlingMapNoAuth::from_specs(&specs, &cfg)
            .err()
            .expect("should error")
            .to_string();
        assert!(err.contains("undefined rate_limit zone"), "{err}");
    }

    #[test]
    fn shared_zone_arc_within_map() {
        let cfg = cfg_with_rate("ip", rate_zone_cfg(10, 10, KeyType::Ip));
        let specs = vec![
            op(Method::GET, "/a", Some(thr("ip", "", false))),
            op(Method::GET, "/b", Some(thr("ip", "", false))),
        ];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();
        let a = map.inner.routes[&(Method::GET, "/a".to_owned())]
            .rate_zone
            .clone()
            .unwrap();
        let b = map.inner.routes[&(Method::GET, "/b".to_owned())]
            .rate_zone
            .clone()
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn shared_zone_arc_across_partitions() {
        // The same IP-keyed zone referenced by a pre-auth and a post-auth
        // operation must resolve to a single limiter instance.
        let cfg = cfg_with_rate("ip", rate_zone_cfg(10, 10, KeyType::Ip));
        let specs = vec![
            op(Method::GET, "/pre", Some(thr("ip", "", false))),
            op(Method::GET, "/post", Some(thr("ip", "", true))),
        ];
        let (auth, noauth, _pruner) = build_maps(&specs, &cfg).unwrap();
        let pre = noauth.inner.routes[&(Method::GET, "/pre".to_owned())]
            .rate_zone
            .clone()
            .unwrap();
        let post = auth.inner.routes[&(Method::GET, "/post".to_owned())]
            .rate_zone
            .clone()
            .unwrap();
        assert!(Arc::ptr_eq(&pre, &post));
    }

    #[test]
    fn client_ip_ignores_forwarding_headers_without_trusted_proxies() {
        // With trusted_proxy_hops = 0, client-supplied headers must be ignored
        // and the peer address used, so a caller cannot spoof the bucket key.
        let mut req = Request::builder()
            .header("x-forwarded-for", "203.0.113.7, 10.0.0.1")
            .header("x-real-ip", "198.51.100.9")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(
            "192.168.1.5:1234".parse::<SocketAddr>().unwrap(),
        ));
        assert_eq!(client_ip(&req, 0), "192.168.1.5");

        // No peer address either → "unknown".
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.7")
            .body(Body::empty())
            .unwrap();
        assert_eq!(client_ip(&req, 0), "unknown");
    }

    #[test]
    fn client_ip_uses_trusted_proxy_hop() {
        // One trusted proxy: the rightmost XFF entry is the peer-observed client
        // (or a spoofed prefix shifts the trusted index right, never affecting it).
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.7")
            .body(Body::empty())
            .unwrap();
        assert_eq!(client_ip(&req, 1), "203.0.113.7");

        // A spoofed leftmost entry is ignored; the trusted (rightmost) hop wins.
        let req = Request::builder()
            .header("x-forwarded-for", "1.1.1.1, 203.0.113.7")
            .body(Body::empty())
            .unwrap();
        assert_eq!(client_ip(&req, 1), "203.0.113.7");

        // Two trusted proxies: pick the entry two from the right.
        let req = Request::builder()
            .header("x-forwarded-for", "9.9.9.9, 203.0.113.7, 10.0.0.1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(client_ip(&req, 2), "203.0.113.7");
    }

    #[test]
    fn client_ip_trusted_proxy_falls_back_when_xff_short_or_invalid() {
        // Fewer XFF entries than trusted hops → fall back to X-Real-IP.
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.7")
            .header("x-real-ip", "198.51.100.9")
            .body(Body::empty())
            .unwrap();
        assert_eq!(client_ip(&req, 3), "198.51.100.9");

        // Non-IP XFF token → fall back to peer address.
        let mut req = Request::builder()
            .header("x-forwarded-for", "not-an-ip")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(
            "192.168.1.5:1234".parse::<SocketAddr>().unwrap(),
        ));
        assert_eq!(client_ip(&req, 1), "192.168.1.5");
    }

    #[test]
    fn compute_key_identity_uses_subject_or_anonymous() {
        // No SecurityContext present → anonymous.
        let req = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(compute_key(KeyType::Identity, &req, 0), "anonymous");
    }

    #[tokio::test]
    async fn rate_limit_denies_after_burst() {
        let cfg = cfg_with_rate("ip", rate_zone_cfg(1, 1, KeyType::Ip));
        let specs = vec![op(Method::GET, "/x", Some(thr("ip", "", false)))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        let first = app
            .clone()
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(second.headers().contains_key(header::RETRY_AFTER));
        // Rate-limit rejections echo the policy/limit headers (legacy parity).
        assert!(second.headers().contains_key("RateLimit-Policy"));
        assert!(second.headers().contains_key("RateLimit-Limit"));
        assert!(second.headers().contains_key("X-RateLimit-Limit"));
    }

    #[test]
    fn rate_zone_admit_caps_distinct_keys_until_reset() {
        let mut zones = HashMap::new();
        let mut cfg = rate_zone_cfg(1000, 1000, KeyType::Ip);
        cfg.max_keys = 2;
        let zone = get_or_build_rate_zone(&mut zones, "z", &cfg).unwrap();

        assert!(zone.admit("a"));
        assert!(zone.admit("b"));
        // Saturated: a new key is refused, already-admitted keys still pass.
        assert!(!zone.admit("c"));
        assert!(zone.admit("a"));

        // The prune resets admission; freed capacity is admittable again.
        zone.reset_admitted();
        assert!(zone.admit("c"));
    }

    #[tokio::test]
    async fn rate_limit_max_keys_rejects_new_keys_when_saturated() {
        let mut zone = rate_zone_cfg(1000, 1000, KeyType::Ip);
        zone.max_keys = 2;
        let cfg = cfg_with_rate("ip", zone);
        let specs = vec![op(Method::GET, "/x", Some(thr("ip", "", false)))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        let req_from = |ip: &str| {
            let mut req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            req.extensions_mut().insert(ConnectInfo(
                format!("{ip}:1000").parse::<SocketAddr>().unwrap(),
            ));
            req
        };

        // Two distinct client IPs fill the admission cap.
        for ip in ["10.0.0.1", "10.0.0.2"] {
            let resp = app.clone().oneshot(req_from(ip)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // A third, never-seen IP is refused admission outright.
        let third = app.clone().oneshot(req_from("10.0.0.3")).await.unwrap();
        assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            third
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some(KEY_PRUNE_INTERVAL.as_secs().to_string().as_str())
        );

        // Already-admitted keys keep flowing while the zone is saturated.
        let again = app.oneshot(req_from("10.0.0.1")).await.unwrap();
        assert_eq!(again.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn inflight_rejection_sets_retry_after() {
        let mut cfg = ApiGatewayConfig::default();
        // in_flight_limit = 0 with no backlog => first request is rejected.
        cfg.in_flight_limit_zones
            .insert("ifl".to_owned(), inflight_zone_cfg(0, KeyType::Ip, vec![]));
        let specs = vec![op(Method::GET, "/x", Some(thr("", "ifl", false)))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry = resp
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .expect("retry-after present");
        assert_eq!(retry, DEFAULT_IN_FLIGHT_RETRY_AFTER_SECS);
    }

    #[tokio::test]
    async fn inflight_excluded_key_bypasses_limit() {
        let mut cfg = ApiGatewayConfig::default();
        cfg.in_flight_limit_zones.insert(
            "ifl".to_owned(),
            inflight_zone_cfg(1, KeyType::Ip, vec!["unknown".to_owned()]),
        );
        let specs = vec![op(Method::GET, "/x", Some(thr("", "ifl", false)))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        // Client IP resolves to "unknown" (no ConnectInfo/headers), which is excluded.
        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rate_limit_headers_on_success_response() {
        let cfg = cfg_with_rate("ip", rate_zone_cfg(10, 10, KeyType::Ip));
        let specs = vec![op(Method::GET, "/x", Some(thr("ip", "", false)))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Metadata headers are exposed on the served response, not the request.
        let headers = resp.headers();
        assert!(headers.contains_key("RateLimit-Policy"));
        assert!(headers.contains_key("RateLimit-Limit"));
        assert!(headers.contains_key("RateLimit-Remaining"));
        assert!(headers.contains_key("X-RateLimit-Limit"));
        assert!(headers.contains_key("X-RateLimit-Remaining"));
    }

    #[tokio::test]
    async fn dry_run_rate_limit_serves_over_burst() {
        // rps 1 / burst 1: the second request would normally be rejected (429),
        // but dry-run serves it and logs instead.
        let cfg = cfg_with_rate("ip", rate_zone_cfg(1, 1, KeyType::Ip));
        let specs = vec![op(Method::GET, "/x", Some(thr_dry("ip", "")))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        let first = app
            .clone()
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // Would-be-throttled request is served instead of rejected.
        let second = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        // Bypassed requests carry no rejection hint.
        assert!(!second.headers().contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn dry_run_does_not_grow_limiter_past_max_keys() {
        // max_keys = 1: in dry-run every request is served, but unadmitted
        // keys must not create limiter state — the keyed store stays capped
        // even in observe mode.
        let mut zone = rate_zone_cfg(1000, 1000, KeyType::Ip);
        zone.max_keys = 1;
        let cfg = cfg_with_rate("ip", zone);
        let specs = vec![op(Method::GET, "/x", Some(thr_dry("ip", "")))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();
        let rate_zone = Arc::clone(
            map.inner
                .routes
                .values()
                .next()
                .unwrap()
                .rate_zone
                .as_ref()
                .unwrap(),
        );

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        for ip in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
            let mut req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            req.extensions_mut().insert(ConnectInfo(
                format!("{ip}:1000").parse::<SocketAddr>().unwrap(),
            ));
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        assert!(rate_zone.limiter.len() <= 1);
    }

    #[tokio::test]
    async fn dry_run_in_flight_serves_over_limit() {
        // in_flight_limit = 0 with no backlog => the request would normally be
        // rejected (429), but dry-run serves it and logs instead.
        let mut cfg = ApiGatewayConfig::default();
        cfg.in_flight_limit_zones
            .insert("ifl".to_owned(), inflight_zone_cfg(0, KeyType::Ip, vec![]));
        let specs = vec![op(Method::GET, "/x", Some(thr_dry("", "ifl")))];
        let map = ThrottlingMapNoAuth::from_specs(&specs, &cfg).unwrap();

        let app =
            Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    move |req: Request, next: Next| {
                        let map = map.clone();
                        async move { throttling_no_auth_middleware(map, req, next).await }
                    },
                ));

        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!resp.headers().contains_key(header::RETRY_AFTER));
    }

    #[test]
    fn throttle_key_pruner_without_zones_spawns_nothing() {
        // No throttling zones => empty pruner => nothing to spawn (no runtime needed).
        let (_, _, pruner) = build_maps(&[], &ApiGatewayConfig::default()).unwrap();
        assert!(pruner.spawn(CancellationToken::new()).is_none());
    }

    #[tokio::test]
    async fn throttle_key_pruner_task_stops_on_cancel() {
        // A configured zone yields a pruner that spawns a task bound to the
        // lifecycle token; cancelling it must let the task exit cleanly.
        let cfg = cfg_with_rate("ip", rate_zone_cfg(10, 10, KeyType::Ip));
        let specs = vec![op(Method::GET, "/x", Some(thr("ip", "", false)))];
        let (_, _, pruner) = build_maps(&specs, &cfg).unwrap();
        let cancel = CancellationToken::new();
        let handle = pruner
            .spawn(cancel.clone())
            .expect("zone present -> prune task spawned");
        cancel.cancel();
        handle.await.expect("prune task joins without panicking");
    }

    fn inflight_zone(max_keys: u64) -> InFlightZone {
        let mut cfg = inflight_zone_cfg(1, KeyType::Ip, vec![]);
        cfg.max_keys = max_keys;
        InFlightZone {
            name: "test".to_owned(),
            cfg,
            keys: DashMap::new(),
            excluded: HashSet::new(),
        }
    }

    #[test]
    fn inflight_gate_does_not_prune_on_hot_path() {
        // Regression: the request hot path must not scan/evict. Even well past
        // `max_keys`, gate() only inserts — pruning is deferred to the sweep.
        let zone = inflight_zone(1);
        drop(zone.gate("a"));
        drop(zone.gate("b"));
        drop(zone.gate("c"));
        assert_eq!(zone.keys.len(), 3);
    }

    #[test]
    fn inflight_prune_idle_keys_drops_only_unreferenced() {
        // Over the cap, the sweep drops gates with no in-flight holder
        // (strong_count == 1) and keeps those still referenced by a request.
        let zone = inflight_zone(1);
        let held = zone.gate("held"); // strong_count 2 (map + this handle)
        drop(zone.gate("idle")); // strong_count 1 (map only)
        assert_eq!(zone.keys.len(), 2);

        zone.prune_idle_keys();

        assert!(zone.keys.contains_key("held"));
        assert!(!zone.keys.contains_key("idle"));
        drop(held);
    }

    #[test]
    fn inflight_prune_idle_keys_skips_scan_under_cap() {
        // Under the cap the scan is skipped, so idle gates are retained.
        let zone = inflight_zone(100);
        drop(zone.gate("idle"));
        zone.prune_idle_keys();
        assert!(zone.keys.contains_key("idle"));
    }
}
