//! SeaORM-backed implementation of [`RoleDefinitionRepository`].
//!
//! The seeder owns the built-in upsert path; this repo writes **custom**
//! role definitions only. Reads (`find_by_id` / `list` /
//! `count_assignments_for_role`) cover any row.
//!
//! SQLSTATE mapping:
//! * `23505` → constraint introspection: `uq_role_name_per_tenant` →
//!   `NameTaken`, `uq_role_name_builtin` → `NameReservedByBuiltin`.
//! * `23503` on `role_assignments_role_definition_id_fkey` →
//!   `AssignmentsExist`; other FK violations fall through to `Internal`.
//!
//! `update` / `delete` enforce optimistic concurrency by filtering on
//! `Column::UpdatedAt.eq(if_match_updated_at)`; zero affected rows →
//! `StaleEtag`.

use async_trait::async_trait;
use chrono::{SubsecRound, Utc};
#[allow(unused_imports)]
// `PaginatorTrait` is used via the `.count()` method on `EntityTrait::find()`.
use sea_orm::PaginatorTrait;
// sea-query 1.0 moved the inherent `Expr` combinators (`count`, comparisons)
// onto `ExprTrait`, so the trait has to be in scope to build the `COUNT()`
// aggregate behind `count_by_type`.
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, DbErr, EntityTrait, ExprTrait, FromQueryResult,
    QueryFilter, QuerySelect,
};
use serde_json::Value as JsonValue;
use toolkit_db::{
    DBProvider, DbError,
    secure::{
        AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
        SecureUpdateExt,
    },
};
use uuid::Uuid;

use rbac_sdk::models::{PermissionRule, Scope};

use toolkit_db::odata::sea_orm_filter::{LimitCfg, filter_node_to_condition, paginate_odata};
use toolkit_odata::{ODataQuery, Page, SortDir, filter::convert_expr_to_filter_node};

use crate::domain::error::DomainError;
use crate::domain::etag::{Etag, etag_for};
use crate::domain::model::RoleDefinitionModel;
use crate::domain::role_definition_repo::{
    NewRoleDefinition, RoleDefinitionPatch, RoleDefinitionVisibility, RoleTypeCounts,
};
use crate::infra::canonical_mapping::{classify_db_err_to_domain, extract_constraint_hint};
use crate::infra::error_conv::redacted_scope_error;
use crate::infra::odata_normalize::normalize_filter_literals;
use crate::infra::storage::entity::{role_assignment, role_definition};
use crate::infra::storage::odata_mapping::RoleDefinitionODataMapper;
use crate::odata::RoleDefinitionFilterField;

/// Per-endpoint pagination bounds for `GET /rbac/v1/role-definitions`.
const ROLE_DEFINITION_LIMIT_CFG: LimitCfg = LimitCfg {
    default: 50,
    max: 200,
};

/// Upper bound on the id count per `find_by_ids` `IN (...)` query.
/// The id set is derived from a subject's assignments, whose count a
/// caller can influence, so the lookup is chunked to keep each `IN (...)`
/// well under any driver bind-parameter limit and off a pathological plan
/// on the authz hot path.
const FIND_BY_IDS_CHUNK: usize = 500;

/// Constraint names used for SQLSTATE-23505 introspection. Mirror
/// `m20260521_000001_create_role_definitions_table` (`role_definitions` DDL).
const UQ_ROLE_NAME_PER_TENANT: &str = "uq_role_name_per_tenant";
const UQ_ROLE_NAME_BUILTIN: &str = "uq_role_name_builtin";
/// Constraint name used for SQLSTATE-23503 introspection. Mirror
/// `m20260521_000002_create_role_assignments_table` (`role_assignments` DDL).
const FK_ASSIGNMENT_ROLE: &str = "role_assignments_role_definition_id_fkey";

