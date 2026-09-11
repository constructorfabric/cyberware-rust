//! Background workers.
//!
//! - `ingest_outbox`: the `toolkit-db` outbox's `LeasedMessageHandler`
//!   draining the ingest publish path.
//! - `reaper`: expired subscriptions and idempotency-key cleanup
//!   (`DESIGN.md` §3.7 Key Invariants).
//! - `retention`: the paced tick that drives each topic's backend through one
//!   retention pass. The deletion itself stays in the backend that owns the
//!   rows, per `DESIGN.md` §3.7's invariant that the storage backend owns all
//!   event deletion; this only decides when a pass happens and what bounds it
//!   must end within.

pub mod ingest_outbox;
pub mod reaper;
pub mod retention;
pub mod specification_refresh;

pub use ingest_outbox::IngestOutboxHandler;
pub use reaper::Reaper;
pub use retention::{RetentionWorker, SweepReport};
pub use specification_refresh::SpecificationRefreshWorker;
