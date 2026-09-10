use super::types::OutboxError;

pub const DEFAULT_OUTBOX_TABLE_PREFIX: &str = "toolkit_outbox";
pub const DEFAULT_OUTBOX_MIGRATION_NAME: &str = "m001_create_toolkit_outbox_schema";

const MAX_IDENTIFIER_LEN: usize = 63;
const MAX_PREFIX_LEN: usize = 36;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxTables {
    prefix: String,
    body: String,
    partitions: String,
    incoming: String,
    outgoing: String,
    dead_letters: String,
    processor: String,
    vacuum_counter: String,
    trace: String,
    body_id_sequence: String,
    incoming_id_sequence: String,
    idx_incoming_partition: String,
    idx_incoming_body_id: String,
    idx_outgoing_partition_seq: String,
    idx_outgoing_body_id: String,
    idx_dl_replayable: String,
    idx_dl_status_deadline: String,
    idx_dl_status_failed: String,
    idx_trace_mail: String,
    idx_trace_stalled: String,
    idx_trace_key: String,
    migration_name: String,
}

impl Default for OutboxTables {
    fn default() -> Self {
        match Self::new(DEFAULT_OUTBOX_TABLE_PREFIX) {
            Ok(tables) => tables,
            Err(err) => panic!("default outbox prefix is invalid: {err}"),
        }
    }
}

impl OutboxTables {
    pub(crate) fn new(prefix: impl Into<String>) -> Result<Self, OutboxError> {
        let prefix = prefix.into();
        validate_prefix(&prefix)?;

        let tables = Self {
            body: suffixed(&prefix, "body"),
            partitions: suffixed(&prefix, "partitions"),
            incoming: suffixed(&prefix, "incoming"),
            outgoing: suffixed(&prefix, "outgoing"),
            dead_letters: suffixed(&prefix, "dead_letters"),
            processor: suffixed(&prefix, "processor"),
            vacuum_counter: suffixed(&prefix, "vacuum_counter"),
            trace: suffixed(&prefix, "trace"),
            body_id_sequence: suffixed(&prefix, "body_id_sequence"),
            incoming_id_sequence: suffixed(&prefix, "incoming_id_sequence"),
            idx_incoming_partition: indexed(&prefix, "incoming_partition"),
            idx_incoming_body_id: indexed(&prefix, "incoming_body_id"),
            idx_outgoing_partition_seq: indexed(&prefix, "outgoing_partition_seq"),
            idx_outgoing_body_id: indexed(&prefix, "outgoing_body_id"),
            idx_dl_replayable: indexed(&prefix, "dl_replayable"),
            idx_dl_status_deadline: indexed(&prefix, "dl_status_deadline"),
            idx_dl_status_failed: indexed(&prefix, "dl_status_failed"),
            idx_trace_mail: indexed(&prefix, "trace_mail"),
            idx_trace_stalled: indexed(&prefix, "trace_stalled"),
            idx_trace_key: indexed(&prefix, "trace_key"),
            migration_name: migration_name(&prefix),
            prefix,
        };

        tables.validate_derived_identifiers()?;
        Ok(tables)
    }

