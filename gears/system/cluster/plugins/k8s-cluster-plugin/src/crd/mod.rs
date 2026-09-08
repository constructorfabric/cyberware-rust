//! The `ClusterCacheEntry` custom resource (DESIGN.md §2.7): the
//! `#[derive(CustomResource)]` type and its projection to/from `CacheEntry`.

mod cache_entry;

pub use cache_entry::{ClusterCacheEntry, ClusterCacheEntrySpec};
