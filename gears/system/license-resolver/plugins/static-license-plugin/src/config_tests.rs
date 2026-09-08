//! Unit tests for the plugin configuration and its rule-matching semantics.

use serde_json::{Map, json};

use super::*;
use crate::test_support::{
    OTHER_SUBJECT_TYPE, RESOURCE_BASE, RESOURCE_TYPE, SUBJECT_TYPE, other_tenant, request,
    request_for, resource, rule, subject, tenant,
};

/// The default is deny-all: a plugin added but not configured licenses nothing.
#[test]
fn default_config_grants_nothing() {
    let cfg = StaticLicensePluginConfig::default();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert!(cfg.grants.is_empty());
}

#[test]
fn a_full_rule_deserializes() {
    let cfg: StaticLicensePluginConfig = serde_json::from_value(json!({
        "vendor": "acme",
        "priority": 5,
        "grants": [{
            "resource_type": RESOURCE_TYPE,
            "resource_id": "gpt-4o",
            "subject_type": SUBJECT_TYPE,
            "subject_id": "acme-admin",
            "tenant_id": tenant(),
            "resource_metadata": { "model_vendor": "openai" },
            "subject_metadata": { "category": "internal" },
        }],
    }))
    .expect("config deserializes");

    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, 5);
    let grant = &cfg.grants[0];
    assert_eq!(grant.resource_id.as_deref(), Some("gpt-4o"));
    assert_eq!(grant.subject_id.as_deref(), Some("acme-admin"));
    assert_eq!(grant.tenant_id, Some(tenant()));
    assert_eq!(
        grant.resource_metadata.get("model_vendor"),
        Some(&json!("openai"))
    );
}

#[test]
fn a_minimal_rule_deserializes_with_everything_unconstrained() {
    let cfg: StaticLicensePluginConfig = serde_json::from_value(json!({
        "grants": [{ "resource_type": RESOURCE_TYPE, "subject_type": SUBJECT_TYPE }],
    }))
    .expect("config deserializes");

    let grant = &cfg.grants[0];
    assert!(grant.resource_id.is_none());
    assert!(grant.subject_id.is_none());
    assert!(grant.tenant_id.is_none());
    assert!(grant.resource_metadata.is_empty());
    assert!(grant.subject_metadata.is_empty());
}

#[test]
fn an_unknown_rule_field_is_rejected() {
    let result: Result<StaticLicensePluginConfig, _> = serde_json::from_value(json!({
        "grants": [{
            "resource_type": RESOURCE_TYPE,
            "subject_type": SUBJECT_TYPE,
            "metadata_match": { "model_vendor": "openai" },
        }],
    }));
    assert!(
        result.is_err(),
        "a misspelled field must not be silently ignored into a weaker rule"
    );
}

#[test]
fn a_well_formed_rule_set_validates() {
    let cfg = StaticLicensePluginConfig {
        grants: vec![rule()],
        ..StaticLicensePluginConfig::default()
    };
    assert!(cfg.validate().is_ok());
}

/// A rule naming a type no check can carry would never match, and a rule that
/// never matches reads as "not licensed" — a deny that hides a typo.
#[test]
fn a_rule_naming_the_wrong_licensing_base_is_rejected() {
    let mut swapped = rule();
    swapped.resource_type = SUBJECT_TYPE.to_owned();
    let cfg = StaticLicensePluginConfig {
        grants: vec![swapped],
        ..StaticLicensePluginConfig::default()
    };
    let err = cfg
        .validate()
        .expect_err("a mis-slotted contract is invalid");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("resource_type"),
        "the error must name the offending field: {rendered}"
    );
    assert!(
        rendered.contains("#0"),
        "the error must name the offending rule: {rendered}"
    );
}

#[test]
fn a_rule_naming_a_malformed_type_id_is_rejected() {
    let mut malformed = rule();
    malformed.subject_type = "not-a-gts-id".to_owned();
    let cfg = StaticLicensePluginConfig {
        grants: vec![malformed],
        ..StaticLicensePluginConfig::default()
    };
    assert!(cfg.validate().is_err());
}

