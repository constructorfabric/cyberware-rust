//! Tests for `OoP` configuration merge logic
//!
//! Tests cover all merge scenarios:
//! - Database: field-by-field merge (global.servers → gear.database in master → gear.database in local)
//! - Logging: key-by-key merge (local keys override master keys)
//! - Config: full replacement (local replaces master if present)

use super::*;
use crate::bootstrap::config::{
    AppConfig, ConsoleFormat, GlobalDatabaseConfig, LoggingConfig, RenderedDbConfig,
    RenderedGearConfig, Section, SectionFile, ServerConfig, default_logging_config,
};
use std::collections::HashMap;
use std::time::Duration;
use toolkit_db::{DbConnConfig, PoolCfg};
use tracing::Level;

/// Helper to create a minimal `AppConfig` for testing
fn minimal_app_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            home_dir: std::env::temp_dir().join("toolkit_test"),
            ..Default::default()
        },
        logging: default_logging_config(),
        ..Default::default()
    }
}

/// Helper to create a logging section
fn logging_section(console_level: Option<Level>, file: &str) -> Section {
    Section {
        console_level,
        section_file: Some(SectionFile {
            file: file.to_owned(),
            file_level: Some(Level::DEBUG),
        }),
        console_format: ConsoleFormat::default(),
        max_age_days: Some(7),
        max_backups: Some(3),
        max_size_mb: Some(100),
    }
}

// =============================================================================
// Logging Merge Tests
// =============================================================================

mod logging_merge {
    use super::*;

    #[test]
    fn test_merge_logging_local_only() {
        // When only local has logging, result should be local's logging
        let local_logging: LoggingConfig = [(
            "default".to_owned(),
            logging_section(Some(Level::DEBUG), "logs/local.log"),
        )]
        .into();

        let result = merge_logging_configs(None, &local_logging);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get("default").unwrap().console_level,
            Some(Level::DEBUG)
        );
        assert_eq!(
            result.get("default").unwrap().file().unwrap(),
            "logs/local.log"
        );
    }

    #[test]
    fn test_merge_logging_local_overrides_master_key() {
        // Local key should override master key
        let master_logging: LoggingConfig = [
            (
                "default".to_owned(),
                logging_section(Some(Level::INFO), "logs/master.log"),
            ),
            (
                "gear_a".to_owned(),
                logging_section(Some(Level::INFO), "logs/a-master.log"),
            ),
        ]
        .into();

        let local_logging: LoggingConfig = [(
            "default".to_owned(),
            logging_section(Some(Level::DEBUG), "logs/local.log"),
        )]
        .into();

        let result = merge_logging_configs(Some(&master_logging), &local_logging);

        assert_eq!(result.len(), 2);
        // Local overrides default
        assert_eq!(
            result.get("default").unwrap().console_level,
            Some(Level::DEBUG)
        );
        assert_eq!(
            result.get("default").unwrap().file().unwrap(),
            "logs/local.log"
        );
        // Master's gear_a preserved
        assert_eq!(
            result.get("gear_a").unwrap().console_level,
            Some(Level::INFO)
        );
        assert_eq!(
            result.get("gear_a").unwrap().file().unwrap(),
            "logs/a-master.log"
        );
    }

    #[test]
    fn test_merge_logging_local_adds_new_key() {
        // Local can add new keys that don't exist in master
        let master_logging: LoggingConfig = [(
            "default".to_owned(),
            logging_section(Some(Level::INFO), "logs/default.log"),
        )]
        .into();

        let local_logging: LoggingConfig = [(
            "new_gear".to_owned(),
            logging_section(Some(Level::TRACE), "logs/new.log"),
        )]
        .into();

        let result = merge_logging_configs(Some(&master_logging), &local_logging);

        assert_eq!(result.len(), 2);
        assert_eq!(
            result.get("default").unwrap().console_level,
            Some(Level::INFO)
        );
        assert_eq!(
            result.get("new_gear").unwrap().console_level,
            Some(Level::TRACE)
        );
    }

    #[test]
    fn test_merge_logging_multiple_overrides() {
        // Multiple keys can be overridden
        let master_logging: LoggingConfig = [
            (
                "default".to_owned(),
                logging_section(Some(Level::INFO), "logs/default.log"),
            ),
            (
                "sqlx".to_owned(),
                logging_section(Some(Level::WARN), "logs/sql.log"),
            ),
            (
                "api".to_owned(),
                logging_section(Some(Level::INFO), "logs/api.log"),
            ),
        ]
        .into();

        let local_logging: LoggingConfig = [
            (
                "default".to_owned(),
                logging_section(Some(Level::DEBUG), "logs/local-default.log"),
            ),
            (
                "sqlx".to_owned(),
                logging_section(Some(Level::DEBUG), "logs/local-sql.log"),
            ),
        ]
        .into();

        let result = merge_logging_configs(Some(&master_logging), &local_logging);

        assert_eq!(result.len(), 3);
        // Overridden
        assert_eq!(
            result.get("default").unwrap().console_level,
            Some(Level::DEBUG)
        );
        assert_eq!(
            result.get("sqlx").unwrap().console_level,
            Some(Level::DEBUG)
        );
        // Preserved from master
        assert_eq!(result.get("api").unwrap().console_level, Some(Level::INFO));
    }
}

