use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, InsertResult, IntoActiveModel, ModelTrait,
    QueryFilter,
    sea_query::{IntoIden, OnConflict, SimpleExpr},
};
use std::marker::PhantomData;

use crate::secure::cond::build_scope_condition;
use crate::secure::error::ScopeError;
use crate::secure::{
    AccessScope, DBRunner, DBRunnerInternal, ScopableEntity, Scoped, SeaOrmRunner, SecureEntityExt,
    Unscoped,
};

/// Convert a `sea_orm::Value` to a [`ScopeValue`] for comparison with scope filter values.
///
/// Supports UUID, String. Returns `None` for unsupported types.
fn sea_value_to_scope_value(v: &sea_orm::Value) -> Option<toolkit_security::ScopeValue> {
    use toolkit_security::ScopeValue;
    match v {
        sea_orm::Value::Uuid(Some(u)) => Some(ScopeValue::Uuid(*u)),
        sea_orm::Value::String(Some(s)) => {
            // Try UUID first for consistent matching
            if let Ok(uuid) = uuid::Uuid::parse_str(s) {
                Some(ScopeValue::Uuid(uuid))
            } else {
                Some(ScopeValue::String(s.clone()))
            }
        }
        sea_orm::Value::BigInt(Some(n)) => Some(ScopeValue::Int(*n)),
        sea_orm::Value::Int(Some(n)) => Some(ScopeValue::Int(i64::from(*n))),
        sea_orm::Value::SmallInt(Some(n)) => Some(ScopeValue::Int(i64::from(*n))),
        sea_orm::Value::TinyInt(Some(n)) => Some(ScopeValue::Int(i64::from(*n))),
        sea_orm::Value::Bool(Some(b)) => Some(ScopeValue::Bool(*b)),
        _ => None,
    }
}

/// Validate that the values in an `ActiveModel` satisfy at least one constraint
/// in the provided `AccessScope`.
///
/// This is the INSERT-time counterpart of `build_scope_condition`: instead of
/// adding `WHERE` clauses to a query, it checks the `ActiveModel`'s column
/// values in-memory against every scope filter.
///
/// # Semantics
///
/// - Multiple constraints are **OR-ed**: the insert is allowed if ANY constraint
///   matches entirely.
/// - Filters within a constraint are **AND-ed**: ALL filters must match for
///   that constraint to pass.
/// - A filter whose property resolves to a column where the `ActiveModel` value
///   is `NotSet` is **skipped** (the column is not being inserted, so there's
///   nothing to validate).
/// - A filter whose property does **not** resolve (unknown property) causes
///   that constraint to fail (fail-closed), consistent with the query-path
///   behavior in `build_scope_condition`.
///
/// # Errors
///
/// Returns `ScopeError::Denied` if no constraint matches the `ActiveModel`.
fn validate_insert_scope<A>(am: &A, scope: &AccessScope) -> Result<(), ScopeError>
where
    A: ActiveModelTrait,
    A::Entity: ScopableEntity + EntityTrait,
    <A::Entity as EntityTrait>::Column: ColumnTrait + Copy,
{
    if scope.is_unconstrained() || A::Entity::IS_UNRESTRICTED {
        return Ok(());
    }
    if scope.is_deny_all() {
        return Err(ScopeError::Denied(
            "insert denied: scope has no constraints",
        ));
    }

    // OR over constraints: at least one must match entirely.
    'next_constraint: for constraint in scope.constraints() {
        // AND over filters within this constraint.
        for filter in constraint.filters() {
            let Some(col) = <A::Entity as ScopableEntity>::resolve_property(filter.property())
            else {
                // Unknown property → this constraint fails (fail-closed).
                continue 'next_constraint;
            };

            // Extract the column value from the ActiveModel.
            match am.get(col) {
                sea_orm::ActiveValue::NotSet => {
                    // Column not being set in this insert — skip this filter.
                    // (e.g., auto-generated columns, defaults)
                }
                sea_orm::ActiveValue::Set(v) | sea_orm::ActiveValue::Unchanged(v) => {
                    let Some(sv) = sea_value_to_scope_value(&v) else {
                        // Unsupported column type — can't match filter.
                        continue 'next_constraint;
                    };

                    if !filter.values().contains(&sv) {
                        continue 'next_constraint;
                    }
                }
            }
        }
        // All filters in this constraint matched → insert is allowed.
        return Ok(());
    }

    Err(ScopeError::Denied(
        "insert denied: entity values do not satisfy any scope constraint",
    ))
}

/// Secure insert helper for Scopable entities.
///
/// This helper performs a standard `INSERT` through `SeaORM` but wraps database
/// errors into a unified `ScopeError` type for consistent error handling across
/// secure data-access code.
///
/// # Scope Validation
///
/// Validates **all** scope constraints against the `ActiveModel`'s column values,
/// not just `tenant_id`. For each constraint in the scope, every filter's property
/// is resolved to a column via `ScopableEntity::resolve_property`, and the
/// `ActiveModel`'s value for that column is checked against the filter's values.
/// At least one constraint must match entirely (OR semantics) for the insert to
/// proceed.
///
/// # Responsibilities
///
/// - Does **not** inspect the `SecurityContext` or enforce tenant scoping rules.
/// - Does **not** automatically populate any entity fields.
/// - Callers are responsible for:
///   - Setting all required fields before calling.
///   - Validating that the operation is authorized within the current
///     `SecurityContext` (e.g., verifying `tenant_id` or resource ownership).
///
/// # Behavior by Entity Type
///
/// ## Tenant-scoped entities (have `tenant_col`)
/// - Must have a valid, non-empty `tenant_id` set in the `ActiveModel` before insert.
/// - The `tenant_id` should come from the request payload or be validated against
///   `SecurityContext` by the service layer before calling this helper.
///
/// ## Global entities (no `tenant_col`)
/// - May be inserted freely without tenant validation.
/// - Typical examples include system-wide configuration or audit logs.
///
/// # Recommended Field Population
///
/// When inserting entities, populate these fields from `SecurityContext` in service code:
/// - `tenant_id`: from payload or validated via `ctx.scope()`
/// - `owner_id`: from `ctx.subject_id()`
/// - `created_by`: from `ctx.subject_id()` if applicable
///
/// # Example
///
/// ```ignore
/// use toolkit_db::secure::{secure_insert, SecurityContext};
///
/// // Domain/service layer validates tenant_id beforehand
/// let am = user::ActiveModel {
///     id: Set(Uuid::new_v4()),
///     tenant_id: Set(tenant_id),
///     owner_id: Set(ctx.subject_id()),
///     email: Set("user@example.com".to_string()),
///     ..Default::default()
/// };
///
/// // Simple secure insert wrapper
/// let user = secure_insert::<user::Entity>(am, &ctx, conn).await?;
/// ```
///
/// # Errors
///
/// - Returns `ScopeError::Db` if the database insert fails.
/// - Returns `ScopeError::Denied` if the `ActiveModel` values do not satisfy any scope constraint.
/// - Returns `ScopeError::TenantNotInScope` for tenant isolation violations.
pub async fn secure_insert<E>(
    am: E::ActiveModel,
    scope: &AccessScope,
    runner: &impl DBRunner,
) -> Result<E::Model, ScopeError>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Send,
    E::Model: sea_orm::IntoActiveModel<E::ActiveModel>,
{
    // Tenant-scoped entities must have tenant_id set in the ActiveModel.
    if let Some(tenant_col) = E::tenant_col()
        && let sea_orm::ActiveValue::NotSet = am.get(tenant_col)
    {
        return Err(ScopeError::Invalid("tenant_id is required"));
    }

    validate_insert_scope(&am, scope)?;

    match DBRunnerInternal::as_seaorm(runner) {
        SeaOrmRunner::Conn(db) => Ok(am.insert(db).await?),
        SeaOrmRunner::Tx(tx) => Ok(am.insert(tx).await?),
    }
}

