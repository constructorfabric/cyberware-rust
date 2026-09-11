//! Unit tests for [`reduce_reference`] — the reduction rules ADR-0005
//! restates from `resolve_credential` and requires the collection read to
//! reuse verbatim.

use credstore_sdk::{InheritanceStatus, OwnerId, SecretType, SharingMode, TenantId, ValueId};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{Reduced, reduce_reference};
use crate::domain::secret::model::{Fallback, SecretRow, SecretStatus};

#[allow(
    clippy::too_many_arguments,
    reason = "test fixture builder; one call site per scenario reads better with named fields \
              flattened than with a builder pattern for a handful of tests"
)]
fn row(
    tenant: Uuid,
    reference: &str,
    sharing: SharingMode,
    status: SecretStatus,
    fallback: Fallback,
) -> SecretRow {
    SecretRow {
        id: Uuid::new_v4(),
        tenant_id: TenantId(tenant),
        reference: reference.to_owned(),
        sharing,
        owner_id: OwnerId(Uuid::new_v4()),
        status,
        version: 1,
        updated_at: OffsetDateTime::now_utc(),
        secret_type_uuid: SecretType::generic().uuid(),
        expires_at: None,
        value_id: if status == SecretStatus::Active {
            Some(ValueId::new_v4())
        } else {
            None
        },
        value_fp: if status == SecretStatus::Active {
            Some(vec![1u8; 32])
        } else {
            None
        },
        fp_key_id: if status == SecretStatus::Active {
            Some(1)
        } else {
            None
        },
        fallback,
    }
}

fn reduce<'a>(
    candidates: &'a [SecretRow],
    own_tenant: Uuid,
    chain: &[Uuid],
) -> Option<Reduced<'a>> {
    reduce_reference(
        candidates,
        TenantId(own_tenant),
        OwnerId(Uuid::new_v4()),
        chain,
    )
}

#[test]
fn empty_candidates_reduce_to_none() {
    let own = Uuid::new_v4();
    assert!(reduce(&[], own, &[own]).is_none());
}

#[test]
fn own_active_row_alone_is_own() {
    let own = Uuid::new_v4();
    let candidates = [row(
        own,
        "r",
        SharingMode::Tenant,
        SecretStatus::Active,
        Fallback::Inherit,
    )];
    let reduced = reduce(&candidates, own, &[own]).expect("reduces");
    assert!(reduced.own.is_some());
    assert_eq!(reduced.winner.map(|w| w.id), reduced.own.map(|o| o.id));
    assert_eq!(reduced.inheritance, InheritanceStatus::Own);
}

#[test]
fn ancestor_shared_row_with_no_own_row_is_inherited() {
    let own = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let candidates = [row(
        parent,
        "r",
        SharingMode::Shared,
        SecretStatus::Active,
        Fallback::Inherit,
    )];
    let reduced = reduce(&candidates, own, &[own, parent]).expect("reduces");
    assert!(reduced.own.is_none());
    assert!(reduced.winner.is_some());
    assert_eq!(reduced.inheritance, InheritanceStatus::Inherited);
}

#[test]
fn own_active_row_shadowing_an_ancestor_is_overridden() {
    let own = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let candidates = [
        row(
            own,
            "r",
            SharingMode::Tenant,
            SecretStatus::Active,
            Fallback::Inherit,
        ),
        row(
            parent,
            "r",
            SharingMode::Shared,
            SecretStatus::Active,
            Fallback::Inherit,
        ),
    ];
    let reduced = reduce(&candidates, own, &[own, parent]).expect("reduces");
    assert_eq!(reduced.own.map(|o| o.tenant_id.0), Some(own));
    // Own tenant is nearer in the chain, so it wins the reduction outright.
    assert_eq!(reduced.winner.map(|w| w.tenant_id.0), Some(own));
    assert_eq!(reduced.inheritance, InheritanceStatus::Overridden);
}

