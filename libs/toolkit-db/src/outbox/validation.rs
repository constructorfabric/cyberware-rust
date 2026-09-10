//! Every rule the outbox applies to caller-supplied input, one named function
//! per field.
//!
//! A rejection names the field and the rule it broke. It never reproduces the
//! submitted value: an error body travels into logs, aggregators and bug
//! reports that the original submission was never meant to reach, and the
//! caller already knows what it sent. A derived measurement is not the value
//! and may be reported.

use super::types::OutboxError;
use toolkit_utils::byte_size::KIB_LEN;

/// Maximum queue name length (fits VARCHAR(1024) column).
const MAX_QUEUE_NAME_LEN: usize = 1024;

/// Maximum payload type length.
const MAX_PAYLOAD_TYPE_LEN: usize = 1024;

/// Maximum trace length (fits VARCHAR(256) column).
const MAX_TRACE_LEN: usize = 256;

/// Maximum payload size in bytes.
pub const MAX_PAYLOAD_SIZE: usize = 64 * KIB_LEN;

/// Validate a queue name: `[a-zA-Z0-9._-]{1,1024}`, must start and end with
/// alphanumeric.
pub fn validate_queue_name(name: &str) -> Result<(), OutboxError> {
    let reason = |reason| Err(OutboxError::InvalidQueueName { reason });

    if name.is_empty() || name.len() > MAX_QUEUE_NAME_LEN {
        return reason("must be 1-1024 bytes");
    }

    let bytes = name.as_bytes();

    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return reason("must start and end with an ASCII alphanumeric");
    }

    for &b in bytes {
        if !(b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-') {
            return reason("must contain only ASCII alphanumerics, '.', '_' and '-'");
        }
    }

    Ok(())
}

/// Validate a payload type: 1-1024 printable ASCII chars (`0x20..=0x7E`).
pub fn validate_payload_type(payload_type: &str) -> Result<(), OutboxError> {
    let reason = |reason| Err(OutboxError::InvalidPayloadType { reason });

    if payload_type.is_empty() || payload_type.len() > MAX_PAYLOAD_TYPE_LEN {
        return reason("must be 1-1024 bytes");
    }

    for &b in payload_type.as_bytes() {
        if !(0x20..=0x7E).contains(&b) {
            return reason("must contain only printable ASCII (0x20-0x7E)");
        }
    }

    Ok(())
}

/// Validate a trace: 1-256 printable ASCII chars (`0x20..=0x7E`).
pub fn validate_trace(trace: &str) -> Result<(), OutboxError> {
    let reason = |reason| Err(OutboxError::InvalidTrace { reason });

    if trace.is_empty() {
        return reason("must not be empty");
    }

    if trace.len() > MAX_TRACE_LEN {
        return Err(OutboxError::TraceTooLong {
            size: trace.len(),
            max: MAX_TRACE_LEN,
        });
    }

    for &b in trace.as_bytes() {
        if !(0x20..=0x7E).contains(&b) {
            return reason("must contain only printable ASCII (0x20-0x7E)");
        }
    }

    Ok(())
}