/// Secure batch-insert helper for `Scopable` entities -- the multi-row
/// counterpart of [`secure_insert`], issuing one `INSERT ... VALUES (r1),
/// (r2), ...` instead of one `INSERT` per model.
///
/// # Security
///
/// Every model is validated against `scope` individually, in memory, before
/// any row reaches the database: if any model fails, nothing is inserted --
/// stronger than looping `secure_insert`, which can leave earlier rows committed.
///
/// This is what [`SecureInsertMany`] cannot offer: by the time a row reaches
/// `InsertMany` it has been lowered into an `InsertStatement`, so that wrapper
/// can only take the scope on trust. Here the `ActiveModel`s are still in
/// hand, so the batch gets the same per-row check a single `secure_insert`
/// performs -- and then goes on to execute *through* [`SecureInsertMany`], so
/// the statement still passes the secure type-state rather than bypassing it.
///
/// # Batching
///
/// A multi-row insert binds one parameter per column per row, and each
/// backend caps that count (see [`max_bind_params_for`]); the batch is split
/// into as many statements as needed, one statement when it fits.
///
/// **A split batch is not atomic on its own** -- wrap the call in a
/// transaction if a partial commit across split statements is unacceptable.
///
/// # Divergences from `secure_insert`, checked against sea-orm 2.0.2
///
/// Both are consequences of going through `InsertMany::many` rather than one
/// `ActiveModelTrait::insert` per model, and neither is a reason to forbid
/// heterogeneous batches -- a legitimate scenario -- only to know about
/// before mixing `Set`/`NotSet` shapes in one call:
///
/// - A column that is `NotSet` on one model but `Set` on another in the same
///   batch is not left to the database's `DEFAULT`: `many` records the typed
///   null of whichever row does set the column, and binds it for every row
///   where it's `NotSet` (`InsertMany::many` in `sea_orm::query::insert`). A
///   column-default that isn't `NULL` is silently discarded for those rows.
/// - `ActiveModelBehavior::before_save` never runs. It lives on
///   `ActiveModelTrait::insert`, which this helper does not call --
///   `InsertMany::exec` has no hook for it. Any invariant a model normally
///   enforces in `before_save` is the caller's job for a batched insert.
///
/// # Errors
///
/// - `ScopeError::Invalid` if a tenant-scoped model is missing `tenant_id`.
/// - `ScopeError::Denied` if a model's values do not satisfy the scope.
/// - `ScopeError::Db` if the database insert fails.
pub async fn secure_insert_many<E>(
    models: Vec<E::ActiveModel>,
    scope: &AccessScope,
    runner: &impl DBRunner,
) -> Result<(), ScopeError>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Send,
{
    if models.is_empty() {
        return Ok(());
    }

    // Validate every row before sending anything, so a scope violation anywhere
    // in the batch inserts nothing at all -- including when the batch is about
    // to be split into several statements below.
    for am in &models {
        if let Some(tenant_col) = E::tenant_col()
            && let sea_orm::ActiveValue::NotSet = am.get(tenant_col)
        {
            return Err(ScopeError::Invalid("tenant_id is required"));
        }
        validate_insert_scope(am, scope)?;
    }

    let backend = sea_orm::ConnectionTrait::get_database_backend(
        &DBRunnerInternal::as_seaorm(runner).executor(),
    );
    let rows_per_statement = rows_per_insert::<E>(backend);

    let mut rest = models;
    while !rest.is_empty() {
        let take = rows_per_statement.min(rest.len());
        let chunk: Vec<E::ActiveModel> = rest.drain(..take).collect();
        // Through `SecureInsertMany` rather than around it: the rows are
        // already validated above, so `scope_unchecked` is the honest
        // transition, and the batch stays inside the secure type-state
        // instead of reaching `InsertMany::exec` directly.
        E::insert_many(chunk)
            .secure()
            .scope_unchecked(scope)?
            .exec(runner)
            .await?;
    }
    Ok(())
}

/// The restriction [`secure_insert_from_select`] enforces, extracted so it can
/// be checked without a database.
///
/// Only an entity that declares no scope column at all may be written this
/// way.
///
/// # Errors
///
/// `ScopeError::Invalid` when `E` declares any scope column.
fn insert_from_select_allowed<E>() -> Result<(), ScopeError>
where
    E: ScopableEntity + EntityTrait,
{
    if E::tenant_col().is_some()
        || E::resource_col().is_some()
        || E::owner_col().is_some()
        || E::type_col().is_some()
    {
        return Err(ScopeError::Invalid(
            "insert-from-select is limited to entities without scope columns: \
             rows produced inside the database cannot be validated per row",
        ));
    }
    Ok(())
}

/// Execute an `INSERT ... SELECT`: the rows are computed and written inside
/// the database and never travel through this process.
///
/// The set-based counterpart of [`secure_insert_many`]. Use it where the rows
/// to insert are a function of rows the database already holds -- a closure
/// table rebuilt from itself, say. [`secure_insert_many`] still has to
/// materialize every row here first, which for a cross product means the row
/// count in process memory and on the wire; this sends one statement whose
/// size does not depend on how many rows it writes.
///
/// # Security
///
/// [`secure_insert`] and [`secure_insert_many`] validate every row against
/// the caller's scope before any of it reaches the database. Rows produced by
/// a `SELECT` inside the database are never visible here, so that check
/// cannot be run against them. What this helper checks instead is `scope`
/// itself: it must be unconstrained. That is **stricter** than the per-row
/// path, which also lets through an empty constraint list (an `AccessScope`
/// can be built with zero constraints without being the unconstrained
/// sentinel) -- and deliberately so. An entity with no scope columns has
/// nothing a non-empty constraint could match against, so there is no
/// per-row check to run in the first place; but telling an empty constraint
/// apart from an unconstrained scope here would add a distinction this
/// helper has no way to act on. Refusing both alike is the honest contract:
/// a caller passing any scope narrower than "everything" gets an error, not
/// a silent no-op-per-row insert. The entity-side restriction from before is
/// unchanged and independent: `E` must still declare no scope columns at
/// all, checked before the statement is built so it cannot be waived by a
/// caller that would rather not honour it.
///
/// The target table is `E`'s, and only `E`'s. The caller supplies the target
/// columns and the source `SELECT`; the `INSERT` around them is built here.
/// An earlier shape took a prepared `InsertStatement` and used `E` only for
/// the check above -- which meant the check and the table it was supposed to
/// be about were not connected: passing a scope-free `E` alongside a
/// statement writing into a scoped table satisfied the guard and defeated it
/// in the same call. Nothing about that was detectable from here, so the
/// statement is no longer the caller's to hand over.
///
/// Scoping the *source* rows remains the caller's job: build the inner
/// `SELECT` from a scoped query when the source table is scoped.
///
/// The source may read the target table — every backend here permits an
/// `INSERT ... SELECT` whose `SELECT` names the table being written. The
/// symmetry does not extend to deletes: `MySQL` rejects a `DELETE` whose
/// subquery names the target table (ER 1093), where `PostgreSQL` and `SQLite`
/// accept it. Callers pairing this helper with a set-based `DELETE` over the
/// same table are writing `PostgreSQL`/`SQLite`-only code — unless the
/// `DELETE` wraps its subquery in a derived table, which `MySQL`
/// materializes before deleting and therefore permits.
///
/// # Returns
///
/// The number of rows the statement wrote, as the database counted them.
/// Nothing here can derive it -- the rows never enter this process -- so a
/// caller that needs the figure (a metric, a budget check) has to be given
/// it.
///
/// # Errors
///
/// - `ScopeError::Denied` if `scope` is not unconstrained.
/// - `ScopeError::Invalid` if `E` declares any scope column, or if the target
///   column count does not match the source `SELECT`.
/// - `ScopeError::Db` if the statement fails.
pub async fn secure_insert_from_select<E, C>(
    columns: C,
    source: sea_orm::sea_query::SelectStatement,
    scope: &AccessScope,
    runner: &impl DBRunner,
) -> Result<u64, ScopeError>
where
    E: ScopableEntity + EntityTrait,
    C: IntoIterator<Item = E::Column>,
{
    if !scope.is_unconstrained() {
        return Err(ScopeError::Denied(
            "insert-from-select requires an unconstrained scope",
        ));
    }
    insert_from_select_allowed::<E>()?;

    let mut insert = sea_orm::sea_query::Query::insert();
    insert.into_table(E::default()).columns(columns);
    insert.select_from(source).map_err(|_| {
        ScopeError::Invalid(
            "insert-from-select: the target column count does not match the source select",
        )
    })?;
    let stmt = &insert;

    // SeaORM 2.0's `execute` takes the `StatementBuilder` itself and builds it
    // against the connection's own backend, so there is nothing to build here.
    let result =
        sea_orm::ConnectionTrait::execute(&DBRunnerInternal::as_seaorm(runner).executor(), stmt)
            .await?;
    Ok(result.rows_affected())
}

/// Bind-parameter budget for one statement, per backend.
///
/// Deliberately below each backend's hard ceiling, since the count here is
/// derived from the entity's declared columns and a builder may bind extras:
///
/// - `PostgreSQL`/`MySQL`: `u16` wire-protocol limit, 65535.
/// - `SQLite`: `SQLITE_MAX_VARIABLE_NUMBER`, 32766 on modern builds.
#[must_use]
const fn max_bind_params(backend: sea_orm::DbBackend) -> usize {
    match backend {
        sea_orm::DbBackend::Postgres | sea_orm::DbBackend::MySql => 60_000,
        // SQLite, and the floor for any backend `DbBackend` gains later -- it
        // is `#[non_exhaustive]` as of SeaORM 2.0, and an unknown backend is
        // better off chunked too small (a few extra statements) than too
        // large (a driver error).
        _ => 30_000,
    }
}

/// Maximum bind parameters a single statement may carry on `runner`'s
/// backend.
///
/// Exposed so callers building their own multi-row statement -- an
/// `IN (...)` delete over a collected id list, say -- can chunk against the
/// same limit [`secure_insert_many`] uses, instead of duplicating the
/// number or discovering it as a driver error on a large enough input.
#[must_use]
pub fn max_bind_params_for(runner: &impl DBRunner) -> usize {
    let backend = sea_orm::ConnectionTrait::get_database_backend(
        &DBRunnerInternal::as_seaorm(runner).executor(),
    );
    max_bind_params(backend)
}

/// How many rows of `E` fit in one multi-row insert on `backend`.
///
/// Uses the entity's full column count as the per-row cost, an upper bound
/// since a row with `NotSet` columns binds fewer. Truncates on purpose --
/// rounding up could push a statement past the limit -- and floors at 1.
#[allow(clippy::integer_division)]
fn rows_per_insert<E: EntityTrait>(backend: sea_orm::DbBackend) -> usize {
    use sea_orm::Iterable as _;
    let columns_per_row = E::Column::iter().count().max(1);
    (max_bind_params(backend) / columns_per_row).max(1)
}

