//! Tests for [`super::RbacServiceConfig`].
//!
//! Two groups, with different failure modes.
//!
//! The built-in role targets and the boot-time grant lists are what a
//! deployment points at its own vendor and its own principals, and a malformed
//! entry there has to fail `init()` rather than seed a role that authorizes
//! nothing or a grant that dangles.
//!
//! Display-name resolution is the opposite shape: the *defaults* must be safe
//! on a cluster that has never heard of the feature, and an *override* that
//! inverts its cost model must be refused loudly instead of quietly turning a
//! batched read into an N+1. The override is the one that matters in
//! production — nobody ever ships the defaults broken, and a default-only
//! assertion never touches the value an operator actually typed into `config:`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// The default targets are the Constructor Fabric families, so a `cf`
/// platform needs no configuration and keeps today's behaviour.
#[test]
fn default_builtin_role_targets_are_the_cf_families() {
    let t = BuiltinRoleTargets::default();
    assert_eq!(t.platform, vec!["gts.cf.*".to_owned()]);
    assert_eq!(t.resources_family, vec!["gts.cf.resources.*".to_owned()]);
    assert!(RbacServiceConfig::default().validate().is_ok());
}

/// The slot resolver is what lets a fork point the built-ins at its own
/// vendor without touching the catalog.
#[test]
// These are permission *target* strings — family wildcards are exactly what
// the matcher consumes — but DE0901 only recognises the wildcard form in
// contexts it can name, so the literals below need the allow.
#[allow(unknown_lints, de0901_gts_string_pattern)]
fn targets_resolve_slots_and_pass_fixed_ones_through() {
    let t = BuiltinRoleTargets {
        platform: vec!["gts.vendor.*".to_owned()],
        resources_family: vec!["gts.vendor.resources.*".to_owned()],
    };
    assert_eq!(t.resolve(TargetSpec::Platform), vec!["gts.vendor.*"]);
    assert_eq!(
        t.resolve(TargetSpec::ResourcesFamily),
        vec!["gts.vendor.resources.*"]
    );
    assert_eq!(
        t.resolve(TargetSpec::Fixed("gts.cf.core.rbac.role_definition.v1~")),
        vec!["gts.cf.core.rbac.role_definition.v1~"],
        "a target the gear owns is never re-pointed by config"
    );
}

/// The reason the settings are lists: a fork needs its own family *and*
/// `gts.cf.*` for the platform's internal types, RBAC's own included.
#[test]
#[allow(unknown_lints, de0901_gts_string_pattern)]
fn a_slot_resolves_to_every_configured_target_in_order() {
    let t = BuiltinRoleTargets {
        platform: vec!["gts.cf.*".to_owned(), "gts.vendor.*".to_owned()],
        ..BuiltinRoleTargets::default()
    };
    assert_eq!(
        t.resolve(TargetSpec::Platform),
        vec!["gts.cf.*", "gts.vendor.*"],
        "order follows the config list so the seeded rule order is stable"
    );
    assert!(
        RbacServiceConfig {
            builtin_role_targets: t,
            ..RbacServiceConfig::default()
        }
        .validate()
        .is_ok()
    );
}

#[test]
#[allow(unknown_lints, de0901_gts_string_pattern)]
fn validate_accepts_wildcards_and_concrete_type_ids() {
    for value in [
        "gts.vendor.*",
        "gts.vendor.resources.*",
        "gts.cf.core.rbac.role_definition.v1~",
    ] {
        let cfg = RbacServiceConfig {
            builtin_role_targets: BuiltinRoleTargets {
                platform: vec![value.to_owned()],
                ..BuiltinRoleTargets::default()
            },
            ..RbacServiceConfig::default()
        };
        assert!(cfg.validate().is_ok(), "{value} should be accepted");
    }
}

#[test]
#[allow(unknown_lints, de0901_gts_string_pattern)]
fn validate_rejects_malformed_role_targets() {
    // Not a GTS id; the bare wildcard; an instance id where a type is
    // required; a wildcard with no separator kept.
    for value in [
        "vendor.*",
        "gts.*",
        "gts.cf.core.rbac.role_definition.v1",
        "gts.vendor*",
    ] {
        let cfg = RbacServiceConfig {
            builtin_role_targets: BuiltinRoleTargets {
                resources_family: vec![value.to_owned()],
                ..BuiltinRoleTargets::default()
            },
            ..RbacServiceConfig::default()
        };
        let err = cfg
            .validate()
            .expect_err("a malformed target must fail init");
        assert!(
            err.to_string().contains("resources_family"),
            "the error must name the field for {value}: {err}"
        );
    }
}