#[test]
fn own_declared_inherit_row_never_competes_and_ancestor_wins_inherited() {
    let own = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let candidates = [
        row(
            own,
            "r",
            SharingMode::Tenant,
            SecretStatus::Declared,
            Fallback::Inherit,
        ),
        row(
            parent,
            "r",
            SharingMode::Shared,
            SecretStatus::Active,
            Fallback::Inherit,
        ),
    ];
    let reduced = reduce(&candidates, own, &[own, parent]).expect("reduces");
    // The own row exists (status: declared) but is not the winner: a
    // declared/inherit row must not shadow a resolvable inherited ancestor.
    assert_eq!(reduced.own.map(|o| o.status), Some(SecretStatus::Declared));
    assert_eq!(reduced.winner.map(|w| w.tenant_id.0), Some(parent));
    assert_eq!(reduced.inheritance, InheritanceStatus::Inherited);
}

#[test]
fn own_declared_none_row_wins_as_suppressed_even_over_a_resolvable_ancestor() {
    let own = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let candidates = [
        row(
            own,
            "r",
            SharingMode::Tenant,
            SecretStatus::Declared,
            Fallback::None,
        ),
        row(
            parent,
            "r",
            SharingMode::Shared,
            SecretStatus::Active,
            Fallback::Inherit,
        ),
    ];
    let reduced = reduce(&candidates, own, &[own, parent]).expect("reduces");
    assert_eq!(reduced.winner.map(|w| w.tenant_id.0), Some(own));
    assert_eq!(reduced.inheritance, InheritanceStatus::Suppressed);
}

#[test]
fn own_declared_inherit_row_alone_with_nothing_behind_it_is_own() {
    let own = Uuid::new_v4();
    let candidates = [row(
        own,
        "r",
        SharingMode::Tenant,
        SecretStatus::Declared,
        Fallback::Inherit,
    )];
    let reduced = reduce(&candidates, own, &[own]).expect("reduces");
    assert!(reduced.winner.is_none());
    assert!(reduced.own.is_some());
    assert_eq!(reduced.inheritance, InheritanceStatus::Own);
}

#[test]
fn private_beats_tenant_at_the_same_own_tenant() {
    let own = Uuid::new_v4();
    let candidates = [
        row(
            own,
            "r",
            SharingMode::Tenant,
            SecretStatus::Active,
            Fallback::Inherit,
        ),
        row(
            own,
            "r",
            SharingMode::Private,
            SecretStatus::Active,
            Fallback::Inherit,
        ),
    ];
    let reduced = reduce(&candidates, own, &[own]).expect("reduces");
    assert_eq!(reduced.own.map(|o| o.sharing), Some(SharingMode::Private));
}

#[test]
fn expired_active_row_does_not_compete_as_winner() {
    let own = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let mut expired_own = row(
        own,
        "r",
        SharingMode::Tenant,
        SecretStatus::Active,
        Fallback::Inherit,
    );
    expired_own.expires_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(5));
    let candidates = [
        expired_own,
        row(
            parent,
            "r",
            SharingMode::Shared,
            SecretStatus::Active,
            Fallback::Inherit,
        ),
    ];
    let reduced = reduce(&candidates, own, &[own, parent]).expect("reduces");
    // The expired own row still counts as `own` (it holds the reference),
    // but it cannot win the reduction, so the ancestor's row does.
    assert_eq!(reduced.own.map(|o| o.tenant_id.0), Some(own));
    assert_eq!(reduced.winner.map(|w| w.tenant_id.0), Some(parent));
    assert_eq!(reduced.inheritance, InheritanceStatus::Inherited);
}

#[test]
fn own_status_reflects_the_callers_own_row_only() {
    use credstore_sdk::CredentialStatus;

    let own = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let candidates = [row(
        parent,
        "r",
        SharingMode::Shared,
        SecretStatus::Active,
        Fallback::Inherit,
    )];
    let reduced = reduce(&candidates, own, &[own, parent]).expect("reduces");
    assert_eq!(reduced.own_status(), CredentialStatus::None);

    let candidates = [row(
        own,
        "r",
        SharingMode::Tenant,
        SecretStatus::Declared,
        Fallback::Inherit,
    )];
    let reduced = reduce(&candidates, own, &[own]).expect("reduces");
    assert_eq!(reduced.own_status(), CredentialStatus::Declared);
}
