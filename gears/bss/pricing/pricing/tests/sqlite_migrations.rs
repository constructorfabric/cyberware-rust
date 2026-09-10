//! The migration chain — the Foundation tables and the slice-owned ones that
//! follow them — exercised end to end on an in-memory `SQLite`.
//!
//! Three properties, each a real boot failure mode:
//!
//! 1. **Completeness, and agreement with the entities** — after `up`, every
//!    table can be read through its `SeaORM` entity. That is a stronger check
//!    than "the table exists": `SeaORM` names every column in the `SELECT`, so
//!    a migration and an entity that disagree about a column fail here rather
//!    than at the first production read.
//! 2. **Re-run safety** — a second boot over the same database applies nothing
//!    and skips everything. The sibling ledger carries a whole Postgres
//!    regression for the version of this that bit it (bookkeeping landing in the
//!    wrong schema made every migration re-run and a non-`IF NOT EXISTS`
//!    `CREATE TABLE` abort in a crash loop); the cheap half of that check
//!    belongs in the fast suite.
//! 3. **Reversibility** — `down` then `up` round-trips, so a rollback leaves a
//!    database the chain can walk forward again rather than a half-dropped one.
//!    This one introspects `sqlite_master` directly, because it is also where
//!    the shared `coord_leases` table (spliced in for the singleton warm
//!    re-drive) is checked; `coord` does not export its entity.
//!
//! 4. **The object census** — every trigger and every index the chain creates, by
//!    name, in both directions. That is a fourth property rather than a detail of
//!    the first: a table can exist with its guards missing, and on the backend the
//!    in-crate suite actually runs, a **dropped trigger or index changes nothing
//!    any application-level test can see**. Deleting
//!    `uq_pricing_approval_key_pending` or any of the register's `RAISE(ABORT)`
//!    triggers left the whole suite green — the contention tests are answered by
//!    `approval_repo::find_pending_key_holder`, which is a `SELECT` — so the rule
//!    silently degraded from a constraint to a check. The census is what makes that
//!    a red test.
//!
//! Postgres-backed coverage is testcontainers-gated by convention in this repo
//! and none is added: the append-only guards are mirrored onto `SQLite` as
//! `RAISE(ABORT, ...)` triggers, so `sqlite_append_only.rs` exercises them with
//! no Docker.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::infra::storage::entity::{
    approval, approval_key, approval_threshold, approval_threshold_tombstone, audit_log,
    brand_taxonomy, bulk_operation, bulk_row_lock, bundle, bundle_component, bundle_revshare,
    bundle_revshare_group, catalog_version_ref, composite_meter, customer_group_taxonomy,
    gl_code_taxonomy, group_membership, idempotency_dedup, migration, operator_flag,
    org_tier_taxonomy, outbox, partner_taxonomy, pin_frontier, plan, plan_addon_rule,
    plan_descriptor_set, plan_period_floor_cap, plan_phase, policy_object, price, price_overlay,
    price_overlay_line, price_overlay_line_amount, price_tier_band, price_window, read_model,
    region_taxonomy, repricing_journal, rounding_policy_taxonomy, snapshot_provenance,
};
use bss_pricing::infra::storage::migrations::Migrator;
use sea_orm::{ConnectionTrait, Database, EntityName, EntityTrait, Statement};
use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// Every table the chain creates, `coord_leases` included.
const EXPECTED_TABLES: &[&str] = &[
    "pricing_plan",
    "pricing_plan_phase",
    "pricing_plan_addon_rule",
    "pricing_plan_descriptor_set",
    "pricing_plan_period_floor_cap",
    "pricing_price",
    "pricing_price_tier_band",
    "pricing_read_model",
    "pricing_catalog_version_ref",
    "pricing_bulk_operation",
    // Slice 12's per-row spine, after the run it keys on.
    "pricing_repricing_journal",
    // Slice 12's bulk lock, a side table rather than a column on `pricing_price`.
    "pricing_bulk_row_lock",
    "pricing_composite_meter",
    "pricing_pin_frontier",
    "pricing_policy_object",
    "pricing_operator_flag",
    "pricing_idempotency_dedup",
    "pricing_outbox",
    "pricing_audit_log",
    "pricing_approval",
    "pricing_approval_key",
    "pricing_approval_threshold",
    "pricing_approval_threshold_tombstone",
    "pricing_price_window",
    "pricing_bundle",
    "pricing_bundle_component",
    "pricing_bundle_revshare_group",
    "pricing_bundle_revshare",
    // Slice 4's four scope-value taxonomies, built on the Slice 9 chain because
    // `inst-plv-scope` validates against them and none of them existed.
    "pricing_region_taxonomy",
    "pricing_brand_taxonomy",
    "pricing_partner_taxonomy",
    "pricing_org_tier_taxonomy",
    // Slice 9's own taxonomy (`inst-cg-taxonomy`), on its own route rather than
    // the `config` route the four above share — see
    // `pricing_customer_group_taxonomy`'s migration doc.
    "pricing_customer_group_taxonomy",
    // Slice 9's own membership plane (`inst-cg-record`), D-09's two-layer
    // non-overlap invariant carried at the schema layer on both engines.
    "pricing_group_membership",
    // Slice 9's three, in dependency order.
    "pricing_price_overlay",
    "pricing_price_overlay_line",
    "pricing_price_overlay_line_amount",
    // Slice 11's migration plane: one scheduled plan migration and its section 4
    // state machine.
    "pricing_migration",
    // Slice 11's synthesis half: the frozen `migrated-origin` record.
    "pricing_snapshot_provenance",
    // D-334's rounding-policy taxonomy (`pricing_rounding_policy_taxonomy`). It was in neither
    // this list nor the `assert_readable!` roster until 2026-08-20, although the
    // primary-key census has named it since it landed — which is the drift a
    // completeness check built out of a second hand list cannot see, and why
    // `owed` below is now taken from the database.
    "pricing_rounding_policy_taxonomy",
    // D-356's GL-code taxonomy (`pricing_gl_code_taxonomy`): the same shape on its own
    // table, named here on the day it landed.
    "pricing_gl_code_taxonomy",
    "coord_leases",
];

/// Every trigger the chain creates.
///
/// Transcribed rather than counted, and asserted **in both directions**: a trigger
/// missing from the database fails, and a trigger the database has and this list does
/// not fails too. The second half is what makes it a census — a guard added without a
/// line here is a guard nobody decided to add.
const EXPECTED_TRIGGERS: &[&str] = &[
    "trg_pricing_approval_born_submitted",
    "trg_pricing_approval_flip_whitelist",
    "trg_pricing_approval_immutable_once_decided",
    "trg_pricing_approval_key_born_submitted",
    "trg_pricing_approval_key_born_under_a_pending_unit",
    "trg_pricing_approval_key_follow_state",
    "trg_pricing_approval_key_follows_its_unit",
    "trg_pricing_approval_key_follows_once",
    "trg_pricing_approval_key_no_delete",
    "trg_pricing_approval_key_pinned_columns",
    "trg_pricing_approval_no_delete",
    "trg_pricing_approval_pinned_columns",
    "trg_pricing_approval_threshold_no_delete",
    "trg_pricing_approval_threshold_no_update",
    "trg_pricing_approval_threshold_tombstone_no_delete",
    "trg_pricing_approval_threshold_tombstone_no_update",
    "trg_pricing_audit_log_no_delete",
    "trg_pricing_audit_log_no_update",
    // Slice 12's bulk operation: the DELETE ban, the frozen-column whitelist
    // and section 4's edges, as three triggers mirroring the one PL/pgSQL function.
    "trg_pricing_bulk_operation_born_validating",
    "trg_pricing_bulk_operation_frozen_columns",
    "trg_pricing_bulk_operation_no_delete",
    "trg_pricing_bulk_operation_transitions",
    // The bulk lock: one trigger per condition, mirroring the PL/pgSQL function
    // one to one. There is no `DELETE` arm, deliberately -- release must always
    // be available (D-37), so this census is where a later hand adding one shows
    // up.
    "trg_pricing_bulk_row_lock_no_update",
    "trg_pricing_bulk_row_lock_only_while_committing",
    "trg_pricing_bulk_row_lock_same_tenant_as_its_run",
    "trg_pricing_bundle_component_no_delete",
    "trg_pricing_bundle_component_no_insert",
    "trg_pricing_bundle_component_no_update",
    "trg_pricing_bundle_revshare_group_no_delete",
    "trg_pricing_bundle_revshare_group_no_insert",
    "trg_pricing_bundle_revshare_group_no_update",
    "trg_pricing_bundle_revshare_no_delete",
    "trg_pricing_bundle_revshare_no_insert",
    "trg_pricing_bundle_revshare_no_update",
    // Slice 10's composite meter. Three arms mirroring the one Postgres
    // function, `pricing_plan_phase`'s shape: the parent revision's
    // `lifecycle_state` is the row's, so every verb consults it.
    "trg_pricing_composite_meter_no_delete",
    "trg_pricing_composite_meter_no_insert",
    "trg_pricing_composite_meter_no_update",
    "trg_pricing_composite_meter_same_tenant_as_its_revision_on_insert",
    "trg_pricing_composite_meter_same_tenant_as_its_revision_on_update",
    // Slice 9's membership plane, D-09's cross-group non-overlap invariant on
    // the `SQLite` arm -- `pricing_group_membership`'s two `RAISE(ABORT, ...)` triggers,
    // one per DML verb the interval can change through.
    "trg_pricing_group_membership_no_overlap_insert",
    "trg_pricing_group_membership_no_overlap_update",
    // Slice 11. Five arms, mirroring the one Postgres function: the DELETE ban,
    // the terminal-row ban, the frozen-column whitelist, section 4's edges, and
    // D-65's replay guard on the persisted exclusion set.
    "trg_pricing_migration_exclusion_replay",
    "trg_pricing_migration_flip_whitelist",
    "trg_pricing_migration_frozen_columns",
    "trg_pricing_migration_immutable_history",
    "trg_pricing_migration_no_delete",
    "trg_pricing_plan_addon_rule_no_delete",
    "trg_pricing_plan_addon_rule_no_insert",
    "trg_pricing_plan_addon_rule_no_update",
    "trg_pricing_plan_addon_rule_same_tenant_as_its_revision_on_insert",
    "trg_pricing_plan_addon_rule_same_tenant_as_its_revision_on_update",
    "trg_pricing_plan_descriptor_set_no_delete",
    "trg_pricing_plan_descriptor_set_no_insert",
    "trg_pricing_plan_descriptor_set_no_update",
    "trg_pricing_plan_descriptor_set_same_tenant_as_its_revision_on_insert",
    "trg_pricing_plan_descriptor_set_same_tenant_as_its_revision_on_update",
    "trg_pricing_plan_draft_flip_whitelist",
    "trg_pricing_plan_flip_whitelist",
    "trg_pricing_plan_frozen_columns",
    "trg_pricing_plan_no_delete",
    "trg_pricing_plan_period_floor_cap_no_delete",
    "trg_pricing_plan_period_floor_cap_no_insert",
    "trg_pricing_plan_period_floor_cap_no_update",
    "trg_pricing_plan_period_floor_cap_same_tenant_as_its_revision_on_insert",
    "trg_pricing_plan_period_floor_cap_same_tenant_as_its_revision_on_update",
    "trg_pricing_plan_phase_no_delete",
    "trg_pricing_plan_phase_no_insert",
    "trg_pricing_plan_phase_no_update",
    "trg_pricing_plan_phase_same_tenant_as_its_revision_on_insert",
    "trg_pricing_plan_phase_same_tenant_as_its_revision_on_update",
    "trg_pricing_price_draft_flip_whitelist",
    "trg_pricing_price_flip_whitelist",
    "trg_pricing_price_frozen_columns",
    "trg_pricing_price_grandfather_monotonic",
    "trg_pricing_price_no_delete",
    // Slice 9's ten. The header carries the `pricing_plan` arrangement — a
    // frozen-column whitelist, a draft-exit whitelist, a frozen-state flip
    // whitelist and a no-delete off the draft plane — and the two child tables
    // carry `pricing_bundle_component`'s three-verb guard, which is what freezes
    // a published revision's lines and their money with it (D-92).
    "trg_pricing_price_overlay_draft_exit",
    "trg_pricing_price_overlay_frozen_columns",
    "trg_pricing_price_overlay_frozen_flip",
    "trg_pricing_price_overlay_line_amount_no_delete",
    "trg_pricing_price_overlay_line_amount_no_insert",
    "trg_pricing_price_overlay_line_amount_no_update",
    "trg_pricing_price_overlay_line_no_delete",
    "trg_pricing_price_overlay_line_no_insert",
    "trg_pricing_price_overlay_line_no_update",
    "trg_pricing_price_overlay_line_same_tenant_as_its_revision_on_insert",
    "trg_pricing_price_overlay_line_same_tenant_as_its_revision_on_update",
    "trg_pricing_price_overlay_no_delete",
    "trg_pricing_price_tier_band_kind_insert",
    "trg_pricing_price_tier_band_kind_update",
    "trg_pricing_price_tier_band_no_delete",
    "trg_pricing_price_tier_band_no_insert",
    "trg_pricing_price_tier_band_no_update",
    "trg_pricing_price_tier_band_parent_kind",
    "trg_pricing_price_window_act_sequence",
    "trg_pricing_price_window_flip_whitelist",
    "trg_pricing_price_window_frozen_columns",
    "trg_pricing_price_window_future_end",
    "trg_pricing_price_window_immutable_history",
    "trg_pricing_price_window_no_delete",
    // The non-overlap invariant, in the schema instead of in a
    // read two writers could step through (D-352). The Postgres half is an
    // `EXCLUDE USING gist`; SQLite has no exclusion constraint, so it is these.
    "trg_pricing_price_window_no_overlap_insert",
    "trg_pricing_price_window_no_overlap_update",
    // Slice 12's repricing journal: born pending, undeletable, keyed on a frozen
    // key, final once decided, journalling only under a repricing run, and only
    // under its **own tenant's** run — six triggers mirroring the one PL/pgSQL
    // function, one per arm of it.
    "trg_pricing_repricing_journal_born_pending",
    "trg_pricing_repricing_journal_decided_is_final",
    "trg_pricing_repricing_journal_frozen_key",
    "trg_pricing_repricing_journal_no_delete",
    "trg_pricing_repricing_journal_only_under_a_repricing_run",
    "trg_pricing_repricing_journal_same_tenant_as_its_run",
    // Slice 11. Two unconditional arms and no whitelist: a migrated-origin
    // snapshot is **frozen**, so no UPDATE is sanctioned at all.
    "trg_pricing_snapshot_provenance_no_delete",
    "trg_pricing_snapshot_provenance_no_update",
];

