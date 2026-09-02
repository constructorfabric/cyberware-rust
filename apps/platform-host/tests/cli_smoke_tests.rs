#![allow(clippy::unwrap_used, clippy::expect_used)]

//! CLI smoke tests for the `platform-host` binary.
//!
//! These mirror `cf-gears-example-server`'s CLI smoke tests: they invoke the
//! compiled binary and assert on its argument parsing, config loading, and the
//! `--print-config` / `--list-gears` / `migrate` code paths. This exercises the
//! thin `main.rs` entrypoint (which is otherwise never run under the coverage
//! harness) without standing up a full server.

use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// Path to the compiled `platform-host` binary (provided by Cargo to tests).
fn platform_host_binary() -> String {
    std::env::var("CARGO_BIN_EXE_platform-host")
        .or_else(|_| std::env::var("CARGO_BIN_EXE_PLATFORM_HOST"))
        .expect("CARGO_BIN_EXE_platform-host must be set for tests")
}

/// Run the binary with the given args and capture its output.
fn run_platform_host(args: &[&str]) -> std::process::Output {
    Command::new(platform_host_binary())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to execute platform-host")
}

/// Write a config file into `dir` and return its path as a String.
fn write_config(dir: &TempDir, name: &str, contents: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).expect("failed to write config file");
    path.to_str().unwrap().to_owned()
}

#[test]
fn test_cli_help_command() {
    let output = run_platform_host(&["--help"]);

    assert!(output.status.success(), "help command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("platform-host") || stdout.contains("Platform Host"),
        "should contain the binary name"
    );
    assert!(
        stdout.contains("Usage:") || stdout.contains("USAGE:"),
        "should contain usage information"
    );
    assert!(stdout.contains("run"), "should list the 'run' subcommand");
    assert!(
        stdout.contains("migrate"),
        "should list the 'migrate' subcommand"
    );
    assert!(
        stdout.contains("--config"),
        "should mention the --config option"
    );
}

#[test]
fn test_cli_version_command() {
    let output = run_platform_host(&["--version"]);

    assert!(output.status.success(), "version command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("platform-host"),
        "should contain the binary name"
    );
    assert!(
        stdout.chars().any(|c| c.is_ascii_digit()),
        "should contain version numbers"
    );
}

#[test]
fn test_cli_invalid_command() {
    let output = run_platform_host(&["definitely-not-a-command"]);

    assert!(!output.status.success(), "invalid command should fail");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error") || stderr.contains("unexpected") || stderr.contains("invalid"),
        "should report an error for an unknown command: {stderr}"
    );
}

#[test]
fn test_cli_missing_config_fails() {
    // An explicitly-specified config that does not exist must be an error,
    // and it is hit before any subcommand dispatch (via --list-gears).
    let output = run_platform_host(&[
        "--config",
        "/nonexistent/platform-host.yaml",
        "--list-gears",
    ]);

    assert!(
        !output.status.success(),
        "should fail when the config file does not exist"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist")
            || stderr.contains("not found")
            || stderr.contains("config"),
        "should indicate the config file could not be loaded: {stderr}"
    );
}

#[test]
fn test_cli_list_gears() {
    let dir = TempDir::new().unwrap();
    let config = write_config(
        &dir,
        "list.yaml",
        r#"
logging:
  default:
    console_level: error
    file: "list.log"
    file_level: error

gears:
  gear_alpha:
    config:
      enabled: true
  gear_beta:
    config:
      enabled: false
  gear_gamma:
    config:
      setting: "value"
"#,
    );

    let output = run_platform_host(&["--config", &config, "--list-gears"]);

    assert!(
        output.status.success(),
        "--list-gears should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Configured gears"),
        "should print the gear list header"
    );
    assert!(stdout.contains("gear_alpha"), "should list gear_alpha");
    assert!(stdout.contains("gear_beta"), "should list gear_beta");
    assert!(stdout.contains("gear_gamma"), "should list gear_gamma");

    let alpha = stdout.find("gear_alpha").unwrap();
    let beta = stdout.find("gear_beta").unwrap();
    let gamma = stdout.find("gear_gamma").unwrap();
    assert!(
        alpha < beta && beta < gamma,
        "gears should be listed in alphabetical order"
    );
}

#[test]
fn test_cli_list_gears_empty() {
    let dir = TempDir::new().unwrap();
    let config = write_config(
        &dir,
        "empty.yaml",
        r#"
logging:
  default:
    console_level: error
    file: "empty.log"
    file_level: error
"#,
    );

    let output = run_platform_host(&["--config", &config, "--list-gears"]);

    assert!(
        output.status.success(),
        "--list-gears should succeed with no gears. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Configured gears") && stdout.contains("(0)"),
        "should report zero configured gears: {stdout}"
    );
}

#[test]
fn test_cli_print_config() {
    let dir = TempDir::new().unwrap();
    let config = write_config(
        &dir,
        "print.yaml",
        r#"
logging:
  default:
    console_level: error
    file: "print.log"
    file_level: error

gears:
  gear_alpha:
    config:
      enabled: true
"#,
    );

    let output = run_platform_host(&["--config", &config, "--print-config"]);

    assert!(
        output.status.success(),
        "--print-config should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Effective configuration:"),
        "should print the effective configuration header"
    );
    assert!(
        stdout.contains("gear_alpha"),
        "effective configuration should include configured gears"
    );
}

#[test]
fn test_cli_migrate_command() {
    // Exercises the subcommand-dispatch arm (`Commands::Migrate => run_migrate`).
    // Migrations run against a throwaway SQLite home, so no external services are
    // required. We assert only that the command terminates (success or a clean
    // error) within a bound — the goal is to drive the dispatch path, not to
    // validate any specific gear's schema here.
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let config = write_config(
        &dir,
        "migrate.yaml",
        &format!(
            r#"
server:
  home_dir: "{home}"

database:
  servers:
    sqlite_users:
      engine: "sqlite"

logging:
  default:
    console_level: error
    file: "migrate.log"
    file_level: error

gears:
  resource-group:
    database:
      server: "sqlite_users"
      file: "resource_group.db"
    config: {{}}
  credstore:
    database:
      server: "sqlite_users"
      file: "credstore.db"
    config:
      vendor: "constructorfabric"
  account-management:
    database:
      server: "sqlite_users"
      file: "account_management.db"
"#,
            home = home.to_string_lossy().replace('\\', "/"),
        ),
    );

    let mut child = Command::new(platform_host_binary())
        .args(["--config", &config, "migrate"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn platform-host migrate");

    // Bounded wait so a hung migration fails loudly rather than stalling CI.
    let deadline = Duration::from_mins(2);
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().expect("failed to poll child").is_some() {
            break;
        }
        assert!(
            start.elapsed() < deadline,
            "migrate command did not finish within {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Reaching here means the `migrate` dispatch arm executed to completion.
}