/// Production SeaORM-backed implementation of
/// [`RoleDefinitionRepository`].
#[derive(Clone)]
///
/// Stateless: the executor arrives per call as `db: &C`, so one instance
/// serves both connection-backed reads and transaction-backed writes.
pub struct RoleDefinitionRepository;

/// Translate a `SeaORM` `DbErr` to a typed [`DomainError`].
///
/// Refinement-only call site: the central
/// [`classify_db_err_to_domain`] returns generic AIP-193 variants;
/// this function extracts a [`ConstraintHint`] on the same `DbErr` and
/// refines a generic `AlreadyExists` / `Conflict` into typed RBAC
/// variants when the violated constraint is one of
/// `uq_role_name_per_tenant`, `uq_role_name_builtin`, or
/// `role_assignments_role_definition_id_fkey`. `kind`
/// (`"create"` / `"update"` / `"delete"`) is used for log correlation only and
/// never appears in the public envelope.
///
/// [`ConstraintHint`]: crate::infra::canonical_mapping::ConstraintHint
fn map_db_err(kind: &'static str, err: DbErr) -> DomainError {
    let _ = kind; // generic diagnostic now owned by the classifier; kind reserved for tracing
    let hint = extract_constraint_hint(&err);
    let generic = classify_db_err_to_domain(err);

    match (&generic, &hint) {
        // Uniqueness refinement. Check the more-specific constraint
        // first so the SQLite column-set fallback doesn't mis-route a
        // per-tenant violation (`name` + `owner_tenant_id`) as a
        // built-in violation (`name` only).
        (DomainError::AlreadyExists { .. }, Some(h)) => {
            if h.matches(UQ_ROLE_NAME_PER_TENANT, &["name", "owner_tenant_id"]) {
                DomainError::RoleDefinitionNameTaken {
                    name: h.extract_quoted_value().unwrap_or_default(),
                    owner_tenant_id: None,
                }
            } else if h.matches(UQ_ROLE_NAME_BUILTIN, &["name"]) {
                DomainError::RoleDefinitionNameReservedByBuiltin {
                    name: h.extract_quoted_value().unwrap_or_default(),
                }
            } else {
                // Unattributed unique violation — keep the generic.
                generic
            }
        }
        // FK refinement. Match by the specific referencing column so
        // the SQLite column-set fallback can't accept a future FK on
        // either table.
        (DomainError::Conflict { .. }, Some(h)) => {
            if h.matches(FK_ASSIGNMENT_ROLE, &["role_definition_id"]) {
                // Role id is not in the driver message; the handler
                // attaches it at the call site.
                DomainError::RoleDefinitionAssignmentsExist {
                    role_definition_id: Uuid::nil(),
                }
            } else {
                generic
            }
        }
        _ => generic,
    }
}

/// Read the row's current `ETag` after a lost CAS so the caller can
/// surface `StaleEtag { current_etag }` without forcing the client to
/// run a separate `GET`. Two failure modes are handled distinctly:
///
/// * `Ok(None)` — the row vanished (concurrent delete). Returns
///   `Ok(String::new())` so the caller surfaces `412` with an empty
///   etag and the client follows the normal not-found retry path.
/// * `Err(_)` — secondary DB failure (pool exhaustion, network blip,
///   etc.). Surfaces as `Err(DomainError::*)` via `map_scope_err` so
///   operators see a real `ServiceUnavailable`/`Internal` instead of
///   a misleading `StaleEtag { current_etag: "" }` that pretends the
///   DB is healthy.
async fn fetch_current_etag(db: &impl DBRunner, id: Uuid) -> Result<String, DomainError> {
    match role_definition::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(db)
        .await
    {
        Ok(Some(row)) => Ok(etag_for(row.updated_at, row.id).into_string()),
        Ok(None) => {
            // Row vanished — the CAS-lost handler explicitly anticipates
            // this race ("client takes the normal not-found path on
            // retry"). `debug!` to keep ops timelines unspoiled.
            tracing::debug!(
                target: "rbac.db",
                %id,
                "fetch_current_etag: row vanished during CAS-recovery read",
            );
            Ok(String::new())
        }
        Err(err) => {
            // Never interpolate `ScopeError`'s `Display` — its
            // `Db(DbErr)` variant forwards driver text (`DETAIL`, host,
            // statement fragments). The downstream classifier in
            // `map_scope_err` emits its own structured, redacted log; this
            // warn adds the CAS-recovery context with a redacted summary.
            tracing::warn!(
                target: "rbac.db",
                error = %redacted_scope_error(&err),
                %id,
                "fetch_current_etag: DB error during CAS-recovery read",
            );
            Err(map_scope_err("fetch_current_etag", err))
        }
    }
}

