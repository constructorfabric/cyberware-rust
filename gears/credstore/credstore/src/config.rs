//! Validated credential-store configuration.
//!
//! Controls backend plugin selection, hierarchy-cache lifetime, and the
//! periodic maintenance job's (`credstore gc`) batch size and pending-age
//! threshold (ADR-0006). There is no in-gear resident timer any more: the
//! `reaper` config block (`tick_secs`, `provisioning_timeout_secs`,
//! `deprovisioning_timeout_secs`) is withdrawn outright, not renamed —
//! `deny_unknown_fields` makes an old `reaper:` key a hard config-validation
//! failure rather than a silently ignored no-op.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredStoreConfig {
    pub vendor: String,
    pub hierarchy: HierarchyCfg,
    pub gc: GcCfg,
    pub list: ListCfg,
}

impl Default for CredStoreConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            hierarchy: HierarchyCfg::default(),
            gc: GcCfg::default(),
            list: ListCfg::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HierarchyCfg {
    pub ancestor_cache_ttl_secs: u64,
}

impl Default for HierarchyCfg {
    fn default() -> Self {
        Self {
            ancestor_cache_ttl_secs: 300,
        }
    }
}

/// Settings for the periodic maintenance job (`credstore gc`), read by that
/// admin entrypoint (Phase 3), not by the gear's own `serve` lifecycle —
/// nothing in the gear runs on a timer (ADR-0006 D7).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GcCfg {
    /// Age after which a `pending` gc entry with no row still referencing its
    /// `value_id` is reclaimed (backend entry deleted, gc row dropped) — the
    /// backstop for a write that crashed between its intent insert and its
    /// row CAS.
    pub pending_max_age_secs: u64,
    /// Bounded batch size for both the expired-row sweep and the gc
    /// drain/pending-reclaim passes.
    pub batch_size: u64,
}

impl Default for GcCfg {
    fn default() -> Self {
        Self {
            pending_max_age_secs: 3600,
            batch_size: 256,
        }
    }
}

/// Settings for the collection read (`GET /credstore/v1/credentials`,
/// ADR-0005/ADR-0004): the metadata-mode page-size cap and the value-mode
/// (`$select` containing `secret`) match-set cap. Neither is specified by a
/// config key in the design docs; both are introduced here as the
/// implementation's own knobs, named after the ADRs' proposed defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListCfg {
    /// Maximum `limit`/`$top` for a metadata-mode page; a caller-supplied
    /// value above this is rejected (400 `INVALID_LIMIT`) rather than
    /// silently clamped.
    pub max_limit: u64,
    /// Cap on how many references a value-mode (`$select=…,secret`) request
    /// may match. Enforced by fetching `cap + 1` candidate references and
    /// failing closed with `400 TOO_MANY_MATCHES` if the `(cap + 1)`th
    /// appears — never by a `COUNT` query.
    pub value_mode_cap: u64,
}

impl Default for ListCfg {
    fn default() -> Self {
        Self {
            max_limit: 200,
            value_mode_cap: 25,
        }
    }
}

impl CredStoreConfig {
    /// # Errors
    /// Returns `Err` with a description if any field is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty() {
            return Err("vendor must be non-empty".to_owned());
        }
        if self.gc.pending_max_age_secs == 0 {
            return Err("gc.pending_max_age_secs must be > 0".to_owned());
        }
        if self.gc.batch_size == 0 {
            return Err("gc.batch_size must be > 0".to_owned());
        }
        if self.hierarchy.ancestor_cache_ttl_secs == 0 {
            return Err("hierarchy.ancestor_cache_ttl_secs must be > 0".to_owned());
        }
        if self.list.max_limit == 0 {
            return Err("list.max_limit must be > 0".to_owned());
        }
        if self.list.value_mode_cap == 0 {
            return Err("list.value_mode_cap must be > 0".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::CredStoreConfig;

    #[test]
    fn default_config_is_valid() {
        let cfg = CredStoreConfig::default();
        // Must match the backend plugin's default vendor (static-credstore-plugin
        // defaults to "constructorfabric"); otherwise a default-config deployment
        // resolves no backend plugin and 503s on every secret op.
        assert_eq!(cfg.vendor, "constructorfabric");
        assert_eq!(cfg.hierarchy.ancestor_cache_ttl_secs, 300);
        assert_eq!(cfg.gc.pending_max_age_secs, 3600);
        assert_eq!(cfg.gc.batch_size, 256);
        assert_eq!(cfg.list.max_limit, 200);
        assert_eq!(cfg.list.value_mode_cap, 25);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn deserializes_partial_config_with_defaults() {
        let cfg: CredStoreConfig =
            serde_json::from_str(r#"{"vendor":"acme","gc":{"batch_size":5}}"#)
                .expect("deserialize");
        assert_eq!(cfg.vendor, "acme");
        assert_eq!(cfg.gc.batch_size, 5);
        // Unspecified fields fall back to defaults.
        assert_eq!(cfg.gc.pending_max_age_secs, 3600);
        assert_eq!(cfg.hierarchy.ancestor_cache_ttl_secs, 300);
    }

    #[test]
    fn deserializes_partial_list_config_with_defaults() {
        let cfg: CredStoreConfig =
            serde_json::from_str(r#"{"list":{"max_limit":50}}"#).expect("deserialize");
        assert_eq!(cfg.list.max_limit, 50);
        // Unspecified fields fall back to defaults.
        assert_eq!(cfg.list.value_mode_cap, 25);
    }

    #[test]
    fn validate_rejects_each_invalid_field() {
        use super::{GcCfg, HierarchyCfg, ListCfg};

        let empty_vendor = CredStoreConfig {
            vendor: String::new(),
            ..Default::default()
        };
        assert!(empty_vendor.validate().is_err());

        let zero_pending_age = CredStoreConfig {
            gc: GcCfg {
                pending_max_age_secs: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(zero_pending_age.validate().is_err());

        let zero_batch = CredStoreConfig {
            gc: GcCfg {
                batch_size: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(zero_batch.validate().is_err());

        let zero_ttl = CredStoreConfig {
            hierarchy: HierarchyCfg {
                ancestor_cache_ttl_secs: 0,
            },
            ..Default::default()
        };
        assert!(zero_ttl.validate().is_err());

        let zero_max_limit = CredStoreConfig {
            list: ListCfg {
                max_limit: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(zero_max_limit.validate().is_err());

        let zero_value_mode_cap = CredStoreConfig {
            list: ListCfg {
                value_mode_cap: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(zero_value_mode_cap.validate().is_err());
    }

    #[test]
    fn rejects_the_withdrawn_reaper_config_block() {
        // ADR-0006 withdraws the reaper outright, not a rename: an old
        // `reaper:` key must fail config validation rather than being
        // silently ignored (`deny_unknown_fields`).
        let err = serde_json::from_str::<CredStoreConfig>(
            r#"{"reaper":{"tick_secs":60,"provisioning_timeout_secs":300,"deprovisioning_timeout_secs":300}}"#,
        )
        .expect_err("reaper key must be rejected");
        assert!(err.to_string().contains("reaper"));
    }
}
