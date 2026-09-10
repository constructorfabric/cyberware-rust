//! Migration set for the bss-pricing gear (schema `bss`).
//!
//! Greenfield chain, one migration per table, ordered by foreign-key tier: the
//! FK-free tables first (`000001`–`000028`), then the FK-bearing ones, and last the
//! two that reference a table inside that second block and so cannot sort with it.
//! Each table names its slice, its §6 and its reason for existing in **its own**
//! module doc. The constraint that actually binds is that a table sorts after every
//! table it references — not which slice owns it.
//!
//! **The per-slice enumeration that stood here is gone rather than extended**, and the
//! reason is what happened to it: it listed Slices 2, 3, 5 and 7's tables and then
//! silently stopped being the whole set when `pricing_approval_key` was appended, so
//! a reader who trusted it was told the register did not exist. `Migrator::migrations`
//! below is the roster; a second copy of it in prose is a second thing to keep true,
//! and the copy is the one that goes stale.
//!
//! Two shape rules the roster does not show, which is why they are worth prose:
//! **a table per migration**, so the chain reads as a list of decisions rather than a
//! script; and **columns a slice adds to an existing table are amended onto it**
//! rather than tabled separately — Slice 3's per-row price columns onto
//! `pricing_price`, Slice 2's per-revision columns onto `pricing_plan` — because a
//! side table keyed the same way as its parent is a join nobody needs and a second
//! place for one row's facts to live.
//!
//! **What a migration in this chain owes its own verification step.** A change to DDL,
//! to a trigger body or to a column *name* is narrow in the diff and narrow on
//! **neither engine**: `RENAME COLUMN` rewrites `SQLite` trigger bodies (since 3.25)
//! and does not rewrite a PL/pgSQL body, which Postgres keeps as opaque text in
//! `pg_proc.prosrc`. A rename verified against `SQLite` targets alone therefore leaves
//! the matching PL/pgSQL body raising `42703` on every UPDATE that reaches it, and a
//! repair verified against Postgres alone restores the pre-fix `SQLite` body verbatim.
//! One asymmetry, two defects available, and a green run reachable on either side.
//!
//! The rule that follows is narrower than "run everything": **a migration touching
//! DDL, a trigger or a column name owes one Postgres suite and one `SQLite` suite in
//! its own step list**, not in a batch gate at the end of a fix wave —
//! `make test-pricing-pg` (or `--test postgres_migrations --test postgres_schema_*`)
//! together with `--test sqlite_migrations`. `down_then_up_round_trips` and the
//! per-engine rosters are what catch both halves, and neither is reachable from the
//! other engine's tier.
//!
//! **A `DOWN` drops what its own `UP` created, and nothing else.** One table per
//! migration is what buys that shape: there is no predecessor state to restore and no
//! earlier `UP` to transcribe, so every `down` here is `DROP TABLE` plus, on Postgres,
//! that table's own `DROP FUNCTION`s. A chain of patches cannot hold this rule — its
//! `DOWN` has to reproduce a predecessor's text, which reintroduces whatever defect
//! that predecessor carried, and the reverse walk reaches the repair first.
//!
//! **Schema creation has its own migration**, `create_bss_schema`, first in the roster
//! and the only place `CREATE SCHEMA` appears in this chain. The shared `coord` lease
//! migration (whose `m0001_...` name sorts first under the toolkit runner's name
//! ordering) issues the same statement before its own `CREATE TABLE`; both are
//! `IF NOT EXISTS`, so the schema exists no matter which the runner reaches first.
//!
//! **Ordering.** The toolkit migration runner applies migrations in **name**
//! order, not vec order, and rejects a duplicate `DeriveMigrationName` outright
//! — which is what `tests/module_test.rs` asserts about this list, because a
//! duplicate name would otherwise be a migration that silently never runs.

