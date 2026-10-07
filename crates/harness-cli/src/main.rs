use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use clap::{Parser, Subcommand};
use harness_agent::AgentTask;
use harness_context::WorkspaceMetadata;
use harness_core::{
    discover_workspace, init_logging, CheckpointId, HarnessConfig, SessionId, WorkingTreeState,
};
use harness_git::{CheckpointStore, GitClient, GitDiff, ShadowCheckpointStore};
use harness_models::{
    has_model_environment_override, load_project_model_preference, load_user_model_preference,
    provider_from_config_with_store, save_project_model_preference, save_user_model_preference,
    validate_provider_credential, ContentBlock, CredentialSecret, CredentialSource,
    CredentialStatus, CredentialStore, FinishReason, InputAttachment, Message, ModelConfig,
    ModelPreference, ModelPreferenceError, ModelRegistry, ModelRegistryFilter, ModelRequest,
    ModelResponse, ModelStreamEvent, ProviderError, ProviderKind, ReasoningEffort,
    SystemCredentialStore, ToolCall, Usage,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_rpc::{
    HarnessConnectionManager, RpcResponse, RuntimeConnectError, RuntimeLaunchConfig, ServerMessage,
};
use harness_session::{
    EventId, EventPayload, HarnessEvent, JsonlSessionStore, Session, SessionStore, TaskMode,
};
use harness_tools::CancellationToken;
use harness_verification::VerificationPlan;
use serde_json::{json, Value};
use tui::{StartupInfo, Tui, TuiSender};

mod tui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InteractiveCommand {
    Help,
    Run,
    Resume,
    Inspect,
    Sessions,
    Status,
    Goal,
    PlanStatus,
    Diff,
    Undo,
    Config,
    Models,
    ModelSelect,
    Connect,
    Mcp,
    Mode,
    Explore,
    Plan,
    Code,
    Clear,
    Cancel,
    Exit,
}

struct InteractiveCommandDefinition {
    name: &'static str,
    usage: &'static str,
    description: &'static str,
    command: InteractiveCommand,
    plain_supported: bool,
}

#[derive(Default)]
struct AgentRunUi {
    task_mode: TaskMode,
    tui: Option<TuiSender>,
    cancellation: Option<CancellationToken>,
    attachments: Vec<InputAttachment>,
}

/// The single source used for interactive dispatch, help, and Tab completion.
const INTERACTIVE_COMMANDS: &[InteractiveCommandDefinition] = &[
    InteractiveCommandDefinition {
        name: "/help",
        usage: "/help",
        description: "Show commands and keyboard controls",
        command: InteractiveCommand::Help,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/run",
        usage: "/run <task>",
        description: "Run a task",
        command: InteractiveCommand::Run,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/resume",
        usage: "/resume <session-id> [task]",
        description: "Continue a saved session",
        command: InteractiveCommand::Resume,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/inspect",
        usage: "/inspect [path]",
        description: "Inspect workspace metadata",
        command: InteractiveCommand::Inspect,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/sessions",
        usage: "/sessions [limit]",
        description: "List recent sessions",
        command: InteractiveCommand::Sessions,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/status",
        usage: "/status [session-id]",
        description: "Show session status",
        command: InteractiveCommand::Status,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/goal",
        usage: "/goal [session-id]",
        description: "Show the durable objective and completion condition",
        command: InteractiveCommand::Goal,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/plan-status",
        usage: "/plan-status [session-id]",
        description: "Show persisted milestone and task progress",
        command: InteractiveCommand::PlanStatus,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/diff",
        usage: "/diff [file]",
        description: "Show workspace changes",
        command: InteractiveCommand::Diff,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/undo",
        usage: "/undo [checkpoint-id]",
        description: "Restore a checkpoint through the runtime",
        command: InteractiveCommand::Undo,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/config",
        usage: "/config",
        description: "Show effective configuration",
        command: InteractiveCommand::Config,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/model",
        usage: "/model [provider/model] [--effort level]",
        description: "Show or select a registry model",
        command: InteractiveCommand::ModelSelect,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/models",
        usage: "/models [refresh [provider-id]]",
        description: "List dynamic model catalog or refresh provider catalogs",
        command: InteractiveCommand::Models,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/connect",
        usage: "/connect [provider-id]",
        description: "Validate and securely store a provider credential",
        command: InteractiveCommand::Connect,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/mcp",
        usage: "/mcp [refresh [server] | disconnect <server> | resources <server>]",
        description: "Inspect configured MCP servers and manage their local connections",
        command: InteractiveCommand::Mcp,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/mode",
        usage: "/mode",
        description: "Show the workspace execution mode",
        command: InteractiveCommand::Mode,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/explore",
        usage: "/explore",
        description: "Use read/search-only task behavior",
        command: InteractiveCommand::Explore,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/plan",
        usage: "/plan",
        description: "Inspect and produce a structured plan without edits",
        command: InteractiveCommand::Plan,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/code",
        usage: "/code",
        description: "Use the normal coding-task workflow",
        command: InteractiveCommand::Code,
        plain_supported: true,
    },
    InteractiveCommandDefinition {
        name: "/clear",
        usage: "/clear",
        description: "Clear the visible activity feed",
        command: InteractiveCommand::Clear,
        plain_supported: false,
    },
    InteractiveCommandDefinition {
        name: "/cancel",
        usage: "/cancel",
        description: "Cancel a running task",
        command: InteractiveCommand::Cancel,
        plain_supported: false,
    },
    InteractiveCommandDefinition {
        name: "/exit",
        usage: "/exit",
        description: "Exit the interactive CLI",
        command: InteractiveCommand::Exit,
        plain_supported: true,
    },
];

fn parse_interactive_command(line: &str) -> Option<(InteractiveCommand, &str)> {
    let line = line.trim();
    if !line.starts_with('/') {
        return None;
    }
    let (name, arguments) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let command = if name == "/quit" {
        Some(InteractiveCommand::Exit)
    } else {
        INTERACTIVE_COMMANDS
            .iter()
            .find(|definition| definition.name == name)
            .map(|definition| definition.command)
    }?;
    Some((command, arguments.trim()))
}

fn interactive_help(plain: bool) -> String {
    let commands = INTERACTIVE_COMMANDS
        .iter()
        .filter(|definition| !plain || definition.plain_supported)
        .map(|definition| format!("{} — {}", definition.usage, definition.description))
        .collect::<Vec<_>>()
        .join("  ·  ");
    if plain {
        format!("Commands: {commands}")
    } else {
        format!("Slash commands: {commands}  ·  Enter runs · Alt+Enter adds a line · Tab completes · Ctrl+C cancels")
    }
}

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
    #[arg(long)]
    model_effort: Option<String>,
    #[arg(long, default_value = ".cogito/sessions")]
    session_root: PathBuf,
    #[arg(long)]
    compaction_threshold: Option<u32>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    yes: bool,
    #[arg(long, global = true)]
    rpc_address: Option<String>,
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
        #[arg(long = "attach", value_name = "FILE", action = clap::ArgAction::Append)]
        attachments: Vec<PathBuf>,
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
    Model {
        selection: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        project: bool,
    },
    Models {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        provider: Option<String>,
    },
    Mcp {
        action: Option<String>,
        server_id: Option<String>,
    },
    Agent {
        task: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long = "attach", value_name = "FILE", action = clap::ArgAction::Append)]
        attachments: Vec<PathBuf>,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    Tui,
}

#[derive(Clone, Debug, Subcommand)]
enum RuntimeCommand {
    /// Show whether a healthy runtime is available without starting one.
    Status,
    /// Restart the persistent per-user runtime cleanly.
    Restart,
    /// Stop the persistent per-user runtime cleanly.
    #[command(alias = "shutdown")]
    Stop,
}

