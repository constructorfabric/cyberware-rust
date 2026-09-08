use rbac_sdk::models::PrincipalType;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::principal_type_from_security_context;

fn ctx_with_subject_type(subject_type: Option<&str>) -> SecurityContext {
    let mut b = SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x1111_1111))
        .subject_tenant_id(Uuid::from_u128(0x2222_2222));
    if let Some(s) = subject_type {
        b = b.subject_type(s);
    }
    b.build().unwrap()
}

#[test]
fn maps_subject_user_gts() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(Some(
            "gts.cf.core.security.subject_user.v1~"
        )))
        .expect("known subject_type maps cleanly"),
        PrincipalType::User
    );
}

#[test]
fn maps_subject_service_gts() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(Some(
            "gts.cf.core.security.subject_service.v1~"
        )))
        .expect("known subject_type maps cleanly"),
        PrincipalType::ServicePrincipal
    );
}

#[test]
fn maps_subject_group_gts() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(Some(
            "gts.cf.core.security.subject_group.v1~"
        )))
        .expect("known subject_type maps cleanly"),
        PrincipalType::Group
    );
}

#[test]
fn defaults_to_user_when_subject_type_is_absent() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(None))
            .expect("absent subject_type stays Ok(User)"),
        PrincipalType::User
    );
}

#[test]
fn rejects_unknown_subject_type() {
    let result = principal_type_from_security_context(&ctx_with_subject_type(Some(
        "gts.cf.core.security.subject_something_else.v1~",
    )));
    assert!(
        matches!(
            result,
            Err(crate::domain::error::DomainError::Validation { .. })
        ),
        "unknown subject_type MUST surface as Validation, not silent User fallback; \
         got {result:?}"
    );
}

// ---------------------------------------------------------------
// Raw IdP claim values from keycloak-authn / static-authn.
// The substring matcher only accepted GTS tags; live deployments
// were blocked because the plugins pass the raw `user_type` /
// `service` claim through unchanged.
// ---------------------------------------------------------------

#[test]
fn maps_raw_user_claim_from_keycloak_authn() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(Some("user")))
            .expect("raw \"user\" claim must classify as User"),
        PrincipalType::User
    );
}

#[test]
fn maps_raw_service_claim_from_static_authn() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(Some("service")))
            .expect("raw \"service\" claim must classify as ServicePrincipal"),
        PrincipalType::ServicePrincipal
    );
}

#[test]
fn maps_raw_service_principal_alias() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(Some("service_principal")))
            .expect("raw \"service_principal\" alias must classify as ServicePrincipal"),
        PrincipalType::ServicePrincipal
    );
}

#[test]
fn maps_raw_group_claim() {
    assert_eq!(
        principal_type_from_security_context(&ctx_with_subject_type(Some("group")))
            .expect("raw \"group\" claim must classify as Group"),
        PrincipalType::Group
    );
}

/// Raw matching uses exact equality, not substring — so a string
/// that merely *contains* `"user"` MUST NOT classify as User on
/// the raw branch. (It also doesn't match the GTS substring
/// because it lacks `"subject_user"`, so the whole call rejects.)
#[test]
fn rejects_substring_collision_super_user() {
    let result = principal_type_from_security_context(&ctx_with_subject_type(Some("super_user")));
    assert!(
        matches!(
            result,
            Err(crate::domain::error::DomainError::Validation { .. })
        ),
        "raw matcher MUST use exact equality so unrelated strings containing \
         \"user\" don't silently classify as User; got {result:?}"
    );
}
