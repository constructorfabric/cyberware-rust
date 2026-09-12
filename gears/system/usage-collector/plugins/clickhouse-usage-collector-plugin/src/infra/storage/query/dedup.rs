//! Duplicate-row handling for `ReplacingMergeTree` reads, in place of `FINAL`.
//!
//! Both plugin tables are `ReplacingMergeTree(version)`, so several physical
//! rows can share one sort key until a background merge collapses them. Reads
//! used to append `FINAL` to force that collapse at query time; `FINAL` merges
//! parts on the read path, which dominated query cost. The explicit
//! `ORDER BY version DESC LIMIT 1 BY <key>` form that replaced it still sorts
//! (or hash-groups) every scanned row, which is the dominant CPU cost on the
//! range reads. This module therefore offers two tools:
//!
//! - **Explicit resolution** ([`latest_by`], [`LATEST_ONE`]) for *point* reads
//!   whose candidate set is already tiny — `get`, the deactivation cascade, the
//!   create-path dedup lookups, and the catalog.
//! - **Marker anti-join** ([`active_survivors`], [`resolved_survivors`]) for the
//!   *range* reads — `list` and `aggregate` — which do not resolve versions at
//!   all. They scan raw rows and exclude the ids that carry a deactivation
//!   marker, a set that is tiny or empty and costs one hash probe per row.
//!
//! ## The invariant this rests on
//!
//! Nothing in this plugin issues an `UPDATE`. The only way a second row appears
//! for one sort key is:
//!
//! - a **deactivation marker** — [`make_inactive_marker`] clones the source row
//!   and changes only `status` (→ `inactive`) and `version` (→ higher). It keeps
//!   the source row's `id`, and `status` never transitions back; or
//! - a **duplicate create** — two uncoordinated creates for one dedup key both
//!   write, storing byte-identical payloads under different `version`s.
//!
//! In both cases every column except `status` and `version` is identical across
//! all rows sharing a sort key. Consequences:
//!
//! 1. A predicate over any column **other than `status`/`version`** selects the
//!    same set of ids whether or not a marker exists, so it may be applied to
//!    the raw scan *and* repeated inside the marker subquery, where both prune
//!    granules. [`split_version_invariant`] is how the read paths act on this:
//!    a caller `$filter` is split on its top-level `AND` spine and the invariant
//!    half goes into the scan. This is not a micro-optimisation — `tenant_id`
//!    and `created_at` reach the plugin *only* through `$filter`, and they are
//!    two thirds of the `usage_records` key prefix.
//! 2. On the range reads, a logical row is **inactive iff any physical row with
//!    its `id` has `status = 'inactive'`** (markers keep the `id`; nothing
//!    reactivates). That is what the anti-join checks, so it is exact before any
//!    merge runs and needs no `version` at all. Every surviving row's raw
//!    `status` equals its resolved status, which is why the caller's
//!    `status`-naming `$filter` conjuncts, keyset predicate and `ORDER BY` can
//!    read the raw column in the same `WHERE`.
//! 3. On the point reads that still resolve with [`latest_by`], a `status`
//!    predicate must **not** be applied to the scan *below* the resolution
//!    step: it would retain a superseded active row while discarding its own
//!    marker. The deactivation cascade applies it above the step.
//!
//! **Duplicate creates are not collapsed by the range reads.** They are
//! prevented at the engine instead: every `usage_records` `INSERT` carries an
//! `insert_deduplication_token` (see `record_store::insert_dedup_token`) and the
//! table has a `non_replicated_deduplication_window`, so a racing retry of the
//! same row(s) is dropped before it becomes a part. The residual case — an
//! asynchronous single-record insert whose twin landed in a different flush, or
//! two non-identical batches overlapping — is visible twice to `list` and
//! counted twice by `aggregate` until the background merge collapses it.
//!
//! [`make_inactive_marker`]: crate::infra::storage::mapper::make_inactive_marker

