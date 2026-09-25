use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use harness_agent::{AgentLimits, AgentRunner, AgentTask};
use harness_context::{ContextBuilder, WorkspaceMetadata};
use harness_core::{AgentRuntime, Error, RunOutcome, RunRequest, SessionId};
use harness_git::{CheckpointStore, ShadowCheckpointStore};
use harness_models::{
    ContentBlock, FinishReason, ModelCapabilities, ModelProvider, ModelRequest, ModelResponse,
    ProviderError, ScriptedMockProvider, StreamDelta, StreamDeltaKind, ToolCall, Usage,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_rpc::{
    ApprovalBroker, RpcApprovalHandler, RpcClient, RpcServer, Runtime, ServerMessage,
};
use harness_session::{EventBus, JsonlSessionStore, SessionStore};
use harness_tools::ToolRegistry;
use serde_json::{json, Value};
use tempfile::tempdir;

struct DummyAgent;

impl AgentRuntime for DummyAgent {
    fn run(&self, _request: RunRequest) -> Result<RunOutcome, Error> {
        Err(Error::InvalidRequest {
            reason: "the RPC agent runner is required".to_owned(),
        })
    }
}

fn response(tool: Option<(&str, Value)>, text: &str) -> ModelResponse {
    let has_tool = tool.is_some();
    ModelResponse {
        id: "rpc-mock".to_owned(),
        model: "rpc-mock".to_owned(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }]
        },
        tool_calls: tool
            .map(|(name, arguments)| ToolCall {
                id: format!("call-{name}"),
                name: name.to_owned(),
                arguments,
            })
            .into_iter()
            .collect(),
        finish_reason: if has_tool {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        },
        usage: Some(Usage::new(1, 1)),
    }
}

fn setup(
    root: &Path,
    mode: ExecutionMode,
    provider: Arc<dyn ModelProvider>,
    approvals: Arc<ApprovalBroker>,
) -> (Arc<Runtime>, Arc<JsonlSessionStore>) {
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .unwrap();
    let event_bus = EventBus::new();
    let sessions = Arc::new(
        JsonlSessionStore::with_event_bus(root.join("sessions"), event_bus.clone()).unwrap(),
    );
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(ShadowCheckpointStore::new(root.join(".cogito/checkpoints")).unwrap());
    let policy: Arc<dyn Policy> = Arc::new(PolicyEngine::new(mode, root));
    let runner = AgentRunner::new(
        provider,
        "rpc-mock",
        ToolRegistry::with_workspace_tools(),
        Arc::clone(&policy),
        Arc::clone(&sessions) as Arc<dyn SessionStore>,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(RpcApprovalHandler::new(approvals)),
    )
    .with_event_bus(event_bus)
    .with_checkpoints(Arc::clone(&checkpoints));
    let runtime = Arc::new(
        Runtime::new(
            Arc::new(DummyAgent),
            Vec::new(),
            ToolRegistry::with_workspace_tools(),
            policy,
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
            checkpoints,
            Vec::new(),
        )
        .with_agent_runner(Arc::new(runner))
        .with_workspace_root(root.to_path_buf()),
    );
    (runtime, sessions)
}

fn start_server(
    runtime: Arc<Runtime>,
    approvals: Arc<ApprovalBroker>,
) -> (
    RpcClient,
    std::net::SocketAddr,
    Arc<std::sync::atomic::AtomicBool>,
    thread::JoinHandle<()>,
) {
    let server =
        RpcServer::bind_with_approvals("127.0.0.1:0".parse().unwrap(), runtime, approvals).unwrap();
    let address = server.local_addr().unwrap();
    let shutdown = server.shutdown_token();
    let handle = thread::spawn(move || server.serve().unwrap());
    let client = RpcClient::connect(address).unwrap();
    (client, address, shutdown, handle)
}