    #[allow(dead_code)]
    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }

    pub(crate) fn partitions(&self) -> &str {
        &self.partitions
    }

    pub(crate) fn incoming(&self) -> &str {
        &self.incoming
    }

    pub(crate) fn outgoing(&self) -> &str {
        &self.outgoing
    }

    pub(crate) fn dead_letters(&self) -> &str {
        &self.dead_letters
    }

    pub(crate) fn processor(&self) -> &str {
        &self.processor
    }

    pub(crate) fn vacuum_counter(&self) -> &str {
        &self.vacuum_counter
    }

    pub(crate) fn trace(&self) -> &str {
        &self.trace
    }

    pub(crate) fn body_id_sequence(&self) -> &str {
        &self.body_id_sequence
    }

    pub(crate) fn incoming_id_sequence(&self) -> &str {
        &self.incoming_id_sequence
    }

    #[allow(dead_code)]
    pub(crate) fn table_names(&self) -> [&str; 8] {
        [
            self.body(),
            self.partitions(),
            self.incoming(),
            self.outgoing(),
            self.dead_letters(),
            self.processor(),
            self.vacuum_counter(),
            self.trace(),
        ]
    }

    pub(crate) fn idx_incoming_partition(&self) -> &str {
        &self.idx_incoming_partition
    }

    pub(crate) fn idx_incoming_body_id(&self) -> &str {
        &self.idx_incoming_body_id
    }

    pub(crate) fn idx_outgoing_partition_seq(&self) -> &str {
        &self.idx_outgoing_partition_seq
    }

    pub(crate) fn idx_outgoing_body_id(&self) -> &str {
        &self.idx_outgoing_body_id
    }

    pub(crate) fn idx_dl_replayable(&self) -> &str {
        &self.idx_dl_replayable
    }

    pub(crate) fn idx_dl_status_deadline(&self) -> &str {
        &self.idx_dl_status_deadline
    }

    pub(crate) fn idx_dl_status_failed(&self) -> &str {
        &self.idx_dl_status_failed
    }

    pub(crate) fn idx_trace_mail(&self) -> &str {
        &self.idx_trace_mail
    }

    pub(crate) fn idx_trace_stalled(&self) -> &str {
        &self.idx_trace_stalled
    }

    pub(crate) fn idx_trace_key(&self) -> &str {
        &self.idx_trace_key
    }

    pub(crate) fn migration_name(&self) -> &str {
        &self.migration_name
    }

    fn validate_derived_identifiers(&self) -> Result<(), OutboxError> {
        for ident in [
            self.body(),
            self.partitions(),
            self.incoming(),
            self.outgoing(),
            self.dead_letters(),
            self.processor(),
            self.vacuum_counter(),
            self.trace(),
            self.body_id_sequence(),
            self.incoming_id_sequence(),
            self.idx_incoming_partition(),
            self.idx_incoming_body_id(),
            self.idx_outgoing_partition_seq(),
            self.idx_outgoing_body_id(),
            self.idx_dl_replayable(),
            self.idx_dl_status_deadline(),
            self.idx_dl_status_failed(),
            self.idx_trace_mail(),
            self.idx_trace_stalled(),
            self.idx_trace_key(),
        ] {
            if ident.len() > MAX_IDENTIFIER_LEN {
                return Err(OutboxError::InvalidTablePrefix {
                    reason: "derived table and index names must fit 63 bytes",
                });
            }
        }
        Ok(())
    }
}

fn validate_prefix(prefix: &str) -> Result<(), OutboxError> {
    let reason = |reason| Err(OutboxError::InvalidTablePrefix { reason });

    if prefix.is_empty() || prefix.len() > MAX_PREFIX_LEN {
        return reason("must be 1-36 bytes");
    }

    let bytes = prefix.as_bytes();
    if !bytes[0].is_ascii_alphabetic() {
        return reason("must start with an ASCII letter");
    }

    for &b in bytes {
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return reason("must contain only ASCII alphanumerics and '_'");
        }
    }

    Ok(())
}

fn suffixed(prefix: &str, suffix: &str) -> String {
    format!("{prefix}_{suffix}")
}

fn indexed(prefix: &str, suffix: &str) -> String {
    format!("idx_{prefix}_{suffix}")
}