/// Every index the chain creates, `uq_` and `idx_` alike.
///
/// The `uq_` half is the load-bearing one: each of those is a **rule** — one current
/// revision per plan, one open draft, one published row per scope key, one pending
/// holder per key — and an index dropped from the chain turns the rule into whatever
/// the application happens to check. Asserted in both directions, for
/// [`EXPECTED_TRIGGERS`]' reason.
const EXPECTED_INDEXES: &[&str] = &[
    "idx_pricing_approval_key_approval",
    "idx_pricing_approval_subject",
    "idx_pricing_audit_log_recorded",
    "idx_pricing_audit_log_subject",
    "idx_pricing_bulk_operation_live",
    "idx_pricing_bulk_row_lock_operation",
    "idx_pricing_bundle_component_plan",
    "idx_pricing_bundle_component_revision",
    "idx_pricing_bundle_revshare_group_revision",
    "idx_pricing_bundle_revshare_revision",
    "idx_pricing_bundle_tenant",
    "idx_pricing_catalog_version_ref_version",
    "idx_pricing_composite_meter_revision",
    // The resolution walk (`inst-cg-resolve`) and the exclusion constraint's own
    // probe are both per-payer range scans.
    "idx_pricing_group_membership_payer",
    "idx_pricing_group_membership_walk",
    "idx_pricing_idempotency_dedup_created",
    "idx_pricing_migration_due",
    "idx_pricing_migration_source",
    "idx_pricing_migration_target",
    "idx_pricing_operator_flag_by_flag",
    "idx_pricing_outbox_undrained",
    "idx_pricing_plan_addon_rule_revision",
    "idx_pricing_plan_descriptor_set_revision",
    "idx_pricing_plan_period_floor_cap_revision",
    "idx_pricing_plan_phase_revision",
    "idx_pricing_plan_tenant",
    "idx_pricing_price_overlay_line_amount_tenant",
    "idx_pricing_price_overlay_line_plan",
    "idx_pricing_price_overlay_line_revision",
    "idx_pricing_price_overlay_scope",
    "idx_pricing_price_plan",
    "idx_pricing_price_supersedes",
    "idx_pricing_price_tier_band_price",
    "idx_pricing_price_window_due",
    "idx_pricing_price_window_price",
    "idx_pricing_read_model_resolve",
    "idx_pricing_snapshot_provenance_plan",
    "uq_pricing_approval_key_pending",
    "uq_pricing_approval_policy_pending",
    "uq_pricing_bulk_operation_client_key",
    "uq_pricing_bundle_plan",
    "uq_pricing_composite_meter_output",
    "uq_pricing_outbox_dedup_key",
    "uq_pricing_outbox_sequence",
    "uq_pricing_plan_current",
    "uq_pricing_plan_open_draft",
    "uq_pricing_plan_phase_terminal",
    "uq_pricing_price_meter_line_current",
    // D-42's null-safe line key — an **expression** index over three COALESCEd
    // sentinels, because a plain UNIQUE over three nullable columns admits the
    // very rows section 6 spells as "one default line, one line per plan".
    "uq_pricing_price_overlay_line_key",
    "uq_pricing_price_overlay_open_draft",
    // D-107's partial predicate. Dropping the `WHERE lifecycle_state =
    // 'published'` makes a draft revision of a published overlay collide with
    // itself, so an overlay is authorable exactly once.
    "uq_pricing_price_overlay_precedence",
    "uq_pricing_price_scope_key_current",
    "uq_pricing_price_scope_key_draft",
    // Section 9's idempotency rule as an index: one subscription, one frozen
    // snapshot, ever. Keyed without the trigger on purpose - D-81 gives the two
    // triggers different instants, so a per-trigger key would let one
    // subscription hold two different frozen prices.
    "uq_pricing_snapshot_provenance_subscription",
];

/// Every named CHECK constraint the chain declares, **by name**.
///
/// A roster and not a count, for `postgres_migrations.rs`'s reason: a count cannot tell
/// a guard from a tautology.
const EXPECTED_CHECKS: &[&str] = &[
    "chk_pricing_approval_approver",
    "chk_pricing_approval_decided_at",
    "chk_pricing_approval_distinct_principals",
    "chk_pricing_approval_key_state",
    "chk_pricing_approval_reason",
    "chk_pricing_approval_state",
    "chk_pricing_approval_subject_kind",
    "chk_pricing_approval_threshold_absolute_non_negative",
    "chk_pricing_approval_threshold_basis",
    "chk_pricing_approval_threshold_currency",
    "chk_pricing_approval_threshold_percent_positive",
    "chk_pricing_approval_threshold_tombstone_version",
    "chk_pricing_approval_threshold_version",
    "chk_pricing_audit_log_action",
    "chk_pricing_audit_log_entry_kind",
    "chk_pricing_audit_log_rollup",
    "chk_pricing_audit_log_seq",
    // Z6-6 (`pricing_audit_log`): D-158's enumeration spells two columns and only
    // `pricing_approval`'s was held to it.
    "chk_pricing_audit_log_subject_kind",
    "chk_pricing_brand_taxonomy_state",
    "chk_pricing_brand_taxonomy_value_present",
    // Slice 12's bulk operation. Four: the two vocabularies, D-137's
    // import-never-awaits edge, and the terminal/completed_at agreement.
    "chk_pricing_bulk_operation_completed_at",
    "chk_pricing_bulk_operation_import_never_awaits",
    "chk_pricing_bulk_operation_kind",
    "chk_pricing_bulk_operation_state",
    "chk_pricing_bundle_component_min_qty",
    "chk_pricing_bundle_component_qty_range",
    "chk_pricing_bundle_invoice_itemization",
    "chk_pricing_bundle_price_basis",
    "chk_pricing_bundle_revshare_effective_share_bp",
    "chk_pricing_bundle_revshare_group_absorber",
    "chk_pricing_bundle_revshare_group_platform_cut_bp",
    "chk_pricing_bundle_revshare_party",
    "chk_pricing_bundle_revshare_share_bp",
    "chk_pricing_catalog_version_ref_commit",
    "chk_pricing_catalog_version_ref_subject_kind",
    "chk_pricing_catalog_version_ref_subject_lifecycle",
    "chk_pricing_catalog_version_ref_subject_revision",
    "chk_pricing_catalog_version_ref_version",
    // Slice 10's composite meter. One CHECK only: the arity and self-reference
    // rules are publish rules rather than column constraints -- see
    // `pricing_composite_meter`'s migration doc for why, and `chk_pricing_price_overlay_lifecycle_state`'s for the
    // portability argument it inherits.
    "chk_pricing_composite_meter_output_unit",
    // Slice 9's own taxonomy (`inst-cg-taxonomy`), the taxonomy pair of CHECKs
    // over `pricing_customer_group_taxonomy`.
    "chk_pricing_customer_group_taxonomy_state",
    "chk_pricing_customer_group_taxonomy_value_present",
    // D-356's declared GL-code vocabulary -- the taxonomy shape, on its own table.
    "chk_pricing_gl_code_taxonomy_state",
    "chk_pricing_gl_code_taxonomy_value_present",
    // Slice 9's membership plane (`inst-cg-record`): the value-present guard the
    // four taxonomies also carry, the half-open interval sanity check
    // `pricing_price_window`/`pricing_price_overlay` carry too, and the entity
    // tag's floor. D-09's
    // non-overlap invariant is a separate object (`excl_pricing_group_membership_no_overlap`,
    // `contype = 'x'`) and does not belong to this census -- see
    // `postgres_migrations.rs`'s `CHECKS_SQL`, which filters on `contype = 'c'`.
    "chk_pricing_group_membership_group_value_present",
    "chk_pricing_group_membership_interval",
    "chk_pricing_group_membership_row_version",
    "chk_pricing_idempotency_dedup_answered",
    "chk_pricing_idempotency_dedup_status",
    // Slice 11. The two implications that carry section 4's reachable set
    // (`cancelled` is reachable both started and unstarted, so `started_at` is
    // deliberately not a biconditional), the two that are, D-65's co-nullable
    // exclusion set, D-49's row-local half, and three ordering rules.
    "chk_pricing_migration_announced_before_effective",
    "chk_pricing_migration_cancelled_at",
    "chk_pricing_migration_cancelled_order",
    "chk_pricing_migration_completed_at",
    "chk_pricing_migration_completed_order",
    "chk_pricing_migration_distinct_plans",
    "chk_pricing_migration_exclusion_snapshot",
    "chk_pricing_migration_scheduled_unstarted",
    "chk_pricing_migration_source_revision",
    "chk_pricing_migration_started_order",
    "chk_pricing_migration_started_required",
    "chk_pricing_migration_state",
    "chk_pricing_operator_flag_name",
    "chk_pricing_org_tier_taxonomy_state",
    "chk_pricing_org_tier_taxonomy_value_present",
    "chk_pricing_outbox_event_name",
    "chk_pricing_outbox_sequence",
    "chk_pricing_partner_taxonomy_state",
    "chk_pricing_partner_taxonomy_value_present",
    "chk_pricing_pin_frontier_version",
    "chk_pricing_plan_addon_rule_max_qty",
    "chk_pricing_plan_addon_rule_min_qty",
    "chk_pricing_plan_addon_rule_qty_range",
    "chk_pricing_plan_addon_rule_required_max_qty",
    "chk_pricing_plan_addon_rule_step_qty",
    "chk_pricing_plan_availability",
    "chk_pricing_plan_billing_cycle",
    "chk_pricing_plan_custom_interval_n",
    "chk_pricing_plan_custom_interval_pairing",
    "chk_pricing_plan_custom_interval_unit",
    "chk_pricing_plan_frequency",
    "chk_pricing_plan_lifecycle_state",
    "chk_pricing_plan_period_floor_cap_cap_positive",
    "chk_pricing_plan_period_floor_cap_currency",
    "chk_pricing_plan_period_floor_cap_floor_positive",
    "chk_pricing_plan_period_floor_cap_ordered",
    "chk_pricing_plan_period_floor_cap_present",
    "chk_pricing_plan_phase_display_trial_days",
    "chk_pricing_plan_phase_duration_non_negative",
    "chk_pricing_plan_phase_kind",
    "chk_pricing_plan_phase_trial_projection_non_negative",
    "chk_pricing_plan_purchase_max_qty",
    "chk_pricing_plan_purchase_min_qty",
    "chk_pricing_plan_purchase_qty",
    "chk_pricing_plan_revision",
    "chk_pricing_plan_row_version",
    "chk_pricing_policy_object_interval_days_cap",
    "chk_pricing_policy_object_interval_months_cap",
    "chk_pricing_policy_object_notice_floor",
    "chk_pricing_policy_object_price_row_cap",
    // Slice 4's C4 switch, and since D-240 the only tax-display constraint on
    // this table.
    // `chk_pricing_policy_object_tax_display` would sit above it, holding
    // a display *basis* default under a name section 6 spends on this
    // fail-closed *enforcement* mode; retiring it is what makes the name
    // unambiguous rather than merely adjacent.
    "chk_pricing_policy_object_tax_display_policy",
    "chk_pricing_policy_object_tier_band_cap",
    "chk_pricing_price_aggregation_function",
    "chk_pricing_price_aggregation_granularity",
    "chk_pricing_price_amount_non_negative",
    "chk_pricing_price_billing_granularity",
    "chk_pricing_price_billing_timing",
    "chk_pricing_price_charge_kind",
    "chk_pricing_price_cohort_eligibility",
    "chk_pricing_price_eligibility",
    "chk_pricing_price_grandfather_until",
    "chk_pricing_price_lifecycle_state",
    "chk_pricing_price_manual_quantity",
    "chk_pricing_price_max_hold_granules",
    "chk_pricing_price_meter_no_separator",
    "chk_pricing_price_min_qty_purchase",
    "chk_pricing_price_min_qty_usage",
    "chk_pricing_price_model_kind",
    "chk_pricing_price_overlay",
    // Slice 9's overlay object. Note the near-collision one line up:
    // `chk_pricing_price_overlay` is the **price row's** `price_overlay` axis
    // CHECK (always `base`, Foundation section 4.1), and everything from here
    // down belongs to the overlay object — which is a separate row evaluated
    // downstream, not a value of that axis.
    "chk_pricing_price_overlay_disclosure",
    "chk_pricing_price_overlay_interval",
    "chk_pricing_price_overlay_lifecycle_state",
    "chk_pricing_price_overlay_line_adjustment_kind",
    "chk_pricing_price_overlay_line_amount_currency",
    "chk_pricing_price_overlay_line_amount_value_minor",
    "chk_pricing_price_overlay_line_cohort_needs_plan",
    "chk_pricing_price_overlay_line_discount_ceiling",
    "chk_pricing_price_overlay_line_fixed_is_amount",
    "chk_pricing_price_overlay_line_magnitude_kind",
    "chk_pricing_price_overlay_line_magnitude_pairing",
    "chk_pricing_price_overlay_line_magnitude_positive",
    "chk_pricing_price_overlay_line_plan_id_not_nil",
    "chk_pricing_price_overlay_line_sku_needs_plan",
    "chk_pricing_price_overlay_line_target_sku_present",
    "chk_pricing_price_overlay_revision",
    "chk_pricing_price_overlay_row_version",
    "chk_pricing_price_overlay_scope_class",
    "chk_pricing_price_overlay_scope_value",
    "chk_pricing_price_overlay_tax_basis",
    "chk_pricing_price_package_fields_kind",
    "chk_pricing_price_package_price",
    "chk_pricing_price_package_size",
    "chk_pricing_price_quantity_source",
    "chk_pricing_price_region_no_separator",
    "chk_pricing_price_reserved_rate_nano",
    "chk_pricing_price_row_version",
    "chk_pricing_price_tier_aggregation_window",
    "chk_pricing_price_tier_band_from_qty",
    "chk_pricing_price_tier_band_unit_price",
    "chk_pricing_price_tier_band_width",
    "chk_pricing_price_tier_qualification_window",
    // D-311's `unit_rate_nano` carries this CHECK on both engines, and every
    // column rule here does. Giving one to Postgres alone, on the ground that
    // rebuilding `pricing_price` for a single clause "would be a large edit",
    // costs half the rule: the mirror is restated whole whenever anything on the
    // table moves, so the saving is never taken and the arm that was skipped is
    // simply absent.
    "chk_pricing_price_unit_rate_nano",
    "chk_pricing_price_window_activated_at",
    "chk_pricing_price_window_activation_order",
    "chk_pricing_price_window_cancelled_at",
    "chk_pricing_price_window_expired_at",
    "chk_pricing_price_window_expiry_order",
    "chk_pricing_price_window_interval",
    "chk_pricing_price_window_mutation_seq",
    "chk_pricing_price_window_open_ended",
    "chk_pricing_price_window_reason_code",
    "chk_pricing_price_window_state",
    "chk_pricing_read_model_catalog_version",
    "chk_pricing_read_model_subject_kind",
    "chk_pricing_read_model_warm_marker",
    "chk_pricing_region_taxonomy_state",
    "chk_pricing_region_taxonomy_value_present",
    // Slice 12's repricing journal. Four: the state vocabulary, the two
    // outcome-column agreements, and `inst-mp-standard`'s refusal of a successor
    // wearing the selected row's own id.
    "chk_pricing_repricing_journal_applied",
    "chk_pricing_repricing_journal_failed",
    "chk_pricing_repricing_journal_state",
    "chk_pricing_repricing_journal_successor_is_new",
    // D-334's declared rounding vocabulary — the taxonomy shape, on its own table.
    "chk_pricing_rounding_policy_taxonomy_state",
    "chk_pricing_rounding_policy_taxonomy_value_present",
    "chk_pricing_snapshot_provenance_payload",
    "chk_pricing_snapshot_provenance_resolved",
    "chk_pricing_snapshot_provenance_revision",
    "chk_pricing_snapshot_provenance_trigger",
];

