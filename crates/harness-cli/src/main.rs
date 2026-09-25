use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use harness_agent::{AgentLimits, AgentRunner, AgentTask, ApprovalHandler};
use harness_context::{ContextBuilder, WorkspaceMetadata};
use harness_core::{discover_workspace, init_logging, HarnessConfig};
use harness_git::GitClient;
use harness_models::{
    provider_from_config, Message, ModelConfig, ModelRequest, ProviderError, StreamDelta,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_session::{EventBus, JsonlSessionStore};
use harness_tools::{CancellationToken, LocalProcessRunner, ToolRegistry};
use harness_verification::{CommandVerifier, VerificationPlan};
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
    Agent {
        task: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = ".cogito/sessions")]
        session_root: PathBuf,
    },
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
        Some(Command::Agent {
            ref task,
            ref path,
            ref session_root,
        }) => run_agent(&cli, task.clone(), path.clone(), session_root.clone()),
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

fn run_agent(
    cli: &Cli,
    task: String,
    path: PathBuf,
    session_root: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let description = discover_workspace(&path)?;
    let model_config = model_config(cli)?;
    let provider = provider_from_config(&model_config)?;
    let provider: Arc<dyn harness_models::ModelProvider> = Arc::from(provider);
    let policy: Arc<dyn Policy> = if path.join(".agent/config.toml").is_file() {
        Arc::new(PolicyEngine::from_file(
            &path.join(".agent/config.toml"),
            &path,
        )?)
    } else {
        Arc::new(PolicyEngine::new(ExecutionMode::Normal, &path))
    };
    let sessions = Arc::new(JsonlSessionStore::new(session_root)?);
    let git_status = GitClient::open(&path)
        .ok()
        .and_then(|client| client.status().ok());
    let workspace = WorkspaceMetadata {
        root: description
            .repository_root
            .clone()
            .or_else(|| Some(description.current_directory.clone())),
        branch: description.git.branch.clone(),
        monorepo: description.monorepo.is_monorepo,
        languages: description
            .languages
            .iter()
            .map(|language| format!("{language:?}"))
            .collect(),
        manifests: description
            .manifests
            .iter()
            .map(|manifest| manifest.path.display().to_string())
            .collect(),
        details: Default::default(),
    };
    let verification_plan = VerificationPlan::all(&description);
    let agent_task = AgentTask {
        workspace_root: path,
        user_task: task,
        system_instructions:
            "You are the CogitoAI coding agent. Follow project instructions and use tools safely."
                .to_owned(),
        workspace,
        instructions: description.instructions,
        git_status,
        verification_plan: Some(verification_plan),
        ..AgentTask::default()
    };
    let event_bus = EventBus::new();
    let _subscription = event_bus.subscribe(Arc::new(|event: &harness_session::HarnessEvent| {
        if let harness_session::EventPayload::AssistantDelta { text } = &event.payload {
            print!("{text}");
            let _ = io::stdout().flush();
        }
    }));
    let runner = AgentRunner::new(
        provider,
        model_config.model,
        ToolRegistry::with_workspace_tools(),
        policy,
        sessions,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(CliApproval),
    )
    .with_event_bus(event_bus)
    .with_verifier(Arc::new(CommandVerifier::new(Arc::new(LocalProcessRunner))));
    let cancellation = CancellationToken::new();
    let handler_token = cancellation.clone();
    ctrlc::set_handler(move || handler_token.cancel())?;
    let outcome = runner.run(&agent_task, &cancellation)?;
    println!("\n{}", outcome.final_message);
    println!("session: {}", outcome.session_id);
    Ok(())
}

struct CliApproval;

impl ApprovalHandler for CliApproval {
    fn request(
        &self,
        request: &harness_tools::ToolRequest,
    ) -> Result<bool, harness_agent::AgentError> {
        eprint!("Approve tool {}? [y/N] ", request.name);
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|error| harness_agent::AgentError::Core(error.to_string()))?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }
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