// =============================================================================
// JSON Object Merge Tests
// =============================================================================

mod json_merge {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_merge_json_flat_objects() {
        let mut target = json!({"a": 1, "b": 2});
        let source = json!({"b": 3, "c": 4});

        merge_json_objects(&mut target, &source);

        assert_eq!(target, json!({"a": 1, "b": 3, "c": 4}));
    }

    #[test]
    fn test_merge_json_nested_objects() {
        let mut target = json!({
            "database": {
                "host": "localhost",
                "port": 5432
            }
        });
        let source = json!({
            "database": {
                "port": 5433,
                "user": "admin"
            }
        });

        merge_json_objects(&mut target, &source);

        assert_eq!(
            target,
            json!({
                "database": {
                    "host": "localhost",
                    "port": 5433,
                    "user": "admin"
                }
            })
        );
    }

    #[test]
    fn test_merge_json_deeply_nested() {
        let mut target = json!({
            "level1": {
                "level2": {
                    "a": 1,
                    "b": 2
                }
            }
        });
        let source = json!({
            "level1": {
                "level2": {
                    "b": 3,
                    "c": 4
                },
                "new_key": "value"
            }
        });

        merge_json_objects(&mut target, &source);

        assert_eq!(
            target,
            json!({
                "level1": {
                    "level2": {
                        "a": 1,
                        "b": 3,
                        "c": 4
                    },
                    "new_key": "value"
                }
            })
        );
    }

    #[test]
    fn test_merge_json_source_replaces_non_object() {
        // When target has non-object value, source object replaces it
        let mut target = json!({"key": "string_value"});
        let source = json!({"key": {"nested": true}});

        merge_json_objects(&mut target, &source);

        assert_eq!(target, json!({"key": {"nested": true}}));
    }

    #[test]
    fn test_merge_json_non_object_replaces_object() {
        // When source has non-object value, it replaces target object
        let mut target = json!({"key": {"nested": true}});
        let source = json!({"key": "string_value"});

        merge_json_objects(&mut target, &source);

        assert_eq!(target, json!({"key": "string_value"}));
    }

    #[test]
    fn test_merge_json_empty_source() {
        let mut target = json!({"a": 1, "b": 2});
        let source = json!({});

        merge_json_objects(&mut target, &source);

        assert_eq!(target, json!({"a": 1, "b": 2}));
    }

    #[test]
    fn test_merge_json_empty_target() {
        let mut target = json!({});
        let source = json!({"a": 1, "b": 2});

        merge_json_objects(&mut target, &source);

        assert_eq!(target, json!({"a": 1, "b": 2}));
    }
}

// =============================================================================
// Database Merge Tests (via build_merged_db_options)
// =============================================================================

mod database_merge {
    use super::*;
    use serde_json::json;

