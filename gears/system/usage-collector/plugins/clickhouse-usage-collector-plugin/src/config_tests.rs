use super::*;

#[test]
fn config_defaults_are_applied() {
    let cfg: ClickHousePluginConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.vendor, "cyberfabric");
    assert_eq!(cfg.priority, 10);
    assert_eq!(cfg.request_timeout_secs, 30);
    assert_eq!(cfg.lock_ttl_secs, 30);
    assert_eq!(cfg.lock_timeout_secs, 5);
    assert_eq!(cfg.retention_period_secs, 365 * 86_400);
    assert!(cfg.database_url.expose().is_empty());
    assert!(!cfg.allow_insecure_http);
}

#[test]
fn validate_rejects_empty_database_url() {
    let cfg: ClickHousePluginConfig = serde_json::from_str("{}").unwrap();
    assert!(cfg.validate().is_err());
}

/// A non-URL `database_url` is rejected at validate time rather than reaching
/// `build_client`, which would otherwise fall back to using the string verbatim
/// as the endpoint and silently drop any embedded credentials.
#[test]
fn validate_rejects_unparseable_database_url() {
    let json = r#"{ "database_url": "not-a-url", "allow_insecure_http": true }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    let err = cfg
        .validate()
        .expect_err("a non-URL database_url must be rejected");
    assert!(
        err.contains("valid absolute URL"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_rejects_zero_request_timeout() {
    let json = r#"{ "database_url": "http://u:p@h/db", "request_timeout_secs": 0 }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_zero_lock_ttl() {
    let json = r#"{ "database_url": "http://u:p@h/db", "lock_ttl_secs": 0 }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_zero_lock_timeout() {
    let json = r#"{ "database_url": "http://u:p@h/db", "lock_timeout_secs": 0 }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn validate_rejects_zero_retention() {
    let json = r#"{ "database_url": "http://u:p@h/db", "retention_period_secs": 0 }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    let err = cfg.validate().unwrap_err();
    assert!(
        err.contains("retention_period_secs"),
        "a zero retention window would drop every row immediately, got: {err}"
    );
}

#[test]
fn validate_rejects_excessive_retention() {
    let json = format!(
        r#"{{ "database_url": "http://u:p@h/db", "retention_period_secs": {} }}"#,
        u64::MAX
    );
    let cfg: ClickHousePluginConfig = serde_json::from_str(&json).unwrap();
    let err = cfg.validate().unwrap_err();
    assert!(
        err.contains("retention_period_secs"),
        "an absurd retention window must be rejected before it reaches ClickHouse, got: {err}"
    );
}

#[test]
fn validate_accepts_large_but_sane_retention() {
    let ten_years = 10u64 * 365 * 86_400;
    let json = format!(
        r#"{{ "database_url": "https://u:p@h/db", "retention_period_secs": {ten_years} }}"#
    );
    let cfg: ClickHousePluginConfig = serde_json::from_str(&json).unwrap();
    assert!(cfg.validate().is_ok());
}

/// A blank `vendor` would otherwise surface as an opaque failure at GTS
/// instance registration, long after config load.
#[test]
fn validate_rejects_blank_vendor() {
    for vendor in ["", "   "] {
        let json = format!(r#"{{ "database_url": "https://u:p@h/db", "vendor": "{vendor}" }}"#);
        let cfg: ClickHousePluginConfig = serde_json::from_str(&json).unwrap();
        let err = cfg
            .validate()
            .expect_err("a blank vendor must be rejected at config-validation time");
        assert!(err.contains("vendor"), "unexpected error: {err}");
    }
}

#[test]
fn validate_accepts_well_formed_config() {
    let json = r#"{ "database_url": "https://user:pass@ch:8123/usage" }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_ok());
}

#[test]
fn validate_rejects_plaintext_http_database_url_by_default() {
    let json = r#"{ "database_url": "http://u:p@h/db" }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        !cfg.allow_insecure_http,
        "allow_insecure_http must default to false"
    );
    let err = cfg.validate().unwrap_err();
    assert!(
        err.contains("allow_insecure_http"),
        "a plaintext http:// database_url must be rejected unless allow_insecure_http is set, \
         got: {err}"
    );
}

#[test]
fn validate_accepts_plaintext_http_database_url_with_explicit_override() {
    let json = r#"{ "database_url": "http://u:p@h/db", "allow_insecure_http": true }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_ok(),
        "an explicit allow_insecure_http override must permit a plaintext database_url"
    );
}

#[test]
fn validate_accepts_https_database_url_without_override() {
    let json = r#"{ "database_url": "https://u:p@h/db" }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_ok(),
        "a https:// database_url never needs allow_insecure_http"
    );
}

#[test]
fn config_rejects_unknown_fields() {
    let json = r#"{ "database_url": "http://u:p@h/db", "nope": true }"#;
    assert!(serde_json::from_str::<ClickHousePluginConfig>(json).is_err());
}

#[test]
fn expand_vars_expands_database_url_placeholders() {
    use toolkit::var_expand::ExpandVars;
    let json = r#"{ "database_url": "http://u:${UC_CH_DSN_PORT_CANARY_9f3a:-8123}/db" }"#;
    let mut cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    cfg.expand_vars()
        .expect("expand_vars should resolve placeholders");
    assert_eq!(cfg.database_url.expose(), "http://u:8123/db");
}

#[test]
fn debug_does_not_leak_database_url_password() {
    let json = r#"{ "database_url": "http://chuser:sup3r-s3cret@ch:8123/usage" }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    let dump = format!("{cfg:?}");
    assert!(
        !dump.contains("sup3r-s3cret"),
        "Debug of the config must not leak the URL password; got: {dump}"
    );
}
