//! Docker-free unit tests for [`super::authorize_caller`] — the in-process
//! trust gate on `dyn RbacServiceClientV1`.
//!
//! The gate decides anonymous rejection, cross-tenant rejection and Root
//! escalation, and its only coverage used to be `tests/postgres_local_client.rs`
//! — every case behind `#[ignore]` and a Postgres container. That is a real
//! suite (CI runs it via `make test-rbac-pg`), but it means a broken caller
//! check does not fail the default `cargo nextest run`, and a developer gets no
//! signal without Docker. `authorize_caller` is a pure function of
//! `(SecurityContext, Scope)`, so it needs neither.

use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::Scope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::authorize_caller;
use crate::domain::role_definition::CallerScope;

/// An ordinary tenant-scoped caller: authenticated, no wildcard token scope.
fn tenant_ctx(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .subject_type("user")
        .build()
        .expect("an authenticated tenant ctx must build")
}

/// A first-party root caller — identified by `token_scopes == ["*"]`, never by
/// an absent tenant.
fn root_ctx(home_tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(home_tenant)
        .subject_type("service")
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("a root ctx must build")
}

fn assert_denied(result: Result<CallerScope, RbacServiceError>, case: &str) {
    match result {
        Err(RbacServiceError::AuthorizationDenied { .. }) => {}
        other => panic!("{case} MUST be AuthorizationDenied, got {other:?}"),
    }
}

#[test]
fn anonymous_caller_is_rejected() {
    // `SecurityContext::anonymous()` carries nil ids, which is the shape an
    // unauthenticated in-process call arrives with. It must never reach the
    // evaluator, whatever scope it asks for.
    let anonymous = SecurityContext::anonymous();
    for scope in [
        Scope::Root,
        Scope::tenant(Uuid::new_v4()),
        Scope::resource_group(Uuid::new_v4(), Uuid::new_v4()),
    ] {
        assert_denied(
            authorize_caller(&anonymous, &scope),
            &format!("an anonymous caller asking for {scope:?}"),
        );
    }
}

#[test]
fn caller_with_a_nil_tenant_is_rejected() {
    // A non-nil subject with a nil tenant is still not an authenticated
    // caller: the tenant is what the scope check is measured against.
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::nil())
        .subject_type("user")
        .build()
        .expect("builder accepts a nil tenant; the gate is what rejects it");
    assert_denied(
        authorize_caller(&ctx, &Scope::tenant(Uuid::new_v4())),
        "a caller with a nil subject_tenant_id",
    );
}

#[test]
fn tenant_caller_may_address_its_own_tenant() {
    let tenant = Uuid::new_v4();
    let caller = authorize_caller(&tenant_ctx(tenant), &Scope::tenant(tenant))
        .expect("a tenant caller MUST reach its own tenant");
    assert_eq!(caller, CallerScope::Tenant(tenant));
}

#[test]
fn tenant_caller_may_address_a_resource_group_under_its_own_tenant() {
    let tenant = Uuid::new_v4();
    let scope = Scope::resource_group(tenant, Uuid::new_v4());
    let caller = authorize_caller(&tenant_ctx(tenant), &scope)
        .expect("an RG under the caller's own tenant MUST be reachable");
    assert_eq!(caller, CallerScope::Tenant(tenant));
}

#[test]
fn tenant_caller_cannot_address_another_tenant() {
    let caller_tenant = Uuid::new_v4();
    let other_tenant = Uuid::new_v4();
    let ctx = tenant_ctx(caller_tenant);

    assert_denied(
        authorize_caller(&ctx, &Scope::tenant(other_tenant)),
        "a cross-tenant Scope::Tenant",
    );
    // The RG form must not be a way around the tenant check: the gate compares
    // the RG's OWNING tenant, not just the variant.
    assert_denied(
        authorize_caller(&ctx, &Scope::resource_group(other_tenant, Uuid::new_v4())),
        "a cross-tenant Scope::ResourceGroup",
    );
}

#[test]
fn tenant_caller_cannot_escalate_to_root_scope() {
    // The escalation case: `Scope::Root` is the platform-wide scope, and a
    // tenant-bound caller asking for it must be refused rather than silently
    // narrowed to its own tenant.
    let tenant = Uuid::new_v4();
    assert_denied(
        authorize_caller(&tenant_ctx(tenant), &Scope::Root),
        "a tenant caller asking for Scope::Root",
    );
}

#[test]
fn root_caller_may_address_any_scope() {
    let home = Uuid::new_v4();
    let ctx = root_ctx(home);
    for scope in [
        Scope::Root,
        Scope::tenant(home),
        Scope::tenant(Uuid::new_v4()),
        Scope::resource_group(Uuid::new_v4(), Uuid::new_v4()),
    ] {
        let caller = authorize_caller(&ctx, &scope)
            .unwrap_or_else(|err| panic!("root MUST reach {scope:?}, got {err:?}"));
        assert_eq!(caller, CallerScope::Root, "scope={scope:?}");
    }
}

#[test]
fn root_authority_comes_from_the_wildcard_scope_not_from_the_subject_type() {
    // A `service` subject_type without `token_scopes == ["*"]` is an ordinary
    // tenant caller. Deriving root from the subject type instead would hand
    // every service principal platform-wide reach.
    let tenant = Uuid::new_v4();
    let service_but_scoped = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .subject_type("service")
        .token_scopes(vec!["rbac:read".to_owned()])
        .build()
        .expect("ctx must build");
    assert_denied(
        authorize_caller(&service_but_scoped, &Scope::Root),
        "a scoped service principal asking for Scope::Root",
    );
}