/// Every trigger's body, pinned by digest — the roster, in `sqlite_master`'s
/// name order.
///
/// A module-level `const` like the four rosters above it, and it was a `Vec`
/// built inside the accessor until Slice 8's nine triggers pushed that function
/// past the line cap. Nothing about it was ever computed: the digests are
/// literals that a legitimate change to a trigger re-pins here, deliberately.
/// Every table's primary key, `(table, "col, col")`, ordered by table name.
///
/// **Seeded from the live schema once (D-236) and then hand-checked**, because a
/// roster taken from the code's own output pins whatever the code does, bug
/// included. Four keys are named by a decision and were each read back against the
/// migration that declares them rather than against the schema this census reads:
/// `pricing_catalog_version_ref`'s key carries the subject beside the handle (the
/// shape D-236 is about — every other roster passed it unchanged);
/// `pricing_read_model`'s matching four-part key
/// (`pricing_read_model`); `pricing_price_overlay_line`'s `(line_id,
/// overlay_revision)` (`pricing_price_overlay_line`, D-42's line container); and
/// `pricing_plan`'s `(plan_id, revision)` (`pricing_plan`, the revision
/// identity D-145 makes a name rather than a counter). The rest were read for
/// shape: every revision-scoped child carries its parent's revision, every
/// tenant-scoped config table is keyed by tenant, and `coord_leases` is `coord`'s
/// own table spliced in for the warm re-drive.
///
/// **A table whose key is empty gets a line saying so**, not a gap. The silence
/// D-236 names is a key absent from the census, so absence must be spelled.
const EXPECTED_PRIMARY_KEYS: &[(&str, &str)] = &[
    ("coord_leases", "key"),
    ("pricing_approval", "approval_id"),
    ("pricing_approval_key", "approval_id, scope_key"),
    ("pricing_approval_threshold", "tenant_id, version, currency"),
    ("pricing_approval_threshold_tombstone", "tenant_id, version"),
    ("pricing_audit_log", "tenant_id, chain_id, seq"),
    ("pricing_brand_taxonomy", "tenant_id, value"),
    ("pricing_bulk_operation", "operation_id"),
    ("pricing_bulk_row_lock", "tenant_id, price_id"),
    ("pricing_bundle", "bundle_id"),
    (
        "pricing_bundle_component",
        "bundle_id, plan_revision, component_plan_id",
    ),
    (
        "pricing_bundle_revshare",
        "bundle_id, plan_revision, vendor_sku_id, party",
    ),
    (
        "pricing_bundle_revshare_group",
        "bundle_id, plan_revision, vendor_sku_id",
    ),
    (
        "pricing_catalog_version_ref",
        "tenant_id, pending_ref, subject_kind, subject_ref",
    ),
    // Slice 10's composite meter, D-106's revision discipline in the key
    // itself: `composite_id` is stable across revisions and the revision is the
    // second column, so a copy-forward is a new row rather than an edit.
    // **Widened by `pricing_composite_meter` (A1-1)**, 2026-08-18, and for the reason
    // `pricing_plan_phase`'s row below records one day earlier: it was
    // `composite_id, plan_revision`, with a client-supplied `composite_id` and no
    // tenant, so a composite id belonged to one plan per revision *number* across
    // the whole table. `pricing_composite_meter`'s migration doc named this key as
    // `pricing_plan_phase`'s shape one table over; `pricing_plan_phase` moved that
    // one and left this one. The `plan_revision` half stays for D-106's
    // copy-forward, and one revision still may not hold the same composite id
    // twice.
    (
        "pricing_composite_meter",
        "tenant_id, plan_id, plan_revision, composite_id",
    ),
    // Slice 9's own taxonomy (`inst-cg-taxonomy`), the four's own key shape on
    // its own table.
    ("pricing_customer_group_taxonomy", "tenant_id, value"),
    // D-356 (`pricing_gl_code_taxonomy`): the taxonomies' key, on a table of their shape.
    ("pricing_gl_code_taxonomy", "tenant_id, value"),
    // Slice 9's membership plane (`inst-cg-record`). Keyed on its own surrogate
    // id, not `(tenant_id, payer_tenant_id, effective_from)`: a payer may hold
    // several historical rows and D-09's non-overlap is the exclusion
    // constraint's job, not the primary key's.
    ("pricing_group_membership", "membership_id"),
    (
        "pricing_idempotency_dedup",
        "tenant_id, operation, client_key",
    ),
    // Client-supplied (`inst-ms-api`, M2), and therefore **tenant-scoped since
    // `pricing_migration`**. It was `migration_id` alone until 2026-08-11, which
    // put a client-chosen identifier in a deployment-wide namespace: one tenant
    // could take an id and deny it to every other permanently, with no remedy
    // (`trg_pricing_migration_no_delete` refuses the DELETE that would free it).
    // Every sibling client-key store already scoped its key; this was the last.
    ("pricing_migration", "tenant_id, migration_id"),
    ("pricing_operator_flag", "tenant_id, subject_ref, flag"),
    ("pricing_org_tier_taxonomy", "tenant_id, value"),
    ("pricing_outbox", "outbox_id"),
    ("pricing_partner_taxonomy", "tenant_id, value"),
    ("pricing_pin_frontier", "tenant_id"),
    ("pricing_plan", "plan_id, revision"),
    (
        "pricing_plan_addon_rule",
        "plan_id, plan_revision, addon_sku_id",
    ),
    ("pricing_plan_descriptor_set", "plan_id, plan_revision"),
    (
        "pricing_plan_period_floor_cap",
        "plan_id, plan_revision, currency, region",
    ),
    // **Widened by `pricing_plan_phase` (D-340)**, 2026-08-17. It was
    // `phase_id, plan_revision`, which said a phase id belongs to one plan per
    // revision *number* across the whole table — every tenant's included, so the
    // refusal on that key was also an oracle over another tenant's ids. The
    // `plan_revision` half stays for D-83's copy-forward, and one revision still
    // may not hold the same phase id twice.
    (
        "pricing_plan_phase",
        "tenant_id, plan_id, plan_revision, phase_id",
    ),
    ("pricing_policy_object", "tenant_id"),
    ("pricing_price", "price_id"),
    ("pricing_price_overlay", "price_overlay_id, revision"),
    // **Both widened by `pricing_price_overlay_line_amount` (A1-3, and A1-4 for the child)**,
    // 2026-08-18. The line was `line_id, overlay_revision` with a client-supplied
    // `line_id`, so a line id belonged to one overlay per revision *number* across
    // the whole table. The child moves in the same migration and not later: once
    // two tenants may hold one `(line_id, overlay_revision)`, a narrow key here
    // collides on their amounts instead, which is the condition A1-4 records as
    // the one that arms this table's untyped insert catch-all.
    (
        "pricing_price_overlay_line",
        "tenant_id, overlay_revision, line_id",
    ),
    (
        "pricing_price_overlay_line_amount",
        "tenant_id, overlay_revision, line_id, currency",
    ),
    ("pricing_price_tier_band", "band_id"),
    ("pricing_price_window", "window_id"),
    (
        "pricing_read_model",
        "tenant_id, catalog_version, subject_kind, subject_ref",
    ),
    ("pricing_region_taxonomy", "tenant_id, value"),
    ("pricing_repricing_journal", "run_id, price_id"),
    // D-334 (`pricing_rounding_policy_taxonomy`): the taxonomies' key, on a table of their
    // shape.
    ("pricing_rounding_policy_taxonomy", "tenant_id, value"),
    // Read back from `pricing_snapshot_provenance`'s own DDL. `provenance_id` and not the
    // subscription: the subscription's uniqueness is a partial-free UNIQUE index
    // beside it, which is the rule rather than the identity.
    ("pricing_snapshot_provenance", "provenance_id"),
];

fn expected_primary_keys() -> Vec<(String, String)> {
    EXPECTED_PRIMARY_KEYS
        .iter()
        .map(|(table, columns)| ((*table).to_owned(), (*columns).to_owned()))
        .collect()
}