#[derive(Clone, Debug, Subcommand)]
enum AuthCommand {
    List,
    Connect { provider: Option<String> },
    Disconnect { provider: String },
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
            let error = harness_core::redact_sensitive(&error.to_string());
            if cli.json {
                eprintln!("{}", json!({"type": "error", "error": error}));
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
        Some(Command::Workspace { path }) => {
            if !cli.json && io::stdin().is_terminal() && io::stdout().is_terminal() {
                let mut interactive_cli = cli.clone();
                interactive_cli.workspace = effective_path(cli, path);
                interactive(&interactive_cli)
            } else {
                inspect(cli, &effective_path(cli, path))
            }
        }
        Some(Command::Run {
            task,
            path,
            attachments,
        }) => run_agent_with_files(
            cli,
            task.clone(),
            effective_path(cli, path),
            None,
            attachments,
        ),
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
        Some(Command::Model {
            selection,
            effort,
            project,
        }) => model_command(cli, selection.as_deref(), effort.as_deref(), *project),
        Some(Command::Models { refresh, provider }) => {
            let arguments = if *refresh {
                format!("refresh {}", provider.as_deref().unwrap_or("all"))
            } else if let Some(provider) = provider {
                format!("refresh {provider}")
            } else {
                String::new()
            };
            models_command(cli, &arguments)
        }
        Some(Command::Mcp { action, server_id }) => {
            let arguments = match (action.as_deref(), server_id.as_deref()) {
                (Some(action), Some(server)) => format!("{action} {server}"),
                (Some(action), None) => action.to_owned(),
                _ => String::new(),
            };
            mcp_command(cli, &arguments)
        }
        Some(Command::Agent {
            task,
            path,
            attachments,
        }) => run_agent_with_files(
            cli,
            task.clone(),
            effective_path(cli, path),
            None,
            attachments,
        ),
        Some(Command::Session { command }) => session_command(cli, command),
        Some(Command::Auth { command }) => auth_command(cli, command),
        Some(Command::Runtime { command }) => runtime_command(cli, command),
        Some(Command::Tui) => interactive(cli),
        None => {
            if !cli.json && io::stdin().is_terminal() && io::stdout().is_terminal() {
                return interactive(cli);
            } else if cli.json {
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
    let connector = HarnessConnectionManager::default();
    if std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb")) {
        return plain_interactive(cli, connector);
    }

    let workspace_path = std::fs::canonicalize(effective_path(cli, Path::new(".")))?;
    let workspace = discover_workspace(&workspace_path)?;
    let model = model_config(cli)?;
    let project_root = workspace
        .repository_root
        .clone()
        .unwrap_or_else(|| workspace.current_directory.clone());
    let policy = policy_for_workspace(&project_root)?;
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
        model: model.model.clone(),
        provider: provider_id(model.provider).to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        workspace: display_workspace_path(&workspace_path),
        notice: (!notices.is_empty()).then(|| notices.join("  |  ")),
        execution_mode: execution_mode_name(policy.mode()).to_owned(),
        branch: workspace.git.branch.clone(),
        workspace_dirty: workspace
            .git
            .working_tree
            .map(|state| matches!(state, WorkingTreeState::Dirty)),
    };
    let mut tui = Tui::new(startup, model.clone())?;
    tui.show_runtime_status("Connecting to harness...")?;
    let launch = runtime_launch_config(cli, &workspace_path, &model)?;
    let startup_connection = match connector.connect_existing(&launch) {
        Ok(client) => Ok(client),
        Err(_) => {
            tui.show_runtime_status("Starting runtime...")?;
            connector.connect_or_start(&launch)
        }
    };
    match startup_connection {
        Ok(mut client) => {
            report_connected_runtime(cli, &connector, &mut client, Some(&tui.sender()));
            tui.set_runtime_status(Some("Connected"));
        }
        Err(error) => {
            tui.set_runtime_status(Some("Runtime unavailable"));
            if is_verbose(cli) {
                tui.add_activity(format!("Runtime startup failed · {error}"));
            }
        }
    }
    let cli = cli.clone();
    let run_connector = connector.clone();
    tui.run(move |line, tui| dispatch_interactive(&cli, line, tui, &run_connector))?;
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

fn policy_for_workspace(
    project_root: &Path,
) -> Result<Arc<dyn Policy>, harness_policy::PolicyError> {
    let config = project_root.join(".agent/config.toml");
    if config.is_file() {
        Ok(Arc::new(PolicyEngine::from_file(&config, project_root)?))
    } else {
        Ok(Arc::new(PolicyEngine::new(
            ExecutionMode::Normal,
            project_root,
        )))
    }
}

fn execution_mode_name(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::ReadOnly => "read-only",
        ExecutionMode::Safe => "safe",
        ExecutionMode::Normal => "normal",
        ExecutionMode::Auto => "auto",
    }
}

/// A line-oriented fallback for terminals that report `TERM=dumb`.
fn plain_interactive(
    cli: &Cli,
    connector: HarnessConnectionManager,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut session_cli = cli.clone();
    let mut task_mode = TaskMode::Code;
    let workspace = std::fs::canonicalize(effective_path(&session_cli, Path::new(".")))?;
    let model = model_config(&session_cli)?;
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
    println!("Enter a task or type /help. Ctrl+D exits.");
    let launch = runtime_launch_config(&session_cli, &workspace, &model)?;
    eprint!("Connecting to harness...");
    let startup_connection = match connector.connect_existing(&launch) {
        Ok(client) => Ok(client),
        Err(_) => connector.connect_or_start(&launch),
    };
    match startup_connection {
        Ok(mut client) => {
            report_connected_runtime(cli, &connector, &mut client, None);
            eprintln!(" connected.");
        }
        Err(error) => {
            eprintln!(" unavailable.");
            if is_verbose(cli) {
                eprintln!("runtime startup failed: {error}");
            }
        }
    }
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
        if line.starts_with('/') {
            let Some((command, arguments)) = parse_interactive_command(line) else {
                eprintln!(
                    "unknown command {}; type /help for available commands",
                    line.split_whitespace().next().unwrap_or(line)
                );
                continue;
            };
            match command {
                InteractiveCommand::Help => println!("{}", interactive_help(true)),
                InteractiveCommand::Run => {
                    if arguments.is_empty() {
                        eprintln!("usage: /run <task>");
                    } else {
                        run_agent_with_task_mode(
                            &session_cli,
                            arguments.to_owned(),
                            effective_path(&session_cli, Path::new(".")),
                            None,
                            connector.clone(),
                            task_mode,
                        )?;
                        break;
                    }
                }
                InteractiveCommand::Resume => {
                    let mut fields = arguments.splitn(2, char::is_whitespace);
                    let id = fields.next().unwrap_or_default();
                    if id.is_empty() {
                        eprintln!("usage: /resume <session-id> [task]");
                    } else {
                        let task = fields
                            .next()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned);
                        resume_session_with_task_mode(
                            &session_cli,
                            id,
                            task,
                            None,
                            connector.clone(),
                            task_mode,
                        )?;
                        break;
                    }
                }
                InteractiveCommand::Inspect => {
                    let path = if arguments.is_empty() {
                        effective_path(&session_cli, Path::new("."))
                    } else {
                        PathBuf::from(arguments.trim_matches('"'))
                    };
                    inspect(&session_cli, &path)?;
                }
                InteractiveCommand::Sessions => {
                    let limit = if arguments.is_empty() {
                        20
                    } else {
                        arguments.parse::<usize>()?
                    };
                    sessions(&session_cli, limit)?;
                }
                InteractiveCommand::Status => status(
                    &session_cli,
                    (!arguments.is_empty()).then_some(arguments),
                    None,
                )?,
                InteractiveCommand::Goal => {
                    if arguments.is_empty() {
                        eprintln!("usage: /goal <session-id>");
                    } else {
                        task_state_command(&session_cli, Some(arguments), false)?;
                    }
                }
                InteractiveCommand::PlanStatus => {
                    if arguments.is_empty() {
                        eprintln!("usage: /plan-status <session-id>");
                    } else {
                        task_state_command(&session_cli, Some(arguments), true)?;
                    }
                }
                InteractiveCommand::Diff => {
                    let file =
                        (!arguments.is_empty()).then(|| PathBuf::from(arguments.trim_matches('"')));
                    diff(&session_cli, file.as_deref(), None)?;
                }
                InteractiveCommand::Undo => undo(
                    &session_cli,
                    (!arguments.is_empty()).then_some(arguments),
                    None,
                )?,
                InteractiveCommand::Config => config_command(&session_cli, None)?,
                InteractiveCommand::ModelSelect => {
                    if arguments.is_empty() {
                        model_info(&session_cli)?;
                    } else {
                        let selected = select_interactive_model(&session_cli, arguments, None)?;
                        configure_cli_model(&mut session_cli, &selected);
                        println!("selected {}", canonical_model_id(&selected));
                    }
                }
                InteractiveCommand::Models => models_command(&session_cli, arguments)?,
                InteractiveCommand::Connect => connect_provider_cli(
                    &session_cli,
                    (!arguments.is_empty()).then_some(arguments),
                )?,
                InteractiveCommand::Mcp => mcp_command(&session_cli, arguments)?,
                InteractiveCommand::Mode => {
                    if !arguments.is_empty() {
                        eprintln!("usage: /mode (shows the workspace-configured mode)");
                    } else {
                        mode_info(&session_cli)?;
                    }
                }
                InteractiveCommand::Explore
                | InteractiveCommand::Plan
                | InteractiveCommand::Code => {
                    if !arguments.is_empty() {
                        eprintln!("usage: /{}", task_mode_command_for(command));
                    } else {
                        task_mode = match command {
                            InteractiveCommand::Explore => TaskMode::Explore,
                            InteractiveCommand::Plan => TaskMode::Plan,
                            InteractiveCommand::Code => TaskMode::Code,
                            _ => unreachable!("combined task-mode command arm"),
                        };
                        let (label, description) = match task_mode {
                            TaskMode::Explore => ("EXPLORE", "workspace reads and searches only"),
                            TaskMode::Plan => ("PLAN", "read-only structured implementation plan"),
                            TaskMode::Code => ("CODE", "normal coding workflow"),
                        };
                        println!("Task mode: {label} · {description}");
                    }
                }
                InteractiveCommand::Clear | InteractiveCommand::Cancel => {
                    let name = if command == InteractiveCommand::Clear {
                        "/clear"
                    } else {
                        "/cancel"
                    };
                    eprintln!("{name} is available in the full-screen TUI only");
                }
                InteractiveCommand::Exit => break,
            }
            continue;
        }
        run_agent_with_task_mode(
            &session_cli,
            line.to_owned(),
            effective_path(&session_cli, Path::new(".")),
            None,
            connector.clone(),
            task_mode,
        )?;
        // The line-oriented fallback has no persistent event loop for signal
        // registration. Exit after a run so its one-shot Ctrl+C handler is not
        // installed a second time in the same process.
        break;
    }
    Ok(())
}

fn dispatch_interactive(
    cli: &Cli,
    line: String,
    tui: &mut Tui,
    connector: &HarnessConnectionManager,
) -> Result<(), String> {
    if !line.starts_with('/') {
        let resume_session = plan_continuation_session(tui)?;
        return start_interactive_run(
            cli,
            line,
            effective_path(cli, Path::new(".")),
            resume_session,
            tui,
            connector,
        );
    }
    let Some((command, arguments)) = parse_interactive_command(&line) else {
        let command = line.split_whitespace().next().unwrap_or(&line);
        return Err(format!(
            "unknown command {command}; type /help for available commands"
        ));
    };
    match command {
        InteractiveCommand::Help => {
            tui.add_activity(interactive_help(false));
            Ok(())
        }
        InteractiveCommand::Run => {
            if arguments.is_empty() {
                Err("usage: /run <task>".to_owned())
            } else {
                let resume_session = plan_continuation_session(tui)?;
                start_interactive_run(
                    cli,
                    arguments.to_owned(),
                    effective_path(cli, Path::new(".")),
                    resume_session,
                    tui,
                    connector,
                )
            }
        }
        InteractiveCommand::Resume => {
            let mut fields = arguments.splitn(2, char::is_whitespace);
            let id = fields.next().unwrap_or_default();
            if id.is_empty() {
                return Err("usage: /resume <session-id> [task]".to_owned());
            }
            let session_id = SessionId::new(id.to_owned()).map_err(|error| error.to_string())?;
            let store =
                JsonlSessionStore::new(&cli.session_root).map_err(|error| error.to_string())?;
            let existing = store.load(&session_id).map_err(|error| error.to_string())?;
            if cli.model_provider.is_none() && cli.model.is_none() && cli.model_effort.is_none() {
                let mut selected = tui.model_config();
                apply_session_model_preference(cli, &existing, &mut selected)
                    .map_err(|error| error.to_string())?;
                tui.set_model_config(selected);
            }
            let task = fields
                .next()
                .map(str::trim)
                .filter(|task| !task.is_empty())
                .unwrap_or(
                    "Continue from the compacted session state and finish the remaining work.",
                )
                .to_owned();
            tui.set_active_session_id(Some(session_id.to_string()));
            start_interactive_run(
                cli,
                task,
                existing.workspace_root,
                Some(session_id),
                tui,
                connector,
            )
        }
        InteractiveCommand::Clear => {
            tui.clear_activity();
            Ok(())
        }
        InteractiveCommand::Cancel => {
            tui.cancel_run();
            Ok(())
        }
        InteractiveCommand::Exit => {
            tui.request_exit();
            Ok(())
        }
        InteractiveCommand::Inspect => {
            let path = if arguments.is_empty() {
                effective_path(cli, Path::new("."))
            } else {
                PathBuf::from(arguments.trim_matches('"'))
            };
            run_visible_command(tui, || inspect(cli, &path))
        }
        InteractiveCommand::Sessions => {
            let limit = if arguments.is_empty() {
                20
            } else {
                arguments
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?
            };
            run_visible_command(tui, || sessions(cli, limit))
        }
        InteractiveCommand::Status => run_visible_command(tui, || {
            status(cli, (!arguments.is_empty()).then_some(arguments), None)
        }),
        InteractiveCommand::Goal | InteractiveCommand::PlanStatus => {
            let session_id = session_argument_or_active(arguments, tui.active_session_id())?;
            run_visible_command(tui, || {
                task_state_command(
                    cli,
                    Some(&session_id),
                    command == InteractiveCommand::PlanStatus,
                )
            })
        }
        InteractiveCommand::Diff => {
            let file = (!arguments.is_empty()).then(|| PathBuf::from(arguments.trim_matches('"')));
            run_visible_command(tui, || diff(cli, file.as_deref(), None))
        }
        InteractiveCommand::Undo => run_visible_command(tui, || {
            undo(cli, (!arguments.is_empty()).then_some(arguments), None)
        }),
        InteractiveCommand::Config => run_visible_command(tui, || config_command(cli, None)),
        InteractiveCommand::ModelSelect => {
            if arguments.is_empty() {
                run_visible_command(tui, || model_info(cli))
            } else {
                let selected = select_interactive_model(cli, arguments, Some(tui))
                    .map_err(|error| error.to_string())?;
                tui.set_model_config(selected.clone());
                tui.add_activity(format!(
                    "Model selected · {}",
                    canonical_model_id(&selected)
                ));
                Ok(())
            }
        }
        InteractiveCommand::Models => run_visible_command(tui, || models_command(cli, arguments)),
        InteractiveCommand::Connect => run_visible_command(tui, || {
            connect_provider_cli(cli, (!arguments.is_empty()).then_some(arguments))
        }),
        InteractiveCommand::Mcp => run_visible_command(tui, || mcp_command(cli, arguments)),
        InteractiveCommand::Mode => {
            if !arguments.is_empty() {
                return Err("usage: /mode (shows the workspace-configured mode)".to_owned());
            }
            run_visible_command(tui, || mode_info(cli))
        }
        InteractiveCommand::Explore => set_interactive_task_mode(tui, TaskMode::Explore, arguments),
        InteractiveCommand::Plan => set_interactive_task_mode(tui, TaskMode::Plan, arguments),
        InteractiveCommand::Code => set_interactive_task_mode(tui, TaskMode::Code, arguments),
    }
}

