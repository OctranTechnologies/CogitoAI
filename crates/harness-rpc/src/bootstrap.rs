//! Shared local runtime discovery, startup, and RPC connection management.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::{EmbeddedRuntimeLauncher, RuntimeLaunchConfig, RuntimeLauncher};
use crate::{RpcClient, RpcClientError, RpcResponse};

const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_secs(12);
const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_millis(80);
const STALE_LOCK_AGE: Duration = Duration::from_secs(120);

/// Current state of the shared runtime connection lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ConnectionState {
    Disconnected,
    Discovering,
    Starting,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConnectionStatus {
    pub state: ConnectionState,
    pub endpoint: Option<SocketAddr>,
    pub workspace_root: Option<PathBuf>,
    pub started_runtime: bool,
    pub last_error: Option<String>,
}

impl Default for ConnectionStatus {
    fn default() -> Self {
        Self {
            state: ConnectionState::Disconnected,
            endpoint: None,
            workspace_root: None,
            started_runtime: false,
            last_error: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum RuntimeConnectError {
    #[error("invalid RPC endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("invalid workspace path: {0}")]
    Workspace(String),
    #[error("runtime connection failed: {0}")]
    Transport(String),
    #[error("runtime rejected {method}: {message}")]
    Rejected { method: String, message: String },
    #[error("runtime startup failed: {0}")]
    Startup(String),
    #[error("timed out waiting for the runtime at {endpoint}")]
    ReadinessTimeout { endpoint: SocketAddr },
    #[error("timed out waiting for the runtime startup lock at {endpoint}")]
    StartupLockTimeout { endpoint: SocketAddr },
    #[error("invalid connection-state transition: {from:?} → {to:?}")]
    InvalidTransition {
        from: ConnectionState,
        to: ConnectionState,
    },
}

impl From<RpcClientError> for RuntimeConnectError {
    fn from(error: RpcClientError) -> Self {
        Self::Transport(error.to_string())
    }
}

#[derive(Clone)]
pub struct RuntimeConnector {
    inner: Arc<ConnectorInner>,
}

struct ConnectorInner {
    status: Mutex<ConnectionStatus>,
    launcher: Arc<dyn RuntimeLauncher>,
    readiness_timeout: Duration,
    retry_interval: Duration,
}

impl Default for RuntimeConnector {
    fn default() -> Self {
        Self::new(Arc::new(EmbeddedRuntimeLauncher))
    }
}

impl RuntimeConnector {
    pub fn new(launcher: Arc<dyn RuntimeLauncher>) -> Self {
        Self::with_timing(launcher, DEFAULT_READINESS_TIMEOUT, DEFAULT_RETRY_INTERVAL)
    }

    /// Constructor with short timings for deterministic connector tests.
    pub fn with_timing(
        launcher: Arc<dyn RuntimeLauncher>,
        readiness_timeout: Duration,
        retry_interval: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(ConnectorInner {
                status: Mutex::new(ConnectionStatus::default()),
                launcher,
                readiness_timeout,
                retry_interval,
            }),
        }
    }

    /// Resolves an explicit endpoint, `COGITO_RPC_ADDRESS`, or a stable
    /// workspace-specific loopback endpoint in that order.
    pub fn resolve_endpoint(
        workspace_root: &std::path::Path,
        explicit: Option<&str>,
    ) -> Result<SocketAddr, RuntimeConnectError> {
        let configured = explicit
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))
            .map(str::to_owned)
            .or_else(|| {
                std::env::var("COGITO_RPC_ADDRESS")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            });
        if let Some(configured) = configured {
            return configured
                .parse()
                .map_err(|error: std::net::AddrParseError| {
                    RuntimeConnectError::InvalidEndpoint(error.to_string())
                });
        }
        let workspace =
            fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
        // Use a specified hash rather than `DefaultHasher`, whose algorithm is
        // intentionally not a stable cross-version interface. The endpoint
        // should remain predictable for a workspace after a Harness upgrade.
        let path = workspace.to_string_lossy();
        #[cfg(windows)]
        let path = path.to_lowercase();
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let port = 30_000 + (hash % 20_000) as u16;
        Ok(SocketAddr::from(([127, 0, 0, 1], port)))
    }

