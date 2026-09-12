//! Background self-registration (directory presence) for `OoP` gears
//! (`cpt-cf-component-oop-bootstrap`).
//!
//! **Presence** ([`presence_loop`]) is the single owner of this instance's
//! `DirectoryService` presence. It registers the instance's gRPC/REST endpoints
//! and `OpenAPI` spec (retrying with exponential backoff, 100ms → 30s cap), then
//! runs one loop that both sends periodic **heartbeats** (the steady-state
//! liveness signal the directory uses to keep the instance routable) and
//! periodically **re-registers** to self-heal after a `DirectoryService` restart
//! / connection loss. Consolidating both into one task (owned by the `OoP` serve
//! lifecycle) avoids the split-brain where an independent heartbeat task and a
//! re-registration task raced to write conflicting liveness state onto the same
//! directory record.
//!
//! Dependency resolution is **not** here: consumers are wired by the
//! proxy-wiring phase via typed `#[toolkit::consumes]` directory-resolving
//! clients, which feed the shared
//! [`DependencyChecker`](super::readiness::DependencyChecker) that gates
//! `/readyz`.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use cf_system_sdks::directory::{
    DirectoryClient, DirectoryInvalidArgument, DirectoryPermissionDenied, RegisterInstanceInfo,
};

/// Initial retry backoff for registration and dependency polling.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
/// Maximum retry backoff (cap).
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Interval at which a successfully-registered instance re-registers to
/// self-heal after a directory restart / connection loss.
const RE_REGISTER_INTERVAL: Duration = Duration::from_secs(30);

/// Next backoff in the exponential schedule (doubles, capped at [`MAX_BACKOFF`]).
fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_BACKOFF)
}

/// Sleep for `dur`, returning early (`false`) if `cancel` fires first.
async fn sleep_or_cancel(dur: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(dur) => true,
    }
}

/// Operator-facing guidance for a *permanent* registration rejection, or `None`
/// if `err` is transient and should be retried.
///
/// A permanent rejection can never succeed on retry, so the caller stops the
/// backoff retry and logs loudly instead of spinning forever at `warn!`. The
/// rejection *category* is already carried by the sentinel's `Display` (logged
/// in the structured `error` field), so this returns only the "what to check"
/// remediation hint.
fn permanent_rejection_guidance(err: &anyhow::Error) -> Option<&'static str> {
    if err.is::<DirectoryInvalidArgument>() {
        return Some("Check its configured labels and endpoint URIs");
    }
    if err.is::<DirectoryPermissionDenied>() {
        return Some(
            "Check that this peer is authorized to register this gear (its ServiceAccount \
             namespace / trust domain allowlist) and that none of its gRPC service names are \
             already owned by another gear",
        );
    }
    None
}

/// Register `info` with the directory, retrying with exponential backoff.
///
/// Returns `true` if the presence loop should keep running (the directory
/// accepted the registration, or [permanently rejected](permanent_rejection_guidance)
/// it), and `false` only if the task was cancelled. A permanent rejection still
/// returns `true`, so the caller keeps the presence loop alive; see
/// [`presence_loop`].
async fn register_once_with_backoff(
    directory: &Arc<dyn DirectoryClient>,
    info: &RegisterInstanceInfo,
    cancel: &CancellationToken,
) -> bool {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        match directory.register_instance(info.clone()).await {
            Ok(()) => {
                tracing::info!(gear = %info.gear, instance = %info.instance_id, "registered with DirectoryService");
                return true;
            }
            Err(e) => {
                if let Some(guidance) = permanent_rejection_guidance(&e) {
                    tracing::error!(
                        gear = %info.gear,
                        instance = %info.instance_id,
                        error = %e,
                        guidance,
                        "registration permanently rejected by the directory; this instance \
                         may not be discoverable"
                    );
                    return true;
                }
                tracing::warn!(
                    gear = %info.gear,
                    error = %e,
                    backoff_ms = backoff.as_millis(),
                    "registration attempt failed; retrying"
                );
                if !sleep_or_cancel(backoff, cancel).await {
                    return false;
                }
                backoff = next_backoff(backoff);
            }
        }
    }
}

/// Single directory-presence loop for an `OoP` instance.
///
/// Registers `info` (with backoff), then owns **both** liveness signals in one
/// task until `cancel` fires:
///
/// - a **heartbeat** every `heartbeat_interval` — the steady-state signal the
///   directory uses to keep the instance `Healthy`/routable (and to avoid
///   heartbeat-timeout eviction);
/// - an idempotent **re-registration** every [`RE_REGISTER_INTERVAL`] — self-heals
///   after a `DirectoryService` restart / connection loss (a heartbeat alone cannot,
///   since the directory silently ignores heartbeats for an instance it has
///   forgotten). Re-registration preserves the directory-side liveness state
///   (see `GearManager::register_instance`), so it does not disturb the
///   heartbeat-maintained `Healthy` state.
///
/// Registering *before* the first heartbeat is deliberate: a heartbeat for an
/// unregistered instance is a no-op on the directory.
///
/// A [permanent rejection](permanent_rejection_guidance) never tears the
/// presence loop down: it is logged loudly at every attempt, but the loop keeps
/// sending heartbeats (so an already-registered instance is not evicted by a
/// transient directory-side policy / validation change) and keeps retrying the
/// idempotent re-registration (so it self-heals if the directory later accepts
/// it — e.g. once a conflicting owner deregisters). Only cancellation stops the
/// loop.
pub(super) async fn presence_loop(
    directory: Arc<dyn DirectoryClient>,
    info: RegisterInstanceInfo,
    heartbeat_interval: Duration,
    cancel: CancellationToken,
) {
    if !register_once_with_backoff(&directory, &info, &cancel).await {
        return;
    }

    // `tokio::time::interval` panics on a zero period; clamp defensively.
    let heartbeat_interval = heartbeat_interval.max(Duration::from_secs(1));
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await; // consume the immediate first tick

    let mut reregister = tokio::time::interval(RE_REGISTER_INTERVAL);
    reregister.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reregister.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = heartbeat.tick() => {
                if let Err(e) = directory.send_heartbeat(&info.gear, &info.instance_id).await {
                    tracing::warn!(
                        gear = %info.gear,
                        instance = %info.instance_id,
                        error = %e,
                        "heartbeat failed; re-registering to self-heal"
                    );
                    if !register_once_with_backoff(&directory, &info, &cancel).await {
                        return;
                    }
                } else {
                    tracing::trace!(gear = %info.gear, "heartbeat sent");
                }
            }
            _ = reregister.tick() => {
                if !register_once_with_backoff(&directory, &info, &cancel).await {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "oop_registration_tests.rs"]
mod tests;
