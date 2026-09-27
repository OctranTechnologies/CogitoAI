use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
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
use tui::{StartupInfo, Tui, TuiSender};

mod tui;

#[derive(Clone, Debug, Parser)]
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

#[derive(Clone, Debug, Subcommand)]
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
    Tui,
}

#[derive(Clone, Debug, Subcommand)]
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
        Some(Command::Tui) => interactive(cli),
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

fn interactive(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.json {
        return Err(
            "the interactive TUI cannot be combined with --json; use --json with a command".into(),
        );
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("the interactive TUI needs a terminal on stdin and stdout; use `harness run` for scripts and pipes".into());
    }
    if std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb")) {
        return plain_interactive(cli);
    }

    let workspace_path = std::fs::canonicalize(effective_path(cli, Path::new(".")))?;
    let workspace = discover_workspace(&workspace_path)?;
    let model = model_config(cli)?;
    let mut notices = Vec::new();
    if workspace.repository_root.is_none() {
        notices.push(
            "Notice: this workspace is not a Git repository; checkpoints are unavailable"
                .to_owned(),
        );
    }
    if model.provider != ProviderKind::Mock && std::env::var_os(&model.api_key_env).is_none() {
        notices.push(format!(
            "Notice: set {} to connect to the configured provider",
            model.api_key_env
        ));
    }
    let startup = StartupInfo {
        model: model.model,
        provider: format!("{:?}", model.provider),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        workspace: display_workspace_path(&workspace_path),
        notice: (!notices.is_empty()).then(|| notices.join("  |  ")),
    };
    let cli = cli.clone();
    let mut tui = Tui::new(startup)?;
    tui.run(move |line, tui| dispatch_interactive(&cli, line, tui))?;
    Ok(())
}

fn display_workspace_path(path: &Path) -> String {
    let displayed = path.display().to_string();
    #[cfg(windows)]
    {
        if let Some(unc_path) = displayed.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc_path}");
        }
        displayed
            .strip_prefix(r"\\?\")
            .unwrap_or(&displayed)
            .to_owned()
    }
    #[cfg(not(windows))]
    displayed
}

/// A line-oriented fallback for terminals that report `TERM=dumb`.
fn plain_interactive(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = std::fs::canonicalize(effective_path(cli, Path::new(".")))?;
    let model = model_config(cli)?;
    println!(
        "[<>] COGITOAI harness v{} (plain terminal mode)\nModel: {}  |  Provider: {:?}\nWorkspace: {}",
        env!("CARGO_PKG_VERSION"),
        model.model,
        model.provider,
        display_workspace_path(&workspace)
    );
    if model.provider != ProviderKind::Mock && std::env::var_os(&model.api_key_env).is_none() {
        println!(
            "Notice: set {} to connect to the configured provider",
            model.api_key_env
        );
    }
    println!("Enter a task, /help, or /exit. Ctrl+D exits.");
    loop {
        print!("harness> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/exit" | "/quit") {
            break;
        }
        if line == "/help" {
            println!("Type a task to run it. /inspect and /sessions show workspace data; /exit leaves the prompt.");
            continue;
        }
        if line == "/inspect" {
            inspect(cli, &effective_path(cli, Path::new(".")))?;
            continue;
        }
        if line == "/sessions" {
            sessions(cli, 20)?;
            continue;
        }
        if let Some(task) = line.strip_prefix("/run ") {
            run_agent(
                cli,
                task.to_owned(),
                effective_path(cli, Path::new(".")),
                None,
            )?;
            break;
        }
        run_agent(
            cli,
            line.to_owned(),
            effective_path(cli, Path::new(".")),
            None,
        )?;
        // The line-oriented fallback has no persistent event loop for signal
        // registration. Exit after a run so its one-shot Ctrl+C handler is not
        // installed a second time in the same process.
        break;
    }
    Ok(())
}

