//! Validation rules for the text values event-broker accepts.
//!
//! One public function per field, each taking only the value. A call site names
//! the rule it wants and cannot pass the wrong bound, because it passes no
//! bound; the primitives and the per-field bounds below stay private so a
//! field's contract is written down exactly once.
//!
//! Naming follows the field rather than the underlying grammar - `client_agent`
//! rather than `user_agent` - because a rejection has to name the field that
//! carried the value, and one grammar serves several fields.
//!
//! The bounds mirror the committed schemas under
//! `gears/system/event-broker/docs/schemas/`, which remain the authority: a
//! bound that changes there changes here, and the two are cross-checked by
//! test.

use crate::error::reasons;

/// Printable ASCII, the only range any event-broker text field admits. The
/// event `data` payload is the sole exception and is not validated here - it is
/// the producer's own body, governed by its event type's schema.
const PRINTABLE_ASCII: std::ops::RangeInclusive<u8> = 0x20..=0x7E;

/// Inclusive byte bounds for a text value. Named fields rather than a
/// positional constructor, so a minimum and a maximum cannot be transposed.
struct Bounds {
    min: usize,
    max: usize,
}

/// One field's complete contract: the name a rejection reports, and its bounds.
struct Field {
    name: &'static str,
    bounds: Bounds,
}

const CLIENT_AGENT: Field = Field {
    name: "client_agent",
    bounds: Bounds { min: 1, max: 256 },
};

/// Empty is admitted deliberately: `description` is optional everywhere it
/// appears, so rejecting `""` would make an empty string and an absent field
/// behave differently for no gain.
const DESCRIPTION: Field = Field {
    name: "description",
    bounds: Bounds { min: 0, max: 1024 },
};

const SOURCE: Field = Field {
    name: "source",
    bounds: Bounds { min: 1, max: 256 },
};

const SUBJECT: Field = Field {
    name: "subject",
    bounds: Bounds { min: 1, max: 1024 },
};

/// Which rule a value broke. Two rules, so a caller can tell a value it must
/// re-encode from one it must shorten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Carried a byte outside printable ASCII.
    Encoding,
    /// Fell outside the field's byte bounds.
    Length,
}

impl Rule {
    /// The machine-readable reason this rule reports on the wire.
    #[must_use]
    pub fn as_reason(self) -> &'static str {
        match self {
            Rule::Encoding => reasons::ASCII_ONLY,
            Rule::Length => reasons::FIELD_TOO_LONG,
        }
    }
}

/// A text value that does not satisfy its field's contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFieldError {
    field: &'static str,
    rule: Rule,
    detail: String,
}

impl TextFieldError {
    /// The field that carried the offending value.
    #[must_use]
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// Which of the two rules the value broke.
    #[must_use]
    pub fn rule(&self) -> Rule {
        self.rule
    }

    /// What was wrong, phrased for a caller reading a problem body.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for TextFieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.detail)
    }
}

impl std::error::Error for TextFieldError {}

fn printable_ascii(value: &str) -> bool {
    value.bytes().all(|byte| PRINTABLE_ASCII.contains(&byte))
}

/// Encoding is checked before length, which is what makes the reason
/// actionable: a value that is both mis-encoded and over-long reports the
/// encoding failure, because shortening it would not make it acceptable.
fn check(field: &Field, value: &str) -> Result<(), TextFieldError> {
    if !printable_ascii(value) {
        return Err(TextFieldError {
            field: field.name,
            rule: Rule::Encoding,
            detail: "must contain only printable ASCII (0x20-0x7E)".to_owned(),
        });
    }

    let len = value.len();
    if len < field.bounds.min || len > field.bounds.max {
        let (min, max) = (field.bounds.min, field.bounds.max);
        return Err(TextFieldError {
            field: field.name,
            rule: Rule::Length,
            detail: format!("must be {min}-{max} bytes, got {len}"),
        });
    }

    Ok(())
}

/// Validates a `client_agent` against the RFC 9110 User-Agent contract the
/// consumer-group, subscription and producer schemas all declare: printable
/// ASCII, 1-256 bytes.
///
/// # Errors
/// Returns [`TextFieldError`] naming `client_agent` when the value carries a
/// byte outside printable ASCII, is empty, or exceeds 256 bytes.
pub fn client_agent(value: &str) -> Result<(), TextFieldError> {
    check(&CLIENT_AGENT, value)
}

/// Validates a resource `description`: printable ASCII, up to 1024 bytes, empty
/// permitted.
///
/// # Errors
/// Returns [`TextFieldError`] naming `description` when the value carries a
/// byte outside printable ASCII or exceeds 1024 bytes.
pub fn description(value: &str) -> Result<(), TextFieldError> {
    check(&DESCRIPTION, value)
}

/// Validates an event's `source`: printable ASCII, 1-256 bytes.
///
/// # Errors
/// Returns [`TextFieldError`] naming `source` when the value carries a byte
/// outside printable ASCII, is empty, or exceeds 256 bytes.
pub fn source(value: &str) -> Result<(), TextFieldError> {
    check(&SOURCE, value)
}

/// Validates an event's `subject`: printable ASCII, 1-1024 bytes.
///
/// # Errors
/// Returns [`TextFieldError`] naming `subject` when the value carries a byte
/// outside printable ASCII, is empty, or exceeds 1024 bytes.
pub fn subject(value: &str) -> Result<(), TextFieldError> {
    check(&SUBJECT, value)
}