/// Secure update helper for updating a single entity by ID inside a scope.
///
/// # Security
/// - Verifies the target row exists **within the scope** before updating.
/// - For tenant-scoped entities, forbids changing `tenant_id` (immutable).
///
/// # Errors
/// - `ScopeError::Denied` if the row is not accessible in the scope.
/// - `ScopeError::Denied("tenant_id is immutable")` if caller attempts to change `tenant_id`.
pub async fn secure_update_with_scope<E>(
    am: E::ActiveModel,
    scope: &AccessScope,
    id: uuid::Uuid,
    runner: &impl DBRunner,
) -> Result<E::Model, ScopeError>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
    E::ActiveModel: ActiveModelTrait<Entity = E> + Send,
    E::Model: sea_orm::IntoActiveModel<E::ActiveModel> + sea_orm::ModelTrait<Entity = E>,
{
    let existing = E::find()
        .secure()
        .scope_with(scope)
        .and_id(id)?
        .one(runner)
        .await?;

    let Some(existing) = existing else {
        return Err(ScopeError::Denied(
            "entity not found or not accessible in current security scope",
        ));
    };

    if let Some(tcol) = E::tenant_col() {
        let sea_orm::Value::Uuid(Some(stored)) = existing.get(tcol) else {
            return Err(ScopeError::Invalid("tenant_id has unexpected type"));
        };

        let incoming = match am.get(tcol) {
            sea_orm::ActiveValue::Set(v) | sea_orm::ActiveValue::Unchanged(v) => match v {
                sea_orm::Value::Uuid(Some(u)) => Some(u),
                sea_orm::Value::Uuid(None) => {
                    return Err(ScopeError::Invalid("tenant_id is required"));
                }
                _ => {
                    return Err(ScopeError::Invalid("tenant_id has unexpected type"));
                }
            },
            sea_orm::ActiveValue::NotSet => None,
        };

        if let Some(incoming) = incoming
            && incoming != stored
        {
            return Err(ScopeError::Denied("tenant_id is immutable"));
        }
    }

    match DBRunnerInternal::as_seaorm(runner) {
        SeaOrmRunner::Conn(db) => Ok(am.update(db).await?),
        SeaOrmRunner::Tx(tx) => Ok(am.update(tx).await?),
    }
}

/// Helper to validate a tenant ID is in the scope.
///
/// Use this when manually setting `tenant_id` in `ActiveModels` to ensure
/// the value matches the security scope.
///
/// For unconstrained scopes (allow-all), this always succeeds.
///
/// # Errors
/// Returns `ScopeError::Denied` if tenant scope is missing.
/// Returns `ScopeError::TenantNotInScope` if the tenant ID is not in any constraint.
pub fn validate_tenant_in_scope(
    tenant_id: uuid::Uuid,
    scope: &AccessScope,
) -> Result<(), ScopeError> {
    if scope.is_unconstrained() {
        return Ok(());
    }
    let prop = toolkit_security::pep_properties::OWNER_TENANT_ID;
    if !scope.has_property(prop) {
        return Err(ScopeError::Denied(
            "tenant scope required for tenant-scoped insert",
        ));
    }
    if scope.contains_uuid(prop, tenant_id) {
        return Ok(());
    }
    Err(ScopeError::TenantNotInScope { tenant_id })
}

/// A type-safe wrapper around `SeaORM`'s `Insert` that enforces scoping.
///
/// This wrapper uses the typestate pattern to ensure that insert operations
/// cannot be executed without first applying access control via
/// `.scope_with_model()` (validated) or `.scope_unchecked()` (unvalidated).
///
/// Unlike the simpler `secure_insert()` helper, this wrapper preserves `SeaORM`'s
/// builder methods like `on_conflict()` for upsert semantics.
///
/// # Example
/// ```ignore
/// use toolkit_db::secure::{AccessScope, SecureInsertExt};
/// use sea_orm::sea_query::OnConflict;
///
/// let scope = AccessScope::for_tenants(vec![tenant_id]);
/// let am = user::ActiveModel {
///     tenant_id: Set(tenant_id),
///     email: Set("user@example.com".to_string()),
///     ..Default::default()
/// };
///
/// user::Entity::insert(am)
///     .secure()                        // Returns SecureInsertOne<E, Unscoped>
///     .scope_with_model(&scope, &am)?  // Returns SecureInsertOne<E, Scoped>
///     .on_conflict(OnConflict::...)     // Builder methods still available
///     .exec(conn)                  // Now can execute
///     .await?;
/// ```
#[derive(Debug)]
pub struct SecureInsertOne<A, S>
where
    A: ActiveModelTrait,
{
    pub(crate) inner: sea_orm::Insert<A>,
    pub(crate) _state: PhantomData<S>,
}

/// Extension trait to convert a regular `SeaORM` `Insert` into a `SecureInsertOne`.
pub trait SecureInsertExt<A: ActiveModelTrait>: Sized {
    /// Convert this insert operation into a secure (unscoped) insert.
    /// You must call `.scope_with_model()` or `.scope_unchecked()` before executing.
    fn secure(self) -> SecureInsertOne<A, Unscoped>;
}

impl<A> SecureInsertExt<A> for sea_orm::Insert<A>
where
    A: ActiveModelTrait,
{
    fn secure(self) -> SecureInsertOne<A, Unscoped> {
        SecureInsertOne {
            inner: self,
            _state: PhantomData,
        }
    }
}

// Methods available only on Unscoped inserts
impl<A> SecureInsertOne<A, Unscoped>
where
    A: ActiveModelTrait + Send,
    A::Entity: ScopableEntity + EntityTrait,
    <A::Entity as EntityTrait>::Column: ColumnTrait + Copy,
{
    /// Transition to `Scoped` state **without** validating the `ActiveModel`
    /// against the scope constraints.
    ///
    /// # Safety (logical)
    ///
    /// This method performs **no** validation. The caller is responsible for
    /// ensuring the `ActiveModel` satisfies the scope (e.g., correct
    /// `tenant_id`). Prefer [`scope_with_model`](Self::scope_with_model)
    /// which validates all scope constraints automatically.
    ///
    /// # Errors
    ///
    /// Returns [`ScopeError`] if the access scope cannot be applied.
    pub fn scope_unchecked(
        self,
        scope: &AccessScope,
    ) -> Result<SecureInsertOne<A, Scoped>, ScopeError> {
        let _ = scope;
        Ok(SecureInsertOne {
            inner: self.inner,
            _state: PhantomData,
        })
    }

    /// Apply access control scope with explicit `ActiveModel` validation.
    ///
    /// This method validates **all** scope constraints against the `ActiveModel`'s
    /// column values (not just `tenant_id`). See [`validate_insert_scope`] for
    /// the full semantics.
    ///
    /// # Errors
    /// - Returns `ScopeError::Denied` if the `ActiveModel` values do not satisfy
    ///   any scope constraint.
    pub fn scope_with_model(
        self,
        scope: &AccessScope,
        am: &A,
    ) -> Result<SecureInsertOne<A, Scoped>, ScopeError> {
        validate_insert_scope(am, scope)?;
        Ok(SecureInsertOne {
            inner: self.inner,
            _state: PhantomData,
        })
    }
}

// Fluent builder methods (available only on Scoped inserts to prevent pre-scope execution)
impl<A> SecureInsertOne<A, Scoped>
where
    A: ActiveModelTrait,
    A::Entity: ScopableEntity + EntityTrait,
    <A::Entity as EntityTrait>::Column: ColumnTrait + Copy,
{
    /// Set the `ON CONFLICT` clause for upsert semantics using `SecureOnConflict`.
    ///
    /// This is the recommended way to add upsert semantics as it enforces
    /// tenant immutability at compile/validation time.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let on_conflict = SecureOnConflict::<Entity>::columns([Column::TenantId, Column::UserId])
    ///     .update_columns([Column::Theme, Column::Language])?;
    ///
    /// Entity::insert(am)
    ///     .secure()
    ///     .scope_unchecked(&scope)?
    ///     .on_conflict(on_conflict)
    ///     .exec(conn)
    ///     .await?;
    /// ```
    #[must_use]
    pub fn on_conflict(mut self, on_conflict: SecureOnConflict<A::Entity>) -> Self {
        self.inner = self.inner.on_conflict(on_conflict.build());
        self
    }

    /// Set the `ON CONFLICT` clause using raw `SeaORM` `OnConflict`.
    ///
    /// # Safety
    ///
    /// This method bypasses tenant immutability validation. The caller is
    /// responsible for ensuring that `tenant_id` is not included in update columns.
    /// Use `on_conflict()` with `SecureOnConflict` for automatic validation.
    #[must_use]
    pub fn on_conflict_raw(mut self, on_conflict: OnConflict) -> Self {
        self.inner = self.inner.on_conflict(on_conflict);
        self
    }
}