fn plan_continuation_session(tui: &Tui) -> Result<Option<SessionId>, String> {
    tui.approved_plan_session_for_code()
        .map(SessionId::new)
        .transpose()
        .map_err(|error| error.to_string())
}

fn set_interactive_task_mode(tui: &mut Tui, mode: TaskMode, arguments: &str) -> Result<(), String> {
    if !arguments.is_empty() {
        return Err(format!("usage: /{}", task_mode_command(mode)));
    }
    tui.set_task_mode(mode);
    let (label, description) = match mode {
        TaskMode::Explore => ("EXPLORE", "workspace reads and searches only"),
        TaskMode::Plan => ("PLAN", "read-only structured implementation plan"),
        TaskMode::Code => ("CODE", "normal coding workflow"),
    };
    tui.add_activity(format!("Task mode · {label} · {description}"));
    Ok(())
}

fn task_mode_command(mode: TaskMode) -> &'static str {
    match mode {
        TaskMode::Explore => "explore",
        TaskMode::Plan => "plan",
        TaskMode::Code => "code",
    }
}

fn task_mode_command_for(command: InteractiveCommand) -> &'static str {
    match command {
        InteractiveCommand::Explore => "explore",
        InteractiveCommand::Plan => "plan",
        InteractiveCommand::Code => "code",
        _ => unreachable!("not a task-mode command"),
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
    connector: &HarnessConnectionManager,
) -> Result<(), String> {
    tui.set_active_session_id(resume_session.as_ref().map(ToString::to_string));
    let selected_model = tui.model_config();
    let mut cli = cli.clone();
    configure_cli_model(&mut cli, &selected_model);
    let cancellation = CancellationToken::new();
    if !tui.start_run(&task, cancellation.clone()) {
        return Err("a task is already running".to_owned());
    }
    let sender = tui.sender();
    let run_connector = connector.clone();
    let task_mode = tui.task_mode();
    let failed_sender = sender.clone();
    let spawn = std::thread::Builder::new()
        .name("harness-agent-tui".to_owned())
        .spawn(move || {
            let result = run_agent_with_ui(
                &cli,
                task,
                workspace,
                resume_session,
                AgentRunUi {
                    task_mode,
                    tui: Some(sender.clone()),
                    cancellation: Some(cancellation),
                    attachments: Vec::new(),
                },
                run_connector,
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
    if !has_model_environment_override() {
        let preference_error = |error: ModelPreferenceError| ProviderError::Configuration {
            reason: error.to_string(),
        };
        if let Some(preference) = load_user_model_preference().map_err(preference_error)? {
            preference.apply_to(&mut config).map_err(preference_error)?;
        }
        let workspace = effective_path(cli, Path::new("."));
        if let Some(preference) =
            load_project_model_preference(&workspace).map_err(preference_error)?
        {
            preference.apply_to(&mut config).map_err(preference_error)?;
        }
    }
    if let Some(provider) = &cli.model_provider {
        let provider =
            serde_json::from_value(Value::String(provider.clone())).map_err(|error| {
                ProviderError::InvalidResponse {
                    provider: "configuration",
                    reason: error.to_string(),
                }
            })?;
        config.select_provider(provider);
    }
    if let Some(model) = &cli.model {
        if let Ok(preference) = ModelPreference::from_canonical_id(model, config.reasoning_effort) {
            preference
                .apply_to(&mut config)
                .map_err(|error| ProviderError::Configuration {
                    reason: error.to_string(),
                })?;
        } else {
            config.model = model.clone();
        }
    }
    if let Some(effort) = &cli.model_effort {
        config.reasoning_effort = parse_reasoning_effort(effort)
            .map_err(|reason| ProviderError::Configuration { reason })?;
    }
    Ok(config)
}

fn parse_reasoning_effort(value: &str) -> Result<Option<ReasoningEffort>, String> {
    if value.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    serde_json::from_value(Value::String(value.to_ascii_lowercase()))
        .map(Some)
        .map_err(|_| format!("unsupported reasoning effort '{value}'"))
}

fn canonical_model_id(config: &ModelConfig) -> String {
    let provider = match config.provider {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::OpenCodeGo => "opencode-go",
    };
    let model = config
        .model
        .strip_prefix(&format!("{provider}/"))
        .unwrap_or(&config.model);
    format!("{provider}/{model}")
}

fn configure_cli_model(cli: &mut Cli, model: &ModelConfig) {
    cli.model_provider = Some(provider_id(model.provider).to_owned());
    cli.model = Some(model.model.clone());
    cli.model_effort = model.reasoning_effort.and_then(|effort| {
        serde_json::to_value(effort)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
    });
}

fn model_command(
    cli: &Cli,
    selection: Option<&str>,
    effort: Option<&str>,
    project: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut config = model_config(cli)?;
    if selection.is_none() && effort.is_none() {
        return model_info(cli);
    }
    if let Some(selection) = selection {
        let preference = ModelPreference::from_canonical_id(selection, config.reasoning_effort)?;
        preference.apply_to(&mut config)?;
    }
    if let Some(effort) = effort {
        let effort_value = parse_reasoning_effort(effort)?;
        if let Some(effort) = effort_value {
            ensure_model_reasoning_level(cli, &config, effort)?;
        } else {
            ensure_model_reasoning_supported(cli, &config)?;
        }
        config.reasoning_effort = effort_value;
    }
    let preference =
        ModelPreference::from_canonical_id(&canonical_model_id(&config), config.reasoning_effort)?;
    if project {
        save_project_model_preference(&effective_path(cli, Path::new(".")), &preference)?;
    } else {
        save_user_model_preference(&preference)?;
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&preference)?);
    } else {
        println!("selected {}", preference.canonical_id());
    }
    Ok(())
}

fn ensure_model_reasoning_supported(
    cli: &Cli,
    config: &ModelConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = model_registry(cli, config);
    let selected = registry
        .models(&ModelRegistryFilter {
            provider_id: Some(provider_id(config.provider).to_owned()),
            requirements: Vec::new(),
        })
        .into_iter()
        .find(|descriptor| descriptor.canonical_id() == canonical_model_id(config));
    if !selected
        .and_then(|descriptor| descriptor.metadata.reasoning_levels)
        .is_some_and(|levels| !levels.is_empty())
    {
        return Err("this model does not advertise configurable reasoning levels".into());
    }
    Ok(())
}

fn ensure_model_reasoning_level(
    cli: &Cli,
    config: &ModelConfig,
    effort: ReasoningEffort,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = model_registry(cli, config);
    let selected = registry
        .models(&ModelRegistryFilter {
            provider_id: Some(provider_id(config.provider).to_owned()),
            requirements: Vec::new(),
        })
        .into_iter()
        .find(|descriptor| descriptor.canonical_id() == canonical_model_id(config));
    if !selected
        .and_then(|descriptor| descriptor.metadata.reasoning_levels)
        .is_some_and(|levels| levels.contains(&effort))
    {
        return Err(
            "reasoning effort is not advertised for this model; use /models refresh first".into(),
        );
    }
    Ok(())
}

fn model_registry(cli: &Cli, config: &ModelConfig) -> ModelRegistry {
    let workspace = effective_path(cli, Path::new("."));
    let credential_store = SystemCredentialStore::new();
    ModelRegistry::with_builtins_and_credentials(
        config,
        Some(workspace.join(".cogito/model-catalog.json")),
        &credential_store,
    )
}

fn select_interactive_model(
    cli: &Cli,
    arguments: &str,
    tui: Option<&mut Tui>,
) -> Result<ModelConfig, Box<dyn std::error::Error>> {
    let mut fields = arguments.split_whitespace();
    let selection = fields.next().unwrap_or_default();
    let mut effort = None;
    while let Some(field) = fields.next() {
        match field {
            "--effort" => {
                effort = Some(
                    fields
                        .next()
                        .ok_or("usage: /model provider/model [--effort level]")?,
                );
            }
            other => return Err(format!("unexpected model option '{other}'").into()),
        }
    }
    let mut config = match tui.as_ref() {
        Some(tui) => tui.model_config(),
        None => model_config(cli)?,
    };
    let preference = ModelPreference::from_canonical_id(selection, config.reasoning_effort)?;
    preference.apply_to(&mut config)?;
    if let Some(effort) = effort {
        let parsed = parse_reasoning_effort(effort)?;
        if let Some(value) = parsed {
            ensure_model_reasoning_level(cli, &config, value)?;
        } else {
            ensure_model_reasoning_supported(cli, &config)?;
        }
        config.reasoning_effort = parsed;
    }
    let preference =
        ModelPreference::from_canonical_id(&canonical_model_id(&config), config.reasoning_effort)?;
    save_user_model_preference(&preference)?;
    if let Some(session_id) = tui.as_ref().and_then(|tui| tui.active_session_id()) {
        let store = JsonlSessionStore::new(&cli.session_root)?;
        let id = SessionId::new(session_id)?;
        store.append_event(
            &id,
            HarnessEvent::new(
                id.clone(),
                EventPayload::ModelChanged {
                    provider: provider_id(config.provider).to_owned(),
                    model: config.model.clone(),
                    reasoning_effort: config.reasoning_effort.and_then(|effort| {
                        serde_json::to_value(effort)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned))
                    }),
                },
                None,
                None,
            ),
        )?;
    }
    Ok(config)
}