    fn create_global_db_config() -> GlobalDatabaseConfig {
        let mut servers = HashMap::new();
        servers.insert(
            "sqlite_main".to_owned(),
            DbConnConfig {
                engine: Some(toolkit_db::config::DbEngineCfg::Sqlite),
                server: None,
                dsn: None,
                host: None,
                port: None,
                user: None,
                password: None,
                dbname: None,
                file: None,
                path: None,
                params: Some([("WAL".to_owned(), "true".to_owned())].into()),
                pool: Some(PoolCfg {
                    max_conns: Some(5),
                    min_conns: None,
                    acquire_timeout: Some(Duration::from_secs(30)),
                    idle_timeout: None,
                    max_lifetime: None,
                    test_before_acquire: None,
                }),
                lock_keepalive: None,
            },
        );
        GlobalDatabaseConfig {
            servers,
            auto_provision: Some(true),
        }
    }

    fn create_gear_db_config() -> DbConnConfig {
        DbConnConfig {
            engine: Some(toolkit_db::config::DbEngineCfg::Sqlite),
            server: Some("sqlite_main".to_owned()),
            dsn: None,
            host: None,
            port: None,
            user: None,
            password: None,
            dbname: None,
            file: Some("gear.db".to_owned()),
            path: None,
            params: None,
            pool: None,
            lock_keepalive: None,
        }
    }

    #[test]
    fn test_rendered_db_config_no_database() {
        // When no database config, result should be DbOptions::None
        let home_dir = std::env::temp_dir().join("toolkit_test_no_db");
        let local_config = minimal_app_config();

        let result = build_merged_db_options(&home_dir, "test_gear", None, &local_config);

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DbOptions::None));
    }

    #[test]
    fn test_rendered_db_config_master_only() {
        // When only master has database config
        let home_dir = std::env::temp_dir().join("toolkit_test_master_only");
        _ = std::fs::create_dir_all(&home_dir);

        let rendered_db = RenderedDbConfig::new(
            Some(create_global_db_config()),
            Some(create_gear_db_config()),
        );

        let local_config = minimal_app_config();

        let result =
            build_merged_db_options(&home_dir, "test_gear", Some(&rendered_db), &local_config);

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DbOptions::Manager(_)));
    }

    #[test]
    fn test_rendered_db_config_local_only() {
        // When only local has database config (standalone mode)
        let home_dir = std::env::temp_dir().join("toolkit_test_local_only");
        _ = std::fs::create_dir_all(&home_dir);

        let mut local_config = minimal_app_config();
        local_config.database = Some(create_global_db_config());
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "database": {
                    "server": "sqlite_main",
                    "file": "local.db"
                }
            }),
        );

        let result = build_merged_db_options(&home_dir, "test_gear", None, &local_config);

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DbOptions::Manager(_)));
    }

    #[test]
    fn test_rendered_db_config_local_overrides_pool() {
        // Local config should override pool settings from master
        let home_dir = std::env::temp_dir().join("toolkit_test_pool_override");
        _ = std::fs::create_dir_all(&home_dir);

        let rendered_db = RenderedDbConfig::new(
            Some(create_global_db_config()),
            Some(create_gear_db_config()),
        );

        let mut local_config = minimal_app_config();
        // Local overrides pool.max_conns
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "database": {
                    "pool": {
                        "max_conns": 10
                    }
                }
            }),
        );

        let result =
            build_merged_db_options(&home_dir, "test_gear", Some(&rendered_db), &local_config);

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DbOptions::Manager(_)));
    }

    #[test]
    fn test_rendered_db_config_local_overrides_file() {
        // Local config should override file path from master
        let home_dir = std::env::temp_dir().join("toolkit_test_file_override");
        _ = std::fs::create_dir_all(&home_dir);

        let rendered_db = RenderedDbConfig::new(
            Some(create_global_db_config()),
            Some(create_gear_db_config()),
        );

        let mut local_config = minimal_app_config();
        // Local overrides file
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "database": {
                    "file": "local_override.db"
                }
            }),
        );

        let result =
            build_merged_db_options(&home_dir, "test_gear", Some(&rendered_db), &local_config);

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DbOptions::Manager(_)));
    }

    #[test]
    fn test_rendered_db_config_local_adds_params() {
        // Local config can add new params to master's params
        let home_dir = std::env::temp_dir().join("toolkit_test_params_add");
        _ = std::fs::create_dir_all(&home_dir);

        let rendered_db = RenderedDbConfig::new(
            Some(create_global_db_config()),
            Some(create_gear_db_config()),
        );

        let mut local_config = minimal_app_config();
        // Local adds new params
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "database": {
                    "params": {
                        "new_param": "value"
                    }
                }
            }),
        );

        let result =
            build_merged_db_options(&home_dir, "test_gear", Some(&rendered_db), &local_config);

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DbOptions::Manager(_)));
    }

    #[test]
    fn test_rendered_db_config_local_global_merges_with_master() {
        // Local global database config merges with master's global config
        let home_dir = std::env::temp_dir().join("toolkit_test_global_merge");
        _ = std::fs::create_dir_all(&home_dir);

        let rendered_db = RenderedDbConfig::new(
            Some(create_global_db_config()),
            Some(create_gear_db_config()),
        );

        let mut local_config = minimal_app_config();
        // Local adds a new server to global database config
        let mut new_servers = HashMap::new();
        new_servers.insert(
            "new_server".to_owned(),
            DbConnConfig {
                engine: Some(toolkit_db::config::DbEngineCfg::Sqlite),
                server: None,
                dsn: Some(toolkit_utils::SecretString::new("sqlite://new.db")),
                host: None,
                port: None,
                user: None,
                password: None,
                dbname: None,
                file: None,
                path: None,
                params: None,
                pool: None,
                lock_keepalive: None,
            },
        );
        local_config.database = Some(GlobalDatabaseConfig {
            servers: new_servers,
            auto_provision: None,
        });

        let result =
            build_merged_db_options(&home_dir, "test_gear", Some(&rendered_db), &local_config);

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DbOptions::Manager(_)));
    }
}

