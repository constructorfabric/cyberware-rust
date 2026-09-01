//! Tests for REST authentication-context extraction.

use super::require_authenticated;
use axum::extract::Extension;
use toolkit_security::SecurityContext;

fn ctx_with_token_scopes(scopes: Vec<String>) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::uuid!("11111111-2222-3333-4444-555555555555"))
        .subject_tenant_id(uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"))
        .subject_type("gts.cf.core.security.subject_user.v1~")
        .token_scopes(scopes)
        .build()
        .expect("test SecurityContext must build")
}

/// A caller with non-`"*"` token scopes is NOT flagged as first-party root, so
/// `caller_scope_from_context` routes them via `CallerScope::Tenant(_)`.

#[test]
fn require_authenticated_rejects_missing_extension() {
    assert!(
        require_authenticated(None).is_err(),
        "absent SecurityContext extension MUST yield 401"
    );
}

#[test]
fn require_authenticated_rejects_nil_subject_id() {
    let ctx = SecurityContext::builder()
        .subject_id(uuid::Uuid::nil())
        .subject_tenant_id(uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"))
        .subject_type("gts.cf.core.security.subject_user.v1~")
        .build()
        .expect("builder accepts a nil subject id");
    assert!(
        require_authenticated(Some(Extension(ctx))).is_err(),
        "all-zero subject id is the anonymous placeholder \u{2014} MUST yield 401"
    );
}

#[test]
fn require_authenticated_rejects_nil_tenant_id() {
    let ctx = SecurityContext::builder()
        .subject_id(uuid::uuid!("11111111-2222-3333-4444-555555555555"))
        .subject_tenant_id(uuid::Uuid::nil())
        .subject_type("gts.cf.core.security.subject_user.v1~")
        .build()
        .expect("builder accepts a nil tenant id");
    assert!(
        require_authenticated(Some(Extension(ctx))).is_err(),
        "all-zero tenant id is the anonymous placeholder \u{2014} MUST yield 401"
    );
}

#[test]
fn require_authenticated_rejects_absent_subject_type() {
    let ctx = SecurityContext::builder()
        .subject_id(uuid::uuid!("11111111-2222-3333-4444-555555555555"))
        .subject_tenant_id(uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"))
        .build()
        .expect("builder permits an unset subject_type");
    assert!(
        require_authenticated(Some(Extension(ctx))).is_err(),
        "a real AuthN resolver always sets subject_type; its absence MUST yield 401"
    );
}

#[test]
fn require_authenticated_accepts_a_fully_populated_context() {
    let ctx = ctx_with_token_scopes(vec!["rbac:read".to_owned()]);
    assert!(
        require_authenticated(Some(Extension(ctx))).is_ok(),
        "a non-nil subject/tenant with subject_type set MUST authenticate"
    );
}
