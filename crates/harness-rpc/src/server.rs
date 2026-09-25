use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use harness_agent::{AgentError, AgentTask, ApprovalHandler};
use harness_core::{discover_workspace, CheckpointId, RunId, SessionId};
use harness_git::{GitClient, GitError};
use harness_session::{EventId, EventSubscriber, EventSubscription, HarnessEvent};
use harness_tools::{CancellationToken, ToolRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

use crate::client::RpcClientError;
use crate::protocol::{
    RpcError, RpcNotification, RpcRequest, RpcResponse, ServerMessage, METHODS,
    RPC_PROTOCOL_VERSION,
};
use crate::Runtime;

const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RpcServerError {
    #[error("RPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("RPC JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("RPC client error: {0}")]
    Client(#[from] RpcClientError),
    #[error("core error: {0}")]
    Core(#[from] harness_core::Error),
    #[error("Git error: {0}")]
    Git(#[from] GitError),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("agent runtime is not configured")]
    AgentUnavailable,
}

pub struct RpcServer {
    listener: TcpListener,
    runtime: Arc<Runtime>,
    state: Arc<ServerState>,
    shutdown: Arc<AtomicBool>,
    _event_subscription: Option<EventSubscription>,
}

impl RpcServer {
    pub fn bind(address: SocketAddr, runtime: Arc<Runtime>) -> Result<Self, RpcServerError> {
        Self::bind_with_approvals(address, runtime, Arc::new(ApprovalBroker::new()))
    }

    pub fn bind_with_approvals(
        address: SocketAddr,
        runtime: Arc<Runtime>,
        approvals: Arc<ApprovalBroker>,
    ) -> Result<Self, RpcServerError> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let state = Arc::new(ServerState::new(approvals));
        let event_subscription = runtime.agent_event_bus().map(|bus| {
            let state = Arc::clone(&state);
            bus.subscribe(Arc::new(EventForwarder { state }))
        });
        Ok(Self {
            listener,
            runtime,
            state,
            shutdown: Arc::new(AtomicBool::new(false)),
            _event_subscription: event_subscription,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, RpcServerError> {
        Ok(self.listener.local_addr()?)
    }

    pub fn shutdown_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    pub fn serve(self) -> Result<(), RpcServerError> {
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    let state = Arc::clone(&self.state);
                    let runtime = Arc::clone(&self.runtime);
                    thread::spawn(move || {
                        let _ = serve_connection(stream, state, runtime);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

struct EventForwarder {
    state: Arc<ServerState>,
}

impl EventSubscriber for EventForwarder {
    fn on_event(&self, event: &HarnessEvent) {
        self.state.on_event(event);
    }
}

struct ServerState {
    clients: Mutex<HashMap<u64, Sender<ServerMessage>>>,
    next_client: AtomicU64,
    active_run: Mutex<Option<ActiveRun>>,
    next_run: AtomicU64,
    seen_events: Mutex<Vec<EventId>>,
    approvals: Arc<ApprovalBroker>,
}

struct ActiveRun {
    id: RunId,
    cancellation: CancellationToken,
}

impl ServerState {
    fn new(approvals: Arc<ApprovalBroker>) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(0),
            active_run: Mutex::new(None),
            next_run: AtomicU64::new(0),
            seen_events: Mutex::new(Vec::new()),
            approvals,
        }
    }

    fn register(&self, sender: Sender<ServerMessage>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.clients
            .lock()
            .expect("RPC client lock poisoned")
            .insert(id, sender.clone());
        self.approvals.set_sender(id, sender);
        id
    }

    fn unregister(&self, id: u64) {
        self.clients
            .lock()
            .expect("RPC client lock poisoned")
            .remove(&id);
        self.approvals.clear_sender(id);
        self.approvals.deny_all();
        if let Some(active) = self
            .active_run
            .lock()
            .expect("RPC run lock poisoned")
            .as_ref()
        {
            active.cancellation.cancel();
        }
    }

    fn send(&self, id: u64, message: ServerMessage) {
        if let Some(sender) = self
            .clients
            .lock()
            .expect("RPC client lock poisoned")
            .get(&id)
        {
            let _ = sender.send(message);
        }
    }

    fn broadcast(&self, message: ServerMessage) {
        let senders = self
            .clients
            .lock()
            .expect("RPC client lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for sender in senders {
            let _ = sender.send(message.clone());
        }
    }

    fn begin_run(&self, cancellation: CancellationToken) -> Result<RunId, RpcServerError> {
        let mut active = self.active_run.lock().expect("RPC run lock poisoned");
        if active.is_some() {
            return Err(RpcServerError::Runtime("run_in_progress".to_owned()));
        }
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let run_id = RunId::new(format!("run-{}-{}", std::process::id(), id))
            .map_err(|error| RpcServerError::Runtime(error.to_string()))?;
        *active = Some(ActiveRun {
            id: run_id.clone(),
            cancellation,
        });
        Ok(run_id)
    }

    fn finish_run(&self, run_id: &RunId) {
        let mut active = self.active_run.lock().expect("RPC run lock poisoned");
        if active.as_ref().is_some_and(|active| &active.id == run_id) {
            *active = None;
        }
    }

    fn cancel_run(&self, run_id: &RunId) -> bool {
        let active = self.active_run.lock().expect("RPC run lock poisoned");
        if let Some(active) = active.as_ref().filter(|active| &active.id == run_id) {
            active.cancellation.cancel();
            true
        } else {
            false
        }
    }

    fn current_run_id(&self) -> Option<RunId> {
        self.active_run
            .lock()
            .expect("RPC run lock poisoned")
            .as_ref()
            .map(|active| active.id.clone())
    }

    fn on_event(&self, event: &HarnessEvent) {
        let mut seen = self.seen_events.lock().expect("RPC event lock poisoned");
        if seen.contains(&event.event_id) {
            return;
        }
        seen.push(event.event_id.clone());
        if seen.len() > 4096 {
            seen.remove(0);
        }
        drop(seen);
        self.broadcast(ServerMessage::Notification(RpcNotification {
            version: RPC_PROTOCOL_VERSION,
            method: "agent.event".to_owned(),
            params: json!({
                "run_id": self.current_run_id(),
                "event": event,
            }),
        }));
    }
}

pub struct ApprovalBroker {
    pending: Mutex<HashMap<String, SyncSender<bool>>>,
    next_id: AtomicU64,
    sender: Mutex<Option<(u64, Sender<ServerMessage>)>>,
}

impl Default for ApprovalBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            sender: Mutex::new(None),
        }
    }

    fn set_sender(&self, client: u64, sender: Sender<ServerMessage>) {
        *self.sender.lock().expect("approval sender lock poisoned") = Some((client, sender));
    }

    fn clear_sender(&self, client: u64) {
        let mut sender = self.sender.lock().expect("approval sender lock poisoned");
        if sender.as_ref().is_some_and(|(id, _)| *id == client) {
            *sender = None;
        }
    }

    fn request(&self, request: &ToolRequest) -> Result<bool, AgentError> {
        let approval_id = format!(
            "approval-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.pending
            .lock()
            .expect("approval pending lock poisoned")
            .insert(approval_id.clone(), sender);
        let notification = RpcNotification {
            version: RPC_PROTOCOL_VERSION,
            method: "approval.request".to_owned(),
            params: json!({
                "approval_id": approval_id,
                "tool": request,
            }),
        };
        let client_sender = self
            .sender
            .lock()
            .expect("approval sender lock poisoned")
            .as_ref()
            .map(|(_, sender)| sender.clone());
        if let Some(client_sender) = client_sender {
            if client_sender
                .send(ServerMessage::Notification(notification))
                .is_err()
            {
                self.remove(&approval_id);
                return Ok(false);
            }
        } else {
            self.remove(&approval_id);
            return Ok(false);
        }
        let result = receiver.recv().unwrap_or(false);
        self.remove(&approval_id);
        Ok(result)
    }

    fn resolve(&self, approval_id: &str, approved: bool) -> bool {
        let sender = self
            .pending
            .lock()
            .expect("approval pending lock poisoned")
            .remove(approval_id);
        sender.is_some_and(|sender| sender.try_send(approved).is_ok())
    }

    fn deny_all(&self) {
        let pending =
            std::mem::take(&mut *self.pending.lock().expect("approval pending lock poisoned"));
        for (_, sender) in pending {
            let _ = sender.try_send(false);
        }
    }

    fn remove(&self, approval_id: &str) {
        self.pending
            .lock()
            .expect("approval pending lock poisoned")
            .remove(approval_id);
    }
}

pub struct RpcApprovalHandler {
    broker: Arc<ApprovalBroker>,
}

impl RpcApprovalHandler {
    pub fn new(broker: Arc<ApprovalBroker>) -> Self {
        Self { broker }
    }
}

impl ApprovalHandler for RpcApprovalHandler {
    fn request(&self, request: &ToolRequest) -> Result<bool, AgentError> {
        self.broker.request(request)
    }
}

#[derive(Debug, Deserialize)]
struct AgentRunParams {
    task: AgentTask,
}

#[derive(Debug, Deserialize, Default)]
struct ConfigUpdate {
    package_manager: Option<String>,
    test: Option<Vec<String>>,
    build: Option<Vec<String>>,
    format: Option<Vec<String>>,
    lint: Option<Vec<String>>,
    typecheck: Option<Vec<String>>,
}

fn serve_connection(
    stream: TcpStream,
    state: Arc<ServerState>,
    runtime: Arc<Runtime>,
) -> Result<(), RpcServerError> {
    let writer = stream.try_clone()?;
    let (sender, receiver) = mpsc::channel();
    let client_id = state.register(sender);
    let writer_state = Arc::clone(&state);
    let writer_client = client_id;
    let writer_thread =
        thread::spawn(move || write_messages(writer, receiver, writer_state, writer_client));
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            state.send(
                client_id,
                ServerMessage::Response(error_response(
                    None,
                    "message_too_large",
                    "RPC line exceeds the maximum size",
                )),
            );
            break;
        }
        match serde_json::from_str::<RpcRequest>(&line) {
            Ok(request) => {
                let response = dispatch(request, &state, &runtime);
                state.send(client_id, ServerMessage::Response(response));
            }
            Err(error) => state.send(
                client_id,
                ServerMessage::Response(error_response(
                    None,
                    "malformed_request",
                    error.to_string(),
                )),
            ),
        }
    }
    state.unregister(client_id);
    drop(state);
    let _ = writer_thread.join();
    Ok(())
}

fn write_messages(
    mut stream: TcpStream,
    receiver: Receiver<ServerMessage>,
    state: Arc<ServerState>,
    client_id: u64,
) {
    for message in receiver {
        if serde_json::to_writer(&mut stream, &message).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            break;
        }
    }
    state.unregister(client_id);
}

fn dispatch(request: RpcRequest, state: &Arc<ServerState>, runtime: &Arc<Runtime>) -> RpcResponse {
    if request.version != RPC_PROTOCOL_VERSION {
        return error_response(
            request.id,
            "unsupported_version",
            format!("unsupported RPC protocol version {}", request.version),
        );
    }
    if request.id.is_none() {
        return error_response(request.id, "missing_id", "RPC requests require an id");
    }
    let result = match request.method.as_str() {
        "rpc.initialize" => Ok(json!({
            "version": RPC_PROTOCOL_VERSION,
            "server": "cogito-harness",
            "methods": METHODS,
        })),
        "workspace.open" | "workspace.inspect" => workspace(runtime, &request.params),
        "config.inspect" => config_inspect(runtime, &request.params),
        "config.update" => config_update(runtime, &request.params),
        "session.create" => session_create(runtime, &request.params),
        "session.list" => session_list(runtime, &request.params),
        "session.inspect" => session_inspect(runtime, &request.params),
        "session.state" => session_state(runtime, &request.params),
        "session.resume" => session_resume(runtime, &request.params),
        "agent.send" | "agent.run" => return start_run(request, state, runtime),
        "agent.approve" => approval(request.clone(), state, true),
        "agent.deny" => approval(request.clone(), state, false),
        "agent.cancel" => cancel(request.clone(), state),
        "git.status" => git_status(runtime, &request.params),
        "git.diff" => git_diff(runtime, &request.params),
        "checkpoint.list" => checkpoint_list(runtime),
        "checkpoint.inspect" => checkpoint_inspect(runtime, &request.params),
        "checkpoint.undo" => checkpoint_undo(runtime, &request.params),
        _ => Err(RpcServerError::Runtime(format!(
            "unknown method {}",
            request.method
        ))),
    };
    match result {
        Ok(value) => ok_response(request.id, value),
        Err(error) => error_response(request.id, error_code(&error), error.to_string()),
    }
}

fn workspace(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let root = workspace_path(runtime, params)?;
    Ok(serde_json::to_value(discover_workspace(&root)?)?)
}

fn config_inspect(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let root = workspace_path(runtime, params)?;
    Ok(serde_json::to_value(
        discover_workspace(&root)?.configuration,
    )?)
}

fn config_update(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let root = workspace_path(runtime, &json!({}))?;
    let update: ConfigUpdate = serde_json::from_value(params.clone())?;
    let config_path = root.join(".agent/config.toml");
    let mut table = if config_path.is_file() {
        std::fs::read_to_string(&config_path)?
            .parse::<toml::Table>()
            .map_err(|error| RpcServerError::Runtime(error.to_string()))?
    } else {
        toml::Table::new()
    };
    if let Some(package_manager) = update.package_manager {
        validate_package_manager(&package_manager)?;
        table.insert(
            "package_manager".to_owned(),
            toml::Value::String(package_manager),
        );
    }
    let mut commands = table
        .get("commands")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    for (name, value) in [
        ("test", update.test),
        ("build", update.build),
        ("format", update.format),
        ("lint", update.lint),
        ("typecheck", update.typecheck),
    ] {
        if let Some(value) = value {
            validate_command(name, &value)?;
            commands.insert(
                name.to_owned(),
                toml::Value::Array(value.into_iter().map(toml::Value::String).collect()),
            );
        }
    }
    table.insert("commands".to_owned(), toml::Value::Table(commands));
    std::fs::create_dir_all(root.join(".agent"))?;
    std::fs::write(
        &config_path,
        toml::to_string_pretty(&table).map_err(|error| {
            RpcServerError::Runtime(format!("could not serialize configuration: {error}"))
        })?,
    )?;
    Ok(serde_json::to_value(
        discover_workspace(&root)?.configuration,
    )?)
}

fn session_create(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let root = workspace_path(runtime, params)?;
    let session = runtime.create_session(&root)?;
    Ok(json!({"session": session}))
}

fn session_list(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .min(1000) as usize;
    Ok(serde_json::to_value(runtime.recent_sessions(limit)?)?)
}

fn session_inspect(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let id = session_id(params)?;
    Ok(serde_json::to_value(runtime.inspect_session(&id)?)?)
}

fn session_state(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let id = session_id(params)?;
    Ok(serde_json::to_value(runtime.session_state(&id)?)?)
}

fn session_resume(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let id = session_id(params)?;
    Ok(serde_json::to_value(runtime.resume_session(&id)?)?)
}

fn start_run(request: RpcRequest, state: &Arc<ServerState>, runtime: &Arc<Runtime>) -> RpcResponse {
    let params: AgentRunParams = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                request.id,
                "invalid_params",
                format!("invalid agent task: {error}"),
            )
        }
    };
    let root = match runtime.workspace_root() {
        Some(root) => root.to_path_buf(),
        None => {
            return error_response(
                request.id,
                "workspace_not_open",
                "the server has no workspace root",
            )
        }
    };
    if params.task.workspace_root != root {
        return error_response(
            request.id,
            "workspace_mismatch",
            "the task workspace does not match the open workspace",
        );
    }
    let cancellation = CancellationToken::new();
    let run_id = match state.begin_run(cancellation.clone()) {
        Ok(run_id) => run_id,
        Err(error) => return error_response(request.id, "run_in_progress", error.to_string()),
    };
    let state_for_run = Arc::clone(state);
    let runtime_for_run = Arc::clone(runtime);
    let run_for_worker = run_id.clone();
    let task = params.task;
    thread::spawn(move || {
        let result = runtime_for_run.run_agent(&task, &cancellation);
        state_for_run.finish_run(&run_for_worker);
        let notification = match result {
            Ok(outcome) => RpcNotification {
                version: RPC_PROTOCOL_VERSION,
                method: "agent.completed".to_owned(),
                params: json!({"run_id": run_for_worker, "outcome": outcome}),
            },
            Err(error) => RpcNotification {
                version: RPC_PROTOCOL_VERSION,
                method: "agent.failed".to_owned(),
                params: json!({
                    "run_id": run_for_worker,
                    "error": {"code": error_code_agent(&error), "message": error.to_string()},
                }),
            },
        };
        state_for_run.broadcast(ServerMessage::Notification(notification));
    });
    ok_response(request.id, json!({"run_id": run_id}))
}

