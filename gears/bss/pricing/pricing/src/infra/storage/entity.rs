//! `SeaORM` entities for the bss-pricing gear (schema `bss`).
//!
//! One module per physical table — the ten Foundation-owned ones and the
//! slice-owned tables that follow them — each tenant-scoped through
//! `SecureORM` (`#[secure(tenant_col = "tenant_id",...)]`) so cross-tenant
//! reads are denied in SQL rather than by a forgotten `WHERE` clause. Column
//! types are chosen to round-trip on **both** backends: `Uuid` reads from
//! Postgres `uuid` and `SQLite` `text`, `OffsetDateTime` from `timestamptz` and
//! `text`, `JsonValue` from `jsonb` and `text`, `Vec<u8>` from `bytea` and
//! `blob`.

/// Declare the entity modules and, from the same list, the roster of them.
///
/// **A second list of these names is a list that goes stale.** `rest_support`'s
/// denial census reads one plane per entity to prove a refused call wrote
/// nothing, and a table it has no plane for reads as a table nothing wrote — so
/// the roster has to come from the declarations rather than from a reader's
/// memory. Adding a module here is the whole cost of adding an entity.
macro_rules! entities {
    ($($module:ident),+ $(,)?) => {
        $(pub mod $module;)+

        /// Every entity module of this schema, in declaration order.
        ///
        /// Module names and not table names: a caller matching a roster against
        /// this one is naming the same things the `use` lists name.
        pub const MODULES: &[&str] = &[$(stringify!($module)),+];
    };
}

entities! {
    approval,
    approval_key,
    approval_threshold,
    approval_threshold_tombstone,
    audit_log,
    brand_taxonomy,
    bulk_operation,
    bulk_row_lock,
    bundle,
    bundle_component,
    bundle_revshare,
    bundle_revshare_group,
    catalog_version_ref,
    composite_meter,
    customer_group_taxonomy,
    gl_code_taxonomy,
    group_membership,
    idempotency_dedup,
    migration,
    operator_flag,
    org_tier_taxonomy,
    outbox,
    partner_taxonomy,
    pin_frontier,
    plan,
    plan_addon_rule,
    plan_descriptor_set,
    plan_period_floor_cap,
    plan_phase,
    policy_object,
    price,
    price_overlay,
    price_overlay_line,
    price_overlay_line_amount,
    price_tier_band,
    price_window,
    read_model,
    region_taxonomy,
    repricing_journal,
    rounding_policy_taxonomy,
    snapshot_provenance,
}
