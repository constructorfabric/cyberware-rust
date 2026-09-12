//! `ClickHouse` connection-pool bootstrap and schema migration.
//!
//! Exposes five entry points:
//! - [`build_client`] — constructs and configures the `clickhouse::Client`.
//! - `configure_insert` — applies this plugin's per-`INSERT` timeouts and
//!   settings (including the insert dedup token) to a freshly acquired
//!   `Insert` handle.
//! - [`apply_migrations`] — runs the embedded DDL against the connected
//!   `ClickHouse` instance.
//! - [`ensure_retention_ttl`] — reconciles `usage_records` TTL with config.
//! - [`ensure_insert_dedup_window`] — retrofits the `usage_records` insert
//!   dedup window onto tables created before the migration carried it.

use std::time::Duration;

use anyhow::Context as _;
use percent_encoding::percent_decode_str;
use url::Url;

use crate::config::{ClickHousePluginConfig, is_plaintext_url};

/// Endpoint components split out of a `database_url` for the `clickhouse`
/// crate's connection-configuration methods.
///
/// The `clickhouse` 0.15.x crate's `Client::with_url` stores the given string
/// verbatim as the base request URL — every query/insert does
/// `Url::parse(&client.url)` and only clears/rewrites the *query string*
/// (`query_pairs_mut().clear()`); the URL's **path** and **userinfo** pass
/// straight through unchanged (see `clickhouse::query::do_execute` /
/// `insert_formatted`). So a `database_url` like
/// `http://user:pass@host:8123/mydb` sent as-is to `with_url` would request
/// the literal HTTP path `/mydb` on every call — which `ClickHouse`'s HTTP
/// interface rejects with "There is no handle /mydb..." (only `/`, `/ping`,
/// etc. are implemented) — while silently discarding the credentials, since
/// `with_url` never touches `Client::authentication` / `Client::database`.
/// Those must instead be extracted here and applied via
/// `with_user`/`with_password`/`with_database`, matching this plugin's own
/// documented config contract (README: `database_url = "https://user:pass@host:8443/db"`).
struct ParsedEndpoint {
    /// Bare scheme + host + port, no userinfo/path/query — safe to pass to
    /// `Client::with_url`.
    base_url: String,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
}

/// Split a `database_url` into a bare HTTP endpoint plus optional
/// user/password/database.
///
/// Username and password come from the URL's userinfo and are
/// percent-decoded before being returned: `Url::username` / `Url::password`
/// yield the encoded form, but `Client::with_user` / `with_password` need the
/// literal credentials. Callers embedding `${VAR}`-expanded secrets that
/// contain URL-reserved characters must still percent-encode them in
/// `database_url` so the URL parses. The database name is the URL path with
/// leading/trailing slashes trimmed; an empty path yields `None`
/// (`ClickHouse` then uses the server's default database for the resolved
/// user).
///
/// # Errors
///
/// Returns [`url::ParseError`] if `database_url` is not a valid absolute URL.
fn parse_endpoint(database_url: &str) -> Result<ParsedEndpoint, url::ParseError> {
    let mut url = Url::parse(database_url)?;

    let user = (!url.username().is_empty()).then(|| decode_userinfo(url.username()));
    let password = url.password().map(decode_userinfo);
    let database = {
        let path = url.path().trim_matches('/');
        (!path.is_empty()).then(|| path.to_owned())
    };

    // Strip userinfo/path/query so the remaining string is a bare endpoint
    // safe to hand to `Client::with_url` (see struct-level doc comment).
    // `set_username`/`set_password` only fail for schemes that cannot carry
    // credentials (e.g. `file:`); an http(s) URL that parsed successfully
    // above always accepts them, so the `Result` is deliberately ignored.
    url.set_username("").ok();
    url.set_password(None).ok();
    url.set_path("");
    url.set_query(None);

    Ok(ParsedEndpoint {
        base_url: url.to_string(),
        user,
        password,
        database,
    })
}

/// Percent-decode a URL userinfo component for `with_user` / `with_password`.
///
/// Invalid UTF-8 after decoding is lossily replaced — credentials are opaque
/// bytes at the wire level, but the `clickhouse` client takes `&str`.
fn decode_userinfo(encoded: &str) -> String {
    percent_decode_str(encoded).decode_utf8_lossy().into_owned()
}

