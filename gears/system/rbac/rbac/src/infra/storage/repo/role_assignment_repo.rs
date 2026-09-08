//! SeaORM-backed implementation of [`RoleAssignmentRepository`].
//!
//! SQLSTATE mapping:
//! * `23505` on `uq_assignment` → [`DomainError::RoleAssignmentDuplicate`].
//! * `23503` on `role_assignments_role_definition_id_fkey` →
//!   [`DomainError::RoleDefinitionMissing`] (handler upgrades to
//!   `RoleDefinitionNotFound`).
//!
//! `role_assignments` is create-and-delete only — no PATCH path. The
//! strong `ETag` from `find_by_id` is invariant for the row's lifetime
//! (`updated_at == created_at`).

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{SubsecRound, Utc};
// sea-query 1.0 moved the inherent `Expr` combinators (`and`, `or`, `count`,
// comparisons) onto `ExprTrait`, so the trait has to be in scope to chain
// conditions and to build the `COUNT()` aggregate.
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DbErr, EntityTrait, ExprTrait, FromQueryResult, QueryFilter,
    QueryOrder, QuerySelect,
};
use toolkit_db::{
    odata::sea_orm_filter::{LimitCfg, filter_node_to_condition, paginate_odata},
    secure::{
        AccessScope, DBRunner, ScopeError, SecureDeleteExt, SecureEntityExt, SecureInsertExt,
    },
};
use toolkit_odata::{ODataQuery, Page, SortDir, filter::convert_expr_to_filter_node};
use uuid::Uuid;

use rbac_sdk::models::PrincipalType;

use crate::domain::error::DomainError;
use crate::domain::model::RoleAssignmentModel;
use crate::domain::role_assignment_repo::{
    NewRoleAssignment, SubjectAssignmentsQuery, VisibilityFilter,
};
use crate::infra::canonical_mapping::{classify_db_err_to_domain, extract_constraint_hint};
use crate::infra::odata_normalize::normalize_filter_literals;
use crate::infra::storage::entity::role_assignment;
use crate::infra::storage::odata_mapping::RoleAssignmentODataMapper;
use crate::odata::RoleAssignmentFilterField;

/// Per-endpoint pagination bounds for `GET /rbac/v1/role-assignments`.
/// The same 50 / 200 bounds as the role-definition list's `DEFAULT_LIMIT` /
/// `MAX_LIMIT`.
const ROLE_ASSIGNMENT_LIMIT_CFG: LimitCfg = LimitCfg {
    default: 50,
    max: 200,
};

/// `uq_assignment (role_definition_id, principal_type, principal_id, scope)`
/// — `m20260521_000002_create_role_assignments_table`.
const UQ_ASSIGNMENT: &str = "uq_assignment";
/// `role_assignments_role_definition_id_fkey` — `m20260521_000002_create_role_assignments_table`.
const FK_ASSIGNMENT_ROLE: &str = "role_assignments_role_definition_id_fkey";

/// Upper bound on the `group_principals` count per `IN (...)` query.
/// A subject's group set is attacker-influenceable (membership count), so
/// an uncapped `IN (...)` on the authz hot path risks the driver
/// bind-parameter limit / a pathological plan — the same hazard
/// `find_by_ids` chunks at 500. Above this, the query is split into
/// chunks whose row sets are unioned.
const GROUP_PRINCIPALS_CHUNK: usize = 500;

/// Upper bound on the role-id count per `count_by_role` `IN (...)` query.
/// Same rationale and same bound as [`GROUP_PRINCIPALS_CHUNK`] and
/// `role_definition_repo::FIND_BY_IDS_CHUNK`: the id list is derived from a
/// caller-sized page, so the `IN (...)` is kept well under any driver
/// bind-parameter limit rather than growing with the request.
const ROLE_ID_CHUNK: usize = 500;

/// Production SeaORM-backed implementation of [`RoleAssignmentRepository`].
#[derive(Clone)]
///
/// Stateless: the executor arrives per call as `db: &C`, so one instance
/// serves both connection-backed reads and transaction-backed writes.
pub struct RoleAssignmentRepository;