/// A malformed entry anywhere in the list fails, and the message says
/// which one — a list makes "the third entry has a typo" a real case.
#[test]
#[allow(unknown_lints, de0901_gts_string_pattern)]
fn validate_names_the_offending_list_entry() {
    let cfg = RbacServiceConfig {
        builtin_role_targets: BuiltinRoleTargets {
            platform: vec!["gts.cf.*".to_owned(), "gts.vendor*".to_owned()],
            ..BuiltinRoleTargets::default()
        },
        ..RbacServiceConfig::default()
    };
    let err = cfg.validate().expect_err("a malformed entry must fail");
    assert!(
        err.to_string().contains("platform[1]"),
        "the error must name the index: {err}"
    );
}

/// An empty list would seed a built-in role with no rules at all: it would
/// exist, assign cleanly, and authorize nothing.
#[test]
fn validate_rejects_empty_target_lists() {
    for (field, targets) in [
        (
            "platform",
            BuiltinRoleTargets {
                platform: Vec::new(),
                ..BuiltinRoleTargets::default()
            },
        ),
        (
            "resources_family",
            BuiltinRoleTargets {
                resources_family: Vec::new(),
                ..BuiltinRoleTargets::default()
            },
        ),
    ] {
        let err = RbacServiceConfig {
            builtin_role_targets: targets,
            ..RbacServiceConfig::default()
        }
        .validate()
        .expect_err("an empty target list must fail init");
        assert!(err.to_string().contains(field), "{err}");
    }
}

/// A fresh config grants nothing and seeds no integration roles: every
/// deployment-specific privilege must be asked for explicitly.
#[test]
fn default_config_grants_nothing() {
    let cfg = RbacServiceConfig::default();
    assert!(cfg.service_principal_grants.is_empty());
    assert!(
        cfg.user_grants.is_empty(),
        "a user grant is a privilege; RBAC MUST never invent one"
    );
    assert!(
        !cfg.seed_integration_roles,
        "roles targeting other gears MUST be opt-in"
    );
    assert!(cfg.validate().is_ok());
}

fn grant(role: &str, principal: &str) -> RbacServiceConfig {
    RbacServiceConfig {
        service_principal_grants: vec![RoleGrant {
            role: role.to_owned(),
            principal_id: principal.to_owned(),
        }],
        ..RbacServiceConfig::default()
    }
}

fn user_grant(role: &str, principal: &str) -> RbacServiceConfig {
    RbacServiceConfig {
        user_grants: vec![RoleGrant {
            role: role.to_owned(),
            principal_id: principal.to_owned(),
        }],
        ..RbacServiceConfig::default()
    }
}

#[test]
fn validate_accepts_a_core_role_grant() {
    assert!(grant("Owner", "svc-1").validate().is_ok());
    assert!(user_grant("Reader", "user-1").validate().is_ok());
}

#[test]
fn validate_rejects_blank_fields() {
    for (role, principal, field) in [("", "svc-1", "role"), ("Owner", "  ", "principal_id")] {
        let err = grant(role, principal)
            .validate()
            .expect_err("a blank field must fail init");
        assert!(
            err.to_string().contains(field),
            "the error must name {field}: {err}"
        );
    }
}

/// Both lists get the same validation, and the message names the list the
/// operator actually wrote — otherwise a `user_grants` typo would be
/// reported against `service_principal_grants`.
#[test]
fn validate_names_the_grant_list_that_failed() {
    let err = user_grant("No Such Role", "user-1")
        .validate()
        .expect_err("unknown role must fail init");
    assert!(err.to_string().contains("user_grants"), "{err}");
    assert!(
        !err.to_string().contains("service_principal_grants"),
        "the error must not blame the other list: {err}"
    );

    let err = grant("", "svc-1")
        .validate()
        .expect_err("a blank field must fail init");
    assert!(
        err.to_string().contains("service_principal_grants"),
        "{err}"
    );
}

/// A grant naming an integration role while the roles are gated off would
/// point at a role definition that never gets seeded — refuse it rather
/// than write a dangling privilege.
#[test]
fn validate_rejects_a_grant_for_an_unseeded_integration_role() {
    let cfg = grant("Credstore Secret Operator", "svc-1");
    let err = cfg.validate().expect_err("ungated role must fail init");
    assert!(err.to_string().contains("seed_integration_roles"), "{err}");

    let enabled = RbacServiceConfig {
        seed_integration_roles: true,
        ..cfg
    };
    assert!(
        enabled.validate().is_ok(),
        "the same grant MUST pass once the role is seeded"
    );
}