// Execution methods (require Scoped state)
impl<A> SecureInsertOne<A, Scoped>
where
    A: ActiveModelTrait,
{
    /// Execute the insert operation.
    ///
    /// # Errors
    /// Returns `ScopeError::Db` if the database operation fails.
    #[allow(clippy::disallowed_methods)]
    pub async fn exec<C>(self, runner: &C) -> Result<InsertResult<A>, ScopeError>
    where
        C: DBRunner,
        A: Send,
    {
        match DBRunnerInternal::as_seaorm(runner) {
            SeaOrmRunner::Conn(db) => Ok(self.inner.exec(db).await?),
            SeaOrmRunner::Tx(tx) => Ok(self.inner.exec(tx).await?),
        }
    }

    /// Execute the insert and return the inserted model.
    ///
    /// This is useful when you need the inserted data with any database-generated
    /// values (like auto-increment IDs or default values).
    ///
    /// # Errors
    /// Returns `ScopeError::Db` if the database operation fails.
    #[allow(clippy::disallowed_methods)]
    pub async fn exec_with_returning<C>(
        self,
        runner: &C,
    ) -> Result<<A::Entity as EntityTrait>::Model, ScopeError>
    where
        C: DBRunner,
        A: Send,
        <A::Entity as EntityTrait>::Model: IntoActiveModel<A>,
    {
        match DBRunnerInternal::as_seaorm(runner) {
            SeaOrmRunner::Conn(db) => Ok(self.inner.exec_with_returning(db).await?),
            SeaOrmRunner::Tx(tx) => Ok(self.inner.exec_with_returning(tx).await?),
        }
    }

    /// Unwrap the inner `SeaORM` `Insert` for advanced use cases.
    ///
    /// # Safety
    /// The caller must ensure they don't remove or bypass the security
    /// validation that was applied during `.scope_with_model()` / `.scope_unchecked()`.
    #[must_use]
    pub fn into_inner(self) -> sea_orm::Insert<A> {
        self.inner
    }
}

/// Secure wrapper around a multi-row `SeaORM` insert.
///
/// `SeaORM` 2.0 split `Entity::insert_many` out of `Insert<A>` into its own
/// `InsertMany<A>` builder, so the single-row [`SecureInsertOne`] wrapper no
/// longer covers it. This is the multi-row counterpart, with the same
/// `Unscoped` -> `Scoped` type-state so a batch insert cannot be executed
/// before a scope decision has been made.
///
/// Unlike [`SecureInsertOne`], there is no `scope_with_model` equivalent:
/// `InsertMany` has already lowered its rows into an `InsertStatement` and does
/// not hand back the `ActiveModel`s, so per-row validation is not possible
/// here. Callers that need validated rows have two options: scope each row
/// through [`SecureInsertOne`], or hand the whole batch to
/// [`secure_insert_many`], which validates the `ActiveModel`s while it still
/// has them and then executes through this wrapper -- one statement per
/// bind-budget chunk rather than one per row.
pub struct SecureInsertMany<A, S>
where
    A: ActiveModelTrait,
{
    pub(crate) inner: sea_orm::InsertMany<A>,
    pub(crate) _state: PhantomData<S>,
}

/// Extension trait to convert a `SeaORM` `InsertMany` into a `SecureInsertMany`.
pub trait SecureInsertManyExt<A: ActiveModelTrait>: Sized {
    /// Convert this batch insert into a secure (unscoped) insert.
    /// You must call `.scope_unchecked()` before executing.
    fn secure(self) -> SecureInsertMany<A, Unscoped>;
}

impl<A> SecureInsertManyExt<A> for sea_orm::InsertMany<A>
where
    A: ActiveModelTrait,
{
    fn secure(self) -> SecureInsertMany<A, Unscoped> {
        SecureInsertMany {
            inner: self,
            _state: PhantomData,
        }
    }
}

impl<A> SecureInsertMany<A, Unscoped>
where
    A: ActiveModelTrait + Send,
    A::Entity: ScopableEntity + EntityTrait,
    <A::Entity as EntityTrait>::Column: ColumnTrait + Copy,
{
    /// Transition to `Scoped` **without** validating the rows against the scope.
    ///
    /// # Safety (logical)
    ///
    /// Performs no validation — see the type-level note on why per-row
    /// validation is unavailable for batch inserts. The caller is responsible
    /// for ensuring every row satisfies the scope. This is the appropriate mode
    /// for entities declared `no_tenant`/`no_resource`/`no_owner`/`no_type`
    /// (e.g. cross-tenant closure tables).
    ///
    /// # Errors
    ///
    /// Returns [`ScopeError`] if the access scope cannot be applied.
    pub fn scope_unchecked(
        self,
        scope: &AccessScope,
    ) -> Result<SecureInsertMany<A, Scoped>, ScopeError> {
        if scope.is_deny_all() {
            return Err(ScopeError::Denied(
                "insert denied: scope has no constraints",
            ));
        }
        Ok(SecureInsertMany {
            inner: self.inner,
            _state: PhantomData,
        })
    }
}

impl<A> SecureInsertMany<A, Scoped>
where
    A: ActiveModelTrait,
    A::Entity: ScopableEntity + EntityTrait,
    <A::Entity as EntityTrait>::Column: ColumnTrait + Copy,
{
    /// Set the `ON CONFLICT` clause using [`SecureOnConflict`], which enforces
    /// tenant immutability.
    #[must_use]
    pub fn on_conflict(mut self, on_conflict: SecureOnConflict<A::Entity>) -> Self {
        self.inner = self.inner.on_conflict(on_conflict.build());
        self
    }

    /// Set the `ON CONFLICT` clause using raw `SeaORM` `OnConflict`.
    ///
    /// # Safety
    ///
    /// Bypasses tenant-immutability validation; the caller must ensure
    /// `tenant_id` is not among the update columns. Prefer
    /// [`on_conflict`](Self::on_conflict).
    #[must_use]
    pub fn on_conflict_raw(mut self, on_conflict: OnConflict) -> Self {
        self.inner = self.inner.on_conflict(on_conflict);
        self
    }
}

impl<A> SecureInsertMany<A, Scoped>
where
    A: ActiveModelTrait,
{
    /// Execute the batch insert.
    ///
    /// Note the `SeaORM` 2.0 semantics: an empty row set is **not** an error, it
    /// yields `InsertManyResult { last_insert_id: None }`.
    ///
    /// # Errors
    /// Returns `ScopeError::Db` if the database operation fails.
    #[allow(clippy::disallowed_methods)]
    pub async fn exec<C>(self, runner: &C) -> Result<sea_orm::InsertManyResult<A>, ScopeError>
    where
        C: DBRunner,
        A: Send,
    {
        match DBRunnerInternal::as_seaorm(runner) {
            SeaOrmRunner::Conn(db) => Ok(self.inner.exec(db).await?),
            SeaOrmRunner::Tx(tx) => Ok(self.inner.exec(tx).await?),
        }
    }

    /// Execute the batch insert and return the inserted models.
    ///
    /// # Errors
    /// Returns `ScopeError::Db` if the database operation fails.
    #[allow(clippy::disallowed_methods)]
    pub async fn exec_with_returning<C>(
        self,
        runner: &C,
    ) -> Result<Vec<<A::Entity as EntityTrait>::Model>, ScopeError>
    where
        C: DBRunner,
        A: Send,
        <A::Entity as EntityTrait>::Model: IntoActiveModel<A>,
    {
        match DBRunnerInternal::as_seaorm(runner) {
            SeaOrmRunner::Conn(db) => Ok(self.inner.exec_with_returning(db).await?),
            SeaOrmRunner::Tx(tx) => Ok(self.inner.exec_with_returning(tx).await?),
        }
    }

    /// Unwrap the inner `SeaORM` `InsertMany` for advanced use cases.
    ///
    /// # Safety
    /// The caller must ensure they don't remove or bypass the security
    /// decision applied during `.scope_unchecked()`.
    #[must_use]
    pub fn into_inner(self) -> sea_orm::InsertMany<A> {
        self.inner
    }
}

/// A secure builder for `ON CONFLICT DO UPDATE` clauses that enforces tenant immutability.
///
/// For tenant-scoped entities (`ScopableEntity::tenant_col() != None`), this builder
/// ensures that `tenant_id` is never included in the update columns. Attempting to
/// update `tenant_id` via `update_columns()` or `value()` returns an error.
///
/// # Security Rationale
///
/// `ON CONFLICT DO UPDATE` can be exploited to change an entity's tenant:
/// ```sql
/// INSERT INTO users (id, tenant_id, email) VALUES ($1, $2, $3)
/// ON CONFLICT (id) DO UPDATE SET tenant_id = excluded.tenant_id;
/// ```
/// This would allow moving a row from one tenant to another, violating tenant isolation.
///
/// # Example
///
/// ```ignore
/// use toolkit_db::secure::{SecureOnConflict, SecureInsertExt};
/// use sea_orm::ActiveValue::Set;
///
/// let scope = AccessScope::single(ScopeConstraint::new(vec![
///     ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
///     ScopeFilter::in_uuids(pep_properties::RESOURCE_ID, vec![user_id]),
/// ]));
/// let am = settings::ActiveModel {
///     tenant_id: Set(tenant_id),
///     user_id: Set(user_id),
///     theme: Set(Some("dark".to_string())),
///     language: Set(Some("en".to_string())),
/// };
///
/// // Build secure on_conflict - validates tenant_id is not updated
/// let on_conflict = SecureOnConflict::<settings::Entity>::columns([
///         settings::Column::TenantId,
///         settings::Column::UserId,
///     ])
///     .update_columns([settings::Column::Theme, settings::Column::Language])?;
///
/// settings::Entity::insert(am)
///     .secure()
///     .scope_unchecked(&scope)?
///     .on_conflict(on_conflict)
///     .exec(conn)
///     .await?;
/// ```
#[derive(Debug, Clone)]
pub struct SecureOnConflict<E: EntityTrait> {
    inner: OnConflict,
    _entity: PhantomData<E>,
}