/// Lift the entity-adjacent mapping error to a domain-level diagnostic.
/// Every `RoleAssignmentMappingError` variant indicates a corrupted-row
/// condition: writes validate the canonical scope and principal type, and the
/// denormalized query columns must agree with that canonical scope.
fn entity_to_model(model: role_assignment::Model) -> Result<RoleAssignmentModel, DomainError> {
    role_assignment::entity_to_model(model)
        .map_err(|err| DomainError::internal(format!("role_assignments mapping: {err}")))
}

/// Multi-row variant of [`entity_to_model`]: a single corrupted or
/// legacy row must not turn an entire `list` / `get_subject_assignments`
/// page into a 500 — which would deny a subject *all* of their roles
/// because of one unparseable or internally inconsistent row. Log the
/// offending row id and the mapping error, then drop just that row so the rest of the page (and
/// the subject's other grants) still surface. Single-row reads
/// (`find_by_id`) keep the fail-fast [`entity_to_model`] so a direct
/// lookup of a corrupt row stays an honest error rather than a silent 404.
fn entity_to_model_skip_corrupt(model: role_assignment::Model) -> Option<RoleAssignmentModel> {
    let id = model.id;
    match role_assignment::entity_to_model(model) {
        Ok(m) => Some(m),
        Err(err) => {
            tracing::warn!(
                target: "rbac.db",
                role_assignment_id = %id,
                error = %err,
                "skipping corrupt role_assignment row on a multi-row read; \
                 the row is excluded from the result set"
            );
            None
        }
    }
}

/// Translate a `SeaORM` `DbErr` to a typed [`DomainError`].
///
/// Prefers the structured `constraint` field from sqlx's `DatabaseError`;
/// the formatted message is locale-fragile (non-English `lc_messages`
/// re-words it) so substring matching is only the fallback. Unattributed
/// errors fall through to the generic classifier in
/// [`classify_db_err_to_domain`].
fn map_db_err(kind: &'static str, err: DbErr) -> DomainError {
    let _ = kind; // generic diagnostic now owned by the classifier; kind reserved for tracing
    let hint = extract_constraint_hint(&err);
    let generic = classify_db_err_to_domain(err);

    match (&generic, &hint) {
        (DomainError::AlreadyExists { .. }, Some(h)) => {
            if h.matches(
                UQ_ASSIGNMENT,
                &[
                    "role_definition_id",
                    "principal_type",
                    "principal_id",
                    "scope",
                ],
            ) {
                // Caller (only `create` today) restores the tuple by
                // matching on `RoleAssignmentDuplicate { .. }`. After
                // The `principal_type` field is the typed enum, so
                // there is no "empty-marker" form like the other
                // fields' empty strings carry. `PrincipalType::User`
                // is the placeholder; the only producer (`create`)
                // always overwrites it with the request's real value.
                DomainError::RoleAssignmentDuplicate {
                    role_definition_id: Uuid::nil(),
                    principal_type: PrincipalType::User,
                    principal_id: String::new(),
                    scope: String::new(),
                }
            } else {
                generic
            }
        }
        (DomainError::Conflict { .. }, Some(h)) => {
            // Match by the specific referencing column so the SQLite
            // column-set fallback can't accept a future FK.
            if h.matches(FK_ASSIGNMENT_ROLE, &["role_definition_id"]) {
                DomainError::RoleDefinitionMissing {
                    role_definition_id: Uuid::nil(),
                }
            } else {
                generic
            }
        }
        _ => generic,
    }
}

fn map_scope_err(kind: &'static str, err: ScopeError) -> DomainError {
    match err {
        ScopeError::Db(db_err) => map_db_err(kind, db_err),
        other => DomainError::internal(format!(
            "rbac role_assignments {kind}: scope error: {other}"
        )),
    }
}

