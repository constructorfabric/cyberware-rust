use std::collections::BTreeSet;
use std::sync::Mutex;

use fred::error::ErrorKind;

use super::*;

/// What the double should answer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EvalOutcome {
    /// Succeed.
    Ok,
    /// Answer `NOSCRIPT` to everything, including the `EVAL` recovery — which a
    /// real server cannot do, since `EVAL` carries the source. The double can,
    /// and that is the point: it is the only way to observe that the recovery
    /// path is entered at most once.
    NoScript,
    /// Answer `NOSCRIPT` to `EVALSHA` and succeed on the `EVAL` recovery — the
    /// ordinary case this policy exists for.
    NoScriptThenOk,
}

/// An in-memory [`ScriptExecutor`] that records what it was asked to do.
///
/// The point of the seam: the recovery bound has a real failure mode — a server
/// that answers `NOSCRIPT` to everything turning one restart into an unbounded
/// retry storm — and it is only observable by counting calls, which a live Redis
/// makes far harder than a double does.
struct FakeExecutor {
    outcome: EvalOutcome,
    state: Mutex<Calls>,
}

#[derive(Default)]
struct Calls {
    /// Sources passed to `SCRIPT LOAD`.
    loads: Vec<&'static str>,
    /// SHAs passed to `EVALSHA`.
    evalshas: Vec<String>,
    /// `(source, key)` pairs passed to the `EVAL` recovery.
    eval_sources: Vec<(&'static str, String)>,
}

impl FakeExecutor {
    fn new(outcome: EvalOutcome) -> Self {
        Self {
            outcome,
            state: Mutex::new(Calls::default()),
        }
    }

    fn load_count(&self) -> usize {
        self.state.lock().expect("uncontended").loads.len()
    }

    fn evalsha_count(&self) -> usize {
        self.state.lock().expect("uncontended").evalshas.len()
    }

    fn eval_source_calls(&self) -> Vec<(&'static str, String)> {
        self.state.lock().expect("uncontended").eval_sources.clone()
    }

    fn noscript() -> Error {
        Error::new(ErrorKind::Unknown, "NOSCRIPT No matching script.")
    }
}

impl ScriptExecutor for FakeExecutor {
    async fn script_load(&self, source: &'static str) -> Result<String, Error> {
        self.state.lock().expect("uncontended").loads.push(source);
        // A stand-in for the SHA the server computes: stable per source, as a
        // real one is.
        Ok(format!("sha-of-{:x}", source.len()))
    }

    async fn evalsha(&self, sha: &str, _key: &str, _args: &[Value]) -> Result<Value, Error> {
        self.state
            .lock()
            .expect("uncontended")
            .evalshas
            .push(sha.to_owned());
        if self.outcome == EvalOutcome::Ok {
            Ok(Value::Integer(1))
        } else {
            Err(Self::noscript())
        }
    }

