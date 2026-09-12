use toolkit_odata::filter::{FilterField, FilterNode, FilterOp, ODataValue};
use usage_collector_sdk::UsageRecordFilterField;

use super::{
    LATEST_ONE, RECORD_DEDUP_KEY, active_survivors, inactive_ids_subquery, latest_by,
    resolved_survivors, split_version_invariant,
};

// ── marker anti-join ─────────────────────────────────────────────────────────

const SCAN: &str = "gts_id = ? AND tenant_id = ?";

/// The subquery is the scan predicate plus the raw marker filter — nothing
/// else, so it prunes exactly the enclosing query's key range and reads only
/// `id` and `status`.
#[test]
fn inactive_ids_subquery_embeds_the_scan_and_filters_raw_inactive() {
    assert_eq!(
        inactive_ids_subquery(SCAN),
        "SELECT id FROM usage_records WHERE gts_id = ? AND tenant_id = ? AND status = 'inactive'"
    );
}

/// Aggregate survivors: raw active rows whose id has no marker. Both halves are
/// load-bearing — dropping `status = 'active'` would keep the markers, dropping
/// the anti-join would keep the rows they superseded.
#[test]
fn active_survivors_requires_raw_active_and_anti_joins_markers() {
    let pred = active_survivors(SCAN);
    assert_eq!(
        pred,
        format!(
            "status = 'active' AND id NOT IN ({})",
            inactive_ids_subquery(SCAN)
        )
    );
    assert!(
        !pred.starts_with('('),
        "callers AND it on; a bare conjunction"
    );
}

/// List survivors: a marker stands in for the row it supersedes (its raw
/// `status` is the resolved one), an unmarked active row stands for itself.
/// The disjunction is parenthesised because callers `AND` it onto the scan.
#[test]
fn resolved_survivors_keeps_markers_and_unmarked_actives() {
    let pred = resolved_survivors(SCAN);
    assert_eq!(
        pred,
        format!(
            "(status = 'inactive' OR id NOT IN ({}))",
            inactive_ids_subquery(SCAN)
        )
    );
    assert!(pred.starts_with('(') && pred.ends_with(')'));
}

/// The helpers add no `?` of their own: every placeholder in the rendered text
/// is a copy of the scan's, which is what lets a caller bind the scan values
/// twice and nothing else.
#[test]
fn survivor_helpers_add_no_placeholders_of_their_own() {
    let scan_placeholders = SCAN.matches('?').count();
    assert_eq!(
        inactive_ids_subquery(SCAN).matches('?').count(),
        scan_placeholders
    );
    assert_eq!(
        active_survivors(SCAN).matches('?').count(),
        scan_placeholders
    );
    assert_eq!(
        resolved_survivors(SCAN).matches('?').count(),
        scan_placeholders
    );
    assert!(!active_survivors(SCAN).contains("version"));
    assert!(!resolved_survivors(SCAN).contains("version"));
}

// ── latest_by ────────────────────────────────────────────────────────────────

#[test]
fn latest_by_renders_single_key() {
    assert_eq!(
        latest_by(&["gts_id"]),
        " ORDER BY version DESC LIMIT 1 BY gts_id"
    );
}

#[test]
fn latest_by_renders_full_record_key() {
    assert_eq!(
        latest_by(RECORD_DEDUP_KEY),
        " ORDER BY version DESC LIMIT 1 BY gts_id, tenant_id, created_at, id"
    );
}

#[test]
fn latest_by_starts_with_space_so_it_appends_to_a_query_body() {
    // Call sites concatenate this straight onto `… WHERE x = ?` with no
    // separator of their own.
    assert!(latest_by(&["id"]).starts_with(' '));
    assert!(LATEST_ONE.starts_with(' '));
}

#[test]
fn latest_by_orders_version_descending() {
    // The DESC is the whole point: `LIMIT 1 BY` takes the first row per key in
    // the ordered stream, so ASC would resolve to the *superseded* row.
    assert!(latest_by(&["id"]).contains("version DESC"));
    assert!(LATEST_ONE.contains("version DESC"));
}

// ── RECORD_DEDUP_KEY ─────────────────────────────────────────────────────────

#[test]
fn record_dedup_key_matches_the_migration_order_by() {
    // Must stay in lock-step with `ORDER BY (gts_id, tenant_id, created_at, id)`
    // in migrations/0001_init.sql — that tuple is what the engine collapses on,
    // so a column added or dropped here silently resolves the wrong row. The
    // column *order* is cosmetic (`LIMIT 1 BY` is order-insensitive), but it is
    // pinned so the emitted SQL stays readable against the DDL.
    assert_eq!(
        RECORD_DEDUP_KEY,
        &["gts_id", "tenant_id", "created_at", "id"]
    );
}

// ── split_version_invariant ──────────────────────────────────────────────────

/// A record filter field by name.
fn field(name: &str) -> UsageRecordFilterField {
    UsageRecordFilterField::from_name(name).expect("field must be on the record filter schema")
}

