//! Standalone `OoP` entrypoint for the hello gear.
//!
//! Runs the gear as its own process: it connects to the platform host's
//! `DirectoryService` (via `TOOLKIT_DIRECTORY_ENDPOINT`), registers its REST
//! endpoint (from `oop_http.advertise_uri`), serves `/hello/v1/ping`, and
//! deregisters on shutdown.

mod registered_gears;

use clap::Parser;
use mimalloc::MiMalloc;
use std::path::PathBuf;
use toolkit::bootstrap::oop::{OopRunOptions, run_oop_with_options};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Hello `OoP` gear.
#[derive(Parser)]
#[command(name = "hello-oop")]
#[command(about = "Hello - minimal REST OoP demo gear")]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log verbosity level (-v debug, -vv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let opts = OopRunOptions {
        gear_name: "hello".to_owned(),
        config_path: cli.config,
        verbose: cli.verbose,
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        ..Default::default()
    };

    run_oop_with_options(opts).await
}