const EXPECTED_TRIGGER_BODIES: &[(&str, u64)] = &[
    (
        "trg_pricing_approval_born_submitted",
        8_026_324_167_547_094_374_u64,
    ),
    (
        "trg_pricing_approval_flip_whitelist",
        7_582_204_510_596_437_500_u64,
    ),
    (
        "trg_pricing_approval_immutable_once_decided",
        8_082_372_707_353_450_395_u64,
    ),
    (
        "trg_pricing_approval_key_born_submitted",
        2_440_950_770_022_816_954_u64,
    ),
    (
        "trg_pricing_approval_key_born_under_a_pending_unit",
        17_401_364_553_705_688_472_u64,
    ),
    (
        "trg_pricing_approval_key_follow_state",
        1_503_439_957_052_833_582_u64,
    ),
    (
        "trg_pricing_approval_key_follows_its_unit",
        3_659_799_981_708_444_309_u64,
    ),
    (
        "trg_pricing_approval_key_follows_once",
        17_250_957_205_851_589_411_u64,
    ),
    (
        "trg_pricing_approval_key_no_delete",
        8_268_739_358_483_246_584_u64,
    ),
    (
        "trg_pricing_approval_key_pinned_columns",
        2_791_595_470_115_359_269_u64,
    ),
    (
        "trg_pricing_approval_no_delete",
        13_958_316_444_295_959_010_u64,
    ),
    (
        "trg_pricing_approval_pinned_columns",
        16_147_021_889_530_757_421_u64,
    ),
    (
        "trg_pricing_approval_threshold_no_delete",
        12_053_586_872_877_274_445_u64,
    ),
    (
        "trg_pricing_approval_threshold_no_update",
        11_709_629_607_986_505_725_u64,
    ),
    (
        "trg_pricing_approval_threshold_tombstone_no_delete",
        12_721_364_154_841_815_973_u64,
    ),
    (
        "trg_pricing_approval_threshold_tombstone_no_update",
        12_807_063_624_490_211_381_u64,
    ),
    (
        "trg_pricing_audit_log_no_delete",
        4_599_062_756_050_227_754_u64,
    ),
    (
        "trg_pricing_audit_log_no_update",
        8_228_055_037_257_075_408_u64,
    ),
    (
        "trg_pricing_bulk_operation_born_validating",
        15_400_506_675_831_746_121_u64,
    ),
    (
        "trg_pricing_bulk_operation_frozen_columns",
        6_962_147_701_888_848_379_u64,
    ),
    (
        "trg_pricing_bulk_operation_no_delete",
        11_270_963_154_713_380_806_u64,
    ),
    (
        "trg_pricing_bulk_operation_transitions",
        1_668_939_029_741_021_138_u64,
    ),
    (
        "trg_pricing_bulk_row_lock_no_update",
        16_754_342_763_826_305_448_u64,
    ),
    (
        "trg_pricing_bulk_row_lock_only_while_committing",
        12_667_493_367_361_831_668_u64,
    ),
    (
        "trg_pricing_bulk_row_lock_same_tenant_as_its_run",
        6_733_270_478_632_137_863_u64,
    ),
    (
        "trg_pricing_bundle_component_no_delete",
        18_311_349_621_428_712_190_u64,
    ),
    (
        "trg_pricing_bundle_component_no_insert",
        6_322_203_830_226_671_838_u64,
    ),
    (
        "trg_pricing_bundle_component_no_update",
        13_120_646_898_347_948_270_u64,
    ),
    (
        "trg_pricing_bundle_revshare_group_no_delete",
        17_696_855_402_771_122_202_u64,
    ),
    (
        "trg_pricing_bundle_revshare_group_no_insert",
        6_332_360_706_892_612_382_u64,
    ),
    (
        "trg_pricing_bundle_revshare_group_no_update",
        16_628_523_182_385_343_342_u64,
    ),
    (
        "trg_pricing_bundle_revshare_no_delete",
        7_894_102_050_253_230_881_u64,
    ),
    (
        "trg_pricing_bundle_revshare_no_insert",
        12_489_306_521_005_756_213_u64,
    ),
    (
        "trg_pricing_bundle_revshare_no_update",
        18_093_686_238_376_949_727_u64,
    ),
    (
        "trg_pricing_composite_meter_no_delete",
        7_816_235_272_572_446_796_u64,
    ),
    (
        "trg_pricing_composite_meter_no_insert",
        16_688_907_306_112_963_488_u64,
    ),
    (
        "trg_pricing_composite_meter_no_update",
        10_850_547_674_715_117_695_u64,
    ),
    (
        "trg_pricing_composite_meter_same_tenant_as_its_revision_on_insert",
        6_343_971_475_040_683_740_u64,
    ),
    (
        "trg_pricing_composite_meter_same_tenant_as_its_revision_on_update",
        15_646_905_659_757_730_516_u64,
    ),
    (
        "trg_pricing_group_membership_no_overlap_insert",
        18_387_155_234_780_561_333_u64,
    ),
    (
        "trg_pricing_group_membership_no_overlap_update",
        13_095_055_540_033_788_195_u64,
    ),
    (
        "trg_pricing_migration_exclusion_replay",
        9_285_572_797_681_964_741_u64,
    ),
    (
        "trg_pricing_migration_flip_whitelist",
        8_032_921_964_833_177_687_u64,
    ),
    (
        "trg_pricing_migration_frozen_columns",
        3_679_481_201_681_159_361_u64,
    ),
    (
        "trg_pricing_migration_immutable_history",
        7_463_691_635_803_712_952_u64,
    ),
    (
        "trg_pricing_migration_no_delete",
        759_106_875_609_865_220_u64,
    ),
    (
        "trg_pricing_plan_addon_rule_no_delete",
        157_003_417_877_367_644_u64,
    ),
    (
        "trg_pricing_plan_addon_rule_no_insert",
        14_964_979_796_966_671_700_u64,
    ),
    (
        "trg_pricing_plan_addon_rule_no_update",
        1_054_084_016_548_546_187_u64,
    ),
    (
        "trg_pricing_plan_addon_rule_same_tenant_as_its_revision_on_insert",
        4_449_259_937_241_254_376_u64,
    ),
    (
        "trg_pricing_plan_addon_rule_same_tenant_as_its_revision_on_update",
        5_814_866_044_157_723_696_u64,
    ),
    (
        "trg_pricing_plan_descriptor_set_no_delete",
        16_652_343_744_580_170_347_u64,
    ),
    (
        "trg_pricing_plan_descriptor_set_no_insert",
        15_250_228_291_084_614_111_u64,
    ),
    (
        "trg_pricing_plan_descriptor_set_no_update",
        13_026_743_661_363_952_284_u64,
    ),
    (
        "trg_pricing_plan_descriptor_set_same_tenant_as_its_revision_on_insert",
        2_198_825_988_851_607_917_u64,
    ),
    (
        "trg_pricing_plan_descriptor_set_same_tenant_as_its_revision_on_update",
        10_030_716_169_952_795_981_u64,
    ),
    (
        "trg_pricing_plan_draft_flip_whitelist",
        1_063_197_060_918_151_682_u64,
    ),
    (
        "trg_pricing_plan_flip_whitelist",
        2_936_899_670_102_780_293_u64,
    ),
    (
        "trg_pricing_plan_frozen_columns",
        16_522_338_372_357_234_734_u64,
    ),
    ("trg_pricing_plan_no_delete", 11_619_837_810_759_772_588_u64),
    (
        "trg_pricing_plan_period_floor_cap_no_delete",
        10_977_406_220_280_042_442_u64,
    ),
    (
        "trg_pricing_plan_period_floor_cap_no_insert",
        12_569_978_638_472_499_082_u64,
    ),
    (
        "trg_pricing_plan_period_floor_cap_no_update",
        3_292_669_217_055_190_501_u64,
    ),
    (
        "trg_pricing_plan_period_floor_cap_same_tenant_as_its_revision_on_insert",
        2_039_500_122_126_340_362_u64,
    ),
    (
        "trg_pricing_plan_period_floor_cap_same_tenant_as_its_revision_on_update",
        5_252_644_657_725_384_850_u64,
    ),
    (
        "trg_pricing_plan_phase_no_delete",
        10_984_812_811_725_408_938_u64,
    ),
    (
        "trg_pricing_plan_phase_no_insert",
        18_074_982_436_648_678_574_u64,
    ),
    (
        "trg_pricing_plan_phase_no_update",
        16_963_852_538_399_811_121_u64,
    ),
    (
        "trg_pricing_plan_phase_same_tenant_as_its_revision_on_insert",
        8_831_960_736_121_635_034_u64,
    ),
    (
        "trg_pricing_plan_phase_same_tenant_as_its_revision_on_update",
        16_487_883_600_432_877_794_u64,
    ),
    (
        "trg_pricing_price_draft_flip_whitelist",
        12_283_433_772_935_461_712_u64,
    ),
    (
        "trg_pricing_price_flip_whitelist",
        6_864_967_922_611_899_704_u64,
    ),
    (
        "trg_pricing_price_frozen_columns",
        9_876_821_329_598_270_805_u64,
    ),
    (
        "trg_pricing_price_grandfather_monotonic",
        6_472_678_356_918_752_723_u64,
    ),
    ("trg_pricing_price_no_delete", 4_952_185_589_843_057_617_u64),
    (
        "trg_pricing_price_overlay_draft_exit",
        1_991_170_413_819_745_157_u64,
    ),
    (
        "trg_pricing_price_overlay_frozen_columns",
        17_562_952_219_673_499_162_u64,
    ),
    (
        "trg_pricing_price_overlay_frozen_flip",
        15_942_597_115_297_834_987_u64,
    ),
    (
        "trg_pricing_price_overlay_line_amount_no_delete",
        3_084_499_414_037_791_705_u64,
    ),
    (
        "trg_pricing_price_overlay_line_amount_no_insert",
        17_838_746_935_386_292_974_u64,
    ),
    (
        "trg_pricing_price_overlay_line_amount_no_update",
        14_080_711_794_770_702_202_u64,
    ),
    (
        "trg_pricing_price_overlay_line_no_delete",
        2_097_612_106_108_158_022_u64,
    ),
    (
        "trg_pricing_price_overlay_line_no_insert",
        18_270_568_870_244_755_780_u64,
    ),
    (
        "trg_pricing_price_overlay_line_no_update",
        863_269_477_514_513_885_u64,
    ),
    (
        "trg_pricing_price_overlay_line_same_tenant_as_its_revision_on_insert",
        9_908_998_901_415_834_354_u64,
    ),
    (
        "trg_pricing_price_overlay_line_same_tenant_as_its_revision_on_update",
        9_443_384_772_773_428_778_u64,
    ),
    (
        "trg_pricing_price_overlay_no_delete",
        11_656_928_464_923_846_849_u64,
    ),
    (
        "trg_pricing_price_tier_band_kind_insert",
        10_239_315_632_932_883_221_u64,
    ),
    (
        "trg_pricing_price_tier_band_kind_update",
        10_273_461_858_767_310_749_u64,
    ),
    (
        "trg_pricing_price_tier_band_no_delete",
        16_327_178_531_631_512_347_u64,
    ),
    (
        "trg_pricing_price_tier_band_no_insert",
        17_806_256_337_294_222_064_u64,
    ),
    (
        "trg_pricing_price_tier_band_no_update",
        4_573_092_537_464_078_304_u64,
    ),
    (
        "trg_pricing_price_tier_band_parent_kind",
        12_207_771_120_581_007_916_u64,
    ),
    (
        "trg_pricing_price_window_act_sequence",
        8_416_459_948_900_544_137_u64,
    ),
    (
        "trg_pricing_price_window_flip_whitelist",
        7_945_364_764_739_140_221_u64,
    ),
    (
        "trg_pricing_price_window_frozen_columns",
        3_006_703_635_329_582_194_u64,
    ),
    (
        "trg_pricing_price_window_future_end",
        674_975_173_687_143_698_u64,
    ),
    (
        "trg_pricing_price_window_immutable_history",
        2_969_521_874_630_654_905_u64,
    ),
    (
        "trg_pricing_price_window_no_delete",
        8_334_934_610_813_099_928_u64,
    ),
    (
        "trg_pricing_price_window_no_overlap_insert",
        9_764_480_166_630_561_019_u64,
    ),
    (
        "trg_pricing_price_window_no_overlap_update",
        4_203_920_929_710_482_773_u64,
    ),
    (
        "trg_pricing_repricing_journal_born_pending",
        11_969_090_648_213_225_350_u64,
    ),
    (
        "trg_pricing_repricing_journal_decided_is_final",
        1_753_028_611_995_600_928_u64,
    ),
    (
        "trg_pricing_repricing_journal_frozen_key",
        17_909_310_162_257_836_633_u64,
    ),
    (
        "trg_pricing_repricing_journal_no_delete",
        64_642_075_391_113_409_u64,
    ),
    (
        "trg_pricing_repricing_journal_only_under_a_repricing_run",
        18_235_109_188_380_125_402_u64,
    ),
    (
        "trg_pricing_repricing_journal_same_tenant_as_its_run",
        15_565_762_449_475_938_019_u64,
    ),
    (
        "trg_pricing_snapshot_provenance_no_delete",
        14_812_364_302_530_093_290_u64,
    ),
    (
        "trg_pricing_snapshot_provenance_no_update",
        3_248_933_936_782_516_701_u64,
    ),
];

/// [`EXPECTED_TRIGGER_BODIES`] in the shape the assertion compares against.
fn expected_trigger_bodies() -> Vec<(String, u64)> {
    EXPECTED_TRIGGER_BODIES
        .iter()
        .map(|(name, hash)| ((*name).to_owned(), *hash))
        .collect()
}

/// Every **named CHECK constraint** the chain created, in name order.
///
/// The census `SQLite` had none of, and it is the backend every in-crate suite runs on.
/// A CHECK is not a `sqlite_master` object of its own — it lives inside its table's
/// `CREATE TABLE` text — so a dropped one leaves the table, the trigger census and the
/// index census all green while the value it refused reaches the column. Two of this
/// chain's migrations **re-type whole table bodies by hand** (`SQLite` cannot
/// `ALTER TABLE ... DROP CONSTRAINT`, so `pricing_approval_threshold` and `chk_pricing_approval_subject_kind`
/// rebuild), which is exactly the operation a constraint goes missing in.
///
/// Read out of the DDL rather than out of the migration sources on purpose: the
/// question is what the database ended up with.
async fn checks_of(conn: &sea_orm::DatabaseConnection) -> Vec<String> {
    let sql = "SELECT sql AS v FROM sqlite_master WHERE type = 'table' \
               AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL ORDER BY name";
    let rows = conn
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql.to_owned(),
        ))
        .await
        .expect("query sqlite_master");
    let mut names: Vec<String> = Vec::new();
    for row in rows {
        let ddl: String = row.try_get("", "v").expect("the table's DDL");
        // `CONSTRAINT <name> CHECK` is how every one of them is declared; the name is
        // what a refusal message carries, so it is what a census can be written
        // against.
        let mut rest = ddl.as_str();
        while let Some(at) = rest.find("CONSTRAINT ") {
            rest = &rest[at + "CONSTRAINT ".len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if rest[name.len()..].trim_start().starts_with("CHECK") {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// Every table's primary key, as `(table, "col, col")`.
///
/// `PRAGMA table_info`'s `pk` is the **1-based position within the key**, not a
/// boolean, so the columns are ordered by it rather than by their position in the
/// table. A composite key read in declaration order instead of key order is a
/// different key, and this census would then pass a real change.
async fn primary_keys_of(conn: &sea_orm::DatabaseConnection) -> Vec<(String, String)> {
    let tables = objects_of(conn, "table").await;
    let mut keys: Vec<(String, String)> = Vec::new();
    for table in tables {
        let rows = conn
            .query_all_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                format!("PRAGMA table_info({table})"),
            ))
            .await
            .unwrap_or_else(|e| panic!("table_info for {table}: {e}"));
        let mut parts: Vec<(i32, String)> = Vec::new();
        for row in rows {
            let position: i32 = row.try_get("", "pk").expect("the `pk` ordinal");
            if position > 0 {
                parts.push((position, row.try_get("", "name").expect("the column name")));
            }
        }
        parts.sort_by_key(|(position, _)| *position);
        let columns: Vec<String> = parts.into_iter().map(|(_, name)| name).collect();
        // A table with no declared key is recorded as such rather than skipped: the
        // silence D-236 is about is a key that is *absent* from the census, so an
        // empty key must be a line here and not a gap.
        keys.push((table, columns.join(", ")));
    }
    keys
}

/// A stable, dependency-free digest of one string — FNV-1a, 64-bit.
///
/// **Not a security primitive and deliberately not `sha2`** (DE0708 bans it in this
/// crate): what this pins is a transcription, and the adversary is a typo in a
/// hand-rebuilt trigger body rather than a forger.
fn digest(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Every trigger the chain created as `(name, digest-of-body)`, in name order.
///
/// The name census answers "is the trigger there" and **nothing about what it does**.
/// That gap is not hypothetical here: `chk_pricing_approval_subject_kind` re-types eight trigger bodies
/// by hand, because a `SQLite` table rebuild drops the triggers attached to the old
/// table and they have to be re-created — and a `RAISE(ABORT)` whose `WHEN` clause lost
/// a disjunct is a guard that still exists, still has its name, and refuses less.
async fn trigger_bodies(conn: &sea_orm::DatabaseConnection) -> Vec<(String, u64)> {
    let sql = "SELECT name AS n, sql AS v FROM sqlite_master WHERE type = 'trigger' \
               AND name NOT LIKE 'sqlite_%' ORDER BY name";
    let rows = conn
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql.to_owned(),
        ))
        .await
        .expect("query sqlite_master");
    rows.into_iter()
        .map(|row| {
            let name: String = row.try_get("", "n").expect("the trigger's name");
            let body: String = row.try_get("", "v").expect("the trigger's body");
            // Whitespace-normalised, so a reformat that changes no rule does not
            // present as a changed guard.
            let normalised = body.split_whitespace().collect::<Vec<_>>().join(" ");
            (name, digest(&normalised))
        })
        .collect()
}

/// Every object of one `sqlite_master` type the chain created, in name order.
async fn objects_of(conn: &sea_orm::DatabaseConnection, kind: &str) -> Vec<String> {
    let sql = format!(
        "SELECT name AS v FROM sqlite_master WHERE type = '{kind}' \
         AND name NOT LIKE 'sqlite_%' ORDER BY name"
    );
    conn.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Sqlite,
        sql,
    ))
    .await
    .expect("query sqlite_master")
    .iter()
    .map(|row| row.try_get::<String>("", "v").expect("a name"))
    .collect()
}

/// Read every row of an entity under a tenant scope, asserting the table and
/// its column set are what the entity expects.
///
/// Expands to the **table names it read**, which is what
/// [`the_chain_creates_every_table_and_re_runs_cleanly`] compares against
/// [`EXPECTED_TABLES`]. The list used to be a bare hand-enumeration with no
/// completeness check, and it had drifted exactly as that shape does (Z6-4): it
/// named 28 entities while the roster knew 39 tables, so every table added after
/// the census was written — Slices 8, 10, 11 and 12 — was covered by
/// `table_exists` alone, which is existence and not column agreement. The table
/// name comes off `EntityName` rather than being re-typed here, so the comparison
/// is between the migration's roster and the entity's own `table_name` attribute
/// with no third spelling in between.
macro_rules! assert_readable {
    ($conn:expr, $scope:expr, $($entity:path),+ $(,)?) => {{
        let mut read: Vec<String> = Vec::new();
        $(
            let rows = <$entity>::find()
                .secure()
                .scope_with($scope)
                .all($conn)
                .await
                .unwrap_or_else(|e| panic!("{} must be readable: {e}", stringify!($entity)));
            assert!(rows.is_empty(), "{} starts empty", stringify!($entity));
            read.push(EntityName::table_name(&<$entity>::default()).to_owned());
        )+
        read
    }};
}

