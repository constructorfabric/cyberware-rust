use super::*;

#[test]
fn config_defaults_are_applied() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 10);
    assert_eq!(cfg.pool_size_min, 2);
    assert_eq!(cfg.pool_size_max, 16);
    assert_eq!(cfg.connection_timeout_secs, 10);
    assert_eq!(cfg.statement_timeout_secs, 30);
    assert_eq!(cfg.retention_period_secs, 365 * 86_400);
    assert!(cfg.database_url.expose().is_empty());
}

#[test]
fn validate_rejects_empty_database_url() {
    let cfg: TimescaleDbPluginConfig = serde_json::from_str("{}").unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_min_gt_max_pool() {
    let json = r#"{ "database_url": "postgres://x", "pool_size_min": 20, "pool_size_max": 4 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_pool_max_of_one() {
    // A max of 1 self-deadlocks startup: post-migration setup holds the single
    // connection under an advisory lock while the retention policy tries to
    // acquire a second, so the pool must allow at least 2.
    let json = r#"{ "database_url": "postgres://x", "pool_size_min": 1, "pool_size_max": 1 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "pool_size_max of 1 must be rejected: it self-deadlocks post-migration setup"
    );
}

#[test]
fn validate_rejects_zero_connection_timeout() {
    let json = r#"{ "database_url": "postgres://x", "connection_timeout_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a zero acquire timeout yields a pool that times out immediately"
    );
}

#[test]
fn validate_rejects_zero_statement_timeout() {
    // Postgres treats `statement_timeout = 0` as *disabled* (no bound), which
    // would reintroduce the unbounded-query footgun this setting exists to close,
    // so a zero must be rejected rather than silently disabling the timeout.
    let json = r#"{ "database_url": "postgres://x", "statement_timeout_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a zero statement_timeout disables the bound, leaving request-path queries unbounded"
    );
}

#[test]
fn validate_accepts_nonzero_statement_timeout() {
    let json = r#"{ "database_url": "postgres://x", "statement_timeout_secs": 45 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn validate_rejects_zero_retention() {
    let json = r#"{ "database_url": "postgres://x", "retention_period_secs": 0 }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a zero retention window would drop every chunk immediately"
    );
}

#[test]
fn validate_rejects_excessive_retention() {
    // A retention so large that `make_interval(secs => ...)` overflows at the
    // DB would otherwise fail *after* migrations run, as a confusing
    // post-migration init error. Catch it as a clean config error upfront.
    let json = format!(
        r#"{{ "database_url": "postgres://x", "retention_period_secs": {} }}"#,
        u64::MAX
    );
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&json).unwrap();
    assert!(
        cfg.validate().is_err(),
        "an absurd retention window must be rejected before it reaches make_interval"
    );
}

#[test]
fn validate_accepts_large_but_sane_retention() {
    // 10 years is well within make_interval's range and a plausible operator
    // choice; it must not trip the upper bound.
    let ten_years = 10u64 * 365 * 86_400;
    let json =
        format!(r#"{{ "database_url": "postgres://x", "retention_period_secs": {ten_years} }}"#);
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&json).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn validate_accepts_well_formed_config() {
    let json = r#"{ "database_url": "postgres://u:p@h/db?sslmode=require" }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn config_rejects_unknown_fields() {
    let json = r#"{ "database_url": "postgres://x", "nope": true }"#;
    assert!(serde_json::from_str::<TimescaleDbPluginConfig>(json).is_err());
}

#[test]
fn expand_vars_expands_database_url_placeholders() {
    use toolkit::var_expand::ExpandVars;
    let json = r#"{ "database_url": "postgres://u:p@h:${UC_TS_DSN_PORT_CANARY_9f3a:-5432}/db?sslmode=require" }"#;
    let mut cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    cfg.expand_vars()
        .expect("expand_vars should resolve placeholders");
    assert_eq!(
        cfg.database_url.expose(),
        "postgres://u:p@h:5432/db?sslmode=require"
    );
}

#[test]
fn debug_does_not_leak_database_url_password() {
    // The DSN embeds the Postgres password; any `Debug` of the config (a stray
    // `tracing::debug!(?cfg)`, a panic formatter) must not print it.
    let json =
        r#"{ "database_url": "postgres://pguser:sup3r-s3cret@db:5432/app?sslmode=require" }"#;
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(json).unwrap();
    let dump = format!("{cfg:?}");
    assert!(
        !dump.contains("sup3r-s3cret"),
        "Debug of the config must not leak the DSN password; got: {dump}"
    );
}
