//! Standalone out-of-process (Profile 3) entrypoint for the
//! `api-contracts-consumer` gear.
//!
//! Built only with `--features oop_module` (see `Cargo.toml`). Runs the gear as
//! its own process/pod: connects to the platform host's `DirectoryService` (via
//! `TOOLKIT_DIRECTORY_ENDPOINT`), registers its REST endpoint, authenticates
//! tenant-plane requests locally (embedded authn stack), and serves
//! `POST /api-contracts-consumer/v1/charge`. That handler resolves the
//! `PaymentApi` contract from the `ClientHub` — wired by `#[toolkit::consumes]` to
//! a directory-resolving REST client — and forwards the charge to the
//! `api-contracts` PROVIDER pod over REST (`OoP` gear-to-gear).

mod registered_gears;

use clap::Parser;
use mimalloc::MiMalloc;
use std::path::PathBuf;
use toolkit::bootstrap::oop::{OopRunOptions, run_oop_with_options};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// `api-contracts-consumer` `OoP` gear.
#[derive(Parser)]
#[command(name = "api-contracts-consumer-oop")]
#[command(
    about = "api-contracts-consumer - resolves PaymentApi from another OoP pod over REST (Profile 3)"
)]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Path to configuration file.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log verbosity level (-v debug, -vv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let opts = OopRunOptions {
        gear_name: "api-contracts-consumer".to_owned(),
        config_path: cli.config,
        verbose: cli.verbose,
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        ..Default::default()
    };

    run_oop_with_options(opts).await
}
