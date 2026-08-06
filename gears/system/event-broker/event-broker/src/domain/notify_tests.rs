//! The notification key: derived from the identifier alone, and legal where a
//! GTS identifier is not.

use toolkit_gts::GtsInstanceId;

use super::{NOTIFICATION_PREFIX, notification_key};

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.notify.topic.v1";
const OTHER: &str = "gts.cf.core.events.topic.v1~x.eb.notify.other.v1";

fn topic(raw: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(raw).expect("test id is a valid GTS instance id")
}

/// The property the whole scheme rests on: no coordination. Two instances hold
/// nothing in common but the identifier, and must still write and watch the
/// same key - so the key is a function of the identifier and nothing else.
#[test]
fn one_topic_yields_one_key_wherever_it_is_derived() {
    assert_eq!(
        notification_key(&topic(TOPIC), 3),
        notification_key(&topic(TOPIC), 3),
        "no database and no cache is consulted, so there is nothing to disagree about"
    );
    assert_ne!(
        notification_key(&topic(TOPIC), 3),
        notification_key(&topic(OTHER), 3),
        "two topics must not share a key"
    );
    assert_ne!(
        notification_key(&topic(TOPIC), 3),
        notification_key(&topic(TOPIC), 4),
        "two partitions of one topic must not share a key"
    );
}

/// `ClusterCacheV1` admits `[a-zA-Z0-9_-]+(/[a-zA-Z0-9_-]+)*` only, which is
/// why the identifier itself cannot be the key: it carries `.` and `~`.
#[test]
fn the_key_holds_only_characters_the_cache_admits() {
    let key = notification_key(&topic(TOPIC), 7).expect("a valid topic yields a key");

    assert!(
        key.starts_with(&format!("{NOTIFICATION_PREFIX}/")),
        "the shared prefix is what delivery watches: {key}"
    );
    assert!(
        key.ends_with("/7"),
        "the partition is the last segment: {key}"
    );
    assert!(
        key.split('/').all(|segment| !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')),
        "every segment must satisfy the cache's key grammar: {key}"
    );
    assert!(
        !key.contains('.') && !key.contains('~'),
        "the two characters a GTS identifier would have brought: {key}"
    );
}
