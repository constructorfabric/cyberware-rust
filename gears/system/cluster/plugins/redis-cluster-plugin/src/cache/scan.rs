//! `scan_prefix` over `SCAN` (DESIGN.md §4.4).
//!
//! **`KEYS` is never used.** It is O(N) *blocking the whole server*, which on a
//! shared production Redis is an outage rather than a slow query. `SCAN` pages
//! the keyspace instead, and its weak guarantees — keys present for the whole
//! scan are returned at least once, keys added or removed mid-scan may or may
//! not appear — are exactly the contract `PollingPrefixWatch` needs and no
//! stronger, since it diffs sets rather than trusting order or completeness.
//!
//! Two costs worth naming rather than discovering. `MATCH` filters *after* the
//! server has already looked at each key, so the cost scales with the whole
//! keyspace and not the matched subset — which is one of the reasons
//! `watch_mode: publish` (making the native prefix watch available and the
//! polling polyfill unnecessary) is the default. And in cluster mode the scan
//! runs on every primary and concatenates, so a slot migration mid-scan can
//! duplicate or miss a key.

use cluster_sdk::ClusterError;
use fred::clients::Pool;
use fred::types::Key;
use futures_util::TryStreamExt;

use crate::redis_error::map_redis_error;

/// Keys requested per `SCAN` page (DESIGN.md §4.4).
///
/// A hint, not a limit: `COUNT` bounds the work the server does per call, not
/// the number of matches returned. 500 keeps each page's server-side work short
/// enough not to stall other clients on a shared instance, while making the
/// round-trip count for a large keyspace a fraction of the default `COUNT 10`.
const SCAN_COUNT: u32 = 500;

/// Lists the consumer-visible keys under `prefix`.
///
/// `entry_prefix` is the cache's own `<key_prefix>:c:` stem: it is prepended to
/// build the `MATCH` pattern and stripped from every key on the way out, so the
/// caller never sees the plugin's own namespacing.
///
/// Both halves of the pattern are glob-escaped, and the stem for the same reason
/// as the consumer's prefix: it embeds the operator's `key_prefix`, which no
/// validation constrains to a glob-free charset. Escaping only the caller's half
/// would leave `key_prefix: "tenant[a]"` scanning a character class — matching
/// `tenanta:c:…` and missing every key this cache actually wrote. Stripping on
/// the way out uses the *unescaped* stem, because that is what the stored keys
/// carry; only the pattern is escaped.
///
/// # Errors
/// Whatever [`map_redis_error`] makes of a failing `SCAN`.
pub async fn scan_prefix(
    pool: &Pool,
    clustered: bool,
    entry_prefix: &str,
    prefix: &str,
) -> Result<Vec<String>, ClusterError> {
    let pattern = match_pattern(entry_prefix, prefix);
    // `scan` lives on the concrete client rather than on `Pool`, so this picks
    // one connection and runs the whole cursor loop on it. That is what `SCAN`
    // requires anyway: a cursor is meaningful only to the node that issued it,
    // so spreading pages across a pool would restart the scan on each page.
    let client = pool.next();

    // The buffered variants run the cursor loop internally, which is precisely
    // the "until the cursor returns to 0" loop DESIGN.md §4.4 describes —
    // driving `Scanner::next()` by hand here would reimplement `fred`'s pager
    // for no gain, since this function collects every page before returning and
    // so has no use for the backpressure the manual form buys.
    let keys: Vec<Key> = if clustered {
        client
            .scan_cluster_buffered(pattern, Some(SCAN_COUNT), None)
            .try_collect()
            .await
            .map_err(map_redis_error)?
    } else {
        client
            .scan_buffered(pattern, Some(SCAN_COUNT), None)
            .try_collect()
            .await
            .map_err(map_redis_error)?
    };

    Ok(keys
        .iter()
        .filter_map(|key| strip_entry_prefix(entry_prefix, key))
        .collect())
}

/// Strips the plugin's `<key_prefix>:c:` stem from a scanned key, or drops the
/// key when it does not carry one.
///
/// A key that does not match the stem cannot have been returned by this
/// function's own `MATCH` pattern, so reaching this is either a non-UTF-8 key or
/// something outside the plugin's namespace. Dropping it is the conservative
/// answer: the alternative is handing a consumer a key it did not write and
/// cannot address.
fn strip_entry_prefix(entry_prefix: &str, key: &Key) -> Option<String> {
    key.as_str()?.strip_prefix(entry_prefix).map(str::to_owned)
}