/// Translate a `toolkit_db::secure::ScopeError` (wrapping a `DbErr` or
/// scope-policy failure) into a typed [`DomainError`].
fn map_scope_err(kind: &'static str, err: ScopeError) -> DomainError {
    match err {
        ScopeError::Db(db_err) => map_db_err(kind, db_err),
        other => DomainError::internal(format!(
            "rbac role_definitions {kind}: scope error: {other}"
        )),
    }
}

// `role_definition` is `#[secure(unrestricted)]`: the entity has no
// tenant column the SecureORM filter could narrow against. Every
// `scope_with(&AccessScope::allow_all())` / `scope_unchecked(...)`
// below uses the escape hatch deliberately; tenant isolation is
// enforced upstream via explicit `Column::OwnerTenantId` filters and
// handler-level `caller_scope`. Adding a tenant column later means
// revisiting every call site here.
#[async_trait]
impl crate::domain::role_definition_repo::RoleDefinitionRepository for RoleDefinitionRepository {
    async fn create<C: DBRunner>(
        &self,
        db: &C,
        new: NewRoleDefinition,
    ) -> Result<RoleDefinitionModel, DomainError> {
        let now = Utc::now().trunc_subsecs(6);

        let active = role_definition::ActiveModel {
            id: Set(new.id),
            name: Set(new.name.clone()),
            description: Set(new.description.clone()),
            is_built_in: Set(false),
            permissions: Set(rules_to_jsonb(&new.permissions)),
            not_permissions: Set(rules_to_jsonb(&new.not_permissions)),
            assignable_scopes: Set(scopes_to_jsonb(&new.assignable_scopes)),
            owner_tenant_id: Set(Some(new.owner_tenant_id)),
            created_at: Set(now),
            updated_at: Set(now),
            created_by: Set(new.created_by.clone()),
        };

        let insert = role_definition::Entity::insert(active)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .map_err(|e| {
                DomainError::internal(format!(
                    "create role_definition: apply allow-all scope failed: {e}"
                ))
            })?;

        insert
            .exec(db)
            .await
            .map_err(|err| match map_scope_err("create", err) {
                // DB error doesn't always carry the role name; fall back
                // to the requested name when the helper couldn't extract.
                DomainError::RoleDefinitionNameTaken {
                    name,
                    owner_tenant_id,
                } if name.is_empty() => DomainError::RoleDefinitionNameTaken {
                    name: new.name.clone(),
                    owner_tenant_id: owner_tenant_id.or(Some(new.owner_tenant_id)),
                },
                DomainError::RoleDefinitionNameTaken {
                    name,
                    owner_tenant_id: None,
                } => DomainError::RoleDefinitionNameTaken {
                    name,
                    owner_tenant_id: Some(new.owner_tenant_id),
                },
                DomainError::RoleDefinitionNameReservedByBuiltin { name } if name.is_empty() => {
                    DomainError::RoleDefinitionNameReservedByBuiltin {
                        name: new.name.clone(),
                    }
                }
                other => other,
            })?;

        Ok(RoleDefinitionModel {
            id: new.id,
            name: new.name,
            description: new.description,
            is_built_in: false,
            permissions: new.permissions,
            not_permissions: new.not_permissions,
            assignable_scopes: new.assignable_scopes,
            owner_tenant_id: Some(new.owner_tenant_id),
            created_at: now,
            updated_at: now,
            created_by: new.created_by,
        })
    }

