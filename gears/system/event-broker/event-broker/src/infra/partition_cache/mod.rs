//! Per-`(topic, partition)` segment cache and the runway allocator that sizes
//! it. Wiring only.

pub mod accounting;
pub mod budget;
pub mod cache;
pub mod demand;
pub mod reclaim;
pub mod runway;
pub mod segment;
pub mod span;

#[cfg(test)]
mod accounting_tests;
#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod concurrency_tests;
#[cfg(test)]
mod convergence_tests;
#[cfg(test)]
mod demand_tests;
#[cfg(test)]
mod reclaim_tests;
#[cfg(test)]
mod residency_tests;
#[cfg(test)]
mod runway_tests;
#[cfg(test)]
mod segment_tests;
#[cfg(test)]
mod span_tests;
