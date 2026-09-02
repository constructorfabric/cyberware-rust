//! Configuration for `[quota-enforcement-coordination-plugin]`.

use serde::Deserialize;

/// Plugin configuration. Read once at `Gear::init`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoordinationPluginConfig {
    /// Vendor the gear selects this backend by. Must match
    /// `gears.quota-enforcement.config.coordination_vendor`.
    pub vendor: String,
    /// Selection priority among plugins of the same vendor. Lower wins.
    pub priority: i16,
}

impl Default for CoordinationPluginConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
        }
    }
}

impl CoordinationPluginConfig {
    /// Reject a configuration that can never be selected.
    ///
    /// # Errors
    ///
    /// Returns an error when `vendor` is empty or whitespace only.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.vendor.trim().is_empty() {
            anyhow::bail!(
                "[quota-enforcement-coordination-plugin].vendor must not be empty or whitespace-only"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
