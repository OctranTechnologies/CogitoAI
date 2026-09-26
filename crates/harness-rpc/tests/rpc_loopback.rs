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
fn exposes_checkpoints_file_views_and_diffs_and_restores_through_the_runtime() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    // Commit a baseline file so `HEAD` exists and diffs have an "original" side.
    // `setup` also runs `git init`, but the baseline commit must happen first.
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .unwrap();
    std::fs::write(root.join("baseline.txt"), "original\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=harness@example.invalid",
            "-c",
            "user.name=Harness",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ])
        .current_dir(root)
        .status()
        .unwrap();

    let approvals = Arc::new(ApprovalBroker::new());
    let provider = Arc::new(ScriptedMockProvider::new(
        "rpc-mock",
        vec![
            response(
                Some((
                    "write_file",
                    json!({"path": "baseline.txt", "content": "agent edit\n"}),
                )),
                "",
            ),
            response(
                Some((
                    "write_file",
                    json!({"path": "created.txt", "content": "new file\n"}),
                )),
                "",
            ),
            response(None, "edits complete"),
        ],
    ));
    let (runtime, _sessions) = setup(
        root,
        ExecutionMode::Normal,
        provider,
        Arc::clone(&approvals),
    );
    let (mut client, _address, shutdown, server) = start_server(runtime, approvals);

    assert_ok(&client.request("rpc.initialize", json!({})).unwrap());
    assert_ok(&client.request("workspace.open", json!({})).unwrap());
    let created = client.request("session.create", json!({})).unwrap();
    assert_ok(&created);
    let session_id = created.result.unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let accepted = client
        .request(
            "agent.send",
            json!({"task": task(root, Some(SessionId::new(session_id.clone()).unwrap()))}),
        )
        .unwrap();
    assert_ok(&accepted);
    let mut completed = false;
    for _ in 0..300 {
        match client.receive().unwrap() {
            ServerMessage::Notification(notification)
                if notification.method == "agent.completed" =>
            {
                completed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(completed, "mock session did not complete");

    // The runtime reports the worktree change.
    let status = client.request("git.status", json!({})).unwrap();
    assert_ok(&status);
    let changed = status.result.unwrap()["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(changed.contains(&"baseline.txt".to_owned()));
    assert!(changed.contains(&"created.txt".to_owned()));
    // The runtime's own session/checkpoint files live inside the repository but
    // must never be reported as user code changes.
    assert!(
        !changed.iter().any(|path| path.starts_with(".cogito/")),
        "runtime state leaked into changed_files: {changed:?}"
    );

    // An unrelated user edit that the agent never touched.
    std::fs::write(root.join("unrelated.txt"), "user work\n").unwrap();

    // A checkpoint was recorded for the run, and can be listed and inspected.
    let checkpoints = client.request("checkpoint.list", json!({})).unwrap();
    assert_ok(&checkpoints);
    let listed = checkpoints.result.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let checkpoint_id = listed[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(listed[0]["session_id"], session_id);
    let mut affected = listed[0]["recorded_changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    affected.sort();
    assert_eq!(affected, vec!["baseline.txt", "created.txt"]);

    let inspected = client
        .request(
            "checkpoint.inspect",
            json!({"checkpoint_id": checkpoint_id}),
        )
        .unwrap();
    assert_ok(&inspected);

    // Read-only file view.
    let read = client
        .request("file.read", json!({"path": "baseline.txt"}))
        .unwrap();
    assert_ok(&read);
    let read = read.result.unwrap();
    assert_eq!(read["content"], "agent edit\n");
    assert_eq!(read["language"], "plaintext");

    // Per-file diff exposes both sides plus line counts for Monaco.
    let modified = client
        .request("git.file_diff", json!({"path": "baseline.txt"}))
        .unwrap();
    assert_ok(&modified);
    let modified = modified.result.unwrap();
    assert_eq!(modified["kind"], "modified");
    assert_eq!(modified["original"], "original\n");
    assert_eq!(modified["modified"], "agent edit\n");
    assert_eq!(modified["additions"], 1);
    assert_eq!(modified["deletions"], 1);

    let added = client
        .request("git.file_diff", json!({"path": "created.txt"}))
        .unwrap();
    assert_ok(&added);
    let added = added.result.unwrap();
    assert_eq!(added["kind"], "added");
    assert_eq!(added["original"], "");
    assert_eq!(added["modified"], "new file\n");
    assert_eq!(added["additions"], 1);

    // Path traversal is rejected at the runtime boundary.
    let escape = client
        .request("file.read", json!({"path": "../outside.txt"}))
        .unwrap();
    assert!(!escape.ok, "traversal must be rejected");

    // Aggregate diff across the workspace.
    let aggregate = client.request("git.diff", json!({})).unwrap();
    assert_ok(&aggregate);
    assert!(aggregate.result.unwrap()["unstaged"]
        .as_str()
        .unwrap()
        .contains("baseline.txt"));

    // Restore goes through the runtime's own safety logic.
    let restored = client
        .request("checkpoint.undo", json!({"checkpoint_id": checkpoint_id}))
        .unwrap();
    assert_ok(&restored);
    let restored = restored.result.unwrap();
    let mut restored_files = restored["restored_files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(|path| path.replace('\\', "/"))
        .map(|path| path.rsplit('/').next().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    restored_files.sort();
    assert_eq!(restored_files, vec!["baseline.txt", "created.txt"]);

    // The agent's edits were reverted, and unrelated user work survived.
    assert_eq!(
        std::fs::read_to_string(root.join("baseline.txt")).unwrap(),
        "original\n"
    );
    assert!(!root.join("created.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("unrelated.txt")).unwrap(),
        "user work\n"
    );

    drop(client);
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn human_terminals_are_separate_from_agent_command_execution() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let approvals = Arc::new(ApprovalBroker::new());
    let provider = Arc::new(ScriptedMockProvider::new("rpc-mock", Vec::new()));
    let (runtime, _sessions) = setup(
        root,
        ExecutionMode::Normal,
        provider,
        Arc::clone(&approvals),
    );
    let (mut client, _address, shutdown, server) = start_server(runtime, approvals);

    assert_ok(&client.request("rpc.initialize", json!({})).unwrap());
    let initialize = client.request("rpc.initialize", json!({})).unwrap();
    let advertised = initialize.result.unwrap();
    let methods: Vec<String> = advertised["methods"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    for method in [
        "terminal.open",
        "terminal.write",
        "terminal.resize",
        "terminal.close",
        "terminal.list",
    ] {
        assert!(
            methods.iter().any(|name| name == method),
            "runtime does not advertise {method}"
        );
    }

    assert_ok(&client.request("workspace.open", json!({})).unwrap());

    // A non-human origin is refused: the agent path cannot open a terminal, so
    // an agent-injected call fails loudly instead of gaining a free shell.
    let agent_origin = client
        .request("terminal.open", json!({"origin": "agent"}))
        .unwrap();
    assert!(!agent_origin.ok, "agent origin must be rejected");
    assert!(
        agent_origin
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("human")),
        "unexpected error: {:?}",
        agent_origin.error
    );
    let missing_origin = client.request("terminal.open", json!({})).unwrap();
    assert!(!missing_origin.ok, "a missing origin must be rejected");

    // A human terminal opens confined to the workspace.
    //
    // An explicit shell is used because the default PowerShell line editor waits
    // for cursor-position replies from a terminal emulator before it runs
    // anything; the desktop satisfies that with xterm.js, but a raw RPC test
    // client is not a terminal.
    let opened = client
        .request(
            "terminal.open",
            json!({"origin": "human", "program": test_shell(), "cols": 100, "rows": 30}),
        )
        .unwrap();
    assert_ok(&opened);
    let opened = opened.result.unwrap();
    assert_eq!(opened["origin"], "human");
    assert_eq!(opened["cols"], 100);
    assert_eq!(opened["rows"], 30);
    let terminal_id = opened["id"].as_str().unwrap().to_owned();
    let pid = opened["pid"].as_u64().expect("pid");

    let listed = client.request("terminal.list", json!({})).unwrap();
    assert_ok(&listed);
    assert_eq!(listed.result.unwrap().as_array().unwrap().len(), 1);

    // Output is streamed as a notification, not returned by a request.
    client
        .request(
            "terminal.write",
            json!({"terminal_id": terminal_id, "data": "echo rpc-terminal-marker\r\n"}),
        )
        .unwrap();
    // Windows console processes query the terminal for the cursor position and
    // block until something answers, which the desktop does through xterm.js.
    // This test client answers so the shell proceeds.
    let mut saw_output = false;
    let mut unanswered = 0_usize;
    for _ in 0..400 {
        match client.receive().unwrap() {
            ServerMessage::Notification(notification)
                if notification.method == "terminal.output"
                    && notification.params["terminal_id"] == terminal_id =>
            {
                let data = notification.params["data"].as_str().unwrap_or_default();
                let queries = data.matches("\u{1b}[6n").count();
                for _ in 0..queries {
                    unanswered += 1;
                    client
                        .request(
                            "terminal.write",
                            json!({
                                "terminal_id": terminal_id,
                                "data": "\u{1b}[1;1R",
                            }),
                        )
                        .unwrap();
                }
                if data.contains("rpc-terminal-marker") {
                    saw_output = true;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        saw_output,
        "expected streamed terminal.output carrying the marker (answered {unanswered} cursor queries)"
    );

    // Resizing is reported back through the runtime.
    let resized = client
        .request(
            "terminal.resize",
            json!({"terminal_id": terminal_id, "cols": 120, "rows": 40}),
        )
        .unwrap();
    assert_ok(&resized);
    assert_eq!(resized.result.unwrap()["cols"], 120);

    // Closing terminates the process and removes the session.
    let closed = client
        .request("terminal.close", json!({"terminal_id": terminal_id}))
        .unwrap();
    assert_ok(&closed);
    assert!(
        wait_longer(|| !process_is_alive(pid as u32)),
        "terminal process survived close"
    );
    let listed = client.request("terminal.list", json!({})).unwrap();
    assert!(listed.result.unwrap().as_array().unwrap().is_empty());

    let unknown = client
        .request(
            "terminal.write",
            json!({"terminal_id": "pty-missing", "data": "echo hi"}),
        )
        .unwrap();
    assert!(!unknown.ok, "writing to an unknown terminal must fail");

    drop(client);
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn a_disconnecting_client_does_not_leak_its_terminal_processes() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let approvals = Arc::new(ApprovalBroker::new());
    let provider = Arc::new(ScriptedMockProvider::new("rpc-mock", Vec::new()));
    let (runtime, _sessions) = setup(
        root,
        ExecutionMode::Normal,
        provider,
        Arc::clone(&approvals),
    );
    let runtime = Arc::clone(&runtime);
    let (mut client, _address, shutdown, server) = start_server(runtime, approvals);

    assert_ok(&client.request("workspace.open", json!({})).unwrap());
    let opened = client
        .request(
            "terminal.open",
            json!({"origin": "human", "program": test_shell()}),
        )
        .unwrap();
    assert_ok(&opened);
    let opened = opened.result.unwrap();
    let pid = opened["pid"].as_u64().expect("pid") as u32;
    assert!(process_is_alive(pid), "terminal did not start");

    // Closing the desktop window drops the connection without a close request.
    drop(client);
    assert!(
        wait_longer(|| !process_is_alive(pid)),
        "terminal process leaked after the client disconnected"
    );

    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().unwrap();
}

/// Shell used by the terminal RPC tests.
///
/// The Windows default (PowerShell) blocks on a cursor-position query until a
/// terminal emulator answers, which a raw RPC client does not do.
fn test_shell() -> &'static str {
    if cfg!(windows) {
        "cmd.exe"
    } else {
        "/bin/sh"
    }
}

/// Waits up to ten seconds, for process teardown which can lag the request.
fn wait_longer<F>(mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    for _ in 0..500 {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match output {
        Ok(output) => !String::from_utf8_lossy(&output.stdout).contains("INFO: No tasks"),
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn process_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
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
