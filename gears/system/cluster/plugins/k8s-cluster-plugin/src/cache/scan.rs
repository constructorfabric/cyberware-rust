//! Paginated `scan_prefix` over the cache keyspace (DESIGN.md §6.4).
//!
//! A `LIST` with the cache label selector, paginated at 500 (`ListParams::limit` +
//! continue), with the prefix matched **client-side** on each object's recovered key
//! and expired entries excluded on the read path (§6.2, §6.4). The per-object
//! decision — does this entry's key match the prefix, and is it still live? — is the
//! pure, tested piece ([`live_matching_key`]); the pagination I/O is Phase 6.

use k8s_openapi::jiff::Timestamp;

use crate::crd::ClusterCacheEntry;

use super::is_expired;
use super::watch::key_of;

/// One page size for the paginated scan (§6.4).
pub const LIST_PAGE: u32 = 500;

/// The entry's cache key if it matches `prefix` and is still live at `now`, else
/// `None` (§6.4).
///
/// Filters on the recovered key (not the mapped object name), so the prefix a
/// consumer passes matches the keys it wrote; and drops entries past their
/// `expiresAt`, so a scan never returns a key that `get` would report absent.
#[must_use]
pub fn live_matching_key(
    entry: &ClusterCacheEntry,
    prefix: &str,
    now: Timestamp,
) -> Option<String> {
    let key = key_of(entry)?;
    if !key.starts_with(prefix) {
        return None;
    }
    if is_expired(entry.spec.expires_at.as_deref(), now) {
        return None;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::live_matching_key;
    use crate::crd::{ClusterCacheEntry, ClusterCacheEntrySpec};
    use crate::naming::ANNOTATION_NAME;
    use k8s_openapi::jiff::{SignedDuration, Timestamp};
    use std::collections::BTreeMap;

    fn entry(key: &str, expires_at: Option<String>) -> ClusterCacheEntry {
        let mut e = ClusterCacheEntry::new("obj", ClusterCacheEntrySpec::new(b"v", 1, expires_at));
        e.metadata.annotations = Some(BTreeMap::from([(
            ANNOTATION_NAME.to_owned(),
            key.to_owned(),
        )]));
        e
    }

    #[test]
    fn matches_only_the_prefix() {
        let now = Timestamp::from_second(1_000).unwrap();
        assert_eq!(
            live_matching_key(&entry("shard/7", None), "shard/", now),
            Some("shard/7".to_owned())
        );
        assert_eq!(
            live_matching_key(&entry("other/1", None), "shard/", now),
            None
        );
        // An empty prefix matches every key (the "scan all" case).
        assert_eq!(
            live_matching_key(&entry("anything", None), "", now),
            Some("anything".to_owned())
        );
    }

    #[test]
    fn excludes_expired_entries() {
        let now = Timestamp::from_second(1_000).unwrap();
        let past = (now - SignedDuration::from_secs(1)).to_string();
        let future = (now + SignedDuration::from_secs(60)).to_string();
        // Past deadline → excluded even though the prefix matches.
        assert_eq!(
            live_matching_key(&entry("shard/7", Some(past)), "shard/", now),
            None
        );
        // Future deadline → included.
        assert_eq!(
            live_matching_key(&entry("shard/7", Some(future)), "shard/", now),
            Some("shard/7".to_owned())
        );
    }

    #[test]
    fn an_unkeyed_object_is_skipped() {
        let now = Timestamp::from_second(1_000).unwrap();
        // No name annotation → not one of our keys.
        let bare = ClusterCacheEntry::new("obj", ClusterCacheEntrySpec::new(b"v", 1, None));
        assert_eq!(live_matching_key(&bare, "", now), None);
    }
}
