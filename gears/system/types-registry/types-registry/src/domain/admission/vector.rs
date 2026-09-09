//! Revision-vector derivation and commit-time drift detection (SPEC §8.1, D4).

use std::collections::HashMap;

use toolkit_db::DbTx;
use toolkit_db::secure::AccessScope;

use super::errors::{ItemFailure, WorkerError};
use crate::domain::admission::AdmissionFailureReason;
use crate::domain::enums::{EntityKind, LifecycleStatus};
use crate::domain::ports::{EntityRow, ReverseImpact, Stores};

use toolkit_macros::domain_model;

pub use super::drift::{VectorDrift, VectorRole};

/// One entity as of the read that recorded it.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorEntry {
    pub gts_id: String,
    pub role: VectorRole,
    pub resource_version: i64,
    /// `Some` exactly where effective content was consumed — a live Type Schema dependent.
    pub resolution_fingerprint: Option<Vec<u8>>,
}

/// The full vector, plus the roots it was derived from.
#[domain_model]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RevisionVector {
    /// Candidate and reference roots used during evaluation.
    pub roots: Vec<String>,
    /// One entry per dependency and dependent, `(gts_id, role)`-sorted.
    entries: Vec<VectorEntry>,
}

impl RevisionVector {
    /// Sort collected entries into canonical `(gts_id, role)` order.
    #[must_use]
    pub fn new(roots: Vec<String>, mut entries: Vec<VectorEntry>) -> Self {
        entries.sort_by(|a, b| key(a).cmp(&key(b)));
        Self { roots, entries }
    }

    /// One entry per dependency and dependent, `(gts_id, role)`-sorted.
    #[must_use]
    pub fn entries(&self) -> &[VectorEntry] {
        &self.entries
    }

    /// The drift between this vector and a freshly-derived one, or `None` when the two agree.
    #[must_use]
    pub fn drift(&self, fresh: &Self) -> Option<VectorDrift> {
        let mut recorded = self.entries.iter();
        let mut found = fresh.entries.iter();
        let mut left = recorded.next();
        let mut right = found.next();
        loop {
            match (left, right) {
                (None, None) => return None,
                (Some(a), None) => {
                    return Some(VectorDrift::Vanished {
                        gts_id: a.gts_id.clone(),
                        role: a.role,
                    });
                }
                (None, Some(b)) => {
                    return Some(VectorDrift::Appeared {
                        gts_id: b.gts_id.clone(),
                        role: b.role,
                    });
                }
                (Some(a), Some(b)) => match key(a).cmp(&key(b)) {
                    std::cmp::Ordering::Less => {
                        return Some(VectorDrift::Vanished {
                            gts_id: a.gts_id.clone(),
                            role: a.role,
                        });
                    }
                    std::cmp::Ordering::Greater => {
                        return Some(VectorDrift::Appeared {
                            gts_id: b.gts_id.clone(),
                            role: b.role,
                        });
                    }
                    std::cmp::Ordering::Equal => {
                        if let Some(drift) = moved(a, b) {
                            return Some(drift);
                        }
                        left = recorded.next();
                        right = found.next();
                    }
                },
            }
        }
    }
}

/// The sort and merge key.
fn key(entry: &VectorEntry) -> (&str, VectorRole) {
    (entry.gts_id.as_str(), entry.role)
}

/// The two column comparisons for one entity present on both sides.
fn moved(recorded: &VectorEntry, found: &VectorEntry) -> Option<VectorDrift> {
    if recorded.resource_version != found.resource_version {
        return Some(VectorDrift::Moved {
            gts_id: found.gts_id.clone(),
            role: found.role,
            recorded: recorded.resource_version,
            found: found.resource_version,
        });
    }
    if recorded.resolution_fingerprint != found.resolution_fingerprint {
        return Some(VectorDrift::Refreshed {
            gts_id: found.gts_id.clone(),
        });
    }
    None
}

