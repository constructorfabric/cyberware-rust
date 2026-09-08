//! Tests for caller-authority → `CallerScope` derivation.
//!
//! Moved here with the code: the decision is an authorization concern, not
//! a transport one. The `require_authenticated` tests stayed in
//! `api/rest/auth_context_tests.rs`, which is where extraction lives.

use super::{caller_scope_from_context, is_first_party_root};
use crate::domain::role_definition::CallerScope;
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
fn non_root_caller_is_not_first_party_root() {
    let ctx = ctx_with_token_scopes(vec!["rbac:read".to_owned()]);
    assert!(
        !is_first_party_root(&ctx),
        "tenant-bound caller (no `*` token scope) MUST NOT be flagged as first-party root"
    );
}

#[test]
fn non_root_caller_resolves_to_tenant_scope() {
    let ctx = ctx_with_token_scopes(vec!["rbac:write".to_owned()]);
    assert_eq!(
        caller_scope_from_context(&ctx),
        CallerScope::Tenant(ctx.subject_tenant_id())
    );
}

#[test]
fn first_party_root_caller_resolves_to_root_scope() {
    let ctx = ctx_with_token_scopes(vec!["*".to_owned()]);
    assert_eq!(caller_scope_from_context(&ctx), CallerScope::Root);
}

/// Empty `token_scopes` MUST NOT read as "unrestricted".
///
/// `is_first_party_root` requires `len() == 1`, so this holds today — but no
/// test covered it, and rewriting the predicate as
/// `scopes.iter().any(|s| s == "*")` or adding an `is_empty()` short-circuit
/// would have kept every existing test green while handing an unscoped token
/// platform root.
#[test]
fn empty_token_scopes_are_not_first_party_root() {
    let ctx = ctx_with_token_scopes(vec![]);
    assert!(
        !is_first_party_root(&ctx),
        "an empty token_scopes MUST NOT be treated as unrestricted"
    );
    assert_eq!(
        caller_scope_from_context(&ctx),
        CallerScope::Tenant(ctx.subject_tenant_id()),
        "an unscoped caller stays bound to its own tenant"
    );
}

/// `"*"` MIXED with other scopes MUST NOT read as root either: the wildcard is
/// a whole-token marker, so a token that also carries narrow scopes has not
/// been granted first-party authority.
#[test]
fn wildcard_mixed_with_other_scopes_is_not_first_party_root() {
    for scopes in [
        vec!["*".to_owned(), "rbac:read".to_owned()],
        vec!["rbac:read".to_owned(), "*".to_owned()],
    ] {
        let ctx = ctx_with_token_scopes(scopes.clone());
        assert!(
            !is_first_party_root(&ctx),
            "`*` alongside {scopes:?} MUST NOT be treated as unrestricted"
        );
        assert_eq!(
            caller_scope_from_context(&ctx),
            CallerScope::Tenant(ctx.subject_tenant_id()),
            "a mixed-scope caller stays bound to its own tenant; scopes={scopes:?}"
        );
    }
}
