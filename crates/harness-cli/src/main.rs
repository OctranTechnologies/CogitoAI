use std::path::PathBuf;

use clap::{Parser, Subcommand};
use harness_core::{discover_workspace, init_logging, HarnessConfig};

#[derive(Debug, Parser)]
#[command(name = "harness", about = "CogitoAI coding-agent harness")]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long, default_value = "info")]
    log_level: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
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
    match cli.command {
        Some(Command::Inspect { path, json }) => inspect(path, json),
        None => {
            println!(
                "CogitoAI harness workspace: {}",
                config.workspace_root.display()
            );
            Ok(())
        }
    }
}

fn inspect(path: PathBuf, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let description = discover_workspace(&path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&description)?);
    } else {
        print_summary(&description);
    }
    Ok(())
}

fn print_summary(description: &harness_core::WorkspaceDescription) {
    println!(
        "Current directory: {}",
        description.current_directory.display()
    );
    println!(
        "Repository root: {}",
        description
            .repository_root
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("<none>"))
            .display()
    );
    println!(
        "Git: available={}, branch={}, working_tree={:?}",
        description.git.available,
        description.git.branch.as_deref().unwrap_or("<none>"),
        description.git.working_tree
    );
    println!("Languages: {:?}", description.languages);
    println!(
        "Package manager: {:?}",
        description.configuration.package_manager
    );
    println!("Monorepo: {}", description.monorepo.is_monorepo);
    println!("Manifests: {}", description.manifests.len());
    println!("Instructions: {}", description.instructions.len());
    println!(
        "Test commands: {:?}",
        description.configuration.commands.test
    );
}
