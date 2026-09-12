//! `K5`'s exit criteria over the real registry (DESIGN.md).
//!
//! # Why these drive a local registry rather than the process one
//!
//! [`requirements()`] is process-global by design, and `cargo test` runs a target's
//! tests **in one process, in parallel**. Tests that shared it would record into each
//! other's state and the whole suite would be order-dependent — so every test here
//! builds its own [`RequirementRegistry`], which is exactly the same type the process
//! one is. The two tests that must exercise the *global* path
//! ([`the_process_registry_is_one_per_process`] and the `bind` integration in
//! `binding_tests.rs`) say so.
//!
//! # And why they do not assert a process exit
//!
//! The escalation rule is asserted as a **decision** ([`escalation_due`]), never as
//! an action. Wiring it to `std::process::exit` is deliberately not implemented (see
//! `escalation_due`'s docs), and a test that could trigger one would take
//! the whole test binary down with it — under a paused clock, advancing five minutes
//! is one line.

use std::time::Duration;

use super::{
    CODE_MISCONFIGURED, CODE_PROFILE_DEGRADED, CODE_STARTING, CONTRIBUTOR_SILENCE_WARNING,
    NOT_WIRED_GRACE, PERMANENT_GRACE, RequirementRegistry, Verdict, requirements,
};
use crate::cache::{CacheCapability, validate_cache_capabilities_from};
use crate::dto::{
    CacheDescriptor, LeaderElectionDescriptor, LockDescriptor, ProfileDescriptor, ProfileHealth,
    WireCacheConsistency, WireCacheFeatures, WireLeaderElectionFeatures, WireLockFeatures,
};

const PROFILE: &str = "orders";

/// A descriptor for a linearizable, prefix-watching standalone cache.
fn descriptor(health: ProfileHealth) -> ProfileDescriptor {
    ProfileDescriptor {
        name: PROFILE.to_owned(),
        cache: CacheDescriptor {
            consistency: WireCacheConsistency::Linearizable,
            features: WireCacheFeatures {
                prefix_watch: true,
                watch: Some(true),
            },
            provider: "standalone".to_owned(),
        },
        lock: LockDescriptor {
            features: WireLockFeatures { linearizable: true },
            provider: "standalone".to_owned(),
        },
        leader_election: LeaderElectionDescriptor {
            features: WireLeaderElectionFeatures { linearizable: true },
            provider: "standalone".to_owned(),
        },
        health,
    }
}

/// Records a cache resolve requiring `caps`, exactly as `bind` would.
fn record_cache(registry: &RequirementRegistry, caps: Vec<CacheCapability>) {
    registry.record(
        PROFILE,
        "cache",
        Box::new(move |profile: &ProfileDescriptor| {
            validate_cache_capabilities_from(&profile.cache, &caps)
        }),
    );
}

/// A descriptor source that answers for `PROFILE` and nothing else.
fn available(health: ProfileHealth) -> impl Fn(&str) -> Option<ProfileDescriptor> {
    move |name: &str| (name == PROFILE).then(|| descriptor(health))
}

/// A descriptor source that answers for nothing — the cold start.
fn unavailable() -> impl Fn(&str) -> Option<ProfileDescriptor> {
    |_name: &str| None
}

// The four verdicts, in the order the classification evaluates them

/// A process that never resolved has no cluster readiness to report.
///
/// It must be `Ready`, not `Starting`: holding a consumer out of rotation for a
/// dependency it does not have would make the contributor unsafe to wire
/// unconditionally, and wiring it unconditionally is the whole ergonomic point.
#[test]
fn a_process_that_never_resolved_is_ready() {
    let registry = RequirementRegistry::default();
    assert_eq!(registry.verdict(&unavailable()), Verdict::Ready);
}

#[test]
fn a_satisfied_requirement_is_ready() {
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    record_cache(&registry, vec![CacheCapability::Linearizable]);

    assert_eq!(
        registry.verdict(&available(ProfileHealth::Serving)),
        Verdict::Ready
    );
}