    async fn eval_source(
        &self,
        source: &'static str,
        key: &str,
        _args: &[Value],
    ) -> Result<Value, Error> {
        self.state
            .lock()
            .expect("uncontended")
            .eval_sources
            .push((source, key.to_owned()));
        if self.outcome == EvalOutcome::NoScript {
            Err(Self::noscript())
        } else {
            Ok(Value::Integer(1))
        }
    }
}

#[tokio::test]
async fn every_script_is_loaded_exactly_once_and_its_sha_cached() {
    let executor = FakeExecutor::new(EvalOutcome::Ok);
    let cache = load_catalog(&executor, ALL_SCRIPTS)
        .await
        .expect("the catalog loads");

    assert_eq!(
        executor.load_count(),
        ALL_SCRIPTS.len(),
        "SCRIPT LOAD must be issued once per catalogued script, not once per call site"
    );
    for script in ALL_SCRIPTS {
        cache
            .sha(script.name)
            .unwrap_or_else(|err| panic!("`{}` must be in the cache: {err}", script.name));
    }
}

/// The signal sink `eval` reports a `NOSCRIPT` recovery through.
///
/// `cluster_redis_script_reloads_total` is an `OpenTelemetry` counter rather
/// than a `ClusterMetrics` call, so its *value* needs a reader and is asserted
/// at Layer 3 (`RD-LIFE-008`). What this keeps testable here is the recovery
/// policy itself, which is the part with a real failure mode.
fn signals() -> std::sync::Arc<crate::observability::RedisSignals> {
    crate::test_support::recording_signals().0
}

#[tokio::test]
async fn a_successful_eval_costs_one_round_trip_and_no_recovery() {
    let executor = FakeExecutor::new(EvalOutcome::Ok);
    let cache = load_catalog(&executor, CACHE_SCRIPTS)
        .await
        .expect("the catalog loads");
    let loads_after_startup = executor.load_count();

    let script = &CACHE_SCRIPTS[0];
    eval(&executor, &cache, script, "cluster:c:k", &[], &signals())
        .await
        .expect("the eval succeeds");

    assert_eq!(executor.evalsha_count(), 1);
    assert!(
        executor.eval_source_calls().is_empty(),
        "the happy path must not fall back to EVAL"
    );
    assert_eq!(
        executor.load_count(),
        loads_after_startup,
        "the happy path must not touch SCRIPT LOAD"
    );
}

#[tokio::test]
async fn one_noscript_recovers_with_a_single_key_routed_eval() {
    let executor = FakeExecutor::new(EvalOutcome::NoScriptThenOk);
    let cache = load_catalog(&executor, CACHE_SCRIPTS)
        .await
        .expect("the catalog loads");
    let loads_after_startup = executor.load_count();

    let script = &CACHE_SCRIPTS[0];
    eval(&executor, &cache, script, "cluster:c:k", &[], &signals())
        .await
        .expect("the recovery succeeds");

    assert_eq!(
        executor.evalsha_count(),
        1,
        "the EVALSHA is not repeated; EVAL replaces it"
    );
    assert_eq!(
        executor.load_count(),
        loads_after_startup,
        "recovery is EVAL, not a second SCRIPT LOAD: a keyless SCRIPT LOAD could warm a node \
         other than the one that reported the miss"
    );
    assert_eq!(
        executor.eval_source_calls(),
        vec![(script.source, "cluster:c:k".to_owned())],
        "the recovery must carry the same key, so it routes to the node that missed"
    );
}

#[tokio::test]
async fn a_second_noscript_is_a_provider_error_rather_than_an_unbounded_loop() {
    let executor = FakeExecutor::new(EvalOutcome::NoScript);
    let cache = load_catalog(&executor, CACHE_SCRIPTS)
        .await
        .expect("the catalog loads");

    let script = &CACHE_SCRIPTS[0];
    let err = eval(&executor, &cache, script, "cluster:c:k", &[], &signals())
        .await
        .expect_err("a server that always answers NOSCRIPT must fail the call");

    assert!(
        matches!(
            err,
            ClusterError::Provider {
                kind: ProviderErrorKind::Other,
                ..
            }
        ),
        "a NOSCRIPT from the recovery path must surface as Provider{{Other}}, got {err:?}"
    );
    assert_eq!(executor.evalsha_count(), 1);
    assert_eq!(
        executor.eval_source_calls().len(),
        1,
        "the recovery path must be entered at most once per call"
    );
}

#[tokio::test]
async fn a_non_noscript_failure_is_not_retried_at_all() {
    struct AlwaysDown;
    impl ScriptExecutor for AlwaysDown {
        async fn script_load(&self, _source: &'static str) -> Result<String, Error> {
            Ok("sha".to_owned())
        }
        async fn evalsha(&self, _sha: &str, _key: &str, _args: &[Value]) -> Result<Value, Error> {
            Err(Error::new(ErrorKind::IO, "connection reset by peer"))
        }
        async fn eval_source(
            &self,
            _source: &'static str,
            _key: &str,
            _args: &[Value],
        ) -> Result<Value, Error> {
            panic!("an IO failure is not a script-cache miss and must not reach the recovery path")
        }
    }

    let cache = load_catalog(&AlwaysDown, CACHE_SCRIPTS)
        .await
        .expect("the catalog loads");
    let err = eval(&AlwaysDown, &cache, &CACHE_SCRIPTS[0], "k", &[], &signals())
        .await
        .expect_err("an IO failure must surface");
    assert!(
        matches!(
            err,
            ClusterError::Provider {
                kind: ProviderErrorKind::ConnectionLost,
                ..
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn evaluating_a_script_the_plugin_never_loaded_names_the_plugin_bug() {
    // The standalone lock plugin loads only `LOCK_SCRIPTS` (DESIGN.md §3.5), so
    // a cache script reaching it is a wiring mistake inside the plugin, not an
    // operator or server condition.
    let executor = FakeExecutor::new(EvalOutcome::Ok);
    let cache = load_catalog(&executor, LOCK_SCRIPTS)
        .await
        .expect("the lock catalog loads");

    let err = eval(&executor, &cache, &CACHE_SCRIPTS[0], "k", &[], &signals())
        .await
        .expect_err("an unloaded script must not be evaluated");
    let ClusterError::Provider { kind, message } = err else {
        panic!("expected a Provider error");
    };
    assert_eq!(kind, ProviderErrorKind::Other);
    assert!(
        message.contains(CACHE_SCRIPTS[0].name),
        "the error must name the script, got {message}"
    );
    assert_eq!(
        executor.evalsha_count(),
        0,
        "nothing must reach the server on a cache miss"
    );
}

#[test]
fn every_catalogued_script_declares_exactly_one_key() {
    // DESIGN.md §6's Cluster-correctness invariant, read back out of the Lua
    // source so a future two-key script fails here rather than in production
    // with `CROSSSLOT`.
    let expected: BTreeSet<usize> = [1].into_iter().collect();
    for script in ALL_SCRIPTS {
        assert_eq!(
            script.declared_key_indices(),
            expected,
            "`{}` must reference KEYS[1] and nothing else",
            script.name
        );
    }
}

#[test]
fn the_key_index_scan_actually_sees_a_second_key() {
    // Guards the guard: an invariant check that cannot fail is worse than none,
    // because it reads as coverage.
    let two_key = ScriptSpec {
        name: "hypothetical",
        source: "redis.call('SET', KEYS[1], redis.call('GET', KEYS[2]))",
    };
    assert_eq!(
        two_key.declared_key_indices(),
        [1, 2].into_iter().collect::<BTreeSet<usize>>()
    );
}

#[test]
fn the_catalog_halves_partition_the_whole() {
    // The standalone lock plugin loads `LOCK_SCRIPTS` and the combined plugin
    // loads `ALL_SCRIPTS`; a script present in neither half would be loaded by
    // the combined plugin and silently missing from the standalone one.
    let all: Vec<&str> = ALL_SCRIPTS.iter().map(|s| s.name).collect();
    let halves: Vec<&str> = CACHE_SCRIPTS
        .iter()
        .chain(LOCK_SCRIPTS)
        .map(|s| s.name)
        .collect();
    assert_eq!(all, halves);
    assert_eq!(
        all.iter().collect::<BTreeSet<_>>().len(),
        all.len(),
        "script names are cache keys and must be unique"
    );
}

#[test]
fn no_script_source_uses_a_blocking_or_nondeterministic_command() {
    // TESTING.md §6's "no blocking commands" rule reaches Lua too, where the
    // compile-time guard of leaving `i-lists` out of fred's feature list does
    // not apply: a script body is a string. `TIME`/`RANDOMKEY` are here for a
    // different reason — a script that read either would be non-deterministic,
    // which is what makes a script unsafe to replicate.
    for script in ALL_SCRIPTS {
        for forbidden in [
            "BLPOP",
            "BRPOP",
            "BLMOVE",
            "BLMPOP",
            "WAIT",
            "TIME",
            "RANDOMKEY",
        ] {
            assert!(
                !script.source.contains(forbidden),
                "`{}` must not call {forbidden}",
                script.name
            );
        }
    }
}

#[test]
fn no_script_source_issues_a_keyspace_wide_command() {
    // The Lua half of TESTING.md §6's `KEYS`/`FLUSHALL`/`FLUSHDB` rule, whose
    // Rust half is `static_analysis_tests.rs`. Split because the two need
    // different matchers, not because the rule differs: a bare `KEYS` cannot be
    // searched for in a script body, since `KEYS[1]` is Lua's *argument global*
    // and appears — correctly — in every script in the catalog. The `redis.call`
    // form is what distinguishes issuing the command from indexing the global.
    //
    // Every script here is single-key by design (DESIGN.md §6, asserted
    // structurally by `every_catalogued_script_declares_exactly_one_key`), so a
    // keyspace-wide command inside one would additionally break the Cluster
    // routing invariant that makes `CROSSSLOT` unreachable.
    for script in ALL_SCRIPTS {
        for command in ["KEYS", "FLUSHALL", "FLUSHDB", "SCAN", "RANDOMKEY"] {
            for quote in ['\'', '"'] {
                let call = format!("redis.call({quote}{command}{quote}");
                assert!(
                    !script.source.contains(&call),
                    "`{}` must not issue {command}: it reaches past the single key the script \
                     declares, which is both a keyspace-wide operation and a Cluster routing \
                     violation",
                    script.name
                );
            }
        }
    }
}
