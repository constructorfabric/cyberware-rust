use super::*;

#[test]
fn config_defaults_are_applied() {
    let cfg: ClickHousePluginConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.vendor, "cyberfabric");
    assert_eq!(cfg.priority, 10);
    assert_eq!(cfg.request_timeout_secs, 30);
    assert_eq!(cfg.lock_ttl_secs, 60);
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

/// The client-side deadline must sit strictly *past* the server-side budget,
/// so that when the server is answering it is the server's own descriptive
/// timeout that surfaces, not a bare client-side one.
#[test]
fn client_deadline_is_the_request_timeout_plus_the_grace_margin() {
    let json = r#"{ "database_url": "https://u:p@h/db", "request_timeout_secs": 12 }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert_eq!(
        cfg.client_deadline(),
        std::time::Duration::from_secs(12 + CLIENT_DEADLINE_GRACE_SECS)
    );

    let default_cfg = ClickHousePluginConfig::default();
    assert_eq!(
        default_cfg.client_deadline(),
        std::time::Duration::from_secs(30 + CLIENT_DEADLINE_GRACE_SECS),
        "the default config must also get a deadline past its server-side budget"
    );
}

/// `validate` bounds `request_timeout_secs` only from below, so the grace add
/// has to saturate rather than overflow.
#[test]
fn client_deadline_saturates_instead_of_overflowing() {
    let json = format!(
        r#"{{ "database_url": "https://u:p@h/db", "request_timeout_secs": {} }}"#,
        u64::MAX
    );
    let cfg: ClickHousePluginConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(
        cfg.client_deadline(),
        std::time::Duration::from_secs(u64::MAX)
    );
}

#[test]
fn validate_rejects_zero_lock_ttl() {
    let json = r#"{ "database_url": "http://u:p@h/db", "lock_ttl_secs": 0 }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(cfg.validate().is_err());
}

/// The default pair must satisfy the invariant `validate` enforces — a stock
/// config is not allowed to be a config the plugin refuses to start on.
#[test]
fn default_lock_ttl_exceeds_the_default_client_deadline() {
    let default_cfg = ClickHousePluginConfig::default();
    assert!(
        default_cfg.lock_ttl_secs > default_cfg.client_deadline().as_secs(),
        "default lock_ttl_secs ({}) must exceed the default client deadline ({}s)",
        default_cfg.lock_ttl_secs,
        default_cfg.client_deadline().as_secs()
    );
}

/// A lock lease that a single `ClickHouse` round-trip can outlive makes the
/// pre-write renew meaningless: the write it protects can expire the lease it
/// was just handed. Rejected at config load rather than surfacing as sporadic
/// `Transient` failures under load.
#[test]
fn validate_rejects_lock_ttl_at_or_below_the_client_deadline() {
    // request_timeout 30 => client deadline 35; a 35s TTL is exactly the
    // boundary case and must be rejected too (the relation is strict).
    for ttl in [30_u64, 35] {
        let json = format!(
            r#"{{ "database_url": "https://u:p@h/db", "request_timeout_secs": 30, "lock_ttl_secs": {ttl} }}"#
        );
        let cfg: ClickHousePluginConfig = serde_json::from_str(&json).unwrap();
        let err = cfg
            .validate()
            .expect_err("a lock TTL within one round-trip of the client deadline must be rejected");
        assert!(
            err.contains("lock_ttl_secs") && err.contains("client deadline"),
            "the error must name both sides of the relation, got: {err}"
        );
    }

    let json = r#"{ "database_url": "https://u:p@h/db", "request_timeout_secs": 30, "lock_ttl_secs": 36 }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    cfg.validate()
        .expect("one second past the client deadline satisfies the relation");
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

/// URL schemes are case-insensitive: a mixed-case `HTTP://` DSN connects in
/// cleartext exactly like `http://`, since the `url` crate lowercases the
/// scheme before the endpoint reaches the client. The TLS gate must therefore
/// catch it too — a prefix test on the raw string does not.
#[test]
fn validate_rejects_mixed_case_plaintext_scheme_by_default() {
    for url in ["HTTP://u:p@h/db", "Http://u:p@h/db"] {
        let json = format!(r#"{{ "database_url": "{url}" }}"#);
        let cfg: ClickHousePluginConfig = serde_json::from_str(&json).unwrap();
        let Err(err) = cfg.validate() else {
            panic!("{url} must be rejected like http://: the scheme is case-insensitive");
        };
        assert!(
            err.contains("allow_insecure_http"),
            "{url} must be refused by the TLS gate, got: {err}"
        );
    }
}

/// The mixed-case gate is symmetric: the override still opts out, so
/// normalizing the scheme tightened the check without breaking the escape hatch.
#[test]
fn validate_accepts_mixed_case_plaintext_scheme_with_explicit_override() {
    let json = r#"{ "database_url": "HTTP://u:p@h/db", "allow_insecure_http": true }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_ok(),
        "an explicit allow_insecure_http override must permit a mixed-case plaintext database_url"
    );
}

#[test]
fn validate_accepts_mixed_case_tls_scheme_without_override() {
    let json = r#"{ "database_url": "HTTPS://u:p@h/db" }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.validate().is_ok(),
        "a mixed-case HTTPS:// database_url is encrypted and never needs the override"
    );
}

/// The `clickhouse` client speaks only `ClickHouse`'s HTTP interface, so any
/// other scheme — including the native-protocol `clickhouse://` URL an operator
/// might reach for by habit — is a misconfiguration. Rejecting it here turns an
/// opaque first-query failure into a startup error naming the scheme.
#[test]
fn validate_rejects_non_http_schemes() {
    for url in [
        "ftp://h/db",
        "file:///tmp/db",
        "clickhouse://ch:9000/db",
        "tcp://ch:9000",
        // A bare `host:port` is not a relative URL: `Url::parse` reads `ch` as
        // the scheme and `8123` as the path, so it clears the well-formedness
        // check and only the scheme allowlist catches it.
        "ch:8123",
    ] {
        let json = format!(r#"{{ "database_url": "{url}" }}"#);
        let cfg: ClickHousePluginConfig = serde_json::from_str(&json).unwrap();
        let Err(err) = cfg.validate() else {
            panic!("{url} must be rejected: the ClickHouse client is HTTP-only");
        };
        assert!(
            err.contains("scheme"),
            "{url} must be refused with a scheme error, got: {err}"
        );
    }
}

/// `allow_insecure_http` is consent to skip TLS, not consent to a scheme the
/// client cannot speak. The two checks stay independent.
#[test]
fn validate_rejects_non_http_schemes_even_with_the_insecure_override() {
    let json = r#"{ "database_url": "clickhouse://ch:9000/db", "allow_insecure_http": true }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    let err = cfg
        .validate()
        .expect_err("allow_insecure_http must not admit a non-HTTP scheme");
    assert!(err.contains("scheme"), "unexpected error: {err}");
}

/// The scheme is the only part of the DSN that may appear in the error: the URL
/// carries credentials and is otherwise never surfaced.
#[test]
fn scheme_rejection_does_not_leak_the_database_url() {
    let json = r#"{ "database_url": "ftp://chuser:sup3r-s3cret@ch:2121/usage" }"#;
    let cfg: ClickHousePluginConfig = serde_json::from_str(json).unwrap();
    let err = cfg.validate().unwrap_err();
    assert!(
        !err.contains("sup3r-s3cret") && !err.contains("chuser"),
        "the scheme rejection must not echo the DSN, got: {err}"
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