/// The `MATCH` pattern covering every entry under `prefix`.
///
/// Split out from [`scan_prefix`] so the escaping rule is reachable from a
/// Layer-1 test without a pool: the cursor loop needs a server, the pattern does
/// not, and the pattern is the part that has been wrong.
fn match_pattern(entry_prefix: &str, prefix: &str) -> String {
    format!("{}{}*", escape_glob(entry_prefix), escape_glob(prefix))
}

/// Escapes the glob metacharacters Redis's `MATCH` recognizes, so a consumer key
/// containing one is matched literally.
///
/// Without this, `scan_prefix("report[2024]")` would be handed to Redis as a
/// character class and match `report2`, `report0`, `report4`, and nothing the
/// caller meant — and `scan_prefix("*")` would return the entire cache. The
/// consumer's key space is opaque to this plugin (DESIGN.md §2.1), so nothing
/// upstream rules these characters out.
///
/// Redis's glob syntax treats `\` as the escape character and gives `*`, `?`,
/// `[`, and `]` their special meanings; escaping the backslash itself first is
/// what keeps a key that legitimately contains one from consuming the next
/// character.
#[must_use]
pub fn escape_glob(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len());
    for ch in pattern.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

// Layer-1 unit tests: glob escaping and prefix stripping, the two pure pieces of
// this file. The cursor loop itself is Layer 3.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_prefix_is_unchanged() {
        assert_eq!(escape_glob("tenant-42/rate-limit"), "tenant-42/rate-limit");
    }

    #[test]
    fn glob_metacharacters_are_escaped() {
        // Without the escape this pattern is a character class, and the scan
        // returns keys the caller never asked about.
        assert_eq!(escape_glob("report[2024]"), r"report\[2024\]");
        assert_eq!(escape_glob("a*b?c"), r"a\*b\?c");
    }

    #[test]
    fn a_lone_star_stops_matching_the_whole_cache() {
        assert_eq!(escape_glob("*"), r"\*");
    }

    #[test]
    fn a_backslash_is_escaped_before_it_can_escape_something_else() {
        // `a\*` unescaped would tell Redis to match a literal `*`; escaped, it
        // matches a backslash followed by anything, which is what the key says.
        assert_eq!(escape_glob(r"a\*"), r"a\\\*");
    }

    #[test]
    fn the_match_pattern_escapes_both_halves() {
        // The operator's `key_prefix` reaches this stem, and nothing validates
        // it against a glob-free charset. Escaping only the consumer's half
        // would leave `key_prefix: "tenant[a]"` scanning a character class:
        // Redis would match `tenanta:c:...` and miss every key this cache
        // actually wrote, so `scan_prefix` would answer "empty" for a populated
        // cache. `LockNames::release_pattern` and `KeyspaceNames::new` escape
        // the same operator prefix for the same reason.
        assert_eq!(
            match_pattern("tenant[a]:c:", "report[2024]"),
            r"tenant\[a\]:c:report\[2024\]*"
        );
    }

    #[test]
    fn a_plain_match_pattern_carries_no_escapes() {
        // The escaping must be invisible to the overwhelmingly common case, or
        // it changes what a default deployment scans.
        assert_eq!(
            match_pattern("cluster:c:", "tenant-42/"),
            "cluster:c:tenant-42/*"
        );
    }

    #[test]
    fn the_plugin_prefix_is_stripped_from_scanned_keys() {
        let key = Key::from("cluster:c:tenant-42/limit");
        assert_eq!(
            strip_entry_prefix("cluster:c:", &key).as_deref(),
            Some("tenant-42/limit")
        );
    }

    #[test]
    fn a_key_outside_the_plugin_namespace_is_dropped() {
        // Unreachable through this module's own MATCH pattern; dropping beats
        // handing a consumer a key it did not write.
        let key = Key::from("someone-elses:key");
        assert_eq!(strip_entry_prefix("cluster:c:", &key), None);
    }
}
