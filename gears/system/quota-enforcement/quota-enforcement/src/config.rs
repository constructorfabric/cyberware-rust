//! Configuration for `[quota-enforcement]`. Read once at `Gear::init`.

use std::time::Duration;

use gts::GtsTypeId;
use serde::Deserialize;

use crate::domain::catalog::CatalogConfig;

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
}

impl Default for QuotaEnforcementConfig {
    fn default() -> Self {
        Self {
            storage_vendor: "constructorfabric".to_owned(),
            election: ElectionTimingConfig::default(),
            sweeper_stop_timeout_secs: 10,
            metrics: MetricsConfig::default(),
            catalog: CatalogSection::default(),
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
        self.catalog.validate()
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
