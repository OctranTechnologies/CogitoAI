//! Shared runtime composition used by the standalone RPC process.
//!
//! The CLI and desktop shell both start the same RPC-backed runtime through
//! [`RuntimeConnector`]. This module owns the server-side composition so the
//! clients do not build agent runners themselves.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use harness_agent::{AgentLimits, AgentRunner, CompactionConfig};
use harness_context::ContextBuilder;
use harness_core::{discover_workspace, AgentRuntime, Error, RunOutcome, RunRequest};
use harness_git::{CheckpointStore, GitClient, ShadowCheckpointStore};
use harness_models::{
    provider_from_config_with_store, CredentialStore, FinishReason, ModelConfig, ModelProvider,
    ModelResponse, ProviderError, ProviderKind, ScriptedMockProvider, SystemCredentialStore,
    ToolCall, Usage,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_session::{EventBus, JsonlSessionStore, SessionStore};
use harness_tools::{
    CancellationToken, LocalExecutionEnvironment, McpManager, ProcessManager, ToolRegistry,
};
use harness_verification::CommandVerifier;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::metadata::{set_private_file_mode, RuntimeMetadataStore};
use crate::{AgentRunnerFactory, ApprovalBroker, RpcApprovalHandler, RpcServer, Runtime};

/// Options used when a client needs to bring up a local runtime.
#[derive(Clone, Serialize, Deserialize)]
pub struct RuntimeLaunchConfig {
    pub address: SocketAddr,
    /// The expected instance identity is set by `RuntimeConnector` before the
    /// runtime starts so the metadata and health response agree.
    pub instance_id: Option<String>,
    pub workspace_root: PathBuf,
    /// If absent, provider environment configuration and saved preferences are
    /// used. CLI invocations pass their resolved config to preserve flags.
    pub model: Option<ModelConfig>,
    pub session_root: Option<PathBuf>,
    pub compaction_threshold_tokens: Option<u32>,
    /// A deterministic script is only used by the mock provider.
    pub mock_responses: Option<Vec<ModelResponse>>,
    /// Desktop startup resolves user/project preferences in the runtime.
    pub apply_saved_preferences: bool,
}

impl RuntimeLaunchConfig {
    pub fn new(address: SocketAddr, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            address,
            instance_id: None,
            workspace_root: workspace_root.into(),
            model: None,
            session_root: None,
            compaction_threshold_tokens: None,
            mock_responses: None,
            apply_saved_preferences: true,
        }
    }
}

/// Starts a local RPC server for the supplied runtime configuration.
pub trait RuntimeLauncher: Send + Sync {
    fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String>;

    /// Called after readiness succeeds so a process launcher can relinquish
    /// its startup handle without stopping the persistent runtime.
    fn startup_succeeded(&self, _instance_id: &str) {}

    /// Stops only a process launched by this client when it failed readiness.
    fn startup_failed(&self, _instance_id: &str) {}
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeLaunchInfo {
    pub endpoint: SocketAddr,
    pub instance_id: String,
    pub pid: u32,
    pub runtime_version: String,
}

/// In-process launcher retained for runtime unit tests and local harnesses.
#[derive(Default)]
pub struct EmbeddedRuntimeLauncher;

impl RuntimeLauncher for EmbeddedRuntimeLauncher {
    fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String> {
        let approvals = Arc::new(ApprovalBroker::new());
        let runtime = build_runtime(config, Arc::clone(&approvals))?;
        let instance_id = runtime.instance_id().to_owned();
        let server = RpcServer::bind_with_approvals(config.address, runtime, approvals)
            .map_err(|error| error.to_string())?;
        let endpoint = server.local_addr().map_err(|error| error.to_string())?;
        std::thread::Builder::new()
            .name("cogito-rpc-runtime".to_owned())
            .spawn(move || {
                let _ = server.serve();
            })
            .map_err(|error| format!("could not start the RPC server: {error}"))?;
        Ok(RuntimeLaunchInfo {
            endpoint,
            instance_id,
            pid: std::process::id(),
            runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
        })
    }
}

/// Entry point used by the detached runtime binary. The private readiness file
/// avoids inheriting a long-lived stdout pipe into client process trees.
pub fn serve_runtime_process(config: RuntimeLaunchConfig) -> Result<(), String> {
    let approvals = Arc::new(ApprovalBroker::new());
    let runtime = build_runtime(&config, Arc::clone(&approvals))?;
    let instance_id = runtime.instance_id().to_owned();
    let server = RpcServer::bind_with_approvals(config.address, runtime, approvals)
        .map_err(|error| error.to_string())?;
    let launch = RuntimeLaunchInfo {
        endpoint: server.local_addr().map_err(|error| error.to_string())?,
        instance_id,
        pid: std::process::id(),
        runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let ready_path = std::env::var_os("COGITO_RUNTIME_READY_FILE")
        .map(PathBuf::from)
        .ok_or_else(|| "runtime readiness file path is not configured".to_owned())?;
    write_runtime_readiness(&ready_path, &launch)?;
    server.serve().map_err(|error| error.to_string())
}

fn write_runtime_readiness(
    path: &std::path::Path,
    launch: &RuntimeLaunchInfo,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "runtime readiness path has no parent directory".to_owned())?;
    RuntimeMetadataStore::new(parent)
        .ensure_private_directory()
        .map_err(|error| format!("could not prepare runtime readiness directory: {error}"))?;
    let temporary = path.with_extension("ready.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_mode(&mut options);
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("could not create runtime readiness file: {error}"))?;
    let payload = serde_json::to_vec(launch).map_err(|error| error.to_string())?;
    if let Err(error) = file.write_all(&payload).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write runtime readiness file: {error}"));
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not publish runtime readiness file: {error}"));
    }
    Ok(())
}

