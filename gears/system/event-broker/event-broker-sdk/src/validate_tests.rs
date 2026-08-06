//! Boundary tests for the text-field rules.
//!
//! Every assertion is on the whole error value - field, rule and detail - so a
//! test says exactly what a caller would see rather than only that something
//! failed.

use crate::validate::{Rule, TextFieldError, client_agent, description, source, subject};

fn err(result: Result<(), TextFieldError>) -> TextFieldError {
    match result {
        Ok(()) => panic!("expected the value to be rejected"),
        Err(error) => error,
    }
}

// -- client_agent: printable ASCII, 1-256 bytes ---------------------------

#[test]
fn client_agent_accepts_a_single_byte() {
    assert_eq!(client_agent("x"), Ok(()));
}

#[test]
fn client_agent_accepts_exactly_the_maximum() {
    assert_eq!(client_agent(&"x".repeat(256)), Ok(()));
}

#[test]
fn client_agent_rejects_one_byte_over_the_maximum() {
    let error = err(client_agent(&"x".repeat(257)));
    assert_eq!(error.field(), "client_agent");
    assert_eq!(error.rule(), Rule::Length);
    assert_eq!(error.detail(), "must be 1-256 bytes, got 257");
    assert_eq!(error.rule().as_reason(), "field_too_long");
}

#[test]
fn client_agent_rejects_empty() {
    let error = err(client_agent(""));
    assert_eq!(error.field(), "client_agent");
    assert_eq!(error.rule(), Rule::Length);
    assert_eq!(error.detail(), "must be 1-256 bytes, got 0");
}

#[test]
fn client_agent_rejects_an_embedded_control_byte() {
    let error = err(client_agent("agent\u{1}/1.0"));
    assert_eq!(error.field(), "client_agent");
    assert_eq!(error.rule(), Rule::Encoding);
    assert_eq!(
        error.detail(),
        "must contain only printable ASCII (0x20-0x7E)"
    );
    assert_eq!(error.rule().as_reason(), "ascii_only");
}

#[test]
fn client_agent_rejects_del() {
    // 0x7F sits one past the printable range, and is exactly what
    // `str::is_ascii` admits - the looseness this rule replaces.
    let error = err(client_agent("agent\u{7f}"));
    assert_eq!(error.field(), "client_agent");
    assert_eq!(error.rule(), Rule::Encoding);
    assert_eq!(
        error.detail(),
        "must contain only printable ASCII (0x20-0x7E)"
    );
}

#[test]
fn client_agent_rejects_a_multi_byte_character() {
    let error = err(client_agent("agent-\u{e9}"));
    assert_eq!(error.field(), "client_agent");
    assert_eq!(error.rule(), Rule::Encoding);
    assert_eq!(
        error.detail(),
        "must contain only printable ASCII (0x20-0x7E)"
    );
}

#[test]
fn client_agent_reports_encoding_before_length() {
    // 200 two-byte characters: 200 characters, 400 bytes, so both rules are
    // broken. Encoding is reported because shortening the value would not
    // make it acceptable.
    //
    // This is also why the byte-versus-character distinction is not directly
    // observable: among values that satisfy the encoding rule, every byte is
    // one character, so the two counts cannot diverge. Measuring bytes is
    // what keeps the rule aligned with the schema's stated units, not a
    // behaviour a caller can provoke on its own.
    let error = err(client_agent(&"\u{e9}".repeat(200)));
    assert_eq!(error.field(), "client_agent");
    assert_eq!(error.rule(), Rule::Encoding);
}

#[test]
fn client_agent_accepts_a_realistic_agent_string() {
    assert_eq!(
        client_agent("cf-gears-event-broker-sdk/0.2.1 (linux)"),
        Ok(())
    );
}

// -- description: printable ASCII, 0-1024 bytes ---------------------------

#[test]
fn description_accepts_empty() {
    // Optional everywhere it appears, so "" and an absent field agree.
    assert_eq!(description(""), Ok(()));
}

#[test]
fn description_accepts_exactly_the_maximum() {
    assert_eq!(description(&"d".repeat(1024)), Ok(()));
}

#[test]
fn description_rejects_one_byte_over_the_maximum() {
    let error = err(description(&"d".repeat(1025)));
    assert_eq!(error.field(), "description");
    assert_eq!(error.rule(), Rule::Length);
    assert_eq!(error.detail(), "must be 0-1024 bytes, got 1025");
}

#[test]
fn description_rejects_a_multi_byte_character_and_a_control_byte() {
    // The pair scenario consumer/groups/1.06 sends.
    let error = err(description("caf\u{e9}\u{2} orders"));
    assert_eq!(error.field(), "description");
    assert_eq!(error.rule(), Rule::Encoding);
    assert_eq!(
        error.detail(),
        "must contain only printable ASCII (0x20-0x7E)"
    );
    assert_eq!(error.rule().as_reason(), "ascii_only");
}

// -- source: printable ASCII, 1-256 bytes ---------------------------------

#[test]
fn source_accepts_exactly_the_maximum() {
    assert_eq!(source(&"s".repeat(256)), Ok(()));
}

#[test]
fn source_rejects_one_byte_over_the_maximum() {
    let error = err(source(&"s".repeat(257)));
    assert_eq!(error.field(), "source");
    assert_eq!(error.rule(), Rule::Length);
    assert_eq!(error.detail(), "must be 1-256 bytes, got 257");
}

#[test]
fn source_rejects_empty() {
    let error = err(source(""));
    assert_eq!(error.field(), "source");
    assert_eq!(error.rule(), Rule::Length);
    assert_eq!(error.detail(), "must be 1-256 bytes, got 0");
}