    async fn find_by_id<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<Option<RoleDefinitionModel>, DomainError> {
        let row = role_definition::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(db)
            .await
            .map_err(|err| map_scope_err("find_by_id", err))?;

        row.map(|model| {
            role_definition::entity_to_model(model).map_err(|e| {
                DomainError::internal(format!("find_by_id role_definition: mapping failed: {e}"))
            })
        })
        .transpose()
    }

    async fn find_by_ids<C: DBRunner>(
        &self,
        db: &C,
        ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        // `WHERE id IN ()` is a parse error on every dialect we ship —
        // short-circuit before reaching the DB. Mirrors the empty-list
        // guard in `list` for `ListFilter::CustomForTenants`.
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Query in bounded chunks so the `IN (...)` never grows with
        // an attacker-influenceable id count. See `FIND_BY_IDS_CHUNK`.
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(FIND_BY_IDS_CHUNK) {
            let rows = role_definition::Entity::find()
                .filter(role_definition::Column::Id.is_in(chunk.iter().copied()))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .all(db)
                .await
                .map_err(|err| map_scope_err("find_by_ids", err))?;
            for model in rows {
                out.push(role_definition::entity_to_model(model).map_err(|e| {
                    DomainError::internal(format!(
                        "find_by_ids role_definition: mapping failed: {e}"
                    ))
                })?);
            }
        }
        Ok(out)
    }

    /// SQL-level twin of the trait's in-memory default: the visibility
    /// predicate is pushed into the `WHERE` clause rather than applied to
    /// rows already fetched, so a role the caller may not see never leaves
    /// the database. Shares `visibility_condition` with `list` and
    /// `count_by_type`, which is what keeps "the name shown on an
    /// assignment row" and "the row the catalog will serve you" one
    /// decision instead of three.
    async fn find_by_ids_visible<C: DBRunner>(
        &self,
        db: &C,
        visibility: RoleDefinitionVisibility,
        ids: &[Uuid],
    ) -> Result<Vec<RoleDefinitionModel>, DomainError> {
        // Same two short-circuits `find_by_ids` and `list` make: an empty
        // `IN (...)` is a parse error, and `CustomForTenants(empty)`
        // admits no row at all.
        if ids.is_empty()
            || matches!(
                &visibility,
                RoleDefinitionVisibility::CustomForTenants(tenants) if tenants.is_empty()
            )
        {
            return Ok(Vec::new());
        }
        let condition = visibility_condition(&visibility);
        let mut out = Vec::with_capacity(ids.len());
        // Chunked exactly like `find_by_ids`: the id set comes from a
        // caller-sized page, so the `IN (...)` must not grow with it.
        for chunk in ids.chunks(FIND_BY_IDS_CHUNK) {
            let mut select = role_definition::Entity::find()
                .filter(role_definition::Column::Id.is_in(chunk.iter().copied()));
            if let Some(cond) = condition.clone() {
                select = select.filter(cond);
            }
            let rows = select
                .secure()
                .scope_with(&AccessScope::allow_all())
                .all(db)
                .await
                .map_err(|err| map_scope_err("find_by_ids_visible", err))?;
            for model in rows {
                out.push(role_definition::entity_to_model(model).map_err(|e| {
                    DomainError::internal(format!(
                        "find_by_ids_visible role_definition: mapping failed: {e}"
                    ))
                })?);
            }
        }
        Ok(out)
    }

    async fn list<C: DBRunner>(
        &self,
        db: &C,
        visibility: RoleDefinitionVisibility,
        query: &ODataQuery,
    ) -> Result<Page<RoleDefinitionModel>, DomainError> {
        // `CustomForTenants(empty)` short-circuits before the DB.
        if matches!(
            &visibility,
            RoleDefinitionVisibility::CustomForTenants(tenants) if tenants.is_empty()
        ) {
            return Ok(empty_page());
        }

        // Apply caller-derived visibility before paginate_odata so it
        // composes with the user `$filter` as a single SQL `WHERE`.
        let mut base_select = role_definition::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all());
        if let Some(cond) = visibility_condition(&visibility) {
            base_select = base_select.filter(cond);
        }