pub mod m20260821_000001_create_bss_schema;
pub mod m20260821_000002_install_btree_gist;
pub mod m20260821_000003_create_pricing_approval;
pub mod m20260821_000004_create_pricing_approval_key;
pub mod m20260821_000005_create_pricing_approval_threshold;
pub mod m20260821_000006_create_pricing_approval_threshold_tombstone;
pub mod m20260821_000007_create_pricing_audit_log;
pub mod m20260821_000008_create_pricing_brand_taxonomy;
pub mod m20260821_000009_create_pricing_bulk_operation;
pub mod m20260821_000010_create_pricing_bundle;
pub mod m20260821_000011_create_pricing_catalog_version_ref;
pub mod m20260821_000012_create_pricing_customer_group_taxonomy;
pub mod m20260821_000013_create_pricing_group_membership;
pub mod m20260821_000014_create_pricing_idempotency_dedup;
pub mod m20260821_000015_create_pricing_migration;
pub mod m20260821_000016_create_pricing_operator_flag;
pub mod m20260821_000017_create_pricing_org_tier_taxonomy;
pub mod m20260821_000018_create_pricing_outbox;
pub mod m20260821_000019_create_pricing_partner_taxonomy;
pub mod m20260821_000020_create_pricing_pin_frontier;
pub mod m20260821_000021_create_pricing_plan;
pub mod m20260821_000022_create_pricing_policy_object;
pub mod m20260821_000023_create_pricing_price;
pub mod m20260821_000024_create_pricing_price_overlay;
pub mod m20260821_000025_create_pricing_read_model;
pub mod m20260821_000026_create_pricing_region_taxonomy;
pub mod m20260821_000027_create_pricing_rounding_policy_taxonomy;
pub mod m20260821_000028_create_pricing_snapshot_provenance;
pub mod m20260821_000029_create_pricing_bulk_row_lock;
pub mod m20260821_000030_create_pricing_bundle_component;
pub mod m20260821_000031_create_pricing_bundle_revshare_group;
pub mod m20260821_000032_create_pricing_composite_meter;
pub mod m20260821_000033_create_pricing_plan_addon_rule;
pub mod m20260821_000034_create_pricing_plan_descriptor_set;
pub mod m20260821_000035_create_pricing_plan_period_floor_cap;
pub mod m20260821_000036_create_pricing_plan_phase;
pub mod m20260821_000037_create_pricing_price_overlay_line;
pub mod m20260821_000038_create_pricing_price_tier_band;
pub mod m20260821_000039_create_pricing_price_window;
pub mod m20260821_000040_create_pricing_repricing_journal;
pub mod m20260821_000041_create_pricing_bundle_revshare;
pub mod m20260821_000042_create_pricing_price_overlay_line_amount;
pub mod m20260821_000043_create_pricing_gl_code_taxonomy;

use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

/// Run the backend's statement list, in order.
///
/// Every migration in this chain is a pair of raw-SQL const arrays (Postgres
/// canonical, `SQLite` mirror) plus this dispatch. Factored out rather than
/// copied ten times: the branch that must never drift is "which backend gets
/// which statements", and there is exactly one copy of it.
///
/// # Errors
/// [`DbErr::Migration`] for `MySQL` (not a supported backend for this gear), or the
/// driver's error for a failing statement — **wrapped with the migration that owns
/// it and the statement's index within that migration's list.** Every migration in the
/// chain runs one of these lists at boot, some of them eight statements long, and a
/// bare driver string names a constraint or a relation without saying which migration
/// was applying it or how far it got: the operator's first question after a failed
/// boot is exactly the one the unwrapped error does not answer.
pub(crate) async fn exec_backend(
    migration: &str,
    manager: &SchemaManager<'_>,
    postgres: &[&str],
    sqlite: &[&str],
) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    let conn = manager.get_connection();
    let statements: &[&str] = match backend {
        sea_orm::DatabaseBackend::Postgres => postgres,
        sea_orm::DatabaseBackend::Sqlite => sqlite,
        // `_`, not `MySql`. `DatabaseBackend` became `#[non_exhaustive]` in
        // sea-orm 2, so naming the one unsupported variant no longer covers the
        // match and a backend added by a later sea-orm would not compile here.
        // The catch-all is what this arm always meant: **this gear ships two
        // dialects and refuses every other one**, which is the same reading
        // `coord`'s own migration took upstream. The message keeps naming MySQL
        // because that is still the only backend anyone would try.
        _ => {
            return Err(DbErr::Migration(
                "MySQL is not supported for bss-pricing".to_owned(),
            ));
        }
    };
    for (index, sql) in statements.iter().enumerate() {
        conn.execute_raw(Statement::from_string(backend, (*sql).to_owned()))
            .await
            .map_err(|e| {
                DbErr::Migration(format!(
                    "{migration}: statement {} of {} failed on {backend:?}: {e}",
                    index + 1,
                    statements.len()
                ))
            })?;
    }
    Ok(())
}

