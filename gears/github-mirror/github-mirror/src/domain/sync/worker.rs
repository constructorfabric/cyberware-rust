//! Worker trait and dispatcher.
//!
//! Ported from the reference implementation's `scheduler/src/worker.rs`.
//! [`Worker`] is the extension point that handles specific
//! `(phase, entity_type)` combinations; [`WorkerDispatcher`] routes an
//! [`ExtractionTask`] to the first registered worker that claims it.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::queue::TaskQueue;
use super::task::{ExtractionTask, TaskPhase};
use crate::domain::error::DomainError;

/// Shared context passed to every [`Worker::execute`] invocation: the queue
/// (so a worker can enqueue follow-up tasks) and the run's cancellation token.
#[derive(Clone)]
pub struct WorkerContext {
    pub queue: Arc<TaskQueue>,
    pub cancel: CancellationToken,
}

/// Extension point for task execution.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Whether this worker handles the given `(phase, entity_type)`.
    fn handles(&self, phase: TaskPhase, entity_type: &str) -> bool;

    /// Execute the task. On success the runner marks it done; on error,
    /// failed. The worker must not touch task status itself.
    ///
    /// # Errors
    /// Whatever the fetch or the write failed with; the runner records it
    /// against the task.
    async fn execute(&self, ctx: &WorkerContext, task: &ExtractionTask) -> Result<(), DomainError>;
}

/// Routes tasks to the first [`Worker`] that claims them, in registration
/// order.
#[derive(Default)]
pub struct WorkerDispatcher {
    workers: Vec<Arc<dyn Worker>>,
}

impl WorkerDispatcher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, worker: Arc<dyn Worker>) {
        self.workers.push(worker);
    }

    /// # Errors
    /// `Internal` when no registered worker handles the task's
    /// `(phase, entity_type)`; otherwise whatever the worker returned.
    pub async fn dispatch(
        &self,
        ctx: &WorkerContext,
        task: &ExtractionTask,
    ) -> Result<(), DomainError> {
        for worker in &self.workers {
            if worker.handles(task.phase, &task.entity_type) {
                return worker.execute(ctx, task).await;
            }
        }
        Err(DomainError::internal(format!(
            "no worker registered for phase={} entity_type={}",
            task.phase, task.entity_type
        )))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::domain::sync::task::{ExtractionTask, TaskPhase, TaskPriority, TaskStatus};

    fn dummy_task(phase: TaskPhase, entity_type: &str) -> ExtractionTask {
        ExtractionTask {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            phase,
            entity_type: entity_type.to_owned(),
            entity_id: None,
            priority: TaskPriority::NORMAL,
            attempt: 0,
            status: TaskStatus::Running,
            created_at: Utc::now(),
        }
    }

    struct AlwaysSucceedWorker {
        handled_phase: TaskPhase,
        handled_entity: &'static str,
        called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Worker for AlwaysSucceedWorker {
        fn handles(&self, phase: TaskPhase, entity_type: &str) -> bool {
            phase == self.handled_phase && entity_type == self.handled_entity
        }

        async fn execute(
            &self,
            _ctx: &WorkerContext,
            _task: &ExtractionTask,
        ) -> Result<(), DomainError> {
            self.called.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    fn dummy_context() -> WorkerContext {
        WorkerContext {
            queue: Arc::new(TaskQueue::new()),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn dispatcher_routes_to_matching_worker() {
        let called = Arc::new(AtomicBool::new(false));
        let worker = Arc::new(AlwaysSucceedWorker {
            handled_phase: TaskPhase::Discovery,
            handled_entity: "repository",
            called: Arc::clone(&called),
        });

        let mut dispatcher = WorkerDispatcher::new();
        dispatcher.register(worker);

        let ctx = dummy_context();
        let task = dummy_task(TaskPhase::Discovery, "repository");
        dispatcher.dispatch(&ctx, &task).await.expect("dispatch");
        assert!(
            called.load(Ordering::Relaxed),
            "worker must have been called"
        );
    }

    #[tokio::test]
    async fn dispatcher_returns_an_error_for_an_unregistered_phase() {
        let dispatcher = WorkerDispatcher::new();
        let ctx = dummy_context();
        let task = dummy_task(TaskPhase::Refinement, "issue");
        assert!(
            dispatcher.dispatch(&ctx, &task).await.is_err(),
            "no worker handles refinement here"
        );
    }

    #[tokio::test]
    async fn dispatcher_skips_a_non_matching_worker() {
        let called = Arc::new(AtomicBool::new(false));
        let wrong_worker = Arc::new(AlwaysSucceedWorker {
            handled_phase: TaskPhase::Indexing,
            handled_entity: "pull_requests",
            called: Arc::clone(&called),
        });

        let mut dispatcher = WorkerDispatcher::new();
        dispatcher.register(wrong_worker);

        let ctx = dummy_context();
        let task = dummy_task(TaskPhase::Discovery, "repository");
        assert!(dispatcher.dispatch(&ctx, &task).await.is_err());
        assert!(
            !called.load(Ordering::Relaxed),
            "wrong worker must not be called"
        );
    }
}