// =============================================================================
// Full OoP Config Build Tests
// =============================================================================

mod full_oop_config {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_build_oop_config_standalone_mode() {
        // No rendered config - standalone mode
        let mut local_config = minimal_app_config();
        local_config.logging = [(
            "default".to_owned(),
            logging_section(Some(Level::DEBUG), "logs/standalone.log"),
        )]
        .into();
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "config": {
                    "setting": "local_value"
                }
            }),
        );

        let result = build_oop_config_and_db(&local_config, "test_gear", None);

        assert!(result.is_ok());
        let (final_config, db_options) = result.unwrap();

        // Config should be from local
        let gear_config = final_config.gears.get("test_gear").unwrap();
        assert_eq!(gear_config["config"]["setting"], "local_value");

        // Logging should be from local
        assert_eq!(final_config.logging.len(), 1);
        assert_eq!(
            final_config.logging.get("default").unwrap().console_level,
            Some(Level::DEBUG)
        );

        // No database
        assert!(matches!(db_options, DbOptions::None));
    }

    #[test]
    fn test_build_oop_config_with_rendered_config() {
        // With rendered config from master
        let local_config = minimal_app_config();

        let rendered = RenderedGearConfig {
            database: None,
            config: json!({"master_setting": "value"}),
            logging: Some(
                [(
                    "default".to_owned(),
                    logging_section(Some(Level::INFO), "logs/master.log"),
                )]
                .into(),
            ),
            opentelemetry: None,
        };

        let result = build_oop_config_and_db(&local_config, "test_gear", Some(&rendered));

        assert!(result.is_ok());
        let (final_config, _) = result.unwrap();
        let merged_logging = &final_config.logging;

        // Config should be from master (local has no config section)
        let gear_config = final_config.gears.get("test_gear").unwrap();
        assert_eq!(gear_config["config"]["master_setting"], "value");

        // Logging from master
        assert_eq!(
            merged_logging.get("default").unwrap().console_level,
            Some(Level::INFO)
        );
    }

    #[test]
    fn test_build_oop_config_local_overrides_master_config() {
        // Local config section completely replaces master
        let mut local_config = minimal_app_config();
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "config": {
                    "local_setting": "local_value"
                }
            }),
        );

        let rendered = RenderedGearConfig {
            database: None,
            config: json!({
                "master_setting": "master_value",
                "another": "setting"
            }),
            logging: None,
            opentelemetry: None,
        };

        let result = build_oop_config_and_db(&local_config, "test_gear", Some(&rendered));

        assert!(result.is_ok());
        let (final_config, _) = result.unwrap();

        // Config should be from LOCAL (full replacement)
        let gear_config = final_config.gears.get("test_gear").unwrap();
        assert_eq!(gear_config["config"]["local_setting"], "local_value");
        // Master's settings should NOT be present
        assert!(gear_config["config"].get("master_setting").is_none());
    }

    #[test]
    fn test_build_oop_config_logging_merge() {
        // Logging should merge (key-by-key)
        let mut local_config = minimal_app_config();
        local_config.logging = [
            (
                "default".to_owned(),
                logging_section(Some(Level::DEBUG), "logs/local-default.log"),
            ),
            (
                "new_key".to_owned(),
                logging_section(Some(Level::TRACE), "logs/new.log"),
            ),
        ]
        .into();

        let rendered = RenderedGearConfig {
            database: None,
            config: json!({}),
            logging: Some(
                [
                    (
                        "default".to_owned(),
                        logging_section(Some(Level::INFO), "logs/master-default.log"),
                    ),
                    (
                        "sqlx".to_owned(),
                        logging_section(Some(Level::WARN), "logs/sql.log"),
                    ),
                ]
                .into(),
            ),
            opentelemetry: None,
        };

        let result = build_oop_config_and_db(&local_config, "test_gear", Some(&rendered));

        assert!(result.is_ok());
        let (final_config, _) = result.unwrap();
        let merged_logging = &final_config.logging;

        // 3 keys total: default (overridden), sqlx (from master), new_key (from local)
        assert_eq!(merged_logging.len(), 3);

        // default: overridden by local
        assert_eq!(
            merged_logging.get("default").unwrap().console_level,
            Some(Level::DEBUG)
        );
        assert_eq!(
            merged_logging.get("default").unwrap().file().unwrap(),
            "logs/local-default.log"
        );

        // sqlx: from master
        assert_eq!(
            merged_logging.get("sqlx").unwrap().console_level,
            Some(Level::WARN)
        );

        // new_key: from local
        assert_eq!(
            merged_logging.get("new_key").unwrap().console_level,
            Some(Level::TRACE)
        );
    }

    #[test]
    fn test_build_oop_config_empty_local_config_section() {
        // When local has empty config section (null), use master's
        let mut local_config = minimal_app_config();
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "config": null
            }),
        );

        let rendered = RenderedGearConfig {
            database: None,
            config: json!({"master_setting": "value"}),
            logging: None,
            opentelemetry: None,
        };

        let result = build_oop_config_and_db(&local_config, "test_gear", Some(&rendered));

        assert!(result.is_ok());
        let (final_config, _) = result.unwrap();

        // Config should be from master since local is null
        let gear_config = final_config.gears.get("test_gear").unwrap();
        assert_eq!(gear_config["config"]["master_setting"], "value");
    }

    #[test]
    fn test_build_oop_config_no_config_section_in_local() {
        // When local has no config section at all, use master's
        let mut local_config = minimal_app_config();
        local_config.gears.insert(
            "test_gear".to_owned(),
            json!({
                "database": {}  // has database but no config
            }),
        );

        let rendered = RenderedGearConfig {
            database: None,
            config: json!({"master_setting": "value"}),
            logging: None,
            opentelemetry: None,
        };

        let result = build_oop_config_and_db(&local_config, "test_gear", Some(&rendered));

        assert!(result.is_ok());
        let (final_config, _) = result.unwrap();

        // Config should be from master
        let gear_config = final_config.gears.get("test_gear").unwrap();
        assert_eq!(gear_config["config"]["master_setting"], "value");
    }
}

