//! `IngestService`-backed REST handlers (`DESIGN.md:579-583`): publish,
//! producers, topics, event-types catalog reads.

pub mod dto;
pub mod event_types;
pub mod events;
pub mod producers;
pub mod topics;

#[cfg(test)]
#[path = "events_tests.rs"]
mod events_tests;

#[cfg(test)]
#[path = "producers_tests.rs"]
mod producers_tests;

#[cfg(test)]
#[path = "topics_tests.rs"]
mod topics_tests;

#[cfg(test)]
#[path = "event_types_tests.rs"]
mod event_types_tests;
