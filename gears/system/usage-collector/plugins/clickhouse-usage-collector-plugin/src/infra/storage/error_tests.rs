use clickhouse::error::Error as ChError;
use usage_collector_sdk::UsageCollectorPluginError;

use super::{acquire_error_clears_readiness, map_ch_err};

#[test]
fn network_error_maps_to_transient() {
    let err = ChError::Network(Box::new(std::io::Error::other("connection refused")));
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Transient { .. }
    ));
}

#[test]
fn timed_out_maps_to_transient() {
    let err = ChError::TimedOut;
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Transient { .. }
    ));
}

#[test]
fn row_not_found_maps_to_internal() {
    let err = ChError::RowNotFound;
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Internal(_)
    ));
}

#[test]
fn bad_response_maps_to_internal() {
    let err = ChError::BadResponse("unexpected response".to_owned());
    assert!(matches!(
        map_ch_err(&err),
        UsageCollectorPluginError::Internal(_)
    ));
}

#[test]
fn network_error_clears_readiness() {
    let err = ChError::Network(Box::new(std::io::Error::other("refused")));
    assert!(acquire_error_clears_readiness(&err));
}

#[test]
fn timed_out_clears_readiness() {
    assert!(acquire_error_clears_readiness(&ChError::TimedOut));
}

#[test]
fn bad_response_does_not_clear_readiness() {
    assert!(!acquire_error_clears_readiness(&ChError::BadResponse(
        "oops".to_owned()
    )));
}