impl<E> SecureOnConflict<E>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    /// Start building an `ON CONFLICT` clause with the specified conflict columns.
    ///
    /// These are the columns that define uniqueness (typically the primary key
    /// or a unique constraint).
    #[must_use]
    pub fn columns<C, I>(cols: I) -> Self
    where
        C: IntoIden,
        I: IntoIterator<Item = C>,
    {
        Self {
            inner: OnConflict::columns(cols),
            _entity: PhantomData,
        }
    }

    /// Specify columns to update on conflict.
    ///
    /// # Errors
    ///
    /// Returns `ScopeError::Denied("tenant_id is immutable")` if the entity has
    /// a tenant column and it appears in the update columns list.
    pub fn update_columns<C, I>(mut self, cols: I) -> Result<Self, ScopeError>
    where
        C: IntoIden + Copy + 'static,
        I: IntoIterator<Item = C>,
    {
        let cols: Vec<C> = cols.into_iter().collect();

        // Check if tenant column is in the update list
        if let Some(tenant_col) = E::tenant_col() {
            let tenant_iden = tenant_col.into_iden();
            for col in &cols {
                let col_iden = col.into_iden();
                if col_iden.to_string() == tenant_iden.to_string() {
                    return Err(ScopeError::Denied("tenant_id is immutable"));
                }
            }
        }

        self.inner.update_columns(cols);
        Ok(self)
    }

    /// Set a custom update expression for a column on conflict.
    ///
    /// # Errors
    ///
    /// Returns `ScopeError::Denied("tenant_id is immutable")` if the entity has
    /// a tenant column and the specified column matches it.
    pub fn value<C>(mut self, col: C, expr: SimpleExpr) -> Result<Self, ScopeError>
    where
        C: IntoIden + Copy + 'static,
    {
        // Check if this is the tenant column
        if let Some(tenant_col) = E::tenant_col() {
            let tenant_iden = tenant_col.into_iden();
            let col_iden = col.into_iden();
            if col_iden.to_string() == tenant_iden.to_string() {
                return Err(ScopeError::Denied("tenant_id is immutable"));
            }
        }

        self.inner.value(col, expr);
        Ok(self)
    }

    /// Consume the builder and return the underlying `SeaORM` `OnConflict`.
    ///
    /// Call this after configuring all update columns/values.
    #[must_use]
    pub fn build(self) -> OnConflict {
        self.inner
    }

    /// Get a reference to the inner `OnConflict` for chaining with `SeaORM` methods
    /// that are not wrapped by this builder.
    ///
    /// # Safety
    ///
    /// The caller must ensure they don't add tenant column updates through the
    /// inner `OnConflict` directly, as this would bypass the security check.
    #[must_use]
    pub fn inner_mut(&mut self) -> &mut OnConflict {
        &mut self.inner
    }
}

/// A type-safe wrapper around `SeaORM`'s `UpdateMany` that enforces scoping.
///
/// This wrapper uses the typestate pattern to ensure that update operations
/// cannot be executed without first applying access control via `.scope_with()`.
///
/// # Example
/// ```ignore
/// use toolkit_db::secure::{AccessScope, SecureUpdateExt};
///
/// let scope = AccessScope::for_tenants(vec![tenant_id]);
/// let result = user::Entity::update_many()
///     .col_expr(user::Column::Status, Expr::value("active"))
///     .secure()           // Returns SecureUpdateMany<E, Unscoped>
///     .scope_with(&scope)? // Returns SecureUpdateMany<E, Scoped>
///     .exec(conn)         // Now can execute
///     .await?;
/// ```
#[derive(Clone, Debug)]
pub struct SecureUpdateMany<E: EntityTrait, S> {
    pub(crate) inner: sea_orm::UpdateMany<E>,
    pub(crate) _state: PhantomData<S>,
    pub(crate) tenant_update_attempted: bool,
}

// Fluent builder methods (available in all typestates).
impl<E, S> SecureUpdateMany<E, S>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    /// Set a column expression (mirrors `SeaORM`'s `UpdateMany::col_expr`).
    #[must_use]
    pub fn col_expr(mut self, col: E::Column, expr: sea_orm::sea_query::SimpleExpr) -> Self {
        if let Some(tcol) = E::tenant_col()
            && std::mem::discriminant(&col) == std::mem::discriminant(&tcol)
        {
            self.tenant_update_attempted = true;
        }
        self.inner = self.inner.col_expr(col, expr);
        self
    }

    /// Add an additional filter. Scope conditions remain in place once applied.
    #[must_use]
    pub fn filter(mut self, filter: sea_orm::Condition) -> Self {
        self.inner = QueryFilter::filter(self.inner, filter);
        self
    }
}

/// Extension trait to convert a regular `SeaORM` `UpdateMany` into a `SecureUpdateMany`.
pub trait SecureUpdateExt<E: EntityTrait>: Sized {
    /// Convert this update operation into a secure (unscoped) update.
    /// You must call `.scope_with()` before executing.
    fn secure(self) -> SecureUpdateMany<E, Unscoped>;
}

impl<E> SecureUpdateExt<E> for sea_orm::UpdateMany<E>
where
    E: EntityTrait,
{
    fn secure(self) -> SecureUpdateMany<E, Unscoped> {
        SecureUpdateMany {
            inner: self,
            _state: PhantomData,
            tenant_update_attempted: false,
        }
    }
}

// Methods available only on Unscoped updates
impl<E> SecureUpdateMany<E, Unscoped>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    /// Apply access control scope to this update, transitioning to the `Scoped` state.
    ///
    /// This applies the implicit policy:
    /// - Empty scope → deny all (no rows updated)
    /// - Tenants only → update only in specified tenants
    /// - Resources only → update only specified resource IDs
    /// - Both → AND them together
    ///
    #[must_use]
    pub fn scope_with(self, scope: &AccessScope) -> SecureUpdateMany<E, Scoped> {
        let cond = build_scope_condition::<E>(scope);
        SecureUpdateMany {
            inner: self.inner.filter(cond),
            _state: PhantomData,
            tenant_update_attempted: self.tenant_update_attempted,
        }
    }
}

// Methods available only on Scoped updates
impl<E> SecureUpdateMany<E, Scoped>
where
    E: EntityTrait,
{
    /// Execute the update operation.
    ///
    /// # Errors
    /// Returns `ScopeError::Db` if the database operation fails.
    #[allow(clippy::disallowed_methods)]
    pub async fn exec(self, runner: &impl DBRunner) -> Result<sea_orm::UpdateResult, ScopeError> {
        if self.tenant_update_attempted {
            return Err(ScopeError::Denied("tenant_id is immutable"));
        }
        match DBRunnerInternal::as_seaorm(runner) {
            SeaOrmRunner::Conn(db) => Ok(self.inner.exec(db).await?),
            SeaOrmRunner::Tx(tx) => Ok(self.inner.exec(tx).await?),
        }
    }

    /// Unwrap the inner `SeaORM` `UpdateMany` for advanced use cases.
    ///
    /// # Safety
    /// The caller must ensure they don't remove or bypass the security
    /// conditions that were applied during `.scope_with()`.
    #[must_use]
    pub fn into_inner(self) -> sea_orm::UpdateMany<E> {
        self.inner
    }
}

/// A type-safe wrapper around `SeaORM`'s `DeleteMany` that enforces scoping.
///
/// This wrapper uses the typestate pattern to ensure that delete operations
/// cannot be executed without first applying access control via `.scope_with()`.
///
/// # Example
/// ```ignore
/// use toolkit_db::secure::{AccessScope, SecureDeleteExt};
///
/// let scope = AccessScope::for_tenants(vec![tenant_id]);
/// let result = user::Entity::delete_many()
///     .filter(user::Column::Status.eq("inactive"))
///     .secure()           // Returns SecureDeleteMany<E, Unscoped>
///     .scope_with(&scope)? // Returns SecureDeleteMany<E, Scoped>
///     .exec(conn)         // Now can execute
///     .await?;
/// ```
#[derive(Clone, Debug)]
pub struct SecureDeleteMany<E: EntityTrait, S> {
    pub(crate) inner: sea_orm::DeleteMany<E>,
    pub(crate) _state: PhantomData<S>,
}

/// Extension trait to convert a regular `SeaORM` `DeleteMany` into a `SecureDeleteMany`.
pub trait SecureDeleteExt<E: EntityTrait>: Sized {
    /// Convert this delete operation into a secure (unscoped) delete.
    /// You must call `.scope_with()` before executing.
    fn secure(self) -> SecureDeleteMany<E, Unscoped>;
}

impl<E> SecureDeleteExt<E> for sea_orm::DeleteMany<E>
where
    E: EntityTrait,
{
    fn secure(self) -> SecureDeleteMany<E, Unscoped> {
        SecureDeleteMany {
            inner: self,
            _state: PhantomData,
        }
    }
}

