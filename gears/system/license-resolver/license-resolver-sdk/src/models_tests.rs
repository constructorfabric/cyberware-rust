//! Unit tests for the license resolver value objects.

use toolkit_gts::gts_id;

use super::{LicenseCheckContext, LicenseCheckRequest, LicenseDecision, Resource, Subject};

const SUBJECT_CONTRACT: &str = gts_id!("cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~");
const RESOURCE_CONTRACT: &str = gts_id!("cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~");

#[test]
fn subject_round_trips() {
    let json = format!(
        r#"{{"type":"{SUBJECT_CONTRACT}","id":"acme-admin","metadata":{{"category":"internal"}}}}"#
    );
    let subject: Subject = serde_json::from_str(&json).expect("valid subject");

    assert_eq!(subject.gts_type.as_ref(), SUBJECT_CONTRACT);
    assert_eq!(subject.id.as_deref(), Some("acme-admin"));
    assert_eq!(
        subject.metadata.get("category").and_then(|v| v.as_str()),
        Some("internal")
    );

    let wire = serde_json::to_value(&subject).unwrap();
    assert_eq!(
        wire.get("type").and_then(|v| v.as_str()),
        Some(SUBJECT_CONTRACT)
    );
    let back: Subject = serde_json::from_value(wire).unwrap();
    assert_eq!(back, subject);
}

#[test]
fn resource_whole_type_omits_id() {
    let json = format!(r#"{{"type":"{RESOURCE_CONTRACT}","metadata":{{}}}}"#);
    let resource: Resource = serde_json::from_str(&json).expect("valid whole-type resource");
    assert!(resource.id.is_none());

    let wire = serde_json::to_value(&resource).unwrap();
    assert!(
        wire.get("id").is_none(),
        "absent id must be omitted for a whole-type check, got: {wire}"
    );
    assert_eq!(
        wire.get("type").and_then(|v| v.as_str()),
        Some(RESOURCE_CONTRACT)
    );
}

#[test]
fn resource_specific_instance_carries_id() {
    let json = format!(
        r#"{{"type":"{RESOURCE_CONTRACT}","id":"gpt-4o","metadata":{{"model_vendor":"openai"}}}}"#
    );
    let resource: Resource = serde_json::from_str(&json).expect("valid resource");
    assert_eq!(resource.id.as_deref(), Some("gpt-4o"));
    assert_eq!(
        resource
            .metadata
            .get("model_vendor")
            .and_then(|v| v.as_str()),
        Some("openai")
    );
}

#[test]
fn decision_defaults_to_not_granted_with_empty_diagnostics() {
    let decision = LicenseDecision::default();
    assert!(!decision.granted);
    assert!(decision.diagnostics.is_empty());
}

#[test]
fn request_round_trips() {
    let request = LicenseCheckRequest::new(
        serde_json::from_str(&format!(
            r#"{{"type":"{SUBJECT_CONTRACT}","metadata":{{}}}}"#
        ))
        .unwrap(),
        serde_json::from_str(&format!(
            r#"{{"type":"{RESOURCE_CONTRACT}","metadata":{{}}}}"#
        ))
        .unwrap(),
        LicenseCheckContext::builder()
            .tenant_id(uuid::Uuid::nil())
            .build()
            .expect("tenant set"),
    );
    let back: LicenseCheckRequest =
        serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
    assert_eq!(back, request);
}

#[test]
fn nonconforming_resource_payloads_do_not_deserialize() {
    for (case, raw) in [
        (
            "missing metadata",
            format!(r#"{{"type":"{RESOURCE_CONTRACT}"}}"#),
        ),
        (
            "unknown top-level field",
            format!(r#"{{"type":"{RESOURCE_CONTRACT}","metadata":{{}},"bogus":1}}"#),
        ),
        (
            "non-object metadata",
            format!(r#"{{"type":"{RESOURCE_CONTRACT}","metadata":"invalid"}}"#),
        ),
        ("missing type", r#"{"metadata":{}}"#.to_owned()),
    ] {
        assert!(
            serde_json::from_str::<Resource>(&raw).is_err(),
            "`{case}` must not be laundered into a conforming Resource: {raw}"
        );
    }
}

#[test]
fn nonconforming_subject_payloads_do_not_deserialize() {
    for (case, raw) in [
        (
            "unknown top-level field",
            format!(r#"{{"type":"{SUBJECT_CONTRACT}","metadata":{{}},"bogus":1}}"#),
        ),
        (
            "misspelled id",
            format!(r#"{{"type":"{SUBJECT_CONTRACT}","idd":"acme-admin","metadata":{{}}}}"#),
        ),
    ] {
        assert!(
            serde_json::from_str::<Subject>(&raw).is_err(),
            "`{case}` must not be laundered into a conforming Subject: {raw}"
        );
    }
}

#[test]
fn misspelled_resource_id_is_rejected_instead_of_widening_the_check() {
    let raw = format!(r#"{{"type":"{RESOURCE_CONTRACT}","idd":"gpt-4o","metadata":{{}}}}"#);
    assert!(
        serde_json::from_str::<Resource>(&raw).is_err(),
        "a misspelled id must not silently become a whole-type check: {raw}"
    );
}

#[test]
fn envelope_ignores_unknown_fields() {
    let tenant = uuid::Uuid::from_u128(0x1d2f_6e3a);
    let raw = format!(
        concat!(
            r#"{{"subject":{{"type":"{subject}","metadata":{{}}}},"#,
            r#""resource":{{"type":"{resource}","metadata":{{}}}},"#,
            r#""context":{{"tenant_id":"{tenant}","future_contextual_input":true}},"#,
            r#""future_request_input":1}}"#
        ),
        subject = SUBJECT_CONTRACT,
        resource = RESOURCE_CONTRACT,
        tenant = tenant
    );

    let request: LicenseCheckRequest = serde_json::from_str(&raw)
        .expect("a newer producer's request must stay readable by an older reader");
    assert_eq!(request.context.tenant_id, tenant);
    assert_eq!(request.subject.gts_type.as_ref(), SUBJECT_CONTRACT);
}

#[test]
fn decision_ignores_unknown_fields() {
    let decision: LicenseDecision =
        serde_json::from_str(r#"{"granted":true,"diagnostics":{},"grant_status":"active"}"#)
            .expect("granted is authoritative on its own");
    assert!(decision.granted);
}

#[test]
fn context_derives_tenant_from_security_context() {
    let tenant = uuid::Uuid::from_u128(0x1d2f_6e3a);
    let ctx = toolkit_security::SecurityContext::builder()
        .subject_id(uuid::Uuid::from_u128(7))
        .subject_tenant_id(tenant)
        .build()
        .expect("valid security context");

    let derived = LicenseCheckContext::from_security_context(&ctx);
    assert_eq!(derived.tenant_id, tenant);
    assert_ne!(derived.tenant_id, uuid::Uuid::nil());

    let request = LicenseCheckRequest::new(
        serde_json::from_str(&format!(
            r#"{{"type":"{SUBJECT_CONTRACT}","metadata":{{}}}}"#
        ))
        .unwrap(),
        serde_json::from_str(&format!(
            r#"{{"type":"{RESOURCE_CONTRACT}","metadata":{{}}}}"#
        ))
        .unwrap(),
        LicenseCheckContext::from_security_context(&ctx),
    );
    assert_eq!(request.context, derived);
}