use toolkit_odata::filter::{FilterField, FilterNode, FilterOp};

/// Suffix resolving each sort key to its highest-`version` row: `ORDER BY
/// version DESC LIMIT 1 BY <keys>`.
///
/// `keys` must name the columns identifying one logical row — the table's
/// `ORDER BY` tuple, or the subset of it left unpinned by the query's `WHERE`.
/// Rendered with a leading space so it appends directly to a query body.
///
/// Callers that also need a presentation order put it *outside* this suffix
/// (in an enclosing query), because `ORDER BY version DESC` is what makes
/// `LIMIT 1 BY` pick the resolved row.
#[must_use]
pub fn latest_by(keys: &[&str]) -> String {
    format!(" ORDER BY version DESC LIMIT 1 BY {}", keys.join(", "))
}

/// Suffix resolving a single expected sort key: `ORDER BY version DESC LIMIT 1`.
///
/// Only for reads whose `WHERE` pins the whole sort key (so at most one logical
/// row can match) — `usage_type_catalog` point reads, whose sort key is
/// `(gts_id)` alone. A read that can span sort keys must use [`latest_by`]
/// instead, or it may return a row that is not its own key's resolved version.
pub const LATEST_ONE: &str = " ORDER BY version DESC LIMIT 1";

/// The `usage_records` sort key — the tuple `ReplacingMergeTree` collapses on.
///
/// Kept beside the helpers so a call site cannot drift from
/// `migrations/0001_init.sql`'s `ORDER BY (gts_id, tenant_id, created_at, id)`.
///
/// The *set* of columns is the row identity; their order is not. `LIMIT 1 BY`
/// is order-insensitive, so this constant tracks the migration's column order
/// for legibility rather than for correctness.
pub const RECORD_DEDUP_KEY: &[&str] = &["gts_id", "tenant_id", "created_at", "id"];

/// The ids inside a scan's key range that carry a deactivation marker:
/// `SELECT id FROM usage_records WHERE <scan_where> AND status = 'inactive'`.
///
/// `scan_where` is the enclosing query's own scan predicate, repeated verbatim
/// so the subquery prunes on the same key range (and so the enclosing query
/// binds its scan values twice, in text order). Every conjunct in it must be
/// version-invariant (rule 1 in the module docs); markers clone their source
/// row, so such a conjunct keeps or drops a marker exactly when it keeps or
/// drops the row it marks. A `status`-naming conjunct must not enter here — the
/// subquery carries its own `status = 'inactive'`.
///
/// Rendered without a leading or trailing space; the callers below embed it.
#[must_use]
pub fn inactive_ids_subquery(scan_where: &str) -> String {
    format!("SELECT id FROM usage_records WHERE {scan_where} AND status = 'inactive'")
}

/// Aggregate survivor predicate: the raw rows that *are* the active logical
/// rows — `status = 'active' AND id NOT IN (<inactive ids>)`.
///
/// Raw `status = 'active'` drops the markers themselves; the anti-join drops the
/// source rows the markers superseded. What remains is one physical row per
/// active logical row (plus engine-missed duplicate creates, see the module
/// docs), so the enclosing aggregate needs no resolution step.
///
/// Rendered as a bare conjunction with no wrapping parentheses; callers join
/// it onto their scan predicate with ` AND `.
#[must_use]
pub fn active_survivors(scan_where: &str) -> String {
    format!(
        "status = 'active' AND id NOT IN ({})",
        inactive_ids_subquery(scan_where)
    )
}

/// List survivor predicate: one raw row per logical row, active or not —
/// `(status = 'inactive' OR id NOT IN (<inactive ids>))`.
///
/// A marker survives on its own: its raw `status` *is* the resolved status,
/// and it carries every other column of the row it marks. An active row
/// survives iff no marker exists for its id. Because the surviving row's raw
/// `status` always equals the resolved one, the caller may filter, keyset-page
/// and order on the raw `status` column in the same `WHERE`.
///
/// Rendered inside parentheses because it is a disjunction that callers `AND`
/// onto their scan predicate.
#[must_use]
pub fn resolved_survivors(scan_where: &str) -> String {
    format!(
        "(status = 'inactive' OR id NOT IN ({}))",
        inactive_ids_subquery(scan_where)
    )
}

