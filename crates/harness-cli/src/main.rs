use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use harness_agent::{AgentLimits, AgentRunner, AgentTask, ApprovalHandler, CompactionConfig};
use harness_context::{ContextBuilder, WorkspaceMetadata};
use harness_core::{discover_workspace, init_logging, CheckpointId, HarnessConfig, SessionId};
use harness_git::{CheckpointStore, GitClient, GitDiff, ShadowCheckpointStore};
use harness_models::{
    provider_from_config, ContentBlock, FinishReason, Message, ModelConfig, ModelProvider,
    ModelRequest, ModelResponse, ProviderError, ProviderKind, ScriptedMockProvider, StreamDelta,
    ToolCall, Usage,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_session::{
    EventBus, EventId, EventPayload, EventSubscription, HarnessEvent, JsonlSessionStore,
    SessionStore,
};
use harness_tools::{CancellationToken, LocalProcessRunner, ToolRegistry};
use harness_verification::{CommandVerifier, VerificationPlan};
use serde_json::{json, Value};

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
    #[arg(long, default_value = ".cogito/sessions")]
    session_root: PathBuf,
    #[arg(long)]
    compaction_threshold: Option<u32>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    yes: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(name = ".", about = "Inspect the current workspace")]
    Workspace {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    Run {
        task: String,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Resume {
        id: String,
        task: Option<String>,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Diff {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Undo {
        id: Option<String>,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Config {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Inspect {
        #[arg(default_value = ".")]
        path: PathBuf,
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
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Inspect {
        id: String,
    },
    Resume {
        id: String,
        task: Option<String>,
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if cli.json {
                eprintln!("{}", json!({"type": "error", "error": error.to_string()}));
            } else {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = HarnessConfig {
        workspace_root: cli.workspace.clone(),
        log_level: cli.log_level.clone(),
        ..HarnessConfig::default()
    };
    config.validate()?;
    init_logging(&config.log_level)?;
    match &cli.command {
        Some(Command::Workspace { path }) => inspect(cli, &effective_path(cli, path)),
        Some(Command::Run { task, path }) => {
            run_agent(cli, task.clone(), effective_path(cli, path), None)
        }
        Some(Command::Sessions { limit }) => sessions(cli, *limit),
        Some(Command::Resume { id, task, path }) => {
            resume_session(cli, id, task.clone(), path.clone())
        }
        Some(Command::Status { session, path }) => status(
            cli,
            session.as_deref(),
            path.as_ref().map(|path| path.as_path()),
        ),
        Some(Command::Diff { file, path }) => diff(
            cli,
            file.as_ref().map(|path| path.as_path()),
            path.as_ref().map(|path| path.as_path()),
        ),
        Some(Command::Undo { id, path }) => {
            undo(cli, id.as_deref(), path.as_ref().map(|path| path.as_path()))
        }
        Some(Command::Config { path }) => {
            config_command(cli, path.as_ref().map(|path| path.as_path()))
        }
        Some(Command::Inspect { path }) => inspect(cli, &effective_path(cli, path)),
        Some(Command::Ask { prompt, stream }) => ask(cli, prompt, *stream),
        Some(Command::ModelInfo) => model_info(cli),
        Some(Command::Agent { task, path }) => {
            run_agent(cli, task.clone(), effective_path(cli, path), None)
        }
        Some(Command::Session { command }) => session_command(cli, command),
        None => {
            if cli.json {
                println!("{}", json!({"type": "workspace", "path": cli.workspace}));
            } else {
                println!("CogitoAI harness workspace: {}", cli.workspace.display());
            }
            Ok(())
        }
    }
}

fn effective_path(cli: &Cli, path: &Path) -> PathBuf {
    if path == Path::new(".") {
        cli.workspace.clone()
    } else {
        path.to_path_buf()
    }
}

fn inspect(cli: &Cli, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let description = discover_workspace(path)?;
    if cli.json {
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
                if !cli.json {
                    print!("{text}");
                    let _ = io::stdout().flush();
                }
            }
            Ok(())
        })?
    } else {
        provider.complete(&request)?
    };
    if cli.json {
        println!("{}", serde_json::to_string(&response)?);
    } else {
        if stream {
            println!();
        } else if !response.text().is_empty() {
            println!("{}", response.text());
        }
        if !response.tool_calls.is_empty() {
            println!(
                "tool calls: {}",
                serde_json::to_string_pretty(&response.tool_calls)?
            );
        }
    }
    Ok(())
}

fn model_info(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let config = model_config(cli)?;
    let provider = provider_from_config(&config)?;
    let info = json!({
        "provider": provider.name(),
        "model": config.model,
        "capabilities": provider.capabilities(),
    });
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("provider: {}", provider.name());
        println!("model: {}", config.model);
        println!(
            "capabilities: {}",
            serde_json::to_string_pretty(&provider.capabilities())?
        );
    }
    Ok(())
}

fn sessions(cli: &Cli, limit: usize) -> Result<(), Box<dyn std::error::Error>> {
    let store = JsonlSessionStore::new(&cli.session_root)?;
    print_sessions(&store.recent(limit)?, cli.json)
}

fn resume_session(
    cli: &Cli,
    id: &str,
    task: Option<String>,
    path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::new(id.to_owned())?;
    let store = JsonlSessionStore::new(&cli.session_root)?;
    let existing = store.load(&session_id)?;
    let path = path.unwrap_or_else(|| existing.workspace_root.clone());
    let task = task.unwrap_or_else(|| {
        "Continue from the compacted session state and finish the remaining work.".to_owned()
    });
    run_agent(cli, task, path, Some(session_id))
}

fn status(
    cli: &Cli,
    session_id: Option<&str>,
    path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_path = path.unwrap_or(&cli.workspace);
    let workspace = GitClient::open(workspace_path)
        .ok()
        .map(|client| client.status())
        .transpose()?;
    let session = if let Some(id) = session_id {
        Some(JsonlSessionStore::new(&cli.session_root)?.load(&SessionId::new(id.to_owned())?)?)
    } else {
        JsonlSessionStore::new(&cli.session_root)?
            .recent(1)?
            .first()
            .map(|summary| JsonlSessionStore::new(&cli.session_root)?.load(&summary.id))
            .transpose()?
    };
    if cli.json {
        println!("{}", json!({"workspace": workspace, "session": session}));
    } else {
        match workspace {
            Some(status) => {
                println!("workspace: {}", status.repository_root.display());
                println!(
                    "branch: {}",
                    status.branch.as_deref().unwrap_or("<detached>")
                );
                println!("clean: {}", status.is_clean);
                println!("changed: {}", status.changed_files.join(", "));
            }
            None => println!("workspace: not a Git repository"),
        }
        match session {
            Some(session) => {
                let state = session.state()?;
                println!("session: {}", session.id);
                println!("status: {:?}", state.status);
                println!("events: {}", session.events.len());
                println!("compactions: {}", state.context_compactions);
            }
            None => println!("session: none"),
        }
    }
    Ok(())
}

fn diff(
    cli: &Cli,
    file: Option<&Path>,
    path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = GitClient::open(path.unwrap_or(&cli.workspace))?;
    let diff = match file {
        Some(file) => client.diff_file(file)?,
        None => client.diff()?,
    };
    print_diff(&diff, cli.json)
}

fn undo(
    cli: &Cli,
    id: Option<&str>,
    path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = path.unwrap_or(&cli.workspace);
    let store = ShadowCheckpointStore::new(workspace.join(".cogito/checkpoints"))?;
    let checkpoint_id: CheckpointId = if let Some(id) = id {
        CheckpointId::new(id.to_owned())?
    } else {
        store
            .list()?
            .into_iter()
            .max_by_key(|checkpoint| checkpoint.created_at.clone())
            .map(|checkpoint| checkpoint.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no checkpoints found"))?
    };
    let report = store.undo(&checkpoint_id)?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("restored checkpoint: {}", report.checkpoint_id);
        println!("files: {}", report.restored_files.len());
        if !report.conflicts.is_empty() {
            println!("conflicts: {}", report.conflicts.len());
        }
    }
    Ok(())
}

fn config_command(cli: &Cli, path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let description = discover_workspace(path.unwrap_or(&cli.workspace))?;
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&description.configuration)?
        );
    } else {
        println!("workspace: {}", description.current_directory.display());
        println!(
            "package manager: {:?}",
            description.configuration.package_manager
        );
        println!("source: {:?}", description.configuration.source);
        println!(
            "format commands: {}",
            description.configuration.commands.format.len()
        );
        println!(
            "lint commands: {}",
            description.configuration.commands.lint.len()
        );
        println!(
            "typecheck commands: {}",
            description.configuration.commands.typecheck.len()
        );
        println!(
            "build commands: {}",
            description.configuration.commands.build.len()
        );
        println!(
            "test commands: {}",
            description.configuration.commands.test.len()
        );
    }
    Ok(())
}

fn session_command(cli: &Cli, command: &SessionCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        SessionCommand::List { limit } => sessions(cli, *limit),
        SessionCommand::Inspect { id } => {
            let store = JsonlSessionStore::new(&cli.session_root)?;
            let report = store.load_with_report(&SessionId::new(id.to_owned())?)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let state = report.session.state()?;
                println!("session: {}", report.session.id);
                println!("workspace: {}", report.session.workspace_root.display());
                println!("status: {:?}", state.status);
                println!("events: {}", report.session.events.len());
                println!("compactions: {}", state.context_compactions);
                if let Some(state) = state.continuation {
                    println!("continuation:\n{}", state.render());
                }
            }
            Ok(())
        }
        SessionCommand::Resume { id, task, path } => {
            resume_session(cli, id, task.clone(), path.clone())
        }
    }
}

