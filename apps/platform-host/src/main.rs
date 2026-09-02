mod registered_gears;

use anyhow::Result;
use clap::{Parser, Subcommand};
use mimalloc::MiMalloc;
use std::path::PathBuf;
use toolkit::bootstrap::{AppConfig, list_gear_names, run_migrate, run_server};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// CF/Gears Platform Host - trust-coupled core + system gears.
#[derive(Parser)]
#[command(name = "platform-host")]
#[command(about = "CF/Gears Platform Host - trust-coupled core + system gears")]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Print effective configuration (YAML) and exit
    #[arg(long)]
    print_config: bool,

    /// List all configured gear names and exit
    #[arg(long)]
    list_gears: bool,

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
    /// Run database migrations and exit (for cloud deployments)
    Migrate,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = AppConfig::load_or_default(cli.config.as_ref())?;
    config.apply_cli_overrides(cli.verbose);

    if cli.print_config {
        println!("Effective configuration:\n{}", config.to_yaml()?);
        return Ok(());
    }

    if cli.list_gears {
        let gears = list_gear_names(&config);
        println!("Configured gears ({}):", gears.len());
        for gear in gears {
            println!("  - {gear}");
        }
        return Ok(());
    }

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => run_server(config).await,
        Commands::Migrate => run_migrate(config).await,
    }
}
