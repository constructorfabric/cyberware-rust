//! Tests for the gear's job-cadence configuration.

use super::*;

#[test]
fn default_jobs_config_validates() {
    assert!(JobsConfig::default().validate().is_ok());
}

#[test]
fn zero_tie_out_tick_rejected() {
    let cfg = JobsConfig {
        tie_out_tick_secs: 0,
        ..JobsConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_period_open_tick_rejected() {
    let cfg = JobsConfig {
        period_open_tick_secs: 0,
        ..JobsConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_aged_alarm_tick_rejected() {
    let cfg = JobsConfig {
        aged_alarm_tick_secs: 0,
        ..JobsConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_verify_tick_rejected() {
    let cfg = JobsConfig {
        verify_tick_secs: 0,
        ..JobsConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn default_tie_out_interval_is_one_day() {
    assert_eq!(JobsConfig::default().tie_out_interval().as_secs(), 86_400);
}

#[test]
fn default_aged_alarm_interval_is_one_hour() {
    assert_eq!(JobsConfig::default().aged_alarm_interval().as_secs(), 3_600);
}

#[test]
fn default_recognition_config_validates() {
    assert!(RecognitionConfig::default().validate().is_ok());
}

#[test]
fn default_max_segments_is_120() {
    assert_eq!(RecognitionConfig::default().max_segments_per_schedule, 120);
}

#[test]
fn zero_max_segments_rejected() {
    let cfg = RecognitionConfig {
        max_segments_per_schedule: 0,
        ..RecognitionConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_recognition_run_tick_rejected() {
    let cfg = RecognitionConfig {
        recognition_run_tick_secs: 0,
        ..RecognitionConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn default_recognition_run_interval_is_five_minutes() {
    assert_eq!(
        RecognitionConfig::default()
            .recognition_run_interval()
            .as_secs(),
        300
    );
}

#[test]
fn zero_queue_applier_tick_rejected() {
    let cfg = JobsConfig {
        queue_applier_tick_secs: 0,
        ..JobsConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn default_period_open_interval_is_one_day() {
    assert_eq!(
        JobsConfig::default().period_open_interval().as_secs(),
        86_400
    );
}

#[test]
fn default_queue_applier_interval_is_five_minutes() {
    assert_eq!(
        JobsConfig::default().queue_applier_interval().as_secs(),
        300
    );
}

#[test]
fn default_fx_config_validates() {
    assert!(FxConfig::default().validate().is_ok());
}

#[test]
fn zero_stale_g10_hours_rejected() {
    let cfg = FxConfig {
        stale_g10_hours: 0,
        ..FxConfig::default()
    };
    assert!(cfg.validate().is_err());
}

/// Finding 19: `stale_g10_hours` above the 7-day bound (`MAX_STALE_G10_HOURS`)
/// must be rejected at `validate` — `rate_source::is_stale` feeds it to
/// `chrono::Duration::hours`, which panics when the window overflows chrono's
/// millisecond representation. Reject loudly at `init` instead.
#[test]
fn over_max_stale_g10_hours_rejected() {
    let cfg = FxConfig {
        stale_g10_hours: MAX_STALE_G10_HOURS + 1,
        ..FxConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn over_seven_days_stale_default_rejected() {
    let cfg = FxConfig {
        stale_default_max_days: 8,
        ..FxConfig::default()
    };
    assert!(cfg.validate().is_err());
}

/// A sub-floor tick must be rejected: the `FxNoFetchAttempted` alert window is
/// `3 ×` the tick, and one attempt can stay in flight for up to the adapter's
/// 30 s per-source timeout ceiling — a sub-minute tick would let a single slow
/// attempt span the whole window and read as "no attempt occurred".
#[test]
fn sub_floor_rate_sync_tick_rejected() {
    let cfg = FxConfig {
        rate_sync_tick_secs: MIN_RATE_SYNC_TICK_SECS - 1,
        ..FxConfig::default()
    };
    assert!(cfg.validate().is_err());
}

/// The floor itself must validate — together with the rejection above this pins
/// the bound exactly, where either test alone would pass for an arbitrary one.
#[test]
fn rate_sync_tick_at_the_floor_validates() {
    let cfg = FxConfig {
        rate_sync_tick_secs: MIN_RATE_SYNC_TICK_SECS,
        ..FxConfig::default()
    };
    assert!(cfg.validate().is_ok());
}

#[test]
fn default_recon_config_validates() {
    assert!(ReconConfig::default().validate().is_ok());
}

#[test]
fn default_recon_config_values() {
    let cfg = ReconConfig::default();
    assert_eq!(cfg.recon_tick_secs, 300);
    assert_eq!(cfg.ar_tolerance_minor_per_k_lines, 1);
    assert!(
        !cfg.manifest_enforcement,
        "manifest enforcement default OFF"
    );
    assert!(
        !cfg.bill_run_enforcement,
        "bill-run enforcement default OFF"
    );
    assert_eq!(cfg.close_lock_timeout_ms, 5_000);
}

#[test]
fn zero_recon_tick_rejected() {
    let cfg = ReconConfig {
        recon_tick_secs: 0,
        ..ReconConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn zero_close_lock_timeout_rejected() {
    let cfg = ReconConfig {
        close_lock_timeout_ms: 0,
        ..ReconConfig::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn default_recon_tick_interval_is_five_minutes() {
    assert_eq!(ReconConfig::default().recon_tick_interval().as_secs(), 300);
}

#[test]
fn default_config_has_no_seller_types_and_events_off() {
    let cfg = BssLedgerConfig::default();
    assert!(!cfg.events_enabled, "events default OFF");
    assert!(
        cfg.seller_tenant_types.is_empty(),
        "the gear MUST NOT ship tenant-type ids: they belong to the deployment's \
         catalogue, and a built-in list would name one product"
    );
    assert!(cfg.jobs.validate().is_ok());
    assert!(cfg.recognition.validate().is_ok());
    assert!(cfg.recon.validate().is_ok());
}

#[test]
fn validate_rejects_a_config_without_seller_types() {
    let err = BssLedgerConfig::default()
        .validate()
        .expect_err("an empty seller set must fail init, not degrade silently");
    let msg = err.to_string();
    assert!(
        msg.contains("seller_tenant_types") && msg.contains("must be set"),
        "the error must name the missing field: {msg}"
    );
}

#[test]
fn validate_accepts_concrete_ids_and_family_wildcards() {
    for entry in [
        "gts.cf.core.am.tenant_type.v1~cf.core.am.partner.v1~",
        "gts.cf.core.am.tenant_type.v1~*",
        "gts.cf.core.am.tenant_type.v1~cf.core.am.partner.*",
    ] {
        let cfg = BssLedgerConfig {
            seller_tenant_types: vec![entry.to_owned()],
            ..BssLedgerConfig::default()
        };
        assert!(cfg.validate().is_ok(), "{entry} should be accepted");
    }
}

#[test]
fn validate_rejects_malformed_seller_entries() {
    // Not a GTS id; a concrete id missing its trailing `~`; the bare wildcard
    // ("every type of every vendor"); and a wildcard with no separator kept.
    for entry in [
        "cf.core.am.tenant_type.v1~cf.core.am.partner.v1~",
        "gts.cf.core.am.tenant_type.v1~cf.core.am.partner.v1",
        "gts.*",
        "gts.cf.core.am.tenant_type.v1~cf.core.am.partner*",
    ] {
        let cfg = BssLedgerConfig {
            seller_tenant_types: vec![entry.to_owned()],
            ..BssLedgerConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("malformed entry must be rejected at init");
        assert!(
            err.to_string().contains("seller_tenant_types"),
            "the error must name the field for {entry}"
        );
    }
}

#[test]
fn validate_rejects_a_blank_fx_provider_vendor() {
    let mut cfg = BssLedgerConfig {
        seller_tenant_types: vec!["gts.cf.core.am.tenant_type.v1~*".to_owned()],
        ..BssLedgerConfig::default()
    };
    cfg.fx.provider_vendor = "   ".to_owned();
    let err = cfg
        .validate()
        .expect_err("a blank vendor matches no plugin");
    assert!(err.to_string().contains("fx.provider_vendor"), "{err}");
}

#[test]
fn default_verify_interval_is_one_day() {
    assert_eq!(JobsConfig::default().verify_interval().as_secs(), 86_400);
}

/// design F-8: the §6 event payloads ship DORMANT — `events_enabled`
/// MUST default to `false` so no broker producer is wired until the platform GTS
/// event-type model lands. A flip to `true` by default would silently activate a
/// surface with no vendored schema and no producer; this locks the deferral.
#[test]
fn events_disabled_by_default_keeps_slice6_events_dormant() {
    assert!(
        !BssLedgerConfig::default().events_enabled,
        "§6 events must stay dormant (events_enabled=false) until a producer is wired (design F-8)"
    );
}

#[test]
fn default_payments_cap_is_the_working_default() {
    assert_eq!(
        PaymentsConfig::default().max_invoices_per_allocation,
        crate::infra::payment::allocate::MAX_INVOICES_PER_ALLOCATION,
    );
    assert!(PaymentsConfig::default().validate().is_ok());
}

#[test]
fn zero_allocation_cap_rejected() {
    let cfg = PaymentsConfig {
        max_invoices_per_allocation: 0,
    };
    assert!(matches!(
        cfg.validate(),
        Err(ConfigError::MustBePositive { .. })
    ));
}

#[test]
fn allocation_cap_above_ceiling_rejected() {
    let cfg = PaymentsConfig {
        max_invoices_per_allocation: MAX_INVOICES_PER_ALLOCATION_CEILING + 1,
    };
    assert!(matches!(cfg.validate(), Err(ConfigError::AboveMax { .. })));
}

#[test]
fn allocation_cap_at_ceiling_accepted() {
    let cfg = PaymentsConfig {
        max_invoices_per_allocation: MAX_INVOICES_PER_ALLOCATION_CEILING,
    };
    assert!(cfg.validate().is_ok());
}
