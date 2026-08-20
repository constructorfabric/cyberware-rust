use super::{
    DEFAULT_RETENTION_SECS, MIGRATION_SQL, parse_endpoint, parse_ttl_seconds, split_sql_statements,
    strip_line_comments,
};
use crate::config::is_plaintext_url;

// ---------------------------------------------------------------------------
// Comment stripping ahead of `;`-splitting
//
// Regression coverage for the bug where a semicolon inside prose comment text
// (this migration file literally has one: "...pg_advisory_lock equivalent;
// unlike the reference plugin...") was treated as a statement boundary by a
// naive `sql.split(';')`, producing a comment fragment (missing its `--`
// prefix, left behind in the previous chunk) that ClickHouse then rejected as
// a syntax error.
// ---------------------------------------------------------------------------

#[test]
fn strip_line_comments_removes_full_comment_lines() {
    let sql = "-- a comment\nCREATE TABLE t (x Int32);\n-- trailing comment";
    let stripped = strip_line_comments(sql);
    assert!(!stripped.contains("comment"));
    assert!(stripped.contains("CREATE TABLE t (x Int32);"));
}

#[test]
fn strip_line_comments_removes_semicolon_hidden_in_prose() {
    // The exact sentence from this migration file's header, verbatim.
    let sql = "-- ClickHouse has no pg_advisory_lock\n\
               -- equivalent; unlike the reference plugin (TimescaleDB), no advisory-lock\n\
               CREATE TABLE t (x Int32);";
    let stripped = strip_line_comments(sql);
    assert_eq!(
        stripped.matches(';').count(),
        1,
        "the only semicolon left after stripping comments must be the real \
         statement terminator, not the one in \"equivalent; unlike\": {stripped:?}"
    );
}

/// A `--` comment trailing executable text on the same line must be stripped
/// too: a semicolon inside such a comment would otherwise split the statement
/// it trails and fail the startup DDL.
#[test]
fn strip_line_comments_removes_trailing_inline_comment() {
    let sql = "CREATE TABLE t (x Int32) ENGINE = Memory; -- one; two\nSELECT 1;";
    let stripped = strip_line_comments(sql);
    assert!(!stripped.contains("one"), "got: {stripped:?}");
    let statements = split_sql_statements(&stripped);
    assert_eq!(
        statements,
        vec![
            "CREATE TABLE t (x Int32) ENGINE = Memory".to_owned(),
            "SELECT 1".to_owned(),
        ]
    );
}

/// A `--` inside a `COMMENT '...'` column annotation is string content, not a
/// comment marker, and must survive stripping.
#[test]
fn strip_line_comments_keeps_double_dash_inside_string_literal() {
    let sql = "CREATE TABLE t (x String COMMENT 'a -- b') ENGINE = Memory;";
    assert_eq!(strip_line_comments(sql), sql);
}

#[test]
fn migration_sql_after_comment_strip_yields_exactly_two_statements() {
    let stripped = strip_line_comments(MIGRATION_SQL);
    let statements = split_sql_statements(&stripped);
    assert_eq!(
        statements.len(),
        2,
        "expected exactly 2 executable DDL statements after stripping comments (2 CREATE TABLE), \
         got: {statements:#?}"
    );
    for stmt in &statements {
        assert!(
            stmt.starts_with("CREATE TABLE IF NOT EXISTS"),
            "unexpected statement kind: {stmt}"
        );
    }
}

// ---------------------------------------------------------------------------
// Quote-aware statement splitting -- `COMMENT '...'` column annotations in
// this migration are prose that itself contains semicolons as punctuation
// (e.g. "Usage type; application-enforced reference to..."), which must NOT
// be treated as statement boundaries.
// ---------------------------------------------------------------------------

#[test]
fn split_sql_statements_ignores_semicolon_inside_string_literal() {
    let sql = "CREATE TABLE t (x String COMMENT 'a; b') ENGINE = Memory;";
    let statements = split_sql_statements(sql);
    assert_eq!(statements, vec![sql.trim_end_matches(';').to_owned()]);
}