/// Run one `get_subject_assignments` query for the given principal
/// selectors (`user_principal` OR `group_principals`), applying the scope
/// predicate from `query` (skipped in `all_scopes` mode). Returns raw
/// entity rows; the caller maps + skips corrupt rows. Factored out so
/// `get_subject_assignments` can split a large `group_principals` set
/// across several bounded queries while the common case stays a
/// single combined query.
async fn run_subject_assignments_query(
    db: &impl DBRunner,
    user_principal: Option<&(PrincipalType, String)>,
    group_principals: &[String],
    query: &SubjectAssignmentsQuery,
) -> Result<Vec<role_assignment::Model>, DomainError> {
    // Principal predicate: user-principal OR group-principal IN (...).
    let mut principal_any = sea_orm::Condition::any();
    if let Some((pt, pid)) = user_principal {
        principal_any = principal_any.add(
            role_assignment::Column::PrincipalType
                .eq(pt.as_str())
                .and(role_assignment::Column::PrincipalId.eq(pid.as_str())),
        );
    }
    if !group_principals.is_empty() {
        principal_any = principal_any.add(
            role_assignment::Column::PrincipalType
                .eq(PrincipalType::Group.as_str())
                .and(role_assignment::Column::PrincipalId.is_in(group_principals.iter().cloned())),
        );
    }

    // The scope predicate applies only when NOT in all-scopes mode — the
    // root-context list matches a subject's grants across every tenant, so
    // it deliberately skips the ancestor-scope narrowing.
    let mut select = role_assignment::Entity::find().filter(principal_any);
    if !query.all_scopes {
        // Ancestor scope-equality OR `scope LIKE rg_prefix`.
        // `ancestor_scopes` carries EXACT scope strings — an RG-scoped
        // assignment under an ancestor tenant grants access to that RG
        // only and MUST NOT be returned for a different context tenant
        // (I-33b). Root rows pivot on `tenant_id IS NULL` (only `/`
        // resolves to a `None` `tenant_id`); tenant- and other-shaped rows
        // pivot on `scope IN (...)`.
        let (root_match, tenant_ids, other_scopes) = split_ancestor_scopes(&query.ancestor_scopes);
        let mut scope_any = sea_orm::Condition::any();
        if root_match {
            scope_any = scope_any.add(role_assignment::Column::TenantId.is_null());
        }
        if !tenant_ids.is_empty() {
            let tenant_scopes: Vec<String> = tenant_ids
                .iter()
                .map(|id| format!("/tenants/{id}"))
                .collect();
            scope_any = scope_any.add(role_assignment::Column::Scope.is_in(tenant_scopes));
        }
        if !other_scopes.is_empty() {
            scope_any = scope_any.add(role_assignment::Column::Scope.is_in(other_scopes));
        }
        if !query.context_tenant_rg_prefix.is_empty() {
            // Server-built pattern (`/tenants/{uuid}/resourceGroups/%`).
            // Nothing here is caller-supplied today, but follow the
            // two-step escape contract from `like_escape.rs` so a future
            // change that lets external input flow into the prefix can't
            // silently widen the match.
            let raw = query.context_tenant_rg_prefix.as_str();
            let (literal, wildcard) = raw.strip_suffix('%').map_or((raw, ""), |s| (s, "%"));
            let escaped = crate::infra::storage::like_escape::escape_like_literal(literal);
            scope_any = scope_any.add(role_assignment::Column::Scope.like(
                crate::infra::storage::like_escape::escaped_like(format!("{escaped}{wildcard}")),
            ));
        }
        select = select.filter(scope_any);
    }

    select
        // Index-backed ordering via `idx_role_assignments_scope_depth`.
        .order_by_desc(role_assignment::Column::ScopeDepth)
        .order_by_desc(role_assignment::Column::Id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(db)
        .await
        .map_err(|err| map_scope_err("get_subject_assignments", err))
}

// `role_assignment` is `#[secure(unrestricted)]`: tenant scoping is
// expressed via the `scope` / `tenant_id` / `scope_depth` columns,
// which we filter on explicitly per call. Every
// `scope_with(&AccessScope::allow_all())` / `scope_unchecked(...)`
// below uses the escape hatch deliberately; flipping the entity to a
// secured column would require revisiting every call site here.
#[async_trait]
impl crate::domain::role_assignment_repo::RoleAssignmentRepository for RoleAssignmentRepository {
    async fn create<C: DBRunner>(
        &self,
        db: &C,
        new: NewRoleAssignment,
    ) -> Result<RoleAssignmentModel, DomainError> {
        let id = Uuid::now_v7();
        let now = Utc::now().trunc_subsecs(6);

        // Serialise the typed `Scope` to its canonical path at the storage
        // boundary. `scope_depth` and `tenant_id` are derived from
        // the same `Scope` in Rust.
        let scope_path = new.scope.path();
        let active = role_assignment::ActiveModel {
            id: Set(id),
            role_definition_id: Set(new.role_definition_id),
            principal_id: Set(new.principal_id.clone()),
            principal_type: Set(new.principal_type.as_str().to_owned()),
            scope: Set(scope_path.clone()),
            scope_depth: Set(new.scope.depth()),
            tenant_id: Set(new.scope.tenant_id()),
            created_at: Set(now),
            updated_at: Set(now),
            created_by: Set(new.created_by.clone()),
            // The author's kind is stored as the same closed-enum tag the
            // `principal_type` column uses, so the two columns hold the
            // same vocabulary; only the *reader* differs (the author's kind
            // is parsed leniently, `principal_type` is not — see
            // `entity::role_assignment::entity_to_model`). `None` writes SQL
            // NULL, which the read path renders as "no author name".
            created_by_type: Set(new.created_by_type.map(|kind| kind.as_str().to_owned())),
            created_by_tenant_id: Set(new.created_by_tenant_id),
        };

        let insert = role_assignment::Entity::insert(active)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .map_err(|e| {
                DomainError::internal(format!(
                    "create role_assignment: apply allow-all scope failed: {e}"
                ))
            })?;

        insert
            .exec(db)
            .await
            .map_err(|err| match map_scope_err("create", err) {
                // Restore the caller's tuple; DB message omits it.
                DomainError::RoleAssignmentDuplicate { .. } => {
                    DomainError::RoleAssignmentDuplicate {
                        role_definition_id: new.role_definition_id,
                        principal_type: new.principal_type,
                        principal_id: new.principal_id.clone(),
                        scope: scope_path.clone(),
                    }
                }
                DomainError::RoleDefinitionMissing { .. } => DomainError::RoleDefinitionMissing {
                    role_definition_id: new.role_definition_id,
                },
                other => other,
            })?;

        Ok(RoleAssignmentModel {
            id,
            role_definition_id: new.role_definition_id,
            principal_id: new.principal_id,
            principal_type: new.principal_type,
            scope: new.scope,
            created_at: now,
            updated_at: now,
            created_by: new.created_by,
            // Echoed back rather than re-read: the insert above is the
            // authority for what the row now holds, and a 201 body must
            // agree with it without a round trip.
            created_by_type: new.created_by_type,
            created_by_tenant_id: new.created_by_tenant_id,
        })
    }

    async fn find_by_id<C: DBRunner>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<Option<RoleAssignmentModel>, DomainError> {
        let row = role_assignment::Entity::find_by_id(id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(db)
            .await
            .map_err(|err| map_scope_err("find_by_id", err))?;
        row.map(entity_to_model).transpose()
    }

    async fn list<C: DBRunner>(
        &self,
        db: &C,
        visibility: VisibilityFilter,
        query: &ODataQuery,
    ) -> Result<Page<RoleAssignmentModel>, DomainError> {
        // `None` visibility (or an empty `Subtrees` set) short-circuits
        // before the DB.
        match &visibility {
            VisibilityFilter::None => return Ok(empty_page()),
            VisibilityFilter::Subtrees(prefixes) if prefixes.is_empty() => {
                return Ok(empty_page());
            }
            _ => {}
        }

        // Apply caller-derived visibility before `paginate_odata` so it
        // composes with the user `$filter` as a single SQL `WHERE`.
        let mut base_select = role_assignment::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all());
        if let Some(cond) = visibility_condition(&visibility) {
            base_select = base_select.filter(cond);
        }

        // Lower the user `$filter` AST into a SeaORM `Condition` over
        // this entity's mapped columns.
        //
        // Literals are normalized first: this endpoint returns a `text`
        // `principal_id` next to a `uuid` `role_definition_id`, and without
        // the pass the correct spelling of an `eq` filter would differ
        // between the two ids in the same response. See
        // `infra::odata_normalize`.
        if let Some(ast) = query.filter.as_ref() {
            let ast = normalize_filter_literals::<RoleAssignmentFilterField>(ast);
            let node =
                convert_expr_to_filter_node::<RoleAssignmentFilterField>(&ast).map_err(|e| {
                    DomainError::Validation {
                        detail: format!("$filter: {e}"),
                    }
                })?;
            let filter_cond = filter_node_to_condition::<
                RoleAssignmentFilterField,
                RoleAssignmentODataMapper,
            >(&node)
            .map_err(|e| DomainError::Validation {
                detail: format!("$filter: {e}"),
            })?;
            base_select = base_select.filter(filter_cond);
        }

        // Default order (`created_at DESC, id DESC`, hits
        // `idx_role_assignments_created_at_id`, migration 000002) +
        // `$filter` stripping is the shared policy in `odata_err` so it
        // can't drift from the role-definition list.
        let query_no_filter = crate::infra::odata_err::list_query_with_default_order(query);

        let page = paginate_odata::<
            RoleAssignmentFilterField,
            RoleAssignmentODataMapper,
            role_assignment::Entity,
            role_assignment::Model,
            _,
            _,
        >(
            base_select,
            db,
            &query_no_filter,
            ("id", SortDir::Desc),
            ROLE_ASSIGNMENT_LIMIT_CFG,
            |m| m,
        )
        .await
        .map_err(crate::infra::odata_err::map_odata_err_to_domain)?;

        let items: Vec<RoleAssignmentModel> = page
            .items
            .into_iter()
            .filter_map(entity_to_model_skip_corrupt)
            .collect();
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn get_subject_assignments<C: DBRunner>(
        &self,
        db: &C,
        query: SubjectAssignmentsQuery,
    ) -> Result<Vec<RoleAssignmentModel>, DomainError> {
        // The `group_principals` count is attacker-influenceable (a
        // subject's group-membership count), so an uncapped `IN (...)` on
        // this authz hot path risks the driver bind-parameter limit and a
        // pathological plan — the same hazard `find_by_ids` chunks.
        let entities: Vec<role_assignment::Model> = if query.group_principals.len()
            <= GROUP_PRINCIPALS_CHUNK
        {
            // Common case: one combined `(user OR group)` query.
            run_subject_assignments_query(
                db,
                query.user_principal.as_ref(),
                &query.group_principals,
                &query,
            )
            .await?
        } else {
            // Chunked: the user-principal once, then each group-id chunk.
            // Rows are disjoint by principal (a row carries exactly one
            // principal), so the dedup-by-id is belt-and-braces.
            let mut out: Vec<role_assignment::Model> = Vec::new();
            let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
            if query.user_principal.is_some() {
                for m in
                    run_subject_assignments_query(db, query.user_principal.as_ref(), &[], &query)
                        .await?
                {
                    if seen.insert(m.id) {
                        out.push(m);
                    }
                }
            }
            for chunk in query.group_principals.chunks(GROUP_PRINCIPALS_CHUNK) {
                for m in run_subject_assignments_query(db, None, chunk, &query).await? {
                    if seen.insert(m.id) {
                        out.push(m);
                    }
                }
            }
            out
        };

        Ok(entities
            .into_iter()
            .filter_map(entity_to_model_skip_corrupt)
            .collect())
    }

    async fn count_by_role<C: DBRunner>(
        &self,
        db: &C,
        visibility: VisibilityFilter,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, u64>, DomainError> {
        /// One `GROUP BY role_definition_id` row. `c` is `i64` because that
        /// is what `COUNT()` yields on both backends we ship.
        #[derive(FromQueryResult)]
        struct RoleGroup {
            role_definition_id: Uuid,
            c: i64,
        }

        // Two short-circuits before any SQL. An empty id list would emit
        // `IN ()`, a syntax error on every dialect (the same guard
        // `find_by_ids` carries); `None` visibility admits no rows at all, so
        // the honest answer is the empty map rather than a query whose result
        // we would then have to throw away.
        if ids.is_empty() || matches!(visibility, VisibilityFilter::None) {
            return Ok(HashMap::new());
        }
        if let VisibilityFilter::Subtrees(prefixes) = &visibility
            && prefixes.is_empty()
        {
            return Ok(HashMap::new());
        }

        let mut out: HashMap<Uuid, u64> = HashMap::with_capacity(ids.len());
        // Chunked for the same reason `find_by_ids` and
        // `get_subject_assignments` are: the id list comes from a caller's
        // page, whose size the caller influences, and an unbounded `IN (...)`
        // risks the driver's bind-parameter limit and a pathological plan.
        // Each chunk is a disjoint set of ids, so the per-chunk maps merge
        // without any need to add counts together.
        for chunk in ids.chunks(ROLE_ID_CHUNK) {
            let mut select = role_assignment::Entity::find()
                .filter(role_assignment::Column::RoleDefinitionId.is_in(chunk.iter().copied()));
            // The caller-visibility predicate — the same one `list` applies,
            // from the same function, so the count and the page it decorates
            // can never disagree about which rows exist for this caller.
            if let Some(cond) = visibility_condition(&visibility) {
                select = select.filter(cond);
            }
            let groups: Vec<RoleGroup> = select
                .secure()
                .scope_with(&AccessScope::allow_all())
                .project_all(db, |q| {
                    q.select_only()
                        .column(role_assignment::Column::RoleDefinitionId)
                        // `idx_role_assignments_role` backs the grouping.
                        .column_as(Expr::col(role_assignment::Column::Id).count(), "c")
                        .group_by(role_assignment::Column::RoleDefinitionId)
                        .into_model::<RoleGroup>()
                })
                .await
                .map_err(|err| map_scope_err("count_by_role", err))?;
            for group in groups {
                // A `COUNT` is never negative; `try_from` keeps the
                // conversion lossless-by-construction instead of an `as`
                // cast that would silently wrap a corrupt value.
                out.insert(
                    group.role_definition_id,
                    u64::try_from(group.c).unwrap_or(0),
                );
            }
        }
        Ok(out)
    }

    async fn delete<C: DBRunner>(&self, db: &C, id: Uuid) -> Result<bool, DomainError> {
        let result = role_assignment::Entity::delete_many()
            .filter(role_assignment::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(db)
            .await
            .map_err(|err| map_scope_err("delete", err))?;
        Ok(result.rows_affected > 0)
    }
}

/// Empty [`Page`] used by the short-circuit branches of `list`.
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

/// Lower a [`VisibilityFilter`] into the SQL predicate that admits exactly
/// the rows the caller may read, or `None` when no predicate is needed.
///
/// The single owner of that translation, because both `list` and
/// `count_by_role` must narrow by the identical set — the count exists to
/// tell a caller how many rows the list would show them, so a divergence
/// here would make the number a lie rather than merely stale.
///
/// `None` maps to `None` too: the caller-facing short-circuit for that case
/// lives in each method (an empty page, an empty map) because there is no
/// predicate that expresses "no rows" without also hitting the database.
fn visibility_condition(visibility: &VisibilityFilter) -> Option<sea_orm::Condition> {
    let VisibilityFilter::Subtrees(prefixes) = visibility else {
        // `Unrestricted` narrows nothing; `None` is short-circuited by the
        // caller before it ever reaches SQL.
        return None;
    };
    let (tenant_only, other_prefixes) = partition_prefixes_by_shape(prefixes);
    let mut any = sea_orm::Condition::any();
    if !tenant_only.is_empty() {
        any = any.add(role_assignment::Column::TenantId.is_in(tenant_only));
    }
    for p in &other_prefixes {
        any = any.add(scope_prefix_condition(p));
    }
    Some(any)
}

/// `scope == prefix` OR `scope LIKE prefix/%` — descendant match.
/// `prefix` is LIKE-escaped so caller-supplied `%` / `_` match literally.
/// The `escaped_like` wrapper appends `ESCAPE '\'` — required for the
/// escape characters to actually take effect on `SQLite`.
fn scope_prefix_condition(prefix: &str) -> sea_orm::Condition {
    let escaped_prefix = crate::infra::storage::like_escape::escape_like_literal(prefix);
    let like = if prefix.ends_with('/') {
        // `prefix` already carries a slash → `LIKE 'prefix%'`.
        format!("{escaped_prefix}%")
    } else {
        format!("{escaped_prefix}/%")
    };
    sea_orm::Condition::any()
        .add(role_assignment::Column::Scope.eq(prefix))
        .add(
            role_assignment::Column::Scope
                .like(crate::infra::storage::like_escape::escaped_like(like)),
        )
}

/// Split a `VisibilityFilter::Subtrees` prefix list: tenant-shaped
/// prefixes go through the indexed `tenant_id` column; everything else
/// falls back to `scope_prefix_condition`.
fn partition_prefixes_by_shape(prefixes: &[String]) -> (Vec<Uuid>, Vec<String>) {
    let mut tenant_only: Vec<Uuid> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    for prefix in prefixes {
        match rbac_sdk::models::Scope::parse(prefix) {
            Ok(rbac_sdk::models::Scope::Tenant { tenant_id }) => tenant_only.push(tenant_id),
            // Root and ResourceGroup don't pivot on `tenant_id`
            // (root has none; RG needs descendant matching via
            // `scope LIKE`). Future variants also fall through.
            _ => other.push(prefix.clone()),
        }
    }
    (tenant_only, other)
}

/// Bucket ancestor scopes by shape: root-match (`/`), tenant UUIDs, and
/// other paths. Root pivots on `tenant_id IS NULL`, tenant paths use the
/// indexed `tenant_id` column; everything else falls back to the general
/// `scope IN (...)` predicate (defence-in-depth — current callers only
/// emit `/` and `/tenants/{uuid}`).
fn split_ancestor_scopes(scopes: &[String]) -> (bool, Vec<Uuid>, Vec<String>) {
    let mut root_match = false;
    let mut tenant_ids: Vec<Uuid> = Vec::with_capacity(scopes.len());
    let mut other_scopes: Vec<String> = Vec::new();
    for path in scopes {
        match rbac_sdk::models::Scope::parse(path) {
            Ok(rbac_sdk::models::Scope::Root) => root_match = true,
            Ok(rbac_sdk::models::Scope::Tenant { tenant_id }) => tenant_ids.push(tenant_id),
            _ => other_scopes.push(path.clone()),
        }
    }
    (root_match, tenant_ids, other_scopes)
}

#[cfg(test)]
mod ancestor_split_test {
    use super::{partition_prefixes_by_shape, split_ancestor_scopes};
    use uuid::Uuid;

    fn t1() -> Uuid {
        uuid::uuid!("11111111-1111-1111-1111-111111111111")
    }
    fn t2() -> Uuid {
        uuid::uuid!("22222222-2222-2222-2222-222222222222")
    }
    fn rg() -> Uuid {
        uuid::uuid!("33333333-3333-3333-3333-333333333333")
    }

    #[test]
    fn split_ancestor_scopes_classifies_each_shape() {
        let scopes = vec![
            "/".to_owned(),
            format!("/tenants/{}", t1()),
            format!("/tenants/{}", t2()),
            format!("/tenants/{}/resourceGroups/{}", t1(), rg()),
            "/not-a-scope".to_owned(),
        ];
        let (root, tenants, other) = split_ancestor_scopes(&scopes);
        assert!(root, "leading `/` must set root_match");
        assert_eq!(tenants, vec![t1(), t2()]);
        assert_eq!(other.len(), 2);
        assert!(other.iter().any(|s| s.contains("resourceGroups")));
        assert!(other.iter().any(|s| s == "/not-a-scope"));
    }

    #[test]
    fn split_ancestor_scopes_empty_input_yields_no_pivots() {
        let (root, tenants, other) = split_ancestor_scopes(&[]);
        assert!(!root);
        assert!(tenants.is_empty());
        assert!(other.is_empty());
    }

    #[test]
    fn partition_prefixes_separates_tenant_from_rg_and_root() {
        let prefixes = vec![
            format!("/tenants/{}", t1()),
            format!("/tenants/{}/resourceGroups/{}", t1(), rg()),
            "/".to_owned(),
        ];
        let (tenants, other) = partition_prefixes_by_shape(&prefixes);
        assert_eq!(tenants, vec![t1()]);
        assert_eq!(other.len(), 2);
    }
}