/// Verdict 1, and the property that matters most: the deferred message is the
/// **inline error verbatim**, because it is produced by the same closure.
#[test]
fn an_unmet_requirement_is_permanent_and_carries_the_inline_diagnostic() {
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    // The fixture cache declares `prefix_watch: true` but is asked for `Fair` locks'
    // cache analogue - use a capability the descriptor genuinely lacks.
    record_cache(&registry, vec![CacheCapability::PrefixWatch]);

    // Satisfied first, to prove the fixture is not trivially failing.
    assert_eq!(
        registry.verdict(&available(ProfileHealth::Serving)),
        Verdict::Ready
    );

    // Now the same requirement against a descriptor that does not offer it.
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    record_cache(&registry, vec![CacheCapability::PrefixWatch]);
    let without_prefix_watch = |name: &str| {
        (name == PROFILE).then(|| {
            let mut profile = descriptor(ProfileHealth::Serving);
            profile.cache.features.prefix_watch = false;
            profile
        })
    };

    let verdict = registry.verdict(&without_prefix_watch);
    assert!(
        verdict.is_permanent(),
        "an unmet requirement cannot fix itself, so it must escalate: {verdict:?}"
    );
    assert_eq!(verdict.code(), CODE_MISCONFIGURED);

    // The exact inline error, embedded in the verdict. This is the assertion that
    // makes "the guarantee and the error text do not vary" (section 4.7) checkable:
    // both come from the same recorded closure.
    let inline = validate_cache_capabilities_from(
        &without_prefix_watch(PROFILE).expect("fixture").cache,
        &[CacheCapability::PrefixWatch],
    )
    .expect_err("the fixture must fail the requirement");
    let Verdict::Misconfigured(message) = verdict else {
        panic!("expected Misconfigured");
    };
    assert!(
        message.contains(&inline.to_string()),
        "the readiness message must carry the inline error verbatim.\n  inline: {inline}\n  verdict: {message}"
    );
    assert!(
        message.contains(PROFILE) && message.contains("cache"),
        "and it must name the profile and primitive: {message}"
    );
}

/// Verdict 2. Only reachable past the grace window, so the paused clock is what
/// makes it observable at all.
#[tokio::test(start_paused = true)]
async fn nothing_wired_becomes_permanent_only_after_the_grace_window() {
    let registry = RequirementRegistry::default();
    // Deliberately NOT calling `set_client_seen` - this is the Profile 1 build
    // mistake: `cluster` not linked, or the client feature off.
    record_cache(&registry, vec![CacheCapability::Linearizable]);

    // Inside the window it reads as a cold start, not a fault. A resolve that races
    // the wiring must not be reported broken.
    assert!(
        matches!(registry.verdict(&unavailable()), Verdict::Starting(_)),
        "inside the grace window, nothing-wired is indistinguishable from a cold start"
    );

    tokio::time::advance(NOT_WIRED_GRACE + Duration::from_secs(1)).await;

    let verdict = registry.verdict(&unavailable());
    assert!(
        verdict.is_permanent(),
        "past the window it is a fault: {verdict:?}"
    );
    let Verdict::Misconfigured(message) = verdict else {
        panic!("expected Misconfigured");
    };
    assert!(
        message.contains("cluster") && message.contains("linked"),
        "the message must tell an operator what to check: {message}"
    );
}

/// Verdict 3, and the two properties that distinguish it from verdict 1: it is
/// **not** permanent, and it **clears itself**.
#[test]
fn a_degraded_profile_is_transient_and_self_clearing() {
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    record_cache(&registry, vec![CacheCapability::Linearizable]);

    let verdict = registry.verdict(&available(ProfileHealth::Degraded));
    assert_eq!(verdict.code(), CODE_PROFILE_DEGRADED);
    assert!(
        !verdict.is_permanent(),
        "a backend outage must never crash-loop the fleet - that is the whole reason \
         this is a distinct variant rather than a differently-worded Misconfigured"
    );

    // Recovery, with no restart and no operator: the next poll returns Ready.
    assert_eq!(
        registry.verdict(&available(ProfileHealth::Serving)),
        Verdict::Ready,
        "a profile that recovers must return its consumers to rotation"
    );
}

/// Verdict 4 — the only verdict expected during a normal Profile 3 startup.
#[test]
fn descriptors_still_landing_is_starting() {
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    record_cache(&registry, vec![CacheCapability::Linearizable]);

    let verdict = registry.verdict(&unavailable());
    assert_eq!(verdict.code(), CODE_STARTING);
    assert!(!verdict.is_permanent());
}