#[test]
fn split_sql_statements_splits_on_real_terminators() {
    let sql =
        "CREATE TABLE a (x Int32) ENGINE = Memory;\nCREATE TABLE b (y Int32) ENGINE = Memory;";
    let statements = split_sql_statements(sql);
    assert_eq!(statements.len(), 2);
    assert!(statements[0].starts_with("CREATE TABLE a"));
    assert!(statements[1].starts_with("CREATE TABLE b"));
}

#[test]
fn split_sql_statements_handles_escaped_quote_inside_string() {
    let sql = "CREATE TABLE t (x String COMMENT 'it''s; fine') ENGINE = Memory;";
    let statements = split_sql_statements(sql);
    assert_eq!(
        statements.len(),
        1,
        "escaped '' must not end the string early"
    );
    assert!(statements[0].contains("it''s; fine"));
}

#[test]
fn split_sql_statements_trims_and_skips_empty_segments() {
    let sql = "  ;  CREATE TABLE t (x Int32) ENGINE = Memory;  ;  ";
    let statements = split_sql_statements(sql);
    assert_eq!(
        statements,
        vec!["CREATE TABLE t (x Int32) ENGINE = Memory".to_owned()]
    );
}

/// A trailing statement without a terminating `;` still executes — dropping it
/// would silently skip the last DDL statement of a migration file.
#[test]
fn split_sql_statements_keeps_unterminated_trailing_statement() {
    let sql = "CREATE TABLE a (x Int32) ENGINE = Memory;\nCREATE TABLE b (y Int32) ENGINE = Memory";
    let statements = split_sql_statements(sql);
    assert_eq!(
        statements,
        vec![
            "CREATE TABLE a (x Int32) ENGINE = Memory".to_owned(),
            "CREATE TABLE b (y Int32) ENGINE = Memory".to_owned(),
        ]
    );
}

// ---------------------------------------------------------------------------
// database_url -> (base endpoint, user, password, database) splitting
//
// Regression coverage for the bug where `database_url`'s embedded
// user/password/path were passed straight through to
// `clickhouse::Client::with_url`, which sends every request to that literal
// path (ClickHouse's HTTP interface only implements `/`, `/ping`, etc. --
// see `pool.rs::build_client` doc comment) while silently dropping the
// credentials.
// ---------------------------------------------------------------------------

#[test]
fn parse_endpoint_splits_user_password_and_database() {
    let endpoint = parse_endpoint("http://chuser:s3cret@ch:8123/usage").unwrap();
    assert_eq!(endpoint.base_url, "http://ch:8123/");
    assert_eq!(endpoint.user.as_deref(), Some("chuser"));
    assert_eq!(endpoint.password.as_deref(), Some("s3cret"));
    assert_eq!(endpoint.database.as_deref(), Some("usage"));
}

#[test]
fn parse_endpoint_base_url_has_no_leftover_path_or_userinfo() {
    // The exact bug this guards: a leftover path segment makes every request
    // 404 against ClickHouse's HTTP interface, and leftover userinfo would
    // never be honored by `Client::with_url` (see struct-level doc comment).
    let endpoint = parse_endpoint("http://user:pass@localhost:8123/mydb").unwrap();
    assert!(
        !endpoint.base_url.contains("mydb"),
        "base_url must not retain the database path: {}",
        endpoint.base_url
    );
    assert!(
        !endpoint.base_url.contains("user") && !endpoint.base_url.contains("pass"),
        "base_url must not retain userinfo: {}",
        endpoint.base_url
    );
}

#[test]
fn parse_endpoint_with_no_userinfo_or_path_yields_none() {
    let endpoint = parse_endpoint("http://localhost:8123").unwrap();
    assert_eq!(endpoint.base_url, "http://localhost:8123/");
    assert_eq!(endpoint.user, None);
    assert_eq!(endpoint.password, None);
    assert_eq!(endpoint.database, None);
}

#[test]
fn parse_endpoint_trims_leading_and_trailing_slashes_from_database() {
    let endpoint = parse_endpoint("http://localhost:8123/usage/").unwrap();
    assert_eq!(endpoint.database.as_deref(), Some("usage"));
}