/// The abstract base is not instantiable, so a rule pointing at it can never be
/// exercised by a check.
#[test]
fn a_rule_naming_the_bare_licensing_base_is_rejected() {
    let mut bare = rule();
    bare.resource_type = RESOURCE_BASE.to_owned();
    let cfg = StaticLicensePluginConfig {
        grants: vec![bare],
        ..StaticLicensePluginConfig::default()
    };
    assert!(
        cfg.validate().is_err(),
        "the licensing base itself is not a contract a check can name"
    );
}

#[test]
fn a_rule_for_another_subject_type_does_not_match() {
    let mut other = rule();
    other.subject_type = OTHER_SUBJECT_TYPE.to_owned();
    assert!(!other.matches(&request()));
}

/// A grant for the whole type answers a check naming one instance of it, and
/// an id-less check about the type as a class.
#[test]
fn a_whole_type_grant_answers_any_check_for_that_type() {
    let grant = rule();
    assert!(grant.matches(&request()));

    let whole_type = request_for(
        subject(None, json!({})),
        resource(None, json!({})),
        tenant(),
    );
    assert!(grant.matches(&whole_type));
}

/// The converse must not hold: a grant for one instance is not a licence for
/// the type as a class, which is what an id-less check asks about.
#[test]
fn an_instance_grant_does_not_answer_a_whole_type_check() {
    let mut grant = rule();
    grant.resource_id = Some("gpt-4o".to_owned());

    let whole_type = request_for(
        subject(None, json!({})),
        resource(None, json!({})),
        tenant(),
    );
    assert!(!grant.matches(&whole_type));
}

#[test]
fn an_instance_grant_matches_only_that_instance() {
    let mut grant = rule();
    grant.resource_id = Some("gpt-4o".to_owned());

    assert!(grant.matches(&request()));

    let other_model = request_for(
        subject(Some("acme-admin"), json!({})),
        resource(Some("claude-4"), json!({})),
        tenant(),
    );
    assert!(!grant.matches(&other_model));
}

#[test]
fn a_tenant_scoped_grant_does_not_leak_to_another_tenant() {
    let mut grant = rule();
    grant.tenant_id = Some(tenant());
    assert!(grant.matches(&request()));

    let elsewhere = request_for(
        subject(Some("acme-admin"), json!({})),
        resource(Some("gpt-4o"), json!({})),
        other_tenant(),
    );
    assert!(
        !grant.matches(&elsewhere),
        "a grant scoped to one tenant must not answer another tenant's check"
    );
}

/// Attribute-based licensing: the rule constrains only the properties it names,
/// and ignores the rest of `metadata`.
#[test]
fn metadata_constraints_are_a_subset_match() {
    let mut grant = rule();
    grant.resource_metadata = match json!({ "model_vendor": "openai" }) {
        serde_json::Value::Object(map) => map,
        _ => Map::new(),
    };
    assert!(
        grant.matches(&request()),
        "model_name is not constrained and must not block the match"
    );

    let other_vendor = request_for(
        subject(Some("acme-admin"), json!({})),
        resource(
            Some("claude-4"),
            json!({ "model_vendor": "anthropic", "model_name": "claude-4" }),
        ),
        tenant(),
    );
    assert!(!grant.matches(&other_vendor));
}

#[test]
fn a_metadata_constraint_on_an_absent_property_does_not_match() {
    let mut grant = rule();
    grant.subject_metadata = match json!({ "category": "internal" }) {
        serde_json::Value::Object(map) => map,
        _ => Map::new(),
    };
    assert!(grant.matches(&request()));

    let no_category = request_for(
        subject(Some("acme-admin"), json!({})),
        resource(Some("gpt-4o"), json!({})),
        tenant(),
    );
    assert!(!grant.matches(&no_category));
}
