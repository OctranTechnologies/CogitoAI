use std::path::PathBuf;

use clap::Parser;
use harness_core::{init_logging, HarnessConfig};

#[derive(Debug, Parser)]
#[command(name = "harness", about = "CogitoAI coding-agent harness")]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = HarnessConfig {
        workspace_root: cli.workspace,
        log_level: cli.log_level,
        ..HarnessConfig::default()
    };
    config.validate()?;
    init_logging(&config.log_level)?;
    println!(
        "CogitoAI harness workspace: {}",
        config.workspace_root.display()
    );
    Ok(())
}
