//! Read-only repo methods: `resolve_for_get`, `find_own`, `find_for_write`,
//! `scope_includes_tenant`.

use credstore_sdk::{OwnerId, SecretRef, SharingMode, TenantId};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QuerySelect};
use toolkit_db::secure::{ScopeError, SecureEntityExt};
use toolkit_security::access_scope::ScopeFilter;
use toolkit_security::{AccessScope, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::secret::model::{Fallback, SecretRow, SecretStatus};
use crate::infra::canonical_mapping::classify_db_err_to_domain;
use crate::infra::storage::entity;
use crate::infra::storage::repo_impl::helpers::{
    SecretRepoImpl, entity_to_model, map_scope_err, sharing_to_i16,
};

fn scope_err_to_domain(e: ScopeError) -> DomainError {
    match e {
        ScopeError::Db(db) => classify_db_err_to_domain(db),
        other => map_scope_err(other),
    }
}

/// Resolution-eligible statuses (ADR-0004, Suppression): `active` and not
/// expired, or `declared` with `fallback = none` (a suppressing row that
/// competes and blocks regardless of any stale `expires_at` it carries — a
/// suppression policy does not expire). A `declared`/`inherit` row is
/// excluded by construction — it is simply not one of these two `(status,
/// fallback)` shapes.
fn resolution_eligible_condition() -> Condition {
    Condition::any()
        .add(
            Condition::all()
                .add(entity::secrets::Column::Status.eq(SecretStatus::Active.as_smallint()))
                .add(
                    Condition::any()
                        .add(entity::secrets::Column::ExpiresAt.is_null())
                        .add(
                            entity::secrets::Column::ExpiresAt.gt(time::OffsetDateTime::now_utc()),
                        ),
                ),
        )
        .add(
            Condition::all()
                .add(entity::secrets::Column::Status.eq(SecretStatus::Declared.as_smallint()))
                .add(entity::secrets::Column::Fallback.eq(Fallback::None.as_smallint())),
        )
}

/// Sharing-visibility predicate for `tenant`'s own rows: private rows only
/// for `subject`, tenant/shared rows visible to the whole tenant. Shared by
/// [`resolve_for_get`], [`resolve_candidates`] and (indirectly) `find_own`.
fn own_tenant_visibility_condition(tenant: Uuid, subject: Uuid) -> Condition {
    Condition::any()
        .add(
            Condition::all()
                .add(entity::secrets::Column::Sharing.eq(sharing_to_i16(SharingMode::Private)))
                .add(entity::secrets::Column::TenantId.eq(tenant))
                .add(entity::secrets::Column::OwnerId.eq(subject)),
        )
        .add(
            Condition::all()
                .add(entity::secrets::Column::Sharing.is_in([
                    sharing_to_i16(SharingMode::Tenant),
                    sharing_to_i16(SharingMode::Shared),
                ]))
                .add(entity::secrets::Column::TenantId.eq(tenant)),
        )
}

pub(super) async fn resolve_for_get(
    repo: &SecretRepoImpl,
    req_tenant: TenantId,
    subject: OwnerId,
    key: &SecretRef,
    chain: &[Uuid],
) -> Result<Option<SecretRow>, DomainError> {
    let conn = repo.db.conn()?;
    let req = req_tenant.0;
    // resolve_for_get applies its own chain + sharing predicates;
    // PDP authorization runs upstream. allow_all skips the scope WHERE clamp.
    // The predicate is `status = active OR (status = declared AND fallback =
    // none)` (ADR-0004, Suppression): a suppressing row competes and, when
    // nearest, wins — the walk stops there rather than falling through.
    let rows = entity::secrets::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(entity::secrets::Column::Reference.eq(key.as_ref())))
        // Expired `active` secrets resolve as not-found (write paths still
        // see the row: overwrite refreshes it, delete revokes it, the
        // maintenance job sweeps it); embedded in the eligibility condition
        // so it never weakens `declared`/`none` suppression.
        .filter(resolution_eligible_condition())
        .filter(Condition::all().add(entity::secrets::Column::TenantId.is_in(chain.to_vec())))
        // Visibility by sharing class, within the ancestor `chain`:
        //   * Private — own tenant + owner only (`tenant_id == req AND
        //     owner_id == subject`). Private is owner-scoped per DESIGN §4.3 and
        //     never inherited, so we pin it to `req` here rather than lean on
        //     the authn-layer invariant that a `subject_id` belongs to a single
        //     tenant. That keeps disclosure closed even if a `subject_id` is
        //     ever reused across tenants or a cross-tenant principal is
        //     introduced (matches `find_own`/`find_for_write`, which already
        //     pin the tenant). `Shared` remains the sole inheritance vector.
        //   * Shared — inherited down the chain (any tenant in `chain`).
        //   * Tenant — own tenant only (`tenant_id == req`), never inherited.
        .filter(
            Condition::any()
                .add(
                    Condition::all()
                        .add(
                            entity::secrets::Column::Sharing
                                .eq(sharing_to_i16(SharingMode::Private)),
                        )
                        .add(entity::secrets::Column::TenantId.eq(req))
                        .add(entity::secrets::Column::OwnerId.eq(subject.0)),
                )
                .add(entity::secrets::Column::Sharing.eq(sharing_to_i16(SharingMode::Shared)))
                .add(
                    Condition::all()
                        .add(
                            entity::secrets::Column::Sharing
                                .eq(sharing_to_i16(SharingMode::Tenant)),
                        )
                        .add(entity::secrets::Column::TenantId.eq(req)),
                ),
        )
        .all(&conn)
        .await
        .map_err(scope_err_to_domain)?;

    // Winner: closest tenant in chain; private beats non-private at same level.
    let pos = |t: Uuid| chain.iter().position(|c| *c == t).unwrap_or(usize::MAX);
    let best = rows.into_iter().min_by(|a, b| {
        pos(a.tenant_id).cmp(&pos(b.tenant_id)).then(
            (a.sharing != sharing_to_i16(SharingMode::Private))
                .cmp(&(b.sharing != sharing_to_i16(SharingMode::Private))),
        )
    });
    best.map(entity_to_model).transpose()
}