// Methods available only on Unscoped deletes
impl<E> SecureDeleteMany<E, Unscoped>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    /// Apply access control scope to this delete, transitioning to the `Scoped` state.
    ///
    /// This applies the implicit policy:
    /// - Empty scope → deny all (no rows deleted)
    /// - Tenants only → delete only in specified tenants
    /// - Resources only → delete only specified resource IDs
    /// - Both → AND them together
    ///
    #[must_use]
    pub fn scope_with(self, scope: &AccessScope) -> SecureDeleteMany<E, Scoped> {
        let cond = build_scope_condition::<E>(scope);
        SecureDeleteMany {
            inner: self.inner.filter(cond),
            _state: PhantomData,
        }
    }
}

// Methods available only on Scoped deletes
impl<E> SecureDeleteMany<E, Scoped>
where
    E: EntityTrait,
{
    /// Add additional filters to the scoped delete.
    /// The scope conditions remain in place.
    #[must_use]
    pub fn filter(mut self, filter: sea_orm::Condition) -> Self {
        self.inner = QueryFilter::filter(self.inner, filter);
        self
    }

    /// Execute the delete operation.
    ///
    /// # Errors
    /// Returns `ScopeError::Db` if the database operation fails.
    #[allow(clippy::disallowed_methods)]
    pub async fn exec(self, runner: &impl DBRunner) -> Result<sea_orm::DeleteResult, ScopeError> {
        match DBRunnerInternal::as_seaorm(runner) {
            SeaOrmRunner::Conn(db) => Ok(self.inner.exec(db).await?),
            SeaOrmRunner::Tx(tx) => Ok(self.inner.exec(tx).await?),
        }
    }

    /// Unwrap the inner `SeaORM` `DeleteMany` for advanced use cases.
    ///
    /// # Safety
    /// The caller must ensure they don't remove or bypass the security
    /// conditions that were applied during `.scope_with()`.
    #[must_use]
    pub fn into_inner(self) -> sea_orm::DeleteMany<E> {
        self.inner
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use sea_orm::entity::prelude::*;

    // Test entity with tenant_col for SecureOnConflict tests
    mod test_entity {
        use super::*;
        use toolkit_security::pep_properties;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "test_table")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: Uuid,
            pub tenant_id: Uuid,
            pub name: String,
            pub value: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl ScopableEntity for Entity {
            fn tenant_col() -> Option<Column> {
                Some(Column::TenantId)
            }
            fn resource_col() -> Option<Column> {
                Some(Column::Id)
            }
            fn owner_col() -> Option<Column> {
                None
            }
            fn type_col() -> Option<Column> {
                None
            }
            fn resolve_property(property: &str) -> Option<Column> {
                match property {
                    pep_properties::OWNER_TENANT_ID => Self::tenant_col(),
                    pep_properties::RESOURCE_ID => Self::resource_col(),
                    _ => None,
                }
            }
            fn scope_columns() -> Vec<Column> {
                vec![Column::TenantId, Column::Id]
            }
        }
    }

    #[test]
    fn insert_from_select_refuses_a_scoped_entity() {
        // The whole justification for skipping per-row scope validation is
        // that there is nothing to validate. An entity that declares a scope
        // column has something, and must not be written this way.
        let err = insert_from_select_allowed::<test_entity::Entity>()
            .expect_err("a tenant-scoped entity must be refused");
        assert!(matches!(err, ScopeError::Invalid(_)), "got: {err:?}");
    }

    #[test]
    fn insert_from_select_allows_an_entity_with_no_scope_columns() {
        // Negative control: without this the rule above would be satisfied by
        // a guard that refuses everything.
        //
        // Not `global_entity` -- that one is global only in the tenant sense
        // and still declares `resource_col`, so the guard refuses it, which is
        // correct. A join or closure table with no identity of its own is the
        // shape this helper exists for.
        insert_from_select_allowed::<unscoped_entity::Entity>()
            .expect("an entity with no scope columns must be allowed");
    }

    /// A link table: two foreign keys and a value, no identity the scope
    /// system could filter on. `resource_group_closure` is the real one.
    mod unscoped_entity {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "unscoped_table")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub left_id: Uuid,
            #[sea_orm(primary_key, auto_increment = false)]
            pub right_id: Uuid,
            pub depth: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl ScopableEntity for Entity {
            fn tenant_col() -> Option<Column> {
                None
            }
            fn resource_col() -> Option<Column> {
                None
            }
            fn owner_col() -> Option<Column> {
                None
            }
            fn type_col() -> Option<Column> {
                None
            }
            fn resolve_property(_property: &str) -> Option<Column> {
                None
            }
            fn scope_columns() -> Vec<Column> {
                Vec::new()
            }
        }
    }

    // Test entity without tenant_col (global entity)
    mod global_entity {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "global_table")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: Uuid,
            pub config_key: String,
            pub config_value: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl ScopableEntity for Entity {
            fn tenant_col() -> Option<Column> {
                None // Global entity - no tenant column
            }
            fn resource_col() -> Option<Column> {
                Some(Column::Id)
            }
            fn owner_col() -> Option<Column> {
                None
            }
            fn type_col() -> Option<Column> {
                None
            }
            fn resolve_property(property: &str) -> Option<Column> {
                match property {
                    "id" => Self::resource_col(),
                    _ => None,
                }
            }
            fn scope_columns() -> Vec<Column> {
                vec![Column::Id]
            }
        }
    }

    #[test]
    fn test_validate_tenant_in_scope() {
        let tenant_id = uuid::Uuid::new_v4();
        let scope = crate::secure::AccessScope::for_tenants(vec![tenant_id]);

        assert!(validate_tenant_in_scope(tenant_id, &scope).is_ok());

        let other_id = uuid::Uuid::new_v4();
        assert!(validate_tenant_in_scope(other_id, &scope).is_err());
    }

    // Note: Full integration tests with database require actual SeaORM entities
    // These tests verify the typestate pattern compiles correctly

    #[test]
    fn test_typestate_compile_check() {
        // This test verifies the typestate markers compile
        let unscoped: PhantomData<Unscoped> = PhantomData;
        let scoped: PhantomData<Scoped> = PhantomData;
        // Use the variables to avoid unused warnings
        let _ = (unscoped, scoped);
    }

    #[test]
    fn test_tenant_not_in_scope_returns_error() {
        // Verify that validate_tenant_in_scope properly rejects tenant IDs not in scope
        let allowed_tenant = uuid::Uuid::new_v4();
        let disallowed_tenant = uuid::Uuid::new_v4();
        let scope = crate::secure::AccessScope::for_tenants(vec![allowed_tenant]);

        // Allowed tenant should succeed
        assert!(validate_tenant_in_scope(allowed_tenant, &scope).is_ok());

        // Disallowed tenant should fail with TenantNotInScope error
        let result = validate_tenant_in_scope(disallowed_tenant, &scope);
        assert!(result.is_err());
        match result {
            Err(ScopeError::TenantNotInScope { tenant_id }) => {
                assert_eq!(tenant_id, disallowed_tenant);
            }
            _ => panic!("Expected TenantNotInScope error"),
        }
    }

    #[test]
    fn test_empty_scope_denied_for_tenant_scoped() {
        // Verify that an empty scope (no tenants) is rejected for tenant-scoped inserts
        let tenant_id = uuid::Uuid::new_v4();
        let empty_scope = crate::secure::AccessScope::default();

        let result = validate_tenant_in_scope(tenant_id, &empty_scope);
        assert!(result.is_err());
        match result {
            Err(ScopeError::Denied(_)) => {}
            _ => panic!("Expected Denied error for empty scope"),
        }
    }

    // SecureOnConflict tests

    #[test]
    fn test_secure_on_conflict_update_columns_allows_non_tenant_columns() {
        use test_entity::{Column, Entity};

        // update_columns with non-tenant columns should succeed
        let result = SecureOnConflict::<Entity>::columns([Column::Id])
            .update_columns([Column::Name, Column::Value]);

        assert!(result.is_ok());
    }

    #[test]
    fn test_secure_on_conflict_update_columns_rejects_tenant_column() {
        use test_entity::{Column, Entity};

        // update_columns with tenant_id should fail
        let result = SecureOnConflict::<Entity>::columns([Column::Id]).update_columns([
            Column::Name,
            Column::TenantId,
            Column::Value,
        ]);

        assert!(result.is_err());
        match result {
            Err(ScopeError::Denied(msg)) => {
                assert!(msg.contains("immutable"), "Expected immutable error: {msg}");
            }
            _ => panic!("Expected Denied error for tenant_id in update_columns"),
        }
    }

    #[test]
    fn test_secure_on_conflict_value_allows_non_tenant_columns() {
        use sea_orm::sea_query::Expr;
        use test_entity::{Column, Entity};

        // value() with non-tenant column should succeed
        let result = SecureOnConflict::<Entity>::columns([Column::Id])
            .value(Column::Name, Expr::value("test"));

        assert!(result.is_ok());
    }

    #[test]
    fn test_secure_on_conflict_value_rejects_tenant_column() {
        use sea_orm::sea_query::Expr;
        use test_entity::{Column, Entity};

        // value() with tenant_id should fail
        let result = SecureOnConflict::<Entity>::columns([Column::Id])
            .value(Column::TenantId, Expr::value(uuid::Uuid::new_v4()));

        assert!(result.is_err());
        match result {
            Err(ScopeError::Denied(msg)) => {
                assert!(msg.contains("immutable"), "Expected immutable error: {msg}");
            }
            _ => panic!("Expected Denied error for tenant_id in value()"),
        }
    }

    #[test]
    fn test_secure_on_conflict_chained_value_rejects_tenant_column() {
        use sea_orm::sea_query::Expr;
        use test_entity::{Column, Entity};

        // Chaining value() calls - should fail when tenant_id is added
        let result = SecureOnConflict::<Entity>::columns([Column::Id])
            .value(Column::Name, Expr::value("test"))
            .and_then(|c| c.value(Column::TenantId, Expr::value(uuid::Uuid::new_v4())));

        assert!(result.is_err());
        match result {
            Err(ScopeError::Denied(msg)) => {
                assert!(msg.contains("immutable"), "Expected immutable error: {msg}");
            }
            _ => panic!("Expected Denied error for tenant_id in chained value()"),
        }
    }

    #[test]
    fn test_secure_on_conflict_global_entity_allows_all_columns() {
        use global_entity::{Column, Entity};

        // Global entity has no tenant_col, so all columns are allowed
        let result = SecureOnConflict::<Entity>::columns([Column::Id])
            .update_columns([Column::ConfigKey, Column::ConfigValue]);

        assert!(result.is_ok());
    }

    #[test]
    fn test_secure_on_conflict_build_produces_on_conflict() {
        use test_entity::{Column, Entity};

        // Verify that build() produces a valid OnConflict
        let on_conflict = SecureOnConflict::<Entity>::columns([Column::Id])
            .update_columns([Column::Name, Column::Value])
            .expect("should succeed")
            .build();

        // The OnConflict should be usable (we can't easily test its internals,
        // but we can verify it doesn't panic)
        _ = format!("{on_conflict:?}");
    }

    // ── validate_insert_scope tests ─────────────────────────────────

    // Test entity with owner_col and a custom pep_prop (city_id),
    // mimicking the Address entity from the users-info example.
    mod owner_entity {
        use super::*;
        use toolkit_security::pep_properties;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "addresses")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: Uuid,
            pub tenant_id: Uuid,
            pub user_id: Uuid,
            pub city_id: Uuid,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl ScopableEntity for Entity {
            fn tenant_col() -> Option<Column> {
                Some(Column::TenantId)
            }
            fn resource_col() -> Option<Column> {
                Some(Column::Id)
            }
            fn owner_col() -> Option<Column> {
                Some(Column::UserId)
            }
            fn type_col() -> Option<Column> {
                None
            }
            fn resolve_property(property: &str) -> Option<Column> {
                match property {
                    pep_properties::OWNER_TENANT_ID => Some(Column::TenantId),
                    pep_properties::RESOURCE_ID => Some(Column::Id),
                    pep_properties::OWNER_ID => Some(Column::UserId),
                    "city_id" => Some(Column::CityId),
                    _ => None,
                }
            }
            fn scope_columns() -> Vec<Column> {
                vec![Column::TenantId, Column::Id, Column::UserId, Column::CityId]
            }
        }
    }

    #[test]
    fn test_validate_insert_scope_allow_all_passes() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;

        let scope = crate::secure::AccessScope::allow_all();
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(Uuid::new_v4()),
            user_id: Set(Uuid::new_v4()),
            city_id: Set(Uuid::new_v4()),
        };
        assert!(validate_insert_scope(&am, &scope).is_ok());
    }

    #[test]
    fn test_validate_insert_scope_deny_all_rejects() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;

        let scope = crate::secure::AccessScope::deny_all();
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(Uuid::new_v4()),
            user_id: Set(Uuid::new_v4()),
            city_id: Set(Uuid::new_v4()),
        };
        assert!(validate_insert_scope(&am, &scope).is_err());
    }

    #[test]
    fn test_validate_insert_scope_tenant_only_matches() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;

        let tenant_id = Uuid::new_v4();
        let scope = crate::secure::AccessScope::for_tenant(tenant_id);
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(Uuid::new_v4()),
            city_id: Set(Uuid::new_v4()),
        };
        assert!(validate_insert_scope(&am, &scope).is_ok());
    }

    #[test]
    fn test_validate_insert_scope_tenant_mismatch_rejects() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;

        let tenant_id = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();
        let scope = crate::secure::AccessScope::for_tenant(tenant_id);
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(other_tenant),
            user_id: Set(Uuid::new_v4()),
            city_id: Set(Uuid::new_v4()),
        };
        assert!(validate_insert_scope(&am, &scope).is_err());
    }

    #[test]
    fn test_validate_insert_scope_owner_id_matches() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;
        use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};
        use toolkit_security::pep_properties;

        let tenant_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let city_id = Uuid::new_v4();

        // Scope: tenant + owner_id + city_id (all must match)
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
            ScopeFilter::eq("city_id", city_id),
        ])]);

        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(user_id),
            city_id: Set(city_id),
        };
        assert!(
            validate_insert_scope(&am, &scope).is_ok(),
            "Insert should pass when all properties match"
        );
    }

    #[test]
    fn test_validate_insert_scope_owner_id_mismatch_rejects() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;
        use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};
        use toolkit_security::pep_properties;

        let tenant_id = Uuid::new_v4();
        let user_a = Uuid::new_v4();
        let user_b = Uuid::new_v4();
        let city_id = Uuid::new_v4();

        // Scope says owner_id must be user_a
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::eq(pep_properties::OWNER_ID, user_a),
            ScopeFilter::eq("city_id", city_id),
        ])]);

        // But ActiveModel has user_id = user_b
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(user_b),
            city_id: Set(city_id),
        };
        assert!(
            validate_insert_scope(&am, &scope).is_err(),
            "Insert must be rejected when owner_id doesn't match"
        );
    }

    #[test]
    fn test_validate_insert_scope_city_id_mismatch_rejects() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;
        use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};
        use toolkit_security::pep_properties;

        let tenant_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let allowed_city = Uuid::new_v4();
        let disallowed_city = Uuid::new_v4();

        // Scope says city_id must be allowed_city
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
            ScopeFilter::eq("city_id", allowed_city),
        ])]);

        // But ActiveModel has city_id = disallowed_city
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(user_id),
            city_id: Set(disallowed_city),
        };
        assert!(
            validate_insert_scope(&am, &scope).is_err(),
            "Insert must be rejected when city_id doesn't match"
        );
    }

    #[test]
    fn test_validate_insert_scope_or_semantics() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;
        use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};
        use toolkit_security::pep_properties;

        let tenant_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let city_1 = Uuid::new_v4();
        let city_2 = Uuid::new_v4();

        // Two constraints (OR-ed): user allowed in city_1 OR city_2
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![
                ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
                ScopeFilter::eq("city_id", city_1),
            ]),
            ScopeConstraint::new(vec![
                ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
                ScopeFilter::eq("city_id", city_2),
            ]),
        ]);

        // Insert with city_2 — matches second constraint
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(user_id),
            city_id: Set(city_2),
        };
        assert!(
            validate_insert_scope(&am, &scope).is_ok(),
            "Insert should pass when matching any constraint (OR semantics)"
        );

        // Insert with city_3 — matches neither
        let city_3 = Uuid::new_v4();
        let am_bad = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(user_id),
            city_id: Set(city_3),
        };
        assert!(
            validate_insert_scope(&am_bad, &scope).is_err(),
            "Insert must be rejected when no constraint matches"
        );
    }

    #[test]
    fn test_validate_insert_scope_unknown_property_fails_closed() {
        use owner_entity::ActiveModel;
        use sea_orm::Set;
        use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};
        use toolkit_security::pep_properties;

        let tenant_id = Uuid::new_v4();

        // Constraint with an unknown property
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::eq("nonexistent_prop", Uuid::new_v4()),
        ])]);

        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(Uuid::new_v4()),
            city_id: Set(Uuid::new_v4()),
        };
        assert!(
            validate_insert_scope(&am, &scope).is_err(),
            "Unknown property must cause constraint to fail (fail-closed)"
        );
    }

    /// Test entity whose scope-resolvable column is a type
    /// `sea_value_to_scope_value` does not support, used to prove the
    /// unsupported-type path fails closed rather than allowing the insert.
    mod unsupported_value_entity {
        use super::*;
        use toolkit_security::pep_properties;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "unsupported_value_table")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: Uuid,
            pub tenant_id: Uuid,
            /// `f64` becomes `Value::Double`, which the scope-value converter
            /// has no `ScopeValue` mapping for.
            pub score: f64,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl ScopableEntity for Entity {
            fn tenant_col() -> Option<Column> {
                Some(Column::TenantId)
            }
            fn resource_col() -> Option<Column> {
                Some(Column::Id)
            }
            fn owner_col() -> Option<Column> {
                None
            }
            fn type_col() -> Option<Column> {
                None
            }
            fn resolve_property(property: &str) -> Option<Column> {
                match property {
                    pep_properties::OWNER_TENANT_ID => Self::tenant_col(),
                    "score" => Some(Column::Score),
                    _ => None,
                }
            }
            fn scope_columns() -> Vec<Column> {
                vec![Column::TenantId, Column::Id, Column::Score]
            }
        }
    }

    // -----------------------------------------------------------------------
    // sea_value_to_scope_value
    //
    // This is the bridge between a column's stored `sea_orm::Value` and the
    // `ScopeValue` an access-scope filter compares against. A variant it fails
    // to recognise makes `validate_insert_scope` fail closed, so the arm list
    // is security-relevant, not cosmetic.
    // -----------------------------------------------------------------------

    #[test]
    fn sea_value_to_scope_value_maps_uuid() {
        let id = Uuid::from_u128(0x91);
        assert_eq!(
            sea_value_to_scope_value(&sea_orm::Value::Uuid(Some(id))),
            Some(toolkit_security::ScopeValue::Uuid(id))
        );
    }

    /// A UUID stored in a text column must still compare as a `Uuid`, so a
    /// scope filter written with UUIDs matches a `VARCHAR` tenant column too.
    #[test]
    fn sea_value_to_scope_value_parses_uuid_shaped_strings_as_uuid() {
        let id = Uuid::from_u128(0x92);
        assert_eq!(
            sea_value_to_scope_value(&sea_orm::Value::String(Some(id.to_string()))),
            Some(toolkit_security::ScopeValue::Uuid(id))
        );
    }

    #[test]
    fn sea_value_to_scope_value_keeps_non_uuid_strings_as_strings() {
        assert_eq!(
            sea_value_to_scope_value(&sea_orm::Value::String(Some("draft".to_owned()))),
            Some(toolkit_security::ScopeValue::String("draft".to_owned()))
        );
    }

    /// Every integer width widens to `ScopeValue::Int(i64)`, so a scope filter
    /// need not know the column's storage width.
    #[test]
    fn sea_value_to_scope_value_widens_every_integer_width_to_i64() {
        let cases = [
            sea_orm::Value::BigInt(Some(7i64)),
            sea_orm::Value::Int(Some(7i32)),
            sea_orm::Value::SmallInt(Some(7i16)),
            sea_orm::Value::TinyInt(Some(7i8)),
        ];
        for v in cases {
            assert_eq!(
                sea_value_to_scope_value(&v),
                Some(toolkit_security::ScopeValue::Int(7)),
                "unexpected mapping for {v:?}"
            );
        }
    }

    // -- batch-insert chunk sizing --

    #[test]
    fn rows_per_insert_stays_under_each_backend_hard_limit() {
        // 4 columns on the test entity. The product of rows and columns must
        // stay under the real wire limit, not just under our own budget:
        // 65535 for Postgres/MySQL, 32766 for modern SQLite.
        let columns = <test_entity::Column as sea_orm::Iterable>::iter().count();
        for (backend, hard_limit) in [
            (sea_orm::DbBackend::Postgres, 65_535),
            (sea_orm::DbBackend::MySql, 65_535),
            (sea_orm::DbBackend::Sqlite, 32_766),
        ] {
            let rows = rows_per_insert::<test_entity::Entity>(backend);
            assert!(
                rows * columns < hard_limit,
                "{backend:?}: {rows} rows x {columns} cols exceeds {hard_limit}"
            );
            assert!(
                rows > 1,
                "{backend:?}: a 4-column entity should batch freely"
            );
        }
    }

    #[test]
    fn sea_value_to_scope_value_maps_bool() {
        assert_eq!(
            sea_value_to_scope_value(&sea_orm::Value::Bool(Some(true))),
            Some(toolkit_security::ScopeValue::Bool(true))
        );
    }

    /// Unsupported variants and SQL NULLs must return `None` so the caller fails
    /// closed instead of treating an unknown value as a match.
    #[test]
    fn sea_value_to_scope_value_returns_none_for_unsupported_and_null_values() {
        let unsupported = [
            sea_orm::Value::Double(Some(1.5)),
            sea_orm::Value::Float(Some(1.5)),
            sea_orm::Value::Uuid(None),
            sea_orm::Value::String(None),
            sea_orm::Value::BigInt(None),
            sea_orm::Value::Bool(None),
        ];
        for v in unsupported {
            assert_eq!(
                sea_value_to_scope_value(&v),
                None,
                "{v:?} must not map to a ScopeValue"
            );
        }
    }

    // -----------------------------------------------------------------------
    // validate_insert_scope — the two branches the existing suite misses
    // -----------------------------------------------------------------------

    /// A filter on a column the insert does not set (auto-generated, defaulted)
    /// must be *skipped*, not treated as a mismatch — otherwise every insert
    /// that omits an optional scoped column would be denied.
    #[test]
    fn validate_insert_scope_skips_filters_on_not_set_columns() {
        use owner_entity::ActiveModel;
        use sea_orm::{NotSet, Set};
        use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};
        use toolkit_security::pep_properties;

        let tenant_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
            // Resolvable, but the ActiveModel leaves it NotSet below.
            ScopeFilter::eq("city_id", Uuid::new_v4()),
        ])]);

        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            user_id: Set(user_id),
            city_id: NotSet,
        };

        assert!(
            validate_insert_scope(&am, &scope).is_ok(),
            "a filter on a NotSet column must be skipped, not denied"
        );
    }

    /// If a resolvable column's value has a type the scope-value converter does
    /// not understand, the constraint must fail closed — comparing an unknown
    /// value optimistically would let an out-of-scope row through.
    #[test]
    fn validate_insert_scope_denies_when_column_type_is_unsupported() {
        use sea_orm::Set;
        use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};
        use toolkit_security::pep_properties;
        use unsupported_value_entity::ActiveModel;

        let tenant_id = Uuid::new_v4();

        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            // `score` resolves to an f64 column -> Value::Double -> no ScopeValue.
            ScopeFilter::eq("score", Uuid::new_v4()),
        ])]);

        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            score: Set(0.5),
        };

        assert!(
            matches!(
                validate_insert_scope(&am, &scope),
                Err(ScopeError::Denied(_))
            ),
            "an unsupported column type must fail closed"
        );
    }

    // -----------------------------------------------------------------------
    // SecureInsertMany — the multi-row wrapper added for SeaORM 2.0's split of
    // insert_many out of Insert<A>. These exercise the builder/type-state half,
    // which needs no database; the exec paths are covered by the sqlite suite.
    // -----------------------------------------------------------------------

    /// The type-state gate: a deny-all scope must be rejected at
    /// `scope_unchecked`, before any statement can be executed.
    #[test]
    fn secure_insert_many_rejects_deny_all_scope() {
        use sea_orm::Set;
        use test_entity::{ActiveModel, Entity};

        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(Uuid::new_v4()),
            name: Set("row".to_owned()),
            value: Set(1),
        };

        let result = Entity::insert_many([am])
            .secure()
            .scope_unchecked(&AccessScope::default());

        assert!(
            matches!(result, Err(ScopeError::Denied(_))),
            "a deny-all scope must not reach the Scoped state"
        );
    }

    #[test]
    fn secure_insert_many_accepts_a_scope_with_constraints() {
        use sea_orm::Set;
        use test_entity::{ActiveModel, Entity};

        let tenant_id = Uuid::new_v4();
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            name: Set("row".to_owned()),
            value: Set(1),
        };

        assert!(
            Entity::insert_many([am])
                .secure()
                .scope_unchecked(&AccessScope::for_tenants(vec![tenant_id]))
                .is_ok()
        );
    }

    /// `into_inner` is the documented escape hatch; the built statement must
    /// still carry the ON CONFLICT clause set through the secure wrapper,
    /// otherwise an upsert would silently degrade to a plain insert.
    #[test]
    fn secure_insert_many_on_conflict_raw_survives_into_inner() {
        use sea_orm::{QueryTrait, Set};
        use test_entity::{ActiveModel, Column, Entity};

        let tenant_id = Uuid::new_v4();
        let am = ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(tenant_id),
            name: Set("row".to_owned()),
            value: Set(1),
        };

        let mut on_conflict = OnConflict::column(Column::Id);
        on_conflict.do_nothing();

        let insert = Entity::insert_many([am])
            .secure()
            .scope_unchecked(&AccessScope::for_tenants(vec![tenant_id]))
            .expect("scope with constraints is accepted")
            .on_conflict_raw(on_conflict)
            .into_inner();

        let sql = insert.build(sea_orm::DatabaseBackend::Postgres).to_string();
        assert!(
            sql.contains("ON CONFLICT"),
            "ON CONFLICT must survive the secure wrapper; got: {sql}"
        );
        assert!(
            sql.contains("DO NOTHING"),
            "the DO NOTHING action must be preserved; got: {sql}"
        );
    }

    /// The secure `on_conflict` path still enforces tenant immutability, so a
    /// caller cannot use an upsert to move a row between tenants.
    #[test]
    fn secure_insert_many_secure_on_conflict_rejects_tenant_column_update() {
        let result = SecureOnConflict::<test_entity::Entity>::columns([test_entity::Column::Id])
            .update_columns([test_entity::Column::TenantId]);

        assert!(
            matches!(result, Err(ScopeError::Denied(_))),
            "tenant_id must stay immutable through an upsert"
        );
    }

    #[test]
    fn rows_per_insert_never_returns_zero() {
        // Guards the division: an entity wider than the whole budget must still
        // make progress, one row per statement, rather than looping forever on a
        // chunk size of zero.
        assert!(rows_per_insert::<test_entity::Entity>(sea_orm::DbBackend::Sqlite) >= 1);
        for backend in [
            sea_orm::DbBackend::Postgres,
            sea_orm::DbBackend::MySql,
            sea_orm::DbBackend::Sqlite,
        ] {
            assert!(max_bind_params(backend) > 0);
        }
    }
}
