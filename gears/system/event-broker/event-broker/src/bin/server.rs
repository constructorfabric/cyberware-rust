//! Standalone-mode event-broker binary (eb-single-process-implementation
//! design.md D7): the real production `DeploymentMode::Standalone`
//! deployable, not a test fixture. Mirrors `apps/cf-gears-example-server`'s
//! CLI shape (`toolkit::bootstrap`), scoped to exactly the gears standalone
//! mode needs.

mod registered_gears;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use toolkit::bootstrap::{AppConfig, run_server};

#[derive(Parser)]
#[command(name = "cf-gears-event-broker-server")]
#[command(about = "Event Broker - standalone-mode deployment")]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log verbosity level (-v debug, -vv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the server
    Run,
    /// Do nothing (config/wiring validation only)
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = AppConfig::load_or_default(cli.config.as_ref())?;
    config.apply_cli_overrides(cli.verbose);

    match cli.command {
        None | Some(Commands::Run) => run_server(config).await,
        Some(Commands::Check) => Ok(()),
    }
}
