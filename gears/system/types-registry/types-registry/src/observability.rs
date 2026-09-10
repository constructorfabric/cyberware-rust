//! Admission tracing spans.

use tracing::{Span, field};
use uuid::Uuid;

use crate::domain::enums::OperationKind;

/// The label an operation's kind carries.
const fn kind_label(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Registration => "registration",
        OperationKind::Deletion => "deletion",
    }
}

/// The span covering one admission pass over one operation.
#[must_use]
pub fn operation_span(operation_id: Uuid) -> Span {
    tracing::info_span!(
        "types_registry.admission.operation",
        %operation_id,
        kind = field::Empty,
        dry_run = field::Empty,
    )
}

/// Fill in the two fields [`operation_span`] left empty.
pub fn record_operation_facts(span: &Span, kind: OperationKind, dry_run: bool) {
    span.record("kind", kind_label(kind));
    span.record("dry_run", dry_run);
}

/// The span covering one admission unit — one candidate, one operation item.
#[must_use]
pub fn unit_span(
    operation_id: Uuid,
    gts_id: &str,
    kind: OperationKind,
    dry_run: bool,
    operation_item_id: i64,
) -> Span {
    tracing::info_span!(
        "types_registry.admission.unit",
        %operation_id,
        gts_id,
        kind = kind_label(kind),
        dry_run,
        operation_item_id,
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "observability_tests.rs"]
mod observability_tests;
