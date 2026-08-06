//! What an operator writes under a topic's `backend.options` for this backend.
//!
//! One key: where the event log lives. The database this backend opens is its
//! own, separate from the one the platform hands the host gear - that database
//! keeps ingest and delivery metadata, and the event log is not metadata. So
//! the location is operator configuration rather than something inherited, and
//! the durability of a topic's events is a deployment choice.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The `path` value that asks for an event log with no file behind it.
const IN_MEMORY: &str = ":memory:";

/// This backend's whole configuration.
///
/// Unknown keys are rejected rather than ignored: a misspelled key would
/// otherwise silently leave the event log wherever the default puts it, which
/// for this backend means silently discarding it on restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SqliteBackendOptions {
    /// The database this backend keeps its event log in: a filesystem path, or
    /// `:memory:` for a log that lives only as long as the process.
    ///
    /// A leading `~/` expands to the current user's home directory. Missing
    /// parent directories are created. Defaults to `:memory:` - see
    /// [`EventLogPath`] for why there is no file default.
    pub path: EventLogPath,
}

/// Where the event log lives.
///
/// The default is in memory, and deliberately not a file: nothing tells this
/// backend where a deployment keeps its data. The platform resolves the gear
/// database's location from a home directory a gear never sees, so any file
/// default would be a location the operator did not choose - the process's
/// working directory, or a path this crate invented. An event log with nowhere
/// to live says so by not surviving the process, and a deployment that wants
/// durability names a path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum EventLogPath {
    /// `:memory:` - the log lives as long as the process and no longer.
    #[default]
    InMemory,
    /// A `SQLite` file. Created, with its parent directories, if missing.
    File(PathBuf),
}

impl From<String> for EventLogPath {
    fn from(raw: String) -> Self {
        if raw.trim() == IN_MEMORY {
            return Self::InMemory;
        }
        Self::File(expand_home(&raw))
    }
}

impl EventLogPath {
    /// The DSN `toolkit_db` opens this location with.
    pub(crate) fn dsn(&self) -> String {
        match self {
            Self::InMemory => "sqlite::memory:".to_owned(),
            // `mode=rwc` is what creates the file on first boot; without it a
            // fresh deployment fails to open a database it was asked to make.
            Self::File(path) => format!("sqlite://{}?mode=rwc", url_path(path)),
        }
    }

    /// Whether this location is shared by every connection that opens it.
    ///
    /// `SQLite` gives each connection to `:memory:` a private database of its
    /// own, so an in-memory log is only coherent behind a single connection.
    pub(crate) fn is_in_memory(&self) -> bool {
        matches!(self, Self::InMemory)
    }
}

impl std::fmt::Display for EventLogPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InMemory => f.write_str(IN_MEMORY),
            Self::File(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A leading `~/` replaced by the current user's home directory.
///
/// The same shorthand the platform's own `home_dir` setting accepts, so a path
/// written beside it in the same configuration file behaves the same way. A
/// bare `~` with nothing after it, and a `~user` form, are left alone: neither
/// is a path this expands.
fn expand_home(raw: &str) -> PathBuf {
    let Some(rest) = raw.strip_prefix("~/") else {
        return PathBuf::from(raw);
    };
    match std::env::home_dir() {
        Some(home) => home.join(rest),
        None => PathBuf::from(raw),
    }
}

/// A filesystem path as the body of a `sqlite://` URL.
///
/// Absolute paths become `sqlite:///dir/file.db`; a relative one keeps the
/// `./` form `toolkit_db` documents, so it resolves against the working
/// directory rather than being read as a host name. Windows drive paths keep
/// their prefix behind the third slash.
fn url_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if raw.starts_with('/') {
        return raw;
    }
    if is_drive_qualified(&raw) {
        return format!("/{raw}");
    }
    format!("./{}", raw.trim_start_matches("./"))
}

/// `C:/data/eb.db` and friends - a Windows path that is already absolute.
fn is_drive_qualified(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
#[path = "options_tests.rs"]
mod options_tests;
