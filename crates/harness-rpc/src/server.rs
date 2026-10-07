use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use harness_agent::{AgentError, AgentTask, ApprovalHandler};
use harness_core::{discover_workspace, CheckpointId, RunId, SessionId};
use harness_git::{is_runtime_state_path, GitClient, GitError};
use harness_models::ModelRegistryFilter;
use harness_policy::{
    ExecutionMode, OperationKind, Policy, PolicyDecision, PolicyRequest, RiskCategory,
};
use harness_pty::{PtyError, PtyRequest, SessionOrigin, TerminalEvent};
use harness_session::{EventId, EventSubscriber, EventSubscription, HarnessEvent};
use harness_tools::{CancellationToken, ToolRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use zeroize::Zeroize;

use crate::client::RpcClientError;
use crate::protocol::{
    RpcError, RpcNotification, RpcRequest, RpcResponse, ServerMessage, METHODS,
    RPC_PROTOCOL_VERSION,
};
use crate::settings::SecretStore;
use crate::Runtime;

const MAX_LINE_BYTES: usize = 26 * 1024 * 1024;
const MAX_IDEMPOTENCY_ENTRIES: usize = 1024;

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
    #[error(transparent)]
    Settings(#[from] crate::settings::SettingsError),
    #[error("agent runtime is not configured")]
    AgentUnavailable,
    #[error("RPC server must bind to a loopback address")]
    NonLoopbackAddress,
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
        if !address.ip().is_loopback() {
            return Err(RpcServerError::NonLoopbackAddress);
        }
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let credential_store = runtime.credential_store();
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(
            ServerState::new(approvals, credential_store)
                .with_session_root(session_root(runtime.as_ref()))
                .with_shutdown(Arc::clone(&shutdown)),
        );
        let event_subscription = runtime.agent_event_bus().map(|bus| {
            let state = Arc::clone(&state);
            bus.subscribe(Arc::new(EventForwarder { state }))
        });
        // Terminal output and exits are pushed to every connected client as
        // notifications, the same way agent events are.
        let terminal_sink_state = Arc::clone(&state);
        runtime
            .ptys()
            .set_sink(Arc::new(move |event: TerminalEvent| {
                terminal_sink_state.broadcast_terminal(event);
            }));
        Ok(Self {
            listener,
            runtime,
            state,
            shutdown,
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
        while !self.shutdown.load(Ordering::Acquire) {
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
    /// Terminals opened by each client, so a disconnect can reap its processes
    /// instead of leaking a shell with no consumer.
    client_terminals: Mutex<HashMap<u64, Vec<String>>>,
    /// Credential source for the settings surfaces. Holds no secret values: the
    /// store is asked only whether a credential exists.
    secret_store: Arc<dyn harness_models::CredentialStore>,
    /// Where sessions are written, reported by the Runtime settings screen.
    session_root: PathBuf,
    shutdown: Arc<AtomicBool>,
    shutdown_responses: Mutex<HashSet<(u64, String)>>,
    idempotency: IdempotencyCache,
}

#[derive(Default)]
struct IdempotencyCache {
    state: Mutex<IdempotencyCacheState>,
}

#[derive(Default)]
struct IdempotencyCacheState {
    entries: HashMap<String, Arc<IdempotencyEntry>>,
    insertion_order: VecDeque<String>,
}

struct IdempotencyEntry {
    request_fingerprint: u64,
    response: Mutex<Option<RpcResponse>>,
    completed: Condvar,
}

enum IdempotencyLookup {
    Execute(Arc<IdempotencyEntry>),
    Existing(Arc<IdempotencyEntry>),
    Conflict,
}

impl IdempotencyCache {
    fn lookup(&self, key: &str, method: &str, params: &Value) -> IdempotencyLookup {
        // Hash even caller-provided idempotency keys so a poorly behaved
        // client cannot cause credential-like values to remain in the cache.
        let key = format!("{:016x}", digest(&key));
        let fingerprint = digest(&(method, params));
        let mut state = self.state.lock().expect("RPC idempotency lock poisoned");
        if let Some(entry) = state.entries.get(&key) {
            return if entry.request_fingerprint == fingerprint {
                IdempotencyLookup::Existing(Arc::clone(entry))
            } else {
                IdempotencyLookup::Conflict
            };
        }
        let entry = Arc::new(IdempotencyEntry {
            request_fingerprint: fingerprint,
            response: Mutex::new(None),
            completed: Condvar::new(),
        });
        state.entries.insert(key.clone(), Arc::clone(&entry));
        state.insertion_order.push_back(key);
        IdempotencyLookup::Execute(entry)
    }

    fn trim(&self) {
        let mut state = self.state.lock().expect("RPC idempotency lock poisoned");
        while state.entries.len() > MAX_IDEMPOTENCY_ENTRIES {
            let removable = state.insertion_order.iter().position(|key| {
                state.entries.get(key).is_some_and(|entry| {
                    entry
                        .response
                        .lock()
                        .expect("RPC idempotency entry lock poisoned")
                        .is_some()
                })
            });
            let Some(index) = removable else { break };
            if let Some(key) = state.insertion_order.remove(index) {
                state.entries.remove(&key);
            }
        }
    }
}

fn digest(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

impl IdempotencyEntry {
    fn complete(&self, response: RpcResponse) {
        *self
            .response
            .lock()
            .expect("RPC idempotency entry lock poisoned") = Some(response);
        self.completed.notify_all();
    }

    fn wait(&self) -> RpcResponse {
        let mut response = self
            .response
            .lock()
            .expect("RPC idempotency entry lock poisoned");
        while response.is_none() {
            response = self
                .completed
                .wait(response)
                .expect("RPC idempotency entry lock poisoned");
        }
        response.as_ref().expect("response checked above").clone()
    }
}

struct ActiveRun {
    id: RunId,
    cancellation: CancellationToken,
}

impl ServerState {
    fn new(
        approvals: Arc<ApprovalBroker>,
        secret_store: Arc<dyn harness_models::CredentialStore>,
    ) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(0),
            active_run: Mutex::new(None),
            next_run: AtomicU64::new(0),
            seen_events: Mutex::new(Vec::new()),
            approvals,
            client_terminals: Mutex::new(HashMap::new()),
            secret_store,
            session_root: PathBuf::from("."),
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_responses: Mutex::new(HashSet::new()),
            idempotency: IdempotencyCache::default(),
        }
    }

    /// Credential source used by the settings surfaces.
    fn secret_store(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secret_store)
    }

    /// Overrides the session storage root reported to clients.
    fn with_session_root(mut self, session_root: PathBuf) -> Self {
        self.session_root = session_root;
        self
    }

    fn with_shutdown(mut self, shutdown: Arc<AtomicBool>) -> Self {
        self.shutdown = shutdown;
        self
    }

    fn schedule_shutdown_response(&self, client_id: u64, response_id: String) {
        self.shutdown_responses
            .lock()
            .expect("RPC shutdown lock poisoned")
            .insert((client_id, response_id));
    }

    fn take_shutdown_response(&self, client_id: u64, response_id: &str) -> bool {
        self.shutdown_responses
            .lock()
            .expect("RPC shutdown lock poisoned")
            .remove(&(client_id, response_id.to_owned()))
    }

    /// Where the runtime persists sessions, reported by the Runtime screen.
    fn session_root(&self) -> PathBuf {
        self.session_root.clone()
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

    /// Records a terminal as owned by a client so it can be reaped on exit.
    fn track_terminal(&self, client: u64, terminal_id: &str) {
        self.client_terminals
            .lock()
            .expect("RPC terminal lock poisoned")
            .entry(client)
            .or_default()
            .push(terminal_id.to_owned());
    }

    fn forget_terminal(&self, client: u64, terminal_id: &str) {
        let mut terminals = self
            .client_terminals
            .lock()
            .expect("RPC terminal lock poisoned");
        if let Some(owned) = terminals.get_mut(&client) {
            owned.retain(|id| id != terminal_id);
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

    /// Broadcasts terminal output and exit notifications.
    fn broadcast_terminal(&self, event: TerminalEvent) {
        let (method, params) = match &event {
            TerminalEvent::Output { terminal_id, data } => (
                "terminal.output",
                json!({ "terminal_id": terminal_id, "data": data }),
            ),
            TerminalEvent::Exited {
                terminal_id,
                exit_code,
                reason,
            } => (
                "terminal.exited",
                json!({
                    "terminal_id": terminal_id,
                    "exit_code": exit_code,
                    "reason": reason,
                }),
            ),
        };
        self.broadcast(ServerMessage::Notification(RpcNotification {
            version: RPC_PROTOCOL_VERSION,
            method: method.to_owned(),
            params,
        }));
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
    cancellation: Mutex<Option<CancellationToken>>,
}

/// How long a pending approval waits before it is treated as declined.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// How often a waiting approval checks for cancellation.
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
            cancellation: Mutex::new(None),
        }
    }

    /// Lets a pending approval be abandoned when the run it belongs to is
    /// cancelled, so cancelling actually takes effect while the agent is
    /// blocked waiting for a person who may have walked away.
    pub fn with_cancellation(&self, token: CancellationToken) {
        *self
            .cancellation
            .lock()
            .expect("approval cancellation lock poisoned") = Some(token);
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

    fn request(
        &self,
        request: &ToolRequest,
        risks: &[RiskCategory],
        reason: &str,
    ) -> Result<bool, AgentError> {
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
                "risk_categories": risks,
                "reason": reason,
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
        // Wait for the person, but never indefinitely: a cancellation must be
        // able to abandon the request, and an unanswered prompt must not pin a
        // worker for the lifetime of the process.
        let deadline = Instant::now() + APPROVAL_TIMEOUT;
        loop {
            if self
                .cancellation
                .lock()
                .expect("approval cancellation lock poisoned")
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                self.remove(&approval_id);
                return Ok(false);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.remove(&approval_id);
                return Ok(false);
            }
            match receiver.recv_timeout(remaining.min(APPROVAL_POLL_INTERVAL)) {
                Ok(approved) => {
                    self.remove(&approval_id);
                    return Ok(approved);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    self.remove(&approval_id);
                    return Ok(false);
                }
            }
        }
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
        self.broker.request(request, &[], "")
    }

    fn request_with_details(
        &self,
        request: &ToolRequest,
        risks: &[RiskCategory],
        reason: &str,
    ) -> Result<bool, AgentError> {
        self.broker.request(request, risks, reason)
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
    let mut read_failure = None;
    loop {
        line.clear();
        // A read failure is recorded rather than propagated so terminal cleanup
        // still runs. Closing a client that has unread output resets the
        // connection instead of sending a clean EOF, and an early return here
        // would leak that client's shell processes.
        let read = match reader.read_line(&mut line) {
            Ok(read) => read,
            Err(error) => {
                line.zeroize();
                read_failure = Some(error);
                break;
            }
        };
        if read == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            line.zeroize();
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
        let parsed = serde_json::from_str::<RpcRequest>(&line);
        // The wire buffer can contain an entered API key. Clear it before
        // dispatch, logging, or the next read overwrites the allocation.
        line.zeroize();
        match parsed {
            Ok(request) => {
                let response = dispatch(request, &state, &runtime, client_id);
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
    // Cleanup runs on every exit path, including a reset connection, so a
    // dropped client never leaves an orphaned shell behind.
    reap_client_terminals(&runtime, &state, client_id);
    state.unregister(client_id);
    drop(state);
    let _ = writer_thread.join();
    match read_failure {
        Some(error) => Err(RpcServerError::Io(error)),
        None => Ok(()),
    }
}

/// Terminates every terminal a disconnecting client owned.
///
/// Without this, closing the desktop window (or dropping a socket) would leave
/// orphaned shell processes running with nobody to read their output.
fn reap_client_terminals(runtime: &Runtime, state: &ServerState, client_id: u64) {
    let owned = {
        let mut terminals = state
            .client_terminals
            .lock()
            .expect("RPC terminal lock poisoned");
        terminals.remove(&client_id).unwrap_or_default()
    };
    if owned.is_empty() {
        return;
    }
    let manager = runtime.ptys();
    for terminal_id in owned {
        let _ = manager.close(&terminal_id);
    }
}

fn write_messages(
    mut stream: TcpStream,
    receiver: Receiver<ServerMessage>,
    state: Arc<ServerState>,
    client_id: u64,
) {
    for message in receiver {
        let response_id = match &message {
            ServerMessage::Response(response) => response.id.clone(),
            ServerMessage::Notification(_) => None,
        };
        if serde_json::to_writer(&mut stream, &message).is_err()
            || stream.write_all(b"\n").is_err()
            || stream.flush().is_err()
        {
            break;
        }
        if response_id
            .as_deref()
            .is_some_and(|id| state.take_shutdown_response(client_id, id))
        {
            state.shutdown.store(true, Ordering::Release);
            break;
        }
    }
    state.unregister(client_id);
}

fn dispatch(
    mut request: RpcRequest,
    state: &Arc<ServerState>,
    runtime: &Arc<Runtime>,
    client_id: u64,
) -> RpcResponse {
    if request.version != RPC_PROTOCOL_VERSION {
        clear_secret_param(&mut request.params);
        let mut response = error_response(
            request.id,
            "unsupported_version",
            format!("unsupported RPC protocol version {}", request.version),
        );
        if let Some(error) = response.error.as_mut() {
            error.data = Some(json!({
                "serverProtocolVersion": RPC_PROTOCOL_VERSION,
                "runtimeVersion": env!("CARGO_PKG_VERSION"),
            }));
        }
        return response;
    }
    if request.id.is_none() {
        clear_secret_param(&mut request.params);
        return error_response(request.id, "missing_id", "RPC requests require an id");
    }

    if request
        .idempotency_key
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.len() > 256 || key.chars().any(char::is_control))
    {
        clear_secret_param(&mut request.params);
        return error_response(
            request.id,
            "invalid_idempotency_key",
            "idempotency keys must be 1–256 printable characters",
        );
    }

    if is_mutating_method(&request.method) {
        if let Some(key) = request.idempotency_key.clone() {
            match state
                .idempotency
                .lookup(&key, &request.method, &request.params)
            {
                IdempotencyLookup::Execute(entry) => {
                    let response = dispatch_validated(request, state, runtime, client_id);
                    entry.complete(response.clone());
                    state.idempotency.trim();
                    return response;
                }
                IdempotencyLookup::Existing(entry) => {
                    let mut response = entry.wait();
                    response.id = request.id;
                    return response;
                }
                IdempotencyLookup::Conflict => {
                    return error_response(
                        request.id,
                        "idempotency_key_conflict",
                        "this idempotency key was already used for a different request",
                    );
                }
            }
        }
    }
    dispatch_validated(request, state, runtime, client_id)
}

fn is_mutating_method(method: &str) -> bool {
    matches!(
        method,
        "rpc.shutdown"
            | "settings.update_model"
            | "settings.update_permissions"
            | "credentials.disconnect"
            | "models.refresh"
            | "mcp.refresh"
            | "mcp.disconnect"
            | "mcp.resources"
            | "config.update"
            | "session.create"
            | "session.resume"
            | "agent.send"
            | "agent.run"
            | "agent.approve"
            | "agent.deny"
            | "agent.cancel"
            | "checkpoint.undo"
            | "terminal.open"
            | "terminal.write"
            | "terminal.resize"
            | "terminal.close"
    )
}

fn dispatch_validated(
    mut request: RpcRequest,
    state: &Arc<ServerState>,
    runtime: &Arc<Runtime>,
    client_id: u64,
) -> RpcResponse {
    let result = match request.method.as_str() {
        "health/check" => Ok(json!({
            "status": "ready",
            // protocolVersion remains as a compatibility alias for existing
            // diagnostics; the connection handshake uses the explicit fields.
            "protocolVersion": RPC_PROTOCOL_VERSION,
            "clientProtocolVersion": request.version,
            "serverProtocolVersion": RPC_PROTOCOL_VERSION,
            "clientVersion": request.params.get("clientVersion")
                .and_then(Value::as_str)
                .unwrap_or(""),
            "runtimeVersion": env!("CARGO_PKG_VERSION"),
            "instanceId": runtime.instance_id(),
            "pid": std::process::id(),
        })),
        "rpc.initialize" => Ok(json!({
            "version": RPC_PROTOCOL_VERSION,
            "server": "cogito-harness",
            "instanceId": runtime.instance_id(),
            "methods": METHODS,
        })),
        "rpc.shutdown" => {
            let requested_instance = request
                .params
                .get("instanceId")
                .or_else(|| request.params.get("instance_id"))
                .and_then(Value::as_str);
            if requested_instance != Some(runtime.instance_id()) {
                Err(RpcServerError::Runtime(
                    "shutdown request did not match this runtime instance".to_owned(),
                ))
            } else {
                state.schedule_shutdown_response(
                    client_id,
                    request
                        .id
                        .clone()
                        .expect("request IDs are validated before dispatch"),
                );
                Ok(json!({"status": "shutting_down"}))
            }
        }
        "workspace.open" | "workspace.inspect" => workspace(runtime, &request.params),
        "config.inspect" => config_inspect(runtime, &request.params),
        "config.update" => config_update(runtime, &request.params),
        "settings.inspect" => settings_inspect(state, runtime),
        "settings.update_model" => settings_update_model(state, runtime, &request.params),
        "settings.update_permissions" => {
            settings_update_permissions(state, runtime, &request.params)
        }
        "settings.test_model" => settings_test_model(state, runtime),
        "credentials.list" => credentials_list(state, runtime),
        "credentials.validate" => {
            let mut params = std::mem::take(&mut request.params);
            credentials_validate(runtime, &mut params)
        }
        "credentials.connect" => {
            let mut params = std::mem::take(&mut request.params);
            credentials_connect(state, runtime, &mut params)
        }
        "credentials.disconnect" => credentials_disconnect(state, runtime, &request.params),
        "models.list" => models_list(runtime, &request.params),
        "models.refresh" => models_refresh(runtime, &request.params),
        "mcp.inspect" => Ok(json!(runtime.mcp_manager().snapshot())),
        "mcp.refresh" => mcp_refresh(runtime, &request.params),
        "mcp.disconnect" => mcp_disconnect(runtime, &request.params),
        "mcp.resources" => mcp_resources(runtime, &request.params),
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
        "file.read" => file_read(runtime, &request.params),
        "git.file_diff" => git_file_diff(runtime, &request.params),
        "checkpoint.list" => checkpoint_list(runtime),
        "checkpoint.inspect" => checkpoint_inspect(runtime, &request.params),
        "checkpoint.undo" => checkpoint_undo(runtime, &request.params),
        "terminal.list" => terminal_list(runtime),
        "terminal.open" => terminal_open(runtime, state, client_id, &request.params),
        "terminal.write" => terminal_write(runtime, &request.params),
        "terminal.resize" => terminal_resize(runtime, &request.params),
        "terminal.close" => terminal_close(runtime, state, client_id, &request.params),
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
    // A pending approval must be abandonable, otherwise cancelling a run whose
    // prompt nobody is answering would have no effect at all.
    state.approvals.with_cancellation(cancellation.clone());
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
                    "error": {"code": error_code_agent(&error), "message": harness_core::redact_sensitive(&error.to_string())},
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
    let mut status = client.status()?;
    // The harness stores its own sessions and checkpoints inside the repository.
    // Those are runtime internals, so they are withheld from code-change views
    // instead of being presented to the user as if they were their own edits.
    for list in [
        &mut status.changed_files,
        &mut status.staged_files,
        &mut status.unstaged_files,
        &mut status.untracked_files,
    ] {
        list.retain(|path| !is_runtime_state_path(path));
    }
    status.is_clean = status.changed_files.is_empty();
    Ok(serde_json::to_value(status)?)
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

/// Reads a worktree file for read-only inspection. The runtime owns all
/// filesystem access; the desktop never reads files directly.
fn file_read(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let client = GitClient::open(&workspace_path(runtime, &json!({}))?)?;
    let path = relative_file_param(params)?;
    Ok(serde_json::to_value(client.read_file(&path)?)?)
}

/// Produces a before/after view of one changed file for diff rendering.
fn git_file_diff(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let client = GitClient::open(&workspace_path(runtime, &json!({}))?)?;
    let path = relative_file_param(params)?;
    Ok(serde_json::to_value(client.file_change(&path)?)?)
}

fn relative_file_param(params: &Value) -> Result<String, RpcServerError> {
    params
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RpcServerError::Runtime("missing path".to_owned()))
}

// ---------------------------------------------------------------------------
// Human terminals
//
// These are interactive pseudo-terminals driven by a person at the desktop
// shell. They are **not** the agent's command path: the agent reaches shell
// execution only through `ToolRegistry` + the policy engine, and no tool can
// open a terminal. A terminal intentionally bypasses agent policy because a
// human is directly responsible for every keystroke. `origin` must be "human",
// so an agent-initiated call fails loudly instead of silently gaining an
// unrestricted shell.
// ---------------------------------------------------------------------------

fn terminal_list(runtime: &Runtime) -> Result<Value, RpcServerError> {
    Ok(serde_json::to_value(runtime.ptys().list())?)
}

fn terminal_open(
    runtime: &Runtime,
    state: &Arc<ServerState>,
    client_id: u64,
    params: &Value,
) -> Result<Value, RpcServerError> {
    let origin = params
        .get("origin")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if origin != "human" {
        return Err(RpcServerError::Runtime(PtyError::InvalidOrigin.to_string()));
    }
    let working_directory = workspace_path(runtime, &json!({}))?;
    let request = PtyRequest {
        origin: SessionOrigin::Human,
        program: params
            .get("program")
            .and_then(Value::as_str)
            .map(str::to_owned),
        args: params
            .get("args")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        working_directory: params
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or(working_directory),
        cols: params
            .get("cols")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(harness_pty::DEFAULT_COLS),
        rows: params
            .get("rows")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(harness_pty::DEFAULT_ROWS),
    };
    let info = runtime.ptys().open(request).map_err(pty_error)?;
    state.track_terminal(client_id, &info.id);
    Ok(serde_json::to_value(info)?)
}

fn terminal_write(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let id = terminal_id_param(params)?;
    let data = params
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing data".to_owned()))?;
    runtime.ptys().write(&id, data).map_err(pty_error)?;
    Ok(json!({ "written": data.len() }))
}

fn terminal_resize(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let id = terminal_id_param(params)?;
    let dimension = |key: &str, fallback: u16| {
        params
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(fallback)
    };
    let info = runtime
        .ptys()
        .resize(&id, dimension("cols", 80), dimension("rows", 24))
        .map_err(pty_error)?;
    Ok(serde_json::to_value(info)?)
}

fn terminal_close(
    runtime: &Runtime,
    state: &Arc<ServerState>,
    client_id: u64,
    params: &Value,
) -> Result<Value, RpcServerError> {
    let id = terminal_id_param(params)?;
    state.forget_terminal(client_id, &id);
    runtime.ptys().close(&id).map_err(pty_error)?;
    Ok(json!({ "closed": true }))
}

fn terminal_id_param(params: &Value) -> Result<String, RpcServerError> {
    params
        .get("terminal_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RpcServerError::Runtime("missing terminal_id".to_owned()))
}

/// Maps a PTY failure onto a stable RPC error code.
fn pty_error(error: PtyError) -> RpcServerError {
    RpcServerError::Runtime(format!("{}: {error}", error.code()))
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

/// Reports where the runtime keeps its durable state, for the Runtime screen.
fn session_root(runtime: &Runtime) -> PathBuf {
    runtime.session_root().map_or_else(
        || {
            runtime
                .workspace_root()
                .map(|root| root.join(".cogito/sessions"))
                .unwrap_or_else(|| PathBuf::from(".cogito/sessions"))
        },
        PathBuf::from,
    )
}

/// Assembles the full settings snapshot for the desktop settings surfaces.
///
/// The snapshot never contains a credential value, only whether one is
/// available and which environment variable holds it.
fn settings_snapshot(state: &ServerState, runtime: &Runtime) -> Result<Value, RpcServerError> {
    let store = state.secret_store();
    let session_root = state.session_root();
    Ok(serde_json::to_value(crate::settings::snapshot(
        runtime,
        store.as_ref(),
        &session_root,
    )?)?)
}

fn settings_inspect(state: &ServerState, runtime: &Runtime) -> Result<Value, RpcServerError> {
    settings_snapshot(state, runtime)
}

fn settings_update_model(
    state: &ServerState,
    runtime: &Runtime,
    params: &Value,
) -> Result<Value, RpcServerError> {
    let request: crate::settings::UpdateModelRequest = serde_json::from_value(params.clone())?;
    crate::settings::apply_model(runtime, &request)?;
    settings_snapshot(state, runtime)
}

fn settings_update_permissions(
    state: &ServerState,
    runtime: &Runtime,
    params: &Value,
) -> Result<Value, RpcServerError> {
    let request: crate::settings::UpdatePermissionsRequest =
        serde_json::from_value(params.clone())?;
    crate::settings::apply_permissions(runtime, &request)?;
    settings_snapshot(state, runtime)
}

fn settings_test_model(state: &ServerState, runtime: &Runtime) -> Result<Value, RpcServerError> {
    let model = runtime.model();
    let result = crate::settings::test_model_connection(&model, state.secret_store().as_ref());
    Ok(serde_json::to_value(result)?)
}

fn credentials_list(state: &ServerState, runtime: &Runtime) -> Result<Value, RpcServerError> {
    Ok(
        json!({"providers": crate::settings::provider_credentials(runtime, state.secret_store().as_ref())}),
    )
}

fn credentials_validate(runtime: &Runtime, params: &mut Value) -> Result<Value, RpcServerError> {
    let request = take_provider_credential_request(params)?;
    let result = crate::settings::validate_provider_key(
        runtime,
        &request.provider_id,
        request.api_key.as_str(),
    );
    serde_json::to_value(result).map_err(Into::into)
}

fn credentials_connect(
    state: &ServerState,
    runtime: &Runtime,
    params: &mut Value,
) -> Result<Value, RpcServerError> {
    let request = take_provider_credential_request(params)?;
    let result = crate::settings::connect_provider(
        runtime,
        state.secret_store().as_ref(),
        &request.provider_id,
        request.api_key.as_str(),
    );
    let result = result?;
    runtime.refresh_agent_runner().map_err(|error| {
        RpcServerError::Runtime(harness_core::redact_sensitive(&error.to_string()))
    })?;
    Ok(
        json!({"provider": result, "providers": crate::settings::provider_credentials(runtime, state.secret_store().as_ref())}),
    )
}

fn take_provider_credential_request(
    params: &mut Value,
) -> Result<crate::settings::ProviderCredentialRequest, RpcServerError> {
    let Some(object) = params.as_object_mut() else {
        return Err(RpcServerError::Runtime(
            "credential request must be an object".to_owned(),
        ));
    };
    let api_key = match object.remove("api_key") {
        Some(Value::String(value)) => zeroize::Zeroizing::new(value),
        _ => {
            return Err(RpcServerError::Runtime(
                "api_key is required as a string".to_owned(),
            ));
        }
    };
    let provider_id = object
        .remove("provider_id")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| RpcServerError::Runtime("provider_id is required".to_owned()))?;
    Ok(crate::settings::ProviderCredentialRequest {
        provider_id,
        api_key,
    })
}

fn clear_secret_param(params: &mut Value) {
    if let Some(Value::String(secret)) = params.get_mut("api_key") {
        secret.zeroize();
    }
}

fn credentials_disconnect(
    state: &ServerState,
    runtime: &Runtime,
    params: &Value,
) -> Result<Value, RpcServerError> {
    let request: crate::settings::ProviderDisconnectRequest =
        serde_json::from_value(params.clone())?;
    let result = crate::settings::disconnect_provider(
        runtime,
        state.secret_store().as_ref(),
        &request.provider_id,
    )?;
    runtime.refresh_agent_runner().map_err(|error| {
        RpcServerError::Runtime(harness_core::redact_sensitive(&error.to_string()))
    })?;
    Ok(
        json!({"provider": result, "providers": crate::settings::provider_credentials(runtime, state.secret_store().as_ref())}),
    )
}

fn models_list(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let filter = if params.is_null() {
        ModelRegistryFilter::default()
    } else {
        serde_json::from_value(params.clone())?
    };
    let registry = runtime.model_registry();
    Ok(json!({
        "models": registry.models(&filter),
        "defaults": registry.defaults(),
    }))
}

fn models_refresh(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let provider_id = params.get("provider_id").and_then(Value::as_str);
    let registry = runtime.model_registry();
    let report = match provider_id.filter(|provider_id| !provider_id.is_empty()) {
        Some(provider_id) => registry.refresh_provider_report(provider_id),
        None => registry.refresh_all(),
    };
    serde_json::to_value(report).map_err(Into::into)
}

fn mcp_refresh(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    authorize_mcp_control(runtime, "connect and discover", params)?;
    let server_id = params.get("server_id").and_then(Value::as_str);
    let snapshot = runtime
        .mcp_manager()
        .refresh(server_id)
        .map_err(|error| RpcServerError::Runtime(error.to_string()))?;
    Ok(serde_json::to_value(snapshot)?)
}

fn mcp_disconnect(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    let server_id = params
        .get("server_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing server_id".to_owned()))?;
    runtime
        .mcp_manager()
        .disconnect(server_id)
        .map_err(|error| RpcServerError::Runtime(error.to_string()))
        .and_then(|snapshot| serde_json::to_value(snapshot).map_err(Into::into))
}

fn mcp_resources(runtime: &Runtime, params: &Value) -> Result<Value, RpcServerError> {
    authorize_mcp_control(runtime, "list resources", params)?;
    let server_id = params
        .get("server_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcServerError::Runtime("missing server_id".to_owned()))?;
    let resources = runtime
        .mcp_manager()
        .list_resources(server_id)
        .map_err(|error| RpcServerError::Runtime(error.to_string()))?;
    Ok(json!({"resources": resources}))
}

/// The settings/CLI control surface is initiated by the human rather than a
/// model tool call. Still evaluate the same MCP policy as the agent. An ASK
/// decision is satisfied only by the explicit one-shot approval attached to
/// this direct user action; DENY rules can never be overridden by that flag.
fn authorize_mcp_control(
    runtime: &Runtime,
    action: &str,
    params: &Value,
) -> Result<(), RpcServerError> {
    let fallback_root = PathBuf::from(".");
    let workspace_root = runtime.workspace_root().unwrap_or(&fallback_root);
    authorize_mcp_control_with_policy(
        runtime.policy().as_ref(),
        workspace_root,
        runtime.execution_mode(),
        action,
        params,
    )
}

fn authorize_mcp_control_with_policy(
    policy: &dyn Policy,
    workspace_root: &std::path::Path,
    mode: ExecutionMode,
    action: &str,
    params: &Value,
) -> Result<(), RpcServerError> {
    let request = PolicyRequest {
        tool_name: format!("mcp_{action}"),
        operation: OperationKind::Mcp,
        workspace_root: workspace_root.to_path_buf(),
        path: None,
        command: None,
        mode,
    };
    let evaluation = policy.evaluate(&request);
    match evaluation.decision {
        PolicyDecision::Allow => Ok(()),
        PolicyDecision::Deny => Err(RpcServerError::Runtime(format!(
            "MCP {action} denied by policy: {}",
            evaluation.reason
        ))),
        PolicyDecision::Ask if params.get("approved").and_then(Value::as_bool) == Some(true) => {
            Ok(())
        }
        PolicyDecision::Ask => Err(RpcServerError::Runtime(format!(
            "MCP {action} requires explicit user approval: {}",
            evaluation.reason
        ))),
    }
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
            message: harness_core::redact_sensitive(&message.into()),
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

#[cfg(test)]
mod mcp_control_policy_tests {
    use super::*;
    use harness_policy::{NetworkAccess, PolicyEngine};

    #[test]
    fn direct_mcp_connect_requires_explicit_approval_for_ask() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = PolicyEngine::new(ExecutionMode::Normal, workspace.path());
        let params = json!({});
        assert!(authorize_mcp_control_with_policy(
            &policy,
            workspace.path(),
            ExecutionMode::Normal,
            "connect and discover",
            &params,
        )
        .is_err());
        assert!(authorize_mcp_control_with_policy(
            &policy,
            workspace.path(),
            ExecutionMode::Normal,
            "connect and discover",
            &json!({"approved": true}),
        )
        .is_ok());
    }

    #[test]
    fn direct_mcp_approval_cannot_override_network_deny() {
        let workspace = tempfile::tempdir().unwrap();
        let mut policy = PolicyEngine::new(ExecutionMode::Normal, workspace.path());
        policy.network_access = NetworkAccess::Deny;
        assert!(authorize_mcp_control_with_policy(
            &policy,
            workspace.path(),
            ExecutionMode::Normal,
            "connect and discover",
            &json!({"approved": true}),
        )
        .unwrap_err()
        .to_string()
        .contains("denied by policy"));
    }
}
