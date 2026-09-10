use super::*;

#[test]
fn config_defaults_are_applied() {
    let cfg: NoopUsageCollectorPluginConfig = serde_json::from_str("{}").unwrap();

    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn config_overrides_are_honored() {
    let json = r#"{ "vendor": "acme", "priority": 5 }"#;

    let cfg: NoopUsageCollectorPluginConfig = serde_json::from_str(json).unwrap();

    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, 5);
}

#[test]
fn config_rejects_unknown_fields() {
    let json = r#"{ "vendor": "constructorfabric", "priority": 100, "unexpected": true }"#;

    let parsed: Result<NoopUsageCollectorPluginConfig, _> = serde_json::from_str(json);
    assert!(parsed.is_err());
}
