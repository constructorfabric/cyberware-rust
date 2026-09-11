// Updated: 2026-09-10 by Constructor Tech — value-seeding withdrawn (ADR-0006).
//! Configuration for the static credential backend.
//!
//! Out-of-band value seeding (secrets keyed by tenant/reference/owner in this
//! plugin's own config) is withdrawn by ADR-0006: a value now enters the
//! store only through the gear's write protocol, which mints its own
//! `value_id` and never has a `reference` or `owner_id` to hand the plugin.
//! What remains here is purely GTS-instance registration input (vendor,
//! priority).
use serde::Deserialize;

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct StaticCredStorePluginConfig {
    /// Vendor name for GTS instance registration.
    pub vendor: String,

    /// Plugin priority (lower = higher priority).
    pub priority: i16,
}

impl Default for StaticCredStorePluginConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