/// The evaluation order is the specification, not an implementation detail.
///
/// A permanently misconfigured consumer whose descriptors are also missing must
/// report the misconfiguration — if `Starting` won, it would report a cold start
/// forever and never escalate.
#[tokio::test(start_paused = true)]
async fn a_misconfiguration_outranks_a_cold_start() {
    let registry = RequirementRegistry::default();
    record_cache(&registry, vec![CacheCapability::Linearizable]);
    tokio::time::advance(NOT_WIRED_GRACE + Duration::from_secs(1)).await;

    // Nothing wired *and* no descriptors: both verdict 2 and verdict 4 apply.
    assert_eq!(
        registry.verdict(&unavailable()).code(),
        CODE_MISCONFIGURED,
        "the permanent verdict must win, or this consumer never escalates"
    );
}

// Escalation — the decision, never the action

/// A permanent verdict escalates after the grace window; a transient one never does,
/// however long it lasts.
#[tokio::test(start_paused = true)]
async fn only_a_permanent_verdict_escalates() {
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    record_cache(&registry, vec![CacheCapability::PrefixWatch]);
    let without = |name: &str| {
        (name == PROFILE).then(|| {
            let mut profile = descriptor(ProfileHealth::Serving);
            profile.cache.features.prefix_watch = false;
            profile
        })
    };

    assert!(registry.verdict(&without).is_permanent());
    assert!(
        !registry.escalation_due(),
        "escalation must not be due the moment the verdict appears"
    );

    tokio::time::advance(PERMANENT_GRACE + Duration::from_secs(1)).await;
    // The timer is read at verdict time, so re-evaluate as a real poll would.
    assert!(registry.verdict(&without).is_permanent());
    assert!(
        registry.escalation_due(),
        "a permanent verdict past the window is due for escalation"
    );

    // And a *degraded* profile, held for twice as long, never is.
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    record_cache(&registry, vec![CacheCapability::Linearizable]);
    for _ in 0..2 {
        assert!(
            !registry
                .verdict(&available(ProfileHealth::Degraded))
                .is_permanent()
        );
        tokio::time::advance(PERMANENT_GRACE + Duration::from_secs(1)).await;
    }
    assert!(
        !registry.escalation_due(),
        "a transient backend outage must never escalate, no matter how long it lasts"
    );
}

/// A permanent verdict that clears resets the timer, so a later recurrence gets a
/// fresh window rather than an inherited one.
#[tokio::test(start_paused = true)]
async fn a_cleared_permanent_verdict_resets_the_timer() {
    let registry = RequirementRegistry::default();
    record_cache(&registry, vec![CacheCapability::Linearizable]);
    tokio::time::advance(NOT_WIRED_GRACE + Duration::from_secs(1)).await;
    assert!(registry.verdict(&unavailable()).is_permanent());

    // Just inside the window, so the next assertion is about the window and not about
    // arithmetic. `checked_sub` because `clippy::unchecked_time_subtraction` is denied.
    let almost = PERMANENT_GRACE
        .checked_sub(Duration::from_secs(1))
        .expect("the grace window is longer than a second");
    tokio::time::advance(almost).await;
    assert!(!registry.escalation_due());

    // The client appears: the fault clears.
    registry.set_client_seen(true);
    assert!(
        !registry
            .verdict(&available(ProfileHealth::Serving))
            .is_permanent()
    );
    tokio::time::advance(PERMANENT_GRACE).await;
    assert!(
        !registry.escalation_due(),
        "a fault that cleared must not escalate on the strength of how long it once lasted"
    );
}

// Recording

#[test]
fn recorded_profiles_are_deduplicated_and_ordered() {
    let registry = RequirementRegistry::default();
    record_cache(&registry, vec![]);
    record_cache(&registry, vec![]);
    registry.record("accounts", "lock", Box::new(|_| Ok(())));

    assert_eq!(registry.recorded_count(), 3, "every resolve is recorded");
    assert_eq!(
        registry.recorded_profiles(),
        vec!["accounts", PROFILE],
        "profiles are deduplicated and sorted, so diagnostics do not vary between runs"
    );
}

/// The process registry is a single instance, which is the property every
/// `resolve()` depends on.
#[test]
fn the_process_registry_is_one_per_process() {
    assert!(
        std::ptr::eq(requirements(), requirements()),
        "requirements() must hand back the same registry every time"
    );
}

// The contributor — the `/readyz` path

