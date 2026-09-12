//! `pricing_gl_code_taxonomy` — the general-ledger codes a tenant declares
//! (D-356).
//!
//! # Why a declared set, and why the list is this gear's to hold
//!
//! A plan's billing-descriptor `glCode` is a D-48 v1 element that freezes into
//! every `CatalogVersion` an ERP posts against, and before D-356 it was **free
//! text validated for presence alone** (`inst-ds-required`). A typo'd or
//! non-existent code therefore froze into an immutable, seven-year version and
//! was met at posting time rather than by its author.
//!
//! The obvious remedy — validate against ledger — has nothing to validate
//! against. Ledger's own PRD stores account **class** and takes the concrete
//! `glCode` as a snapshot *from Catalog* at post time; its chart of accounts has
//! no `gl_code` column, and the column appears only on a posted journal line.
//! Catalog is already the source; the gap was that it accepted any string. So
//! the fix is the one D-334 made for `rounding_policy_ref`: **the tenant
//! declares the vocabulary and the gear refuses references outside it**. The
//! tenant enters its ERP's codes here, which is exactly the "tenant/ERP
//! configured" ledger's PRD describes. This gear still neither defines nor
//! applies a GL code and invents no semantics for `4000-REV`.
//!
//! # The provider seam
//!
//! The publish rule reads a set the caller resolves from this table, and the
//! rule never learns where the set came from. Today the provider is the
//! tenant-declared `PUT /bss-pricing/v1/config/gl-codes`; a future ERP gear
//! (D-356 *Owed*) populates or reconciles **this same table** — a provider swap,
//! not a rewrite — exactly as `RegionTaxReadiness` is tenant-declared today and
//! reconciled against Tax Engine post-GA (D-01).
//!
//! # The shape is the taxonomies', deliberately
//!
//! `(tenant_id, value)` primary key, a `display_name`, and `state IN ('active',
//! 'retired')` — `pricing_rounding_policy_taxonomy` with its name changed. A
//! retired value keeps resolving for the published descriptor sets that already
//! name it and cannot be newly authored, which is what retirement means on every
//! other taxonomy here. It is **not** a fifth `TaxonomyClass`: a GL code scopes
//! no overlay.
//!
//! # The value predicate is D-242's
//!
//! `length(btrim(value, <ascii whitespace>)) > 0` on Postgres,
//! `length(trim(value, <ascii whitespace>)) > 0` on `SQLite` — **not**
//! `length(value) > 0`, which admits `'   '`, and not the one-argument trim, which
//! strips spaces alone and admits a tab. `ScopeValue::new` refuses every one of
//! them. `pricing_region_taxonomy`'s doc carries the argument.
//!
//! # Empty means unconstrained, and that is a decision rather than an oversight
//!
//! A tenant that has declared nothing is not refused: the check binds only where
//! a vocabulary exists. Declaring the set is how a tenant opts into the
//! constraint, and the alternative — an empty set refusing every plan — would
//! fail every existing catalog the moment the table landed, for a vocabulary
//! nobody has had a chance to write. `pricing_rounding_policy_taxonomy` carries
//! the same clause for the same reason.
//!
//! Dependency level 0.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const PG_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE bss.pricing_gl_code_taxonomy (
            tenant_id    uuid NOT NULL,
            value        text NOT NULL,
            display_name text NOT NULL,
            state        text NOT NULL DEFAULT 'active'::text,
            CONSTRAINT chk_pricing_gl_code_taxonomy_state CHECK (state IN ('active', 'retired')),
            CONSTRAINT chk_pricing_gl_code_taxonomy_value_present CHECK ((length(btrim(value, chr(9) || chr(10) || chr(11) || chr(12) || chr(13) || chr(32))) > 0)),
            CONSTRAINT pricing_gl_code_taxonomy_pkey PRIMARY KEY (tenant_id, value)
        )",
];

const PG_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS bss.pricing_gl_code_taxonomy"];

const SQLITE_UP_STATEMENTS: &[&str] = &[
    "CREATE TABLE pricing_gl_code_taxonomy (
            tenant_id    text NOT NULL,
            value        text NOT NULL,
            display_name text NOT NULL,
            state        text NOT NULL DEFAULT 'active',
            PRIMARY KEY (tenant_id, value),
            CONSTRAINT chk_pricing_gl_code_taxonomy_state CHECK (state IN ('active', 'retired')),
            CONSTRAINT chk_pricing_gl_code_taxonomy_value_present CHECK (length(trim(value, char(9,10,11,12,13,32))) > 0)
        )",
];

const SQLITE_DOWN_STATEMENTS: &[&str] = &["DROP TABLE IF EXISTS pricing_gl_code_taxonomy"];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        super::exec_backend(self.name(), manager, PG_UP_STATEMENTS, SQLITE_UP_STATEMENTS).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        super::exec_backend(
            self.name(),
            manager,
            PG_DOWN_STATEMENTS,
            SQLITE_DOWN_STATEMENTS,
        )
        .await
    }
}
