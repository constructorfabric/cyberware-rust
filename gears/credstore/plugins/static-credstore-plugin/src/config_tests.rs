// Created: 2026-04-07 by Constructor Tech
// Updated: 2026-09-10 by Constructor Tech — value-seeding fields removed (ADR-0006).
use super::*;

#[test]
fn config_defaults_are_applied() {
    let cfg: StaticCredStorePluginConfig = serde_saphyr::from_str("{}").expect("parse");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn config_accepts_explicit_values() {
    let yaml = r#"
vendor: "acme"
priority: 5
"#;
    let cfg: StaticCredStorePluginConfig = serde_saphyr::from_str(yaml).expect("parse");
    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, 5);
}

#[test]
fn config_rejects_unknown_fields() {
    let yaml = r#"
vendor: "constructorfabric"
priority: 100
unexpected: true
"#;
    let parsed: Result<StaticCredStorePluginConfig, _> = serde_saphyr::from_str(yaml);
    assert!(parsed.is_err());
}

#[test]
fn config_rejects_withdrawn_secrets_field() {
    // ADR-0006 withdraws out-of-band value seeding; a config carrying the old
    // `secrets:` block must fail closed (deny_unknown_fields), not silently
    // ignore seeded values a caller believes are live.
    let yaml = r#"
secrets:
  - tenant_id: "00000000-0000-0000-0000-000000000001"
    key: "openai_api_key"
    value: "sk-test-123"
"#;
    let parsed: Result<StaticCredStorePluginConfig, _> = serde_saphyr::from_str(yaml);
    assert!(parsed.is_err());
}