fn ask(cli: &Cli, prompt: &str, stream: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = model_config(cli)?;
    let provider = provider_from_config_with_store(&config, &SystemCredentialStore::new())?;
    let request = ModelRequest::new(config.model.clone(), vec![Message::user_text(prompt)]);
    let response = provider.generate(&request, &mut |event: ModelStreamEvent| {
        if stream && !cli.json {
            if let ModelStreamEvent::TextDelta { text } = event {
                print!("{text}");
                let _ = io::stdout().flush();
            }
        }
        Ok(())
    })?;
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
    let provider = provider_from_config_with_store(&config, &SystemCredentialStore::new())?;
    let descriptor = provider.descriptor();
    let info = json!({
        "provider": descriptor.provider,
        "model": descriptor.id,
        "capabilities": descriptor.capabilities,
    });
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("provider: {}", descriptor.provider);
        println!("model: {}", descriptor.id);
        println!(
            "capabilities: {}",
            serde_json::to_string_pretty(&descriptor.capabilities)?
        );
    }
    Ok(())
}

fn models_command(cli: &Cli, arguments: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut fields = arguments.split_whitespace();
    let action = fields.next();
    let provider_id = match action {
        None => None,
        Some("refresh") => fields.next().filter(|provider| *provider != "all"),
        Some(other) => {
            return Err(
                format!("usage: /models [refresh [provider-id]] (unexpected `{other}`)").into(),
            );
        }
    };
    if action == Some("refresh") && fields.next().is_some() {
        return Err("usage: /models [refresh [provider-id]]".into());
    }
    let config = model_config(cli)?;
    let workspace = effective_path(cli, Path::new("."));
    let credential_store = SystemCredentialStore::new();
    let registry = ModelRegistry::with_builtins_and_credentials(
        &config,
        Some(workspace.join(".cogito/model-catalog.json")),
        &credential_store,
    );
    let refresh = if action == Some("refresh") {
        Some(match provider_id {
            Some(provider_id) => registry.refresh_provider_report(provider_id),
            None => registry.refresh_all(),
        })
    } else {
        None
    };
    let models = registry.models(&ModelRegistryFilter::default());
    let defaults = registry.defaults();
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({ "models": models, "defaults": defaults, "refresh": refresh }),
            )?
        );
        return Ok(());
    }
    if let Some(report) = &refresh {
        for result in &report.providers {
            if let Some(error) = &result.error {
                println!("{}: refresh failed: {error}", result.provider_id);
            } else {
                println!(
                    "{}: refreshed {} models",
                    result.provider_id, result.model_count
                );
            }
        }
    }
    if models.is_empty() {
        println!("No cached models. Use /models refresh to query configured providers.");
    } else {
        for model in models {
            let is_default = defaults
                .get(&model.provider)
                .is_some_and(|id| id == &model.id);
            let state = if model.metadata.stale {
                " · stale cache"
            } else {
                ""
            };
            let source = match model.metadata.source {
                harness_models::ModelMetadataSource::Discovered => "discovered",
                harness_models::ModelMetadataSource::Cached => "cached",
                harness_models::ModelMetadataSource::ManuallyConfigured => "manual",
                harness_models::ModelMetadataSource::Unknown => "unknown",
            };
            let capabilities = [
                ("vision", model.metadata.capabilities.vision),
                ("tools", model.metadata.capabilities.tool_calling),
                ("reasoning", model.metadata.capabilities.reasoning),
            ]
            .into_iter()
            .filter_map(|(name, support)| {
                (support == harness_models::CapabilityKnowledge::Supported).then_some(name)
            })
            .collect::<Vec<_>>()
            .join(",");
            println!(
                "{}{}  [{}{}]{}",
                if is_default { "* " } else { "  " },
                model.canonical_id(),
                source,
                if capabilities.is_empty() {
                    String::new()
                } else {
                    format!(" · {capabilities}")
                },
                state
            );
        }
    }
    Ok(())
}

fn mcp_command(cli: &Cli, arguments: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut fields = arguments.split_whitespace();
    let action = fields.next().unwrap_or("status");
    let server_id = fields.next();
    if fields.next().is_some()
        || !matches!(action, "status" | "refresh" | "disconnect" | "resources")
        || (matches!(action, "disconnect" | "resources") && server_id.is_none())
    {
        return Err(
            "usage: /mcp [status | refresh [server] | disconnect <server> | resources <server>]"
                .into(),
        );
    }
    let workspace = std::fs::canonicalize(effective_path(cli, Path::new(".")))?;
    let model = model_config(cli)?;
    let launch = runtime_launch_config(cli, &workspace, &model)?;
    let connector = HarnessConnectionManager::default();
    let mut client = connect_runtime_for_cli(&connector, &launch, cli, None)?;
    let method = match action {
        "refresh" => "mcp.refresh",
        "disconnect" => "mcp.disconnect",
        "resources" => "mcp.resources",
        _ => "mcp.inspect",
    };
    let params = match action {
        "refresh" => server_id.map_or_else(
            || json!({"approved": true}),
            |server_id| json!({"approved": true, "server_id": server_id}),
        ),
        "resources" => json!({"approved": true, "server_id": server_id}),
        "disconnect" => json!({"server_id": server_id}),
        _ => json!({}),
    };
    let result = rpc_result(client.request(method, params)?)?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if action == "resources" {
        let resources = result
            .get("resources")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if resources.is_empty() {
            println!("No resources available from {server_id:?}.");
        } else {
            for resource in resources {
                println!(
                    "{}  {}{}",
                    resource
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("resource"),
                    resource.get("uri").and_then(Value::as_str).unwrap_or(""),
                    resource
                        .get("mime_type")
                        .and_then(Value::as_str)
                        .map_or_else(String::new, |kind| format!("  · {kind}")),
                );
            }
        }
        return Ok(());
    }
    let servers = result
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if servers.is_empty() {
        println!("No MCP servers configured. Add servers to .agent/mcp.toml.");
        return Ok(());
    }
    for server in servers {
        let id = server
            .get("server_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let state = server
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("disconnected");
        let transport = server
            .get("transport")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let tools = server
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let resources = server
            .get("resources")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        println!(
            "{id} · {transport} · {state} · {} tools · {} resources",
            tools.len(),
            resources.len()
        );
        if let Some(error) = server.get("error").and_then(Value::as_str) {
            println!("  {error}");
        }
        for tool in tools {
            println!(
                "  {} — {} (≈{} tokens)",
                tool.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unnamed"),
                tool.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                tool.get("estimated_definition_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn mode_info(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mode = configured_mode(cli)?;
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "execution_mode": mode }))?
        );
    } else {
        println!("execution mode: {mode}");
    }
    Ok(())
}

fn configured_mode(cli: &Cli) -> Result<&'static str, Box<dyn std::error::Error>> {
    let workspace = discover_workspace(&effective_path(cli, Path::new(".")))?;
    let project_root = workspace
        .repository_root
        .unwrap_or_else(|| workspace.current_directory.clone());
    Ok(execution_mode_name(
        policy_for_workspace(&project_root)?.mode(),
    ))
}

fn sessions(cli: &Cli, limit: usize) -> Result<(), Box<dyn std::error::Error>> {
    let store = JsonlSessionStore::new(&cli.session_root)?;
    print_sessions(&store.recent(limit)?, cli.json)
}

fn session_argument_or_active(
    argument: &str,
    active_session: Option<String>,
) -> Result<String, String> {
    let argument = argument.trim();
    if !argument.is_empty() {
        if argument.split_whitespace().count() != 1 {
            return Err("usage: /goal [session-id]".to_owned());
        }
        return Ok(argument.to_owned());
    }
    active_session.ok_or_else(|| {
        "there is no active session yet; run a task or provide a session ID".to_owned()
    })
}