#[test]
fn parse_endpoint_user_without_password_has_no_password() {
    let endpoint = parse_endpoint("http://chuser@localhost:8123/").unwrap();
    assert_eq!(endpoint.user.as_deref(), Some("chuser"));
    assert_eq!(endpoint.password, None);
}

#[test]
fn parse_endpoint_percent_decodes_userinfo_with_reserved_chars() {
    // `@` and `/` must be percent-encoded in the URL; after parse they must
    // be restored to literal credentials for `with_user` / `with_password`.
    let endpoint = parse_endpoint("http://u%3Aser:p%40ss%2Fword@ch:8123/usage").unwrap();
    assert_eq!(endpoint.user.as_deref(), Some("u:ser"));
    assert_eq!(endpoint.password.as_deref(), Some("p@ss/word"));
    assert_eq!(endpoint.database.as_deref(), Some("usage"));
}

#[test]
fn parse_endpoint_rejects_malformed_url() {
    assert!(parse_endpoint("not a url").is_err());
}

// ---------------------------------------------------------------------------
// TLS posture detection
// ---------------------------------------------------------------------------

#[test]
fn plaintext_url_detected_for_http_scheme() {
    assert!(is_plaintext_url("http://user:pass@ch:8123/db"));
    assert!(is_plaintext_url("http://ch:8123/usage"));
}

/// The scheme is matched on the parsed URL, whose scheme the `url` crate
/// lowercases, so case variants of the same cleartext connection are all caught.
#[test]
fn plaintext_url_detected_regardless_of_scheme_case() {
    assert!(is_plaintext_url("HTTP://user:pass@ch:8123/db"));
    assert!(is_plaintext_url("Http://ch:8123/usage"));
}

#[test]
fn tls_url_not_flagged_for_https_scheme() {
    assert!(!is_plaintext_url("https://user:pass@ch:8443/db"));
    assert!(!is_plaintext_url("https://ch:8443/usage"));
    assert!(!is_plaintext_url("HTTPS://ch:8443/usage"));
}

#[test]
fn tls_url_not_flagged_for_non_http_schemes() {
    assert!(!is_plaintext_url("clickhouse://ch:9000/db"));
    assert!(!is_plaintext_url("tcp://ch:9000"));
}

/// A DSN that does not parse cannot be shown to be encrypted, so it counts as
/// plaintext. `validate` rejects it before `build_client`, so this only decides
/// whether the defense-in-depth cleartext warning fires on a path that skipped
/// validation — and a spurious warning is the safe direction there.
#[test]
fn unparseable_url_treated_as_plaintext() {
    assert!(is_plaintext_url("not-a-url"));
    assert!(is_plaintext_url(""));
}

// ---------------------------------------------------------------------------
// TTL parsing and default retention in migration SQL
// ---------------------------------------------------------------------------

#[test]
fn parse_ttl_seconds_from_interval_form() {
    let sql = "CREATE TABLE usage_records (...) TTL created_at + INTERVAL 86400 SECOND DELETE";
    assert_eq!(parse_ttl_seconds(sql), Some(86_400));
}

#[test]
fn parse_ttl_seconds_from_legacy_todatetime_interval_form() {
    let sql = "CREATE TABLE usage_records (...) TTL toDateTime(created_at) + INTERVAL 86400 SECOND DELETE";
    assert_eq!(parse_ttl_seconds(sql), Some(86_400));
}

#[test]
fn parse_ttl_seconds_from_to_interval_second_form() {
    let sql = "CREATE TABLE usage_records (...) TTL created_at + toIntervalSecond(31536000)";
    assert_eq!(parse_ttl_seconds(sql), Some(31_536_000));
}

#[test]
fn parse_ttl_seconds_from_legacy_todatetime_to_interval_second_form() {
    let sql =
        "CREATE TABLE usage_records (...) TTL toDateTime(created_at) + toIntervalSecond(31536000)";
    assert_eq!(parse_ttl_seconds(sql), Some(31_536_000));
}

