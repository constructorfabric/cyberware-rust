use crate::domain::ports::github::{DeclaredCounts, PullDetail};

pub const MAX_REPAIR: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapOutcome {
    Complete,
    AcceptedDrift,
    Repair,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountGap {
    pub entity_type: String,
    pub expected: u64,
    pub stored: u64,
    pub repair_attempts: u32,
    pub previous_gap: Option<u64>,
}

impl CountGap {
    #[must_use]
    pub fn size(&self) -> u64 {
        self.expected.saturating_sub(self.stored)
    }

    #[must_use]
    pub fn outcome(&self) -> GapOutcome {
        if self.size() == 0 {
            return GapOutcome::Complete;
        }
        let stalled = self.previous_gap == Some(self.size());
        if stalled || self.repair_attempts >= MAX_REPAIR {
            return GapOutcome::AcceptedDrift;
        }
        GapOutcome::Repair
    }

    #[must_use]
    pub fn advance(&self, stored: u64) -> Self {
        Self {
            entity_type: self.entity_type.clone(),
            expected: self.expected,
            stored,
            repair_attempts: self.repair_attempts + 1,
            previous_gap: Some(self.size()),
        }
    }
}

#[must_use]
pub fn pull_gaps(detail: &PullDetail) -> Vec<CountGap> {
    let DeclaredCounts { commits, files, .. } = detail.declared;
    [
        ("pull_request_commits", commits, detail.commits.len()),
        ("pull_request_files", files, detail.files.len()),
    ]
    .into_iter()
    .filter_map(|(entity_type, declared, stored)| {
        let expected = u64::try_from(declared?).ok()?;
        let stored = u64::try_from(stored).unwrap_or(u64::MAX);
        (expected > stored).then(|| CountGap {
            entity_type: entity_type.to_owned(),
            expected,
            stored,
            repair_attempts: 0,
            previous_gap: None,
        })
    })
    .collect()
}

#[cfg(test)]
#[path = "verification_tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod verification_tests;
