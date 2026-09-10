//! Removing credential-shaped text before it reaches a log sink.
//!
//! Used by the error mapping in `api/rest` and by the GitHub client in
//! `infra`, so it sits at the crate root rather than inside either.

/// The message with anything credential-shaped taken out.
///
/// Today these strings are the mirror's own - a GitHub path and a status,
/// never an upstream response body - and this keeps that true if a later
/// message quotes more than it should. Three shapes are removed:
///
/// - a token run: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_` or `github_pat_`;
/// - whatever follows an `Authorization` header name or a
///   `Bearer`/`Basic`/`token` scheme, since a credential there carries no
///   prefix of its own. The scheme is looked for inside the word as well as
///   next to it, so `Authorization:Bearer eyJ...` and
///   `Authorization=Bearer%20eyJ...` are caught along with the spaced form;
/// - in a URL, both the query string and any `user:password@` before the
///   host.
#[must_use]
pub fn redacted(msg: &str) -> String {
    // GitHub's own token prefixes: personal, OAuth, user-to-server,
    // server-to-server, refresh, and the fine-grained personal form.
    const SECRET_PREFIXES: [&str; 6] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];
    const CREDENTIAL_INTRODUCERS: [&str; 5] =
        ["Bearer", "bearer", "Basic", "token", "Authorization:"];

    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for word in msg.split_whitespace() {
        // A header or scheme can arrive with no space after it -
        // `Authorization:Bearer eyJ...`, or url-encoded as
        // `Authorization=Bearer%20eyJ...` - so the word is cut on the
        // characters that join a name to its value before matching.
        let parts: Vec<&str> = word
            .split([':', '=', '?', '&'])
            .filter(|part| !part.is_empty())
            .collect();
        let is_introducer = |part: &str| {
            CREDENTIAL_INTRODUCERS
                .iter()
                .any(|introducer| part.eq_ignore_ascii_case(introducer.trim_end_matches(':')))
        };
        let names_a_credential = parts.iter().any(|part| is_introducer(part));
        // Whether the value travels in this word or the next one: the last
        // part being the scheme itself means the value is still to come
        // (`Authorization:Bearer eyJ...`), while anything else means it is
        // already here (`?Authorization=Bearer%20eyJ...`).
        let carries_its_value =
            names_a_credential && parts.last().is_some_and(|part| !is_introducer(part));
        let secret = redact_next
            || names_a_credential
            || SECRET_PREFIXES
                .iter()
                .any(|prefix| word.starts_with(prefix));

        if secret {
            // One `[REDACTED]` for the whole scheme-and-value run, so a
            // reader cannot tell how long the credential was.
            if out.last().map(String::as_str) != Some("[REDACTED]") {
                out.push("[REDACTED]".to_owned());
            }
        } else {
            out.push(redacted_word(word));
        }
        // Only look at the next word when this one named a scheme without
        // supplying the value.
        redact_next = names_a_credential && !carries_its_value;
    }
    out.join(" ")
}

/// One word with its URL secrets removed: the query string, and the
/// `user:password@` an upstream URL can carry before its host.
#[must_use]
pub fn redacted_word(word: &str) -> String {
    let (head, query) = match word.split_once('?') {
        Some((head, _)) => (head, "?[REDACTED]"),
        None => (word, ""),
    };

    // `scheme://userinfo@host/path` - only the part before the first `/` of
    // the path can hold userinfo, so a `@` later in the path is left alone.
    let cleaned = match head.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let separator = if path.is_empty() && !rest.contains('/') {
                ""
            } else {
                "/"
            };
            match authority.rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://[REDACTED]@{host}{separator}{path}"),
                None => head.to_owned(),
            }
        }
        None => head.to_owned(),
    };

    format!("{cleaned}{query}")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in these tests is the failure report"
)]
mod tests {
    use super::{redacted, redacted_word};

    #[test]
    fn an_ordinary_message_survives_intact() {
        assert_eq!(
            redacted("GitHub answered 403 for /repos/acme/widget"),
            "GitHub answered 403 for /repos/acme/widget"
        );
    }

    #[test]
    fn a_query_string_and_a_prefixed_token_are_removed() {
        assert_eq!(
            redacted("GitHub answered 401 for /repos/acme/widget/issues?access_token=ghp_secret"),
            "GitHub answered 401 for /repos/acme/widget/issues?[REDACTED]"
        );
    }

    #[test]
    fn every_github_token_prefix_is_recognised() {
        for token in [
            "ghp_personal",
            "gho_oauth",
            "ghu_usertoserver",
            "ghs_servertoserver",
            "ghr_refresh",
            "github_pat_finegrained",
        ] {
            let out = redacted(&format!("GitHub refused {token} for /repos/acme/widget"));
            assert_eq!(out, "GitHub refused [REDACTED] for /repos/acme/widget");
            assert!(!out.contains(token), "{token} survived: {out}");
        }
    }

    #[test]
    fn a_scheme_takes_the_value_after_it_down_too() {
        for (message, expected) in [
            ("token ghp_abc123 was refused", "[REDACTED] was refused"),
            (
                "Bearer eyJhbGciOi.payload.sig rejected",
                "[REDACTED] rejected",
            ),
            (
                "sent Authorization: Bearer eyJhbGciOi to GitHub",
                "sent [REDACTED] to GitHub",
            ),
            ("Basic dXNlcjpwYXNz denied", "[REDACTED] denied"),
        ] {
            let out = redacted(message);
            assert_eq!(out, expected, "{message:?}");
            for secret in ["ghp_abc123", "eyJhbGciOi", "dXNlcjpwYXNz"] {
                assert!(
                    !out.contains(secret),
                    "{secret} survived redaction of {message:?}: {out}"
                );
            }
        }
    }

    #[test]
    fn a_scheme_joined_to_its_value_is_still_caught() {
        for (message, expected) in [
            (
                "sent Authorization:Bearer eyJhbGciOi to GitHub",
                "sent [REDACTED] to GitHub",
            ),
            (
                "called /repos/acme/widget?Authorization=Bearer%20eyJhbGciOi twice",
                "called [REDACTED] twice",
            ),
            (
                "header authorization:bearer eyJhbGciOi rejected",
                "header [REDACTED] rejected",
            ),
        ] {
            let out = redacted(message);
            assert_eq!(out, expected, "{message:?}");
            assert!(
                !out.contains("eyJhbGciOi"),
                "the value survived redaction of {message:?}: {out}"
            );
        }
    }

    #[test]
    fn url_credentials_and_queries_are_removed() {
        for (word, expected) in [
            (
                "https://user:s3cret@github.example/repos/x",
                "https://[REDACTED]@github.example/repos/x",
            ),
            (
                "https://user:s3cret@github.example?t=1",
                "https://[REDACTED]@github.example?[REDACTED]",
            ),
            (
                "https://api.github.com/repos/acme/widget",
                "https://api.github.com/repos/acme/widget",
            ),
            // A `@` in the path is not userinfo and stays put.
            (
                "https://api.github.com/repos/acme/we@ird",
                "https://api.github.com/repos/acme/we@ird",
            ),
        ] {
            let out = redacted_word(word);
            assert_eq!(out, expected, "{word}");
            assert!(!out.contains("s3cret"), "{word} -> {out}");
        }
    }
}
