//! `CachedMetricRegistry`: the [`MetricRegistry`] port over the platform
//! `types-registry` client, with the bounded in-process cache the
//! quota-lifecycle feature requires ("Metric Validation").
//!
//! Every answer is read from the metric instance document the registry serves
//! and parsed into closed enums; a registered metric whose document lacks a
//! usable classification is an error, never a default. Answers are cached by
//! metric id with least-recently-used eviction and refreshed after `ttl`.
//! When the registry does not answer, a cached entry is served as `Stale`
//! only while it is younger than `ttl + stale_grace`; beyond that the caller
//! fails closed. Negative answers (`NotFound`, another type) are never cached,
//! so a late registration is seen at once. Every failure lifts through a named
//! function (DE1302).
//!
//! # Provisional classification contract
//!
//! The metric base `gts.cf.qe.metric.type.v1~` is a platform namespace (PRD
//! sections 3.2 and 13). Until the platform publishes its schema, QE reads two
//! fields of the instance document, named in [`contract`]:
//! `kind ∈ {counter, gauge}` and `enforcement ∈ {quota_gated, direct}`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use quota_enforcement_sdk::{METRIC_BASE_TYPE, MetricId, MetricKind};
use serde_json::Value;
use tokio::time::Instant;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::TypesRegistryClient;

use crate::domain::error::DomainError;
use crate::domain::ports::metric_registry::{
    Classified, Freshness, MetricDescriptor, MetricMode, MetricRegistry,
};

const LOG_TARGET: &str = "qe.metrics_registry";

/// Default budget for one registry call.
pub const DEFAULT_REGISTRY_DEADLINE: Duration = Duration::from_secs(10);

/// Field names and values of the provisional metric classification contract.
pub mod contract {
    /// The metric kind field of an instance document.
    pub const KIND: &str = "kind";
    /// `kind` value of a cumulative metric.
    pub const KIND_COUNTER: &str = "counter";
    /// `kind` value of a level metric.
    pub const KIND_GAUGE: &str = "gauge";
    /// The enforcement-mode field of an instance document.
    pub const ENFORCEMENT: &str = "enforcement";
    /// `enforcement` value of a metric whose usage flows through QE.
    pub const ENFORCEMENT_QUOTA_GATED: &str = "quota_gated";
    /// `enforcement` value of a metric recorded directly.
    pub const ENFORCEMENT_DIRECT: &str = "direct";
}

/// The bounded cache and its adapter.
pub struct CachedMetricRegistry {
    registry: Arc<dyn TypesRegistryClient>,
    deadline: Duration,
    ttl: Duration,
    stale_grace: Duration,
    cache: Mutex<MetricCache>,
}

struct CacheEntry {
    descriptor: MetricDescriptor,
    fetched_at: Instant,
    last_used: u64,
}

struct MetricCache {
    capacity: usize,
    tick: u64,
    entries: HashMap<MetricId, CacheEntry>,
}

impl MetricCache {
    fn touch(&mut self, metric: &MetricId) -> Option<&CacheEntry> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(metric)?;
        entry.last_used = tick;
        Some(entry)
    }

    fn insert(&mut self, metric: MetricId, descriptor: MetricDescriptor, now: Instant) {
        // Evict the least recently used entry. Linear in the capacity, which
        // is a small configured bound.
        if !self.entries.contains_key(&metric)
            && self.entries.len() >= self.capacity
            && let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(id, _)| id.clone())
        {
            self.entries.remove(&victim);
        }
        self.tick += 1;
        self.entries.insert(
            metric,
            CacheEntry {
                descriptor,
                fetched_at: now,
                last_used: self.tick,
            },
        );
    }
}

impl CachedMetricRegistry {
    /// Bind to `registry` with a cache of `capacity` entries refreshed after
    /// `ttl` and served stale for at most `stale_grace` beyond that.
    #[must_use]
    pub fn new(
        registry: Arc<dyn TypesRegistryClient>,
        capacity: usize,
        ttl: Duration,
        stale_grace: Duration,
    ) -> Self {
        Self {
            registry,
            deadline: DEFAULT_REGISTRY_DEADLINE,
            ttl,
            stale_grace,
            cache: Mutex::new(MetricCache {
                capacity: capacity.max(1),
                tick: 0,
                entries: HashMap::new(),
            }),
        }
    }

