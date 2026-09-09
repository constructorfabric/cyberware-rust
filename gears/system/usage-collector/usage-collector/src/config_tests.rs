//! Unit tests for the `[usage_collector]` configuration surface.
//!
//! Only the vendor binding and serde posture (`#[serde(default,
//! deny_unknown_fields)]`) are exercised here; the metric catalog is plugin-
//! owned under ADR-0012, so there is no host-side declared-catalog surface
//! left to test.

use super::*;

#[test]
fn serde_default_applies_default_vendor() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(
        cfg.vendor, "constructorfabric",
        "serde(default) must use Default impl"
    );
}

#[test]
fn vendor_can_be_overridden_via_serde() {
    let json = r#"{"vendor": "acme"}"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.vendor, "acme");
}

#[test]
fn rejects_unknown_fields() {
    let json = r#"{"vendor": "x", "unexpected": true}"#;
    assert!(serde_json::from_str::<UsageCollectorConfig>(json).is_err());
}

#[test]
fn validate_accepts_default_vendor() {
    assert!(UsageCollectorConfig::default().validate().is_ok());
}

#[test]
fn validate_rejects_empty_vendor() {
    let cfg = UsageCollectorConfig {
        vendor: String::new(),
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_whitespace_only_vendor() {
    let cfg = UsageCollectorConfig {
        vendor: "   \t ".to_owned(),
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

// ── MetricsConfig (operational-metrics prefix substitution) ──
//
// DESIGN §3.11.5: the leading `uc_` namespace segment is "substitutable at
// adapter construction". The prefix defaults to `uc` (NOT the snake_cased
// gear name) so the rendered Prometheus names match the inventory literally.

#[test]
fn metrics_config_defaults_to_uc_prefix() {
    assert_eq!(MetricsConfig::default().effective_prefix(), "uc");
}

#[test]
fn metrics_config_effective_prefix_uses_override() {
    let cfg = MetricsConfig {
        prefix: "acme".to_owned(),
    };
    assert_eq!(cfg.effective_prefix(), "acme");
}

#[test]
fn metrics_config_effective_prefix_falls_back_on_blank() {
    let cfg = MetricsConfig {
        prefix: "   ".to_owned(),
    };
    assert_eq!(
        cfg.effective_prefix(),
        "uc",
        "a blank/whitespace prefix must fall back to the `uc` default"
    );
}

#[test]
fn serde_default_applies_default_metrics_prefix() {
    let cfg: UsageCollectorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.metrics.effective_prefix(), "uc");
}

#[test]
fn metrics_block_can_be_overridden_via_serde() {
    let json = r#"{"vendor": "acme", "metrics": {"prefix": "am"}}"#;
    let cfg: UsageCollectorConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.metrics.effective_prefix(), "am");
}

#[test]
fn metrics_block_rejects_unknown_fields() {
    let json = r#"{"metrics": {"prefix": "uc", "bogus": 1}}"#;
    assert!(serde_json::from_str::<UsageCollectorConfig>(json).is_err());
}

// ── Metrics-prefix validation ──
//
// The effective prefix is interpolated into every `uc_*` instrument name via
// `{prefix}_...`, so it must be a valid Prometheus/OTel name prefix
// (`[A-Za-z_][A-Za-z0-9_]*`). `validate()` rejects a malformed prefix at
// `Gear::init` instead of letting it surface as broken/dropped telemetry.

/// A valid config whose only variable is the metrics prefix.
fn cfg_with_prefix(prefix: &str) -> UsageCollectorConfig {
    UsageCollectorConfig {
        metrics: MetricsConfig {
            prefix: prefix.to_owned(),
        },
        ..Default::default()
    }
}

#[test]
fn validate_accepts_default_metrics_prefix() {
    // Blank prefix → effective "uc", a valid instrument namespace.
    assert!(cfg_with_prefix("").validate().is_ok());
}

#[test]
fn validate_accepts_valid_custom_prefix() {
    assert!(cfg_with_prefix("acme_uc").validate().is_ok());
    assert!(cfg_with_prefix("_private").validate().is_ok());
    assert!(cfg_with_prefix("uc2").validate().is_ok());
}

#[test]
fn validate_accepts_prefix_with_surrounding_whitespace() {
    // effective_prefix() trims, so surrounding whitespace is tolerated.
    let cfg = cfg_with_prefix("  uc  ");
    assert!(cfg.validate().is_ok());
    assert_eq!(cfg.metrics.effective_prefix(), "uc");
}

#[test]
fn validate_rejects_prefix_with_interior_space() {
    assert!(cfg_with_prefix("my prefix").validate().is_err());
}

#[test]
fn validate_rejects_prefix_with_dot() {
    assert!(cfg_with_prefix("uc.v2").validate().is_err());
}

#[test]
fn validate_rejects_prefix_with_slash() {
    assert!(cfg_with_prefix("uc/x").validate().is_err());
}

#[test]
fn validate_rejects_prefix_starting_with_digit() {
    assert!(cfg_with_prefix("2uc").validate().is_err());
}

#[test]
fn validate_rejects_prefix_with_hyphen() {
    // The gear name is `usage-collector`, but instrument names are `uc_*`;
    // a hyphen is not a legal Prometheus/OTel name character.
    assert!(cfg_with_prefix("usage-collector").validate().is_err());
}