fn task_state_command(
    cli: &Cli,
    session_id: Option<&str>,
    show_plan: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = session_id
        .filter(|value| !value.trim().is_empty())
        .ok_or("usage: /goal [session-id] or /plan-status [session-id]")?;
    let session_id = SessionId::new(session_id.to_owned())?;
    let session = JsonlSessionStore::new(&cli.session_root)?.load(&session_id)?;
    let task_run = session
        .state()?
        .task_run
        .ok_or("the selected session has no persisted task goal yet")?;
    if cli.json {
        let value = if show_plan {
            json!({
                "session_id": session_id,
                "execution_plan": task_run.execution_plan,
                "current_milestone": task_run.goal.current_milestone,
                "completion_status": task_run.completion_status,
            })
        } else {
            json!({
                "session_id": session_id,
                "goal": task_run.goal,
                "completion_status": task_run.completion_status,
                "remaining_work": task_run.remaining_work,
            })
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    if show_plan {
        let Some(plan) = task_run.execution_plan else {
            println!("No execution plan has been saved for this session.");
            return Ok(());
        };
        println!(
            "Plan · revision {} · {:?} · session {}",
            plan.revision, plan.status, session_id
        );
        for (index, milestone) in plan.milestones.iter().enumerate() {
            println!(
                "\n{}. {} · {:?}",
                index + 1,
                milestone.title,
                milestone.status
            );
            for task in &milestone.tasks {
                println!(
                    "  [{}] {}",
                    plan_status_label(task.status),
                    task.description
                );
            }
            for command in &milestone.validation_commands {
                println!("  validate: {command}");
            }
        }
        for note in &plan.decision_notes {
            println!("decision: {note}");
        }
    } else {
        println!(
            "Goal · {:?} · session {}",
            task_run.completion_status, session_id
        );
        println!("{}", task_run.goal.objective);
        if let Some(milestone) = &task_run.goal.current_milestone {
            println!("Current milestone: {milestone}");
        }
        println!("Done when: {}", task_run.goal.completion_condition);
        for (label, values) in [
            ("Acceptance", &task_run.goal.acceptance_criteria),
            ("Constraints", &task_run.goal.constraints),
            ("Non-goals", &task_run.goal.non_goals),
        ] {
            if !values.is_empty() {
                println!("{label}:");
                for value in values {
                    println!("  · {value}");
                }
            }
        }
        if !task_run.remaining_work.is_empty() {
            println!("Remaining work:");
            for item in &task_run.remaining_work {
                println!("  · {item}");
            }
        }
    }
    Ok(())
}

fn plan_status_label(status: harness_session::PlanItemStatus) -> &'static str {
    match status {
        harness_session::PlanItemStatus::Pending => "pending",
        harness_session::PlanItemStatus::InProgress => "active",
        harness_session::PlanItemStatus::Completed => "done",
        harness_session::PlanItemStatus::Blocked => "blocked",
    }
}

fn resume_session(
    cli: &Cli,
    id: &str,
    task: Option<String>,
    path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    resume_session_with_connector(cli, id, task, path, HarnessConnectionManager::default())
}

fn resume_session_with_connector(
    cli: &Cli,
    id: &str,
    task: Option<String>,
    path: Option<PathBuf>,
    connector: HarnessConnectionManager,
) -> Result<(), Box<dyn std::error::Error>> {
    resume_session_with_task_mode(cli, id, task, path, connector, TaskMode::Code)
}

fn resume_session_with_task_mode(
    cli: &Cli,
    id: &str,
    task: Option<String>,
    path: Option<PathBuf>,
    connector: HarnessConnectionManager,
    task_mode: TaskMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::new(id.to_owned())?;
    let store = JsonlSessionStore::new(&cli.session_root)?;
    let existing = store.load(&session_id)?;
    let mut effective_cli = cli.clone();
    if effective_cli.model_provider.is_none()
        && effective_cli.model.is_none()
        && effective_cli.model_effort.is_none()
    {
        let mut selected = model_config(cli)?;
        apply_session_model_preference(cli, &existing, &mut selected)?;
        effective_cli.model_provider = Some(provider_id(selected.provider).to_owned());
        effective_cli.model = Some(selected.model);
        effective_cli.model_effort = selected.reasoning_effort.and_then(|effort| {
            serde_json::to_value(effort)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
        });
    }
    let path = path.unwrap_or_else(|| existing.workspace_root.clone());
    let task = task.unwrap_or_else(|| {
        "Continue from the compacted session state and finish the remaining work.".to_owned()
    });
    run_agent_with_task_mode(
        &effective_cli,
        task,
        path,
        Some(session_id),
        connector,
        task_mode,
    )
}

fn apply_session_model_preference(
    cli: &Cli,
    session: &Session,
    model: &mut ModelConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    if has_model_environment_override()
        || cli.model_provider.is_some()
        || cli.model.is_some()
        || cli.model_effort.is_some()
    {
        return Ok(());
    }
    let preference = session.events.iter().rev().find_map(|event| {
        if let EventPayload::ModelChanged {
            provider,
            model,
            reasoning_effort,
        } = &event.payload
        {
            let effort = reasoning_effort
                .as_ref()
                .and_then(|value| serde_json::from_value(Value::String(value.clone())).ok());
            ModelPreference::from_canonical_id(&format!("{provider}/{model}"), effort).ok()
        } else {
            None
        }
    });
    if let Some(preference) = preference {
        preference.apply_to(model)?;
    }
    Ok(())
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

fn load_task_attachments(
    paths: &[PathBuf],
) -> Result<Vec<InputAttachment>, Box<dyn std::error::Error>> {
    const MAX_FILES: usize = 8;
    const MAX_BYTES_PER_FILE: u64 = 10 * 1024 * 1024;
    if paths.len() > MAX_FILES {
        return Err(std::io::Error::other(format!(
            "at most {MAX_FILES} attachments can be supplied"
        ))
        .into());
    }
    let attachments = paths
        .iter()
        .map(|path| {
            let path = std::fs::canonicalize(path)?;
            if harness_policy::is_protected_path(&path) {
                return Err(std::io::Error::other(
                    "credential and shell-profile files cannot be attached",
                ));
            }
            let metadata = std::fs::metadata(&path)?;
            if !metadata.is_file() {
                return Err(std::io::Error::other("attachment path is not a file"));
            }
            if metadata.len() > MAX_BYTES_PER_FILE {
                return Err(std::io::Error::other(format!(
                    "attachment {} exceeds the 10 MiB file limit",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )));
            }
            let bytes = std::fs::read(&path)?;
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| std::io::Error::other("attachment name is not valid Unicode"))?
                .to_owned();
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let media_type = match extension.as_str() {
                "png" => Some("image/png"),
                "jpg" | "jpeg" => Some("image/jpeg"),
                "gif" => Some("image/gif"),
                "webp" => Some("image/webp"),
                "pdf" => {
                    return Err(std::io::Error::other(
                        "PDF attachments are not supported yet; export the relevant pages as text or images",
                    ))
                }
                "json" => Some("application/json"),
                "xml" => Some("application/xml"),
                _ => None,
            };
            if let Some(media_type) = media_type.filter(|media_type| media_type.starts_with("image/")) {
                if !image_signature_matches(media_type, &bytes) {
                    return Err(std::io::Error::other(format!(
                        "attachment {file_name} does not contain a valid {media_type} image"
                    )));
                }
                return Ok(InputAttachment::Image {
                    file_name,
                    media_type: media_type.to_owned(),
                    data: BASE64_STANDARD.encode(bytes),
                });
            }
            let text = String::from_utf8(bytes).map_err(|_| {
                std::io::Error::other(format!(
                    "attachment {file_name} is not a supported image or UTF-8 text file"
                ))
            })?;
            if text.len() > 512 * 1024 {
                return Err(std::io::Error::other(format!(
                    "text attachment {file_name} exceeds the 512 KiB per-file limit"
                )));
            }
            Ok(InputAttachment::Text {
                file_name,
                media_type: media_type.unwrap_or("text/plain").to_owned(),
                text,
            })
        })
        .collect::<Result<Vec<_>, std::io::Error>>()
        ?;
    let image_bytes = attachments
        .iter()
        .filter_map(|attachment| match attachment {
            InputAttachment::Image { data, .. } => Some(data.len()),
            InputAttachment::Text { .. } => None,
        })
        .sum::<usize>();
    let image_raw_bytes = attachments
        .iter()
        .filter_map(|attachment| match attachment {
            InputAttachment::Image { data, .. } => Some(decoded_base64_len(data)),
            InputAttachment::Text { .. } => None,
        })
        .sum::<usize>();
    let text_bytes = attachments
        .iter()
        .filter_map(|attachment| match attachment {
            InputAttachment::Image { .. } => None,
            InputAttachment::Text { text, .. } => Some(text.len()),
        })
        .sum::<usize>();
    if image_bytes > 22 * 1024 * 1024 {
        return Err(std::io::Error::other(
            "combined image attachments exceed the 22 MiB encoded-data limit",
        )
        .into());
    }
    if image_raw_bytes > 16 * 1024 * 1024 {
        return Err(std::io::Error::other(
            "combined image attachments exceed the 16 MiB raw-data limit",
        )
        .into());
    }
    if text_bytes > 2 * 1024 * 1024 {
        return Err(
            std::io::Error::other("combined text attachments exceed the 2 MiB limit").into(),
        );
    }
    Ok(attachments)
}

fn decoded_base64_len(value: &str) -> usize {
    let padding = value
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    (value.len() / 4 * 3).saturating_sub(padding)
}

fn image_signature_matches(media_type: &str, bytes: &[u8]) -> bool {
    match media_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

fn run_agent_with_files(
    cli: &Cli,
    task: String,
    path: PathBuf,
    resume_session: Option<SessionId>,
    attachment_paths: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    let attachments = load_task_attachments(attachment_paths)?;
    run_agent_with_ui(
        cli,
        task,
        path,
        resume_session,
        AgentRunUi {
            attachments,
            ..AgentRunUi::default()
        },
        HarnessConnectionManager::default(),
    )
}

fn run_agent_with_task_mode(
    cli: &Cli,
    task: String,
    path: PathBuf,
    resume_session: Option<SessionId>,
    connector: HarnessConnectionManager,
    task_mode: TaskMode,
) -> Result<(), Box<dyn std::error::Error>> {
    run_agent_with_ui(
        cli,
        task,
        path,
        resume_session,
        AgentRunUi {
            task_mode,
            ..AgentRunUi::default()
        },
        connector,
    )
}

fn run_agent_with_ui(
    cli: &Cli,
    task: String,
    path: PathBuf,
    resume_session: Option<SessionId>,
    ui: AgentRunUi,
    connector: HarnessConnectionManager,
) -> Result<(), Box<dyn std::error::Error>> {
    let AgentRunUi {
        task_mode,
        tui,
        cancellation,
        attachments,
    } = ui;
    let path = std::fs::canonicalize(&path)?;
    let mut current_session_id = resume_session.as_ref().map(ToString::to_string);
    let description = discover_workspace(&path)?;
    let model_config = model_config(cli)?;
    let project_root = description
        .repository_root
        .clone()
        .unwrap_or_else(|| description.current_directory.clone());
    let policy = policy_for_workspace(&project_root)?;
    let execution_mode = execution_mode_name(policy.mode()).to_owned();
    let git_status = GitClient::open(&path)
        .ok()
        .and_then(|client| client.status().ok());
    if let Some(tui) = &tui {
        tui.workspace_context(
            &execution_mode,
            git_status.as_ref().and_then(|status| status.branch.clone()),
            git_status.as_ref().map(|status| !status.is_clean),
        );
    }
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
        workspace_root: path.clone(),
        user_task: task,
        attachments,
        task_mode,
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
    let cancellation = cancellation.unwrap_or_default();
    if tui.is_none() {
        let handler_token = cancellation.clone();
        ctrlc::set_handler(move || handler_token.cancel())?;
    }

    let launch = runtime_launch_config(cli, &path, &model_config)?;
    let mut client = connect_runtime_for_cli(&connector, &launch, cli, tui.as_ref())?;

    let runtime_settings = rpc_result(client.request("settings.inspect", json!({}))?)?;
    let runtime_session_root = runtime_settings
        .get("runtime")
        .and_then(|runtime| runtime.get("session_storage_path"))
        .and_then(Value::as_str)
        .ok_or("runtime did not report its session storage path")?;
    if normalized_existing_path(&cli.session_root)
        != normalized_existing_path(Path::new(runtime_session_root))
    {
        return Err(format!(
            "CLI session root does not match the connected runtime ({}); use the same --session-root or a different --rpc-address",
            runtime_session_root
        )
        .into());
    }

    // CLI flags and preferences remain authoritative for this run, including
    // when the CLI attaches to a runtime that was started by another client.
    let model_update = client.request(
        "settings.update_model",
        json!({
            "provider": provider_id(model_config.provider),
            "model": model_config.model,
            "base_url": model_config.base_url,
            "api_key_env": model_config.api_key_env,
            "reasoning_effort": model_config.reasoning_effort.and_then(|effort| {
                serde_json::to_value(effort)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
            }),
            "preference_scope": "none",
            "record_session_event": false
        }),
    )?;
    ensure_rpc_success(model_update)?;
    let run_response = client.request("agent.run", json!({"task": agent_task}))?;
    let run = rpc_result(run_response)?;
    let run_id = run
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or("RPC response did not include a run ID")?
        .to_owned();
    let event_output = EventOutput::new(cli.json, tui.clone());
    let (reader, writer) = client.split();
    let (message_sender, message_receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("harness-rpc-cli-reader".to_owned())
        .spawn(move || loop {
            match reader.receive() {
                Ok(message) => {
                    if message_sender.send(Ok(message)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = message_sender.send(Err(error.to_string()));
                    break;
                }
            }
        })?;

    let mut cancelled = false;
    let outcome = loop {
        if cancellation.is_cancelled() && !cancelled {
            let _ = writer.request("agent.cancel", json!({"run_id": run_id}));
            cancelled = true;
        }
        match message_receiver.recv_timeout(Duration::from_millis(60)) {
            Ok(Ok(ServerMessage::Notification(notification))) => match notification.method.as_str()
            {
                "agent.event" => {
                    let Some(event_value) = notification.params.get("event") else {
                        continue;
                    };
                    let event: HarnessEvent = serde_json::from_value(event_value.clone())?;
                    if matches!(event.payload, EventPayload::SessionStarted { .. }) {
                        current_session_id = Some(event.session_id.to_string());
                        if let Some(tui) = &tui {
                            tui.active_session(&event.session_id.to_string());
                        }
                    }
                    event_output.print(&event);
                }
                "approval.request" => {
                    let approval_id = notification
                        .params
                        .get("approval_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let tool = notification
                        .params
                        .get("tool")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let risk_categories = notification
                        .params
                        .get("risk_categories")
                        .and_then(Value::as_array)
                        .map(|risks| {
                            risks
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" · ")
                        })
                        .unwrap_or_default();
                    let approval_summary = approval_action_summary(&tool, &risk_categories);
                    let writer = writer.clone();
                    let tui = tui.clone();
                    let auto_approve = cli.yes;
                    std::thread::spawn(move || {
                        let approved = if auto_approve {
                            true
                        } else if let Some(tui) = tui {
                            tui.request_approval(&approval_summary)
                        } else {
                            eprint!("Approve {approval_summary}? [y/N] ");
                            let _ = io::stderr().flush();
                            let mut answer = String::new();
                            io::stdin().read_line(&mut answer).is_ok()
                                && matches!(
                                    answer.trim().to_ascii_lowercase().as_str(),
                                    "y" | "yes"
                                )
                        };
                        let method = if approved {
                            "agent.approve"
                        } else {
                            "agent.deny"
                        };
                        let _ = writer.request(method, json!({"approval_id": approval_id}));
                    });
                }
                "agent.completed"
                    if notification.params.get("run_id").and_then(Value::as_str)
                        == Some(&run_id) =>
                {
                    let outcome = notification
                        .params
                        .get("outcome")
                        .cloned()
                        .ok_or("completed RPC event did not include an outcome")?;
                    break serde_json::from_value::<harness_agent::AgentOutcome>(outcome)?;
                }
                "agent.failed"
                    if notification.params.get("run_id").and_then(Value::as_str)
                        == Some(&run_id) =>
                {
                    let message = notification
                        .params
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("agent run failed");
                    return Err(message.to_owned().into());
                }
                _ => {}
            },
            Ok(Ok(ServerMessage::Response(_))) => {}
            Ok(Err(error)) => {
                writer.shutdown();
                let recovery = recover_after_run_disconnect(
                    &connector,
                    &launch,
                    current_session_id.as_deref(),
                    tui.as_ref(),
                    cli,
                );
                return Err(format!("runtime connection failed: {error}; {recovery}").into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                writer.shutdown();
                let recovery = recover_after_run_disconnect(
                    &connector,
                    &launch,
                    current_session_id.as_deref(),
                    tui.as_ref(),
                    cli,
                );
                return Err(format!(
                    "runtime connection closed before the run completed; {recovery}"
                )
                .into());
            }
        }
    };
    writer.shutdown();
    if let Some(tui) = &tui {
        let final_git_status = GitClient::open(&project_root)
            .ok()
            .and_then(|client| client.status().ok());
        tui.workspace_context(
            &execution_mode,
            final_git_status
                .as_ref()
                .and_then(|status| status.branch.clone()),
            final_git_status.as_ref().map(|status| !status.is_clean),
        );
    }
    print_completion(cli, &outcome, tui.as_ref());
    Ok(())
}

fn approval_action_summary(tool: &Value, risks: &str) -> String {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("tool");
    let arguments = tool.get("arguments").cloned().unwrap_or(Value::Null);
    let detail = if name == "shell" {
        arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|command| format!("{name}: {command}"))
            .unwrap_or_else(|| name.to_owned())
    } else {
        format!("{name}: {arguments}")
    };
    let detail: String = detail.chars().take(200).collect();
    if risks.is_empty() {
        detail
    } else {
        format!("{detail} [{risks}]")
    }
}

fn rpc_result(response: RpcResponse) -> Result<Value, Box<dyn std::error::Error>> {
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        Err(response
            .error
            .map_or_else(
                || "runtime RPC request failed".to_owned(),
                |error| error.message,
            )
            .into())
    }
}

fn ensure_rpc_success(response: RpcResponse) -> Result<(), Box<dyn std::error::Error>> {
    rpc_result(response).map(|_| ())
}

fn normalized_existing_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |current| current.join(path))
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn cli_rpc_address(cli: &Cli, workspace: &Path) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    Ok(HarnessConnectionManager::resolve_endpoint(
        workspace,
        cli.rpc_address.as_deref(),
    )?)
}

fn runtime_launch_config(
    cli: &Cli,
    workspace: &Path,
    model: &ModelConfig,
) -> Result<RuntimeLaunchConfig, Box<dyn std::error::Error>> {
    let mut launch = RuntimeLaunchConfig::new(cli_rpc_address(cli, workspace)?, workspace);
    launch.model = Some(model.clone());
    launch.session_root = Some(cli.session_root.clone());
    launch.compaction_threshold_tokens = cli.compaction_threshold;
    launch.mock_responses = mock_runtime_responses(model)?;
    launch.apply_saved_preferences = false;
    Ok(launch)
}

fn is_verbose(cli: &Cli) -> bool {
    matches!(
        cli.log_level.to_ascii_lowercase().as_str(),
        "debug" | "trace"
    )
}

fn connect_runtime_for_cli(
    connector: &HarnessConnectionManager,
    launch: &RuntimeLaunchConfig,
    cli: &Cli,
    tui: Option<&TuiSender>,
) -> Result<harness_rpc::RpcClient, Box<dyn std::error::Error>> {
    let show_progress = tui.is_none() && !cli.json && io::stderr().is_terminal();
    if let Some(tui) = tui {
        tui.runtime_status(Some("Connecting..."));
    } else if show_progress {
        eprint!("Connecting to harness...");
        let _ = io::stderr().flush();
    }

    match connector.connect_or_start(launch) {
        Ok(mut client) => {
            if let Some(tui) = tui {
                tui.runtime_status(Some("Connected"));
            } else if show_progress {
                eprintln!(" connected.");
            }
            report_connected_runtime(cli, connector, &mut client, tui);
            Ok(client)
        }
        Err(error) => {
            if let Some(tui) = tui {
                tui.runtime_status(Some("Runtime unavailable"));
            } else if show_progress {
                eprintln!(" unavailable.");
            }
            if is_verbose(cli) {
                if let Some(tui) = tui {
                    tui.activity(format!("Runtime connection failed · {error}"));
                } else {
                    eprintln!("runtime connection failed: {error}");
                }
            }
            if is_verbose(cli) || matches!(&error, RuntimeConnectError::IncompatibleProtocol { .. })
            {
                Err(error.into())
            } else {
                Err("Harness runtime unavailable. Check `harness runtime status` or enable --log-level debug for diagnostics.".into())
            }
        }
    }
}

fn report_connected_runtime(
    cli: &Cli,
    connector: &HarnessConnectionManager,
    client: &mut harness_rpc::RpcClient,
    tui: Option<&TuiSender>,
) {
    if !is_verbose(cli) {
        return;
    }
    let status = connector.status();
    let health = client
        .request("health/check", json!({}))
        .ok()
        .and_then(|response| rpc_result(response).ok());
    let pid = health
        .as_ref()
        .and_then(|health| health.get("pid"))
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string());
    let runtime_version = health
        .as_ref()
        .and_then(|health| health.get("runtimeVersion"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let protocol_version = health
        .as_ref()
        .and_then(|health| health.get("protocolVersion"))
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_owned(), |version| version.to_string());
    let endpoint = status
        .endpoint
        .map_or_else(|| "unknown".to_owned(), |endpoint| endpoint.to_string());
    let disposition = if status.started_runtime {
        "started"
    } else {
        "reused"
    };
    let diagnostic = format!(
        "runtime: endpoint={endpoint} pid={pid} disposition={disposition} protocol={protocol_version} version={runtime_version} readiness-retries={} backoff=50-500ms",
        status.retry_count
    );
    if let Some(tui) = tui {
        tui.activity(diagnostic);
    } else {
        eprintln!("{diagnostic}");
    }
}

fn recover_after_run_disconnect(
    connector: &HarnessConnectionManager,
    launch: &RuntimeLaunchConfig,
    session_id: Option<&str>,
    tui: Option<&TuiSender>,
    cli: &Cli,
) -> String {
    if let Some(tui) = tui {
        tui.runtime_status(Some("Reconnecting..."));
    } else if !cli.json && io::stderr().is_terminal() {
        eprintln!("Runtime connection lost; reconnecting...");
    }
    match connector.reconnect(launch) {
        Ok(mut client) => {
            if let Some(tui) = tui {
                tui.runtime_status(Some("Connected"));
            }
            report_connected_runtime(cli, connector, &mut client, tui);
            let refreshed = session_id.is_some_and(|session_id| {
                client
                    .request("session.state", json!({"session_id": session_id}))
                    .ok()
                    .is_some_and(|response| response.ok)
            });
            let message = if refreshed {
                "runtime reconnected and the saved session state was refreshed; the interrupted task was not replayed. Use /resume or `harness resume` to continue."
            } else {
                "runtime reconnected; the interrupted task was not replayed. Check the session list before continuing."
            };
            if let Some(tui) = tui {
                tui.activity(message);
            }
            message.to_owned()
        }
        Err(error) => {
            if let Some(tui) = tui {
                tui.runtime_status(Some("Runtime unavailable"));
                if is_verbose(cli) {
                    tui.activity(format!("Reconnect failed · {error}"));
                }
            }
            if is_verbose(cli) {
                format!("runtime reconnect failed: {error}; the task was not replayed")
            } else {
                "runtime reconnect failed; the task was not replayed".to_owned()
            }
        }
    }
}

fn runtime_command(cli: &Cli, command: &RuntimeCommand) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = std::fs::canonicalize(&cli.workspace)?;
    let launch = RuntimeLaunchConfig::new(cli_rpc_address(cli, &workspace)?, &workspace);
    let connector = HarnessConnectionManager::default();
    match command {
        RuntimeCommand::Status => match connector.connect_existing(&launch) {
            Ok(mut client) => {
                let health = health_summary(&mut client)?;
                if cli.json {
                    println!(
                        "{}",
                        json!({"type":"runtime","status":"connected","runtime":health})
                    );
                } else {
                    println!(
                        "Harness runtime connected · pid {} · protocol {} · v{}",
                        health["pid"], health["protocolVersion"], health["runtimeVersion"]
                    );
                }
                Ok(())
            }
            Err(_) => {
                if cli.json {
                    println!("{}", json!({"type":"runtime","status":"unavailable"}));
                } else {
                    println!("Harness runtime unavailable.");
                }
                Ok(())
            }
        },
        RuntimeCommand::Stop => stop_runtime(cli, &connector, &launch),
        RuntimeCommand::Restart => {
            let model = model_config(cli)?;
            let launch = runtime_launch_config(cli, &workspace, &model)?;
            if let Ok(mut client) = connector.connect_existing(&launch) {
                let _ = request_runtime_shutdown(&mut client)?;
                let deadline = std::time::Instant::now() + Duration::from_secs(8);
                let mut stopped = false;
                while std::time::Instant::now() < deadline {
                    if connector.connect_existing(&launch).is_err() {
                        stopped = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !stopped {
                    return Err("timed out waiting for the Harness runtime to stop".into());
                }
            }
            let mut client = connector.connect_or_start(&launch)?;
            let health = health_summary(&mut client)?;
            if cli.json {
                println!(
                    "{}",
                    json!({"type":"runtime","status":"restarted","runtime":health})
                );
            } else {
                println!(
                    "Harness runtime restarted · pid {} · v{}",
                    health["pid"], health["runtimeVersion"]
                );
            }
            Ok(())
        }
    }
}

fn stop_runtime(
    cli: &Cli,
    connector: &HarnessConnectionManager,
    launch: &RuntimeLaunchConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    match connector.connect_existing(launch) {
        Ok(mut client) => {
            let result = request_runtime_shutdown(&mut client)?;
            if cli.json {
                println!(
                    "{}",
                    json!({"type":"runtime","status":"stopping","result":result})
                );
            } else {
                println!("Harness runtime shutdown requested.");
            }
        }
        Err(_) if cli.json => println!("{}", json!({"type":"runtime","status":"not_running"})),
        Err(_) => println!("Harness runtime is not running."),
    }
    Ok(())
}

fn health_summary(
    client: &mut harness_rpc::RpcClient,
) -> Result<Value, Box<dyn std::error::Error>> {
    rpc_result(client.request("health/check", json!({}))?)
}

fn request_runtime_shutdown(
    client: &mut harness_rpc::RpcClient,
) -> Result<Value, Box<dyn std::error::Error>> {
    let health = health_summary(client)?;
    let instance_id = health
        .get("instanceId")
        .and_then(Value::as_str)
        .ok_or("runtime health response did not include an instance ID")?;
    rpc_result(client.request("rpc.shutdown", json!({"instanceId": instance_id}))?)
}

fn auth_command(cli: &Cli, command: &AuthCommand) -> Result<(), Box<dyn std::error::Error>> {
    let store = SystemCredentialStore::new();
    match command {
        AuthCommand::List => print_credential_statuses(cli, &store),
        AuthCommand::Connect { provider } => connect_provider_cli(cli, provider.as_deref()),
        AuthCommand::Disconnect { provider } => {
            let provider = parse_auth_provider(provider)?;
            let config = credential_model_config(cli, provider);
            let status = store.disconnect(provider_id(provider), &config.api_key_env)?;
            print_credential_status(cli, provider, &status, "Credential disconnected.")
        }
    }
}

fn print_credential_statuses(
    cli: &Cli,
    store: &dyn CredentialStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let providers = [
        ProviderKind::OpenAi,
        ProviderKind::Anthropic,
        ProviderKind::Gemini,
        ProviderKind::OpenCodeZen,
        ProviderKind::OpenCodeGo,
    ]
    .into_iter()
    .map(|provider| {
        let config = credential_model_config(cli, provider);
        let status = store
            .status(provider_id(provider), &config.api_key_env)
            .unwrap_or(CredentialStatus {
                available: false,
                source: CredentialSource::Unavailable,
                env_var: config.api_key_env,
            });
        json!({
            "provider_id": provider_id(provider),
            "provider": provider_label(provider),
            "connected": status.available,
            "source": status.source,
            "env_var": status.env_var,
        })
    })
    .collect::<Vec<_>>();
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"providers": providers}))?
        );
    } else {
        for provider in providers {
            let label = provider["provider"].as_str().unwrap_or("Provider");
            let connected = provider["connected"].as_bool().unwrap_or(false);
            let source = provider["source"].as_str().unwrap_or("unavailable");
            let env_var = provider["env_var"].as_str().unwrap_or_default();
            let status = match source {
                "environment" => format!("Connected · environment ({env_var})"),
                "keychain" => "Connected · OS credential store".to_owned(),
                "unavailable" => "OS credential store unavailable".to_owned(),
                _ if connected => "Connected".to_owned(),
                _ => format!("Not connected · set {env_var} or run `harness auth connect`"),
            };
            println!("{label:<15} {status}");
        }
    }
    Ok(())
}