/// Embedded schema migration SQL.
///
/// Relative path from this file (`src/infra/storage/pool.rs`) three levels
/// up to the crate root, then into `migrations/`.
pub(crate) const MIGRATION_SQL: &str = include_str!("../../../migrations/0001_init.sql");

/// Default `usage_records` TTL baked into [`MIGRATION_SQL`] (1 year in seconds).
pub(crate) const DEFAULT_RETENTION_SECS: u64 = 365 * 86_400;

/// Build a configured `clickhouse::Client` from the plugin config.
///
/// The DSN (embedding credentials) is unwrapped from [`SecretFromEnv`] only
/// here at the connection boundary — it is never logged or stored past this
/// call. `ClickHouse` credentials and usage data are always sent in
/// cleartext when the URL scheme is `http://`; [`ClickHousePluginConfig::validate`]
/// (called by `Gear::init` before this function) already fails closed on a
/// plaintext `database_url` unless `allow_insecure_http` is explicitly set,
/// so a call reaching here with a `http://` URL has already had that
/// override deliberately enabled. This function still emits a
/// [`tracing::warn!`] in that case — defense-in-depth observability for any
/// call path that does not route through `validate()` first, and a durable
/// operator-visible signal every time an insecure connection is actually
/// made.
///
/// Timeout configuration is forwarded via `ClickHouse` session settings
/// `send_timeout` and `receive_timeout` (both bound to
/// `cfg.request_timeout_secs`).  The `clickhouse` 0.15.x `Client` is a
/// lightweight handle over an internal `hyper` connection pool.
///
/// Settings attached here ride on **every** request this client makes,
/// `SELECT`s included. Settings that must apply to writes only are attached
/// per-statement by `configure_insert` instead.
pub fn build_client(cfg: &ClickHousePluginConfig) -> clickhouse::Client {
    let url = cfg.database_url.expose();

    // TLS posture check — mirrors the reference plugin's sslmode-warn pattern.
    // Reaching this branch means `allow_insecure_http` was explicitly set
    // (validate() already rejected an unqualified `http://` database_url);
    // `https://` (the production default) requires no warning.
    if is_plaintext_url(url) {
        tracing::warn!(
            "connecting to `ClickHouse` with http:// scheme: credentials and usage \
             data are sent in cleartext. Use https:// for encrypted transport \
             in production."
        );
    }

    // On the production path this branch is unreachable: `Gear::init` always
    // calls `ClickHousePluginConfig::validate` before `build_client`, and
    // `validate` rejects a `database_url` that `Url::parse` cannot handle. It
    // only defends call sites that construct a client without validating first
    // (direct unit/integration use of `build_client`), where returning an inert
    // client is preferable to panicking.
    //
    // The resulting client cannot connect anywhere: the `clickhouse` crate
    // re-parses `Client::url` on every request, so the same parse failure
    // surfaces as `Error::InvalidParams` before any socket is opened — no
    // request is sent and no credentials leave the process.
    let endpoint = parse_endpoint(url).unwrap_or_else(|err| {
        tracing::warn!(
            error = %err,
            "database_url failed to parse as a URL; every ClickHouse request will fail with \
             an invalid-params error before any connection is attempted"
        );
        ParsedEndpoint {
            base_url: url.to_owned(),
            user: None,
            password: None,
            database: None,
        }
    });

    // `send_timeout` and `receive_timeout` are standard `ClickHouse` HTTP API
    // settings accepted as URL query parameters or per-request headers.
    // The `clickhouse` crate passes them as request headers on every query.
    // Both are set to `request_timeout_secs` — the crate exposes a single
    // combined timeout knob rather than separate send/receive splits.
    let timeout_str = cfg.request_timeout_secs.to_string();

    let mut client = clickhouse::Client::default()
        .with_url(endpoint.base_url)
        .with_setting("send_timeout", &timeout_str)
        .with_setting("receive_timeout", &timeout_str);

    if let Some(user) = endpoint.user {
        client = client.with_user(user);
    }
    if let Some(password) = endpoint.password {
        client = client.with_password(password);
    }
    if let Some(database) = endpoint.database {
        client = client.with_database(database);
    }

    client
}