/// The contributor maps a verdict onto the framework's result, marks the registry as
/// polled, and — the assertion that matters for `/readyz` — reports **`Unhealthy`**
/// rather than `Degraded` for a degraded profile.
///
/// `Degraded` renders as `200` with `ready: true`, which would leave the consumer in
/// rotation; §4.4 requires the opposite. This is the one place that decision is
/// visible, and it is the mirror image of the cluster *gear's* own check.
///
/// It reads the **process** registry, because that is what the contributor does, so
/// it asserts on the verdict's shape rather than on a specific verdict: other tests
/// in this binary have recorded into the same registry, which is exactly the
/// condition a real process with several consumers is in.
#[tokio::test]
async fn the_contributor_reports_a_result_and_marks_the_registry_polled() {
    let hub = std::sync::Arc::new(toolkit::client_hub::ClientHub::default());
    let contributor = super::cluster_readiness(std::sync::Arc::clone(&hub));

    assert_eq!(contributor.name(), "cluster-requirements");
    let result = contributor.check().await;
    assert!(
        requirements().contributor_polled_for_test(),
        "the contributor must record that it was polled, or the missing-healthcheck \
         warning would fire in a process that wired one"
    );
    // Whatever the verdict, a not-ready one is never `Degraded`: that would keep the
    // consumer serving traffic it cannot serve.
    assert_ne!(
        result.status,
        toolkit::HealthcheckStatus::Degraded,
        "a cluster readiness failure must be Unhealthy (503), never Degraded (200)"
    );
}

// The missing-`healthcheck()` warning — I5's only mitigation for a consumer that
// omits the contributor line

/// A `tracing` writer that appends into a shared buffer.
#[derive(Clone)]
struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A **thread-local** WARN capture for the current test.
///
/// `set_default`, never `set_global_default`: [`a_wired_consumer_is_never_warned`]
/// asserts the *absence* of a warning, and a buffer shared with the tests below —
/// which raise that very warning on purpose — could never support that.
///
/// Every test here is a **current-thread** `#[tokio::test]`, which is load-bearing:
/// the warning is raised from the task `arm_contributor_silence_warning` spawns, and
/// a current-thread runtime polls that task on this thread, where this thread-local
/// subscriber is installed. Switching any of them to `flavor = "multi_thread"` would
/// silently stop capturing it.
fn warn_capture() -> (
    tracing::subscriber::DefaultGuard,
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(SharedWriter(std::sync::Arc::clone(&buf)))
        .with_max_level(tracing::Level::WARN)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, buf)
}

/// The captured lines mentioning the missing-`healthcheck()` instruction.
fn silence_warnings(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<String> {
    let bytes = buf
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| line.contains("no readiness contributor has been polled"))
        .map(std::borrow::ToOwned::to_owned)
        .collect()
}

/// A registry with the process registry's lifetime, so the one-shot armer (which
/// spawns a task holding it) can be exercised per-test.
///
/// Leaked deliberately: `arm_contributor_silence_warning` takes `&'static self`
/// because a spawned task outlives any borrow, and the module docs' rule that
/// every test drives *its own* registry is what keeps this binary order-
/// independent. One leaked registry per test is a few hundred bytes.
fn static_registry() -> &'static RequirementRegistry {
    Box::leak(Box::new(RequirementRegistry::default()))
}

/// **The realistic consumer.** It resolves several facades inside `start()`,
/// milliseconds apart, never resolves again, and omits the `healthcheck()` line —
/// so no contributor is ever polled.
///
/// This is the *only* scenario the warning exists to detect (a forgotten
/// `healthcheck()` is a log line rather than a silent loss of I5), and
/// it is the scenario a `record()`-edge trigger cannot reach: the condition needs
/// a minute to become true and the trigger has not fired for a minute.
#[tokio::test(start_paused = true)]
async fn audit_missing_healthcheck_warning_in_a_realistic_consumer() {
    let (_guard, buf) = warn_capture();
    let registry = static_registry();
    registry.set_client_seen(true);

    // `start()`: three facades, milliseconds apart, exactly as `bind` records them.
    record_cache(registry, vec![CacheCapability::Linearizable]);
    tokio::time::advance(Duration::from_millis(3)).await;
    registry.record(PROFILE, "lock", Box::new(|_| Ok(())));
    tokio::time::advance(Duration::from_millis(2)).await;
    registry.record(PROFILE, "leader", Box::new(|_| Ok(())));
    registry.arm_contributor_silence_warning();

    // `start()` returns. Nothing resolves again, and no contributor is ever polled.
    tokio::time::advance(CONTRIBUTOR_SILENCE_WARNING + Duration::from_secs(5)).await;
    // Let the armed task observe the advanced clock.
    tokio::task::yield_now().await;

    let captured = silence_warnings(&buf);
    assert_eq!(
        captured.len(),
        1,
        "EXPECTED the missing-healthcheck warning exactly once, captured: {captured:?}"
    );
    assert!(
        captured[0].contains("cluster_sdk::cluster_readiness(ctx.client_hub())"),
        "the warning must carry the exact line to add: {captured:?}"
    );
}