        // Lower the user `$filter` AST into a SeaORM `Condition` over
        // this entity's mapped columns.
        //
        // The same literal normalization the assignment list applies. It is
        // generic over the field enum, so this endpoint gets the identical
        // treatment for free: `id` / `owner_tenant_id` accept a quoted UUID
        // as well as a bare one, and a bare UUID on the text `name` field
        // becomes its canonical text instead of a type-mismatch 400.
        if let Some(ast) = query.filter.as_ref() {
            let ast = normalize_filter_literals::<RoleDefinitionFilterField>(ast);
            let node =
                convert_expr_to_filter_node::<RoleDefinitionFilterField>(&ast).map_err(|e| {
                    DomainError::Validation {
                        detail: format!("$filter: {e}"),
                    }
                })?;
            let filter_cond = filter_node_to_condition::<
                RoleDefinitionFilterField,
                RoleDefinitionODataMapper,
            >(&node)
            .map_err(|e| DomainError::Validation {
                detail: format!("$filter: {e}"),
            })?;
            base_select = base_select.filter(filter_cond);
        }

        // Default order (`created_at DESC, id DESC`, hits
        // `idx_role_definitions_created_at_id`) + `$filter` stripping is
        // the shared policy in `odata_err` so it can't drift from the
        // role-assignment list.
        let query_no_filter = crate::infra::odata_err::list_query_with_default_order(query);

        let page = paginate_odata::<
            RoleDefinitionFilterField,
            RoleDefinitionODataMapper,
            role_definition::Entity,
            role_definition::Model,
            _,
            _,
        >(
            base_select,
            db,
            &query_no_filter,
            ("id", SortDir::Desc),
            ROLE_DEFINITION_LIMIT_CFG,
            |m| m,
        )
        .await
        .map_err(crate::infra::odata_err::map_odata_err_to_domain)?;

        let items: Vec<RoleDefinitionModel> = page
            .items
            .into_iter()
            .map(|model| {
                role_definition::entity_to_model(model).map_err(|e| {
                    DomainError::internal(format!("list role_definitions: mapping failed: {e}"))
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn update<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
        patch: RoleDefinitionPatch,
        expected_etag: &Etag,
    ) -> Result<RoleDefinitionModel, DomainError> {
        // Re-read so we can (a) verify the ETag and (b) build the
        // partial UPDATE on top of current state. The CAS in the UPDATE
        // step closes the race with concurrent writers; the returned
        // row is rebuilt from `existing + patch + advanced` to avoid a
        // follow-up SELECT that could observe a third writer.
        let existing = role_definition::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(db)
            .await
            .map_err(|err| map_scope_err("update", err))?
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "update role_definition {id}: row vanished between handler pre-fetch and \
                     repository write"
                ))
            })?;
        let existing_updated_at = existing.updated_at;
        let now_etag = etag_for(existing_updated_at, existing.id);
        if &now_etag != expected_etag {
            return Err(DomainError::StaleEtag {
                current_etag: now_etag.into_string(),
            });
        }

        let advanced = advance_updated_at(existing_updated_at);

        // UPDATE only the changed columns; `UpdatedAt` is always advanced.
        // CAS enforced by filtering on the current `UpdatedAt`.
        let mut active = role_definition::ActiveModel {
            updated_at: Set(advanced),
            ..Default::default()
        };
        if let Some(name) = &patch.name {
            active.name = Set(name.clone());
        }
        if let Some(desc) = &patch.description {
            active.description = Set(desc.clone());
        }
        if let Some(rules) = &patch.permissions {
            active.permissions = Set(rules_to_jsonb(rules));
        }
        if let Some(rules) = &patch.not_permissions {
            active.not_permissions = Set(rules_to_jsonb(rules));
        }
        if let Some(s) = &patch.assignable_scopes {
            active.assignable_scopes = Set(scopes_to_jsonb(s));
        }