/// Apply this plugin's per-`INSERT` `ClickHouse` configuration to a freshly
/// acquired `Insert` handle.
///
/// The single place all three insert sites (`record_store`'s single-row and
/// batch writes, `catalog_store`'s type write) go through, so a change to
/// insert-time timeouts or settings cannot land on two of the three.
///
/// * **Timeouts** — `with_timeouts` bounds the subsequent `write` / `end`
///   awaits natively (yielding `ChError::TimedOut`, already classified
///   retryable), which the crate documents as far cheaper than wrapping each
///   of them in `tokio::time::timeout`.
/// * **`async_insert`** — when `async_insert` is `true`, `async_insert = 1`
///   plus `wait_for_async_insert = 1` plus `async_insert_deduplicate = 1`. Set
///   here, on the `Insert`'s own cloned client, rather than on the shared
///   `Client` in [`build_client`]: that client also serves every `SELECT` —
///   including the table-metadata read the crate issues inside
///   `Client::insert` when validation is on — and `async_insert` on a read is
///   at best noise. See [`crate::config::ClickHousePluginConfig::async_insert`]
///   for why `wait_for_async_insert` is pinned to `1` rather than exposed.
/// * **`dedup_token`** — when `Some`, sent as `insert_deduplication_token`, so
///   the engine drops the inserted block if a block with the same token landed
///   within the table's dedup window (`non_replicated_deduplication_window`,
///   see [`ensure_insert_dedup_window`]). Enforced on synchronous inserts. On
///   asynchronous inserts it is enforced only for `Replicated*` tables (which
///   is what `async_insert_deduplicate = 1` requests, and why it is set — it
///   is accepted and inert on the shipped non-replicated engine); the token
///   is still carried so the same call site is correct on either engine.
///
/// `with_setting` rather than the crate's `with_option`: the latter is
/// `#[deprecated(since = "0.14.3")]`, and the workspace lints deprecation.
///
/// # Panics
///
/// The `clickhouse` crate panics if a setting is changed after the request has
/// started, i.e. after the first `write`. Call this on the handle
/// `Client::insert` returned, before any `write` — which is what every call
/// site does.
///
/// No `#[must_use]` of its own: `clickhouse::insert::Insert` already carries
/// one, so dropping the return value is caught either way.
pub(crate) fn configure_insert<T>(
    insert: clickhouse::insert::Insert<T>,
    request_timeout: Duration,
    async_insert: bool,
    dedup_token: Option<&str>,
) -> clickhouse::insert::Insert<T> {
    let mut insert = insert.with_timeouts(Some(request_timeout), Some(request_timeout));
    if async_insert {
        insert = insert
            .with_setting("async_insert", "1")
            .with_setting("wait_for_async_insert", "1")
            .with_setting("async_insert_deduplicate", "1");
    }
    if let Some(token) = dedup_token {
        insert = insert.with_setting("insert_deduplication_token", token);
    }
    insert
}

/// Strip `--` SQL comments, returning only the executable source.
///
/// Comments must be removed *before* splitting on `;`: this migration file's
/// prose comments use semicolons as ordinary punctuation (e.g. "...has no
/// `pg_advisory_lock` equivalent; unlike the reference plugin..."), and a
/// naive `sql.split(';')` treats that mid-sentence semicolon as a statement
/// boundary. The resulting fragment starts mid-comment *without* its `--`
/// prefix (which was left behind in the previous split chunk), so a
/// per-statement "is this comment-only?" check can't catch it — `ClickHouse`
/// then rejects the fragment as a syntax error. Stripping every `--` comment
/// first removes the semicolon from the executable text entirely, so it can
/// never become a false statement boundary.
///
/// A `--` is also stripped when it trails executable text on the same line
/// (`DDL -- trailing comment`), so a comment added that way to the migration
/// file cannot smuggle a semicolon into the executable source. Quote state is
/// tracked while scanning — `''` escapes included, matching
/// [`split_sql_statements`] — so a `--` inside a `COMMENT '...'` column
/// annotation is left untouched.
///
/// # Limitations
///
/// This is a deliberately minimal scanner for one known input
/// (`migrations/0001_init.sql`), not a general SQL lexer. Only `--` line
/// comments and single-quoted strings are recognized; `/* … */` block comments
/// and backtick- or double-quoted identifiers are not. A semicolon inside
/// either construct is therefore treated as executable text and can become a
/// false statement boundary in [`split_sql_statements`]. Migration files must
/// consequently stay within `--` comments and single-quoted strings; if a
/// future migration needs block comments or quoted identifiers containing
/// semicolons, move the statements into per-statement consts or files instead
/// of extending this scanner.
fn strip_line_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_string = false;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if in_string && chars.peek() == Some(&'\'') => {
                out.push('\'');
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }
            '\'' => {
                in_string = !in_string;
                out.push(c);
            }
            '-' if !in_string && chars.peek() == Some(&'-') => {
                // Drop the comment body but keep the newline, so the remaining
                // executable text stays on its original line.
                while chars.peek().is_some_and(|&next| next != '\n') {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }

    out
}

