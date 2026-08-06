//! Turning what partitions want into fetches against the backend. Wiring only.
//!
//! The cache is per `(topic, partition)`; this is per instance. It owns the
//! connection pool, decides which partition gets the next connection, and
//! creates a partition's cache the first time anything asks for it.

pub mod attach;
pub mod backend_source;
pub mod poll;
pub mod scheduler;
pub mod shard;
pub mod sizing;
pub mod source;
pub mod topics;

#[cfg(test)]
mod attach_tests;
#[cfg(test)]
mod poll_tests;
#[cfg(test)]
mod retention_floor_tests;
#[cfg(test)]
mod scheduler_tests;
#[cfg(test)]
mod shard_tests;
#[cfg(test)]
mod sizing_tests;
#[cfg(test)]
mod topics_tests;