fn run_agent(
    cli: &Cli,
    task: String,
    path: PathBuf,
    resume_session: Option<SessionId>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = std::fs::canonicalize(&path)?;
    let description = discover_workspace(&path)?;
    let model_config = model_config(cli)?;
    let provider = provider_for_agent(&model_config)?;
    let project_root = description
        .repository_root
        .clone()
        .unwrap_or_else(|| description.current_directory.clone());
    let policy: Arc<dyn Policy> = if project_root.join(".agent/config.toml").is_file() {
        Arc::new(PolicyEngine::from_file(
            &project_root.join(".agent/config.toml"),
            &project_root,
        )?)
    } else {
        Arc::new(PolicyEngine::new(ExecutionMode::Normal, &project_root))
    };
    let event_bus = EventBus::new();
    let sessions = Arc::new(JsonlSessionStore::with_event_bus(
        &cli.session_root,
        event_bus.clone(),
    )?);
    let checkpoints: Option<Arc<dyn CheckpointStore>> = if GitClient::open(&path).is_ok() {
        Some(Arc::new(ShadowCheckpointStore::with_event_bus(
            path.join(".cogito/checkpoints"),
            Some(event_bus.clone()),
        )?))
    } else {
        None
    };
    let git_status = GitClient::open(&path)
        .ok()
        .and_then(|client| client.status().ok());
    let workspace = WorkspaceMetadata {
        root: Some(project_root.clone()),
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
    let verification_plan = if model_config.provider == ProviderKind::Mock {
        None
    } else {
        Some(VerificationPlan::all(&description))
    };
    let agent_task = AgentTask {
        workspace_root: path,
        user_task: task,
        system_instructions:
            "You are the CogitoAI coding agent. Follow project instructions and use tools safely."
                .to_owned(),
        workspace,
        instructions: description.instructions,
        git_status,
        verification_plan,
        resume_session,
        ..AgentTask::default()
    };
    let event_output = Arc::new(EventOutput::new(cli.json));
    let _subscription = event_output.subscribe(&event_bus);
    let cancellation = CancellationToken::new();
    let handler_token = cancellation.clone();
    ctrlc::set_handler(move || handler_token.cancel())?;
    let runner = AgentRunner::new(
        provider,
        model_config.model,
        ToolRegistry::with_workspace_tools_cancellation(cancellation.clone()),
        policy,
        sessions,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(CliApproval {
            auto_approve: cli.yes,
        }),
    )
    .with_event_bus(event_bus)
    .with_compaction_config(CompactionConfig {
        threshold_tokens: cli
            .compaction_threshold
            .unwrap_or_else(|| CompactionConfig::default().threshold_tokens),
        ..CompactionConfig::default()
    })
    .with_verifier(Arc::new(
        CommandVerifier::new(Arc::new(LocalProcessRunner)).with_cancellation(cancellation.clone()),
    ));
    let mut runner = runner;
    if let Some(checkpoints) = checkpoints {
        runner = runner.with_checkpoints(checkpoints);
    }
    let outcome = runner.run(&agent_task, &cancellation)?;
    print_completion(cli, &outcome);
    Ok(())
}

fn provider_for_agent(config: &ModelConfig) -> Result<Arc<dyn ModelProvider>, ProviderError> {
    if config.provider == ProviderKind::Mock {
        return Ok(Arc::new(ScriptedMockProvider::new(
            config.model.clone(),
            vec![
                tool_response("list_directory", json!({"path": "."})),
                tool_response(
                    "write_file",
                    json!({
                        "path": "mock-output.txt",
                        "content": "Generated by the CLI mock workflow."
                    }),
                ),
                text_response("Mock coding workflow completed."),
            ],
        )));
    }
    Ok(Arc::from(provider_from_config(config)?))
}

fn tool_response(name: &str, arguments: Value) -> ModelResponse {
    ModelResponse {
        id: format!("mock-{name}"),
        model: "mock".to_owned(),
        content: Vec::new(),
        tool_calls: vec![ToolCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments,
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: Some(Usage::new(1, 1)),
    }
}

fn text_response(text: &str) -> ModelResponse {
    ModelResponse {
        id: "mock-final".to_owned(),
        model: "mock".to_owned(),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(Usage::new(1, 1)),
    }
}

fn print_completion(cli: &Cli, outcome: &harness_agent::AgentOutcome) {
    if !cli.json {
        println!(
            "completed session {} (turns={}, tool_calls={}, model_tokens={})",
            outcome.session_id, outcome.turns, outcome.tool_calls, outcome.model_tokens
        );
    }
}

struct EventOutput {
    json: bool,
    seen: Mutex<HashSet<EventId>>,
}

impl EventOutput {
    fn new(json: bool) -> Self {
        Self {
            json,
            seen: Mutex::new(HashSet::new()),
        }
    }

    fn subscribe(self: &Arc<Self>, bus: &EventBus) -> EventSubscription {
        let output = Arc::clone(self);
        bus.subscribe(Arc::new(move |event: &HarnessEvent| output.print(event)))
    }

    fn print(&self, event: &HarnessEvent) {
        if !self
            .seen
            .lock()
            .expect("event output lock poisoned")
            .insert(event.event_id.clone())
        {
            return;
        }
        if self.json {
            println!(
                "{}",
                serde_json::to_string(event).expect("event serialization failed")
            );
            return;
        }
        match &event.payload {
            EventPayload::AssistantDelta { text } => {
                print!("{text}");
                let _ = io::stdout().flush();
            }
            EventPayload::ToolRequested { tool, .. } => println!("[tool] requested {tool}"),
            EventPayload::ToolApproved { tool, .. } => println!("[approval] approved {tool}"),
            EventPayload::ToolDenied { tool, reason } => {
                println!("[approval] denied {tool}: {reason}");
            }
            EventPayload::ToolStarted { tool } => println!("[tool] started {tool}"),
            EventPayload::ToolCompleted { tool } => println!("[tool] completed {tool}"),
            EventPayload::ToolFailed { tool, error } => println!("[tool] failed {tool}: {error}"),
            EventPayload::VerificationStarted { commands } => {
                println!("[verify] {}", commands.join("; "));
            }
            EventPayload::VerificationResult {
                command,
                category,
                passed,
                exit_code,
                ..
            } => println!(
                "[verify] {category} {} passed={passed} exit_code={exit_code:?}",
                command
            ),
            EventPayload::ContextCompacted {
                removed_items,
                summary,
                ..
            } => println!("[context] compacted removed_items={removed_items}\n{summary}"),
            EventPayload::SessionCompleted { reason } => println!(
                "[session] completed{}",
                reason
                    .as_deref()
                    .map_or(String::new(), |reason| format!(": {reason}"))
            ),
            EventPayload::SessionFailed { error } => println!("[session] failed: {error}"),
            _ => {}
        }
    }
}

struct CliApproval {
    auto_approve: bool,
}

impl ApprovalHandler for CliApproval {
    fn request(
        &self,
        request: &harness_tools::ToolRequest,
    ) -> Result<bool, harness_agent::AgentError> {
        if self.auto_approve {
            return Ok(true);
        }
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

fn print_sessions(
    sessions: &[harness_session::SessionSummary],
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(sessions)?);
    } else if sessions.is_empty() {
        println!("no sessions found");
    } else {
        for session in sessions {
            println!(
                "{}  {:?}  events={}  compactions={}  {}",
                session.id,
                session.status,
                session.event_count,
                session.context_compactions,
                session.workspace_root.display()
            );
        }
    }
    Ok(())
}

fn print_diff(diff: &GitDiff, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(diff)?);
    } else if diff.staged.is_empty() && diff.unstaged.is_empty() {
        println!("working tree clean");
    } else {
        if !diff.staged.is_empty() {
            println!("--- staged ---\n{}", diff.staged);
        }
        if !diff.unstaged.is_empty() {
            println!("--- unstaged ---\n{}", diff.unstaged);
        }
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
            .unwrap_or_else(|| Path::new("<none>"))
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
