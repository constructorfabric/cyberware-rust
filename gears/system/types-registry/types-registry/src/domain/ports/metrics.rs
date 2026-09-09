//! Output port for admission metrics.

use std::time::Duration;

use toolkit_macros::domain_model;

use crate::domain::admission::vector::VectorDrift;
use crate::domain::enums::OperationItemStatus;

/// Which half of SPEC §8.1 refused a submission.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalStage {
    /// Steps 1–8: a refusal before the request became a durable operation.
    Acceptance,
    /// Step 3 onwards, per candidate: a refusal recorded on an operation item.
    Admission,
}

impl RefusalStage {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Acceptance => "acceptance",
            Self::Admission => "admission",
        }
    }
}

/// A candidate outcome — the only statuses a terminalized candidate can carry.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Succeeded,
    Unchanged,
    Failed,
}

impl TerminalStatus {
    /// Stable snake-case label value, independent of `Debug`.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Unchanged => "unchanged",
            Self::Failed => "failed",
        }
    }
}

impl TryFrom<OperationItemStatus> for TerminalStatus {
    type Error = OperationItemStatus;

    fn try_from(status: OperationItemStatus) -> Result<Self, Self::Error> {
        match status {
            OperationItemStatus::Succeeded => Ok(Self::Succeeded),
            OperationItemStatus::Unchanged => Ok(Self::Unchanged),
            OperationItemStatus::Failed => Ok(Self::Failed),
            // Preserve the non-terminal status in the error.
            non_terminal => Err(non_terminal),
        }
    }
}

/// The admission path's instrument set.
pub trait AdmissionMetrics: std::fmt::Debug + Send + Sync {
    /// Count candidates terminalized by this pass, by status.
    fn candidate_terminalized(&self, status: TerminalStatus);

    /// `types_registry_refusals_total{stage,reason}` — one increment per refusal.
    fn refused(&self, stage: RefusalStage, reason: &'static str);

    /// Count revalidation retries by drift.
    fn revalidation_retried(&self, drift: &VectorDrift);

    /// Record dependents rewritten by one revision, including zero.
    fn observe_activation_write_set(&self, refreshed: usize);

    /// `types_registry_operation_duration_seconds` — one admission pass, wall-clock.
    fn observe_operation_duration(&self, elapsed: Duration);
}

/// Instruments that count nothing, for a caller with no meter to inject.
#[domain_model]
#[derive(Debug, Default)]
pub struct NoopMetrics;

impl AdmissionMetrics for NoopMetrics {
    fn candidate_terminalized(&self, _status: TerminalStatus) {}

    fn refused(&self, _stage: RefusalStage, _reason: &'static str) {}

    fn revalidation_retried(&self, _drift: &VectorDrift) {}

    fn observe_activation_write_set(&self, _refreshed: usize) {}

    fn observe_operation_duration(&self, _elapsed: Duration) {}
}