fn connect_provider_cli(
    cli: &Cli,
    provider_id_argument: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() {
        return Err("provider connection requires an interactive terminal".into());
    }
    let provider = match provider_id_argument {
        Some(value) => parse_auth_provider(value)?,
        None => select_auth_provider()?,
    };
    let config = credential_model_config(cli, provider);
    let store = SystemCredentialStore::new();
    let current = store
        .status(provider_id(provider), &config.api_key_env)
        .unwrap_or(CredentialStatus {
            available: false,
            source: CredentialSource::Unavailable,
            env_var: config.api_key_env.clone(),
        });
    if current.source == CredentialSource::Environment {
        let key = store
            .get(provider_id(provider), &config.api_key_env)?
            .ok_or("environment credential became unavailable")?;
        validate_provider_credential(&config, key.expose_secret()).map_err(|error| {
            harness_core::redact_sensitive(&format!("credential validation failed: {error}"))
        })?;
        return print_credential_status(
            cli,
            provider,
            &current,
            "Environment credential validated; it remains managed by the environment.",
        );
    }

    eprintln!(
        "Connecting {} ({})",
        provider_label(provider),
        config.api_key_env
    );
    let key = CredentialSecret::new(rpassword::prompt_password("API key (input hidden): ")?);
    validate_provider_credential(&config, key.expose_secret()).map_err(|error| {
        harness_core::redact_sensitive(&format!("credential validation failed: {error}"))
    })?;
    let status = store.store(
        provider_id(provider),
        &config.api_key_env,
        key.expose_secret(),
    )?;
    print_credential_status(
        cli,
        provider,
        &status,
        "Credential validated and stored in the OS credential store.",
    )
}