/// **Positive control**: the machinery is correct and only the trigger was wrong,
/// so the `record()` edge — a genuinely late second resolve — must still warn.
///
/// Deliberately *not* armed: this drives the edge path alone, so it stays a
/// control over `warn_if_nobody_is_listening` rather than over the one-shot.
#[tokio::test(start_paused = true)]
async fn audit_warning_fires_only_on_a_late_second_resolve() {
    let (_guard, buf) = warn_capture();
    let registry = RequirementRegistry::default();
    registry.set_client_seen(true);
    record_cache(&registry, vec![CacheCapability::Linearizable]);

    assert!(
        silence_warnings(&buf).is_empty(),
        "the first resolve starts the window; it cannot already have elapsed"
    );

    tokio::time::advance(CONTRIBUTOR_SILENCE_WARNING + Duration::from_secs(1)).await;
    registry.record(PROFILE, "lock", Box::new(|_| Ok(())));

    let captured = silence_warnings(&buf);
    assert_eq!(
        captured.len(),
        1,
        "LATE-RESOLVE LINES: {captured:?} — the edge path must still warn"
    );
}

/// A consumer that *did* wire the line is never told it forgot to.
///
/// The condition is level, so this is the assertion that keeps a level trigger
/// from becoming a false alarm: one poll before the window closes is enough.
#[tokio::test(start_paused = true)]
async fn a_wired_consumer_is_never_warned() {
    let (_guard, buf) = warn_capture();
    let registry = static_registry();
    registry.set_client_seen(true);
    record_cache(registry, vec![CacheCapability::Linearizable]);
    registry.arm_contributor_silence_warning();

    // The framework composes its router and probes well inside the window.
    tokio::time::advance(Duration::from_secs(5)).await;
    registry.note_contributor_polled();

    tokio::time::advance(CONTRIBUTOR_SILENCE_WARNING + Duration::from_secs(5)).await;
    tokio::task::yield_now().await;

    assert!(
        silence_warnings(&buf).is_empty(),
        "a consumer that wired the contributor must never be warned: {:?}",
        silence_warnings(&buf)
    );
}

/// Arming is idempotent and warns at most once, so a consumer resolving fifty
/// facades gets one task and one log line rather than fifty of each.
#[tokio::test(start_paused = true)]
async fn arming_is_idempotent_and_warns_once() {
    let (_guard, buf) = warn_capture();
    let registry = static_registry();
    registry.set_client_seen(true);
    for _ in 0..5 {
        record_cache(registry, vec![]);
        registry.arm_contributor_silence_warning();
    }

    tokio::time::advance(CONTRIBUTOR_SILENCE_WARNING + Duration::from_secs(5)).await;
    tokio::task::yield_now().await;

    // A late resolve after the one-shot has already warned must not warn again.
    registry.record(PROFILE, "lock", Box::new(|_| Ok(())));

    let captured = silence_warnings(&buf);
    assert_eq!(
        captured.len(),
        1,
        "at most one warning per process, however many resolves: {captured:?}"
    );
}

/// Every not-ready verdict maps to `Unhealthy` and carries its own code, so
/// `/health` distinguishes a cold start from a misconfiguration from a degraded
/// backend without parsing prose.
#[test]
fn each_verdict_maps_to_unhealthy_with_its_own_code() {
    let cases = [
        (Verdict::Starting("s".to_owned()), CODE_STARTING),
        (
            Verdict::ProfileDegraded("d".to_owned()),
            CODE_PROFILE_DEGRADED,
        ),
        (Verdict::Misconfigured("m".to_owned()), CODE_MISCONFIGURED),
    ];
    for (verdict, expected) in cases {
        let result = verdict.clone().into_result();
        assert_eq!(
            result.status,
            toolkit::HealthcheckStatus::Unhealthy,
            "{verdict:?} must be Unhealthy so /readyz is 503"
        );
        assert_eq!(
            result.code.as_deref(),
            Some(expected),
            "code for {verdict:?}"
        );
    }
    assert_eq!(
        Verdict::Ready.into_result().status,
        toolkit::HealthcheckStatus::Healthy
    );
}
