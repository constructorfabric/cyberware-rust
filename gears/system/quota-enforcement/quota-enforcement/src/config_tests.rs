use std::time::Duration;

use super::{ElectionTimingConfig, MetricsConfig, QuotaEnforcementConfig};

#[test]
fn defaults_select_the_platform_vendor_and_the_cluster_election_defaults() {
    let cfg = QuotaEnforcementConfig::default();
    assert_eq!(cfg.storage_vendor, "constructorfabric");
    assert_eq!(cfg.election.ttl(), Duration::from_secs(30));
    assert_eq!(cfg.election.max_missed_renewals, 2);
    assert_eq!(cfg.sweeper_stop_timeout(), Duration::from_secs(10));
    assert_eq!(cfg.metrics.instrument_name("denial_total"), "denial_total");
    cfg.validate().expect("defaults are valid");
}

#[test]
fn a_blank_vendor_and_zero_timings_are_rejected_with_the_field_name() {
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
                election: ElectionTimingConfig {
                    ttl_secs: 0,
                    ..ElectionTimingConfig::default()
                },
                ..QuotaEnforcementConfig::default()
            },
            "ttl_secs",
        ),
        (
            QuotaEnforcementConfig {
                election: ElectionTimingConfig {
                    max_missed_renewals: 0,
                    ..ElectionTimingConfig::default()
                },
                ..QuotaEnforcementConfig::default()
            },
            "max_missed_renewals",
        ),
        (
            QuotaEnforcementConfig {
                sweeper_stop_timeout_secs: 0,
                ..QuotaEnforcementConfig::default()
            },
            "sweeper_stop_timeout_secs",
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
    assert_eq!(cfg.election.ttl_secs, 30);
    let timing: QuotaEnforcementConfig =
        serde_json::from_str(r#"{ "election": { "ttl_secs": 5 } }"#).expect("partial election");
    assert_eq!(timing.election.ttl(), Duration::from_secs(5));
    assert_eq!(timing.election.max_missed_renewals, 2);
    assert!(serde_json::from_str::<QuotaEnforcementConfig>(r#"{ "vendor": "acme" }"#).is_err());
    assert!(
        serde_json::from_str::<QuotaEnforcementConfig>(r#"{ "coordination_vendor": "acme" }"#)
            .is_err(),
        "the retired coordination plugin selector is rejected"
    );
}