// =============================================================================
// advertise_uri startup validation
// =============================================================================

mod advertise_uri {
    use super::*;

    #[test]
    fn accepts_well_formed_routable_hosts() {
        validate_advertise_uri("http://billing.default.svc.cluster.local:8080", false).unwrap();
        validate_advertise_uri("https://billing:8080", false).unwrap();
        // A routable IP passes without the loopback opt-out.
        validate_advertise_uri("http://10.0.0.5:8080", false).unwrap();
    }

    #[test]
    fn rejects_malformed() {
        // Missing scheme (parses without a host).
        assert!(validate_advertise_uri("billing:8080", false).is_err());
        // Unsupported scheme.
        assert!(validate_advertise_uri("ftp://billing:8080", false).is_err());
        // Not a URL at all.
        assert!(validate_advertise_uri("not a uri", false).is_err());
        // Embedded userinfo (credential smuggling).
        assert!(validate_advertise_uri("http://billing@evil.com:8080", false).is_err());
        assert!(validate_advertise_uri("http://user:pass@billing:8080", false).is_err());
        // Missing-host authority.
        assert!(validate_advertise_uri("http://", false).is_err());
    }

    #[test]
    fn rejects_loopback_and_unspecified_by_default() {
        // ADR-0009 section 5: a loopback / unspecified advertise_uri is a
        // registered-but-unreachable instance in multi-host Profile 2 / 3.
        for uri in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            // Fully-qualified `localhost.` (trailing dot) is DNS-equivalent to
            // `localhost`, so it must still be treated as loopback.
            "http://localhost.:8080",
            "http://0.0.0.0:8080",
            "http://[::1]:8080",
            "http://[::]:8080",
        ] {
            assert!(
                validate_advertise_uri(uri, false).is_err(),
                "{uri} must be rejected without the loopback opt-out"
            );
        }
        // The generated default (unspecified bind -> loopback) is exactly the
        // unset-advertise_uri trap and must be rejected too.
        let default = default_advertise_uri("0.0.0.0:8080".parse().unwrap());
        assert!(validate_advertise_uri(&default, false).is_err());
    }

    #[test]
    fn allows_loopback_when_opted_in() {
        // Single-host / local-dev opt-out (oop_http.allow_loopback_advertise).
        for uri in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "http://localhost.:8080",
            "http://0.0.0.0:8080",
            "http://[::1]:8080",
        ] {
            validate_advertise_uri(uri, true).unwrap();
        }
        // The default still passes its own validation with the opt-out.
        let default = default_advertise_uri("0.0.0.0:8080".parse().unwrap());
        validate_advertise_uri(&default, true).unwrap();
    }
}

