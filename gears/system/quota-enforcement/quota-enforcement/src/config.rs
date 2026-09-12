//! Configuration for `[quota-enforcement]`. Read once at `Gear::init`.

use std::time::Duration;

use gts::GtsTypeId;
use serde::Deserialize;

use crate::domain::catalog::CatalogConfig;
use crate::domain::quotas::{GaugeTiming, QuotaLimits};

/// Gear configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaEnforcementConfig {
    /// Vendor of the active storage plugin. Exactly one plugin is active per
    /// deployment (DESIGN, "Single storage plugin per deployment").
    pub storage_vendor: String,
    /// Timing of the sweeper elections on the platform `cluster` gear.
    pub election: ElectionTimingConfig,
    /// Budget, in seconds, for a sweep body to stop after leadership loss or
    /// shutdown. A body that overruns the budget is aborted.
    pub sweeper_stop_timeout_secs: u64,
    /// Operational metrics.
    pub metrics: MetricsConfig,
    /// The owner projections configured for evaluation.
    pub catalog: CatalogSection,
    /// Bounds of the Quota lifecycle surface.
    pub quotas: QuotasSection,
    /// Timing of the lifecycle-gauge refresh.
    pub gauges: GaugesSection,
}

impl Default for QuotaEnforcementConfig {
    fn default() -> Self {
        Self {
            storage_vendor: "constructorfabric".to_owned(),
            election: ElectionTimingConfig::default(),
            sweeper_stop_timeout_secs: 10,
            metrics: MetricsConfig::default(),
            catalog: CatalogSection::default(),
            quotas: QuotasSection::default(),
            gauges: GaugesSection::default(),
        }
    }
}

impl QuotaEnforcementConfig {
    /// Reject a configuration the gear cannot start with.
    ///
    /// # Errors
    ///
    /// Returns an error when the vendor is blank, a timing value is zero, or
    /// the metrics prefix is not a valid instrument-name prefix.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.storage_vendor.trim().is_empty() {
            anyhow::bail!(
                "[quota-enforcement].storage_vendor must not be empty or whitespace-only"
            );
        }
        self.election.validate()?;
        if self.sweeper_stop_timeout_secs == 0 {
            anyhow::bail!("[quota-enforcement].sweeper_stop_timeout_secs must be at least 1");
        }
        self.metrics.validate()?;
        self.catalog.validate()?;
        self.quotas.validate()?;
        self.gauges.validate()
    }

    /// Budget for a sweep body to stop after leadership loss or shutdown.
    #[must_use]
    pub const fn sweeper_stop_timeout(&self) -> Duration {
        Duration::from_secs(self.sweeper_stop_timeout_secs)
    }
}

/// Timing of one sweeper election (`[quota-enforcement.election]`).
///
/// The defaults are the cluster gear's defaults. A shorter TTL gives a faster
/// takeover after a crash at the cost of more renewal traffic; a larger
/// missed-renewal budget tolerates more backend jitter before leadership counts
/// as lost.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ElectionTimingConfig {
    /// Time-to-live of a leadership claim, in seconds. A crashed leader is
    /// replaced within this window plus observation lag.
    pub ttl_secs: u64,
    /// Consecutive renewal failures tolerated before leadership counts as
    /// lost. The cluster gear renews every `ttl / (max_missed_renewals + 1)`.
    pub max_missed_renewals: u8,
}

impl Default for ElectionTimingConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 30,
            max_missed_renewals: 2,
        }
    }
}

impl ElectionTimingConfig {
    /// Reject timing values the cluster gear cannot run an election with.
    ///
    /// # Errors
    ///
    /// Returns an error when the TTL or the missed-renewal budget is zero.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.ttl_secs == 0 {
            anyhow::bail!("[quota-enforcement.election].ttl_secs must be at least 1");
        }
        if self.max_missed_renewals == 0 {
            anyhow::bail!("[quota-enforcement.election].max_missed_renewals must be at least 1");
        }
        Ok(())
    }

    /// Time-to-live of a leadership claim.
    #[must_use]
    pub const fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs)
    }
}

/// Operational-metrics configuration for `[quota-enforcement.metrics]`.
///
/// The PRD section 5.16 catalogue names instruments without a namespace
/// (`denial_total`, ...). The prefix is empty by default so the rendered
/// names match the catalogue verbatim. Operators may set one.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Optional instrument-name prefix.
    pub prefix: String,
}

