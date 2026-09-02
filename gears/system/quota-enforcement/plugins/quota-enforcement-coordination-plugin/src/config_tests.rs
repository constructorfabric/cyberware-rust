use super::CoordinationPluginConfig;

#[test]
fn default_config_selects_the_platform_vendor_and_validates() {
    let cfg = CoordinationPluginConfig::default();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    cfg.validate().expect("default config is valid");
}

#[test]
fn blank_vendor_is_rejected_with_the_config_path_in_the_message() {
    for vendor in ["", "   ", "\t"] {
        let cfg = CoordinationPluginConfig {
            vendor: vendor.to_owned(),
            priority: 1,
        };
        let err = cfg.validate().expect_err("blank vendor rejected");
        assert!(err.to_string().contains("vendor"), "{err}");
    }
}

#[test]
fn unknown_keys_and_partial_configs_deserialize_as_designed() {
    let cfg: CoordinationPluginConfig =
        serde_json::from_str(r#"{ "priority": 7 }"#).expect("partial config uses defaults");
    assert_eq!(cfg.priority, 7);
    assert_eq!(cfg.vendor, "constructorfabric");
    let err = serde_json::from_str::<CoordinationPluginConfig>(r#"{ "vendour": "x" }"#);
    assert!(err.is_err(), "typos must not pass silently");
}