struct UnavailableAgent;

impl AgentRuntime for UnavailableAgent {
    fn run(&self, _request: RunRequest) -> Result<RunOutcome, Error> {
        Err(Error::InvalidRequest {
            reason: "agent tasks must be run through the RPC runtime".to_owned(),
        })
    }
}

struct RuntimeRunnerFactory {
    workspace_root: PathBuf,
    sessions: Arc<dyn SessionStore>,
    event_bus: EventBus,
    checkpoints: Arc<dyn CheckpointStore>,
    checkpoints_enabled: bool,
    credentials: Arc<dyn CredentialStore>,
    approvals: Arc<ApprovalBroker>,
    mock_responses: Vec<ModelResponse>,
    compaction_threshold_tokens: Option<u32>,
    process_manager: Arc<ProcessManager>,
    mcp_manager: Arc<McpManager>,
}

impl AgentRunnerFactory for RuntimeRunnerFactory {
    fn build(&self, model: &ModelConfig, mode: ExecutionMode) -> Result<Arc<AgentRunner>, Error> {
        let provider: Arc<dyn ModelProvider> = if model.provider == ProviderKind::Mock {
            Arc::new(ScriptedMockProvider::new(
                model.model.clone(),
                self.mock_responses.clone(),
            ))
        } else {
            Arc::from(
                provider_from_config_with_store(model, self.credentials.as_ref()).map_err(
                    |error: ProviderError| Error::InvalidConfig {
                        reason: error.to_string(),
                    },
                )?,
            )
        };
        let project_root = discover_workspace(&self.workspace_root)
            .map_err(|error| Error::InvalidConfig {
                reason: error.to_string(),
            })?
            .repository_root
            .unwrap_or_else(|| self.workspace_root.clone());
        let policy_engine = policy_for_workspace(&project_root)
            .map_err(|error| Error::InvalidConfig {
                reason: error.to_string(),
            })?
            .with_mode(mode);
        let execution_environment =
            Arc::new(LocalExecutionEnvironment::new(policy_engine.network_access));
        let policy: Arc<dyn Policy> = Arc::new(policy_engine);
        let threshold = self
            .compaction_threshold_tokens
            .unwrap_or_else(|| CompactionConfig::default().threshold_tokens);
        let mut runner = AgentRunner::new(
            provider,
            model.model.clone(),
            ToolRegistry::with_workspace_tools_and_process_manager(
                CancellationToken::new(),
                Arc::clone(&self.process_manager),
            )
            .with_mcp_manager(Arc::clone(&self.mcp_manager)),
            policy,
            Arc::clone(&self.sessions),
            ContextBuilder::default(),
            AgentLimits::from_env(),
            Arc::new(RpcApprovalHandler::new(Arc::clone(&self.approvals))),
        )
        .with_execution_environment(execution_environment.clone())
        .with_event_bus(self.event_bus.clone())
        .with_subagents()
        .with_reasoning_config(model.reasoning_config())
        .with_compaction_config(CompactionConfig {
            threshold_tokens: threshold,
            ..CompactionConfig::default()
        })
        .with_verifier(Arc::new(CommandVerifier::new(execution_environment)));
        if self.checkpoints_enabled {
            runner = runner.with_checkpoints(Arc::clone(&self.checkpoints));
        }
        Ok(Arc::new(runner))
    }
}

