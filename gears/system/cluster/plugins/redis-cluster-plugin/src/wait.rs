//! The optional `WAIT <replicas> <timeout>` an operator can append to writes
//! whose outcome a failover must not silently undo (DESIGN.md §3.6).
//!
//! Its own module because both primitives use it and neither owns it: the cache
//! applies it to its conditional writes and the lock applies it to acquisition,
//! and a second copy would be free to disagree about what a short count means.
//!
//! **`WAIT` does not upgrade anything.** Per ADR-009 it narrows the Sentinel
//! failover window and leaves the declaration alone — `WAIT 1` reduces but does
//! not eliminate the window in which an acknowledged write is lost to a
//! promotion, and `CacheConsistency` is deliberately two-valued. Nothing in this
//! module touches `consistency()` or `features().linearizable`.

use cluster_sdk::{ClusterError, ProviderErrorKind};
use fred::clients::Pool;
use fred::interfaces::ServerInterface;

use crate::redis_error::map_redis_error;

/// The `WAIT` policy an operator configured, or the absence of one.
///
/// An enum rather than an `Option<WaitPolicy>` threaded through a function
/// argument and three struct fields: "wait, or don't" is a closed either/or, and
/// [`Disabled`](Self::Disabled) already says "no policy", so an `Option` around
/// it would encode the same absence twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPolicy {
    /// No `WAIT` is issued after a write or a lock acquisition.
    Disabled,
    /// Issue `WAIT <replicas> <timeout_ms>` and treat a short count as an error.
    Enabled(WaitTarget),
}

/// What an enabled [`WaitPolicy`] waits for: how many replicas, for how long.
///
/// A separate type with **private fields** rather than a struct variant, because
/// an enum variant's fields are always as visible as the enum itself and so
/// cannot be closed off. Closing them is the point: [`WaitPolicy::from_config`]
/// is then the only way to obtain one, which is what makes an unrepresentable
/// `timeout_ms` impossible rather than merely discouraged. The conversion from
/// the operator's `u64` milliseconds happens there, once, where there is
/// somewhere to report the failure — at the call site there is not, and the
/// clamp that results turns an oversized `wait_timeout_ms` into
/// `WAIT <n> 9223372036854775807`, a ~292-million-year deadline, silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitTarget {
    /// How many replicas must acknowledge.
    replicas: u32,
    /// How long to wait for them, in milliseconds — already narrowed to the
    /// signed count `fred`'s `WAIT` takes.
    timeout_ms: i64,
}

impl WaitPolicy {
    /// Builds the policy from the operator's `wait_replicas` and
    /// `wait_timeout_ms`.
    ///
    /// Called before any connection is opened, alongside the config's other
    /// startup checks, so a `wait_timeout_ms` that cannot be expressed fails
    /// startup the way a zero one already does rather than being clamped at the
    /// first write.
    ///
    /// # Errors
    /// [`ClusterError::InvalidConfig`] when `wait_timeout_ms` does not fit the
    /// signed 64-bit millisecond count `WAIT`'s timeout argument takes.
    pub fn from_config(
        wait_replicas: Option<u32>,
        wait_timeout_ms: u64,
    ) -> Result<Self, ClusterError> {
        let Some(replicas) = wait_replicas else {
            return Ok(Self::Disabled);
        };
        let timeout_ms = i64::try_from(wait_timeout_ms).map_err(|_| ClusterError::InvalidConfig {
            reason: format!(
                "wait_timeout_ms {wait_timeout_ms} does not fit in the signed 64-bit millisecond \
                 count `WAIT`'s timeout argument takes"
            ),
        })?;
        Ok(Self::Enabled(WaitTarget {
            replicas,
            timeout_ms,
        }))
    }
}

/// Issues `WAIT <replicas> <timeout>` when a policy is configured, and reports a
/// short count as an error.
///
/// The short-count arm is the whole reason this is not a fire-and-forget call.
/// `WAIT` does not *error* when it times out — it returns however many replicas
/// acknowledged — so a caller that ignored the count would have opted into a
/// durability guarantee and then silently not received it, which is worse than
/// never having offered the option.
///
/// # Errors
/// [`ClusterError::Provider`] with [`ProviderErrorKind::ResourceExhausted`] when
/// fewer than `replicas` acknowledged before the deadline, and whatever
/// [`map_redis_error`] makes of a failing `WAIT`.
pub async fn wait_for_replicas(pool: &Pool, policy: WaitPolicy) -> Result<(), ClusterError> {
    let WaitPolicy::Enabled(WaitTarget {
        replicas,
        timeout_ms,
    }) = policy
    else {
        return Ok(());
    };
    let acked: i64 = pool
        .wait(i64::from(replicas), timeout_ms)
        .await
        .map_err(map_redis_error)?;
    if acked < i64::from(replicas) {
        return Err(ClusterError::Provider {
            kind: ProviderErrorKind::ResourceExhausted,
            message: format!(
                "WAIT {replicas} {timeout_ms}ms: only {acked} replica(s) acknowledged the write \
                 before the deadline. The write is on the primary but is not yet replicated, so \
                 a failover now could lose it (DESIGN.md sec 3.6)"
            ),
        });
    }
    Ok(())
}

// Layer-1 unit tests (TESTING.md §2, `wait.rs` row): what the operator's two
// config fields become, and the one value that cannot become a policy at all.
// The `WAIT` round trip itself is covered at Layer 3. Out-of-line per DE1101.
#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
