//! Shared parsing for the `:action` colon-suffix path segments
//! (`{id}:reset`, `{id}:seek`) that axum/matchit can't route as a literal
//! path template mixed with a path param - `producers::reset_producer` and
//! `subscriptions::seek_subscription` both register a bare `{id}` instead
//! and parse the suffix themselves.

use uuid::Uuid;

use crate::domain::error::DomainError;

/// Splits `raw` (the whole `<uuid>:<action>` path segment) into its `Uuid`,
/// rejecting anything that isn't `<uuid>:<action>` exactly.
///
/// `resource_noun` (e.g. `"producer"`, `"subscription"`) drives both error
/// messages: the expected-shape message uses `<{resource_noun}_id>` as its
/// placeholder, the invalid-UUID message reads "not a valid `{resource_noun}`
/// id".
#[toolkit_macros::temporary(
    tracking = "gears-rust#4463",
    reason = "axum's pinned matchit can't mix a path param with literal text \
              in one segment, so `{id}:action` routes register a bare `{id}` \
              and this helper splits the suffix by hand instead; matchit \
              0.9.2 (tracked by #4463) adds real support for this"
)]
pub fn parse_action_suffixed_id(
    raw: &str,
    action: &str,
    resource_noun: &str,
) -> Result<Uuid, DomainError> {
    let id_str =
        raw.strip_suffix(&format!(":{action}"))
            .ok_or_else(|| DomainError::Validation {
                code: "InvalidPath",
                message: format!("expected '<{resource_noun}_id>:{action}', got '{raw}'"),
            })?;
    Uuid::parse_str(id_str).map_err(|_| DomainError::Validation {
        code: "InvalidPath",
        message: format!("'{id_str}' is not a valid {resource_noun} id"),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_action_suffixed_id;
    use crate::domain::error::DomainError;

    #[test]
    fn accepts_a_well_formed_suffixed_id() {
        let id = uuid::Uuid::new_v4();
        let raw = format!("{id}:reset");
        assert_eq!(
            parse_action_suffixed_id(&raw, "reset", "producer").unwrap(),
            id
        );
    }

    #[test]
    fn rejects_a_missing_suffix() {
        let id = uuid::Uuid::new_v4();
        let err = parse_action_suffixed_id(&id.to_string(), "reset", "producer").unwrap_err();
        assert!(matches!(
            err,
            DomainError::Validation {
                code: "InvalidPath",
                ..
            }
        ));
    }

    #[test]
    fn rejects_a_malformed_uuid() {
        let err = parse_action_suffixed_id("not-a-uuid:seek", "seek", "subscription").unwrap_err();
        assert!(matches!(
            err,
            DomainError::Validation {
                code: "InvalidPath",
                ..
            }
        ));
    }
}