    /// Override the budget one registry call may take.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MetricCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A cached descriptor younger than `ttl`.
    fn fresh_hit(&self, metric: &MetricId, now: Instant) -> Option<MetricDescriptor> {
        let mut cache = self.lock();
        let entry = cache.touch(metric)?;
        (now.duration_since(entry.fetched_at) <= self.ttl).then_some(entry.descriptor)
    }

    /// A cached descriptor within the stale grace, served because the
    /// registry did not answer; otherwise the failure itself.
    fn stale_or(
        &self,
        metric: &MetricId,
        now: Instant,
        failure: DomainError,
    ) -> Result<Option<Classified>, DomainError> {
        let stale = {
            let mut cache = self.lock();
            cache.touch(metric).and_then(|entry| {
                (now.duration_since(entry.fetched_at) <= self.ttl + self.stale_grace)
                    .then_some(entry.descriptor)
            })
        };
        match stale {
            Some(descriptor) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    metric = %metric,
                    error = %failure,
                    "types registry did not answer; serving the cached metric classification"
                );
                Ok(Some(Classified {
                    descriptor,
                    freshness: Freshness::Stale,
                }))
            }
            None => Err(failure),
        }
    }

    fn evict(&self, metric: &MetricId) {
        self.lock().entries.remove(metric);
    }
}

/// The classification an instance document declares, if usable.
fn parse_classification(object: &Value) -> Option<MetricDescriptor> {
    let kind = match object.get(contract::KIND)?.as_str()? {
        contract::KIND_COUNTER => MetricKind::Counter,
        contract::KIND_GAUGE => MetricKind::Gauge,
        _ => return None,
    };
    let mode = match object.get(contract::ENFORCEMENT)?.as_str()? {
        contract::ENFORCEMENT_QUOTA_GATED => MetricMode::QuotaGated,
        contract::ENFORCEMENT_DIRECT => MetricMode::Direct,
        _ => return None,
    };
    Some(MetricDescriptor { kind, mode })
}

#[async_trait]
impl MetricRegistry for CachedMetricRegistry {
    async fn describe(&self, metric: &MetricId) -> Result<Option<Classified>, DomainError> {
        let now = Instant::now();
        if let Some(descriptor) = self.fresh_hit(metric, now) {
            return Ok(Some(Classified {
                descriptor,
                freshness: Freshness::Fresh,
            }));
        }
        match tokio::time::timeout(self.deadline, self.registry.get_instance(metric.as_str())).await
        {
            Ok(Ok(instance)) => {
                if instance.type_id().as_ref() != METRIC_BASE_TYPE {
                    self.evict(metric);
                    return Ok(None);
                }
                let Some(descriptor) = parse_classification(&instance.object) else {
                    self.evict(metric);
                    return Err(classification_invalid(metric));
                };
                self.lock().insert(metric.clone(), descriptor, now);
                Ok(Some(Classified {
                    descriptor,
                    freshness: Freshness::Fresh,
                }))
            }
            Ok(Err(CanonicalError::NotFound { .. })) => {
                self.evict(metric);
                Ok(None)
            }
            Ok(Err(err)) => self.stale_or(metric, now, unavailable(&err)),
            Err(_elapsed) => self.stale_or(metric, now, timed_out(self.deadline)),
        }
    }
}

/// A registered metric without a usable classification. Logged with the field
/// names so the owner knows what to publish.
fn classification_invalid(metric: &MetricId) -> DomainError {
    tracing::warn!(
        target: LOG_TARGET,
        metric = %metric,
        kind_field = contract::KIND,
        enforcement_field = contract::ENFORCEMENT,
        "metric instance carries no usable classification"
    );
    DomainError::MetricClassificationInvalid {
        metric: metric.as_str().to_owned(),
    }
}

/// A transport or registry failure. The cause is kept as text: the domain
/// error is `Clone + Eq` and crosses the layer boundary as a value.
fn unavailable(err: &CanonicalError) -> DomainError {
    DomainError::TypesRegistryUnavailable(format!("types registry `get_instance` failed: {err}"))
}

fn timed_out(deadline: Duration) -> DomainError {
    DomainError::TypesRegistryUnavailable(format!(
        "types registry `get_instance` exceeded {deadline:?}"
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "metric_registry_tests.rs"]
mod metric_registry_tests;
