use std::path::PathBuf;

use clap::{Parser, Subcommand};
use harness_core::{discover_workspace, init_logging, HarnessConfig};
use harness_models::{
    provider_from_config, Message, ModelConfig, ModelRequest, ProviderError, StreamDelta,
};
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "harness", about = "CogitoAI coding-agent harness")]
struct Cli {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long, default_value = "info")]
    log_level: String,
    #[arg(long)]
    model_provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
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
    Ask {
        prompt: String,
        #[arg(long)]
        stream: bool,
    },
    ModelInfo,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = HarnessConfig {
        workspace_root: cli.workspace.clone(),
        log_level: cli.log_level.clone(),
        ..HarnessConfig::default()
    };
    config.validate()?;
    init_logging(&config.log_level)?;
    match cli.command {
        Some(Command::Inspect { path, json }) => inspect(path, json),
        Some(Command::Ask { ref prompt, stream }) => ask(&cli, prompt, stream),
        Some(Command::ModelInfo) => model_info(&cli),
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

fn model_config(cli: &Cli) -> Result<ModelConfig, ProviderError> {
    let mut config = ModelConfig::from_env();
    if let Some(provider) = &cli.model_provider {
        config.provider =
            serde_json::from_value(Value::String(provider.clone())).map_err(|error| {
                ProviderError::InvalidResponse {
                    provider: "configuration",
                    reason: error.to_string(),
                }
            })?;
    }
    if let Some(model) = &cli.model {
        config.model = model.clone();
    }
    Ok(config)
}

fn ask(cli: &Cli, prompt: &str, stream: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = model_config(cli)?;
    let provider = provider_from_config(&config)?;
    let request = ModelRequest::new(config.model.clone(), vec![Message::user_text(prompt)]);
    let response = if stream {
        provider.stream(&request, &mut |delta: StreamDelta| {
            if let harness_models::StreamDeltaKind::Text { text } = &delta.delta {
                print!("{text}");
            }
            Ok(())
        })?
    } else {
        provider.complete(&request)?
    };
    if stream {
        println!();
    }
    if !stream && !response.text().is_empty() {
        println!("{}", response.text());
    }
    if !response.tool_calls.is_empty() {
        println!(
            "tool calls: {}",
            serde_json::to_string_pretty(&response.tool_calls)?
        );
    }
    Ok(())
}

fn model_info(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = model_config(cli)?;
    let provider = provider_from_config(&config)?;
    println!("provider: {}", provider.name());
    println!("model: {}", config.model);
    println!(
        "capabilities: {}",
        serde_json::to_string_pretty(&provider.capabilities())?
    );
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
