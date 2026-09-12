//! The sync engine for one repository (DESIGN §4 "Task Queue and Scheduling
//! Architecture"), ported from the reference implementation's `engine` and
//! `scheduler` crates.
//!
//! - [`task`]: [`ExtractionTask`], [`TaskPhase`], [`TaskStatus`], [`TaskPriority`], [`Lane`].
//! - [`queue`]: [`TaskQueue`] — indexed in-memory enqueue / claim / complete.
//! - [`worker`]: [`Worker`] trait and [`WorkerDispatcher`].
//! - [`runner`]: [`RepoPhaseRunner`] — phase-ordered executor.
//! - [`mirror_worker`]: the gear's own [`Worker`], fetching through the GitHub
//!   port and writing through the sync writer.
//! - [`change_gate`]: [`ChangeGate`] — decides which entities need refining.
//! - [`sweep_watermark`]: [`SweepWatermark`] — incremental sweep bounds.
//! - [`verification`]: [`CountGap`] — declared versus stored count repair.
//!
//! One level up, `gear.rs`'s `SyncPoolRunner` decides *which repository* syncs
//! next; everything here decides what happens inside one of those syncs.

pub mod change_gate;
pub mod mirror_worker;
pub mod queue;
pub mod runner;
pub mod sweep_watermark;
pub mod task;
pub mod verification;
pub mod worker;

pub use change_gate::{ChangeGate, GateInputs, GateReason};
pub use mirror_worker::{MirrorWorker, RunState};
pub use queue::TaskQueue;
pub use runner::{REPOSITORY_ENTITY, RepoPhaseRunner, RunReport, TaskFailure};
pub use sweep_watermark::{SweepWatermark, sweep_families};
pub use task::{ExtractionTask, Lane, NewTask, TaskPhase, TaskPriority, TaskStatus};
pub use verification::{CountGap, GapOutcome, MAX_REPAIR};
pub use worker::{Worker, WorkerContext, WorkerDispatcher};