fn dispatch_interactive(cli: &Cli, line: String, tui: &mut Tui) -> Result<(), String> {
    if !line.starts_with('/') {
        return start_interactive_run(cli, line, effective_path(cli, Path::new(".")), None, tui);
    }
    let (command, arguments) = line.split_once(char::is_whitespace).unwrap_or((&line, ""));
    let arguments = arguments.trim();
    match command {
        "/help" => {
            tui.add_activity("Commands: /run <task>, /resume <id> [task], /inspect [path], /sessions, /status [id], /diff [file], /undo [id], /config, /model, /clear, /cancel, /exit".to_owned());
            Ok(())
        }
        "/run" => {
            if arguments.is_empty() {
                Err("usage: /run <task>".to_owned())
            } else {
                start_interactive_run(
                    cli,
                    arguments.to_owned(),
                    effective_path(cli, Path::new(".")),
                    None,
                    tui,
                )
            }
        }
        "/resume" => {
            let mut fields = arguments.splitn(2, char::is_whitespace);
            let id = fields.next().unwrap_or_default();
            if id.is_empty() {
                return Err("usage: /resume <session-id> [task]".to_owned());
            }
            let session_id = SessionId::new(id.to_owned()).map_err(|error| error.to_string())?;
            let store =
                JsonlSessionStore::new(&cli.session_root).map_err(|error| error.to_string())?;
            let existing = store.load(&session_id).map_err(|error| error.to_string())?;
            let task = fields
                .next()
                .map(str::trim)
                .filter(|task| !task.is_empty())
                .unwrap_or(
                    "Continue from the compacted session state and finish the remaining work.",
                )
                .to_owned();
            start_interactive_run(cli, task, existing.workspace_root, Some(session_id), tui)
        }
        "/clear" => {
            tui.clear_activity();
            Ok(())
        }
        "/cancel" => {
            tui.cancel_run();
            Ok(())
        }
        "/exit" | "/quit" => {
            tui.request_exit();
            Ok(())
        }
        "/inspect" => {
            let path = if arguments.is_empty() {
                effective_path(cli, Path::new("."))
            } else {
                PathBuf::from(arguments.trim_matches('"'))
            };
            run_visible_command(tui, || inspect(cli, &path))
        }
        "/sessions" => {
            let limit = if arguments.is_empty() {
                20
            } else {
                arguments
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?
            };
            run_visible_command(tui, || sessions(cli, limit))
        }
        "/status" => run_visible_command(tui, || {
            status(cli, (!arguments.is_empty()).then_some(arguments), None)
        }),
        "/diff" => {
            let file = (!arguments.is_empty()).then(|| PathBuf::from(arguments.trim_matches('"')));
            run_visible_command(tui, || diff(cli, file.as_deref(), None))
        }
        "/undo" => run_visible_command(tui, || {
            undo(cli, (!arguments.is_empty()).then_some(arguments), None)
        }),
        "/config" => run_visible_command(tui, || config_command(cli, None)),
        "/model" => run_visible_command(tui, || model_info(cli)),
        _ => Err(format!(
            "unknown command {command}; type /help for available commands"
        )),
    }
}

fn run_visible_command(
    tui: &mut Tui,
    command: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), String> {
    tui.suspend().map_err(|error| error.to_string())?;
    let result = command();
    let resume_result = tui.resume().map_err(|error| error.to_string());
    result.map_err(|error| error.to_string())?;
    resume_result?;
    tui.add_activity("Command finished; full output is in the terminal scrollback".to_owned());
    Ok(())
}

fn start_interactive_run(
    cli: &Cli,
    task: String,
    workspace: PathBuf,
    resume_session: Option<SessionId>,
    tui: &mut Tui,
) -> Result<(), String> {
    let cancellation = CancellationToken::new();
    if !tui.start_run(&task, cancellation.clone()) {
        return Err("a task is already running".to_owned());
    }
    let sender = tui.sender();
    let failed_sender = sender.clone();
    let cli = cli.clone();
    let spawn = std::thread::Builder::new()
        .name("harness-agent-tui".to_owned())
        .spawn(move || {
            let result = run_agent_with_ui(
                &cli,
                task,
                workspace,
                resume_session,
                Some(sender.clone()),
                Some(cancellation),
            )
            .map_err(|error| error.to_string());
            sender.run_finished(result);
        });
    if let Err(error) = spawn {
        failed_sender.run_finished(Err(error.to_string()));
    }
    Ok(())
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
    run_agent_with_ui(cli, task, path, resume_session, None, None)
}