        // Precompute post-patch name + owner_tenant_id so a uniqueness
        // violation with a missing `Key (…)=(…)` segment can have the
        // requested values re-injected. Mirrors the `create` fallback.
        let attempted_name = patch.name.clone().unwrap_or_else(|| existing.name.clone());
        let attempted_owner = existing.owner_tenant_id;

        let result = role_definition::Entity::update_many()
            .set(active)
            .filter(role_definition::Column::Id.eq(id))
            .filter(role_definition::Column::UpdatedAt.eq(existing_updated_at))
            // Defence in depth: the service layer already refuses to
            // mutate built-ins, but a future handler that bypassed the
            // service shouldn't be able to corrupt seeder invariants
            // from the repo. A built-in row simply CAS-misses.
            .filter(role_definition::Column::IsBuiltIn.eq(false))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(db)
            .await
            .map_err(|err| match map_scope_err("update", err) {
                DomainError::RoleDefinitionNameTaken {
                    name,
                    owner_tenant_id,
                } if name.is_empty() => DomainError::RoleDefinitionNameTaken {
                    name: attempted_name.clone(),
                    owner_tenant_id: owner_tenant_id.or(attempted_owner),
                },
                DomainError::RoleDefinitionNameTaken {
                    name,
                    owner_tenant_id: None,
                } => DomainError::RoleDefinitionNameTaken {
                    name,
                    owner_tenant_id: attempted_owner,
                },
                DomainError::RoleDefinitionNameReservedByBuiltin { name } if name.is_empty() => {
                    DomainError::RoleDefinitionNameReservedByBuiltin {
                        name: attempted_name.clone(),
                    }
                }
                other => other,
            })?;

        if result.rows_affected == 0 {
            // CAS lost: another writer modified the row between SELECT
            // and UPDATE. Re-fetch so the caller gets the current ETag
            // and can retry without a separate GET. If the row vanished
            // (race with delete), `current_etag` is empty and the client
            // takes the normal not-found path on retry.
            // A secondary DB failure on the recovery read propagates
            // as `ServiceUnavailable`/`Internal` rather than masquerading
            // as a fake-empty-etag `StaleEtag`.
            let current_etag = fetch_current_etag(db, id).await?;
            return Err(DomainError::StaleEtag { current_etag });
        }

        // Build the post-update Model from `existing` + patch + advanced
        // timestamp, avoiding a follow-up SELECT that could observe a
        // third writer's modification.
        let perms_jsonb = match &patch.permissions {
            Some(rules) => rules_to_jsonb(rules),
            None => existing.permissions,
        };
        let not_perms_jsonb = match &patch.not_permissions {
            Some(rules) => rules_to_jsonb(rules),
            None => existing.not_permissions,
        };
        let updated_model = role_definition::Model {
            id: existing.id,
            name: patch.name.clone().unwrap_or(existing.name),
            description: match patch.description {
                Some(d) => d,
                None => existing.description,
            },
            is_built_in: existing.is_built_in,
            permissions: perms_jsonb,
            not_permissions: not_perms_jsonb,
            assignable_scopes: match &patch.assignable_scopes {
                Some(s) => scopes_to_jsonb(s),
                None => existing.assignable_scopes,
            },
            owner_tenant_id: existing.owner_tenant_id,
            created_at: existing.created_at,
            updated_at: advanced,
            created_by: existing.created_by,
        };