async fn table_exists(conn: &sea_orm::DatabaseConnection, table: &str) -> bool {
    let sql = format!(
        "SELECT count(*) AS c FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
    );
    let row = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql,
        ))
        .await
        .expect("query sqlite_master")
        .expect("count query returns a row");
    row.try_get::<i32>("", "c").expect("read count") == 1
}

/// The `CREATE INDEX ...` statement `SQLite` recorded for `index`, or `None`
/// when the chain created no index by that name.
async fn index_sql(conn: &sea_orm::DatabaseConnection, index: &str) -> Option<String> {
    let sql = format!(
        "SELECT count(*) AS c FROM sqlite_master WHERE type = 'index' AND name = '{index}'"
    );
    let present = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql,
        ))
        .await
        .expect("query sqlite_master")
        .expect("count query returns a row")
        .try_get::<i32>("", "c")
        .expect("read count")
        == 1;
    if !present {
        return None;
    }
    let sql =
        format!("SELECT sql AS v FROM sqlite_master WHERE type = 'index' AND name = '{index}'");
    let statement = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql,
        ))
        .await
        .expect("query sqlite_master")
        .expect("the index row is there")
        .try_get::<String>("", "v")
        .expect("an index this chain created carries its DDL");
    Some(statement)
}

/// The chain, in the order the platform runner applies it (by migration NAME).
fn name_ordered_chain() -> Vec<Box<dyn MigrationTrait>> {
    let mut chain = Migrator::migrations();
    chain.sort_by(|a, b| a.name().cmp(b.name()));
    chain
}

#[tokio::test]
async fn the_chain_creates_every_table_and_re_runs_cleanly() {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");

    let boot1 = run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("boot 1 must apply the whole chain");
    assert_eq!(
        boot1.applied,
        Migrator::migrations().len(),
        "boot 1 applies every migration"
    );
    assert_eq!(boot1.skipped, 0, "boot 1 skips nothing");

    // Boot 2 over the same database: nothing re-runs. No `CREATE TABLE` in this
    // chain is `IF NOT EXISTS` — not one of the 42 — so a re-run that actually
    // executed would fail loudly here rather than passing silently.
    let boot2 = run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("boot 2 must be a clean no-op");
    assert_eq!(boot2.applied, 0, "boot 2 applies nothing");
    assert_eq!(
        boot2.skipped,
        Migrator::migrations().len(),
        "boot 2 skips every migration"
    );

    let provider = DBProvider::<DbError>::new(db);
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(Uuid::from_u128(1));
    let read = assert_readable!(
        &conn,
        &scope,
        plan::Entity,
        plan_phase::Entity,
        plan_addon_rule::Entity,
        plan_descriptor_set::Entity,
        // D-319's fourth revision-scoped child.
        plan_period_floor_cap::Entity,
        price::Entity,
        price_tier_band::Entity,
        read_model::Entity,
        catalog_version_ref::Entity,
        pin_frontier::Entity,
        policy_object::Entity,
        operator_flag::Entity,
        idempotency_dedup::Entity,
        outbox::Entity,
        audit_log::Entity,
        approval::Entity,
        approval_key::Entity,
        approval_threshold::Entity,
        approval_threshold_tombstone::Entity,
        price_window::Entity,
        // Slice 9's seven. This is what proves each entity's column set is the
        // one its migration built — a mismatch is a runtime `SeaORM` error on
        // the first read and nothing earlier catches it.
        region_taxonomy::Entity,
        brand_taxonomy::Entity,
        partner_taxonomy::Entity,
        org_tier_taxonomy::Entity,
        // Slice 9's own taxonomy (`inst-cg-taxonomy`), on its own route rather
        // than `config`'s four above.
        customer_group_taxonomy::Entity,
        // Slice 9's membership plane (`inst-cg-record`) -- proves the entity's
        // column set matches `pricing_group_membership`'s table.
        group_membership::Entity,
        price_overlay::Entity,
        price_overlay_line::Entity,
        price_overlay_line_amount::Entity,
        // Slice 8's four: the bundle's declaration and its composition. Every one
        // of them was outside this census until Z6-4, so `pricing_bundle`'s
        // `effective_share` — a column two migrations have already moved — was read
        // through its entity by no test in the fast tier.
        bundle::Entity,
        bundle_component::Entity,
        bundle_revshare_group::Entity,
        bundle_revshare::Entity,
        // Slice 10's composite meter, and Slice 11's migration plane.
        composite_meter::Entity,
        migration::Entity,
        snapshot_provenance::Entity,
        // Slice 12's bulk plane: the run, its journal and its row locks.
        bulk_operation::Entity,
        repricing_journal::Entity,
        bulk_row_lock::Entity,
        // D-334's rounding-policy taxonomy (`pricing_rounding_policy_taxonomy`), missing from this
        // roster until 2026-08-20: its entity's column set was read back against
        // its migration by nothing in the fast tier.
        rounding_policy_taxonomy::Entity,
        // D-356's GL-code taxonomy (`pricing_gl_code_taxonomy`), named on the day it landed.
        gl_code_taxonomy::Entity,
    );

    // The completeness half, and the reason the roster above is no longer a
    // hand-enumeration: every table the chain creates is read through its entity,
    // not the ones somebody remembered. `coord_leases` is the single exemption and
    // it is a structural one rather than a debt — `coord` owns that table and does
    // not export an entity for it, which is why property 3 above introspects
    // `sqlite_master` for it instead.
    //
    // **`owed` comes from `sqlite_master`, not from [`EXPECTED_TABLES`].** Compared
    // against that constant, this assertion could not fail for a table absent from
    // *both* hand lists — and one was: `pricing_rounding_policy_taxonomy`, created
    // by `pricing_rounding_policy_taxonomy` on both engines, entity and all, sat in neither from the
    // day it landed until 2026-08-20 while the census read green. The roster and
    // the constant are still both here, and both are now measured: the constant
    // against the database on the line below, the roster against the database on
    // the one after it.
    //
    // The introspection needs a plain `sea_orm` connection, and it has to see the
    // schema after **all** the migrations rather than as an early `CREATE` left it
    // — later migrations in this chain drop and recreate tables — so the chain is
    // applied whole onto a second in-memory database. `run_migrations_for_testing`
    // is not used for it: its bookkeeping table is not part of this gear's schema
    // and would have to be filtered back out by name.
    let bare = Database::connect("sqlite::memory:")
        .await
        .expect("connect a second in-memory sqlite for the introspection");
    let bare_manager = SchemaManager::new(&bare);
    for migration in &name_ordered_chain() {
        migration
            .up(&bare_manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }
    let held: std::collections::BTreeSet<String> =
        objects_of(&bare, "table").await.into_iter().collect();
    assert_eq!(
        held,
        EXPECTED_TABLES
            .iter()
            .map(|table| (*table).to_owned())
            .collect::<std::collections::BTreeSet<String>>(),
        "the table roster and the database disagree: a table the chain creates is named in no \
         list here, or a line here names a table the chain does not create"
    );

    let read: std::collections::BTreeSet<String> = read.into_iter().collect();
    let owed: std::collections::BTreeSet<String> = held
        .into_iter()
        .filter(|table| table != "coord_leases")
        .collect();
    assert_eq!(
        read, owed,
        "a table this chain creates is read through no entity, or an entity names a table the \
         chain does not create: the first is a column disagreement nothing catches before the \
         first production read, and the second is an entity pointed at nothing"
    );
}

#[tokio::test]
async fn the_chain_creates_every_trigger_and_every_index() {
    // The property the whole suite lacked: on `SQLite` a missing guard is invisible
    // to every application-level test, because the rules it enforces are also
    // checked in Rust. `uq_pricing_approval_key_pending` is the sharpest case —
    // delete it and `inst-co-single-pending` degrades from a constraint a concurrent
    // writer cannot step through into a `SELECT` that races, with nothing red.
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    assert_eq!(
        objects_of(&conn, "trigger").await,
        EXPECTED_TRIGGERS,
        "the trigger census: a dropped guard, or one added without a line here"
    );
    assert_eq!(
        objects_of(&conn, "index").await,
        EXPECTED_INDEXES,
        "the index census: a dropped rule, or one added without a line here"
    );
    assert_eq!(
        checks_of(&conn).await,
        EXPECTED_CHECKS,
        "the CHECK census: `SQLite` had none, and two migrations re-type whole table bodies by hand"
    );
    assert_eq!(
        primary_keys_of(&conn).await,
        expected_primary_keys(),
        "the primary-key census (D-236): `pricing_catalog_version_ref`'s key is the physical identity of a \
         truth-linkage table on the seven-year horizon, and every other roster is blind to it. A \
         key is the one piece of DDL whose loss shows up first as a duplicate row in a table whose \
         whole contract is that it has none"
    );
    assert_eq!(
        trigger_bodies(&conn).await,
        expected_trigger_bodies(),
        "the trigger **body** census: a name census cannot see a `RAISE(ABORT)` whose `WHEN` lost \
         a disjunct, and the chain re-types eight of these bodies by hand. A legitimate change \
         to a trigger re-pins its digest here, deliberately"
    );
}

#[tokio::test]
async fn the_pending_key_register_is_unique_and_partial_on_submitted() {
    // The **shape** and not only the name, because the two halves say different
    // things and only one of them is `inst-co-single-pending`. `UNIQUE` alone would
    // say "one row per key ever", which refuses a second unit over a key whose first
    // unit was decided and withdrawn - the escape `inst-as-void` exists to give. The
    // `WHERE state = 'submitted'` predicate **is** the rule: a decided unit holds
    // nothing.
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    let ddl = index_sql(&conn, "uq_pricing_approval_key_pending")
        .await
        .expect("the register's constraint");
    let upper = ddl.to_ascii_uppercase();
    assert!(upper.contains("UNIQUE"), "one holder per key: {ddl}");
    assert!(
        ddl.contains("(tenant_id, scope_key)"),
        "and the key is per tenant: {ddl}"
    );
    assert!(
        ddl.contains("WHERE state = 'submitted'"),
        "partial on `submitted`, so a decided unit frees its keys: {ddl}"
    );
}

/// D-196 clause (2): both scope-key indexes carry the usage line, **through the
/// sentinel** — and the behaviour, not only the DDL.
///
/// The DDL half is the cheap half. The behavioural half is the one that matters,
/// because the naive widening — listing `meter` itself — produces DDL that reads
/// correct and silently stops constraining every non-usage key, `meter` being
/// nullable and NULLs being distinct inside a `UNIQUE`. So this asserts both
/// directions on the engine the fast gate runs:
///
///   - two usage lines differing only in `meter` are two keys (D-103's example,
///     which the eight-axis key refused);
///   - two rows with **no** meter on one key still collide (the hole).
///
/// The rows go in as raw SQL rather than through the repository on purpose: the
/// repository is the layer that cannot see a guard stop refusing, and until
/// clause (3) it does not carry the pair from the key onto the columns anyway.
#[tokio::test]
async fn both_scope_key_indexes_carry_the_usage_line_through_the_sentinel() {
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    for index in [
        "uq_pricing_price_scope_key_current",
        "uq_pricing_price_scope_key_draft",
    ] {
        let ddl = index_sql(&conn, index).await.expect("the scope-key index");
        assert!(
            ddl.contains("COALESCE(meter, '')"),
            "the meter axis must be indexed through the sentinel, not as the nullable \
             column: {ddl}"
        );
        assert!(
            ddl.contains("dimension_key"),
            "the tenth axis belongs to the key too: {ddl}"
        );
    }

    let row = |id: u32, state: &str, meter: &str| {
        format!(
            "INSERT INTO pricing_price (price_id, tenant_id, plan_id, currency, region, phase, \
             charge_kind, model_kind, amount_minor, lifecycle_state, created_by, created_at_utc, \
             meter) VALUES ('{id:0>8}-0000-0000-0000-000000000000', \
             '11111111-1111-1111-1111-111111111111', '22222222-2222-2222-2222-222222222222', \
             'USD', 'EU', '33333333-3333-3333-3333-333333333333', 'usage', 'per_unit', 1000, \
             '{state}', '44444444-4444-4444-4444-000000000000', '2026-08-03 09:00:00+00', {meter})"
        )
    };
    let exec = async |sql: String| {
        conn.execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql,
        ))
        .await
    };

    // D-103's example: two meters, one market, two keys — on both planes.
    exec(row(1, "published", "'cloudlets'"))
        .await
        .expect("the first usage line takes its key");
    exec(row(2, "published", "'egress_gb'"))
        .await
        .expect("a second meter is a second key, which is the whole of D-196");
    exec(row(3, "draft", "'cloudlets'"))
        .await
        .expect("the draft plane admits the same pair");
    exec(row(4, "draft", "'egress_gb'"))
        .await
        .expect("a second meter is a second draft key too");

    // And the hole stays closed: no meter is one key, not one key per row.
    exec(row(5, "published", "NULL"))
        .await
        .expect("a meterless usage row takes the sentinel key");
    let collision = exec(row(6, "published", "NULL"))
        .await
        .expect_err("two meterless usage rows share one key and the second must be refused");
    assert!(
        collision
            .to_string()
            .contains("uq_pricing_price_scope_key_current"),
        "the refusal must come from the scope-key index: {collision}"
    );
}