fn run_agent_with_ui(
    cli: &Cli,
    task: String,
    path: PathBuf,
    resume_session: Option<SessionId>,
    tui: Option<TuiSender>,
    cancellation: Option<CancellationToken>,
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
    // The mock provider is a stand-in for a real model, so it runs the same
    // verification and checkpoint machinery a real run does. Only a workspace
    // with a detectable toolchain produces commands, so a plain directory still
    // verifies nothing.
    let verification_plan = Some(VerificationPlan::all(&description));
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
    let event_output = Arc::new(EventOutput::new(cli.json, tui.clone()));
    let _subscription = event_output.subscribe(&event_bus);
    let cancellation = cancellation.unwrap_or_default();
    if tui.is_none() {
        let handler_token = cancellation.clone();
        ctrlc::set_handler(move || handler_token.cancel())?;
    }
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
            tui: tui.clone(),
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
    print_completion(cli, &outcome, tui.as_ref());
    Ok(())
}

fn provider_for_agent(config: &ModelConfig) -> Result<Arc<dyn ModelProvider>, ProviderError> {
    if config.provider == ProviderKind::Mock {
        if let Some(repair) = mock_repair_script()? {
            return Ok(Arc::new(ScriptedMockProvider::new(
                config.model.clone(),
                vec![
                    tool_response("read_file", json!({ "path": repair.path })),
                    tool_response(
                        "write_file",
                        json!({ "path": repair.path, "content": repair.broken }),
                    ),
                    tool_response(
                        "write_file",
                        json!({ "path": repair.path, "content": repair.fixed }),
                    ),
                    text_response("Mock repair workflow completed."),
                ],
            )));
        }
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

/// A deliberately broken edit followed by a corrective one, so the mock can
/// exercise the real verify-fail-then-correct loop.
///
/// Supplied through `COGITO_MOCK_REPAIR` as JSON because file contents contain
/// newlines that a plain environment variable cannot carry portably. This is a
/// test hook for the mock provider only; it never affects a real provider.
struct MockRepair {
    path: String,
    broken: String,
    fixed: String,
}

fn mock_repair_script() -> Result<Option<MockRepair>, ProviderError> {
    let Some(raw) = std::env::var_os("COGITO_MOCK_REPAIR") else {
        return Ok(None);
    };
    let invalid = |reason: String| ProviderError::Configuration {
        reason: format!("COGITO_MOCK_REPAIR: {reason}"),
    };
    let value: Value = serde_json::from_str(&raw.to_string_lossy())
        .map_err(|error| invalid(format!("expected JSON ({error})")))?;
    let field = |name: &str| -> Result<String, ProviderError> {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid(format!("missing string field `{name}`")))
    };
    let repair = MockRepair {
        path: field("path")?,
        broken: field("broken")?,
        fixed: field("fixed")?,
    };
    if repair.path.trim().is_empty() {
        return Err(invalid("path must not be empty".to_owned()));
    }
    if repair.broken == repair.fixed {
        return Err(invalid(
            "`broken` and `fixed` must differ for the repair to be meaningful".to_owned(),
        ));
    }
    Ok(Some(repair))
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

fn print_completion(cli: &Cli, outcome: &harness_agent::AgentOutcome, tui: Option<&TuiSender>) {
    if let Some(tui) = tui {
        tui.activity(format!(
            "Session {} finished · turns={} · tools={} · tokens={}",
            outcome.session_id, outcome.turns, outcome.tool_calls, outcome.model_tokens
        ));
    } else if !cli.json {
        println!(
            "completed session {} (turns={}, tool_calls={}, model_tokens={})",
            outcome.session_id, outcome.turns, outcome.tool_calls, outcome.model_tokens
        );
    }
}

struct EventOutput {
    json: bool,
    tui: Option<TuiSender>,
    seen: Mutex<HashSet<EventId>>,
}

impl EventOutput {
    fn new(json: bool, tui: Option<TuiSender>) -> Self {
        Self {
            json,
            tui,
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
        if let Some(tui) = &self.tui {
            if let Some(activity) = format_activity(event) {
                tui.activity(activity);
            }
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

fn format_activity(event: &HarnessEvent) -> Option<String> {
    match &event.payload {
        EventPayload::UserMessage { text } => Some(format!("Task · {}", concise(text, 120))),
        EventPayload::ModelRequested {
            provider, model, ..
        } => Some(format!("Thinking · {provider}/{model}")),
        EventPayload::ToolRequested { tool, arguments } => {
            let target = arguments
                .get("path")
                .or_else(|| arguments.get("pattern"))
                .or_else(|| arguments.get("query"))
                .or_else(|| arguments.get("command"));
            let description = match tool.as_str() {
                "read_file" => "Reading file",
                "list_directory" => "Reading directory",
                "glob" => "Searching files",
                "grep" => "Searching symbol",
                "write_file" | "apply_patch" => "Editing file",
                "shell" => "Running command",
                _ => "Using tool",
            };
            Some(match target {
                Some(target) => format!("{description} · {}", concise(target, 100)),
                None => format!("{description} · {tool}"),
            })
        }
        EventPayload::ToolApproved { tool, .. } => Some(format!("Approval granted · {tool}")),
        EventPayload::ToolDenied { tool, reason } => Some(format!(
            "Approval denied · {tool} · {}",
            concise(reason, 80)
        )),
        EventPayload::ToolFailed { tool, error } => {
            Some(format!("Tool failed · {tool} · {}", concise(error, 100)))
        }
        EventPayload::ProcessStarted { command, .. } => {
            Some(format!("Running command · {}", concise(command, 120)))
        }
        EventPayload::ProcessExited {
            exit_code,
            timed_out,
            cancelled,
        } => {
            if *cancelled {
                Some("Command cancelled".to_owned())
            } else if *timed_out {
                Some("Command timed out".to_owned())
            } else {
                Some(format!(
                    "Command exited · {}",
                    exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
                ))
            }
        }
        EventPayload::FileChanged { path, change } => Some(format!(
            "{} · {}",
            match change {
                harness_session::FileChange::Added => "Added file",
                harness_session::FileChange::Modified => "Edited file",
                harness_session::FileChange::Deleted => "Deleted file",
            },
            path.display()
        )),
        EventPayload::VerificationStarted { commands } => {
            Some(format!("Running checks · {} command(s)", commands.len()))
        }
        EventPayload::VerificationResult {
            command,
            category,
            passed,
            ..
        } => {
            let kind = if category.to_ascii_lowercase().contains("test") {
                "Test"
            } else {
                "Check"
            };
            Some(format!(
                "{kind} {} · {}",
                if *passed { "passed" } else { "failed" },
                concise(command, 110)
            ))
        }
        EventPayload::AssistantMessage { text } => Some(format!("Agent · {}", concise(text, 160))),
        EventPayload::ContextCompacted { removed_items, .. } => {
            Some(format!("Compacted context · removed {removed_items} items"))
        }
        EventPayload::SessionResumed { .. } => Some("Resumed session".to_owned()),
        EventPayload::SessionCompleted { reason } => Some(format!(
            "Task completed{}",
            reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(" · {}", concise(reason, 100)))
        )),
        EventPayload::SessionFailed { error } => {
            Some(format!("Task failed · {}", concise(error, 140)))
        }
        _ => None,
    }
}

fn concise(value: &str, limit: usize) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let characters = normalized.chars().collect::<Vec<_>>();
    if characters.len() <= limit {
        normalized
    } else {
        format!(
            "{}...",
            characters[..limit.saturating_sub(3)]
                .iter()
                .collect::<String>()
        )
    }
}

struct CliApproval {
    auto_approve: bool,
    tui: Option<TuiSender>,
}

impl ApprovalHandler for CliApproval {
    fn request(
        &self,
        request: &harness_tools::ToolRequest,
    ) -> Result<bool, harness_agent::AgentError> {
        if self.auto_approve {
            return Ok(true);
        }
        if let Some(tui) = &self.tui {
            return Ok(tui.request_approval(&request.name));
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