fn build_runtime(
    config: &RuntimeLaunchConfig,
    approvals: Arc<ApprovalBroker>,
) -> Result<Arc<Runtime>, String> {
    let workspace_root = std::fs::canonicalize(&config.workspace_root)
        .map_err(|error| format!("could not open workspace: {error}"))?;
    let description = discover_workspace(&workspace_root).map_err(|error| error.to_string())?;
    let project_root = description
        .repository_root
        .clone()
        .unwrap_or_else(|| description.current_directory.clone());
    let event_bus = EventBus::new();
    let session_root = config
        .session_root
        .clone()
        .unwrap_or_else(|| workspace_root.join(".cogito/sessions"));
    let sessions: Arc<dyn SessionStore> = Arc::new(
        JsonlSessionStore::with_event_bus(session_root.clone(), event_bus.clone())
            .map_err(|error| error.to_string())?,
    );
    let checkpoints_enabled = GitClient::open(&workspace_root).is_ok();
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        ShadowCheckpointStore::with_event_bus(
            workspace_root.join(".cogito/checkpoints"),
            Some(event_bus.clone()),
        )
        .map_err(|error| error.to_string())?,
    );
    let policy_engine = policy_for_workspace(&project_root).map_err(|error| error.to_string())?;
    let mode = policy_engine.mode;
    let policy: Arc<dyn Policy> = Arc::new(policy_engine);
    let model = config.model.clone().unwrap_or_else(ModelConfig::from_env);
    let credentials: Arc<dyn CredentialStore> = Arc::new(SystemCredentialStore::new());
    let process_manager = Arc::new(ProcessManager::default());
    let mcp_manager =
        Arc::new(McpManager::from_workspace(&project_root).map_err(|error| error.to_string())?);
    let runner_factory = Arc::new(RuntimeRunnerFactory {
        workspace_root: workspace_root.clone(),
        sessions: Arc::clone(&sessions),
        event_bus: event_bus.clone(),
        checkpoints: Arc::clone(&checkpoints),
        checkpoints_enabled,
        credentials: Arc::clone(&credentials),
        approvals,
        mock_responses: config
            .mock_responses
            .clone()
            .unwrap_or_else(default_mock_responses),
        compaction_threshold_tokens: config.compaction_threshold_tokens,
        process_manager: Arc::clone(&process_manager),
        mcp_manager: Arc::clone(&mcp_manager),
    });
    let runner = runner_factory
        .build(&model, mode)
        .map_err(|error| error.to_string())?;
    let runtime = Runtime::new(
        Arc::new(UnavailableAgent),
        Vec::new(),
        ToolRegistry::with_workspace_tools_and_process_manager(
            CancellationToken::new(),
            process_manager,
        )
        .with_mcp_manager(Arc::clone(&mcp_manager)),
        policy,
        sessions,
        checkpoints,
        Vec::new(),
    )
    .with_instance_id(
        config
            .instance_id
            .clone()
            .unwrap_or_else(crate::metadata::new_instance_id),
    )
    .with_agent_runner(runner)
    .with_runner_factory(runner_factory)
    .with_credential_store(credentials)
    .with_workspace_root(workspace_root)
    .with_session_root(session_root);
    let runtime = runtime.with_mcp_manager(mcp_manager);
    if config.apply_saved_preferences && config.model.is_none() {
        runtime
            .apply_saved_model_preferences()
            .map_err(|error| error.to_string())?;
    }
    Ok(Arc::new(runtime))
}

fn policy_for_workspace(
    project_root: &std::path::Path,
) -> Result<PolicyEngine, harness_policy::PolicyError> {
    let config = project_root.join(".agent/config.toml");
    if config.is_file() {
        PolicyEngine::from_file(&config, project_root)
    } else {
        Ok(PolicyEngine::new(ExecutionMode::Normal, project_root))
    }
}

fn default_mock_responses() -> Vec<ModelResponse> {
    vec![
        tool_response("list_directory", json!({"path": "."})),
        tool_response(
            "write_file",
            json!({
                "path": "mock-output.txt",
                "content": "Generated by the CogitoAI mock runtime."
            }),
        ),
        ModelResponse {
            id: "mock-final".to_owned(),
            model: "mock".to_owned(),
            content: vec![harness_models::ContentBlock::Text {
                text: "Mock coding workflow completed.".to_owned(),
            }],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Some(Usage::new(1, 1)),
        },
    ]
}

fn tool_response(name: &str, arguments: serde_json::Value) -> ModelResponse {
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