    /// Connects to a healthy runtime, or serializes startup and launches the
    /// shared embedded RPC runtime when the endpoint is not serving Harness RPC.
    pub fn connect_or_start(
        &self,
        config: &RuntimeLaunchConfig,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let config = normalized_config(config)?;
        self.begin(config.clone());
        self.transition(ConnectionState::Discovering, None)?;
        self.transition(ConnectionState::Connecting, None)?;
        match probe(&config) {
            Ok(client) => return self.connected(client, false),
            Err(error) if is_runtime_absent(&error) => {}
            Err(error) => return self.failed(error),
        }

        let deadline = Instant::now() + self.inner.readiness_timeout;
        let mut startup_lock = loop {
            match try_startup_lock(config.address) {
                Ok(Some(lock)) => break Some(lock),
                Ok(None) => match probe(&config) {
                    Ok(client) => return self.connected(client, false),
                    Err(error) if is_runtime_absent(&error) => {
                        if Instant::now() >= deadline {
                            return self.failed(RuntimeConnectError::StartupLockTimeout {
                                endpoint: config.address,
                            });
                        }
                        thread::sleep(self.inner.retry_interval);
                    }
                    Err(error) => return self.failed(error),
                },
                Err(error) => return self.failed(error),
            }
        };

        // The first starter may have won the race after our first probe.
        match probe(&config) {
            Ok(client) => {
                drop(startup_lock.take());
                return self.connected(client, false);
            }
            Err(error) if is_runtime_absent(&error) => {}
            Err(error) => return self.failed(error),
        }

        self.transition(ConnectionState::Starting, None)?;
        if let Err(error) = self.inner.launcher.start_runtime(&config) {
            // Another process can bind between the recheck and the launch.
            if !is_address_in_use(&error) {
                return self.failed(RuntimeConnectError::Startup(error));
            }
        } else {
            self.set_started_runtime(true);
        }
        // Keep the cross-process lock until a healthy server is reachable.
        // Releasing it immediately after spawning lets a second client race
        // through its recheck and launch a duplicate server before this one
        // binds its listener.
        let _startup_lock = startup_lock.take();
        self.wait_until_ready_normalized(&config)
    }

    /// Connects only to an already-running healthy Harness RPC endpoint.
    pub fn connect_existing(
        &self,
        config: &RuntimeLaunchConfig,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let config = normalized_config(config)?;
        self.begin(config.clone());
        self.transition(ConnectionState::Discovering, None)?;
        self.transition(ConnectionState::Connecting, None)?;
        match probe(&config) {
            Ok(client) => self.connected(client, false),
            Err(error) => self.failed(error),
        }
    }

    /// Starts the configured runtime under the same cross-process startup lock.
    pub fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<(), RuntimeConnectError> {
        let client = self.connect_or_start(config)?;
        drop(client);
        Ok(())
    }

    /// Waits for a started runtime to accept RPC and open the requested
    /// workspace. Slow starts are polled until the configured deadline.
    pub fn wait_until_ready(
        &self,
        config: &RuntimeLaunchConfig,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let config = normalized_config(config)?;
        match self.status().state {
            ConnectionState::Disconnected | ConnectionState::Failed => {
                self.begin(config.clone());
                self.transition(ConnectionState::Discovering, None)?;
            }
            _ => {}
        }
        self.wait_until_ready_normalized(&config)
    }

    /// Re-establishes a connection, starting the runtime if it has stopped.
    pub fn reconnect(
        &self,
        config: &RuntimeLaunchConfig,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let config = normalized_config(config)?;
        self.transition(ConnectionState::Reconnecting, None)?;
        self.transition(ConnectionState::Discovering, None)?;
        self.connect_or_start(&config)
    }

    /// Marks the client disconnected. Closing the concrete socket is the
    /// caller's responsibility because it owns the RPC client/writer.
    pub fn disconnect(&self) -> Result<(), RuntimeConnectError> {
        self.transition(ConnectionState::Disconnected, None)
    }

    pub fn status(&self) -> ConnectionStatus {
        self.inner
            .status
            .lock()
            .expect("runtime connection status lock poisoned")
            .clone()
    }

    fn wait_until_ready_normalized(
        &self,
        config: &RuntimeLaunchConfig,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let deadline = Instant::now() + self.inner.readiness_timeout;
        self.transition(ConnectionState::Connecting, None)?;
        loop {
            match probe(config) {
                Ok(client) => return self.connected(client, self.status().started_runtime),
                Err(error) if is_runtime_absent(&error) => {
                    if Instant::now() >= deadline {
                        return self.failed(RuntimeConnectError::ReadinessTimeout {
                            endpoint: config.address,
                        });
                    }
                    thread::sleep(self.inner.retry_interval);
                }
                Err(error) => return self.failed(error),
            }
        }
    }

    fn begin(&self, config: RuntimeLaunchConfig) {
        let mut status = self
            .inner
            .status
            .lock()
            .expect("runtime connection status lock poisoned");
        if status.endpoint != Some(config.address) {
            status.started_runtime = false;
        }
        status.endpoint = Some(config.address);
        status.workspace_root = Some(config.workspace_root);
        status.last_error = None;
    }

