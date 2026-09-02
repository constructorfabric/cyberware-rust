use std::time::Duration;

use super::{MetricsConfig, QuotaEnforcementConfig};

#[test]
fn defaults_select_the_platform_vendor_and_a_five_second_probe() {
    let cfg = QuotaEnforcementConfig::default();
    assert_eq!(cfg.storage_vendor, "constructorfabric");
    assert_eq!(cfg.coordination_vendor, "constructorfabric");
    assert_eq!(cfg.probe_lock_ttl(), Duration::from_secs(5));
    assert_eq!(cfg.metrics.instrument_name("denial_total"), "denial_total");
    cfg.validate().expect("defaults are valid");
}

#[test]
fn blank_vendors_and_a_zero_probe_ttl_are_rejected_with_the_field_name() {
    let cases: Vec<(QuotaEnforcementConfig, &str)> = vec![
        (
            QuotaEnforcementConfig {
                storage_vendor: " ".to_owned(),
                ..QuotaEnforcementConfig::default()
            },
            "storage_vendor",
        ),
        (
            QuotaEnforcementConfig {
                coordination_vendor: String::new(),
                ..QuotaEnforcementConfig::default()
            },
            "coordination_vendor",
        ),
        (
            QuotaEnforcementConfig {
                probe_lock_ttl_secs: 0,
                ..QuotaEnforcementConfig::default()
            },
            "probe_lock_ttl_secs",
        ),
    ];
    for (cfg, field) in cases {
        let err = cfg.validate().expect_err("invalid config rejected");
        assert!(err.to_string().contains(field), "{field}: {err}");
    }
}

#[test]
fn metrics_prefix_is_validated_and_applied() {
    let empty = MetricsConfig::default();
    empty.validate().expect("empty prefix is valid");
    let spaced = MetricsConfig {
        prefix: "  qe ".to_owned(),
    };
    spaced
        .validate()
        .expect("surrounding whitespace is trimmed");
    assert_eq!(spaced.instrument_name("denial_total"), "qe_denial_total");
    for bad in ["1qe", "qe-x", "qe x", "qe.x"] {
        let cfg = MetricsConfig {
            prefix: bad.to_owned(),
        };
        assert!(cfg.validate().is_err(), "prefix {bad:?} must be rejected");
    }
}

#[test]
fn unknown_keys_are_rejected_and_partial_configs_use_defaults() {
    let cfg: QuotaEnforcementConfig =
        serde_json::from_str(r#"{ "storage_vendor": "acme" }"#).expect("partial config");
    assert_eq!(cfg.storage_vendor, "acme");
    assert_eq!(cfg.coordination_vendor, "constructorfabric");
    assert!(serde_json::from_str::<QuotaEnforcementConfig>(r#"{ "vendor": "acme" }"#).is_err());
}