fn task(root: &Path, session: Option<SessionId>) -> AgentTask {
    AgentTask {
        workspace_root: root.to_path_buf(),
        user_task: "Complete the RPC mock task".to_owned(),
        system_instructions: "Use tools safely.".to_owned(),
        workspace: WorkspaceMetadata::default(),
        resume_session: session,
        ..AgentTask::default()
    }
}

fn assert_ok(response: &harness_rpc::RpcResponse) {
    assert!(response.ok, "RPC error: {:?}", response.error);
}

fn wait_for<F>(mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    for _ in 0..100 {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn drives_a_complete_mock_session_through_rpc() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let approvals = Arc::new(ApprovalBroker::new());
    let provider = Arc::new(ScriptedMockProvider::new(
        "rpc-mock",
        vec![
            response(
                Some((
                    "write_file",
                    json!({"path": "rpc-output.txt", "content": "created by RPC"}),
                )),
                "",
            ),
            response(None, "RPC mock task complete"),
        ],
    ));
    let (runtime, sessions) = setup(
        root,
        ExecutionMode::Normal,
        provider,
        Arc::clone(&approvals),
    );
    let (mut client, _address, shutdown, server) = start_server(runtime, approvals);

    assert_ok(&client.request("rpc.initialize", json!({})).unwrap());
    let opened = client.request("workspace.open", json!({})).unwrap();
    assert_ok(&opened);
    assert_eq!(
        opened.result.unwrap()["current_directory"],
        std::fs::canonicalize(root)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    let updated = client
        .request(
            "config.update",
            json!({"package_manager": "npm", "test": ["npm", "test"]}),
        )
        .unwrap();
    assert_ok(&updated);
    assert_eq!(
        updated.result.unwrap()["commands"]["test"][0]["program"],
        "npm"
    );
    let config = client.request("config.inspect", json!({})).unwrap();
    assert_ok(&config);
    assert_eq!(
        config.result.unwrap()["commands"]["test"][0]["args"][0],
        "test"
    );
    let created = client.request("session.create", json!({})).unwrap();
    assert_ok(&created);
    let session_id = SessionId::new(
        created.result.unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
    .unwrap();
    let accepted = client
        .request(
            "agent.send",
            json!({"task": task(root, Some(session_id.clone()))}),
        )
        .unwrap();
    assert_ok(&accepted);
    let run_id = accepted.result.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut completed = false;
    let mut events = 0;
    for _ in 0..200 {
        match client.receive().unwrap() {
            ServerMessage::Notification(notification) if notification.method == "agent.event" => {
                events += 1;
            }
            ServerMessage::Notification(notification)
                if notification.method == "agent.completed" =>
            {
                assert_eq!(notification.params["run_id"], run_id);
                completed = true;
                break;
            }
            ServerMessage::Notification(_) | ServerMessage::Response(_) => {}
        }
    }
    assert!(completed);
    assert!(events > 0);
    assert_eq!(
        std::fs::read_to_string(root.join("rpc-output.txt")).unwrap(),
        "created by RPC"
    );
    let inspected = client
        .request(
            "session.inspect",
            json!({"session_id": session_id.to_string()}),
        )
        .unwrap();
    assert_ok(&inspected);
    assert!(inspected.result.unwrap()["session"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["event_type"] == "session.completed"));

    drop(client);
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
    assert_eq!(
        sessions
            .load(&session_id)
            .unwrap()
            .state()
            .unwrap()
            .context_compactions,
        0
    );
}

#[test]
fn approval_requests_are_resolved_by_the_client() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let approvals = Arc::new(ApprovalBroker::new());
    let provider = Arc::new(ScriptedMockProvider::new(
        "rpc-mock",
        vec![
            response(
                Some((
                    "write_file",
                    json!({"path": "approved.txt", "content": "approved"}),
                )),
                "",
            ),
            response(None, "approved task complete"),
        ],
    ));
    let (runtime, _sessions) = setup(root, ExecutionMode::Safe, provider, Arc::clone(&approvals));
    let (mut client, _address, shutdown, server) = start_server(runtime, approvals);

    let created = client.request("session.create", json!({})).unwrap();
    assert_ok(&created);
    let session_id = SessionId::new(
        created.result.unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
    .unwrap();
    let accepted = client
        .request("agent.send", json!({"task": task(root, Some(session_id))}))
        .unwrap();
    assert_ok(&accepted);
    let run_id = accepted.result.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut approval_id = None;
    for _ in 0..100 {
        if let ServerMessage::Notification(notification) = client.receive().unwrap() {
            if notification.method == "approval.request" {
                approval_id = Some(
                    notification.params["approval_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
                break;
            }
        }
    }
    let approval_id = approval_id.expect("approval request was not streamed");
    let approved = client
        .request("agent.approve", json!({"approval_id": approval_id}))
        .unwrap();
    assert_ok(&approved);

    let mut completed = false;
    for _ in 0..200 {
        if let ServerMessage::Notification(notification) = client.receive().unwrap() {
            if notification.method == "agent.completed" {
                assert_eq!(notification.params["run_id"], run_id);
                completed = true;
                break;
            }
        }
    }
    assert!(completed);
    assert!(root.join("approved.txt").is_file());
    drop(client);
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn dropped_client_cancels_active_run_and_allows_reconnect() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let approvals = Arc::new(ApprovalBroker::new());
    let (runtime, sessions) = setup(
        root,
        ExecutionMode::Normal,
        Arc::new(SlowProvider),
        approvals.clone(),
    );
    let (mut client, address, shutdown, server) = start_server(runtime, Arc::clone(&approvals));
    let created = client.request("session.create", json!({})).unwrap();
    assert_ok(&created);
    let session_id = SessionId::new(
        created.result.unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
    .unwrap();
    let accepted = client
        .request(
            "agent.send",
            json!({"task": task(root, Some(session_id.clone()))}),
        )
        .unwrap();
    assert_ok(&accepted);
    let run_id = accepted.result.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let cancelled = client
        .request("agent.cancel", json!({"run_id": run_id}))
        .unwrap();
    assert_ok(&cancelled);
    let mut failed = false;
    for _ in 0..200 {
        if let ServerMessage::Notification(notification) = client.receive().unwrap() {
            if notification.method == "agent.failed" {
                assert_eq!(notification.params["error"]["code"], "cancelled");
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    drop(client);

    assert!(wait_for(|| {
        sessions
            .load(&session_id)
            .map(|session| session.state().is_ok())
            .unwrap_or(false)
    }));
    let mut reconnected = RpcClient::connect(address).unwrap();
    let listed = reconnected.request("session.list", json!({})).unwrap();
    assert_ok(&listed);
    assert!(listed
        .result
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|session| { session["id"] == session_id.to_string() }));
    drop(reconnected);
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = server.join();
}

struct SlowProvider;

impl ModelProvider for SlowProvider {
    fn name(&self) -> &str {
        "slow"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calling: false,
            vision: false,
            reasoning: false,
            context_window: Some(1024),
        }
    }

    fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.slow_response()
    }

    fn stream(
        &self,
        _request: &ModelRequest,
        on_delta: &mut dyn FnMut(StreamDelta) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        for _ in 0..100 {
            on_delta(StreamDelta {
                sequence: 0,
                delta: StreamDeltaKind::Text {
                    text: "working".to_owned(),
                },
                finish_reason: None,
                usage: None,
            })?;
            thread::sleep(Duration::from_millis(10));
        }
        self.slow_response()
    }
}

impl SlowProvider {
    fn slow_response(&self) -> Result<ModelResponse, ProviderError> {
        Ok(ModelResponse {
            id: "slow".to_owned(),
            model: "slow".to_owned(),
            content: vec![ContentBlock::Text {
                text: "done".to_owned(),
            }],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Some(Usage::new(1, 1)),
        })
    }
}