    fn connected(
        &self,
        client: RpcClient,
        started_runtime: bool,
    ) -> Result<RpcClient, RuntimeConnectError> {
        self.transition(ConnectionState::Connected, None)?;
        self.set_started_runtime(started_runtime);
        Ok(client)
    }

    fn failed<T>(&self, error: RuntimeConnectError) -> Result<T, RuntimeConnectError> {
        let message = error.to_string();
        let _ = self.transition(ConnectionState::Failed, Some(message));
        Err(error)
    }

    fn set_started_runtime(&self, started_runtime: bool) {
        self.inner
            .status
            .lock()
            .expect("runtime connection status lock poisoned")
            .started_runtime = started_runtime;
    }

    fn transition(
        &self,
        to: ConnectionState,
        error: Option<String>,
    ) -> Result<(), RuntimeConnectError> {
        let mut status = self
            .inner
            .status
            .lock()
            .expect("runtime connection status lock poisoned");
        if !is_valid_transition(status.state, to) {
            return Err(RuntimeConnectError::InvalidTransition {
                from: status.state,
                to,
            });
        }
        status.state = to;
        status.last_error = error;
        Ok(())
    }
}

fn normalized_config(
    config: &RuntimeLaunchConfig,
) -> Result<RuntimeLaunchConfig, RuntimeConnectError> {
    let mut config = config.clone();
    config.workspace_root = fs::canonicalize(&config.workspace_root)
        .map_err(|error| RuntimeConnectError::Workspace(error.to_string()))?;
    Ok(config)
}

fn probe(config: &RuntimeLaunchConfig) -> Result<RpcClient, RuntimeConnectError> {
    let mut client = RpcClient::connect_timeout(config.address, Duration::from_secs(1))
        .map_err(|error| RuntimeConnectError::Transport(error.to_string()))?;
    ensure_response(
        client.request("rpc.initialize", json!({}))?,
        "rpc.initialize",
    )?;
    let workspace = config.workspace_root.to_string_lossy();
    ensure_response(
        client.request("workspace.open", json!({"path": workspace}))?,
        "workspace.open",
    )?;
    client.clear_timeouts()?;
    Ok(client)
}

fn ensure_response(response: RpcResponse, method: &str) -> Result<(), RuntimeConnectError> {
    if response.ok {
        return Ok(());
    }
    Err(RuntimeConnectError::Rejected {
        method: method.to_owned(),
        message: response
            .error
            .map_or_else(|| "unknown RPC error".to_owned(), |error| error.message),
    })
}

fn is_runtime_absent(error: &RuntimeConnectError) -> bool {
    match error {
        RuntimeConnectError::Transport(message) => {
            message.contains("Connection refused")
                || message.contains("connection refused")
                || message.contains("actively refused")
                || message.contains("timed out")
                || message.contains("closed before a response")
                || message.contains("os error 10061")
                || message.contains("os error 10060")
                || message.contains("os error 111")
        }
        _ => false,
    }
}

fn is_address_in_use(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("address already in use")
        || error.contains("only one usage of each socket address")
        || error.contains("os error 10048")
}

fn try_startup_lock(endpoint: SocketAddr) -> Result<Option<StartupLock>, RuntimeConnectError> {
    let lock_dir = std::env::temp_dir().join("cogito-runtime-locks");
    fs::create_dir_all(&lock_dir).map_err(|error| {
        RuntimeConnectError::Startup(format!("could not create startup-lock directory: {error}"))
    })?;
    let safe_endpoint = endpoint.to_string().replace([':', '.', '[', ']'], "_");
    let path = lock_dir.join(format!("{safe_endpoint}.lock"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let write_result = (|| {
                writeln!(file, "pid={}", std::process::id()).map_err(|error| {
                    RuntimeConnectError::Startup(format!("could not write startup lock: {error}"))
                })?;
                file.sync_all().map_err(|error| {
                    RuntimeConnectError::Startup(format!("could not flush startup lock: {error}"))
                })
            })();
            if let Err(error) = write_result {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
            Ok(Some(StartupLock { path, _file: file }))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if stale_lock(&path) {
                let _ = fs::remove_file(&path);
            }
            Ok(None)
        }
        Err(error) => Err(RuntimeConnectError::Startup(format!(
            "could not acquire startup lock: {error}"
        ))),
    }
}

fn stale_lock(path: &std::path::Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK_AGE)
}