        role_definition::entity_to_model(updated_model).map_err(|e| {
            DomainError::internal(format!("update role_definition {id}: mapping failed: {e}"))
        })
    }

    async fn delete<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
        expected_etag: &Etag,
    ) -> Result<(), DomainError> {
        // Same CAS pattern as `update`.
        let existing = role_definition::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(db)
            .await
            .map_err(|err| map_scope_err("delete", err))?
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "delete role_definition {id}: row vanished between handler pre-fetch and \
                     repository write"
                ))
            })?;
        let existing_updated_at = existing.updated_at;
        let now_etag = etag_for(existing_updated_at, existing.id);
        if &now_etag != expected_etag {
            return Err(DomainError::StaleEtag {
                current_etag: now_etag.into_string(),
            });
        }

        let result = role_definition::Entity::delete_many()
            .filter(role_definition::Column::Id.eq(id))
            .filter(role_definition::Column::UpdatedAt.eq(existing_updated_at))
            // Defence in depth: see `update`. Built-ins are seeded and
            // must not be deletable from the repo even if a future
            // handler skips the service-layer guard.
            .filter(role_definition::Column::IsBuiltIn.eq(false))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(db)
            .await
            .map_err(|err| match map_scope_err("delete", err) {
                DomainError::RoleDefinitionAssignmentsExist { .. } => {
                    DomainError::RoleDefinitionAssignmentsExist {
                        role_definition_id: id,
                    }
                }
                other => other,
            })?;

        if result.rows_affected == 0 {
            // CAS lost — same rationale as `update`.
            // A secondary DB failure on the recovery read propagates
            // as `ServiceUnavailable`/`Internal` rather than masquerading
            // as a fake-empty-etag `StaleEtag`.
            let current_etag = fetch_current_etag(db, id).await?;
            return Err(DomainError::StaleEtag { current_etag });
        }
        Ok(())
    }

    async fn count_assignments_for_role<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<u64, DomainError> {
        let count = role_assignment::Entity::find()
            .filter(role_assignment::Column::RoleDefinitionId.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .count(db)
            .await
            .map_err(|err| map_scope_err("count_assignments_for_role", err))?;
        Ok(count)
    }

    async fn count_by_type<C: DBRunner>(
        &self,
        db: &C,
        visibility: RoleDefinitionVisibility,
    ) -> Result<RoleTypeCounts, DomainError> {
        /// One `GROUP BY is_built_in` row. `c` is `i64` because that is what
        /// `COUNT()` yields on both backends we ship.
        #[derive(FromQueryResult)]
        struct TypeGroup {
            is_built_in: bool,
            c: i64,
        }

        // `CustomForTenants(empty)` admits no rows; short-circuit before the
        // DB exactly as `list` does, so an operator reading the two side by
        // side sees the same behaviour and not an `IN ()` syntax error.
        if matches!(
            &visibility,
            RoleDefinitionVisibility::CustomForTenants(tenants) if tenants.is_empty()
        ) {
            return Ok(RoleTypeCounts::default());
        }

        let mut select = role_definition::Entity::find();
        if let Some(cond) = visibility_condition(&visibility) {
            select = select.filter(cond);
        }
        let groups: Vec<TypeGroup> = select
            .secure()
            .scope_with(&AccessScope::allow_all())
            .project_all(db, |q| {
                q.select_only()
                    .column(role_definition::Column::IsBuiltIn)
                    .column_as(Expr::col(role_definition::Column::Id).count(), "c")
                    .group_by(role_definition::Column::IsBuiltIn)
                    .into_model::<TypeGroup>()
            })
            .await
            .map_err(|err| map_scope_err("count_by_type", err))?;

        let mut counts = RoleTypeCounts::default();
        for group in groups {
            // A `COUNT` is never negative; `try_from` keeps the conversion
            // lossless-by-construction rather than an `as` cast that would
            // silently wrap.
            let n = u64::try_from(group.c).unwrap_or(0);
            if group.is_built_in {
                counts.built_in = n;
            } else {
                counts.custom = n;
            }
        }
        // A bucket the `GROUP BY` produced no row for is genuinely zero: the
        // predicate admitted no rows of that kind. `RoleTypeCounts::default()`
        // already carries the zeros, so nothing to do.
        Ok(counts)
    }
}

