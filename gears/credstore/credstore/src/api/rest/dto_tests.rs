//! Unit tests for the credstore REST DTOs.

use credstore_sdk::{
    Credential, CredentialStatus, Fallback, InheritanceStatus, OwnerId, Secret, SecretRef,
    SecretType, SecretValue, SharingMode, Validator,
};
use uuid::Uuid;

use super::*;

fn sref(s: &str) -> SecretRef {
    SecretRef::new(s).expect("valid ref")
}

#[test]
fn sharing_mode_roundtrip() {
    for (dto, sdk) in [
        (SharingModeDto::Private, SharingMode::Private),
        (SharingModeDto::Tenant, SharingMode::Tenant),
        (SharingModeDto::Shared, SharingMode::Shared),
    ] {
        assert_eq!(SharingModeDto::from(sdk), dto);
        assert_eq!(SharingMode::from(dto), sdk);
    }
}

#[test]
fn fallback_roundtrip() {
    for (dto, sdk) in [
        (FallbackDto::Inherit, Fallback::Inherit),
        (FallbackDto::None, Fallback::None),
    ] {
        assert_eq!(FallbackDto::from(sdk), dto);
        assert_eq!(Fallback::from(dto), sdk);
    }
}

#[test]
fn credential_status_and_inheritance_status_convert() {
    assert_eq!(
        CredentialStatusDto::from(CredentialStatus::Active),
        CredentialStatusDto::Active
    );
    assert_eq!(
        CredentialStatusDto::from(CredentialStatus::Declared),
        CredentialStatusDto::Declared
    );
    assert_eq!(
        CredentialStatusDto::from(CredentialStatus::None),
        CredentialStatusDto::None
    );
    assert_eq!(
        InheritanceStatusDto::from(InheritanceStatus::Overridden),
        InheritanceStatusDto::Overridden
    );
    assert_eq!(
        InheritanceStatusDto::from(InheritanceStatus::Suppressed),
        InheritanceStatusDto::Suppressed
    );
}

