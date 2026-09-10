//! Shape checks on the identifiers a caller supplies.
//!
//! These live in `domain/` rather than in the REST layer because every
//! surface reaches the same repositories and the same GitHub client: the
//! `Service` methods are also called by [`crate::domain::local_client`] and
//! by SDK consumers, which never pass through an axum handler.

use crate::domain::error::DomainError;

/// Longest owner or repository segment GitHub accepts.
const MAX_SEGMENT: usize = 100;

/// Longest commit SHA the mirror accepts: SHA-256 in hex.
const MAX_SHA: usize = 64;

/// GitHub owner and repository names are ASCII letters, digits, `.`, `_`
/// and `-`. The path segments arrive percent-decoded, so anything else -
/// a `?`, `#`, `/`, a quote - could re-shape the URL or GraphQL query the
/// mirror sends to GitHub with its own token; such values are rejected
/// here, before any of them is used.
///
/// # Errors
/// `Validation` naming the offending segment.
pub fn validate_repo_path(owner: &str, name: &str) -> Result<(), DomainError> {
    for (field, value) in [("owner", owner), ("name", name)] {
        let well_formed = !value.is_empty()
            && value != "."
            && value != ".."
            && value.len() <= MAX_SEGMENT
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
        if !well_formed {
            return Err(DomainError::Validation {
                field: field.to_owned(),
                message: format!("must be 1-{MAX_SEGMENT} characters from [A-Za-z0-9._-]"),
            });
        }
    }
    Ok(())
}

/// The `owner/name` key rows are stored under, built only from segments
/// [`validate_repo_path`] accepts.
///
/// # Errors
/// Whatever [`validate_repo_path`] returns.
pub fn repo_full_name(owner: &str, name: &str) -> Result<String, DomainError> {
    validate_repo_path(owner, name)?;
    Ok(format!("{owner}/{name}"))
}

/// A commit SHA is hex: abbreviated, 40 characters for SHA-1 or 64 for
/// SHA-256. It reaches both the outbound GitHub path and a storage lookup,
/// so it is bounded the same way the repository segments are. Any length up
/// to the full hash is accepted - git itself abbreviates - the point is that
/// nothing but hex gets through.
///
/// # Errors
/// `Validation` when the value is empty, over 64 characters, or not hex.
pub fn validate_commit_sha(sha: &str) -> Result<(), DomainError> {
    let well_formed =
        !sha.is_empty() && sha.len() <= MAX_SHA && sha.bytes().all(|b: u8| b.is_ascii_hexdigit());
    if well_formed {
        Ok(())
    } else {
        Err(DomainError::Validation {
            field: "sha".to_owned(),
            message: format!("must be 1-{MAX_SHA} hexadecimal characters"),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_segments_pass() {
        assert!(validate_repo_path("rust-lang", "rust.git_1").is_ok());
        assert_eq!(
            repo_full_name("rust-lang", "rust").unwrap(),
            "rust-lang/rust"
        );
    }

    #[test]
    fn dot_segments_and_separators_are_rejected() {
        for (owner, name) in [
            ("..", "rust"),
            (".", "rust"),
            ("", "rust"),
            ("rust-lang", "rust/issues"),
            ("rust-lang", "rust?x=1"),
            ("rust lang", "rust"),
        ] {
            assert!(
                validate_repo_path(owner, name).is_err(),
                "{owner}/{name} must be rejected"
            );
            assert!(repo_full_name(owner, name).is_err());
        }
    }

    #[test]
    fn an_over_long_segment_is_rejected() {
        let long = "a".repeat(MAX_SEGMENT + 1);
        assert!(validate_repo_path(&long, "rust").is_err());
        assert!(validate_repo_path("rust-lang", &long).is_err());
    }

    #[test]
    fn hex_shas_pass_and_anything_else_does_not() {
        assert!(validate_commit_sha("aaa").is_ok());
        assert!(validate_commit_sha("abc1234").is_ok());
        assert!(validate_commit_sha(&"a".repeat(40)).is_ok());
        assert!(validate_commit_sha(&"0".repeat(64)).is_ok());

        for bad in ["", "../../etc", "main", "abc/def", &"a".repeat(65)] {
            assert!(
                validate_commit_sha(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }
}
