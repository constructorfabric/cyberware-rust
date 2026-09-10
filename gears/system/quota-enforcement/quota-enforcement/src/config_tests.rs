use std::time::Duration;

use super::{
    CatalogSection, ElectionTimingConfig, GaugesSection, MetricsConfig, QuotaEnforcementConfig,
    QuotasSection,
};

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

#[test]
fn the_catalog_section_accepts_type_ids_and_rejects_instances_and_duplicates() {
    let section = CatalogSection {
        subject_projections: vec![
            "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~".to_owned(),
            "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~".to_owned(),
        ],
        resource_projections: vec![
            "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~".to_owned(),
        ],
    };
    section.validate().expect("valid");
    let domain = section.to_domain().expect("lowers");
    assert_eq!(domain.subject_projections.len(), 2);
    assert_eq!(domain.resource_projections.len(), 1);
    assert!(
        CatalogSection::default().validate().is_ok(),
        "empty is a valid catalogue"
    );

    let instance = CatalogSection {
        subject_projections: vec!["gts.cf.core.qe.scope.v1~cf.core.qe.user.v1".to_owned()],
        ..CatalogSection::default()
    };
    let err = instance
        .validate()
        .expect_err("an instance id is not a type id");
    assert!(err.to_string().contains("subject_projections[0]"), "{err}");

    let duplicate = CatalogSection {
        resource_projections: vec![
            "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~".to_owned(),
            "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~".to_owned(),
        ],
        ..CatalogSection::default()
    };
    let err = duplicate.validate().expect_err("duplicates rejected");
    assert!(err.to_string().contains("twice"), "{err}");

    let cfg: QuotaEnforcementConfig = serde_json::from_str(
        r#"{ "catalog": { "subject_projections": ["gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~"] } }"#,
    )
    .expect("partial catalog section");
    assert_eq!(cfg.catalog.subject_projections.len(), 1);
    assert!(cfg.catalog.resource_projections.is_empty());
    assert!(
        serde_json::from_str::<QuotaEnforcementConfig>(
            r#"{ "catalog": { "request_contracts": [] } }"#
        )
        .is_err(),
        "request contracts are discovered, never configured"
    );
}

#[test]
fn the_quotas_section_defaults_to_the_prd_bounds_and_rejects_each_zero_or_oversize() {
    let section = QuotasSection::default();
    section.validate().expect("defaults are valid");
    let limits = section.to_limits();
    assert_eq!(limits.metadata_max_bytes, 4096);
    assert_eq!(limits.list_max_limit, 500);
    assert_eq!(limits.list_max_ids, 100);
    assert_eq!(section.metric_cache_ttl(), Duration::from_mins(1));
    assert_eq!(section.metric_cache_stale_grace(), Duration::from_mins(5));

    let cases: Vec<(QuotasSection, &str)> = vec![
        (
            QuotasSection {
                metadata_max_bytes: 1,
                ..QuotasSection::default()
            },
            "metadata_max_bytes",
        ),
        (
            QuotasSection {
                metadata_max_bytes: QuotasSection::MAX_METADATA_BYTES + 1,
                ..QuotasSection::default()
            },
            "metadata_max_bytes",
        ),
        (
            QuotasSection {
                metric_cache_entries: 0,
                ..QuotasSection::default()
            },
            "metric_cache_entries",
        ),
        (
            QuotasSection {
                metric_cache_ttl_secs: 0,
                ..QuotasSection::default()
            },
            "metric_cache_ttl_secs",
        ),
        (
            QuotasSection {
                list_max_limit: 0,
                ..QuotasSection::default()
            },
            "list_max_limit",
        ),
        (
            QuotasSection {
                list_max_ids: 0,
                ..QuotasSection::default()
            },
            "list_max_ids",
        ),
    ];
    for (section, field) in cases {
        let err = section.validate().expect_err("rejected");
        assert!(err.to_string().contains(field), "{field}: {err}");
    }
    assert!(
        QuotasSection {
            metric_cache_stale_grace_secs: 0,
            ..QuotasSection::default()
        }
        .validate()
        .is_ok(),
        "a zero grace serves nothing stale and is allowed"
    );
}

#[test]
fn the_gauges_section_keeps_the_sample_fresher_than_the_refresh_interval() {
    let section = GaugesSection::default();
    section.validate().expect("defaults are valid");
    let timing = section.to_timing();
    assert_eq!(timing.refresh, Duration::from_secs(30));
    assert_eq!(timing.refresh_deadline, Duration::from_secs(10));
    assert_eq!(timing.stale_after, Duration::from_mins(3));

    let cases: Vec<(GaugesSection, &str)> = vec![
        (
            GaugesSection {
                refresh_secs: 0,
                ..GaugesSection::default()
            },
            "refresh_secs",
        ),
        (
            GaugesSection {
                refresh_deadline_secs: 0,
                ..GaugesSection::default()
            },
            "refresh_deadline_secs",
        ),
        (
            GaugesSection {
                refresh_deadline_secs: 31,
                ..GaugesSection::default()
            },
            "refresh_deadline_secs",
        ),
        (
            GaugesSection {
                stale_after_secs: 30,
                ..GaugesSection::default()
            },
            "stale_after_secs",
        ),
    ];
    for (section, field) in cases {
        let err = section.validate().expect_err("rejected");
        assert!(err.to_string().contains(field), "{field}: {err}");
    }

    let whole: QuotaEnforcementConfig = serde_json::from_str(
        r#"{ "quotas": { "metadata_max_bytes": 8192 }, "gauges": { "refresh_secs": 5, "refresh_deadline_secs": 2, "stale_after_secs": 20 } }"#,
    )
    .expect("partial sections");
    whole.validate().expect("valid");
    assert_eq!(whole.quotas.metadata_max_bytes, 8192);
    assert_eq!(
        whole.quotas.list_max_limit, 500,
        "untouched fields keep defaults"
    );
    assert_eq!(whole.gauges.refresh_secs, 5);
    assert!(
        serde_json::from_str::<QuotaEnforcementConfig>(r#"{ "quotas": { "max_bytes": 1 } }"#)
            .is_err(),
        "unknown keys are rejected"
    );
}