/// D-09's cross-group non-overlap invariant, the `SQLite` arm
/// (`pricing_group_membership`'s trigger pair), proved behaviourally rather than by name:
/// the trigger census only proves the guard exists, not that it refuses what it
/// claims to.
///
/// The case §3 `inst-cg-resolve` is actually about — two **different**
/// `group_value`s colliding for one payer — plus the two shapes a wrong rule
/// would get wrong in the opposite direction: sequential future-dated
/// memberships (2026-07-28 review fix; a rule refusing these is wrong) and
/// half-open boundary adjacency (`effective_to = next.effective_from` is legal,
/// not a false positive).
#[tokio::test]
async fn group_membership_non_overlap_refuses_across_groups_and_admits_adjacency() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PAYER: &str = "22222222-2222-2222-2222-222222222222";
    const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    let row = |id: u32, group_value: &str, from: &str, to: &str| {
        format!(
            "INSERT INTO pricing_group_membership (
                 membership_id, tenant_id, payer_tenant_id, group_value,
                 effective_from, effective_to, created_by, created_at_utc)
             VALUES ('{id:0>8}-0000-0000-0000-000000000000', '{TENANT}', '{PAYER}', \
             '{group_value}', '{from}', {to}, '{ACTOR}', '2026-08-11 09:00:00')"
        )
    };
    let exec = async |sql: String| {
        conn.execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            sql,
        ))
        .await
    };

    // The case D-09 is actually about: `trial` from Jan-Jun, then `vip`
    // starting inside it in March -- a different group, same payer.
    exec(row(
        1,
        "trial",
        "2026-01-01 00:00:00",
        "'2026-06-01 00:00:00'",
    ))
    .await
    .expect("the first membership takes its interval");
    let collision = exec(row(
        2,
        "vip",
        "2026-03-01 00:00:00",
        "'2026-09-01 00:00:00'",
    ))
    .await
    .expect_err("a different group overlapping the same payer's live membership must be refused");
    assert!(
        collision.to_string().contains("pricing_group_membership"),
        "the refusal must name the table's own guard: {collision}"
    );

    // Boundary: starting exactly where the first ends is adjacency, not a
    // collision -- the interval is half-open.
    exec(row(
        3,
        "vip",
        "2026-06-01 00:00:00",
        "'2026-09-01 00:00:00'",
    ))
    .await
    .expect("an interval starting where another ends is legal ([)-half-open)");

    // Sequential future-dated memberships are legal, whatever group they land in.
    exec(row(
        4,
        "trial",
        "2099-01-01 00:00:00",
        "'2099-06-01 00:00:00'",
    ))
    .await
    .expect("a future-dated membership on a payer with no live interval there must land");
    exec(row(
        5,
        "vip",
        "2099-08-01 00:00:00",
        "'2099-12-01 00:00:00'",
    ))
    .await
    .expect("two sequential future-dated memberships must both be accepted");

    // Open-ended (`effective_to = NULL`) still reads as unbounded, not as "no
    // constraint applies" -- a later interval starting inside it must still be
    // refused. A distinct time window (2150) so this pair cannot interact with
    // rows 1-5 above. Row 6 is what makes the SQLite trigger's `IS NULL OR`
    // branches reachable at all on the tier that runs on every commit; without
    // it a simplification that dropped them (e.g. to a bare `existing.effective_to
    // > NEW.effective_from`) would only redden the Docker-gated Postgres suite.
    exec(row(6, "trial", "2150-01-01 00:00:00", "NULL"))
        .await
        .expect("an open-ended membership takes its interval");
    let open_ended_collision = exec(row(
        7,
        "vip",
        "2150-06-01 00:00:00",
        "'2150-12-01 00:00:00'",
    ))
    .await
    .expect_err(
        "row 7 starts inside row 6's open-ended interval, in a different group; the open \
         end must be read as unbounded and refuse it",
    );
    assert!(
        open_ended_collision
            .to_string()
            .contains("pricing_group_membership"),
        "the refusal must come from row 6's open-ended interval, not some other guard: \
         {open_ended_collision}"
    );
}

#[tokio::test]
async fn the_policy_mint_guard_is_unique_per_tenant_and_partial_on_both_conjuncts() {
    // The **shape** and not only the name, for the sibling case's reason: each half of
    // this predicate refuses a different legitimate flow if it goes missing, and the
    // name census cannot see either loss.
    //
    // * without `subject_kind = 'policy'` the index would refuse a tenant's second
    //   **plan-revision or window** unit - one tenant holding several of those at once
    //   is the ordinary case, and `inst-co-single-pending` puts that rule on
    //   `pricing_approval_key` per canonical scope key rather than per tenant;
    // * without `state = 'submitted'` it would say "one policy proposal per tenant
    //   **ever**", and a tenant whose first proposal was decided or withdrawn could
    //   never author a second version.
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    let ddl = index_sql(&conn, "uq_pricing_approval_policy_pending")
        .await
        .expect("D-192's mint guard");
    assert!(
        ddl.to_ascii_uppercase().contains("UNIQUE"),
        "one open policy proposal: {ddl}"
    );
    assert!(
        ddl.contains("(tenant_id)"),
        "and it is per tenant, not per deployment: {ddl}"
    );
    assert!(
        ddl.contains("subject_kind = 'policy'"),
        "narrowed to the plane where the rule is per tenant: {ddl}"
    );
    assert!(
        ddl.contains("state = 'submitted'"),
        "partial on `submitted`, so a decided proposal holds nothing: {ddl}"
    );
}

#[tokio::test]
async fn down_then_up_round_trips() {
    // A raw `SeaORM` connection: `SchemaManager` needs one, and the toolkit
    // runner owns bookkeeping but exposes no `down` — this walks the chain the
    // way a rollback would.
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    let chain = name_ordered_chain();

    for migration in &chain {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }
    for table in EXPECTED_TABLES {
        assert!(
            table_exists(&conn, table).await,
            "the chain must create `{table}`"
        );
    }

    for migration in chain.iter().rev() {
        migration
            .down(&manager)
            .await
            .unwrap_or_else(|e| panic!("down {} must succeed: {e}", migration.name()));
    }
    for table in EXPECTED_TABLES {
        assert!(
            !table_exists(&conn, table).await,
            "`{table}` must be gone after down"
        );
    }

    for migration in &chain {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("re-up {} must succeed: {e}", migration.name()));
    }
    // **The second `up` gets the same census as the first.** Asserting only
    // `EXPECTED_TABLES` here would leave the forward half of a rollback-then-forward
    // with no trigger, index or CHECK census at all, so a migration that dropped a
    // guard on the way back up would leave this green.
    assert_eq!(
        objects_of(&conn, "trigger").await,
        EXPECTED_TRIGGERS,
        "the re-up must restore every trigger"
    );
    assert_eq!(
        objects_of(&conn, "index").await,
        EXPECTED_INDEXES,
        "the re-up must restore every index"
    );
    assert_eq!(
        checks_of(&conn).await,
        EXPECTED_CHECKS,
        "the re-up must restore every CHECK"
    );
    assert_eq!(
        primary_keys_of(&conn).await,
        expected_primary_keys(),
        "the re-up must restore every primary key. This is the direction D-236 names as equally \
         silent: a migration that restates a table and drops a key column"
    );
    assert_eq!(
        trigger_bodies(&conn).await,
        expected_trigger_bodies(),
        "and restore them with the same bodies, not merely the same names"
    );
    for table in EXPECTED_TABLES {
        assert!(
            table_exists(&conn, table).await,
            "`{table}` must be back after the re-up"
        );
    }
}

#[tokio::test]
async fn the_version_index_on_the_ref_table_is_not_unique() {
    // The amendment of 2026-08-03, pinned so it cannot regress into the shape
    // it replaced. `uq_pricing_catalog_version_ref_version` asserted a
    // bijection from committed version to publish — and under the registry's
    // batching (D-47, §4.2 step 5) several of one tenant's pending refs commit
    // into ONE version, which is the case the whole model exists to serve and
    // which that index made physically impossible: the second finalize failed.
    // D-157's subject columns already answer "which publish produced this
    // version", and the honest answer is a set.
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    assert_eq!(
        index_sql(&conn, "uq_pricing_catalog_version_ref_version").await,
        None,
        "the unique version index is gone: it refused D-47's normal case"
    );
    let created = index_sql(&conn, "idx_pricing_catalog_version_ref_version")
        .await
        .expect("the version index the projector and the frontier walk read");
    assert!(
        !created.to_ascii_uppercase().contains("UNIQUE"),
        "the replacement must be non-unique, got: {created}"
    );
}

#[tokio::test]
async fn the_ref_tables_key_carries_the_subject_as_well_as_the_handle() {
    // `pricing_catalog_version_ref` (D-234), pinned **because no roster covers it**. This
    // suite carries five rosters — tables, triggers, indexes, CHECK names and
    // trigger bodies — and a primary key is none of them, so the chain changing
    // the physical identity of a truth-linkage table passed every store census
    // in this file silently. That gap is general and is recorded as such; this
    // test closes it for the one table the change was about.
    //
    // What it pins: a handle names one registry assignment, and a publish unit
    // records against it every subject it projects — one on the plan plane, two
    // on the overlay plane and three when a revision moves the scope value
    // (D-112, D-133). Narrowed back to the handle, the second subject of an
    // overlay publish is refused by the key and the act cannot commit at all.
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    let ddl = table_sql(&conn, "pricing_catalog_version_ref").await;
    let normalised = ddl.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalised.contains("PRIMARY KEY (tenant_id, pending_ref, subject_kind, subject_ref)"),
        "the key must carry the subject, got: {normalised}"
    );
}

/// The `CREATE TABLE ...` statement `SQLite` recorded for `table`.
async fn table_sql(conn: &sea_orm::DatabaseConnection, table: &str) -> String {
    let sql =
        format!("SELECT sql AS v FROM sqlite_master WHERE type = 'table' AND name = '{table}'");
    conn.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Sqlite,
        sql,
    ))
    .await
    .expect("query sqlite_master")
    .expect("the table row is there")
    .try_get::<String>("", "v")
    .expect("a table this chain created carries its DDL")
}

#[tokio::test]
async fn the_ref_table_records_the_commit_observation_and_pairs_it_with_nothing() {
    // D-166 clause (1). The column is what every post-commit clause in the set
    // was written against and none of them had: `requested_at` measures the
    // batching wait the requirement explicitly puts OUTSIDE degraded handling,
    // and `committed_at` is stamped by the finalize, which is the step that
    // never runs on the path the signal exists for.
    //
    // The absence of a CHECK is the assertion's other half.
    // `chk_pricing_catalog_version_ref_commit` exists because a version and its
    // commit instant are one fact; this column's whole purpose is to be settable
    // while `catalog_version` is still NULL.
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    let ddl = table_sql(&conn, "pricing_catalog_version_ref").await;
    assert!(
        ddl.contains("commit_observed_at"),
        "the SQLite arm must create the column: {ddl}"
    );
    // Collapse whitespace before matching the declaration. The re-authored chain aligns
    // its column list, so `commit_observed_at      text` is the same declaration that a
    // single-space match would miss -- and missing it would read as a CHECK mentioning
    // the column, which is the opposite of what this asserts.
    let flat = ddl.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        !flat
            .replace("commit_observed_at text", "")
            .contains("commit_observed_at"),
        "and pair it with nothing - no CHECK may mention it: {ddl}"
    );
}

/// D-240: `tax_display_mode` is retired, and the rebuild that retires it kept
/// everything else `pricing_policy_object` carries.
///
/// The column is dropped by a create-copy-drop-rename, because `SQLite` refuses
/// to drop a column a CHECK names. That makes the *retained* half the assertion
/// that matters: a rebuild silently omitting an arm is the failure this shape
/// has, and it is invisible to a case that only checks the column is gone. So
/// every surviving column and every surviving CHECK is named here rather than
/// counted — a count passes against a rebuild that dropped one arm and grew
/// another.
///
/// `tax_display_policy_mode` is listed among them deliberately. It is the column a
/// hand-written column list is most likely to lose — it belongs to Slice 4's C4
/// switch rather than to the policy object's original shape — and a list that drops
/// it still looks complete.
#[tokio::test]
async fn the_retired_tax_display_mode_leaves_every_other_policy_column_standing() {
    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    for migration in &name_ordered_chain() {
        migration
            .up(&manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }

    let ddl = table_sql(&conn, "pricing_policy_object").await;

    assert!(
        !ddl.contains("tax_display_mode"),
        "D-240 retires the column, so no arm of the table may still name it: {ddl}"
    );
    assert!(
        !ddl.contains("chk_pricing_policy_object_tax_display CHECK"),
        "and the CHECK that guarded it goes with it: {ddl}"
    );

    for column in [
        "tenant_id",
        "default_rounding_policy_ref",
        "enforced_migration_notice_days",
        "max_tier_bands_per_row",
        "max_price_rows_per_plan",
        "max_custom_interval_days",
        "max_custom_interval_months",
        "additional_required_descriptors",
        "updated_at_utc",
        "updated_by",
        "tax_display_policy_mode",
    ] {
        assert!(
            ddl.contains(column),
            "the rebuild must carry `{column}` across: {ddl}"
        );
    }

    for check in [
        "chk_pricing_policy_object_notice_floor",
        "chk_pricing_policy_object_tier_band_cap",
        "chk_pricing_policy_object_price_row_cap",
        "chk_pricing_policy_object_interval_days_cap",
        "chk_pricing_policy_object_interval_months_cap",
        "chk_pricing_policy_object_tax_display_policy",
    ] {
        assert!(
            ddl.contains(check),
            "the rebuild must carry `{check}` across: {ddl}"
        );
    }
}

// ---------------------------------------------------------------------------
// Rebuilds and tightenings over a database that is **not** empty.
//
// Every case above this line boots the chain against a fresh in-memory
// database, so every rebuild in the chain has only ever dropped an empty parent
// table and every tightened CHECK has only ever been applied to a table with no
// rows. That is the one state a deployed database is never in, and it is the
// state three of these migrations were measured to fail in.
//
// **What this section can still say changed when the chain was squashed, and the
// banner outlived it.** It promised cases that "stage the chain at one migration,
// write the rows, and then apply it", while all three below applied
// `&name_ordered_chain()` — the whole chain onto an empty database — and inserted
// afterwards: the exact shape the paragraph above contrasts itself with. Measured
// at this commit, no `up` arm in the chain carries a `DROP TABLE`, a `RENAME TO`,
// an `ALTER TABLE` or an `INSERT`: every one of the 42 is a pure create, each
// table is created **once** with every CHECK and every trigger inline, and the
// three re-authoring migrations the paragraph above refers to are not separate
// steps any more. So there is no state in which one of these tables exists and the
// guard under test does not, and a case cannot be written to reach one.
//
// What is still expressible is the other half of the same claim, and it is what
// [`staged_to`] does: apply the chain **as far as the migration that creates the
// table**, write the rows there, then apply the **remainder** over a populated
// database. Every later create then runs against a schema holding rows — its
// foreign keys resolve against occupied parents rather than empty ones — which is
// the state a deployment is always in and no case above this line reaches. Each
// case asserts its rows survived the remainder and re-asks its refusal afterwards,
// because a remainder that silently dropped the row it was applied over is the
// failure this shape has.
// ---------------------------------------------------------------------------

/// Run one statement and hand back the engine's own words on failure.
///
/// Not `expect`: half these cases need the refusal, and the message is what they
/// assert. The error is stringified rather than returned so the two callers can
/// share one helper across `DbErr` and the driver's own type.
async fn try_exec(conn: &sea_orm::DatabaseConnection, sql: &str) -> Result<(), String> {
    conn.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Sqlite,
        sql.to_owned(),
    ))
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// One `count(*)` under the alias `n`.
///
/// The cases below use it to say their rows are still there after the remainder of
/// the chain has been applied over them — which a `SELECT` for the row's own value
/// could not, because a rebuild that dropped and recreated the table empty would
/// answer "absent" identically to one that never held it.
async fn count(conn: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    conn.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Sqlite,
        sql.to_owned(),
    ))
    .await
    .expect("run the count")
    .first()
    .expect("a count query returns one row")
    .try_get::<i64>("", "n")
    .expect("read the count")
}