impl MetricsConfig {
    /// Full instrument name for a catalogue name.
    #[must_use]
    pub fn instrument_name(&self, catalogue_name: &str) -> String {
        let prefix = self.prefix.trim();
        if prefix.is_empty() {
            catalogue_name.to_owned()
        } else {
            format!("{prefix}_{catalogue_name}")
        }
    }

    /// Reject a prefix that is not a valid instrument-name prefix
    /// (`[A-Za-z_][A-Za-z0-9_]*`). Empty is valid.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid prefix.
    pub fn validate(&self) -> anyhow::Result<()> {
        let prefix = self.prefix.trim();
        if prefix.is_empty() {
            return Ok(());
        }
        let mut chars = prefix.chars();
        let valid = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            anyhow::bail!(
                "[quota-enforcement.metrics].prefix must match [A-Za-z_][A-Za-z0-9_]* (got {:?})",
                self.prefix
            );
        }
        Ok(())
    }
}

/// The evaluation catalogue (`[quota-enforcement.catalog]`): the concrete owner
/// projections this deployment resolves at bootstrap (ADR-0007). Request and
/// constraint contracts are discovered from the registry, not configured.
/// Empty lists boot an empty catalogue.
// @cpt-begin:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-config
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogSection {
    /// GTS type ids of concrete subject projections derived from
    /// `gts.cf.core.qe.subj.v1~`.
    pub subject_projections: Vec<String>,
    /// GTS type ids of concrete resource projections derived from
    /// `gts.cf.core.qe.res.v1~`.
    pub resource_projections: Vec<String>,
}
// @cpt-end:cpt-cf-quota-enforcement-flow-owner-projection-publication:p1:inst-pub-config

impl CatalogSection {
    /// Reject ids that are not GTS type ids and duplicate entries.
    ///
    /// # Errors
    ///
    /// Returns an error naming the field and the offending entry.
    pub fn validate(&self) -> anyhow::Result<()> {
        Self::validate_list("subject_projections", &self.subject_projections)?;
        Self::validate_list("resource_projections", &self.resource_projections)
    }

    fn validate_list(field: &str, ids: &[String]) -> anyhow::Result<()> {
        for (index, id) in ids.iter().enumerate() {
            GtsTypeId::try_new(id).map_err(|e| {
                anyhow::anyhow!(
                    "[quota-enforcement.catalog].{field}[{index}] is not a GTS type id: {e}"
                )
            })?;
            if ids[..index].contains(id) {
                anyhow::bail!("[quota-enforcement.catalog].{field} lists {id} twice");
            }
        }
        Ok(())
    }

    /// The domain view of the section.
    ///
    /// # Errors
    ///
    /// Returns an error when an id does not parse; `validate` reports the same
    /// condition with the field name.
    pub fn to_domain(&self) -> anyhow::Result<CatalogConfig> {
        let parse = |ids: &[String]| -> anyhow::Result<Vec<GtsTypeId>> {
            ids.iter()
                .map(|id| GtsTypeId::try_new(id).map_err(|e| anyhow::anyhow!("{id}: {e}")))
                .collect()
        };
        Ok(CatalogConfig {
            subject_projections: parse(&self.subject_projections)?,
            resource_projections: parse(&self.resource_projections)?,
        })
    }
}

/// Bounds of the Quota lifecycle surface (`[quota-enforcement.quotas]`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QuotasSection {
    /// Largest canonical-JSON size of a Quota's `metadata` object, in bytes
    /// (PRD section 5.2, default 4 KiB).
    pub metadata_max_bytes: usize,
    /// Entries the metric classification cache holds before evicting the
    /// least recently used one.
    pub metric_cache_entries: usize,
    /// Age after which a cached classification is refreshed from the
    /// registry, in seconds.
    pub metric_cache_ttl_secs: u64,
    /// Additional age during which a cached classification may still be
    /// served to a write when the registry does not answer, in seconds.
    /// Beyond `ttl + grace` nothing is served; the write fails closed.
    pub metric_cache_stale_grace_secs: u64,
    /// Largest page a list request may ask for.
    pub list_max_limit: u32,
    /// Largest number of explicit ids one list request may name.
    pub list_max_ids: usize,
}