fn approval(
    request: RpcRequest,
    state: &Arc<ServerState>,
    approved: bool,
) -> Result<Value, RpcServerError> {
    let id = request
        .params
        .get("approval_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing approval_id".to_owned()))?;
    let resolved = state.approvals.resolve(id, approved);
    if !resolved {
        return Err(RpcServerError::Runtime("approval_not_found".to_owned()));
    }
    Ok(json!({"resolved": true}))
}

fn cancel(request: RpcRequest, state: &Arc<ServerState>) -> Result<Value, RpcServerError> {
    let run_id = request
        .params
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing run_id".to_owned()))?;
    let run_id = RunId::new(run_id.to_owned())
        .map_err(|error| RpcServerError::Runtime(error.to_string()))?;
    if !state.cancel_run(&run_id) {
        return Err(RpcServerError::Runtime("run_not_active".to_owned()));
    }
    Ok(json!({"cancelled": true}))
}

fn git_status(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let client = GitClient::open(&workspace_path(runtime, params)?)?;
    Ok(serde_json::to_value(client.status()?)?)
}

fn git_diff(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let client = GitClient::open(&workspace_path(runtime, params)?)?;
    let file = params.get("file").and_then(Value::as_str);
    let diff = file.map_or_else(
        || client.diff(),
        |file| client.diff_file(PathBuf::from(file).as_path()),
    )?;
    Ok(serde_json::to_value(diff)?)
}

