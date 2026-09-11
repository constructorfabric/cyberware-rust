//! Optional HTTP gateway (`DESIGN.md:610-612`). Not constructed in
//! standalone mode (`docs/ADR/0007-service-decomposition.md` D4). Resolves
//! and forwards requests to ingest/delivery instances in `cluster_dispatcher`
//! mode (`eb-dispatcher-routing`); per-instance selection algorithm
//! (topic-pattern matching, cache-based consumer-group ownership, failover)
//! is out of scope - see that change's design.md D1; #4438 owns it.

mod forward;
pub mod proxy;
mod proxy_client;
pub mod router;

pub use forward::DispatcherState;
