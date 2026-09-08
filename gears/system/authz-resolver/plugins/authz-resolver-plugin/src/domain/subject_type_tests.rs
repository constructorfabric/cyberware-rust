use super::*;

#[test]
fn raw_idp_claim_values_classify() {
    assert_eq!(classify_subject_type("user"), Some(PrincipalType::User));
    assert_eq!(
        classify_subject_type("service"),
        Some(PrincipalType::ServicePrincipal)
    );
    assert_eq!(
        classify_subject_type("service_principal"),
        Some(PrincipalType::ServicePrincipal)
    );
}

#[test]
fn gts_tags_classify_regardless_of_vendor_segment() {
    // The match is on the `subject_*` substring, not on the full identifier, so
    // a NON-`cf` vendor segment and a bumped version must classify the same.
    // The list previously repeated the same `gts.cf` literal twice, which meant
    // a classifier that hard-coded the `cf` vendor would have passed.
    for tag in [
        "gts.cf.core.security.subject_user.v1~",
        "gts.x.core.security.subject_user.v1~",
        "gts.acme.identity.subject_user.v1~",
        "gts.cf.core.security.subject_user.v2~",
    ] {
        assert_eq!(
            classify_subject_type(tag),
            Some(PrincipalType::User),
            "{tag}"
        );
    }
    for tag in [
        "gts.cf.core.security.subject_service_principal.v1~",
        "gts.x.core.security.subject_service_principal.v1~",
        "gts.acme.identity.subject_service_principal.v2~",
    ] {
        assert_eq!(
            classify_subject_type(tag),
            Some(PrincipalType::ServicePrincipal),
            "{tag}"
        );
    }
}

#[test]
fn raw_match_is_exact_so_super_user_is_not_user() {
    // Guard against the substring trap: "super_user" contains "user" but is
    // not the raw `user` claim and carries no `subject_*` tag → unrecognized.
    assert_eq!(classify_subject_type("super_user"), None);
}

#[test]
fn groups_and_unknown_values_are_rejected() {
    assert_eq!(classify_subject_type("group"), None);
    assert_eq!(
        classify_subject_type("gts.cf.core.security.subject_group.v1~"),
        None
    );
    assert_eq!(classify_subject_type("definitely.not.a.real.type"), None);
    assert_eq!(classify_subject_type(""), None);
}

// ------------ Trusted system-actor pairs ------------

use crate::test_support::trusted_actors::{
    AM_SYSTEM_ACTOR_UUID, AM_SYSTEM_SUBJECT_TYPE, RMS_SYSTEM_ACTOR_UUID, RMS_SYSTEM_SUBJECT_TYPE,
    trusted_actors,
};

#[test]
fn a_configured_pair_is_trusted() {
    let trusted = trusted_actors();
    assert!(trusted.matches(RMS_SYSTEM_ACTOR_UUID, Some(RMS_SYSTEM_SUBJECT_TYPE)));
    assert!(trusted.matches(AM_SYSTEM_ACTOR_UUID, Some(AM_SYSTEM_SUBJECT_TYPE)));
    assert_eq!(trusted.len(), 2);
}

#[test]
fn the_type_alone_is_not_enough() {
    assert!(!trusted_actors().matches(uuid::Uuid::new_v4(), Some(RMS_SYSTEM_SUBJECT_TYPE)));
}

#[test]
fn the_id_alone_is_not_enough() {
    let trusted = trusted_actors();
    assert!(!trusted.matches(RMS_SYSTEM_ACTOR_UUID, Some("user")));
    assert!(!trusted.matches(RMS_SYSTEM_ACTOR_UUID, None));
}

/// A cross-pair combination (one entry's id with another's type) is NOT
/// trusted: both halves must come from the same configured entry.
#[test]
fn cross_pair_combinations_are_not_trusted() {
    let trusted = trusted_actors();
    assert!(!trusted.matches(AM_SYSTEM_ACTOR_UUID, Some(RMS_SYSTEM_SUBJECT_TYPE)));
    assert!(!trusted.matches(RMS_SYSTEM_ACTOR_UUID, Some(AM_SYSTEM_SUBJECT_TYPE)));
}

/// The default is empty: a deployment that configures nothing trusts nothing.
#[test]
fn the_default_set_trusts_nobody() {
    let empty = TrustedSystemActors::default();
    assert_eq!(empty.len(), 0);
    assert!(!empty.matches(AM_SYSTEM_ACTOR_UUID, Some(AM_SYSTEM_SUBJECT_TYPE)));
}