/// Filter fields whose value depends on which `version` of a sort key is read.
///
/// `status` is the only one: it is the single column a deactivation marker
/// changes (see the module docs), and `version` itself is not
/// `OData`-filterable. A predicate naming anything else selects the same set of
/// sort keys before and after resolution.
const VERSION_DEPENDENT_FIELDS: &[&str] = &["status"];

/// The two halves of a split caller filter: conjuncts that may be applied
/// below the version-resolution step, and conjuncts that must stay above it.
///
/// Borrowed from the caller's `FilterNode`, so each half is translated in
/// place rather than cloned into a new AST.
pub type FilterSplit<'a, F> = (Vec<&'a FilterNode<F>>, Vec<&'a FilterNode<F>>);

/// Split a caller filter into `(version_invariant, version_dependent)`
/// conjuncts, so the invariant half can join the scan predicate — and be
/// repeated inside the marker subquery — where it prunes both, while the
/// `status`-naming half trails the survivor predicate (or, on a point read,
/// sits above the resolution step).
///
/// The split walks the node's top-level `AND` spine only, flattening nested
/// `AND`s, and classifies every other node whole: a subtree naming a
/// [`VERSION_DEPENDENT_FIELDS`] column anywhere is version-dependent in full.
/// That is what makes the split sound for `OR` and `NOT` — `status eq 'active'
/// or tenant_id eq X` is not two conjuncts and cannot be split, so it stays
/// above the resolution step as one unit, while `NOT (…)` of an invariant
/// subtree is itself invariant.
///
/// Re-joining the halves with `AND` across the two query levels preserves the
/// caller's semantics exactly: an outer `WHERE b` over an inner `WHERE a` is
/// `a AND b`, and by rule 1 the inner `a` keeps or drops a sort key's rows as a
/// whole, so it cannot change which row the resolution between them picks.
///
/// Either half may be empty (including both, for a filter that is a bare empty
/// `AND`); a caller pushes no clause for an empty half.
#[must_use]
pub fn split_version_invariant<F: FilterField>(node: &FilterNode<F>) -> FilterSplit<'_, F> {
    let mut invariant = Vec::new();
    let mut dependent = Vec::new();
    collect_conjuncts(node, &mut invariant, &mut dependent);
    (invariant, dependent)
}

/// Flatten `node`'s top-level `AND` spine, sorting each leaf conjunct into the
/// invariant or version-dependent bucket.
fn collect_conjuncts<'a, F: FilterField>(
    node: &'a FilterNode<F>,
    invariant: &mut Vec<&'a FilterNode<F>>,
    dependent: &mut Vec<&'a FilterNode<F>>,
) {
    if let FilterNode::Composite {
        op: FilterOp::And,
        children,
    } = node
    {
        for child in children {
            collect_conjuncts(child, invariant, dependent);
        }
    } else if references_version_dependent(node) {
        dependent.push(node);
    } else {
        invariant.push(node);
    }
}

/// Whether `node` names a [`VERSION_DEPENDENT_FIELDS`] column anywhere in its
/// subtree.
fn references_version_dependent<F: FilterField>(node: &FilterNode<F>) -> bool {
    match node {
        FilterNode::Binary { field, .. } | FilterNode::InList { field, .. } => {
            VERSION_DEPENDENT_FIELDS.contains(&field.name())
        }
        FilterNode::Composite { children, .. } => children.iter().any(references_version_dependent),
        FilterNode::Not(inner) => references_version_dependent(inner),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "dedup_tests.rs"]
mod dedup_tests;