/// Split (already comment-stripped) SQL into individual statements on `;`,
/// ignoring semicolons inside single-quoted string literals.
///
/// This migration's `COMMENT '...'` column annotations are prose that itself
/// uses semicolons as punctuation (e.g. `COMMENT 'Usage type; application-\
/// enforced reference to usage_type_catalog (no FK in ClickHouse)'`) --  a
/// plain `sql.split(';')` would cut statements apart mid-string-literal.
/// A doubled `''` inside a string is treated as an escaped literal quote
/// (standard SQL / `ClickHouse` string-escaping) rather than a string
/// terminator, though this file does not currently use that form.
///
/// Shares the quote-tracking limits documented on [`strip_line_comments`]:
/// `/* … */` block comments and backtick- or double-quoted identifiers are not
/// recognized, so a semicolon inside one would split a statement.
fn split_sql_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if in_string && chars.peek() == Some(&'\'') => {
                // Escaped `''` inside a string literal -- consume both quotes
                // as literal content, stay inside the string.
                current.push('\'');
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            '\'' => {
                in_string = !in_string;
                current.push(c);
            }
            ';' if !in_string => {
                let stmt = current.trim().to_owned();
                if !stmt.is_empty() {
                    statements.push(stmt);
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }

    let stmt = current.trim().to_owned();
    if !stmt.is_empty() {
        statements.push(stmt);
    }

    statements
}

/// Apply the embedded initial schema migration against the live `ClickHouse` instance.
///
/// Reads the embedded SQL from `migrations/0001_init.sql` (which bakes a fixed
/// 1-year TTL default into `usage_records`), strips `--` comment lines (see
/// [`strip_line_comments`]), splits the result into statements on `';'` while
/// respecting single-quoted string literals (see [`split_sql_statements`]), and
/// executes each one via the `clickhouse` crate's raw query path.
///
/// Every statement uses `CREATE TABLE IF NOT EXISTS`, making the migration
/// idempotent and safe to re-run on concurrent replica startup. `ClickHouse`
/// has no `pg_advisory_lock` equivalent; idempotent DDL alone is sufficient
/// because `CREATE TABLE IF NOT EXISTS` is internally atomic in `ClickHouse`.
///
/// Config-driven retention is applied separately by [`ensure_retention_ttl`].
///
/// Each statement is bounded by `deadline` (the same client-side budget the
/// request path uses, `ClickHousePluginConfig::client_deadline`). A hung `init`
/// is worse than a failed one: it never surfaces as an error, and the gear's
/// readiness gauge is already published as 0 by then.
///
/// # Errors
///
/// Returns an error if any DDL statement fails or exceeds `deadline`. The
/// context message includes the failing statement text.
pub async fn apply_migrations(
    client: &clickhouse::Client,
    deadline: Duration,
) -> anyhow::Result<()> {
    let sql = strip_line_comments(MIGRATION_SQL);

    for stmt in split_sql_statements(&sql) {
        tokio::time::timeout(deadline, client.query(&stmt).execute())
            .await
            .map_err(|_elapsed| {
                anyhow::anyhow!(
                    "migration DDL statement exceeded the {}s client-side deadline:\n{stmt}",
                    deadline.as_secs()
                )
            })?
            .with_context(|| format!("migration DDL statement failed:\n{stmt}"))?;
    }

    Ok(())
}

/// Parse retention seconds from a `CREATE TABLE` / `create_table_query` string.
///
/// Accepts both the literal form we emit (`INTERVAL <n> SECOND`) and
/// `ClickHouse`'s rewritten form (`toIntervalSecond(<n>)`). Returns `None` when
/// no recognisable TTL interval is present.
pub(crate) fn parse_ttl_seconds(create_table_query: &str) -> Option<u64> {
    // Prefer the rewritten form ClickHouse often stores in system.tables.
    if let Some(secs) = extract_u64_after(create_table_query, "toIntervalSecond(") {
        return Some(secs);
    }
    // Literal INTERVAL form from our DDL / ALTER.
    let upper = create_table_query.to_ascii_uppercase();
    let interval_idx = upper.find("INTERVAL")?;
    let after_interval = &create_table_query[interval_idx + "INTERVAL".len()..];
    let trimmed = after_interval.trim_start();
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digits_end == 0 {
        return None;
    }
    let secs: u64 = trimmed[..digits_end].parse().ok()?;
    let rest = trimmed[digits_end..].trim_start();
    if rest.to_ascii_uppercase().starts_with("SECOND") {
        Some(secs)
    } else {
        None
    }
}

fn extract_u64_after(haystack: &str, needle: &str) -> Option<u64> {
    let idx = haystack.find(needle)?;
    let after = &haystack[idx + needle.len()..];
    let digits_end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    if digits_end == 0 {
        return None;
    }
    after[..digits_end].parse().ok()
}

/// True when the live TTL still casts `created_at` through 32-bit `toDateTime`.
///
/// Matching interval seconds alone is not enough to skip `MODIFY TTL`: a
/// table provisioned with the old wrapping clause would otherwise keep
/// saturating at 2106 until `retention_period_secs` changed.
fn ttl_uses_todatetime_cast(create_table_query: &str) -> bool {
    create_table_query.contains("toDateTime(created_at)")
}

/// Upper bound on the span of one `usage_records` partition, in seconds.
///
/// The table is `PARTITION BY toYYYYMM(created_at)`, so a partition covers one
/// calendar month — 31 days at most.
const PARTITION_SPAN_SECS: u64 = 31 * 86_400;

/// Retention below which the whole-partition TTL mode is worth warning about.
///
/// Two partition spans: at or above it the overshoot below is a small fraction
/// of the window, under it the overshoot is comparable to the window itself.
const SHORT_RETENTION_WARN_SECS: u64 = 2 * PARTITION_SPAN_SECS;

/// Whether whole-partition TTL expiry materially overshoots `retention_period_secs`.
///
/// `usage_records` is provisioned `SETTINGS ttl_only_drop_parts = 1`, so TTL
/// drops a partition once every row in it has expired rather than rewriting
/// parts to delete rows individually. A row therefore lives for
/// `retention_period_secs` **plus** up to the remaining span of its own
/// partition. Against the 1-year default that is a few percent; against a
/// one-week window it is several multiples, which an operator reading only
/// `retention_period_secs` would not expect.
pub(crate) fn retention_overshoots_partition(retention_period_secs: u64) -> bool {
    retention_period_secs < SHORT_RETENTION_WARN_SECS
}

/// Log [`retention_overshoots_partition`] once per `init`.
///
/// Advisory only: the DDL cannot adapt, because `PARTITION BY` is fixed at
/// `CREATE TABLE` while retention is configuration.
fn warn_on_short_retention(retention_period_secs: u64) {
    if retention_overshoots_partition(retention_period_secs) {
        tracing::warn!(
            retention_period_secs,
            partition_span_secs = PARTITION_SPAN_SECS,
            "configured retention is shorter than two usage_records partitions; expiry drops \
             whole monthly partitions (ttl_only_drop_parts = 1), so a row can outlive the \
             configured window by up to one partition span"
        );
    }
}

/// Reconcile `usage_records` TTL with the configured retention window.
///
/// Reads the live `create_table_query` from `system.tables`. When the table
/// has no TTL, the parsed interval seconds differ from
/// `retention_period_secs`, or the live clause still wraps `created_at` in
/// `toDateTime`, issues
/// `ALTER TABLE usage_records MODIFY TTL created_at + INTERVAL <n> SECOND DELETE`.
///
/// Both statements are bounded by `deadline`, for the same reason as
/// [`apply_migrations`].
///
/// # Errors
///
/// Returns an error if the table is missing, either statement fails, or either
/// exceeds `deadline`.
pub async fn ensure_retention_ttl(
    client: &clickhouse::Client,
    retention_period_secs: u64,
    deadline: Duration,
) -> anyhow::Result<()> {
    let create_sql = read_records_create_sql(client, deadline).await?;

    warn_on_short_retention(retention_period_secs);

    let current = parse_ttl_seconds(&create_sql);
    if current == Some(retention_period_secs) && !ttl_uses_todatetime_cast(&create_sql) {
        tracing::debug!(
            retention_period_secs,
            "usage_records TTL already matches configured retention"
        );
        return Ok(());
    }

    let alter = format!(
        "ALTER TABLE usage_records MODIFY TTL \
         created_at + INTERVAL {retention_period_secs} SECOND DELETE"
    );
    tracing::info!(
        previous = ?current,
        retention_period_secs,
        "updating usage_records TTL to match configured retention"
    );
    tokio::time::timeout(deadline, client.query(&alter).execute())
        .await
        .map_err(|_elapsed| {
            anyhow::anyhow!(
                "retention TTL alter exceeded the {}s client-side deadline:\n{alter}",
                deadline.as_secs()
            )
        })?
        .with_context(|| format!("failed to apply retention TTL:\n{alter}"))?;

    Ok(())
}

/// Number of recent inserted blocks `usage_records` deduplicates
/// `insert_deduplication_token`s against (`non_replicated_deduplication_window`).
///
/// Each synchronous single-record create is one block; each batch or marker
/// statement is one block per `toYYYYMM` partition it touches. The window only
/// has to outlast the race it guards — two retries of one write milliseconds
/// apart — so 10 000 blocks is ample headroom rather than a retention horizon.
/// The value in `migrations/0001_init.sql` must match; `pool_tests` pins it.
pub(crate) const INSERT_DEDUP_WINDOW_BLOCKS: u64 = 10_000;

/// Read the live `create_table_query` of `usage_records` from `system.tables`.
///
/// Shared by [`ensure_retention_ttl`] and [`ensure_insert_dedup_window`], both
/// of which reconcile a table-level setting by parsing this text.
async fn read_records_create_sql(
    client: &clickhouse::Client,
    deadline: Duration,
) -> anyhow::Result<String> {
    tokio::time::timeout(
        deadline,
        client
            .query(
                "SELECT create_table_query \
                 FROM system.tables \
                 WHERE database = currentDatabase() AND name = 'usage_records'",
            )
            .fetch_one::<String>(),
    )
    .await
    .map_err(|_elapsed| {
        anyhow::anyhow!(
            "reading usage_records create_table_query exceeded the {}s client-side deadline",
            deadline.as_secs()
        )
    })?
    .context("failed to read usage_records create_table_query from system.tables")
}

/// Parse `non_replicated_deduplication_window` from a `CREATE TABLE` /
/// `create_table_query` string; `None` when the setting is absent.
pub(crate) fn parse_dedup_window(create_table_query: &str) -> Option<u64> {
    extract_u64_after(create_table_query, "non_replicated_deduplication_window = ")
}

/// Reconcile the `usage_records` insert dedup window with
/// [`INSERT_DEDUP_WINDOW_BLOCKS`].
///
/// The migration's `CREATE TABLE IF NOT EXISTS` carries the setting for fresh
/// deployments but cannot retrofit a table created before it did, so this
/// reads the live `create_table_query` and issues
/// `ALTER TABLE usage_records MODIFY SETTING non_replicated_deduplication_window = <n>`
/// when the live value differs. `MODIFY SETTING` is metadata-only and safe to
/// repeat; the read keeps startup silent when nothing changes — the same
/// posture as [`ensure_retention_ttl`].
///
/// On a table an operator provisioned as `Replicated*`, the setting is
/// accepted but inert (`replicated_deduplication_window` governs instead), so
/// this is harmless there.
///
/// # Errors
///
/// Returns an error if the table is missing, either statement fails, or either
/// exceeds `deadline`.
pub async fn ensure_insert_dedup_window(
    client: &clickhouse::Client,
    deadline: Duration,
) -> anyhow::Result<()> {
    let create_sql = read_records_create_sql(client, deadline).await?;
    let current = parse_dedup_window(&create_sql);
    if current == Some(INSERT_DEDUP_WINDOW_BLOCKS) {
        tracing::debug!(
            window_blocks = INSERT_DEDUP_WINDOW_BLOCKS,
            "usage_records insert dedup window already set"
        );
        return Ok(());
    }

    let alter = format!(
        "ALTER TABLE usage_records MODIFY SETTING \
         non_replicated_deduplication_window = {INSERT_DEDUP_WINDOW_BLOCKS}"
    );
    tracing::info!(
        previous = ?current,
        window_blocks = INSERT_DEDUP_WINDOW_BLOCKS,
        "setting usage_records insert dedup window"
    );
    tokio::time::timeout(deadline, client.query(&alter).execute())
        .await
        .map_err(|_elapsed| {
            anyhow::anyhow!(
                "insert dedup window alter exceeded the {}s client-side deadline:\n{alter}",
                deadline.as_secs()
            )
        })?
        .with_context(|| format!("failed to apply insert dedup window:\n{alter}"))?;

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "pool_tests.rs"]
mod pool_tests;
