//! Unit tests for the domain error ladder.

use license_resolver_sdk::field;

use super::*;

fn violation() -> FieldViolation {
    FieldViolation::new(
        field::SUBJECT_TYPE_FIELD,
        "not registered",
        field::CONTRACT_NOT_REGISTERED,
    )
}

#[test]
fn contract_violations_reach_the_caller_intact() {
    let err = DomainError::ContractViolation {
        violations: vec![violation()],
    };
    let mapped: LicenseResolverError = err.into();
    let LicenseResolverError::InvalidRequest { violations } = mapped else {
        panic!("a contract violation must surface as InvalidRequest, not as a decision");
    };
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].reason, field::CONTRACT_NOT_REGISTERED);
    assert_eq!(violations[0].field, field::SUBJECT_TYPE_FIELD);
}

/// An unreachable catalog must surface as unavailable rather than as an internal
/// fault: the caller fails closed either way, but only one is worth retrying.
#[test]
fn unreachable_registry_surfaces_as_unavailable() {
    let mapped: LicenseResolverError =
        DomainError::TypesRegistryUnavailable("connection refused".to_owned()).into();
    assert!(
        matches!(mapped, LicenseResolverError::ServiceUnavailable(_)),
        "expected ServiceUnavailable, got: {mapped:?}"
    );
}

#[test]
fn domain_errors_map_onto_their_sdk_variants() {
    let cases: Vec<(DomainError, &str)> = vec![
        (
            DomainError::PluginNotFound {
                vendor: "acme".to_owned(),
            },
            "NoPluginAvailable",
        ),
        (
            DomainError::PluginUnavailable {
                gts_id: "gts.x".to_owned(),
                reason: "timeout".to_owned(),
            },
            "ServiceUnavailable",
        ),
        (
            DomainError::InvalidPluginInstance {
                gts_id: "gts.x".to_owned(),
                reason: "bad json".to_owned(),
            },
            "Internal",
        ),
        (
            DomainError::ContractUnusable {
                type_id: "gts.x~".to_owned(),
                reason: "not a schema".to_owned(),
            },
            "Internal",
        ),
        (DomainError::Unauthorized, "Unauthorized"),
        (DomainError::Internal("boom".to_owned()), "Internal"),
    ];
    for (err, expected) in cases {
        let mapped: LicenseResolverError = err.into();
        let actual = match mapped {
            LicenseResolverError::NoPluginAvailable => "NoPluginAvailable",
            LicenseResolverError::ServiceUnavailable(_) => "ServiceUnavailable",
            LicenseResolverError::Internal(_) => "Internal",
            LicenseResolverError::Unauthorized => "Unauthorized",
            other => panic!("unexpected mapping: {other:?}"),
        };
        assert_eq!(actual, expected);
    }
}

/// A catalog that cannot be used is the operator's problem, so the caller must
/// not be told its request was invalid.
#[test]
fn unusable_contract_is_never_reported_as_a_bad_request() {
    let mapped: LicenseResolverError = DomainError::ContractUnusable {
        type_id: "gts.cf.core.lic.res.v1~x.y.z.a.v1~".to_owned(),
        reason: "trait `admitted_subjects` is not an array".to_owned(),
    }
    .into();
    assert!(
        !matches!(mapped, LicenseResolverError::InvalidRequest { .. }),
        "catalog drift must not be attributed to the caller: {mapped:?}"
    );
}

#[test]
fn plugin_errors_carry_the_instance_that_produced_them() {
    let instance =
        "gts.cf.toolkit.plugins.plugin.v1~cf.core.license_resolver.plugin.v1~acme.x.y.z.v1";
    let err = DomainError::from_plugin(
        instance,
        LicenseResolverError::ServiceUnavailable("backend timeout".to_owned()),
    );
    let DomainError::PluginUnavailable { gts_id, reason } = &err else {
        panic!("expected PluginUnavailable, got: {err:?}");
    };
    assert_eq!(gts_id, instance);
    assert_eq!(reason, "backend timeout");
}

#[test]
fn plugin_unauthorized_is_propagated_unchanged() {
    let err = DomainError::from_plugin("gts.x", LicenseResolverError::Unauthorized);
    assert!(matches!(err, DomainError::Unauthorized), "got: {err:?}");
    let mapped: LicenseResolverError = err.into();
    assert!(matches!(mapped, LicenseResolverError::Unauthorized));
}

/// Only the gateway can be missing a plugin, so a backend claiming it is a
/// protocol violation rather than this gear's own no-plugin state.
#[test]
fn plugin_reporting_no_plugin_available_is_internal() {
    let err = DomainError::from_plugin("gts.x", LicenseResolverError::NoPluginAvailable);
    assert!(
        matches!(err, DomainError::Internal(ref msg) if msg.contains("gts.x")),
        "expected Internal naming the instance, got: {err:?}"
    );
}

#[test]
fn choose_plugin_failures_map_onto_the_domain() {
    let not_found = DomainError::from(toolkit::plugins::ChoosePluginError::PluginNotFound {
        type_id: "gts.x~".to_owned(),
        vendor: "acme".to_owned(),
    });
    assert!(
        matches!(not_found, DomainError::PluginNotFound { ref vendor } if vendor == "acme"),
        "got: {not_found:?}"
    );

    let invalid = DomainError::from(toolkit::plugins::ChoosePluginError::InvalidPluginInstance {
        gts_id: "gts.x".to_owned(),
        reason: "missing vendor".to_owned(),
    });
    assert!(
        matches!(invalid, DomainError::InvalidPluginInstance { .. }),
        "got: {invalid:?}"
    );
}

#[test]
fn plugin_internal_reason_is_kept_verbatim() {
    let err = DomainError::from_plugin(
        "gts.x",
        LicenseResolverError::Internal("backend bug".to_owned()),
    );
    assert!(
        matches!(err, DomainError::Internal(ref msg) if msg == "backend bug"),
        "got: {err:?}"
    );
}
