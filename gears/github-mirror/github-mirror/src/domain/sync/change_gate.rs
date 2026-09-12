use std::sync::Arc;

use aws_lc_rs::digest::{self, SHA256};
use chrono::{DateTime, Duration, Utc};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::repo::{EntityFingerprintRecord, EntityFingerprintRepository};

pub const REFINEMENT_PENDING: &str = "pending";
pub const REFINEMENT_COMPLETE: &str = "complete";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReason {
    New,
    FingerprintChanged,
    ChildCountsChanged,
    Incomplete,
    TtlExpired,
    Forced,
}

impl GateReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::FingerprintChanged => "fingerprint_changed",
            Self::ChildCountsChanged => "child_counts_changed",
            Self::Incomplete => "incomplete",
            Self::TtlExpired => "ttl_expired",
            Self::Forced => "forced",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GateInputs {
    pub fingerprint: String,
    pub child_counts_hash: Option<String>,
    pub updated_at: Option<String>,
    pub node_id: Option<String>,
    pub terminal: bool,
}

#[must_use]
pub fn fingerprint(mut fields: Vec<(&str, String)>) -> String {
    fields.sort_unstable_by_key(|(key, _)| *key);
    let canonical = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(";");
    hash(&canonical)
}

#[must_use]
pub fn child_counts_hash(counts: &[(&str, Option<i64>)]) -> Option<String> {
    let mut present: Vec<(&str, i64)> = counts
        .iter()
        .filter_map(|(name, count)| count.map(|c| (*name, c)))
        .collect();
    if present.is_empty() {
        return None;
    }
    present.sort_unstable_by_key(|(name, _)| *name);
    let canonical = present
        .iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect::<Vec<_>>()
        .join(";");
    Some(hash(&canonical))
}

fn hash(canonical: &str) -> String {
    hex::encode(digest::digest(&SHA256, canonical.as_bytes()).as_ref())
}

#[must_use]
pub fn family_ttl(family: &str, terminal: bool) -> Option<Duration> {
    match family {
        entities::COMMIT => (!terminal).then(|| Duration::hours(1)),
        entities::PULL_REQUEST => Some(if terminal {
            Duration::days(7)
        } else {
            Duration::hours(2)
        }),
        entities::ISSUE => Some(if terminal {
            Duration::days(7)
        } else {
            Duration::hours(4)
        }),
        _ => Some(Duration::days(1)),
    }
}

pub mod entities {
    pub const ISSUE: &str = "issue";
    pub const PULL_REQUEST: &str = "pull_request";
    pub const COMMIT: &str = "commit";
    pub const WORKFLOW_RUN: &str = "workflow_run";
}

pub struct ChangeGate {
    fingerprints: Arc<dyn EntityFingerprintRepository>,
}

impl ChangeGate {
    #[must_use]
    pub fn new(fingerprints: Arc<dyn EntityFingerprintRepository>) -> Self {
        Self { fingerprints }
    }

    /// # Errors
    /// `Database`/`Internal` when the fingerprint row cannot be read or
    /// written.
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        repo_id: i64,
        family: &str,
        entity_id: &str,
        inputs: &GateInputs,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<Option<GateReason>, DomainError> {
        let stored = self
            .fingerprints
            .find(scope, repo_id, family, entity_id)
            .await?;
        let reason = evaluate_refinement_gate(stored.as_ref(), inputs, family, now, force);

        let refinement_status = if reason.is_some() {
            REFINEMENT_PENDING.to_owned()
        } else {
            stored.as_ref().map_or_else(
                || REFINEMENT_PENDING.to_owned(),
                |s| s.refinement_status.clone(),
            )
        };
        let child_counts_hash = inputs
            .child_counts_hash
            .clone()
            .or_else(|| stored.as_ref().and_then(|s| s.child_counts_hash.clone()));
        self.fingerprints
            .upsert(
                scope,
                tenant_id,
                EntityFingerprintRecord {
                    repo_id,
                    family: family.to_owned(),
                    entity_id: entity_id.to_owned(),
                    fingerprint: inputs.fingerprint.clone(),
                    updated_at: inputs.updated_at.clone(),
                    node_id: inputs.node_id.clone(),
                    child_counts_hash,
                    last_refined_at: stored.and_then(|s| s.last_refined_at),
                    refinement_status,
                },
            )
            .await?;
        Ok(reason)
    }

    /// # Errors
    /// `Database`/`Internal` when the fingerprint row cannot be read or
    /// written.
    pub async fn mark_refined(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        repo_id: i64,
        family: &str,
        entity_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let Some(stored) = self
            .fingerprints
            .find(scope, repo_id, family, entity_id)
            .await?
        else {
            return Ok(());
        };
        self.fingerprints
            .upsert(
                scope,
                tenant_id,
                EntityFingerprintRecord {
                    last_refined_at: Some(now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                    refinement_status: REFINEMENT_COMPLETE.to_owned(),
                    ..stored
                },
            )
            .await?;
        Ok(())
    }
}

fn evaluate_refinement_gate(
    stored: Option<&EntityFingerprintRecord>,
    inputs: &GateInputs,
    family: &str,
    now: DateTime<Utc>,
    force: bool,
) -> Option<GateReason> {
    if force {
        return Some(GateReason::Forced);
    }
    let Some(stored) = stored else {
        return Some(GateReason::New);
    };
    if stored.fingerprint != inputs.fingerprint {
        return Some(GateReason::FingerprintChanged);
    }
    if inputs.child_counts_hash.is_some() && stored.child_counts_hash != inputs.child_counts_hash {
        return Some(GateReason::ChildCountsChanged);
    }
    if stored.refinement_status != REFINEMENT_COMPLETE {
        return Some(GateReason::Incomplete);
    }
    let ttl = family_ttl(family, inputs.terminal)?;
    let fresh = stored
        .last_refined_at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .is_some_and(|last| now - last.with_timezone(&Utc) <= ttl);
    (!fresh).then_some(GateReason::TtlExpired)
}

#[cfg(test)]
#[path = "change_gate_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod change_gate_tests;
