//! Per-document resolution budgets, shared by admission and dependent refresh.

use std::collections::HashSet;

use gts::{GtsId, GtsStore, ResolvedType};

use super::errors::ItemFailure;
use crate::config::Limits;
use crate::domain::admission::AdmissionFailureReason;
use crate::domain::artifacts::{MaterializedArtifacts, materialize};
use crate::domain::dependency::extract_edges;

/// Count the candidate and the distinct documents it consumes before resolving.
///
/// Walk the authored documents in the overlaid store: committed outgoing edges
/// of a revised candidate may have been removed by this revision. Unrelated
/// documents in a shared refresh store do not consume this candidate's budget.
pub fn check_closure(store: &mut GtsStore, root: &str, bound: usize) -> Result<(), ItemFailure> {
    let mut seen = HashSet::new();
    let mut pending = vec![root.to_owned()];
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if seen.len() > bound {
            return Err(ItemFailure::new(
                AdmissionFailureReason::ResolutionClosureExceeded,
                format!("resolution requires more than {bound} documents; nothing was committed"),
            ));
        }
        let Some(document) = store.get(&id) else {
            // Validation owns missing-reference errors; no reader is installed.
            continue;
        };
        let parsed = GtsId::try_new(&id).map_err(|error| {
            ItemFailure::new(AdmissionFailureReason::InvalidSchema, error.to_string())
        })?;
        let edges = extract_edges(&parsed, &document.content).map_err(|error| {
            ItemFailure::new(AdmissionFailureReason::InvalidSchema, error.to_string())
        })?;
        pending.extend(edges.into_iter().map(|edge| edge.target));
    }
    Ok(())
}

/// Apply the byte limit to each canonical effective document before persistence.
pub fn materialize_bounded(
    resolved: &ResolvedType,
    limits: &Limits,
) -> Result<MaterializedArtifacts, ItemFailure> {
    let artifacts = materialize(resolved);
    let bound = limits.resolved_document.bytes();
    for document in [
        &artifacts.resolved_schema,
        &artifacts.effective_traits,
        &artifacts.effective_traits_schema,
    ] {
        if document.len() > bound {
            return Err(ItemFailure::new(
                AdmissionFailureReason::ResolvedDocumentTooLarge,
                format!("a resolved document exceeds {bound} bytes; nothing was committed"),
            ));
        }
    }
    Ok(artifacts)
}
