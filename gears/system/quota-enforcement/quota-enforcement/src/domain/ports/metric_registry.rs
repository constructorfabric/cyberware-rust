//! Output port for metric identity and classification (quota-lifecycle
//! feature, "Metric Validation"; PRD section 3.2).
//!
//! A Quota references a metric by its `types-registry` instance id. The port
//! answers two questions the write path and the gauge refresh ask: is the id a
//! registered instance of the metric base, and how does the registry classify
//! it. The classification contract is provisional: the metric base
//! `gts.cf.qe.metric.type.v1~` is a platform namespace (PRD section 13), and QE
//! documents the fields it reads (`kind`, `enforcement`) as a proposal. A
//! registered metric without a usable classification is an error, never a
//! default.
//!
//! The only implementation is the cached adapter in `infra::metric_registry`;
//! the port exists for the domain-layer dependency rule.

use async_trait::async_trait;
use quota_enforcement_sdk::{MetricId, MetricKind};
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;

/// Registry-reported enforcement mode of a metric (PRD section 3.2, "Gated vs
/// Non-Gated Metrics"). Closed.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricMode {
    /// Usage flows through Quota Enforcement before it is recorded.
    QuotaGated,
    /// Usage is recorded directly; Quotas on it are inert.
    Direct,
}

/// What the registry says about a metric.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricDescriptor {
    /// Counter or gauge.
    pub kind: MetricKind,
    /// Gated or direct.
    pub mode: MetricMode,
}

/// How current a classification answer is.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Read from the registry, or cached within its refresh age.
    Fresh,
    /// Served from the cache after the registry failed to answer, within the
    /// configured grace. Good enough to admit a write; never good enough to
    /// renew a gauge sample.
    Stale,
}

/// A classified metric with the freshness of the answer.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    /// The classification.
    pub descriptor: MetricDescriptor,
    /// How current it is.
    pub freshness: Freshness,
}

/// The registry as the Quota lifecycle needs it.
#[async_trait]
pub trait MetricRegistry: Send + Sync {
    /// Describe `metric`. `Ok(None)` when it is not a registered instance of
    /// the metric base.
    ///
    /// # Errors
    ///
    /// - [`DomainError::TypesRegistryUnavailable`] when the registry cannot
    ///   answer and no cached answer within the grace exists (fail closed).
    /// - [`DomainError::MetricClassificationInvalid`] when the instance is
    ///   registered but carries no usable `kind` and `enforcement`.
    async fn describe(&self, metric: &MetricId) -> Result<Option<Classified>, DomainError>;
}