impl Default for QuotasSection {
    fn default() -> Self {
        Self {
            metadata_max_bytes: 4096,
            metric_cache_entries: 256,
            metric_cache_ttl_secs: 60,
            metric_cache_stale_grace_secs: 300,
            list_max_limit: 500,
            list_max_ids: 100,
        }
    }
}

impl QuotasSection {
    /// Smallest metadata size limit: an empty object serialized.
    pub const MIN_METADATA_BYTES: usize = 2;
    /// Largest metadata size limit.
    pub const MAX_METADATA_BYTES: usize = 1_048_576;

    /// Reject bounds the gear cannot serve with.
    ///
    /// # Errors
    ///
    /// Returns an error naming the field that is out of its range.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(Self::MIN_METADATA_BYTES..=Self::MAX_METADATA_BYTES).contains(&self.metadata_max_bytes)
        {
            anyhow::bail!(
                "[quota-enforcement.quotas].metadata_max_bytes must be within {}..={}",
                Self::MIN_METADATA_BYTES,
                Self::MAX_METADATA_BYTES
            );
        }
        if self.metric_cache_entries == 0 {
            anyhow::bail!("[quota-enforcement.quotas].metric_cache_entries must be at least 1");
        }
        if self.metric_cache_ttl_secs == 0 {
            anyhow::bail!("[quota-enforcement.quotas].metric_cache_ttl_secs must be at least 1");
        }
        if self.list_max_limit == 0 {
            anyhow::bail!("[quota-enforcement.quotas].list_max_limit must be at least 1");
        }
        if self.list_max_ids == 0 {
            anyhow::bail!("[quota-enforcement.quotas].list_max_ids must be at least 1");
        }
        Ok(())
    }

    /// The domain view of the request bounds.
    #[must_use]
    pub const fn to_limits(&self) -> QuotaLimits {
        QuotaLimits {
            metadata_max_bytes: self.metadata_max_bytes,
            list_max_limit: self.list_max_limit,
            list_max_ids: self.list_max_ids,
        }
    }

    /// Age after which a cached classification is refreshed.
    #[must_use]
    pub const fn metric_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.metric_cache_ttl_secs)
    }

    /// Additional age during which a stale classification may serve a write.
    #[must_use]
    pub const fn metric_cache_stale_grace(&self) -> Duration {
        Duration::from_secs(self.metric_cache_stale_grace_secs)
    }
}

/// Timing of the lifecycle-gauge refresh (`[quota-enforcement.gauges]`). The
/// elected replica reads the active-Quota counts every `refresh_secs`, bounds
/// each read by `refresh_deadline_secs`, and withdraws the published sample
/// once no refresh succeeded for `stale_after_secs`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "configuration keys carry their unit, as `sweeper_stop_timeout_secs` does"
)]
pub struct GaugesSection {
    /// Seconds between two refreshes.
    pub refresh_secs: u64,
    /// Seconds one refresh may take before it counts as failed.
    pub refresh_deadline_secs: u64,
    /// Seconds without a successful refresh before the sample is withdrawn.
    pub stale_after_secs: u64,
}

impl Default for GaugesSection {
    fn default() -> Self {
        Self {
            refresh_secs: 30,
            refresh_deadline_secs: 10,
            stale_after_secs: 180,
        }
    }
}

impl GaugesSection {
    /// Reject timings under which the refresh cannot keep a sample fresh.
    ///
    /// # Errors
    ///
    /// Returns an error when a value is zero, the deadline exceeds the
    /// interval, or the staleness bound does not exceed the interval.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.refresh_secs == 0 {
            anyhow::bail!("[quota-enforcement.gauges].refresh_secs must be at least 1");
        }
        if self.refresh_deadline_secs == 0 || self.refresh_deadline_secs > self.refresh_secs {
            anyhow::bail!(
                "[quota-enforcement.gauges].refresh_deadline_secs must be within 1..=refresh_secs"
            );
        }
        if self.stale_after_secs <= self.refresh_secs {
            anyhow::bail!(
                "[quota-enforcement.gauges].stale_after_secs must exceed refresh_secs, else the \
                 sample is withdrawn before the next refresh"
            );
        }
        Ok(())
    }

    /// The domain view of the timing.
    #[must_use]
    pub const fn to_timing(&self) -> GaugeTiming {
        GaugeTiming {
            refresh: Duration::from_secs(self.refresh_secs),
            refresh_deadline: Duration::from_secs(self.refresh_deadline_secs),
            stale_after: Duration::from_secs(self.stale_after_secs),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
