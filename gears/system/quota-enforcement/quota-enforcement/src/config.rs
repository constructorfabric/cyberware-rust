//! Configuration for `[quota-enforcement]`. Read once at `Gear::init`.

use std::time::Duration;

use serde::Deserialize;

/// Gear configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaEnforcementConfig {
    /// Vendor of the active storage plugin. Exactly one plugin is active per
    /// deployment (DESIGN, "Single storage plugin per deployment").
    pub storage_vendor: String,
    /// Vendor of the active coordination plugin.
    pub coordination_vendor: String,
    /// TTL of the bootstrap `try_lock` + `release` probe, in seconds.
    pub probe_lock_ttl_secs: u64,
    /// Operational metrics.
    pub metrics: MetricsConfig,
}

impl Default for QuotaEnforcementConfig {
    fn default() -> Self {
        Self {
            storage_vendor: "constructorfabric".to_owned(),
            coordination_vendor: "constructorfabric".to_owned(),
            probe_lock_ttl_secs: 5,
            metrics: MetricsConfig::default(),
        }
    }
}

impl QuotaEnforcementConfig {
    /// Reject a configuration the gear cannot start with.
    ///
    /// # Errors
    ///
    /// Returns an error when a vendor is blank, the probe TTL is zero, or the
    /// metrics prefix is not a valid instrument-name prefix.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.storage_vendor.trim().is_empty() {
            anyhow::bail!(
                "[quota-enforcement].storage_vendor must not be empty or whitespace-only"
            );
        }
        if self.coordination_vendor.trim().is_empty() {
            anyhow::bail!(
                "[quota-enforcement].coordination_vendor must not be empty or whitespace-only"
            );
        }
        if self.probe_lock_ttl_secs == 0 {
            anyhow::bail!("[quota-enforcement].probe_lock_ttl_secs must be at least 1");
        }
        self.metrics.validate()
    }

    /// TTL of the bootstrap coordination probe.
    #[must_use]
    pub const fn probe_lock_ttl(&self) -> Duration {
        Duration::from_secs(self.probe_lock_ttl_secs)
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
