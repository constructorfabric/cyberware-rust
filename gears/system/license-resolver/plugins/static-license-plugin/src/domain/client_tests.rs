//! Unit tests for the plugin client boundary.

use license_resolver_sdk::LicenseResolverPluginClient;

use super::*;
use crate::config::StaticLicensePluginConfig;
use crate::domain::{deny_reason, diagnostics};
use crate::test_support::{request, rule};

fn service_with(grants: Vec<crate::config::GrantRule>) -> Service {
    Service::from_config(&StaticLicensePluginConfig {
        grants,
        ..StaticLicensePluginConfig::default()
    })
    .expect("valid rule set")
}

#[tokio::test]
async fn a_grant_crosses_the_plugin_boundary() {
    let decision = service_with(vec![rule()])
        .is_licensed(request())
        .await
        .expect("an in-memory backend cannot be unavailable");
    assert!(decision.granted);
}

#[tokio::test]
async fn a_denial_is_returned_as_a_decision_not_an_error() {
    let result = service_with(Vec::new()).is_licensed(request()).await;
    let decision = result.expect("a denial must not surface as an error");
    assert!(!decision.granted);
    assert_eq!(
        decision
            .diagnostics
            .get(diagnostics::DENY_REASON)
            .and_then(|v| v.as_str()),
        Some(deny_reason::NO_GRANTS_CONFIGURED)
    );
}