#[test]
fn put_credential_request_debug_redacts_value() {
    let dto = PutCredentialRequestDto {
        secret_type: None,
        sharing: SharingModeDto::default(),
        fallback: FallbackDto::default(),
        expires_at: None,
        value: Some("super-secret-value".to_owned()),
    };
    let debug = format!("{dto:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("super-secret-value"));
}

#[test]
fn credential_patch_dto_debug_redacts_value_and_shows_tri_state() {
    let absent = CredentialPatchDto {
        secret_type: None,
        sharing: None,
        fallback: None,
        expires_at: None,
        value: None,
    };
    assert!(format!("{absent:?}").contains("<absent>"));

    let null = CredentialPatchDto {
        value: Some(None),
        ..absent.clone()
    };
    assert!(format!("{null:?}").contains("<null>"));

    let set = CredentialPatchDto {
        value: Some(Some("super-secret-value".to_owned())),
        ..absent
    };
    let debug = format!("{set:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("super-secret-value"));
}

#[test]
fn credential_patch_dto_deserializes_absent_null_and_set() {
    let json = r#"{"value": null, "fallback": "none"}"#;
    let dto: CredentialPatchDto = serde_json::from_str(json).expect("deserialize");
    assert_eq!(dto.value, Some(None));
    assert_eq!(dto.fallback, Some(Some(FallbackDto::None)));
    assert_eq!(dto.sharing, None, "omitted field must be Absent (None)");
    assert_eq!(dto.expires_at, None);
    assert_eq!(dto.secret_type, None);

    let json2 = r#"{"value": "rotated"}"#;
    let dto2: CredentialPatchDto = serde_json::from_str(json2).expect("deserialize");
    assert_eq!(dto2.value, Some(Some("rotated".to_owned())));
}

#[test]
fn credential_dto_from_credential_own_row() {
    let owner = Uuid::new_v4();
    let cred = Credential {
        reference: sref("k"),
        secret_type: SecretType::generic().gts_id().to_owned(),
        sharing: SharingMode::Shared,
        fallback: Some(Fallback::Inherit),
        status: CredentialStatus::Active,
        inheritance: InheritanceStatus::Own,
        version: Some(3),
        updated_at: Some(time::OffsetDateTime::now_utc()),
        owner_id: Some(OwnerId(owner)),
        expires_at: None,
        validator: Some(Validator {
            id: Uuid::new_v4(),
            version: 3,
        }),
    };
    let dto = CredentialDto::try_from_credential(&cred).expect("no formatting error");
    assert_eq!(dto.reference, "k");
    assert_eq!(dto.sharing, SharingModeDto::Shared);
    assert_eq!(dto.fallback, Some(FallbackDto::Inherit));
    assert_eq!(dto.status, CredentialStatusDto::Active);
    assert_eq!(dto.inheritance, InheritanceStatusDto::Own);
    assert_eq!(dto.version, Some(3));
    assert!(dto.updated_at.is_some());
    assert_eq!(dto.owner_id, Some(owner.to_string()));

    // The own row's owner id is present on the wire too.
    let json = serde_json::to_value(&dto).expect("serialize");
    assert_eq!(json["owner_id"], owner.to_string());
}

#[test]
fn credential_dto_from_credential_no_own_row() {
    let cred = Credential {
        reference: sref("k"),
        secret_type: SecretType::generic().gts_id().to_owned(),
        sharing: SharingMode::Shared,
        fallback: None,
        status: CredentialStatus::None,
        inheritance: InheritanceStatus::Inherited,
        version: None,
        updated_at: None,
        owner_id: None,
        expires_at: None,
        validator: None,
    };
    let dto = CredentialDto::try_from_credential(&cred).expect("no formatting error");
    assert_eq!(dto.fallback, None);
    assert_eq!(dto.version, None);
    assert_eq!(dto.updated_at, None);
    assert_eq!(dto.status, CredentialStatusDto::None);
    assert_eq!(
        dto.owner_id, None,
        "an inherited record must not carry an ancestor's owner id"
    );

    // Fields absent from Credential must be absent from the wire too.
    let json = serde_json::to_value(&dto).expect("serialize");
    assert!(json.get("fallback").is_none());
    assert!(json.get("version").is_none());
    assert!(json.get("updated_at").is_none());
    assert!(
        json.get("owner_id").is_none(),
        "owner_id key must be absent (not null) on the wire"
    );
}

#[test]
fn secret_dto_debug_redacts_value() {
    let secret = Secret {
        reference: sref("k"),
        secret_type: SecretType::generic().gts_id().to_owned(),
        expires_at: None,
        value: SecretValue::from("super-secret-value"),
        validator: Validator {
            id: Uuid::new_v4(),
            version: 1,
        },
    };
    let dto = SecretDto::try_from_secret(&secret).expect("utf-8 value");
    let debug = format!("{dto:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("super-secret-value"));
    assert_eq!(dto.value, "super-secret-value");
}

#[test]
fn secret_dto_rejects_non_utf8_value() {
    let secret = Secret {
        reference: sref("k"),
        secret_type: SecretType::generic().gts_id().to_owned(),
        expires_at: None,
        value: SecretValue::new(vec![0xff, 0xfe, 0x00]),
        validator: Validator {
            id: Uuid::new_v4(),
            version: 1,
        },
    };
    let err = SecretDto::try_from_secret(&secret)
        .expect_err("non-UTF-8 value must be rejected, not lossily decoded");
    assert!(matches!(
        err,
        crate::domain::error::DomainError::Internal { .. }
    ));
}

#[test]
fn weak_etag_is_deterministic_opaque_and_sensitive_to_its_inputs() {
    let tenant = Uuid::new_v4();
    let winner_id = Uuid::new_v4();
    let a = weak_etag(tenant, "ref", winner_id, 1);
    let b = weak_etag(tenant, "ref", winner_id, 1);
    assert_eq!(a, b, "deterministic");
    assert!(a.starts_with("W/\""));
    assert!(a.ends_with('"'));

    let different_version = weak_etag(tenant, "ref", winner_id, 2);
    assert_ne!(a, different_version, "must change when the winner rotates");

    let different_ref = weak_etag(tenant, "other-ref", winner_id, 1);
    assert_ne!(a, different_ref);

    let different_tenant = weak_etag(Uuid::new_v4(), "ref", winner_id, 1);
    assert_ne!(a, different_tenant);
}