#[test]
fn parse_ttl_seconds_returns_none_when_missing() {
    let sql = "CREATE TABLE usage_records (...) ENGINE = ReplacingMergeTree(version) ORDER BY (id)";
    assert_eq!(parse_ttl_seconds(sql), None);
}

#[test]
fn migration_sql_contains_both_table_names() {
    assert!(
        MIGRATION_SQL.contains("usage_type_catalog"),
        "migration SQL must create the usage_type_catalog table"
    );
    assert!(
        MIGRATION_SQL.contains("usage_records"),
        "migration SQL must create the usage_records table"
    );
}

#[test]
fn migration_sql_uses_replacingmergetree() {
    assert!(
        MIGRATION_SQL.contains("ReplacingMergeTree(version)"),
        "both tables must use ReplacingMergeTree(version) engine"
    );
}

#[test]
fn migration_sql_uses_create_table_if_not_exists() {
    // Count only the DDL statement lines (non-comment lines starting with CREATE).
    let statement_count = MIGRATION_SQL
        .lines()
        .filter(|l| l.trim_start().starts_with("CREATE TABLE IF NOT EXISTS"))
        .count();
    assert_eq!(
        statement_count, 2,
        "migration must have exactly 2 idempotent CREATE TABLE IF NOT EXISTS DDL statements \
         (comments that mention the phrase are intentionally excluded from this count)"
    );
}

#[test]
fn migration_sql_has_default_one_year_ttl() {
    assert!(
        !MIGRATION_SQL.contains("{retention_period_secs}"),
        "migration SQL must not contain a retention placeholder"
    );
    assert!(
        MIGRATION_SQL.contains(&format!("INTERVAL {DEFAULT_RETENTION_SECS} SECOND")),
        "migration SQL must bake the 1-year default TTL ({DEFAULT_RETENTION_SECS}s)"
    );
    assert!(
        MIGRATION_SQL.contains("TTL created_at + INTERVAL"),
        "migration TTL must use DateTime64 created_at directly, not toDateTime"
    );
    assert!(
        !MIGRATION_SQL.contains("TTL toDateTime(created_at)"),
        "migration TTL must not cast created_at through 32-bit toDateTime"
    );
    assert_eq!(DEFAULT_RETENTION_SECS, 31_536_000);
}

#[test]
fn migration_sql_has_correct_order_by_for_catalog() {
    // The catalog table uses a single-column sort key on gts_id.
    // Verify the clause appears in the catalog table section.
    let after_catalog = MIGRATION_SQL
        .split("usage_type_catalog")
        .nth(1)
        .unwrap_or("");
    let before_records = after_catalog
        .split("usage_records")
        .next()
        .unwrap_or(after_catalog);
    assert!(
        before_records.contains("ORDER BY (gts_id)"),
        "usage_type_catalog must use ORDER BY (gts_id)"
    );
}

#[test]
fn migration_sql_has_correct_order_by_for_records() {
    // The records table uses the 4-tuple dedup key as ORDER BY.
    let after_records = MIGRATION_SQL.split("usage_records").last().unwrap_or("");
    assert!(
        after_records.contains("ORDER BY (tenant_id, gts_id, created_at, id)"),
        "usage_records must use the 4-tuple dedup key as ORDER BY"
    );
}

#[test]
fn build_client_accepts_https_url_with_auth_and_database() {
    use super::build_client;
    use crate::config::{ClickHousePluginConfig, SecretFromEnv};

    let cfg = ClickHousePluginConfig {
        database_url: SecretFromEnv::new("https://chuser:secret@clickhouse.example:8443/usage_db"),
        request_timeout_secs: 15,
        ..ClickHousePluginConfig::default()
    };
    cfg.validate().expect("https config is valid");
    // Construction only — no network I/O.
    let _client = build_client(&cfg);
}