fn checkpoint_list(runtime: &Runtime) -> Result<Value, RpcServerError> {
    Ok(serde_json::to_value(runtime.list_checkpoints()?)?)
}

fn checkpoint_inspect(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let id = params
        .get("checkpoint_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing checkpoint_id".to_owned()))?;
    let id = CheckpointId::new(id.to_owned())
        .map_err(|error| RpcServerError::Runtime(error.to_string()))?;
    Ok(serde_json::to_value(runtime.inspect_checkpoint(&id)?)?)
}

fn checkpoint_undo(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let id = params
        .get("checkpoint_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing checkpoint_id".to_owned()))?;
    let id = CheckpointId::new(id.to_owned())
        .map_err(|error| RpcServerError::Runtime(error.to_string()))?;
    Ok(serde_json::to_value(runtime.undo_checkpoint(&id)?)?)
}

fn workspace_path(runtime: &Runtime, params: &Value) -> Result<PathBuf, RpcServerError> {
    let root = runtime
        .workspace_root()
        .ok_or_else(|| RpcServerError::Runtime("workspace is not open".to_owned()))?;
    if let Some(path) = params.get("path").and_then(Value::as_str) {
        let requested = std::fs::canonicalize(path)
            .map_err(|error| RpcServerError::Runtime(error.to_string()))?;
        if !requested.starts_with(root) {
            return Err(RpcServerError::Runtime(
                "path is outside the open workspace".to_owned(),
            ));
        }
        return Ok(requested);
    }
    Ok(root.to_path_buf())
}