/// `<name> eq '<anything>'` — the value is irrelevant to the split, which
/// classifies on field names alone.
fn eq(name: &str) -> FilterNode<UsageRecordFilterField> {
    FilterNode::binary(
        field(name),
        FilterOp::Eq,
        ODataValue::String("x".to_owned()),
    )
}

/// The field each classified conjunct names, or a marker for a subtree that was
/// classified whole.
fn names(nodes: &[&FilterNode<UsageRecordFilterField>]) -> Vec<&'static str> {
    nodes
        .iter()
        .map(|node| match node {
            FilterNode::Binary { field, .. } | FilterNode::InList { field, .. } => field.name(),
            FilterNode::Composite { .. } => "<composite>",
            FilterNode::Not(_) => "<not>",
        })
        .collect()
}

/// The shape the gateway actually sends: the `[from, to)` window is an ordinary
/// `created_at` conjunct pair inside `$filter`, alongside `tenant_id`. All three
/// must reach the scan — they are the `usage_records` key prefix — while
/// `status` stays above the resolution step.
#[test]
fn gateway_window_filter_pushes_tenant_and_time_down_and_keeps_status_up() {
    let node = FilterNode::and(vec![
        eq("tenant_id"),
        FilterNode::binary(
            field("created_at"),
            FilterOp::Ge,
            ODataValue::String("2026-01-01T00:00:00Z".to_owned()),
        ),
        FilterNode::binary(
            field("created_at"),
            FilterOp::Lt,
            ODataValue::String("2026-02-01T00:00:00Z".to_owned()),
        ),
        eq("status"),
    ]);

    let (invariant, dependent) = split_version_invariant(&node);

    assert_eq!(
        names(&invariant),
        vec!["tenant_id", "created_at", "created_at"]
    );
    assert_eq!(names(&dependent), vec!["status"]);
}

#[test]
fn nested_and_spine_is_flattened() {
    let node = FilterNode::and(vec![
        eq("tenant_id"),
        FilterNode::and(vec![eq("resource_id"), FilterNode::and(vec![eq("status")])]),
    ]);

    let (invariant, dependent) = split_version_invariant(&node);

    assert_eq!(names(&invariant), vec!["tenant_id", "resource_id"]);
    assert_eq!(names(&dependent), vec!["status"]);
}

/// An `OR` is not a conjunction, so one naming `status` cannot be split: it
/// stays above the resolution step whole. Pushing only its `tenant_id` arm down
/// would drop rows the caller asked for.
#[test]
fn or_naming_status_stays_version_dependent_whole() {
    let node = FilterNode::and(vec![
        eq("tenant_id"),
        FilterNode::or(vec![eq("status"), eq("resource_id")]),
    ]);

    let (invariant, dependent) = split_version_invariant(&node);

    assert_eq!(names(&invariant), vec!["tenant_id"]);
    assert_eq!(names(&dependent), vec!["<composite>"]);
}

/// An `OR` over version-invariant fields only is itself version-invariant, so
/// the whole subtree prunes the scan.
#[test]
fn or_without_status_is_pushed_down_whole() {
    let node = FilterNode::or(vec![eq("tenant_id"), eq("resource_id")]);

    let (invariant, dependent) = split_version_invariant(&node);

    assert_eq!(names(&invariant), vec!["<composite>"]);
    assert!(dependent.is_empty());
}

/// `NOT` inherits its subtree's classification: negating a version-invariant
/// predicate is still version-invariant, negating `status` is not.
#[test]
fn not_follows_its_subtree() {
    let not_invariant = FilterNode::not(eq("tenant_id"));
    let (invariant, dependent) = split_version_invariant(&not_invariant);
    assert_eq!(names(&invariant), vec!["<not>"]);
    assert!(dependent.is_empty());

    let not_status = FilterNode::not(eq("status"));
    let (invariant, dependent) = split_version_invariant(&not_status);
    assert!(invariant.is_empty());
    assert_eq!(names(&dependent), vec!["<not>"]);
}

/// `status in (…)` is version-dependent for the same reason `status eq …` is —
/// the classification is on the field, not the operator.
#[test]
fn in_list_on_status_stays_version_dependent() {
    let node = FilterNode::InList {
        field: field("status"),
        values: vec![
            ODataValue::String("active".to_owned()),
            ODataValue::String("inactive".to_owned()),
        ],
    };

    let (invariant, dependent) = split_version_invariant(&node);

    assert!(invariant.is_empty());
    assert_eq!(names(&dependent), vec!["status"]);
}

/// An empty `AND` spine yields no conjuncts at all, so a caller emits no clause
/// for either level. The pre-split code translated it to a bare `()`, which
/// `ClickHouse` rejects as a syntax error.
#[test]
fn empty_and_yields_no_conjuncts() {
    let node: FilterNode<UsageRecordFilterField> = FilterNode::and(Vec::new());

    let (invariant, dependent) = split_version_invariant(&node);

    assert!(invariant.is_empty());
    assert!(dependent.is_empty());
}
