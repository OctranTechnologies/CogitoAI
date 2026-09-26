use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use harness_agent::{AgentLimits, AgentRunner};
use harness_context::ContextBuilder;
use harness_core::{discover_workspace, init_logging, AgentRuntime, Error, RunOutcome, RunRequest};
use harness_git::{CheckpointStore, GitClient, ShadowCheckpointStore};
use harness_models::{
    ContentBlock, FinishReason, ModelConfig, ModelProvider, ModelResponse, ProviderKind,
    ScriptedMockProvider, ToolCall, Usage,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_rpc::{AgentRunnerFactory, ApprovalBroker, RpcApprovalHandler, RpcServer, Runtime};
use harness_session::{EventBus, JsonlSessionStore, SessionStore};
use harness_tools::ToolRegistry;
use serde_json::json;

struct UnavailableAgent;

impl AgentRuntime for UnavailableAgent {
    fn run(&self, _request: RunRequest) -> Result<RunOutcome, Error> {
        Err(Error::InvalidRequest {
            reason: "use the RPC server worker for agent execution".to_owned(),
        })
    }
}

fn tool_response(name: &str, arguments: serde_json::Value) -> ModelResponse {
    ModelResponse {
        id: format!("dev-{name}"),
        model: "dev-mock".to_owned(),
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

fn final_response() -> ModelResponse {
    ModelResponse {
        id: "dev-final".to_owned(),
        model: "dev-mock".to_owned(),
        content: vec![ContentBlock::Text {
            text: "Development runtime task complete.".to_owned(),
        }],
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(Usage::new(1, 1)),
    }
}

/// Rebuilds the agent runner when the desktop changes the model or the
/// execution mode, so a settings change takes effect on the next run instead
/// of requiring a restart.
struct DevRunnerFactory {
    provider_script: Arc<Vec<ModelResponse>>,
    project_root: PathBuf,
    sessions: Arc<dyn SessionStore>,
    event_bus: EventBus,
    approvals: Arc<ApprovalBroker>,
    checkpoints: Arc<dyn CheckpointStore>,
}

impl AgentRunnerFactory for DevRunnerFactory {
    fn build(
        &self,
        model: &ModelConfig,
        mode: ExecutionMode,
    ) -> Result<Arc<AgentRunner>, harness_core::Error> {
        let provider: Arc<dyn ModelProvider> = if matches!(model.provider, ProviderKind::Mock) {
            // The dev runtime always uses its scripted provider so a run stays
            // deterministic, but it is still addressed by the selected model so
            // switching is observable end to end.
            Arc::new(ScriptedMockProvider::new(
                model.model.clone(),
                self.provider_script.as_ref().clone(),
            ))
        } else {
            Arc::from(
                harness_models::provider_from_config(model).map_err(|error| {
                    harness_core::Error::InvalidConfig {
                        reason: error.to_string(),
                    }
                })?,
            )
        };
        let policy: Arc<dyn Policy> = Arc::new(PolicyEngine::new(mode, &self.project_root));
        Ok(Arc::new(
            AgentRunner::new(
                provider,
                model.model.clone(),
                ToolRegistry::with_workspace_tools(),
                policy,
                Arc::clone(&self.sessions),
                ContextBuilder::default(),
                AgentLimits::default(),
                Arc::new(RpcApprovalHandler::new(Arc::clone(&self.approvals))),
            )
            .with_event_bus(self.event_bus.clone())
            .with_checkpoints(Arc::clone(&self.checkpoints)),
        ))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging("info")?;
    let mut arguments = std::env::args().skip(1);
    let workspace_argument = arguments.next().unwrap_or_else(|| ".".to_owned());
    let address_argument = arguments
        .next()
        .unwrap_or_else(|| "127.0.0.1:4545".to_owned());
    let session_root = PathBuf::from(workspace_argument.clone()).join(".cogito/sessions");
    let workspace_root = std::fs::canonicalize(&workspace_argument)?;
    let description = discover_workspace(&workspace_root)?;
    let project_root = description
        .repository_root
        .clone()
        .unwrap_or_else(|| description.current_directory.clone());
    let event_bus = EventBus::new();
    let sessions: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::with_event_bus(
        &session_root,
        event_bus.clone(),
    )?);
    let checkpoints: Arc<dyn CheckpointStore> = if GitClient::open(&project_root).is_ok() {
        Arc::new(ShadowCheckpointStore::with_event_bus(
            project_root.join(".cogito/checkpoints"),
            Some(event_bus.clone()),
        )?)
    } else {
        Arc::new(ShadowCheckpointStore::new(
            project_root.join(".cogito/checkpoints"),
        )?)
    };
    let approvals = Arc::new(ApprovalBroker::new());
    let provider = Arc::new(ScriptedMockProvider::new(
        "dev-mock",
        vec![
            tool_response("list_directory", json!({"path": "."})),
            tool_response(
                "write_file",
                json!({
                    "path": "cogito-rpc-dev-output.txt",
                    "content": "Generated by the development RPC runtime."
                }),
            ),
            final_response(),
        ],
    ));
    let policy: Arc<dyn Policy> = Arc::new(PolicyEngine::new(
        harness_policy::ExecutionMode::Normal,
        &project_root,
    ));
    let runner = AgentRunner::new(
        provider,
        "dev-mock",
        ToolRegistry::with_workspace_tools(),
        Arc::clone(&policy),
        Arc::clone(&sessions),
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(RpcApprovalHandler::new(Arc::clone(&approvals))),
    )
    .with_event_bus(event_bus.clone())
    .with_checkpoints(Arc::clone(&checkpoints));
    // Clone the shared state the rebuild factory needs before it is moved into
    // the runtime.
    let factory = Arc::new(DevRunnerFactory {
        provider_script: Arc::new(vec![
            tool_response("list_directory", json!({"path": "."})),
            tool_response(
                "write_file",
                json!({
                    "path": "cogito-rpc-dev-output.txt",
                    "content": "Generated by the development RPC runtime."
                }),
            ),
            final_response(),
        ]),
        project_root: project_root.clone(),
        sessions: Arc::clone(&sessions) as Arc<dyn SessionStore>,
        event_bus: event_bus.clone(),
        approvals: Arc::clone(&approvals),
        checkpoints: Arc::clone(&checkpoints),
    });
    let runtime = Arc::new(
        Runtime::new(
            Arc::new(UnavailableAgent),
            Vec::new(),
            ToolRegistry::with_workspace_tools(),
            policy,
            sessions,
            checkpoints,
            Vec::new(),
        )
        .with_agent_runner(Arc::new(runner))
        .with_runner_factory(factory)
        .with_workspace_root(workspace_root),
    );
    let address: SocketAddr = address_argument.parse()?;
    let server = RpcServer::bind_with_approvals(address, runtime, approvals)?;
    println!(
        "CogitoAI development RPC listening on {}",
        server.local_addr()?
    );
    server.serve()?;
    Ok(())
}