struct StartupLock {
    path: PathBuf,
    _file: File,
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn is_valid_transition(from: ConnectionState, to: ConnectionState) -> bool {
    use ConnectionState::{
        Connected, Connecting, Disconnected, Discovering, Failed, Reconnecting, Starting,
    };
    from == to
        || matches!(
            (from, to),
            (Disconnected, Discovering)
                | (Disconnected, Disconnected)
                | (Disconnected, Reconnecting)
                | (Discovering, Connecting | Starting | Failed | Disconnected)
                | (Starting, Connecting | Connected | Failed | Disconnected)
                | (Connecting, Connected | Starting | Failed | Disconnected)
                | (
                    Connected,
                    Discovering | Connecting | Reconnecting | Disconnected | Failed
                )
                | (
                    Reconnecting,
                    Discovering | Starting | Connecting | Connected | Failed | Disconnected
                )
                | (Failed, Discovering | Disconnected)
                | (Failed, Reconnecting)
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RpcNotification, RpcRequest, RpcResponse, ServerMessage, RPC_PROTOCOL_VERSION};
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestLauncher {
        delay: Duration,
        fail: bool,
        started: AtomicBool,
    }

    impl TestLauncher {
        fn new(delay: Duration, fail: bool) -> Self {
            Self {
                delay,
                fail,
                started: AtomicBool::new(false),
            }
        }
    }

    impl RuntimeLauncher for TestLauncher {
        fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<(), String> {
            if self.fail {
                return Err("test startup failure".to_owned());
            }
            self.started.store(true, Ordering::SeqCst);
            let address = config.address;
            let delay = self.delay;
            thread::spawn(move || {
                thread::sleep(delay);
                let Ok(listener) = TcpListener::bind(address) else {
                    return;
                };
                for stream in listener.incoming().flatten() {
                    thread::spawn(move || serve_test_connection(stream));
                }
            });
            Ok(())
        }
    }

    fn serve_test_connection(mut stream: TcpStream) {
        let Ok(cloned) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(cloned);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let Ok(request) = serde_json::from_str::<RpcRequest>(&line) else {
                break;
            };
            let result = match request.method.as_str() {
                "rpc.initialize" => Some(json!({"version": RPC_PROTOCOL_VERSION})),
                "workspace.open" => Some(json!({"path": request.params["path"]})),
                _ => None,
            };
            let response = match result {
                Some(result) => ServerMessage::Response(RpcResponse {
                    version: RPC_PROTOCOL_VERSION,
                    id: request.id,
                    ok: true,
                    result: Some(result),
                    error: None,
                }),
                None => ServerMessage::Notification(RpcNotification {
                    version: RPC_PROTOCOL_VERSION,
                    method: "test.unknown".to_owned(),
                    params: json!({}),
                }),
            };
            let Ok(mut bytes) = serde_json::to_vec(&response) else {
                break;
            };
            bytes.push(b'\n');
            if stream.write_all(&bytes).is_err() {
                break;
            }
        }
    }

    fn test_config() -> RuntimeLaunchConfig {
        let workspace = std::env::current_dir().expect("current directory");
        RuntimeLaunchConfig::new(free_address(), workspace)
    }

