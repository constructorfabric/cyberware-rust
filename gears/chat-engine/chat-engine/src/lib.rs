//! Chat Engine module — public crate surface.
//!
//! The wire-level types live in `cf-chat-engine-sdk`; this crate consumes
//! the SDK and exposes:
//!
//! - [`ChatEngineModule`] — the `#[toolkit::module]`-annotated entrypoint
//!   used by `cf-gears-example-server` via the `inventory`-based
//!   registrator.
//! - The re-exported SDK types every downstream test / consumer needs so a
//!   single `use chat_engine::*;` import suffices.
//!
//! Internal modules (`api`, `config`, `domain`, `infra`) are marked
//! `#[doc(hidden)]` so they do not pollute the public docs surface; they
//! remain `pub` because the integration tests in `tests/` reach into them.
//
// @cpt-cf-chat-engine-public-surface:p15

#![allow(clippy::module_name_repetitions)]
#![allow(clippy::struct_field_names)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::similar_names)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cognitive_complexity)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::if_not_else)]
#![allow(clippy::unnested_or_patterns)]
#![allow(clippy::single_match_else)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::empty_structs_with_brackets)]
#![allow(clippy::ifs_same_cond)]
#![allow(clippy::trivially_copy_pass_by_ref)]
#![allow(clippy::format_push_string)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::redundant_clone)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::implicit_hasher)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::if_then_some_else_none)]
#![allow(clippy::str_to_string)]
#![allow(clippy::needless_late_init)]
#![allow(clippy::unused_self)]
#![allow(clippy::used_underscore_binding)]
#![allow(clippy::inconsistent_struct_constructor)]
#![allow(clippy::branches_sharing_code)]
#![allow(clippy::useless_let_if_seq)]

#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;

pub mod module;

pub use module::ChatEngineModule;

// Re-export the chat-engine-sdk public surface so downstream phases (and external
// consumers) can depend on a single `chat_engine::*` import for domain types.
pub use chat_engine_sdk::{
    Capability, CapabilityValue, ChatEngineBackendPlugin, HealthStatus, LifecycleState,
    MemoryStrategy, Message, MessagePluginCtx, MessageRole, PluginCallContext, PluginError,
    PluginStream, RetentionPolicy, Session, SessionPluginCtx, SessionType, StreamingChunkEvent,
    StreamingCompleteEvent, StreamingErrorEvent, StreamingEvent, StreamingStartEvent, TenantId,
    UserId, VariantInfo, empty_stream, stream_from_events,
};
