use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::repo::{SyncWatermarkRecord, SyncWatermarkRepository};

pub const SWEEP_OVERLAP: Duration = Duration::minutes(5);

pub mod sweep_families {
    pub const ISSUES: &str = "issues";
    pub const PULL_REQUESTS: &str = "pull_requests";
    pub const COMMITS: &str = "commits";
}

#[must_use]
fn stop_threshold(stored: Option<&SyncWatermarkRecord>, force: bool) -> Option<DateTime<Utc>> {
    if force {
        return None;
    }
    stored
        .and_then(|w| w.last_seen_updated_at.as_deref())
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc) - SWEEP_OVERLAP)
}

#[must_use]
pub fn is_stale(updated_at: Option<&str>, threshold: Option<DateTime<Utc>>) -> bool {
    let (Some(updated_at), Some(threshold)) = (updated_at, threshold) else {
        return false;
    };
    DateTime::parse_from_rfc3339(updated_at).is_ok_and(|at| at.with_timezone(&Utc) < threshold)
}

#[must_use]
pub fn high_water(seen: &[&str], threshold: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    seen.iter()
        .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .max()
        .max(threshold)
}

/// What a sweep needs before it walks: the instant below which an entity is
/// too old to be worth looking at, and the validator page one carried last
/// time so an unchanged listing can stop before page two.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepStart {
    pub updated_after: Option<DateTime<Utc>>,
    pub page1_etag: Option<String>,
}

pub struct SweepWatermark {
    watermark_store: Arc<dyn SyncWatermarkRepository>,
}

impl SweepWatermark {
    #[must_use]
    pub fn new(watermark_store: Arc<dyn SyncWatermarkRepository>) -> Self {
        Self { watermark_store }
    }

    /// # Errors
    /// `Database`/`Internal` when the watermark row cannot be read.
    pub async fn start_sweep(
        &self,
        scope: &AccessScope,
        repo_id: i64,
        family: &str,
        force: bool,
    ) -> Result<SweepStart, DomainError> {
        let stored = self.watermark_store.find(scope, repo_id, family).await?;
        Ok(SweepStart {
            updated_after: stop_threshold(stored.as_ref(), force),
            page1_etag: if force {
                None
            } else {
                stored.and_then(|w| w.page1_etag)
            },
        })
    }

    /// # Errors
    /// `Database`/`Internal` when the watermark row cannot be read or written.
    pub async fn stage(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        repo_id: i64,
        family: &str,
        candidate: Option<DateTime<Utc>>,
    ) -> Result<(), DomainError> {
        let stored = self.watermark_store.find(scope, repo_id, family).await?;
        let candidate = candidate.map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        self.watermark_store
            .upsert(
                scope,
                tenant_id,
                SyncWatermarkRecord {
                    repo_id,
                    family: family.to_owned(),
                    last_seen_updated_at: stored
                        .as_ref()
                        .and_then(|w| w.last_seen_updated_at.clone()),
                    page1_etag: stored.and_then(|w| w.page1_etag),
                    sweep_in_progress: true,
                    candidate_high_water: candidate,
                },
            )
            .await?;
        Ok(())
    }

    /// Move the staged candidate into `last_seen_updated_at` and record page
    /// one's validator. Both happen only here, at the family-complete point
    /// (ALGORITHMS §6.4): a run that stopped before its refinements finished
    /// leaves neither behind, so the next run walks the listing in full and
    /// the gate re-seeds whatever was left `pending`.
    ///
    /// # Errors
    /// `Database`/`Internal` when the watermark row cannot be read or written.
    pub async fn promote(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        repo_id: i64,
        family: &str,
        page1_etag: Option<String>,
    ) -> Result<(), DomainError> {
        let Some(stored) = self.watermark_store.find(scope, repo_id, family).await? else {
            return Ok(());
        };
        let Some(candidate) = stored.candidate_high_water.clone() else {
            return Ok(());
        };
        self.watermark_store
            .upsert(
                scope,
                tenant_id,
                SyncWatermarkRecord {
                    last_seen_updated_at: Some(candidate),
                    page1_etag: page1_etag.or_else(|| stored.page1_etag.clone()),
                    sweep_in_progress: false,
                    candidate_high_water: None,
                    ..stored
                },
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "sweep_watermark_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod sweep_watermark_tests;