async fn must_apply(conn: &sea_orm::DatabaseConnection, sql: &str) {
    try_exec(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("this statement must land: {sql}\n{e}"));
}

async fn apply(manager: &SchemaManager<'_>, chain: &[Box<dyn MigrationTrait>]) {
    for migration in chain {
        migration
            .up(manager)
            .await
            .unwrap_or_else(|e| panic!("up {} must succeed: {e}", migration.name()));
    }
}

/// Split the chain either side of the migration whose name contains `stem`, and
/// apply the first half.
///
/// Hands back the **remainder**, which each caller applies once its rows are in
/// place. The stem is matched rather than an index written down, because an index
/// is a number that silently means a different migration the moment one is
/// inserted before it — and every case here needs the split to be *exactly* after
/// the migration that creates the table it writes to.
///
/// It panics on no match and on more than one: a stem that matched nothing would
/// leave the caller staging the whole chain and writing afterwards, which is the
/// shape this section exists to stop being.
async fn staged_to(manager: &SchemaManager<'_>, stem: &str) -> Vec<Box<dyn MigrationTrait>> {
    let chain = name_ordered_chain();
    let matches: Vec<usize> = chain
        .iter()
        .enumerate()
        .filter(|(_, m)| m.name().contains(stem))
        .map(|(i, _)| i)
        .collect();
    let [index] = matches[..] else {
        panic!("`{stem}` must name exactly one migration of the chain, matched {matches:?}");
    };

    let mut chain = chain;
    let remainder = chain.split_off(index + 1);
    apply(manager, &chain).await;
    remainder
}

/// **The two taxonomies created *after* the tightening carry it too** — D-242
/// (`pricing_region_taxonomy`) on the tables that were not there when it landed.
///
/// `pricing_customer_group_taxonomy` and `pricing_rounding_policy_taxonomy` are the
/// Slice 4 shape on their own routes, and the loosened predicate `length(value) > 0`
/// is the one they are most likely to be written with — D-242 replaced it on the
/// four original siblings, so a sixth and seventh table copied from the older shape
/// reintroduces it. Such a table is invisible to every case that walks a fixed
/// roster, and both suites that assert this refusal walk one:
/// `sqlite_taxonomy_store::TAXONOMIES` and `postgres_schema_taxonomy::TAXONOMIES`
/// each name the original four.
///
/// The damage is the one D-242 records: `ScopeValue::new` trims before
/// it decides, so `'   '` is a value the store admitted and the domain refuses,
/// and `taxonomy_repo`'s readers map that to `RepoError::CorruptRow` — one such
/// row fails `GET` for **every** value in the class, with the `PUT` unable to
/// round-trip a list it cannot read.
///
/// The control is D-242's: `' EU '` **lands**. The predicate refuses
/// a value with no non-blank character at all, never a value that merely needs a
/// trim.
#[tokio::test]
async fn the_taxonomies_created_after_the_tightening_refuse_whitespace_too() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const LATER_TAXONOMIES: &[&str] = &[
        "pricing_customer_group_taxonomy",
        "pricing_rounding_policy_taxonomy",
        "pricing_gl_code_taxonomy",
    ];

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    // Staged at `pricing_gl_code_taxonomy`, the **latest** of the three: it is the
    // earliest point at which all three tables exist, so the rows below are written
    // under a partially applied chain and the remainder runs over them.
    let remainder = staged_to(&manager, "create_pricing_gl_code_taxonomy").await;

    for table in LATER_TAXONOMIES {
        let refused = try_exec(
            &conn,
            &format!(
                "INSERT INTO {table} (tenant_id, value, display_name, state) \
                 VALUES ('{TENANT}', '   ', 'blank', 'active')"
            ),
        )
        .await
        .expect_err("a whitespace-only value must be refused by the store");
        assert!(
            refused.contains(&format!("chk_{table}_value_present")),
            "and refused by that predicate rather than by something else: {refused}"
        );

        must_apply(
            &conn,
            &format!(
                "INSERT INTO {table} (tenant_id, value, display_name, state) \
                 VALUES ('{TENANT}', ' EU ', 'padded', 'active')"
            ),
        )
        .await;
    }

    apply(&manager, &remainder).await;

    for table in LATER_TAXONOMIES {
        assert_eq!(
            count(&conn, &format!("SELECT count(*) AS n FROM {table}")).await,
            1,
            "the remainder of the chain must leave the row it was applied over standing"
        );
        // The same statement, re-asked: a CHECK the remainder of the chain dropped
        // and did not carry across is the failure this shape has, and it is
        // indistinguishable from the guard never having existed unless it is asked
        // twice.
        //
        // Spaces, because the subject here is the remainder of the chain and not the
        // width of the blank. Both predicates name their character set, so a tab is
        // refused too; that is
        // `every_taxonomy_value_predicate_refuses_ascii_whitespace_alone`'s, over
        // every one of the set on every one of the six tables.
        let refused = try_exec(
            &conn,
            &format!(
                "INSERT INTO {table} (tenant_id, value, display_name, state) \
                 VALUES ('{TENANT}', '  ', 'blank', 'active')"
            ),
        )
        .await
        .expect_err("and the predicate must still refuse afterwards");
        assert!(
            refused.contains(&format!("chk_{table}_value_present")),
            "by the same predicate: {refused}"
        );
    }
}

/// Every character the blankness predicates strip, as a code point.
///
/// ASCII whitespace entire. `ScopeValue::new` is Rust's `str::trim`, which strips
/// every character carrying the Unicode `White_Space` property; what this set
/// cannot reach is stated on `pricing_region_taxonomy`'s migration.
const STRIPPED_WHITESPACE: &[u32] = &[9, 10, 11, 12, 13, 32];

/// A value that pads a real one. The control for every case below: the predicates
/// refuse a value with no non-blank character at all, never one that merely needs
/// a trim.
const PADDED: &str = " EU ";

/// **Every taxonomy's `value` predicate refuses ASCII whitespace alone** — D-242
/// (`pricing_region_taxonomy`), on each of the six tables and each character the
/// predicate strips.
///
/// One character per statement rather than one mixed string: `trim(X, Y)` takes a
/// *set*, and a set that lost a member still refuses a string holding the others,
/// so a mixed value cannot say which members are actually in the set.
///
/// `ScopeValue::new` refuses every one of these, and `taxonomy_repo`'s readers map
/// a value the domain refuses to `RepoError::CorruptRow` — one such row fails `GET`
/// for **every** value in its class, with the `PUT` unable to round-trip a list it
/// cannot read. The predicate stops the row existing rather than coping with it.
#[tokio::test]
async fn every_taxonomy_value_predicate_refuses_ascii_whitespace_alone() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const TAXONOMIES: &[&str] = &[
        "pricing_brand_taxonomy",
        "pricing_customer_group_taxonomy",
        "pricing_org_tier_taxonomy",
        "pricing_partner_taxonomy",
        "pricing_region_taxonomy",
        "pricing_rounding_policy_taxonomy",
        "pricing_gl_code_taxonomy",
    ];

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    apply(&manager, &name_ordered_chain()).await;

    for table in TAXONOMIES {
        for code in STRIPPED_WHITESPACE {
            let refused = try_exec(
                &conn,
                &format!(
                    "INSERT INTO {table} (tenant_id, value, display_name, state) \
                     VALUES ('{TENANT}', char({code}), 'blank', 'active')"
                ),
            )
            .await
            .expect_err(&format!(
                "a {table} value of nothing but char({code}) must be refused by the store"
            ));
            assert!(
                refused.contains(&format!("chk_{table}_value_present")),
                "and by that predicate rather than by a neighbouring guard on the same \
                 table: {refused}"
            );
        }

        must_apply(
            &conn,
            &format!(
                "INSERT INTO {table} (tenant_id, value, display_name, state) \
                 VALUES ('{TENANT}', '{PADDED}', 'padded', 'active')"
            ),
        )
        .await;
    }
}

/// **A composite's `output_unit` may not be ASCII whitespace alone** —
/// `chk_pricing_composite_meter_output_unit`, the taxonomies' predicate on another
/// column, held to the same set.
///
/// A unit of nothing but blanks renders on an invoice line as a blank and joins no
/// meter to any unit, and `uq_pricing_composite_meter_output` then holds it as if it
/// were a name — one blank unit per revision, reserved.
///
/// Pinned by the constraint's own name, because a table-name discriminator is shared
/// by every guard here — the draft-only arm, the missing-parent arm, the
/// same-tenant arm — and would pass for whichever one answered.
#[tokio::test]
async fn the_composite_output_unit_refuses_ascii_whitespace_alone() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-2222-2222-2222-222222222222";
    const ACTOR: &str = "44444444-4444-4444-4444-000000000000";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    apply(&manager, &name_ordered_chain()).await;

    // The parent has to be a `draft` revision of this tenant, or
    // `trg_pricing_composite_meter_no_insert` answers ahead of the CHECK -- SQLite
    // runs a `BEFORE` trigger before it checks constraints -- and the case would
    // pass on the wrong guard.
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_plan (plan_id, revision, tenant_id, lifecycle_state, \
             created_by, created_at_utc) \
             VALUES ('{PLAN}', 0, '{TENANT}', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;

    let composite = |id: u32, unit: &str| {
        format!(
            "INSERT INTO pricing_composite_meter (tenant_id, plan_id, plan_revision, \
             composite_id, constituent_units, formula, output_unit) \
             VALUES ('{TENANT}', '{PLAN}', 0, '{id:0>8}-0000-0000-0000-000000000000', \
             '[\"vcpu\"]', '{{\"op\":\"sum\"}}', {unit})"
        )
    };

    for code in STRIPPED_WHITESPACE {
        let refused = try_exec(&conn, &composite(*code, &format!("char({code})")))
            .await
            .expect_err(&format!(
                "an output unit of nothing but char({code}) must be refused by the store"
            ));
        assert!(
            refused.contains("chk_pricing_composite_meter_output_unit"),
            "and by the output-unit predicate rather than by one of the table's three \
             trigger arms: {refused}"
        );
    }

    must_apply(&conn, &composite(99, &format!("'{PADDED}'"))).await;
}

/// **A membership's `group_value` may not be blank, of any width** —
/// `chk_pricing_group_membership_group_value_present`.
///
/// The group value is the name `inst-cg-resolve` resolves a payer's price by, and
/// `required_group` mints the path segment through `ScopeValue::new`, which trims, so
/// a blank one is a group no writer in the gear can produce, no reader can address,
/// and nothing can tell from another blank. `length(group_value) > 0` admits a
/// **single space**, which is why the space is asserted here alongside the rest of
/// the set.
///
/// One payer for every refused statement: none of them lands, so
/// `trg_pricing_group_membership_no_overlap_*` has no interval to collide with and
/// the CHECK is what answers. The control takes its own window for the same reason.
#[tokio::test]
async fn the_group_membership_group_value_refuses_ascii_whitespace_alone() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PAYER: &str = "22222222-2222-2222-2222-222222222222";
    const ACTOR: &str = "44444444-4444-4444-4444-000000000000";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    apply(&manager, &name_ordered_chain()).await;

    let membership = |id: u32, group_value: &str| {
        format!(
            "INSERT INTO pricing_group_membership (membership_id, tenant_id, \
             payer_tenant_id, group_value, effective_from, effective_to, created_by, \
             created_at_utc) \
             VALUES ('{id:0>8}-0000-0000-0000-000000000000', '{TENANT}', '{PAYER}', \
             {group_value}, '2026-01-01 00:00:00', NULL, '{ACTOR}', \
             '2026-08-11 09:00:00')"
        )
    };

    for code in STRIPPED_WHITESPACE {
        let refused = try_exec(&conn, &membership(*code, &format!("char({code})")))
            .await
            .expect_err(&format!(
                "a group value of nothing but char({code}) must be refused by the store"
            ));
        assert!(
            refused.contains("chk_pricing_group_membership_group_value_present"),
            "and by the group-value predicate rather than by the non-overlap trigger \
             pair: {refused}"
        );
    }

    must_apply(&conn, &membership(99, &format!("'{PADDED}'"))).await;
}

/// **A rev-share party is held to both of `Party::new`'s refusals** —
/// `chk_pricing_bundle_revshare_party`.
///
/// Two clauses, and the trim is load-bearing in each. `length(party) > 0` admits a
/// single space, the loosest form on the chain; `party <> 'platform'` compares the
/// **stored** text, so `' platform '` satisfied it while trimming to the sentinel —
/// a party row forging the token `pricing_bundle_revshare_group` uses for D-07's
/// default, which is the one thing that table's doc says the sentinel's safety rests
/// on. `Party::new` refuses both, and `bundle_repo::load_composition` mints every
/// stored party through it and folds a refusal to `RepoError::CorruptRow`, so one
/// such row fails the whole bundle's composition read.
///
/// The padded sentinel is asserted separately from the whitespace widths because a
/// trim on the blankness clause alone leaves it admitted, and the bare `'platform'`
/// case — which stood before either trim — cannot see it.
///
/// Pinned by the constraint's own name: this table also carries a share bound, an
/// effective-share bound and the group foreign key.
#[tokio::test]
async fn the_revshare_party_predicate_refuses_a_blank_and_a_padded_sentinel() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-2222-2222-2222-222222222222";
    const BUNDLE: &str = "55555555-5555-5555-5555-555555555555";
    const VENDOR: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    const ACTOR: &str = "44444444-4444-4444-4444-000000000000";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    apply(&manager, &name_ordered_chain()).await;

    // A draft revision, its bundle and its group: the party row's foreign key and
    // the table's append-only trigger both resolve through them, and either would
    // answer ahead of the CHECK.
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_plan (plan_id, revision, tenant_id, lifecycle_state, \
             created_by, created_at_utc) \
             VALUES ('{PLAN}', 0, '{TENANT}', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_bundle (bundle_id, tenant_id, plan_id, price_basis, \
             invoice_itemization) \
             VALUES ('{BUNDLE}', '{TENANT}', '{PLAN}', 'sum_of_parts', 'aggregate')"
        ),
    )
    .await;
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_bundle_revshare_group (bundle_id, plan_revision, \
             vendor_sku_id, tenant_id, platform_cut_bp, residual_absorber_party) \
             VALUES ('{BUNDLE}', 0, '{VENDOR}', '{TENANT}', 1000, 'platform')"
        ),
    )
    .await;

    let party = |value: &str| {
        format!(
            "INSERT INTO pricing_bundle_revshare (bundle_id, plan_revision, \
             vendor_sku_id, party, tenant_id, share_bp) \
             VALUES ('{BUNDLE}', 0, '{VENDOR}', {value}, '{TENANT}', 9000)"
        )
    };

    for code in STRIPPED_WHITESPACE {
        let refused = try_exec(&conn, &party(&format!("char({code})")))
            .await
            .expect_err(&format!(
                "a party of nothing but char({code}) must be refused by the store"
            ));
        assert!(
            refused.contains("chk_pricing_bundle_revshare_party"),
            "and by the party predicate rather than by a bound or the group key: {refused}"
        );
    }

    // The sentinel, forged with padding on each side and with a tab.
    for forged in ["' platform '", "'platform '", "char(9) || 'platform'"] {
        let refused = try_exec(&conn, &party(forged)).await.expect_err(&format!(
            "{forged} trims to the reserved sentinel and must be refused"
        ));
        assert!(
            refused.contains("chk_pricing_bundle_revshare_party"),
            "and by the party predicate: {refused}"
        );
    }

    // The controls: a padded party is a party — `Party::new` reads `' acme '` back as
    // `acme` — and the predicate is about a value with nothing in it, or a value that
    // is the sentinel wearing padding, and about nothing else a name can carry.
    must_apply(&conn, &party("' acme '")).await;
}