fn migration_name(prefix: &str) -> String {
    if prefix == DEFAULT_OUTBOX_TABLE_PREFIX {
        DEFAULT_OUTBOX_MIGRATION_NAME.to_owned()
    } else {
        format!("{DEFAULT_OUTBOX_MIGRATION_NAME}__{prefix}")
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn default_tables_match_existing_names() {
        let tables = OutboxTables::default();

        assert_eq!(tables.prefix(), "toolkit_outbox");
        assert_eq!(tables.body(), "toolkit_outbox_body");
        assert_eq!(tables.partitions(), "toolkit_outbox_partitions");
        assert_eq!(tables.incoming(), "toolkit_outbox_incoming");
        assert_eq!(tables.outgoing(), "toolkit_outbox_outgoing");
        assert_eq!(tables.dead_letters(), "toolkit_outbox_dead_letters");
        assert_eq!(tables.processor(), "toolkit_outbox_processor");
        assert_eq!(tables.vacuum_counter(), "toolkit_outbox_vacuum_counter");
        assert_eq!(tables.body_id_sequence(), "toolkit_outbox_body_id_sequence");
        assert_eq!(
            tables.incoming_id_sequence(),
            "toolkit_outbox_incoming_id_sequence"
        );
    }

    #[test]
    fn default_indexes_match_existing_names() {
        let tables = OutboxTables::default();

        assert_eq!(
            tables.idx_incoming_partition(),
            "idx_toolkit_outbox_incoming_partition"
        );
        assert_eq!(
            tables.idx_incoming_body_id(),
            "idx_toolkit_outbox_incoming_body_id"
        );
        assert_eq!(
            tables.idx_outgoing_partition_seq(),
            "idx_toolkit_outbox_outgoing_partition_seq"
        );
        assert_eq!(
            tables.idx_outgoing_body_id(),
            "idx_toolkit_outbox_outgoing_body_id"
        );
        assert_eq!(
            tables.idx_dl_replayable(),
            "idx_toolkit_outbox_dl_replayable"
        );
        assert_eq!(
            tables.idx_dl_status_deadline(),
            "idx_toolkit_outbox_dl_status_deadline"
        );
        assert_eq!(
            tables.idx_dl_status_failed(),
            "idx_toolkit_outbox_dl_status_failed"
        );
    }

    #[test]
    fn custom_tables_are_derived_from_prefix() {
        let tables = OutboxTables::new("mini_chat_outbox").unwrap();

        assert_eq!(tables.body(), "mini_chat_outbox_body");
        assert_eq!(tables.partitions(), "mini_chat_outbox_partitions");
        assert_eq!(tables.incoming(), "mini_chat_outbox_incoming");
        assert_eq!(tables.outgoing(), "mini_chat_outbox_outgoing");
        assert_eq!(tables.dead_letters(), "mini_chat_outbox_dead_letters");
        assert_eq!(tables.processor(), "mini_chat_outbox_processor");
        assert_eq!(tables.vacuum_counter(), "mini_chat_outbox_vacuum_counter");
        assert_eq!(
            tables.body_id_sequence(),
            "mini_chat_outbox_body_id_sequence"
        );
        assert_eq!(
            tables.incoming_id_sequence(),
            "mini_chat_outbox_incoming_id_sequence"
        );
        assert_eq!(
            tables.idx_outgoing_partition_seq(),
            "idx_mini_chat_outbox_outgoing_partition_seq"
        );
    }

    #[test]
    fn sequence_tables_handle_prefix_containing_default_table_token() {
        let tables = OutboxTables::new("toolkit_outbox_body").unwrap();

        assert_eq!(tables.body(), "toolkit_outbox_body_body");
        assert_eq!(
            tables.body_id_sequence(),
            "toolkit_outbox_body_body_id_sequence"
        );
        assert_eq!(
            tables.incoming_id_sequence(),
            "toolkit_outbox_body_incoming_id_sequence"
        );
    }

    #[test]
    fn custom_migration_name_is_deterministic_and_distinct() {
        let first = OutboxTables::new("mini_chat_outbox").unwrap();
        let second = OutboxTables::new("mini_chat_outbox").unwrap();

        assert_eq!(first.migration_name(), second.migration_name());
        assert_ne!(first.migration_name(), DEFAULT_OUTBOX_MIGRATION_NAME);
        assert_eq!(
            first.migration_name(),
            "m001_create_toolkit_outbox_schema__mini_chat_outbox"
        );
    }

    #[test]
    fn default_migration_name_is_preserved() {
        let tables = OutboxTables::default();

        assert_eq!(tables.migration_name(), DEFAULT_OUTBOX_MIGRATION_NAME);
    }

    #[test]
    fn valid_prefixes_are_accepted() {
        for prefix in ["a", "outbox1", "toolkit_outbox", "mini_chat_outbox"] {
            assert!(OutboxTables::new(prefix).is_ok(), "{prefix}");
        }
    }

    #[test]
    fn invalid_prefixes_are_rejected() {
        for prefix in [
            "",
            "1outbox",
            "mini chat",
            "mini-chat",
            "public.outbox",
            "outbox;",
            "outbox\"",
            "outbox'",
            "_outbox",
            "\u{0434}\u{0430}\u{043d}\u{043d}\u{044b}\u{0435}",
        ] {
            assert!(OutboxTables::new(prefix).is_err(), "{prefix}");
        }
    }

    #[test]
    fn boundary_length_prefix_is_accepted() {
        let prefix = "a".repeat(MAX_PREFIX_LEN);

        assert!(OutboxTables::new(prefix).is_ok());
    }

    #[test]
    fn overlong_prefix_is_rejected() {
        let prefix = "a".repeat(MAX_PREFIX_LEN + 1);

        assert!(OutboxTables::new(prefix).is_err());
    }
}