fn select_auth_provider() -> Result<ProviderKind, Box<dyn std::error::Error>> {
    let providers = [
        (ProviderKind::OpenAi, "OpenAI"),
        (ProviderKind::Anthropic, "Anthropic"),
        (ProviderKind::Gemini, "Google Gemini"),
        (ProviderKind::OpenCodeZen, "OpenCode Zen"),
        (ProviderKind::OpenCodeGo, "OpenCode Go"),
    ];
    for (index, (_, label)) in providers.iter().enumerate() {
        eprintln!("{}. {label}", index + 1);
    }
    eprint!("Choose provider [1-5]: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let selection = line.trim().parse::<usize>()?;
    providers
        .get(selection.saturating_sub(1))
        .map(|(provider, _)| *provider)
        .ok_or_else(|| "choose a provider from 1 to 5".into())
}

fn parse_auth_provider(value: &str) -> Result<ProviderKind, Box<dyn std::error::Error>> {
    match value.trim().to_ascii_lowercase().as_str() {
        "openai" => Ok(ProviderKind::OpenAi),
        "anthropic" => Ok(ProviderKind::Anthropic),
        "gemini" | "google-gemini" | "google" => Ok(ProviderKind::Gemini),
        "opencode-zen" | "opencode" => Ok(ProviderKind::OpenCodeZen),
        "opencode-go" => Ok(ProviderKind::OpenCodeGo),
        _ => Err(format!("unknown provider `{value}`").into()),
    }
}

fn credential_model_config(cli: &Cli, provider: ProviderKind) -> ModelConfig {
    let active = model_config(cli).ok();
    active
        .filter(|config| config.provider == provider)
        .unwrap_or_else(|| ModelConfig::for_provider(provider))
}

fn provider_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::OpenCodeGo => "opencode-go",
    }
}

fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "Mock",
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Anthropic => "Anthropic",
        ProviderKind::Gemini => "Google Gemini",
        ProviderKind::OpenCodeZen => "OpenCode Zen",
        ProviderKind::OpenCodeGo => "OpenCode Go",
    }
}

fn print_credential_status(
    cli: &Cli,
    provider: ProviderKind,
    status: &CredentialStatus,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "provider_id": provider_id(provider),
                "provider": provider_label(provider),
                "connected": status.available,
                "source": status.source,
                "env_var": status.env_var,
                "message": message,
            }))?
        );
    } else {
        let status_text = match status.source {
            CredentialSource::Environment => {
                format!("Connected · environment ({})", status.env_var)
            }
            CredentialSource::Keychain => "Connected · OS credential store".to_owned(),
            CredentialSource::None => "Not connected".to_owned(),
            CredentialSource::Unavailable => "OS credential store unavailable".to_owned(),
        };
        println!("{}: {status_text}", provider_label(provider));
        println!("{message}");
    }
    Ok(())
}