/// **The absorber predicate is `Absorber::parse`'s two arms** —
/// `chk_pricing_bundle_revshare_group_absorber`.
///
/// The column holds the `platform` sentinel (D-07's default, so an unnominated state
/// cannot exist) or a party of the group, and `Absorber::parse` reads the sentinel by
/// equality **before** it tries `Party::new`. So `' platform '` falls through to
/// `Party::new`, which trims and refuses it for spelling the sentinel: a value that
/// is neither the default nor a nomination, and `bundle_repo::load_composition` folds
/// it to `RepoError::CorruptRow` over the whole bundle.
///
/// The sentinel's own row is the control that makes this falsifiable in the other
/// direction: a predicate that simply trimmed the column would refuse every
/// unnominated group, which is the default and by far the common case.
#[tokio::test]
async fn the_absorber_predicate_refuses_a_blank_and_a_padded_sentinel() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-2222-2222-2222-222222222222";
    const BUNDLE: &str = "55555555-5555-5555-5555-555555555555";
    const ACTOR: &str = "44444444-4444-4444-4444-000000000000";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    apply(&manager, &name_ordered_chain()).await;

    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_plan (plan_id, revision, tenant_id, lifecycle_state, \
             created_by, created_at_utc) \
             VALUES ('{PLAN}', 0, '{TENANT}', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_bundle (bundle_id, tenant_id, plan_id, price_basis, \
             invoice_itemization) \
             VALUES ('{BUNDLE}', '{TENANT}', '{PLAN}', 'sum_of_parts', 'aggregate')"
        ),
    )
    .await;

    let group = |vendor: u32, absorber: &str| {
        format!(
            "INSERT INTO pricing_bundle_revshare_group (bundle_id, plan_revision, \
             vendor_sku_id, tenant_id, platform_cut_bp, residual_absorber_party) \
             VALUES ('{BUNDLE}', 0, '{vendor:0>8}-cccc-cccc-cccc-cccccccccccc', \
             '{TENANT}', 1000, {absorber})"
        )
    };

    for code in STRIPPED_WHITESPACE {
        let refused = try_exec(&conn, &group(*code, &format!("char({code})")))
            .await
            .expect_err(&format!(
                "an absorber of nothing but char({code}) must be refused by the store"
            ));
        assert!(
            refused.contains("chk_pricing_bundle_revshare_group_absorber"),
            "and by the absorber predicate rather than by the platform-cut bound: {refused}"
        );
    }

    for (vendor, forged) in [(90, "' platform '"), (91, "char(9) || 'platform'")] {
        let refused = try_exec(&conn, &group(vendor, forged))
            .await
            .expect_err(&format!(
                "{forged} is neither the default nor a nomination and must be refused"
            ));
        assert!(
            refused.contains("chk_pricing_bundle_revshare_group_absorber"),
            "and by the absorber predicate: {refused}"
        );
    }

    // Both legal inhabitants land: the sentinel exactly, and a named party.
    must_apply(&conn, &group(92, "'platform'")).await;
    must_apply(&conn, &group(93, "'acme'")).await;
}

/// **An overlay line's `target_sku` is absent or names something** —
/// `chk_pricing_price_overlay_line_target_sku_present`.
///
/// `NULL` and a blank string are not the same state and only one of them is a line:
/// the list-default and per-plan lines carry no SKU at all, while `TargetSku::new`
/// trims and `overlay_repo` folds its refusal to `RepoError::CorruptRow` over the
/// revision the row sits in. So the `NULL` arm is asserted as its own control — a
/// tightening that turned an absent SKU into a refusal would break every line
/// `LineKey::list_default` and `LineKey::for_plan` build.
///
/// The plan is named on every row because `chk_..._sku_needs_plan` answers first
/// otherwise, and a mis-arranged fixture here would prove that neighbouring rule
/// twice and leave this one untouched.
#[tokio::test]
async fn the_target_sku_predicate_refuses_a_blank_and_keeps_its_null_arm() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-2222-2222-2222-222222222222";
    const OVERLAY: &str = "66666666-6666-6666-6666-666666666666";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    apply(&manager, &name_ordered_chain()).await;

    // A `draft` overlay revision of this tenant, or the line table's append-only and
    // same-tenant triggers answer ahead of the CHECK.
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_price_overlay (tenant_id, price_overlay_id, revision, \
             lifecycle_state, precedence, scope_class, scope_value, tax_basis) \
             VALUES ('{TENANT}', '{OVERLAY}', 0, 'draft', 10, 'brand', 'acme', \
             'exclusive')"
        ),
    )
    .await;

    let line = |id: u32, sku: &str| {
        format!(
            "INSERT INTO pricing_price_overlay_line (line_id, price_overlay_id, \
             overlay_revision, tenant_id, plan_id, target_sku, cohort, adjustment_kind, \
             magnitude_kind, adjustment_value) \
             VALUES ('{id:0>8}-0000-0000-0000-000000000000', '{OVERLAY}', 0, '{TENANT}', \
             '{PLAN}', {sku}, NULL, 'discount', 'percent_bp', 1500)"
        )
    };

    for code in STRIPPED_WHITESPACE {
        let refused = try_exec(&conn, &line(*code, &format!("char({code})")))
            .await
            .expect_err(&format!(
                "a target SKU of nothing but char({code}) must be refused by the store"
            ));
        assert!(
            refused.contains("chk_pricing_price_overlay_line_target_sku_present"),
            "and by the SKU-present predicate rather than by one of the line's eight \
             other CHECKs: {refused}"
        );
    }

    // The `NULL` arm, which is the whole reason this predicate is a disjunction, and a
    // named SKU beside it.
    must_apply(&conn, &line(98, "NULL")).await;
    must_apply(&conn, &line(99, "' vm-small '")).await;
}

/// **A negative `min_qty` or `max_qty` is refused by the store.**
///
/// Both columns read back as `Option<u32>` through `read_count`, which maps a
/// negative to `RepoError::CorruptRow` and thence to a `500` — one bad row
/// failing the whole revision's add-on set, with direct SQL the only remedy.
/// `chk_pricing_plan_addon_rule_qty_range` bounds only the *relation* between the
/// two columns, so before the two bounds below existed `min_qty = -1` and, on a
/// rule that is not `required`, `max_qty = -1` were both admitted.
///
/// Raw SQL and not the repository, deliberately: the repository renders these
/// columns from the domain's `Option<u32>` and cannot express a negative at all.
/// The writer this guard exists for is the one that never runs the pipeline.
#[tokio::test]
async fn a_negative_add_on_quantity_bound_is_refused() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-2222-2222-2222-222222222222";
    const ACTOR: &str = "44444444-4444-4444-4444-000000000000";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    let remainder = staged_to(&manager, "create_pricing_plan_addon_rule").await;

    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_plan (plan_id, revision, tenant_id, lifecycle_state, \
             created_by, created_at_utc) \
             VALUES ('{PLAN}', 0, '{TENANT}', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;

    let rule = |sku: &str, bounds: &str| {
        format!(
            "INSERT INTO pricing_plan_addon_rule (plan_id, plan_revision, addon_sku_id, \
             tenant_id, required, {bounds}) \
             VALUES ('{PLAN}', 0, '{sku}', '{TENANT}', 0, -1)"
        )
    };

    for (sku, column, constraint) in [
        (
            "55555555-5555-5555-5555-555555555551",
            "min_qty",
            "chk_pricing_plan_addon_rule_min_qty",
        ),
        (
            "55555555-5555-5555-5555-555555555552",
            "max_qty",
            "chk_pricing_plan_addon_rule_max_qty",
        ),
    ] {
        let message = try_exec(&conn, &rule(sku, column))
            .await
            .expect_err(&format!(
                "a negative `{column}` must be refused by the store"
            ));
        assert!(
            message.contains(constraint),
            "and refused by `{constraint}` rather than by some neighbouring guard: {message}"
        );
    }

    // The controls: zero is a bound, and the relation between the two columns is
    // still the one `_qty_range` judges rather than something these two took over.
    must_apply(
        &conn,
        &rule("55555555-5555-5555-5555-555555555553", "min_qty").replace("-1)", "0)"),
    )
    .await;
    let message = try_exec(
        &conn,
        &format!(
            "INSERT INTO pricing_plan_addon_rule (plan_id, plan_revision, addon_sku_id, \
             tenant_id, required, min_qty, max_qty) \
             VALUES ('{PLAN}', 0, '55555555-5555-5555-5555-555555555554', '{TENANT}', 0, 5, 2)"
        ),
    )
    .await
    .expect_err("an inverted pair is still refused");
    assert!(
        message.contains("chk_pricing_plan_addon_rule_qty_range"),
        "the new bounds must not shadow the relation constraint: {message}"
    );

    apply(&manager, &remainder).await;

    assert_eq!(
        count(&conn, "SELECT count(*) AS n FROM pricing_plan_addon_rule").await,
        1,
        "the remainder of the chain must leave the rule it was applied over standing"
    );
    let message = try_exec(
        &conn,
        &rule("55555555-5555-5555-5555-555555555555", "min_qty"),
    )
    .await
    .expect_err("and the bound must still refuse afterwards");
    assert!(
        message.contains("chk_pricing_plan_addon_rule_min_qty"),
        "by the same bound: {message}"
    );
}

/// **A journal row may not name a run belonging to another tenant.**
///
/// `fk_pricing_repricing_journal_run` covers `run_id` alone, and until
/// `trg_pricing_repricing_journal_same_tenant_as_its_run` existed nothing else
/// compared the two tenants — while the sibling table one migration later
/// (`pricing_bulk_row_lock`) documented that exact conjunct as necessary and
/// carried it. The consequence is §6's completion predicate, *"a run is complete
/// when no `pending` rows remain"*, evaluated by a scoped reader over a set that
/// omits the foreign row.
///
/// Defence in depth rather than a live hole, and the positive control is what
/// says so: the same insert under the run's **own** tenant lands, which is the
/// only shape the one production writer can produce.
#[tokio::test]
async fn a_journal_row_may_not_name_another_tenants_run() {
    const TENANT_A: &str = "11111111-1111-1111-1111-111111111111";
    const TENANT_B: &str = "11111111-1111-1111-1111-1111111111b2";
    const PLAN: &str = "22222222-2222-2222-2222-222222222222";
    const PHASE: &str = "33333333-3333-3333-3333-333333333333";
    const ACTOR: &str = "44444444-4444-4444-4444-000000000000";
    const PRICE: &str = "8f8f8f8f-0000-0000-0000-000000000030";
    const RUN: &str = "8f8f8f8f-0000-0000-0000-000000000032";

    let conn = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    let manager = SchemaManager::new(&conn);
    let remainder = staged_to(&manager, "create_pricing_repricing_journal").await;

    // One price row: a second under the same tenant would need a different scope
    // key, and the journal's own key is `(run_id, price_id)`, so both journal rows
    // below can name this one.
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_price (price_id, tenant_id, plan_id, currency, region, \
             phase, charge_kind, model_kind, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{PRICE}', '{TENANT_A}', '{PLAN}', 'USD', 'EU', '{PHASE}', 'usage', \
             'per_unit', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_bulk_operation (operation_id, tenant_id, kind, state, \
             client_key, submitted_by, submitted_at) \
             VALUES ('{RUN}', '{TENANT_A}', 'repricing', 'validating', 'ck-b1-tenancy', \
             '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;

    let message = try_exec(
        &conn,
        &format!(
            "INSERT INTO pricing_repricing_journal (run_id, price_id, tenant_id, state) \
             VALUES ('{RUN}', '{PRICE}', '{TENANT_B}', 'pending')"
        ),
    )
    .await
    .expect_err("a journal row under another tenant's run must be refused by the store");
    assert!(
        message.contains("belongs to another tenant"),
        "and refused by the tenancy arm rather than by the kind arm or the key: {message}"
    );

    // The control, and it is also the shape the only production writer produces:
    // the run and the row are minted from one scope inside one transaction.
    must_apply(
        &conn,
        &format!(
            "INSERT INTO pricing_repricing_journal (run_id, price_id, tenant_id, state) \
             VALUES ('{RUN}', '{PRICE}', '{TENANT_A}', 'pending')"
        ),
    )
    .await;
    // And the arm still defers to the foreign key for a row naming no run at all,
    // which is what keeps that key observable.
    let message = try_exec(
        &conn,
        &format!(
            "INSERT INTO pricing_repricing_journal (run_id, price_id, tenant_id, state) \
             VALUES ('8f8f8f8f-0000-0000-0000-0000000000ff', '{PRICE}', '{TENANT_B}', 'pending')"
        ),
    )
    .await
    .expect_err("a journal row naming no run is still refused");
    assert!(
        message.contains("FOREIGN KEY constraint failed"),
        "by the foreign key, not by a tenancy arm reporting a fault the caller does not have: \
         {message}"
    );

    apply(&manager, &remainder).await;

    assert_eq!(
        count(&conn, "SELECT count(*) AS n FROM pricing_repricing_journal").await,
        1,
        "the remainder of the chain must leave the journal row it was applied over standing"
    );
    let message = try_exec(
        &conn,
        &format!(
            "INSERT INTO pricing_repricing_journal (run_id, price_id, tenant_id, state) \
             VALUES ('{RUN}', '{PRICE}', '{TENANT_B}', 'failed')"
        ),
    )
    .await
    .expect_err("and the tenancy arm must still refuse afterwards");
    assert!(
        message.contains("belongs to another tenant"),
        "by the same arm: {message}"
    );
}
