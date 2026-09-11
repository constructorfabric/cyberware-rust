//! End-to-end round-trip of the REST transport against a real broker.
//!
//! Ignored: the `event-broker` server crate does not build on this branch (it
//! is ~79 commits behind main, a pre-existing rebase artifact), so the
//! standalone binary this test would spawn cannot be built yet. It also depends
//! on the SDK-type / wire-contract reconciliation flagged in the change (the
//! consumer-group id type, `EventType.partition_key`, etc.). Once the branch is
//! rebased and those are reconciled, drop `#[ignore]` and fill in the broker
//! spawn.

#![cfg(feature = "rest-client")]

use event_broker_sdk::rest::RestBroker;

#[tokio::test]
#[ignore = "needs the branch rebased onto main (server build) and SDK<->wire reconciliation"]
async fn rest_client_round_trips_against_standalone_broker() {
    // Would: start the standalone broker, construct RestBroker against its URL,
    // then publish -> join -> stream and assert the published event is delivered,
    // once for each StreamTransport framing.
    let _broker = RestBroker::new("http://127.0.0.1:8080").expect("build client");
}