#[test]
fn build_client_accepts_plaintext_http_when_override_set() {
    use super::build_client;
    use crate::config::{ClickHousePluginConfig, SecretFromEnv};

    let cfg = ClickHousePluginConfig {
        database_url: SecretFromEnv::new("http://default:@localhost:8123/default"),
        allow_insecure_http: true,
        request_timeout_secs: 7,
        ..ClickHousePluginConfig::default()
    };
    cfg.validate().expect("override permits http");
    let _client = build_client(&cfg);
}

/// The fallback client is inert: an unparseable `database_url` fails every
/// request at parameter-validation time, before a socket is opened, so nothing
/// (credentials included) is ever sent anywhere. `validate()` rejects such a
/// URL on the production path; this covers the call sites that skip it.
#[tokio::test]
async fn build_client_falls_back_to_inert_client_on_unparseable_url() {
    use super::build_client;
    use crate::config::{ClickHousePluginConfig, SecretFromEnv};

    let cfg = ClickHousePluginConfig {
        database_url: SecretFromEnv::new("not a url"),
        allow_insecure_http: true,
        ..ClickHousePluginConfig::default()
    };
    cfg.validate()
        .expect_err("an unparseable database_url must not pass validation");

    let client = build_client(&cfg);
    let err = client
        .query("SELECT 1")
        .execute()
        .await
        .expect_err("a client built from an unparseable URL must not reach a server");
    assert!(
        matches!(err, clickhouse::error::Error::InvalidParams(_)),
        "expected an invalid-params failure before any connection attempt, got: {err}"
    );
}

// Live-ClickHouse integration tests are gated behind the `clickhouse` cargo feature.
// Run with: cargo test -p cf-gears-clickhouse-usage-collector-plugin --features clickhouse
#[cfg(feature = "clickhouse")]
mod integration {
    use std::time::Duration;

    use testcontainers::core::WaitFor;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{GenericImage, ImageExt};

    use super::super::{
        DEFAULT_RETENTION_SECS, apply_migrations, build_client, ensure_retention_ttl,
        parse_endpoint, parse_ttl_seconds,
    };
    use crate::config::ClickHousePluginConfig;

    const CH_PASSWORD: &str = "pool_test_pw";

    /// `gts_id` for the row whose `created_at` sits past `DateTime`'s 2106
    /// ceiling, proving a `DateTime64` TTL does not expire it on write.
    const POST_2106_GTS: &str = "post-2106-ttl";