/// Tests for `build_platform_credentials` — the four outcomes that decide
/// whether an `OoP` gear attaches any outbound platform-plane credential. Every
/// wrong answer would otherwise be a silent "no credential", so these are
/// exercised directly.
mod platform_credentials {
    use super::*;
    use secrecy::ExposeSecret as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio_util::sync::CancellationToken;
    use toolkit_contract::runtime::config::CredentialState;
    use toolkit_security::InternalAuthConfig;

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("oop-cred-{tag}-{}-{n}", std::process::id()))
    }

    #[tokio::test]
    async fn shared_secret_yields_provider_with_the_secret() {
        let cfg = InternalAuthConfig::SharedSecret {
            secret: "shared-tok".to_owned(),
            peer_name: "toolkit-internal".to_owned(),
        };
        let (_interceptor, provider) = build_platform_credentials(&cfg, &CancellationToken::new())
            .await
            .expect("shared secret builds");
        let provider = provider.expect("shared secret yields a provider");
        match provider.current() {
            CredentialState::Available(token) => assert_eq!(token.expose_secret(), "shared-tok"),
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn kube_with_token_path_yields_provider_reading_the_file() {
        let dir = unique_dir("kube-ok");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("token");
        tokio::fs::write(&path, "kube.sa.jwt").await.unwrap();

        let cfg = InternalAuthConfig::Kube {
            audiences: vec!["toolkit-internal".to_owned()],
            token_path: Some(path),
        };
        let (_interceptor, provider) = build_platform_credentials(&cfg, &CancellationToken::new())
            .await
            .expect("kube with token path builds");
        let provider = provider.expect("kube-with-path yields a provider");
        match provider.current() {
            CredentialState::Available(token) => assert_eq!(token.expose_secret(), "kube.sa.jwt"),
            other => panic!("expected Available, got {other:?}"),
        }

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn kube_without_token_path_yields_no_provider() {
        let cfg = InternalAuthConfig::Kube {
            audiences: vec!["toolkit-internal".to_owned()],
            token_path: None,
        };
        let (_interceptor, provider) = build_platform_credentials(&cfg, &CancellationToken::new())
            .await
            .expect("kube without token path builds (inbound-only)");
        assert!(
            provider.is_none(),
            "inbound-only kube participant must attach no outbound credential"
        );
    }

    #[tokio::test]
    async fn kube_with_missing_token_file_is_an_error() {
        let path = unique_dir("kube-missing").join("token");
        let cfg = InternalAuthConfig::Kube {
            audiences: vec!["toolkit-internal".to_owned()],
            token_path: Some(path),
        };
        assert!(
            build_platform_credentials(&cfg, &CancellationToken::new())
                .await
                .is_err(),
            "a missing token file must surface a contextualized error, not Ok(None)"
        );
    }
}

/// Tests for `build_directory_client`: the bootstrap slice that must build a
/// `DirectoryService` client even when the directory is unreachable
/// (`cpt-cf-adr-eventual-readiness`).
mod directory_client_bootstrap {
    use super::*;
    use cf_system_sdks::directory::DirectoryClient;
    use tokio_util::sync::CancellationToken;
    use toolkit_security::InternalAuthConfig;

    #[tokio::test]
    async fn builds_against_unreachable_directory_without_credential() {
        // Nothing is listening on port 1, yet the client builds: bootstrap is
        // not blocked on directory reachability.
        let (client, provider) =
            build_directory_client("http://127.0.0.1:1", None, &CancellationToken::new())
                .await
                .expect("lazy directory client must build against an unreachable directory");
        assert!(
            provider.is_none(),
            "no internal_auth => no outbound credential"
        );

        // The deferred connect surfaces as an RPC error, not a hang.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            client.resolve_grpc_service("cf.directory.v1.DirectoryService"),
        )
        .await;
        assert!(outcome.is_ok(), "first RPC must not hang");
        assert!(
            outcome.unwrap().is_err(),
            "first RPC to an unreachable directory must return Err"
        );
    }

    #[tokio::test]
    async fn builds_against_unreachable_directory_with_credential() {
        // Same guarantee on the credentialed path: the client still builds and
        // a configured credential yields an outbound provider.
        let cfg = InternalAuthConfig::SharedSecret {
            secret: "shared-tok".to_owned(),
            peer_name: "toolkit-internal".to_owned(),
        };
        let (_client, provider) =
            build_directory_client("http://127.0.0.1:1", Some(&cfg), &CancellationToken::new())
                .await
                .expect("credentialed lazy client must build against an unreachable directory");
        assert!(
            provider.is_some(),
            "shared secret must yield an outbound token provider"
        );
    }

    #[tokio::test]
    async fn rejects_malformed_endpoint() {
        // A malformed endpoint must still fail fast.
        assert!(
            build_directory_client("", None, &CancellationToken::new())
                .await
                .is_err(),
            "a malformed directory endpoint must fail fast"
        );
    }

    #[tokio::test]
    async fn malformed_endpoint_is_reported_before_credential_failure() {
        // Both inputs are broken: a malformed endpoint AND a kube token_path
        // that does not exist. The endpoint is validated first, so the error is
        // the endpoint error — never the credential one (which would otherwise
        // mask the misconfigured endpoint).
        let cfg = InternalAuthConfig::Kube {
            audiences: vec!["toolkit-internal".to_owned()],
            token_path: Some(
                std::env::temp_dir()
                    .join("cf-oop-missing-token-regression")
                    .join("token"),
            ),
        };
        let Err(err) = build_directory_client("", Some(&cfg), &CancellationToken::new()).await
        else {
            panic!("a malformed endpoint must fail");
        };
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("service-account token"),
            "endpoint must be validated before credential work; got credential error: {msg}"
        );
    }
}
