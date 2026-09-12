// Created: 2026-06-03 by Constructor Tech
//! Cache domain types: the versioned entry, consistency class, key-only event,
//! the native features descriptor, the capability requirement enum, the
//! time-to-live, and the write-request bundle.

use std::time::Duration;

/// A cache entry's time-to-live.
///
/// Models the two write outcomes explicitly rather than overloading
/// `Option<Duration>`, where `None` silently means "never expires".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// Expire the entry once this duration elapses.
    Of(Duration),
    /// Keep the entry until it is explicitly removed; it never expires.
    Indefinite,
}

impl Ttl {
    /// The TTL as an `Option<Duration>`: `Some` for [`Ttl::Of`], `None` for
    /// [`Ttl::Indefinite`]. Lets a backend that stores an optional expiry convert
    /// at its boundary in one call.
    #[must_use]
    pub fn as_duration(self) -> Option<Duration> {
        match self {
            Self::Of(duration) => Some(duration),
            Self::Indefinite => None,
        }
    }
}

impl From<Option<Duration>> for Ttl {
    fn from(value: Option<Duration>) -> Self {
        value.map_or(Self::Indefinite, Self::Of)
    }
}

impl From<Ttl> for Option<Duration> {
    fn from(value: Ttl) -> Self {
        value.as_duration()
    }
}

/// The parameters of a cache write — the shared `key + value + ttl` triple of the
/// [`put`](crate::cache::ClusterCacheBackend::put) and
/// [`put_if_absent`](crate::cache::ClusterCacheBackend::put_if_absent) family.
///
/// Bundling them keeps the two signatures aligned and names each field at the
/// call site. `compare_and_swap` is deliberately left as discrete parameters: it
/// carries an `expected_version` and `new_value` that do not fit this shape.
#[derive(Debug, Clone, Copy)]
pub struct PutRequest<'a> {
    /// The key to write.
    pub key: &'a str,
    /// The value bytes to store.
    pub value: &'a [u8],
    /// The entry's time-to-live.
    pub ttl: Ttl,
}

/// A versioned cache value.
///
/// `version` is opaque and monotonically increasing per key, starting at 1;
/// version 0 is reserved as a sentinel and never observed on a stored entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The stored bytes.
    pub value: Vec<u8>,
    /// The monotonic version (`>= 1`).
    pub version: u64,
}

/// The consistency class a cache backend declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheConsistency {
    /// Linearizable reads and writes — required for correctness-sensitive CAS.
    Linearizable,
    /// Eventually consistent — CAS may exhibit split-brain under partition.
    EventuallyConsistent,
}

/// A lightweight, key-only cache mutation notification. It carries no value —
/// the consumer calls `get(key)` for the current value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheEvent {
    /// The key was created or updated.
    Changed {
        /// The affected key.
        key: String,
    },
    /// The key was deleted.
    Deleted {
        /// The affected key.
        key: String,
    },
    /// The key's TTL elapsed and it was removed.
    Expired {
        /// The affected key.
        key: String,
    },
}

impl CacheEvent {
    /// The key this event concerns.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Changed { key } | Self::Deleted { key } | Self::Expired { key } => key,
        }
    }
}

/// Native capability flags a cache backend declares via
/// [`ClusterCacheBackend::features`](crate::cache::ClusterCacheBackend::features).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CacheFeatures {
    /// Whether the backend supports exact-key watches at all. Nearly every
    /// backend does; a backend answers `false` only when it has an operator mode
    /// in which no watch can be served (e.g. redis `watch_mode: disabled`), in
    /// which case [`watch`](crate::cache::ClusterCacheBackend::watch) must return
    /// [`ClusterError::Unsupported`](crate::ClusterError::Unsupported) with
    /// `feature: "watch"`.
    pub watch: bool,
    /// Whether the backend natively supports prefix watches.
    pub prefix_watch: bool,
}

impl CacheFeatures {
    /// Exact watch supported; `prefix_watch` as given. The common case, and the
    /// meaning [`new`](Self::new) has always carried.
    #[must_use]
    pub fn new(prefix_watch: bool) -> Self {
        Self {
            watch: true,
            prefix_watch,
        }
    }

    /// A backend that cannot serve watches at all. `prefix_watch` is forced off
    /// too — a backend that cannot watch one key cannot watch a family of them.
    #[must_use]
    pub fn without_watch() -> Self {
        Self {
            watch: false,
            prefix_watch: false,
        }
    }
}

/// A capability a consumer can require of a cache backend at resolution time.
/// Each variant maps to a concrete backend characteristic check (DESIGN §3.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheCapability {
    /// Require the backend's [`CacheConsistency`] to be `Linearizable`.
    Linearizable,
    /// Require exact-key watch support, so a consumer that needs a reactive feed
    /// is refused at resolution rather than at first `watch()` call.
    Watch,
    /// Require native prefix-watch support.
    PrefixWatch,
}

#[cfg(test)]
mod tests {
    use super::CacheEvent;

    #[test]
    fn event_exposes_affected_key() {
        assert_eq!(
            CacheEvent::Changed {
                key: "k".to_owned()
            }
            .key(),
            "k"
        );
        assert_eq!(
            CacheEvent::Deleted {
                key: "d".to_owned()
            }
            .key(),
            "d"
        );
        assert_eq!(
            CacheEvent::Expired {
                key: "e".to_owned()
            }
            .key(),
            "e"
        );
    }
}