/// The gear's migration chain.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260821_000001_create_bss_schema::Migration),
            Box::new(m20260821_000002_install_btree_gist::Migration),
            Box::new(m20260821_000003_create_pricing_approval::Migration),
            Box::new(m20260821_000004_create_pricing_approval_key::Migration),
            Box::new(m20260821_000005_create_pricing_approval_threshold::Migration),
            Box::new(m20260821_000006_create_pricing_approval_threshold_tombstone::Migration),
            Box::new(m20260821_000007_create_pricing_audit_log::Migration),
            Box::new(m20260821_000008_create_pricing_brand_taxonomy::Migration),
            Box::new(m20260821_000009_create_pricing_bulk_operation::Migration),
            Box::new(m20260821_000010_create_pricing_bundle::Migration),
            Box::new(m20260821_000011_create_pricing_catalog_version_ref::Migration),
            Box::new(m20260821_000012_create_pricing_customer_group_taxonomy::Migration),
            Box::new(m20260821_000013_create_pricing_group_membership::Migration),
            Box::new(m20260821_000014_create_pricing_idempotency_dedup::Migration),
            Box::new(m20260821_000015_create_pricing_migration::Migration),
            Box::new(m20260821_000016_create_pricing_operator_flag::Migration),
            Box::new(m20260821_000017_create_pricing_org_tier_taxonomy::Migration),
            Box::new(m20260821_000018_create_pricing_outbox::Migration),
            Box::new(m20260821_000019_create_pricing_partner_taxonomy::Migration),
            Box::new(m20260821_000020_create_pricing_pin_frontier::Migration),
            Box::new(m20260821_000021_create_pricing_plan::Migration),
            Box::new(m20260821_000022_create_pricing_policy_object::Migration),
            Box::new(m20260821_000023_create_pricing_price::Migration),
            Box::new(m20260821_000024_create_pricing_price_overlay::Migration),
            Box::new(m20260821_000025_create_pricing_read_model::Migration),
            Box::new(m20260821_000026_create_pricing_region_taxonomy::Migration),
            Box::new(m20260821_000027_create_pricing_rounding_policy_taxonomy::Migration),
            Box::new(m20260821_000028_create_pricing_snapshot_provenance::Migration),
            Box::new(m20260821_000029_create_pricing_bulk_row_lock::Migration),
            Box::new(m20260821_000030_create_pricing_bundle_component::Migration),
            Box::new(m20260821_000031_create_pricing_bundle_revshare_group::Migration),
            Box::new(m20260821_000032_create_pricing_composite_meter::Migration),
            Box::new(m20260821_000033_create_pricing_plan_addon_rule::Migration),
            Box::new(m20260821_000034_create_pricing_plan_descriptor_set::Migration),
            Box::new(m20260821_000035_create_pricing_plan_period_floor_cap::Migration),
            Box::new(m20260821_000036_create_pricing_plan_phase::Migration),
            Box::new(m20260821_000037_create_pricing_price_overlay_line::Migration),
            Box::new(m20260821_000038_create_pricing_price_tier_band::Migration),
            Box::new(m20260821_000039_create_pricing_price_window::Migration),
            Box::new(m20260821_000040_create_pricing_repricing_journal::Migration),
            Box::new(m20260821_000041_create_pricing_bundle_revshare::Migration),
            Box::new(m20260821_000042_create_pricing_price_overlay_line_amount::Migration),
            Box::new(m20260821_000043_create_pricing_gl_code_taxonomy::Migration),
            // Shared `coord_leases` table, owned by the `coord` crate. This gear's
            // background work is coordinated as a singleton (§3.8: background work
            // is coordinated as a singleton via the coordination lease library),
            // so it needs the lease table the same way the sibling ledger needs it
            // for its recognition run — one row per leased ticker, each on its own
            // key. Qualified into `bss` so it lands
            // there regardless of the connection's `search_path` order; its
            // `m0001_...` name sorts FIRST under the runner's name ordering, so
            // its `up` creates the schema itself before the `CREATE TABLE`.
            Box::new(coord::migration::Migration::in_schema("bss")),
        ]
    }
}