fn session_id(params: &Value) -> Result<SessionId, RpcServerError> {
    let value = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing session_id".to_owned()))?;
    SessionId::new(value.to_owned()).map_err(|error| RpcServerError::Runtime(error.to_string()))
}

fn validate_package_manager(value: &str) -> Result<(), RpcServerError> {
    const ALLOWED: &[&str] = &[
        "npm", "pnpm", "yarn", "bun", "cargo", "poetry", "pipenv", "go", "maven", "gradle",
        "composer",
    ];
    if ALLOWED.contains(&value) {
        Ok(())
    } else {
        Err(RpcServerError::Runtime(
            "unsupported package manager".to_owned(),
        ))
    }
}

fn validate_command(name: &str, command: &[String]) -> Result<(), RpcServerError> {
    if command.is_empty() || command.iter().any(|part| part.trim().is_empty()) {
        return Err(RpcServerError::Runtime(format!("{name} command is empty")));
    }
    Ok(())
}

fn ok_response(id: Option<String>, result: Value) -> RpcResponse {
    RpcResponse {
        version: RPC_PROTOCOL_VERSION,
        id,
        ok: true,
        result: Some(result),
        error: None,
    }
}

fn error_response(id: Option<String>, code: &str, message: impl Into<String>) -> RpcResponse {
    RpcResponse {
        version: RPC_PROTOCOL_VERSION,
        id,
        ok: false,
        result: None,
        error: Some(RpcError {
            code: code.to_owned(),
            message: message.into(),
            data: None,
        }),
    }
}

fn error_code(error: &RpcServerError) -> &'static str {
    if matches!(error, RpcServerError::AgentUnavailable) {
        "agent_unavailable"
    } else if error.to_string().contains("run_in_progress") {
        "run_in_progress"
    } else if error.to_string().contains("outside") || error.to_string().contains("workspace") {
        "workspace_error"
    } else {
        "runtime_error"
    }
}

fn error_code_agent(error: &AgentError) -> &'static str {
    match error {
        AgentError::Cancelled => "cancelled",
        AgentError::ApprovalDenied { .. } => "approval_denied",
        AgentError::LimitExceeded { .. } => "limit_exceeded",
        AgentError::Model(_) => "model_error",
        AgentError::Core(_) => "core_error",
        AgentError::Tool(_) => "tool_error",
    }
}