    fn free_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral address");
        listener.local_addr().expect("read local address")
    }

    #[test]
    fn endpoint_resolution_is_workspace_stable_and_accepts_an_explicit_address() {
        let workspace = std::env::current_dir().expect("current directory");
        let first = RuntimeConnector::resolve_endpoint(&workspace, None).unwrap();
        let second = RuntimeConnector::resolve_endpoint(&workspace, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            RuntimeConnector::resolve_endpoint(&workspace, Some("127.0.0.1:4545")).unwrap(),
            "127.0.0.1:4545".parse::<SocketAddr>().unwrap()
        );
        assert!(RuntimeConnector::resolve_endpoint(&workspace, Some("not-an-address")).is_err());
    }

    #[test]
    fn every_documented_connection_state_transition_is_valid_and_invalid_edges_are_rejected() {
        use ConnectionState::{
            Connected, Connecting, Disconnected, Discovering, Failed, Reconnecting, Starting,
        };
        let states = [
            Disconnected,
            Discovering,
            Starting,
            Connecting,
            Connected,
            Reconnecting,
            Failed,
        ];
        let edges = [
            (Disconnected, Discovering),
            (Disconnected, Reconnecting),
            (Discovering, Connecting),
            (Discovering, Starting),
            (Discovering, Failed),
            (Discovering, Disconnected),
            (Starting, Connecting),
            (Starting, Connected),
            (Starting, Failed),
            (Starting, Disconnected),
            (Connecting, Connected),
            (Connecting, Starting),
            (Connecting, Failed),
            (Connecting, Disconnected),
            (Connected, Discovering),
            (Connected, Connecting),
            (Connected, Reconnecting),
            (Connected, Disconnected),
            (Connected, Failed),
            (Reconnecting, Discovering),
            (Reconnecting, Starting),
            (Reconnecting, Connecting),
            (Reconnecting, Connected),
            (Reconnecting, Failed),
            (Reconnecting, Disconnected),
            (Failed, Discovering),
            (Failed, Reconnecting),
            (Failed, Disconnected),
        ];
        for from in states {
            for to in states {
                let expected = from == to || edges.contains(&(from, to));
                assert_eq!(
                    is_valid_transition(from, to),
                    expected,
                    "transition {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn connects_to_a_server_that_is_already_running_without_starting_another() {
        let config = test_config();
        let listener = TcpListener::bind(config.address).expect("test server bind");
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || serve_test_connection(stream));
            }
        });
        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let connector = RuntimeConnector::new(launcher.clone());
        let _client = connector
            .connect_or_start(&config)
            .expect("existing server connects");
        assert!(!launcher.started.load(Ordering::SeqCst));
        assert_eq!(connector.status().state, ConnectionState::Connected);
        assert!(!connector.status().started_runtime);
        drop(_client);
        let _client = connector
            .connect_existing(&config)
            .expect("connect_existing uses the healthy endpoint");
        connector.disconnect().expect("disconnect transition");
        assert_eq!(connector.status().state, ConnectionState::Disconnected);
        let _client = connector
            .reconnect(&config)
            .expect("reconnect reuses the healthy endpoint");
        assert_eq!(connector.status().state, ConnectionState::Connected);
    }

    #[test]
    fn starts_and_connects_when_no_server_is_running() {
        let config = test_config();
        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let connector = RuntimeConnector::with_timing(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        let _client = connector.connect_or_start(&config).expect("runtime starts");
        assert!(launcher.started.load(Ordering::SeqCst));
        assert_eq!(connector.status().state, ConnectionState::Connected);
        assert!(connector.status().started_runtime);
    }

    #[test]
    fn startup_failure_is_reported_and_moves_the_connector_to_failed() {
        let config = test_config();
        let connector = RuntimeConnector::with_timing(
            Arc::new(TestLauncher::new(Duration::ZERO, true)),
            Duration::from_millis(200),
            Duration::from_millis(10),
        );
        assert!(connector.connect_or_start(&config).is_err());
        assert_eq!(connector.status().state, ConnectionState::Failed);
        assert_eq!(
            connector.status().last_error.as_deref(),
            Some("runtime startup failed: test startup failure")
        );
    }

    #[test]
    fn start_and_wait_until_ready_share_the_same_runtime_lifecycle() {
        let config = test_config();
        let connector = RuntimeConnector::with_timing(
            Arc::new(TestLauncher::new(Duration::from_millis(40), false)),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        connector.start_runtime(&config).expect("runtime starts");
        assert_eq!(connector.status().state, ConnectionState::Connected);
        assert!(connector.status().started_runtime);

        let _client = connector
            .wait_until_ready(&config)
            .expect("wait returns a healthy RPC connection");
        assert_eq!(connector.status().state, ConnectionState::Connected);
    }

    #[test]
    fn readiness_waits_for_a_slow_server_to_appear() {
        let config = test_config();
        let launcher = Arc::new(TestLauncher::new(Duration::from_millis(140), false));
        let connector = RuntimeConnector::with_timing(
            launcher,
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        let _client = connector
            .connect_or_start(&config)
            .expect("slow server becomes ready");
        assert_eq!(connector.status().state, ConnectionState::Connected);
    }

    #[test]
    fn concurrent_clients_serialize_startup_until_the_listener_is_ready() {
        let config = test_config();
        let first_launcher = Arc::new(TestLauncher::new(Duration::from_millis(180), false));
        let second_launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let first = RuntimeConnector::with_timing(
            first_launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        let second = RuntimeConnector::with_timing(
            second_launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );

        let first_config = config.clone();
        let first_thread = thread::spawn(move || first.connect_or_start(&first_config));
        thread::sleep(Duration::from_millis(30));
        let _second_client = second
            .connect_or_start(&config)
            .expect("second client connects to first runtime");
        let _first_client = first_thread
            .join()
            .expect("first connector thread completes")
            .expect("first runtime starts");

        assert!(first_launcher.started.load(Ordering::SeqCst));
        assert!(!second_launcher.started.load(Ordering::SeqCst));
    }
}