fn mock_runtime_responses(
    config: &ModelConfig,
) -> Result<Option<Vec<ModelResponse>>, ProviderError> {
    if config.provider != ProviderKind::Mock {
        return Ok(None);
    }
    if let Some(responses) = mock_script_responses()? {
        return Ok(Some(responses));
    }
    if let Some(repair) = mock_repair_script()? {
        return Ok(Some(vec![
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
        ]));
    }
    Ok(Some(vec![
        tool_response("list_directory", json!({"path": "."})),
        tool_response(
            "write_file",
            json!({
                "path": "mock-output.txt",
                "content": "Generated by the CLI mock workflow."
            }),
        ),
        text_response("Mock coding workflow completed."),
    ]))
}

/// Loads a bounded, deterministic response sequence for isolated coding-agent
/// evaluations. This is deliberately available only to the mock provider.
fn mock_script_responses() -> Result<Option<Vec<ModelResponse>>, ProviderError> {
    let Some(raw) = std::env::var_os("COGITO_MOCK_SCRIPT") else {
        return Ok(None);
    };
    let raw = raw.to_string_lossy();
    parse_mock_script(&raw).map(Some)
}

fn parse_mock_script(raw: &str) -> Result<Vec<ModelResponse>, ProviderError> {
    let invalid = |reason: String| ProviderError::Configuration {
        reason: format!("COGITO_MOCK_SCRIPT: {reason}"),
    };
    if raw.len() > 256 * 1024 {
        return Err(invalid("script exceeds 256 KiB".to_owned()));
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|error| invalid(format!("expected JSON ({error})")))?;
    let steps = value
        .get("steps")
        .and_then(Value::as_array)
        .filter(|steps| !steps.is_empty() && steps.len() <= 128)
        .ok_or_else(|| invalid("expected 1 to 128 steps".to_owned()))?;

    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let tool = step.get("tool").and_then(Value::as_str);
            let text = step.get("text").and_then(Value::as_str);
            match (tool, text) {
                (Some(name), None) if !name.is_empty() && name.len() <= 128 => {
                    let arguments = step.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    if arguments.to_string().len() > 64 * 1024 {
                        return Err(invalid(format!("step {index} arguments exceed 64 KiB")));
                    }
                    Ok(ModelResponse {
                        id: format!("eval-response-{index}"),
                        model: "mock-eval".to_owned(),
                        content: Vec::new(),
                        tool_calls: vec![ToolCall {
                            id: format!("eval-call-{index}"),
                            name: name.to_owned(),
                            arguments,
                        }],
                        finish_reason: FinishReason::ToolCalls,
                        // The scripted provider is a harness fixture, not a
                        // token-pricing model. Context estimates are reported
                        // separately by the evaluation runner.
                        usage: None,
                    })
                }
                (None, Some(text)) if text.len() <= 16 * 1024 => Ok(ModelResponse {
                    id: format!("eval-response-{index}"),
                    model: "mock-eval".to_owned(),
                    content: vec![ContentBlock::Text {
                        text: text.to_owned(),
                    }],
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                }),
                _ => Err(invalid(format!(
                    "step {index} must contain exactly one bounded `tool` or `text` field"
                ))),
            }
        })
        .collect()
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
        tui.active_session(&outcome.session_id.to_string());
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
            tui.runtime_event(&event.payload);
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
                "shell" | "run_command" => "Running command",
                "start_background_command" => "Starting background process",
                "read_process_output" => "Reading process logs",
                "list_processes" => "Inspecting processes",
                "stop_process" => "Stopping process",
                "wait_for_process_output" => "Waiting for process readiness",
                "delegate_subagents" => "Delegating to read-only agents",
                _ => "Using tool",
            };
            Some(match target {
                Some(target) => format!("{description} · {}", concise(target, 100)),
                None if tool == "delegate_subagents" => description.to_owned(),
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
        EventPayload::BackgroundProcessStarted { command, pid, .. } => Some(format!(
            "Background process started · PID {pid} · {}",
            concise(command, 100)
        )),
        EventPayload::BackgroundProcessStatus {
            status, process_id, ..
        } => Some(format!(
            "Background process {status} · {}",
            concise(process_id, 50)
        )),
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

#[cfg(test)]
mod interactive_command_tests {
    use super::*;

    #[test]
    fn scripted_mock_responses_are_bounded_and_deterministic() {
        let responses = parse_mock_script(
            r#"{"steps":[{"tool":"read_file","arguments":{"path":"src/lib.py"}},{"text":"done"}]}"#,
        )
        .unwrap();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].tool_calls[0].name, "read_file");
        assert_eq!(responses[0].tool_calls[0].id, "eval-call-0");
        assert_eq!(
            responses[1].content,
            [ContentBlock::Text {
                text: "done".into()
            }]
        );
        assert!(responses.iter().all(|response| response.usage.is_none()));
    }

    #[test]
    fn scripted_mock_rejects_ambiguous_or_unbounded_steps() {
        assert!(parse_mock_script(r#"{"steps":[]}"#).is_err());
        assert!(
            parse_mock_script(r#"{"steps":[{"tool":"read_file","text":"ambiguous"}]}"#).is_err()
        );
        assert!(parse_mock_script(&format!(
            "{{\"steps\":[{{\"text\":\"{}\"}}]}}",
            "x".repeat(16 * 1024 + 1)
        ))
        .is_err());
    }

    #[test]
    fn subagent_delegation_uses_a_compact_activity_label() {
        let event = HarnessEvent::new(
            harness_core::SessionId::new("session-subagent-test").unwrap(),
            EventPayload::ToolRequested {
                tool: "delegate_subagents".to_owned(),
                arguments: Default::default(),
            },
            None,
            None,
        );
        assert_eq!(
            format_activity(&event).as_deref(),
            Some("Delegating to read-only agents")
        );
    }

    #[test]
    fn advertised_commands_have_a_dispatch_target_and_parse_arguments() {
        for definition in INTERACTIVE_COMMANDS {
            let (command, _) = parse_interactive_command(definition.usage)
                .unwrap_or_else(|| panic!("{} should parse", definition.usage));
            assert_eq!(command, definition.command, "{}", definition.name);
        }

        assert_eq!(
            parse_interactive_command("  /resume session-123  finish the remaining work  "),
            Some((
                InteractiveCommand::Resume,
                "session-123  finish the remaining work"
            )),
        );
        assert_eq!(
            parse_interactive_command("/quit"),
            Some((InteractiveCommand::Exit, "")),
        );
        assert_eq!(
            parse_interactive_command("/models refresh opencode-go"),
            Some((InteractiveCommand::Models, "refresh opencode-go")),
        );
        assert_eq!(
            parse_interactive_command("/model opencode-go/vendor/model --effort high"),
            Some((
                InteractiveCommand::ModelSelect,
                "opencode-go/vendor/model --effort high"
            )),
        );
        let parsed = Cli::try_parse_from([
            "harness",
            "model",
            "anthropic/claude-test",
            "--effort",
            "high",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Some(Command::Model { .. })));
        let parsed =
            Cli::try_parse_from(["harness", "models", "--refresh", "--provider", "gemini"])
                .unwrap();
        assert!(matches!(parsed.command, Some(Command::Models { .. })));
        let parsed = Cli::try_parse_from(["harness", "mcp", "refresh", "docs"]).unwrap();
        assert!(matches!(parsed.command, Some(Command::Mcp { .. })));
        assert_eq!(parse_interactive_command("/not-a-command"), None);
        assert_eq!(parse_interactive_command("ordinary task"), None);
    }

    #[test]
    fn help_uses_the_command_catalog_and_limits_plain_mode_to_supported_commands() {
        let full_help = interactive_help(false);
        for command in [
            "/help",
            "/model",
            "/models",
            "/mcp",
            "/mode",
            "/explore",
            "/plan",
            "/goal",
            "/plan-status",
            "/code",
            "/diff",
            "/undo",
            "/resume",
            "/clear",
            "/exit",
        ] {
            assert!(
                full_help.contains(command),
                "missing {command} from {full_help}"
            );
        }
        let plain_help = interactive_help(true);
        assert!(plain_help.contains("/mode"));
        assert!(plain_help.contains("/explore"));
        assert!(plain_help.contains("/plan"));
        assert!(plain_help.contains("/plan-status"));
        assert!(plain_help.contains("/goal"));
        assert!(plain_help.contains("/code"));
        assert!(!plain_help.contains("/cancel"));
        assert!(!plain_help.contains("/clear"));
    }

    #[test]
    fn mode_command_reads_the_configured_workspace_policy() {
        let directory = tempfile::tempdir().unwrap();
        let agent_config = directory.path().join(".agent");
        std::fs::create_dir_all(&agent_config).unwrap();
        std::fs::write(
            agent_config.join("config.toml"),
            "[policy]\nmode = 'safe'\n",
        )
        .unwrap();
        let cli = Cli {
            workspace: directory.path().to_path_buf(),
            log_level: "info".to_owned(),
            model_provider: None,
            model: None,
            model_effort: None,
            rpc_address: None,
            session_root: directory.path().join("sessions"),
            compaction_threshold: None,
            json: false,
            yes: false,
            command: None,
        };
        assert_eq!(configured_mode(&cli).unwrap(), "safe");
    }

    #[test]
    fn goal_and_plan_status_commands_read_the_persisted_session_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let mut cli = Cli::try_parse_from(["harness", "--json"]).unwrap();
        cli.session_root = directory.path().join("sessions");
        let store = JsonlSessionStore::new(&cli.session_root).unwrap();
        let session = store.create(directory.path()).unwrap();
        let mut task_run = harness_session::TaskRun::new("Keep the objective after compaction");
        task_run.execution_plan = Some(harness_session::ExecutionPlan {
            revision: 1,
            status: harness_session::PlanItemStatus::InProgress,
            milestones: vec![harness_session::ExecutionMilestone {
                title: "Persist state".to_owned(),
                tasks: vec![harness_session::ExecutionTask {
                    description: "Save milestone state".to_owned(),
                    status: harness_session::PlanItemStatus::InProgress,
                }],
                ..harness_session::ExecutionMilestone::default()
            }],
            ..harness_session::ExecutionPlan::default()
        });
        store
            .append_event(
                &session.id,
                HarnessEvent::new(
                    session.id.clone(),
                    EventPayload::TaskRunUpdated { task_run },
                    None,
                    None,
                ),
            )
            .unwrap();

        assert!(task_state_command(&cli, Some(&session.id.to_string()), false).is_ok());
        assert!(task_state_command(&cli, Some(&session.id.to_string()), true).is_ok());
        assert_eq!(
            session_argument_or_active("", Some(session.id.to_string())).unwrap(),
            session.id.to_string()
        );
    }
}