#[test]
fn validate_rejects_an_unknown_role_name() {
    let err = grant("No Such Role", "svc-1")
        .validate()
        .expect_err("unknown role must fail init");
    assert!(err.to_string().contains("No Such Role"), "{err}");
}

/// Defaults must be safe on a cluster that has never heard of this
/// feature: on, a short TTL so a rename shows up quickly, and a
/// bounded membership pass so a large tenant cannot make a listing
/// expensive.
#[test]
fn principal_names_defaults_are_conservative() {
    let c = RbacServiceConfig::default().principal_names;
    assert!(c.enabled);
    assert_eq!(c.cache_ttl(), std::time::Duration::from_secs(30));
    assert_eq!(c.max_pages_per_tenant, 5);
    assert!(c.cache_max_entries >= 1000);
    assert!(
        c.max_point_lookups_per_tenant > 0,
        "a zero budget would make a truncated pass unable to name anything"
    );
    assert!(
        c.max_lookup_tenants_per_request > 0,
        "a zero tenant cap would resolve no user names at all"
    );
    assert_eq!(c.resolve_timeout(), std::time::Duration::from_secs(5));
}

/// The defaults are what an unconfigured cluster runs, so they must be
/// the one input `validate` can never reject.
#[test]
fn defaults_validate() {
    assert!(RbacServiceConfig::default().validate().is_ok());
}

/// `Default::default()` MUST yield `None` so operators cannot
/// accidentally pick up a phantom admin ID.
#[test]
fn default_platform_admin_subject_id_is_none() {
    assert!(
        RbacServiceConfig::default()
            .platform_admin_subject_id
            .is_none(),
        "platform_admin_subject_id MUST default to None - it must NEVER \
         have a literal default in source code"
    );
}

/// A zero page budget is the worst of the zero values: it looks like
/// "spend less", and it turns every read into one full Keycloak
/// membership drain per principal — the exact N+1 the pass exists to
/// prevent.
#[test]
fn zero_page_budget_is_rejected() {
    let cfg = PrincipalNamesConfig {
        max_pages_per_tenant: 0,
        ..PrincipalNamesConfig::default()
    };
    let err = cfg
        .validate()
        .expect_err("a zero page budget MUST be refused");
    assert!(
        err.contains("max_pages_per_tenant"),
        "message names the field: {err}"
    );
    // The fail-safe accessor keeps a value that slipped past validation
    // from reaching the pass loop as "run no pass at all".
    assert_eq!(cfg.pages_per_tenant(), 1);
}

/// A zero cache bound makes `remember` clear the cache on every insert,
/// so nothing is ever served from memory.
#[test]
fn zero_cache_bound_is_rejected() {
    let cfg = PrincipalNamesConfig {
        cache_max_entries: 0,
        ..PrincipalNamesConfig::default()
    };
    let err = cfg
        .validate()
        .expect_err("a zero cache bound MUST be refused");
    assert!(
        err.contains("cache_max_entries"),
        "message names the field: {err}"
    );
    assert_eq!(cfg.cache_capacity(), 1);
}

/// The remaining zero values each disable naming while looking like a
/// tuning knob, so each is refused by name.
#[test]
fn other_zero_values_are_rejected_by_name() {
    for (cfg, field) in [
        (
            PrincipalNamesConfig {
                cache_ttl_seconds: 0,
                ..PrincipalNamesConfig::default()
            },
            "cache_ttl_seconds",
        ),
        (
            PrincipalNamesConfig {
                max_point_lookups_per_tenant: 0,
                ..PrincipalNamesConfig::default()
            },
            "max_point_lookups_per_tenant",
        ),
        (
            PrincipalNamesConfig {
                max_lookup_tenants_per_request: 0,
                ..PrincipalNamesConfig::default()
            },
            "max_lookup_tenants_per_request",
        ),
        (
            PrincipalNamesConfig {
                resolve_timeout_ms: 0,
                ..PrincipalNamesConfig::default()
            },
            "resolve_timeout_ms",
        ),
    ] {
        let err = cfg.validate().expect_err("a zero value MUST be refused");
        assert!(err.contains(field), "message names the field: {err}");
    }
}

/// A disabled feature resolves nothing, so its bounds are inert and an
/// operator must not be blocked from leaving values parked there.
#[test]
fn disabled_config_ignores_its_bounds() {
    let cfg = PrincipalNamesConfig {
        enabled: false,
        cache_max_entries: 0,
        max_pages_per_tenant: 0,
        resolve_timeout_ms: 0,
        ..PrincipalNamesConfig::default()
    };
    assert!(cfg.validate().is_ok());
}