    /// Read the live `usage_records` DDL back from `system.tables`.
    async fn live_create_sql(client: &clickhouse::Client) -> String {
        client
            .query(
                "SELECT create_table_query \
                 FROM system.tables \
                 WHERE database = currentDatabase() AND name = 'usage_records'",
            )
            .fetch_one::<String>()
            .await
            .expect("create_table_query must be readable")
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn apply_migrations_creates_tables() {
        let image = GenericImage::new("clickhouse/clickhouse-server", "25.6")
            .with_wait_for(WaitFor::Nothing)
            .with_env_var("CLICKHOUSE_USER", "default")
            .with_env_var("CLICKHOUSE_PASSWORD", CH_PASSWORD)
            .with_env_var("CLICKHOUSE_DB", "default");

        let container = image
            .start()
            .await
            .expect("ClickHouse container must start");

        let port = container
            .get_host_port_ipv4(8123)
            .await
            .expect("container port 8123 must be mapped");
        let url = format!("http://default:{CH_PASSWORD}@127.0.0.1:{port}/default");

        let cfg: ClickHousePluginConfig = serde_json::from_str(&format!(
            r#"{{"database_url": "{url}", "allow_insecure_http": true}}"#
        ))
        .expect("valid test config");

        let probe = clickhouse::Client::default()
            .with_url(format!("http://127.0.0.1:{port}/"))
            .with_user("default")
            .with_password(CH_PASSWORD);
        for _ in 0..120u8 {
            if probe.query("SELECT 1").fetch_one::<u8>().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let endpoint = parse_endpoint(cfg.database_url.expose()).expect("parseable test URL");
        if let Some(db) = endpoint.database.as_deref()
            && db != "default"
        {
            let mut bootstrap = clickhouse::Client::default().with_url(&endpoint.base_url);
            if let Some(user) = &endpoint.user {
                bootstrap = bootstrap.with_user(user);
            }
            if let Some(password) = &endpoint.password {
                bootstrap = bootstrap.with_password(password);
            }
            bootstrap
                .query(&format!("CREATE DATABASE IF NOT EXISTS `{db}`"))
                .execute()
                .await
                .expect("CREATE DATABASE IF NOT EXISTS must succeed");
        }

        let client = build_client(&cfg);
        apply_migrations(&client, cfg.client_deadline())
            .await
            .expect("migration must succeed against a live ClickHouse instance");

        // Matching seconds must still rewrite a legacy toDateTime TTL.
        client
            .query(&format!(
                "ALTER TABLE usage_records MODIFY TTL \
                 toDateTime(created_at) + INTERVAL {DEFAULT_RETENTION_SECS} SECOND DELETE"
            ))
            .execute()
            .await
            .expect("forcing the legacy toDateTime TTL must succeed");
        let legacy_sql = live_create_sql(&client).await;
        assert!(
            legacy_sql.contains("toDateTime(created_at)"),
            "precondition: live TTL must still wrap created_at: {legacy_sql}"
        );
        ensure_retention_ttl(&client, DEFAULT_RETENTION_SECS, cfg.client_deadline())
            .await
            .expect("ensure_retention_ttl must rewrite a toDateTime TTL even when seconds match");
        let rewritten = live_create_sql(&client).await;
        assert!(
            !rewritten.contains("toDateTime(created_at)"),
            "legacy toDateTime TTL must be rewritten to DateTime64: {rewritten}"
        );
        assert_eq!(
            parse_ttl_seconds(&rewritten),
            Some(DEFAULT_RETENTION_SECS),
            "rewritten TTL must keep the matching interval: {rewritten}"
        );

        // Default DDL TTL is 1 year; ensure with a different window must alter.
        let ten_years = 10 * 365 * 86_400;
        ensure_retention_ttl(&client, ten_years, cfg.client_deadline())
            .await
            .expect("ensure_retention_ttl must alter when config differs from default");

        let create_sql = live_create_sql(&client).await;
        assert_eq!(
            parse_ttl_seconds(&create_sql),
            Some(ten_years),
            "live TTL must match the configured retention after ensure: {create_sql}"
        );
        assert!(
            !create_sql.contains("toDateTime(created_at)"),
            "configured TTL must keep DateTime64 created_at: {create_sql}"
        );

        // Idempotent when already matched.
        ensure_retention_ttl(&client, ten_years, cfg.client_deadline())
            .await
            .expect("ensure_retention_ttl must no-op when TTL already matches");

        // A created_at after DateTime's 2106 ceiling must not expire immediately.
        client
            .query(
                "INSERT INTO usage_records (id, tenant_id, gts_id, value, created_at, \
                 resource_id, resource_type, subject_id, subject_type, idempotency_key, \
                 corrects_id, status, metadata, ingested_at, version) VALUES \
                 (generateUUIDv4(), generateUUIDv4(), ?, 1, \
                  toDateTime64('2107-01-01 00:00:00', 6), \
                  'res-1', 'vm', NULL, NULL, 'idem-post-2106', NULL, 'active', map(), \
                  now64(6), 1)",
            )
            .bind(POST_2106_GTS)
            .execute()
            .await
            .expect("inserting a post-2106 created_at must succeed");
        client
            .query("ALTER TABLE usage_records MATERIALIZE TTL")
            .execute()
            .await
            .expect("MATERIALIZE TTL must succeed");
        client
            .query("OPTIMIZE TABLE usage_records FINAL")
            .execute()
            .await
            .expect("OPTIMIZE FINAL must apply TTL during merge");
        let remaining: u64 = client
            .query("SELECT count() FROM usage_records FINAL WHERE gts_id = ?")
            .bind(POST_2106_GTS)
            .fetch_one()
            .await
            .expect("count of post-2106 row must be readable");
        assert_eq!(
            remaining, 1,
            "post-2106 created_at plus a 10-year TTL must not expire on merge"
        );
    }
}
