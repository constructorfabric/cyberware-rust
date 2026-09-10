//! Unit tests for the static grant evaluation.

use serde_json::json;

use super::*;
use crate::config::StaticLicensePluginConfig;
use crate::test_support::{OTHER_SUBJECT_TYPE, SUBJECT_TYPE, request, rule};

fn service_with(grants: Vec<crate::config::GrantRule>) -> Service {
    Service::from_config(&StaticLicensePluginConfig {
        grants,
        ..StaticLicensePluginConfig::default()
    })
    .expect("valid rule set")
}

fn deny_reason_of(decision: &LicenseDecision) -> Option<&str> {
    decision
        .diagnostics
        .get(diagnostics::DENY_REASON)
        .and_then(|v| v.as_str())
}

/// Deny by default: an unconfigured backend licenses nothing, and says why.
#[test]
fn an_empty_rule_set_denies_and_says_so() {
    let decision = service_with(Vec::new()).evaluate(&request());
    assert!(!decision.granted);
    assert_eq!(
        deny_reason_of(&decision),
        Some(deny_reason::NO_GRANTS_CONFIGURED)
    );
    assert_eq!(
        decision.diagnostics.get(diagnostics::BACKEND),
        Some(&json!(BACKEND_ID))
    );
}

#[test]
fn a_matching_rule_grants_and_names_itself() {
    let decision = service_with(vec![rule()]).evaluate(&request());
    assert!(decision.granted);
    assert_eq!(
        decision.diagnostics.get(diagnostics::MATCHED_RULE),
        Some(&json!(0)),
        "a grant must say which rule answered: {decision:?}"
    );
    assert!(
        !decision.diagnostics.contains_key(diagnostics::DENY_REASON),
        "a grant carries no denial reason"
    );
}

#[test]
fn a_rule_set_that_covers_nothing_denies() {
    let mut elsewhere = rule();
    elsewhere.subject_type = OTHER_SUBJECT_TYPE.to_owned();

    let decision = service_with(vec![elsewhere]).evaluate(&request());
    assert!(!decision.granted);
    assert_eq!(
        deny_reason_of(&decision),
        Some(deny_reason::NO_MATCHING_GRANT)
    );
}

/// The reported index must be the rule that actually answered, so an operator
/// reading diagnostics lands on the right config line.
#[test]
fn the_first_matching_rule_answers() {
    let mut misses = rule();
    misses.subject_type = OTHER_SUBJECT_TYPE.to_owned();
    let mut narrower = rule();
    narrower.subject_id = Some("acme-admin".to_owned());

    let decision = service_with(vec![misses, narrower, rule()]).evaluate(&request());
    assert!(decision.granted);
    assert_eq!(
        decision.diagnostics.get(diagnostics::MATCHED_RULE),
        Some(&json!(1))
    );
}

/// The backend refuses to start on a rule set it could never apply, rather than
/// coming up and denying everything.
#[test]
fn an_invalid_rule_set_is_refused_at_construction() {
    let mut broken = rule();
    broken.resource_type = SUBJECT_TYPE.to_owned();

    let result = Service::from_config(&StaticLicensePluginConfig {
        grants: vec![broken],
        ..StaticLicensePluginConfig::default()
    });
    assert!(
        result.is_err(),
        "a rule that can never match must fail construction, not silently deny"
    );
}
