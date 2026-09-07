//! Output port for the PDP reachability probe bootstrap runs
//! (`features/foundation.md`, "Gear Bootstrap and Readiness", step 6).
//!
//! The admission boundary fails closed on an unreachable PDP, so a gear that
//! reports ready with an unreachable PDP denies every request. The probe keeps
//! such a gear from reporting ready. Its only implementation is
//! `infra::pdp_probe::PdpReachability`.

use async_trait::async_trait;

use crate::domain::error::DomainError;

/// One round trip to the PDP at bootstrap.
#[async_trait]
pub trait PdpProbe: Send + Sync {
    /// Ask the PDP for one decision. Which decision it is does not matter;
    /// only that the PDP answered within the budget.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::PdpUnavailable`] when the PDP answers with a
    /// transport error or does not answer within the budget.
    async fn probe(&self) -> Result<(), DomainError>;
}
