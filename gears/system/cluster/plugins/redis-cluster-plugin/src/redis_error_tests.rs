use super::*;

/// Builds the error `fred` produces for a Redis error *reply*.
///
/// `fred` parses a reply in `protocol::utils::pretty_error`, which recognizes
/// only a handful of leading tokens and leaves everything else as
/// [`ErrorKind::Unknown`] with the reply text intact. These tests reproduce
/// that classification rather than guess at it, so each case says which arm of
/// `pretty_error` it is standing in for.
fn reply(kind: ErrorKind, text: &'static str) -> Error {
    Error::new(kind, text)
}

fn provider_kind(err: &ClusterError) -> ProviderErrorKind {
    match err {
        ClusterError::Provider { kind, .. } => *kind,
        other => panic!("expected a Provider error, got {other:?}"),
    }
}

#[test]
fn client_side_kinds_map_per_the_table() {
    for (kind, expected) in [
        (ErrorKind::IO, ProviderErrorKind::ConnectionLost),
        (ErrorKind::Timeout, ProviderErrorKind::Timeout),
        (ErrorKind::Auth, ProviderErrorKind::AuthFailure),
        (
            ErrorKind::Backpressure,
            ProviderErrorKind::ResourceExhausted,
        ),
    ] {
        let mapped = map_redis_error(Error::new(kind.clone(), "boom"));
        assert_eq!(
            provider_kind(&mapped),
            expected,
            "{kind:?} must map to {expected:?}"
        );
    }
}

#[test]
fn noauth_and_wrongpass_are_auth_failures() {
    // `pretty_error` classifies both of these as `ErrorKind::Auth` from the
    // reply's leading token, which is why `redis_error.rs` has no separate
    // string rule for them — asserted here so that reliance is checked rather
    // than assumed.
    for text in [
        "NOAUTH Authentication required.",
        "WRONGPASS invalid username-password pair or user is disabled.",
    ] {
        let mapped = map_redis_error(reply(ErrorKind::Auth, text));
        assert_eq!(provider_kind(&mapped), ProviderErrorKind::AuthFailure);
    }
}

#[test]
fn oom_is_resource_exhausted_and_therefore_retryable() {
    let mapped = map_redis_error(reply(
        ErrorKind::Unknown,
        "OOM command not allowed when used memory > 'maxmemory'.",
    ));
    assert_eq!(provider_kind(&mapped), ProviderErrorKind::ResourceExhausted);
    assert!(
        mapped.is_retryable(),
        "an OOM clears once memory is reclaimed, so it must classify retryable"
    );
}

#[test]
fn readonly_is_connection_lost() {
    // A demoted primary mid-failover. `ConnectionLost` and not `Other`, because
    // `fred` re-resolves the topology and the next attempt reaches the new
    // primary — the retry semantics are the point of the classification.
    let mapped = map_redis_error(reply(
        ErrorKind::Unknown,
        "READONLY You can't write against a read only replica.",
    ));
    assert_eq!(provider_kind(&mapped), ProviderErrorKind::ConnectionLost);
    assert!(mapped.is_retryable());
}

#[test]
fn transient_server_states_are_resource_exhausted() {
    for (kind, text) in [
        (
            ErrorKind::Unknown,
            "LOADING Redis is loading the dataset in memory",
        ),
        (
            ErrorKind::Unknown,
            "MASTERDOWN Link with MASTER is down and replica-serve-stale-data is set to 'no'.",
        ),
        // `CLUSTERDOWN` is the one server reply `fred` does give a distinct
        // kind (`ErrorKind::Cluster`), so it also pins that the reply-code
        // check runs *before* the kind match — via the kind arm alone it would
        // fall through to `Other`.
        (ErrorKind::Cluster, "CLUSTERDOWN Hash slot not served"),
    ] {
        let mapped = map_redis_error(reply(kind, text));
        assert_eq!(
            provider_kind(&mapped),
            ProviderErrorKind::ResourceExhausted,
            "`{text}` must classify as retryable resource exhaustion"
        );
    }
}

#[test]
fn a_malformed_url_is_a_config_error_and_never_a_provider_error() {
    // `RD-LIFE-004`: an operator reading this should be looking at their YAML,
    // not at their server. Wrapping it as `Provider` would also put it in front
    // of `is_retryable`, and a bad DSN does not become good on retry.
    for kind in [ErrorKind::Url, ErrorKind::Config] {
        let mapped = map_redis_error(Error::new(kind.clone(), "relative URL without a base"));
        assert!(
            matches!(mapped, ClusterError::InvalidConfig { .. }),
            "{kind:?} must map to InvalidConfig, got {mapped:?}"
        );
        assert!(!mapped.is_retryable());
    }
}

#[test]
fn crossslot_is_other_because_it_can_only_be_a_plugin_bug() {
    // Unreachable by construction: every catalogued script declares exactly one
    // key (DESIGN.md §6, asserted in `scripts_tests.rs`). If one ever appears,
    // no retry helps — the script has to change.
    let mapped = map_redis_error(reply(
        ErrorKind::Unknown,
        "CROSSSLOT Keys in request don't hash to the same slot",
    ));
    assert_eq!(provider_kind(&mapped), ProviderErrorKind::Other);
    assert!(!mapped.is_retryable());
}

#[test]
fn a_second_noscript_surfaces_as_other_rather_than_looping() {
    // `scripts::eval` recovers from the first `NOSCRIPT` and does not re-enter
    // the recovery path, so the second one arrives here. DESIGN.md §10 makes it
    // `Other` precisely so it cannot be retried again.
    let mapped = map_redis_error(reply(ErrorKind::Unknown, "NOSCRIPT No matching script."));
    assert_eq!(provider_kind(&mapped), ProviderErrorKind::Other);
}

#[test]
fn an_unrecognized_error_falls_through_to_other() {
    let mapped = map_redis_error(reply(
        ErrorKind::Unknown,
        "ERR unknown command 'NOTACOMMAND'",
    ));
    assert_eq!(provider_kind(&mapped), ProviderErrorKind::Other);
}

#[test]
fn the_underlying_message_is_preserved() {
    // The classification is for machines; the message is the only thing an
    // operator reading a log line has to go on.
    let mapped = map_redis_error(reply(
        ErrorKind::Unknown,
        "OOM command not allowed when used memory > 'maxmemory'.",
    ));
    assert!(
        mapped.to_string().contains("maxmemory"),
        "the server's own text must survive the mapping, got {mapped}"
    );
}

#[test]
fn only_a_noscript_reply_is_classified_as_one() {
    assert!(is_noscript(&reply(
        ErrorKind::Unknown,
        "NOSCRIPT No matching script."
    )));
    // The code has to be the leading token, not merely present: a message that
    // happens to mention the word must not trigger a reload.
    assert!(!is_noscript(&reply(
        ErrorKind::Unknown,
        "ERR the NOSCRIPT case is handled elsewhere"
    )));
    assert!(!is_noscript(&Error::new(ErrorKind::IO, "connection reset")));
}
