#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests demonstrating how a consuming Gear builds a license check.
//!
//! They compile from outside the crate, which carries a guarantee no unit test
//! can: `LicenseCheckContext`, `LicenseCheckRequest` and `LicenseDecision` are
//! `#[non_exhaustive]` so a future contextual input stays additive, which puts
//! struct literals out of reach for consumers and makes the constructors the
//! whole public surface. If a growth-surface field ever becomes unreachable
//! through a constructor, this stops compiling.

use license_resolver_sdk::{
    LicenseCheckContext, LicenseCheckContextBuildError, LicenseCheckRequest, LicenseDecision,
    Resource, Subject,
};
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use uuid::Uuid;

const SUBJECT_CONTRACT: &str = gts_id!("cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~");
const RESOURCE_CONTRACT: &str = gts_id!("cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~");

fn subject() -> Subject {
    serde_json::from_str(&format!(
        r#"{{"type":"{SUBJECT_CONTRACT}","metadata":{{"category":"internal"}}}}"#
    ))
    .expect("valid subject")
}

fn resource() -> Resource {
    serde_json::from_str(&format!(
        r#"{{"type":"{RESOURCE_CONTRACT}","id":"gpt-4o","metadata":{{"model_vendor":"openai"}}}}"#
    ))
    .expect("valid resource")
}

#[test]
fn a_consumer_can_build_a_request_from_its_security_context() {
    let tenant = Uuid::from_u128(0x1d2f_6e3a);
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::from_u128(7))
        .subject_tenant_id(tenant)
        .build()
        .expect("valid security context");

    let request = LicenseCheckRequest::new(
        subject(),
        resource(),
        LicenseCheckContext::from_security_context(&ctx),
    );

    assert_eq!(request.context.tenant_id, tenant);
    assert_eq!(request.resource.id.as_deref(), Some("gpt-4o"));
}

#[test]
fn a_consumer_without_a_security_context_can_build_one_field_by_field() {
    let tenant = Uuid::from_u128(42);
    let context = LicenseCheckContext::builder()
        .tenant_id(tenant)
        .build()
        .expect("tenant scope set");

    let request = LicenseCheckRequest::new(subject(), resource(), context);

    assert_eq!(request.context.tenant_id, tenant);
}

#[test]
fn building_a_context_without_a_tenant_is_an_error() {
    let err = LicenseCheckContext::builder()
        .build()
        .expect_err("a check is always tenant-scoped");

    assert!(matches!(
        err,
        LicenseCheckContextBuildError::MissingTenantId
    ));
}

#[test]
fn a_plugin_can_answer_without_a_struct_literal() {
    let denied = LicenseDecision::default();
    assert!(!denied.granted, "the default answer is fail-closed");

    let granted = LicenseDecision::new(true).with_diagnostic("backend", "static-license-plugin");
    assert!(granted.granted);
    assert_eq!(
        granted.diagnostics.get("backend").and_then(|v| v.as_str()),
        Some("static-license-plugin"),
        "diagnostics stay advisory but must survive the constructor"
    );
}
