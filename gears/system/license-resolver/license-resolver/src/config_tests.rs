//! Unit tests for [`LicenseResolverConfig`].

use super::LicenseResolverConfig;

#[test]
fn an_empty_config_selects_the_first_party_vendor() {
    let cfg: LicenseResolverConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(cfg.vendor, "constructorfabric");
}

#[test]
fn unknown_fields_are_rejected() {
    let err = serde_json::from_value::<LicenseResolverConfig>(serde_json::json!({ "vndor": "x" }))
        .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "got: {err}");
}