pub(super) async fn resolve_candidates(
    repo: &SecretRepoImpl,
    req_tenant: TenantId,
    subject: OwnerId,
    key: &SecretRef,
    chain: &[Uuid],
) -> Result<Vec<SecretRow>, DomainError> {
    let conn = repo.db.conn()?;
    let req = req_tenant.0;
    let ancestors: Vec<Uuid> = chain.iter().copied().filter(|t| *t != req).collect();

    // Own tenant: every sharing-visible row, any status (so the record view
    // can report a `declared` own row's `status`/`fallback`/validator even
    // though it never resolves). Ancestors: `shared` rows only, and only
    // those that pass the resolution predicate — an ancestor's
    // `declared`/`inherit` row is invisible here exactly as it is to a value
    // read (ADR-0004, ADR-0005 "Reducing a reference to one item").
    let mut visibility = Condition::any().add(
        Condition::all()
            .add(entity::secrets::Column::TenantId.eq(req))
            .add(own_tenant_visibility_condition(req, subject.0)),
    );
    if !ancestors.is_empty() {
        visibility = visibility.add(
            Condition::all()
                .add(entity::secrets::Column::TenantId.is_in(ancestors))
                .add(entity::secrets::Column::Sharing.eq(sharing_to_i16(SharingMode::Shared)))
                .add(resolution_eligible_condition()),
        );
    }

    let rows = entity::secrets::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(entity::secrets::Column::Reference.eq(key.as_ref())))
        .filter(Condition::all().add(entity::secrets::Column::TenantId.is_in(chain.to_vec())))
        .filter(visibility)
        .all(&conn)
        .await
        .map_err(scope_err_to_domain)?;

    rows.into_iter().map(entity_to_model).collect()
}

pub(super) async fn find_own(
    repo: &SecretRepoImpl,
    scope: &AccessScope,
    tenant: TenantId,
    subject: OwnerId,
    key: &SecretRef,
) -> Result<Option<SecretRow>, DomainError> {
    let conn = repo.db.conn()?;
    // Active or declared — there is no delete saga to resume any more
    // (ADR-0006: `delete_by_id` is one transaction), but a `declared` row is
    // a legitimate "own record" `patch`/`delete` must be able to find
    // (ADR-0004).
    let rows = entity::secrets::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(entity::secrets::Column::Reference.eq(key.as_ref()))
                .add(entity::secrets::Column::TenantId.eq(tenant.0))
                .add(entity::secrets::Column::Status.is_in([
                    SecretStatus::Active.as_smallint(),
                    SecretStatus::Declared.as_smallint(),
                ]))
                .add(
                    Condition::any()
                        .add(
                            Condition::all()
                                .add(
                                    entity::secrets::Column::Sharing
                                        .eq(sharing_to_i16(SharingMode::Private)),
                                )
                                .add(entity::secrets::Column::OwnerId.eq(subject.0)),
                        )
                        .add(entity::secrets::Column::Sharing.is_in([
                            sharing_to_i16(SharingMode::Tenant),
                            sharing_to_i16(SharingMode::Shared),
                        ])),
                ),
        )
        .all(&conn)
        .await
        .map_err(scope_err_to_domain)?;

    // Prefer the private row if both exist.
    let best = rows
        .into_iter()
        .min_by_key(|r| i32::from(r.sharing != sharing_to_i16(SharingMode::Private)));
    best.map(entity_to_model).transpose()
}

