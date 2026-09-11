//! The wall-clock seam.
//!
//! Only for values that appear on the wire or in a persisted row - a
//! heartbeat's timestamp, a subscription's expiry. Monotonic time and sleeping
//! are deliberately **not** abstracted: `tokio::time` is already the injectable
//! clock for those, and `pause`/`advance` intercept sleeps, timeouts and
//! `Instant::now` globally with no production indirection to pay for.

use std::sync::Arc;

use chrono::{DateTime, Utc};

pub type NowFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The production clock.
#[must_use]
pub fn system_now() -> NowFn {
    Arc::new(Utc::now)
}
