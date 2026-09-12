//! REST `Link`-header pagination.
//!
//! GitHub paginates list endpoints with an RFC 5988 `Link` header:
//!
//! ```text
//! Link: <https://api.github.com/...&page=2>; rel="next",
//!       <https://api.github.com/...&page=34>; rel="last"
//! ```
//!
//! Following `rel="next"` until it disappears is the only way to see past the
//! first page; without it the mirror silently stores whatever fits in one.
//!
//! The same signal answers the other question the sync needs: a listing whose
//! last response advertises no `rel="next"` was walked to its end, and only
//! such a complete listing may be used to reconcile upstream deletions.

/// The URL marked `rel="next"`, if the header advertises one.
///
/// Returns `None` on the last page, on an empty header, and on cursor-based
/// endpoints that omit the relation.
#[must_use]
pub fn parse_link_next(header: &str) -> Option<String> {
    parse_link_rel(header, "next")
}

/// The URL marked with `rel`, if the header advertises one.
#[must_use]
pub fn parse_link_rel(header: &str, rel: &str) -> Option<String> {
    let quoted = format!("rel=\"{rel}\"");
    let bare = format!("rel={rel}");
    for part in header.split(',') {
        let Some((url_part, params)) = part.trim().split_once(';') else {
            continue;
        };
        let url = url_part
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        if url.is_empty() {
            continue;
        }
        // One segment may carry several `; key="value"` parameters.
        let matches_rel = params.split(';').any(|p| {
            let p = p.trim();
            p == quoted || p == bare
        });
        if matches_rel {
            return Some(url.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_link_is_extracted() {
        let header = "<https://api.github.com/repos/o/r/issues?page=2>; rel=\"next\", \
                      <https://api.github.com/repos/o/r/issues?page=34>; rel=\"last\"";
        assert_eq!(
            parse_link_next(header).as_deref(),
            Some("https://api.github.com/repos/o/r/issues?page=2")
        );
    }

    #[test]
    fn the_last_page_advertises_no_next() {
        let header = "<https://api.github.com/repos/o/r/issues?page=1>; rel=\"prev\", \
                      <https://api.github.com/repos/o/r/issues?page=1>; rel=\"first\"";
        assert_eq!(parse_link_next(header), None);
        assert_eq!(parse_link_next(""), None);
    }

    #[test]
    fn whitespace_and_an_unquoted_rel_are_tolerated() {
        let header = "  <https://api.github.com/x?page=5> ; rel=next ";
        assert_eq!(
            parse_link_next(header).as_deref(),
            Some("https://api.github.com/x?page=5")
        );
    }

    #[test]
    fn a_segment_with_several_parameters_still_matches() {
        let header = "<https://api.github.com/x?page=2>; type=\"text/html\"; rel=\"next\"";
        assert_eq!(
            parse_link_next(header).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
    }
}