pub(super) async fn find_for_write(
    repo: &SecretRepoImpl,
    scope: &AccessScope,
    tenant: TenantId,
    subject: OwnerId,
    key: &SecretRef,
    sharing: SharingMode,
) -> Result<Option<SecretRow>, DomainError> {
    let conn = repo.db.conn()?;
    // Address the row by the target sharing class only — the same identity the
    // partial unique indexes enforce. A private write must NOT match a coexisting
    // tenant/shared row (and vice-versa); they are distinct secrets per design.
    let class = if sharing == SharingMode::Private {
        Condition::all()
            .add(entity::secrets::Column::Sharing.eq(sharing_to_i16(SharingMode::Private)))
            .add(entity::secrets::Column::OwnerId.eq(subject.0))
    } else {
        Condition::all().add(entity::secrets::Column::Sharing.is_in([
            sharing_to_i16(SharingMode::Tenant),
            sharing_to_i16(SharingMode::Shared),
        ]))
    };
    let row = entity::secrets::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(entity::secrets::Column::Reference.eq(key.as_ref()))
                .add(entity::secrets::Column::TenantId.eq(tenant.0))
                .add(entity::secrets::Column::Status.is_in([
                    SecretStatus::Active.as_smallint(),
                    SecretStatus::Declared.as_smallint(),
                ]))
                .add(class),
        )
        .one(&conn)
        .await
        .map_err(scope_err_to_domain)?;
    row.map(entity_to_model).transpose()
}

pub(super) async fn list_expired(
    repo: &SecretRepoImpl,
    limit: u64,
) -> Result<Vec<SecretRow>, DomainError> {
    let conn = repo.db.conn()?;
    let rows = entity::secrets::Entity::find()
        .filter(
            Condition::all()
                .add(entity::secrets::Column::Status.eq(SecretStatus::Active.as_smallint()))
                .add(entity::secrets::Column::ExpiresAt.is_not_null())
                .add(entity::secrets::Column::ExpiresAt.lte(time::OffsetDateTime::now_utc())),
        )
        .limit(limit)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .map_err(scope_err_to_domain)?;
    rows.into_iter().map(entity_to_model).collect()
}

pub(super) fn scope_includes_tenant(scope: &AccessScope, tenant: Uuid) -> bool {
    if scope.is_unconstrained() {
        return true;
    }
    if scope.is_deny_all() {
        return false;
    }
    // Fail-closed tenant-membership check. A scope's constraints are OR-ed
    // (alternative grants) and the filters within a constraint are AND-ed, so
    // a constraint admits `tenant` only when *every* one of its filters is a
    // tenant-level predicate on `OWNER_TENANT_ID` satisfied by `tenant`. Any
    // sibling filter that narrows below tenant granularity (`owner_id`,
    // `resource_id`, group membership, …) or any filter this gate cannot
    // evaluate makes the whole constraint non-admitting. That way a scope
    // stricter than tenant granularity fails closed (403) instead of being
    // silently widened to the whole tenant on a lone `OWNER_TENANT_ID` match.
    'constraints: for constraint in scope.constraints() {
        // An empty constraint matches everything (mirrors SecureORM's
        // `build_constraint_condition`, which compiles it to `WHERE true`).
        for filter in constraint.filters() {
            // Only `OWNER_TENANT_ID` predicates can affirm tenant-level access.
            if filter.property() != pep_properties::OWNER_TENANT_ID {
                continue 'constraints;
            }
            let admits = match filter {
                ScopeFilter::Eq(_) | ScopeFilter::In(_) => {
                    filter.values().iter().any(|v| v.as_uuid() == Some(tenant))
                }
                // Fail closed on everything structured. Credstore advertises
                // no PDP capabilities, so subtree grants arrive pre-expanded
                // as flat `Eq`/`In` predicates (AUTHZ_USAGE_SCENARIOS
                // S09–S11) — the gear projects no `tenant_closure` and cannot
                // resolve a structured subtree predicate; receiving one is a
                // capability-contract breach. Group membership over
                // `OWNER_TENANT_ID` is likewise not a plain tenant predicate
                // this gate resolves.
                ScopeFilter::InTenantSubtree(_)
                | ScopeFilter::InGroup(_)
                | ScopeFilter::InGroupSubtree(_) => false,
            };
            if !admits {
                continue 'constraints;
            }
        }
        // Every filter affirmed `tenant` (or the constraint was empty).
        return true;
    }
    false
}
