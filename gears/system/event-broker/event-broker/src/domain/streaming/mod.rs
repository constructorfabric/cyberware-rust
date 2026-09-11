//! Server-side consumption-stream pipeline. Wiring only.

pub mod assignment;
pub mod filter;
pub mod frames;
pub mod heartbeat;
pub mod lease;
pub mod progress;
pub mod read;
pub mod read_set;
pub mod reader;
pub mod session;
pub mod source;
pub mod time;

#[cfg(test)]
mod assignment_tests;
#[cfg(test)]
mod filter_tests;
#[cfg(test)]
mod frames_tests;
#[cfg(test)]
mod heartbeat_tests;
#[cfg(test)]
mod lease_tests;
#[cfg(test)]
mod progress_tests;
#[cfg(test)]
mod read_set_tests;
#[cfg(test)]
mod session_tests;