/// Validate a payload against the per-message size cap.
pub fn validate_payload(payload: &[u8]) -> Result<(), OutboxError> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(OutboxError::PayloadTooLarge {
            size: payload.len(),
            max: MAX_PAYLOAD_SIZE,
        });
    }

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // --- Queue name: valid ---

    #[test]
    fn queue_name_simple() {
        assert!(validate_queue_name("orders").is_ok());
    }

    #[test]
    fn queue_name_with_dots_and_dashes() {
        assert!(validate_queue_name("orders.v2").is_ok());
        assert!(validate_queue_name("my-queue").is_ok());
        assert!(validate_queue_name("my_queue").is_ok());
    }

    #[test]
    fn queue_name_single_char() {
        assert!(validate_queue_name("a").is_ok());
        assert!(validate_queue_name("9").is_ok());
    }

    #[test]
    fn queue_name_1024_chars() {
        let name = "a".repeat(1024);
        assert!(validate_queue_name(&name).is_ok());
    }

    #[test]
    fn queue_name_mixed_case() {
        assert!(validate_queue_name("OrderEvents").is_ok());
    }

    // --- Queue name: invalid ---

    #[test]
    fn queue_name_empty() {
        assert!(validate_queue_name("").is_err());
    }

    #[test]
    fn queue_name_too_long() {
        let name = "a".repeat(1025);
        assert!(validate_queue_name(&name).is_err());
    }

    #[test]
    fn queue_name_starts_with_dot() {
        assert!(validate_queue_name(".orders").is_err());
    }

    #[test]
    fn queue_name_ends_with_dash() {
        assert!(validate_queue_name("orders-").is_err());
    }

    #[test]
    fn queue_name_null_byte() {
        assert!(validate_queue_name("orders\0evil").is_err());
    }

    #[test]
    fn queue_name_spaces() {
        assert!(validate_queue_name("my queue").is_err());
    }

    #[test]
    fn queue_name_unicode() {
        assert!(validate_queue_name("\u{0437}\u{0430}\u{043a}\u{0430}\u{0437}\u{044b}").is_err());
    }

    #[test]
    fn queue_name_slashes() {
        assert!(validate_queue_name("orders/v2").is_err());
    }

    // --- Payload type: valid ---

    #[test]
    fn payload_type_simple() {
        assert!(validate_payload_type("json").is_ok());
    }

    #[test]
    fn payload_type_mime_style() {
        assert!(validate_payload_type("application/json").is_ok());
        assert!(validate_payload_type("application/json;orders.created.v1").is_ok());
    }

    #[test]
    fn payload_type_1024_chars() {
        let pt = "a".repeat(1024);
        assert!(validate_payload_type(&pt).is_ok());
    }

    // --- Payload type: invalid ---

    #[test]
    fn payload_type_empty() {
        assert!(validate_payload_type("").is_err());
    }

    #[test]
    fn payload_type_too_long() {
        let pt = "a".repeat(1025);
        assert!(validate_payload_type(&pt).is_err());
    }

    #[test]
    fn payload_type_null_byte() {
        assert!(validate_payload_type("json\0").is_err());
    }

    #[test]
    fn payload_type_newline() {
        assert!(validate_payload_type("json\n").is_err());
    }

    #[test]
    fn payload_type_control_char() {
        assert!(validate_payload_type("json\x01").is_err());
    }

    #[test]
    fn payload_type_non_ascii() {
        assert!(validate_payload_type("\u{0434}\u{0430}\u{043d}\u{043d}\u{044b}\u{0435}").is_err());
    }

    // --- Trace ---

    #[test]
    fn trace_simple() {
        assert!(validate_trace("order-4711").is_ok());
        assert!(validate_trace("import 2026-09-08 #3").is_ok());
    }

    #[test]
    fn trace_256_bytes() {
        assert!(validate_trace(&"a".repeat(256)).is_ok());
    }

    #[test]
    fn trace_empty() {
        assert!(validate_trace("").is_err());
    }

    #[test]
    fn trace_too_long_reports_the_measurement() {
        let err = validate_trace(&"a".repeat(257)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "trace size 257 exceeds maximum 256",
            "the rejection states the measurement, not the value"
        );
    }

    #[test]
    fn trace_non_printable() {
        assert!(validate_trace("order\n4711").is_err());
        assert!(validate_trace("order\u{0000}").is_err());
        assert!(validate_trace("\u{0437}\u{0430}\u{043a}\u{0430}\u{0437}").is_err());
    }

    // --- Rejections carry the rule, never the value ---

    #[test]
    fn rejections_do_not_reproduce_the_input() {
        let secret = "orders/../../etc/passwd?token=hunter2";
        let queue = validate_queue_name(secret).unwrap_err().to_string();
        assert!(
            !queue.contains("hunter2"),
            "queue rejection echoed the input"
        );

        let payload_type = validate_payload_type("json\n\u{0000}hunter2")
            .unwrap_err()
            .to_string();
        assert!(
            !payload_type.contains("hunter2"),
            "payload-type rejection echoed the input"
        );

        let trace = validate_trace("trace\n\u{0000}hunter2")
            .unwrap_err()
            .to_string();
        assert!(
            !trace.contains("hunter2"),
            "trace rejection echoed the input"
        );
    }

    // --- Payload size ---

    #[test]
    fn payload_at_the_cap() {
        assert!(validate_payload(&vec![0u8; MAX_PAYLOAD_SIZE]).is_ok());
    }

    #[test]
    fn payload_over_the_cap() {
        let err = validate_payload(&vec![0u8; MAX_PAYLOAD_SIZE + 1]).unwrap_err();
        assert_eq!(err.to_string(), "payload size 65537 exceeds maximum 65536");
    }
}