#[test]
fn source_rejects_a_newline() {
    // A newline is a control byte, so a header-splitting value cannot pass.
    let error = err(source("orders\nservice"));
    assert_eq!(error.field(), "source");
    assert_eq!(error.rule(), Rule::Encoding);
}

// -- subject: printable ASCII, 1-1024 bytes -------------------------------

#[test]
fn subject_accepts_exactly_the_maximum() {
    assert_eq!(subject(&"j".repeat(1024)), Ok(()));
}

#[test]
fn subject_rejects_one_byte_over_the_maximum() {
    let error = err(subject(&"j".repeat(1025)));
    assert_eq!(error.field(), "subject");
    assert_eq!(error.rule(), Rule::Length);
    assert_eq!(error.detail(), "must be 1-1024 bytes, got 1025");
}

#[test]
fn subject_rejects_empty() {
    let error = err(subject(""));
    assert_eq!(error.field(), "subject");
    assert_eq!(error.rule(), Rule::Length);
    assert_eq!(error.detail(), "must be 1-1024 bytes, got 0");
}

#[test]
fn subject_bound_is_wider_than_source() {
    // The schemas differ deliberately - 1024 against 256 - so a value legal as
    // a subject can be illegal as a source. A shared function would have had
    // to pick one.
    let value = "j".repeat(512);
    assert_eq!(subject(&value), Ok(()));
    assert_eq!(err(source(&value)).rule(), Rule::Length);
}

// -- the schemas are the authority: bounds here must equal bounds there ----
//
// A `validate` function and the committed schema declaring the same field are
// two statements of one contract. These tests read the schema off disk and
// probe the validator at the boundaries it names, so a bound edited on one
// side and not the other fails here rather than in production.

fn committed_schema(file: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docs/schemas")
        .join(file);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("committed schema is valid JSON")
}

/// A field's declared contract, read from the schema rather than restated.
struct Declared {
    min: usize,
    max: usize,
    pattern: String,
}

fn declared(node: &serde_json::Value) -> Declared {
    let as_usize = |key: &str, fallback: usize| {
        node.get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(fallback)
    };
    Declared {
        min: as_usize("minLength", 0),
        max: as_usize("maxLength", 0),
        pattern: node
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

/// Probes a validator against a field's declared bounds: the maximum is
/// accepted, one byte past it is a `Length` rejection, and a byte below the
/// minimum is too when the minimum is above zero.
fn agrees_with(
    field: &Declared,
    validate: fn(&str) -> Result<(), TextFieldError>,
    expect_empty_ok: bool,
) {
    assert!(field.max > 0, "schema declares no maxLength");
    assert_eq!(validate(&"x".repeat(field.max)), Ok(()));
    assert_eq!(
        err(validate(&"x".repeat(field.max + 1))).rule(),
        Rule::Length
    );
    if expect_empty_ok {
        assert_eq!(
            field.min, 0,
            "schema minLength disagrees: empty is accepted"
        );
        assert_eq!(validate(""), Ok(()));
    } else {
        assert_eq!(
            field.min, 1,
            "schema minLength disagrees: empty is rejected"
        );
        assert_eq!(err(validate("")).rule(), Rule::Length);
    }
    // Both printable-ASCII spellings differ only in whether they admit empty.
    let expected = if expect_empty_ok {
        r"^[\x20-\x7E]*$"
    } else {
        r"^[\x20-\x7E]+$"
    };
    assert_eq!(field.pattern, expected);
}

#[test]
fn client_agent_agrees_with_the_consumer_group_schema() {
    let schema = committed_schema("gts.cf.core.events.consumer_group.v1~.schema.json");
    let node = &schema["definitions"]["CreateRequest"]["properties"]["client_agent"];
    agrees_with(&declared(node), client_agent, false);
}

#[test]
fn client_agent_agrees_with_the_subscription_schema() {
    // The same field on another resource - the bound must not have drifted
    // between the two schemas that declare it.
    let schema = committed_schema("gts.cf.core.events.subscription.v1~.schema.json");
    let node = &schema["properties"]["client_agent"];
    agrees_with(&declared(node), client_agent, false);
}

#[test]
fn description_agrees_with_the_consumer_group_schema() {
    let schema = committed_schema("gts.cf.core.events.consumer_group.v1~.schema.json");
    let node = &schema["definitions"]["CreateRequest"]["properties"]["description"];
    agrees_with(&declared(node), description, true);
}

#[test]
fn description_agrees_with_the_topic_schema() {
    let schema = committed_schema("gts.cf.core.events.topic.v1~.schema.json");
    let node = &schema["properties"]["description"];
    agrees_with(&declared(node), description, true);
}

#[test]
fn source_agrees_with_the_event_schema() {
    let schema = committed_schema("gts.cf.core.events.event.v1~.schema.json");
    let node = &schema["properties"]["source"];
    // `source` declares no `minLength`, but its `+` pattern rejects empty, so
    // the validator's 1-byte floor is what the schema means rather than what
    // it spells.
    let field = declared(node);
    assert_eq!(field.max, 256);
    assert_eq!(field.pattern, r"^[\x20-\x7E]+$");
    assert_eq!(source(&"x".repeat(256)), Ok(()));
    assert_eq!(err(source(&"x".repeat(257))).rule(), Rule::Length);
    assert_eq!(err(source("")).rule(), Rule::Length);
}

#[test]
fn subject_agrees_with_the_event_schema() {
    let schema = committed_schema("gts.cf.core.events.event.v1~.schema.json");
    let node = &schema["properties"]["subject"];
    agrees_with(&declared(node), subject, false);
}
