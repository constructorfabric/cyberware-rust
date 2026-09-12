//! Task domain types for the in-memory scheduler.
//!
//! Ported from the reference implementation's `scheduler/src/task.rs`. Each
//! [`ExtractionTask`] is one ephemeral unit of work, living only for the
//! duration of a single sync session. The five phases proceed in a fixed
//! order; [`TaskPhase`] encodes both the phase identity and its scheduling
//! weight so that Discovery/Indexing tasks are always claimed before
//! Refinement/Verification ones.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The sync phase that a task belongs to (DESIGN §4, 5-phase pipeline).
///
/// `priority_weight` returns a lower value for earlier phases so ordering by
/// weight ascending always yields Discovery before Refinement; `EnumIter`
/// walks the variants in that same declaration order.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    /// Phase 1 — repository discovery and base metadata fetch.
    Discovery,
    /// Phase 2 — list-page indexing; seeds the Refinement tasks.
    Indexing,
    /// Phase 3 — change detection (fingerprint gates).
    ChangeDetection,
    /// Phase 4 — per-entity detail refinement.
    Refinement,
    /// Phase 5 — completeness verification and repair.
    Verification,
}

impl TaskPhase {
    /// Numeric weight defining claim order: lower is claimed first.
    #[must_use]
    pub const fn priority_weight(self) -> u8 {
        match self {
            Self::Discovery => 1,
            Self::Indexing => 2,
            Self::ChangeDetection => 3,
            Self::Refinement => 4,
            Self::Verification => 5,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Indexing => "indexing",
            Self::ChangeDetection => "change_detection",
            Self::Refinement => "refinement",
            Self::Verification => "verification",
        }
    }
}

impl std::fmt::Display for TaskPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting to be claimed by a worker.
    Pending,
    /// Claimed by a worker and currently executing.
    Running,
    /// Completed successfully.
    Done,
    /// Permanently failed.
    Failed,
}

impl TaskStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Scheduling priority. Higher numeric value = claimed sooner.
///
/// Within a phase, tasks are ordered by `priority DESC, created_at ASC`. The
/// tiers encode `cpt-cf-github-mirror-fr-sync-order`: open PRs first, then
/// open issues, global metrics, closed PRs, and closed issues last. The phase
/// weight always dominates, so every Indexing task still precedes any
/// Refinement task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskPriority(pub i32);

impl TaskPriority {
    /// Default priority for ordinary tasks.
    pub const NORMAL: Self = Self(0);
    /// Priority for repair tasks re-enqueued by the verification phase.
    pub const HIGH: Self = Self(100);

    /// Open pull requests and their per-entity refinement (group 1).
    pub const OPEN_PR: Self = Self(600);
    /// Open issues and their per-entity refinement (group 2).
    pub const OPEN_ISSUE: Self = Self(500);
    /// Repo-global, list-only families (group 3).
    pub const GLOBAL: Self = Self(400);
    /// Closed pull requests and their per-entity refinement (group 4).
    pub const CLOSED_PR: Self = Self(300);
    /// Closed issues and their per-entity refinement (group 5).
    pub const CLOSED_ISSUE: Self = Self(200);

    /// Whether this is one of the two "open" tiers — how a refinement task
    /// learns the state of the entity it was seeded for.
    #[must_use]
    pub const fn is_open_tier(self) -> bool {
        matches!(self, Self::OPEN_PR | Self::OPEN_ISSUE)
    }
}

impl Default for TaskPriority {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// What [`crate::domain::sync::TaskQueue::enqueue_task`] inserts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    /// The sync session this task belongs to.
    pub session_id: Uuid,
    /// The tenant the session runs for.
    pub tenant_id: Uuid,
    pub phase: TaskPhase,
    /// Entity category (e.g. `"issues"` for a listing, `"issue"` for one).
    pub entity_type: String,
    /// Identifier for single-entity refinement tasks: an issue or pull number,
    /// a commit SHA, a workflow-run id.
    pub entity_id: Option<String>,
    pub priority: TaskPriority,
    /// Which repair pass this is; 0 for the original task.
    pub attempt: u32,
}

/// An ephemeral unit of work, scoped to a single in-memory sync run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionTask {
    pub id: Uuid,
    pub session_id: Uuid,
    pub tenant_id: Uuid,
    pub phase: TaskPhase,
    pub entity_type: String,
    pub entity_id: Option<String>,
    pub priority: TaskPriority,
    pub attempt: u32,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    PullRequest,
    Issue,
    Generic,
}

impl Lane {
    pub const ALL: [Self; 3] = [Self::PullRequest, Self::Issue, Self::Generic];

    #[must_use]
    pub fn of_entity_type(entity_type: &str) -> Self {
        match entity_type {
            "pull_requests" | "pull_request" => Self::PullRequest,
            "issues" | "issue" => Self::Issue,
            _ => Self::Generic,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PullRequest => "pull requests",
            Self::Issue => "issues",
            Self::Generic => "generic",
        }
    }
}

impl std::fmt::Display for Lane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;

    #[test]
    fn phase_weights_ascend_in_declaration_order() {
        let weights: Vec<u8> = TaskPhase::iter().map(TaskPhase::priority_weight).collect();
        let mut sorted = weights.clone();
        sorted.sort_unstable();
        assert_eq!(weights, sorted);
        assert!(TaskPhase::Discovery < TaskPhase::Refinement);
    }

    #[test]
    fn extraction_order_tiers_descend() {
        assert!(TaskPriority::OPEN_PR > TaskPriority::OPEN_ISSUE);
        assert!(TaskPriority::OPEN_ISSUE > TaskPriority::GLOBAL);
        assert!(TaskPriority::GLOBAL > TaskPriority::CLOSED_PR);
        assert!(TaskPriority::CLOSED_PR > TaskPriority::CLOSED_ISSUE);
        assert!(TaskPriority::CLOSED_ISSUE > TaskPriority::NORMAL);
        assert!(TaskPriority::HIGH > TaskPriority::NORMAL);
    }

    #[test]
    fn open_tiers_are_recognised() {
        assert!(TaskPriority::OPEN_PR.is_open_tier());
        assert!(TaskPriority::OPEN_ISSUE.is_open_tier());
        assert!(!TaskPriority::CLOSED_PR.is_open_tier());
        assert!(!TaskPriority::GLOBAL.is_open_tier());
    }

    #[test]
    fn display_uses_snake_case_names() {
        assert_eq!(TaskPhase::ChangeDetection.to_string(), "change_detection");
        assert_eq!(TaskStatus::Pending.to_string(), "pending");
    }
}
