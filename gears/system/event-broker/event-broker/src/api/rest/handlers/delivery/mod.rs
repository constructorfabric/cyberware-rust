//! `DeliveryService`-backed REST handlers (`DESIGN.md:581,584-585`):
//! streaming, consumer groups, subscriptions.

pub mod consumer_groups;
pub mod dto;
pub mod encode;
pub mod streaming;
pub mod subscriptions;

#[cfg(test)]
#[path = "consumer_groups_tests.rs"]
mod consumer_groups_tests;

#[cfg(test)]
#[path = "subscriptions_tests.rs"]
mod subscriptions_tests;

#[cfg(test)]
#[path = "streaming_tests.rs"]
mod streaming_tests;

#[cfg(test)]
#[path = "encode_tests.rs"]
mod encode_tests;

#[cfg(test)]
#[path = "dto_tests.rs"]
mod dto_tests;