/// Lower a [`RoleDefinitionVisibility`] into the SQL predicate that admits
/// exactly the rows the caller may read, or `None` when the variant narrows
/// nothing.
///
/// The single owner of that translation, shared by `list` and
/// `count_by_type`. The summary endpoint exists to label the rows the list
/// returns, so a second copy of this match is the one thing that would make
/// the badge and the page disagree.
fn visibility_condition(visibility: &RoleDefinitionVisibility) -> Option<Condition> {
    match visibility {
        RoleDefinitionVisibility::BuiltinsOnly => {
            Some(Condition::all().add(role_definition::Column::IsBuiltIn.eq(true)))
        }
        RoleDefinitionVisibility::CustomForTenants(tenants) => Some(
            Condition::all()
                .add(role_definition::Column::IsBuiltIn.eq(false))
                .add(role_definition::Column::OwnerTenantId.is_in(tenants.iter().copied())),
        ),
        // Built-ins (always visible) OR custom rows owned by a readable
        // tenant. An empty tenant set collapses to built-ins only.
        RoleDefinitionVisibility::CustomForTenantsWithBuiltins(tenants) => Some(
            Condition::any()
                .add(role_definition::Column::IsBuiltIn.eq(true))
                .add(
                    Condition::all()
                        .add(role_definition::Column::IsBuiltIn.eq(false))
                        .add(role_definition::Column::OwnerTenantId.is_in(tenants.iter().copied())),
                ),
        ),
        // No narrowing — the caller has unrestricted read. A user `$filter`
        // may still narrow further on the list path.
        RoleDefinitionVisibility::All => None,
    }
}

/// Count all role definitions + role assignments platform-wide
/// (`AccessScope::allow_all`) for the `rbac_role_definitions` /
/// `rbac_role_assignments` inventory gauges. A free function (not a
/// `*Repository` trait method) so the periodic refresher in module
/// init can call it over a plain `DBProvider` without widening the
/// repo traits + their mocks. Returns `(role_definitions, role_assignments)`.
pub(crate) async fn count_role_inventory(
    provider: &DBProvider<DbError>,
) -> Result<(u64, u64), DomainError> {
    let db = provider.conn()?;
    let db = &db;
    let role_definitions = role_definition::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .count(db)
        .await
        .map_err(|err| map_scope_err("count_role_definitions", err))?;
    let role_assignments = role_assignment::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .count(db)
        .await
        .map_err(|err| map_scope_err("count_role_assignments", err))?;
    Ok((role_definitions, role_assignments))
}

/// Serialise a slice of [`PermissionRule`]s as JSONB
/// `[{ operation, target_type }, …]` for either column. No per-rule
/// effect tag — the column encodes the rule's class.
fn rules_to_jsonb(rules: &[PermissionRule]) -> JsonValue {
    JsonValue::Array(
        rules
            .iter()
            .map(|rule| {
                serde_json::json!({
                    "operation": rule.operation,
                    "target_type": rule.target_type,
                })
            })
            .collect(),
    )
}

fn scopes_to_jsonb(values: &[Scope]) -> JsonValue {
    JsonValue::Array(values.iter().map(|s| JsonValue::String(s.path())).collect())
}

fn advance_updated_at(previous: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    let candidate = Utc::now().trunc_subsecs(6);
    if candidate > previous {
        candidate
    } else {
        previous + chrono::Duration::microseconds(1)
    }
}

/// Empty `Page` used by the short-circuit branches of `list`.
fn empty_page<T>() -> Page<T> {
    Page {
        items: Vec::new(),
        page_info: toolkit_odata::PageInfo {
            next_cursor: None,
            prev_cursor: None,
            limit: 0,
        },
    }
}

#[cfg(test)]
#[path = "role_definition_repo_tests.rs"]
mod role_definition_repo_tests;