/// Derive a candidate's dependency and reverse-impact vector.
pub async fn derive(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    candidate_gts_id: &str,
    roots: &[String],
    write_set_bound: usize,
) -> Result<Result<RevisionVector, ItemFailure>, WorkerError> {
    let closure = stores.closure(tx, scope, roots).await?;
    derive_from(
        stores,
        tx,
        scope,
        candidate_gts_id,
        roots,
        &closure.entities,
        write_set_bound,
    )
    .await
}

/// [`derive`] over a dependency closure the caller has already read.
#[allow(clippy::too_many_arguments)]
pub async fn derive_from(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    candidate_gts_id: &str,
    roots: &[String],
    closure: &[EntityRow],
    write_set_bound: usize,
) -> Result<Result<RevisionVector, ItemFailure>, WorkerError> {
    let mut entries: Vec<VectorEntry> = Vec::with_capacity(closure.len());
    let mut candidate_id: Option<i64> = None;
    for row in closure {
        if row.gts_id == candidate_gts_id {
            candidate_id = Some(row.id);
            continue;
        }
        entries.push(VectorEntry {
            gts_id: row.gts_id.clone(),
            role: VectorRole::Dependency,
            resource_version: row.resource_version,
            resolution_fingerprint: None,
        });
    }

    // A creation has no row that existing entities can depend on.
    let dependent_roots: Vec<i64> = candidate_id.into_iter().collect();
    let dependents = match stores
        .reverse_impact(tx, scope, &dependent_roots, write_set_bound)
        .await?
    {
        ReverseImpact::Within(rows) => rows,
        ReverseImpact::OverBound { at_least, bound } => {
            return Ok(Err(ItemFailure::new(
                AdmissionFailureReason::ActivationWriteSetExceeded,
                format!(
                    "this revision reaches at least {at_least} dependents, over the \
                     configured activation write set bound of {bound}; the bound is on the \
                     set a revision reaches, of which what it rewrites is a subset; nothing \
                     was committed"
                ),
            )));
        }
    };

    // Batch artifact reads inside the commit transaction.
    let artifact_bearing: Vec<i64> = dependents
        .iter()
        .filter(|row| {
            row.entity_kind == EntityKind::TypeSchema
                && row.lifecycle_status != LifecycleStatus::Deleted
        })
        .map(|row| row.id)
        .collect();
    let mut fingerprints: HashMap<i64, Vec<u8>> = stores
        .current_schemas(tx, scope, &artifact_bearing)
        .await?
        .into_iter()
        .map(|row| (row.entity_id, row.resolution_fingerprint))
        .collect();

    for row in &dependents {
        entries.push(VectorEntry {
            gts_id: row.gts_id.clone(),
            role: VectorRole::Dependent,
            resource_version: row.resource_version,
            // Instances and tombstones have no refreshable artifacts.
            resolution_fingerprint: fingerprints.remove(&row.id),
        });
    }

    Ok(Ok(RevisionVector::new(roots.to_vec(), entries)))
}

/// Step 4.3: re-derive the vector and refuse any drift.
pub async fn guard(
    stores: &dyn Stores,
    tx: &DbTx<'_>,
    scope: &AccessScope,
    candidate_gts_id: &str,
    recorded: &RevisionVector,
    write_set_bound: usize,
) -> Result<Result<(), ItemFailure>, WorkerError> {
    let fresh = match derive(
        stores,
        tx,
        scope,
        candidate_gts_id,
        &recorded.roots,
        write_set_bound,
    )
    .await?
    {
        Ok(fresh) => fresh,
        Err(failure) => return Ok(Err(failure)),
    };
    match recorded.drift(&fresh) {
        None => Ok(Ok(())),
        Some(drift) => Err(WorkerError::RevalidationRequired(drift)),
    }
}

#[cfg(test)]
#[path = "vector_tests.rs"]
mod vector_tests;
