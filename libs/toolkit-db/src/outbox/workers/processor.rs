use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::super::core::Outbox;
use super::super::handler::HandlerResult;
use super::super::statements::OutboxStatements;
use super::super::store::OutboxStore;
use super::super::strategy::{ProcessContext, ProcessingStrategy};
use super::super::taskward::{Directive, WorkerAction};
use super::super::types::OutboxError;
use crate::Db;

/// Report emitted by a processor execution cycle.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProcessorReport {
    /// The partition that was processed.
    pub partition_id: i64,
    /// Number of messages dispatched to the handler.
    pub messages_processed: u32,
    /// Outcome of the handler invocation.
    pub handler_result: HandlerResult,
}

/// Per-partition adaptive batch sizing state machine.
///
/// Degrades to single-message mode on failure, ramps back up on consecutive
/// successes. Analogous to TCP slow start.
#[derive(Debug, Clone)]
pub struct PartitionMode {
    pub state: PartitionModeState,
    pub consecutive_failures: u32,
}

impl PartitionMode {
    /// Create a new `PartitionMode` in normal state.
    pub fn new() -> Self {
        Self {
            state: PartitionModeState::Normal,
            consecutive_failures: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PartitionModeState {
    /// Normal operation — use configured `batch_size`.
    Normal,
    /// Degraded after failure — process fewer messages at a time.
    /// Ramps back up (doubling) on consecutive successes until reaching
    /// the configured `batch_size`, then transitions back to `Normal`.
    Degraded {
        effective_size: u32,
        consecutive_successes: u32,
    },
}

impl PartitionMode {
    /// Returns the effective batch size for this mode.
    pub(crate) fn effective_batch_size(&self, configured: u32) -> u32 {
        match &self.state {
            PartitionModeState::Normal => configured,
            PartitionModeState::Degraded { effective_size, .. } => *effective_size,
        }
    }

    /// Compute the current retry backoff based on consecutive failures.
    ///
    /// Returns `base` when no failures have occurred, then escalates
    /// exponentially (base * 2^(failures-1)) capped at `max`.
    pub fn current_backoff(&self, base: Duration, max: Duration) -> Duration {
        let failures = self.consecutive_failures;
        if failures == 0 {
            return base;
        }
        #[allow(clippy::cast_possible_wrap)]
        let exp = (failures.saturating_sub(1) as i32).min(30);
        base.mul_f64(2.0_f64.powi(exp)).min(max)
    }

    /// Transition after a handler result.
    ///
    /// `processed_count`: how many messages were successfully processed before
    /// the batch ended. `Some` for `PerMessageAdapter` handlers, `None` for batch
    /// handlers. On Retry/Reject, degradation uses `max(pc, 1)` when known,
    /// or falls back to 1 when `None` (batch handler — we can't know where
    /// the failure occurred).
    ///
    /// `degradation_threshold`: how many consecutive failures before the batch
    /// size degrades. Set to 1 for immediate degradation (legacy behavior).
    pub(crate) fn transition(
        &mut self,
        result: &HandlerResult,
        configured_batch_size: u32,
        processed_count: Option<u32>,
        degradation_threshold: u32,
    ) {
        match result {
            HandlerResult::Success => {
                self.consecutive_failures = 0;
                match &mut self.state {
                    PartitionModeState::Normal => {}
                    PartitionModeState::Degraded {
                        effective_size,
                        consecutive_successes,
                    } => {
                        *consecutive_successes += 1;
                        // Double the effective size on each consecutive success
                        let next = effective_size.saturating_mul(2).min(configured_batch_size);
                        if next >= configured_batch_size {
                            self.state = PartitionModeState::Normal;
                        } else {
                            *effective_size = next;
                        }
                    }
                }
            }
            HandlerResult::Retry { .. } => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= degradation_threshold {
                    // Degrade: use max(processed_count, 1) as the new effective
                    // size. If the handler processed some messages before failing,
                    // we know the failure is at position pc+1, so we degrade to
                    // max(pc, 1) to isolate the poison message. For batch handlers
                    // (None), fall back to 1 (most conservative).
                    let degrade_to = processed_count.map_or(1, |pc| pc.max(1));
                    self.state = PartitionModeState::Degraded {
                        effective_size: degrade_to,
                        consecutive_successes: 0,
                    };
                }
            }
            HandlerResult::Reject { .. } => {
                // Reject is terminal — the message is poison and has been
                // dead-lettered, so the cursor advances past it. Reset failure
                // count (no backoff needed) but degrade immediately to isolate
                // any further poison messages in the next batch.
                self.consecutive_failures = 0;
                let degrade_to = processed_count.map_or(1, |pc| pc.max(1));
                self.state = PartitionModeState::Degraded {
                    effective_size: degrade_to,
                    consecutive_successes: 0,
                };
            }
        }
    }
}

/// A per-partition processor parameterized by its processing strategy.
///
/// Each instance owns exactly one `partition_id` and runs as a long-lived
/// tokio task. The strategy (`TransactionalStrategy` or `DecoupledStrategy`)
/// is baked in at compile time via monomorphization.
pub struct PartitionProcessor<S: ProcessingStrategy> {
    strategy: S,
    partition_id: i64,
    tuning: super::super::types::WorkerTuning,
    db: Db,
    statements: Arc<OutboxStatements>,
    mailbox: Arc<super::super::subscription::Mailbox>,
    partition_mode: PartitionMode,
}

impl<S: ProcessingStrategy> PartitionProcessor<S> {
    pub fn new(
        strategy: S,
        partition_id: i64,
        tuning: super::super::types::WorkerTuning,
        db: Db,
        // Both the statements and the mailbox hang off the outbox, so it is
        // one argument in place of two.
        outbox: &Arc<Outbox>,
    ) -> Self {
        let statements = outbox.statements_arc();
        let mailbox = outbox.mailbox();
        Self {
            strategy,
            partition_id,
            tuning,
            db,
            statements,
            mailbox,
            partition_mode: PartitionMode::new(),
        }
    }
}

impl<S: ProcessingStrategy> WorkerAction for PartitionProcessor<S> {
    type Payload = ProcessorReport;
    type Error = OutboxError;

    async fn execute(
        &mut self,
        _cancel: &CancellationToken,
    ) -> Result<Directive<ProcessorReport>, OutboxError> {
        let backend = {
            let sea_conn = self.db.sea_internal();
            sea_conn.get_database_backend()
        };
        debug_assert_eq!(backend, self.statements.backend());
        let store = OutboxStore::new(&self.statements);

        let effective_size = self
            .partition_mode
            .effective_batch_size(self.tuning.batch_size);

        let ctx = ProcessContext {
            db: &self.db,
            store,
            partition_id: self.partition_id,
            mailbox: &self.mailbox,
        };

        let result = self.strategy.process(&ctx, effective_size).await?;

        if let Some(pr) = result {
            let has_more = pr.count >= effective_size;
            let clamped_pc = pr.processed_count.map(|pc| pc.min(pr.count));
            self.partition_mode.transition(
                &pr.handler_result,
                self.tuning.batch_size,
                clamped_pc,
                self.tuning.degradation_threshold,
            );
            if pr.count > 0 {
                debug!(
                    partition_id = self.partition_id,
                    count = pr.count,
                    mode = ?self.partition_mode,
                    "partition batch complete"
                );
            }
            let report = ProcessorReport {
                partition_id: self.partition_id,
                messages_processed: pr.count,
                handler_result: pr.handler_result.clone(),
            };
            match pr.handler_result {
                HandlerResult::Success => {
                    if has_more {
                        Ok(Directive::Proceed(report))
                    } else {
                        Ok(Directive::Idle(report))
                    }
                }
                HandlerResult::Retry { .. } => {
                    let backoff = self
                        .partition_mode
                        .current_backoff(self.tuning.retry_base, self.tuning.retry_max);
                    Ok(Directive::Sleep(backoff, report))
                }
                HandlerResult::Reject { .. } => {
                    // Cursor advanced past the poison message — proceed
                    // immediately to process the next batch without backoff.
                    Ok(Directive::Proceed(report))
                }
            }
        } else {
            Ok(Directive::Idle(ProcessorReport {
                partition_id: self.partition_id,
                messages_processed: 0,
                handler_result: HandlerResult::Success,
            }))
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::*;

    // Helper: create a degraded PartitionMode.
    fn degraded(effective_size: u32, consecutive_successes: u32) -> PartitionMode {
        PartitionMode {
            state: PartitionModeState::Degraded {
                effective_size,
                consecutive_successes,
            },
            consecutive_failures: 0,
        }
    }

    // ---- PartitionMode state machine tests ----

    #[test]
    fn partition_mode_normal_uses_configured_size() {
        let mode = PartitionMode::new();
        assert_eq!(mode.effective_batch_size(50), 50);
    }

    #[test]
    fn partition_mode_degraded_uses_effective_size() {
        let mode = degraded(4, 2);
        assert_eq!(mode.effective_batch_size(50), 4);
    }

    #[test]
    fn partition_mode_retry_degrades_to_one() {
        let mut mode = PartitionMode::new();
        mode.transition(
            &HandlerResult::Retry {
                reason: "fail".into(),
            },
            50,
            None, // batch handler
            1,    // degrade immediately
        );
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 1,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_success_ramps_up() {
        let mut mode = degraded(1, 0);
        // 1 → 2
        mode.transition(&HandlerResult::Success, 50, None, 1);
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 2,
                consecutive_successes: 1,
            }
        ));
        // 2 → 4
        mode.transition(&HandlerResult::Success, 50, None, 1);
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 4,
                ..
            }
        ));
        // 4 → 8
        mode.transition(&HandlerResult::Success, 50, None, 1);
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 8,
                ..
            }
        ));
    }

    #[test]
    fn partition_mode_ramps_up_to_normal() {
        let mut mode = degraded(16, 4);
        // 16 → 32
        mode.transition(&HandlerResult::Success, 32, None, 1);
        // Should transition back to Normal since 32 >= configured(32)
        assert!(matches!(mode.state, PartitionModeState::Normal));
    }

    #[test]
    fn partition_mode_reject_in_normal_degrades() {
        let mut mode = PartitionMode::new();
        mode.transition(
            &HandlerResult::Reject {
                reason: "bad".into(),
            },
            50,
            None, // batch handler — falls back to 1
            1,    // degrade immediately
        );
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 1,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_reject_with_processed_count() {
        // PerMessageAdapter handler processed 3 msgs before poison at index 3
        let mut mode = PartitionMode::new();
        mode.transition(
            &HandlerResult::Reject {
                reason: "bad".into(),
            },
            50,
            Some(3), // PerMessageAdapter processed 3 successfully
            1,       // degrade immediately
        );
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 3,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_retry_with_processed_count_zero() {
        // PerMessageAdapter failed at the very first message
        let mut mode = PartitionMode::new();
        mode.transition(
            &HandlerResult::Retry {
                reason: "fail".into(),
            },
            50,
            Some(0), // failed at first message
            1,       // degrade immediately
        );
        // max(0, 1) = 1
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 1,
                consecutive_successes: 0,
            }
        ));
    }

    #[test]
    fn partition_mode_success_in_normal_stays_normal() {
        let mut mode = PartitionMode::new();
        mode.transition(&HandlerResult::Success, 50, None, 1);
        assert!(matches!(mode.state, PartitionModeState::Normal));
    }

    #[test]
    fn partition_mode_full_recovery_cycle() {
        let mut mode = PartitionMode::new();

        // Retry → Degraded(1)
        mode.transition(&HandlerResult::Retry { reason: "x".into() }, 8, None, 1);
        assert_eq!(mode.effective_batch_size(8), 1);

        // Success: 1→2→4→8→Normal
        mode.transition(&HandlerResult::Success, 8, None, 1);
        assert_eq!(mode.effective_batch_size(8), 2);
        mode.transition(&HandlerResult::Success, 8, None, 1);
        assert_eq!(mode.effective_batch_size(8), 4);
        mode.transition(&HandlerResult::Success, 8, None, 1);
        assert!(matches!(mode.state, PartitionModeState::Normal));
        assert_eq!(mode.effective_batch_size(8), 8);
    }

    // ---- Degradation threshold tests ----

    #[test]
    fn partition_mode_does_not_degrade_below_threshold() {
        let mut mode = PartitionMode::new();
        // threshold=3, so first two failures should NOT degrade
        mode.transition(&HandlerResult::Retry { reason: "x".into() }, 50, None, 3);
        assert!(matches!(mode.state, PartitionModeState::Normal));
        assert_eq!(mode.consecutive_failures, 1);

        mode.transition(&HandlerResult::Retry { reason: "x".into() }, 50, None, 3);
        assert!(matches!(mode.state, PartitionModeState::Normal));
        assert_eq!(mode.consecutive_failures, 2);

        // Third failure hits threshold → degrades
        mode.transition(&HandlerResult::Retry { reason: "x".into() }, 50, None, 3);
        assert!(matches!(
            mode.state,
            PartitionModeState::Degraded {
                effective_size: 1,
                ..
            }
        ));
        assert_eq!(mode.consecutive_failures, 3);
    }

    #[test]
    fn partition_mode_success_resets_consecutive_failures() {
        let mut mode = PartitionMode::new();
        mode.transition(&HandlerResult::Retry { reason: "x".into() }, 50, None, 3);
        assert_eq!(mode.consecutive_failures, 1);
        mode.transition(&HandlerResult::Success, 50, None, 3);
        assert_eq!(mode.consecutive_failures, 0);
    }

    // ---- current_backoff tests ----

    #[test]
    fn current_backoff_no_failures_returns_base() {
        let mode = PartitionMode::new();
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(30);
        assert_eq!(mode.current_backoff(base, max), base);
    }

    #[test]
    fn current_backoff_escalates_exponentially() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(30);

        let mut mode = PartitionMode {
            state: PartitionModeState::Normal,
            consecutive_failures: 1,
        };
        // 1 failure: base * 2^0 = 100ms
        assert_eq!(mode.current_backoff(base, max), Duration::from_millis(100));

        mode.consecutive_failures = 2;
        // 2 failures: base * 2^1 = 200ms
        assert_eq!(mode.current_backoff(base, max), Duration::from_millis(200));

        mode.consecutive_failures = 3;
        // 3 failures: base * 2^2 = 400ms
        assert_eq!(mode.current_backoff(base, max), Duration::from_millis(400));
    }

    #[test]
    fn current_backoff_caps_at_max() {
        let base = Duration::from_millis(100);
        let max = Duration::from_millis(500);

        let mode = PartitionMode {
            state: PartitionModeState::Normal,
            consecutive_failures: 10,
        };
        assert_eq!(mode.current_backoff(base, max), max);
    }
}
