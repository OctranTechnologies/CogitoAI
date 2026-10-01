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

use crate::metadata::{
    process_exists, MetadataSnapshot, RuntimeMetadata, RuntimeMetadataStore,
    RPC_TRANSPORT_TCP_LOOPBACK, RUNTIME_METADATA_FORMAT_VERSION,
};
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
    metadata_store: RuntimeMetadataStore,
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
        Self::with_timing_and_runtime_directory(
            launcher,
            readiness_timeout,
            retry_interval,
            RuntimeMetadataStore::for_current_user_or_fallback()
                .directory()
                .to_path_buf(),
        )
    }

    /// Constructor with an isolated runtime-data directory, primarily useful
    /// for connector tests and managed application hosts.
    pub fn with_runtime_directory(
        launcher: Arc<dyn RuntimeLauncher>,
        runtime_directory: impl Into<PathBuf>,
    ) -> Self {
        Self::with_timing_and_runtime_directory(
            launcher,
            DEFAULT_READINESS_TIMEOUT,
            DEFAULT_RETRY_INTERVAL,
            runtime_directory.into(),
        )
    }

    pub fn with_timing_and_runtime_directory(
        launcher: Arc<dyn RuntimeLauncher>,
        readiness_timeout: Duration,
        retry_interval: Duration,
        runtime_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(ConnectorInner {
                status: Mutex::new(ConnectionStatus::default()),
                launcher,
                readiness_timeout,
                retry_interval,
                metadata_store: RuntimeMetadataStore::new(runtime_directory),
            }),
        }
    }

    /// Resolves an explicit endpoint or `COGITO_RPC_ADDRESS`. Without either,
    /// port zero requests an OS-assigned loopback port and metadata discovery.
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
            let endpoint: SocketAddr =
                configured
                    .parse()
                    .map_err(|error: std::net::AddrParseError| {
                        RuntimeConnectError::InvalidEndpoint(error.to_string())
                    })?;
            if !endpoint.ip().is_loopback() {
                return Err(RuntimeConnectError::InvalidEndpoint(
                    "RPC endpoints must use a loopback address".to_owned(),
                ));
            }
            return Ok(endpoint);
        }
        let _ = workspace_root;
        Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// Connects to a healthy runtime, or serializes startup and launches the
    /// shared embedded RPC runtime when the endpoint is not serving Harness RPC.
    pub fn connect_or_start(
        &self,
        config: &RuntimeLaunchConfig,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let mut config = normalized_config(config)?;
        validate_endpoint(config.address)?;
        self.begin(config.clone());
        self.transition(ConnectionState::Discovering, None)?;
        self.transition(ConnectionState::Connecting, None)?;
        if config.address.port() == 0 {
            match self.try_connect_from_metadata(&config, false) {
                Ok(Some(client)) => return self.connected(client, false),
                Ok(None) => {}
                Err(error) => return self.failed(error),
            }
        } else {
            match probe(&config, None) {
                Ok(client) => return self.connected(client, false),
                Err(error) if is_runtime_absent(&error) => {}
                Err(error) => return self.failed(error),
            }
        }

        let deadline = Instant::now() + self.inner.readiness_timeout;
        let lock_path = self
            .inner
            .metadata_store
            .startup_lock_path(&config.workspace_root);
        self.inner
            .metadata_store
            .ensure_private_directory()
            .map_err(|error| RuntimeConnectError::Startup(error.to_string()))?;
        let mut startup_lock = loop {
            match try_startup_lock(&lock_path) {
                Ok(Some(lock)) => break Some(lock),
                Ok(None) => {
                    let existing = if config.address.port() == 0 {
                        self.try_connect_from_metadata(&config, false)
                    } else {
                        match probe(&config, None) {
                            Ok(client) => Ok(Some(client)),
                            Err(error) if is_runtime_absent(&error) => Ok(None),
                            Err(error) => Err(error),
                        }
                    };
                    let existing = match existing {
                        Ok(existing) => existing,
                        Err(error) => return self.failed(error),
                    };
                    if let Some(client) = existing {
                        return self.connected(client, false);
                    }
                    if Instant::now() >= deadline {
                        return self.failed(RuntimeConnectError::StartupLockTimeout {
                            endpoint: config.address,
                        });
                    }
                    thread::sleep(self.inner.retry_interval);
                }
                Err(error) => return self.failed(error),
            }
        };

        // Re-read only after acquiring the lock. Until then, an unhealthy or
        // half-started metadata record may belong to the process holding it.
        let existing = if config.address.port() == 0 {
            self.try_connect_from_metadata(&config, true)
        } else {
            match probe(&config, None) {
                Ok(client) => Ok(Some(client)),
                Err(error) if is_runtime_absent(&error) => Ok(None),
                Err(error) => Err(error),
            }
        };
        match existing {
            Ok(Some(client)) => {
                drop(startup_lock.take());
                return self.connected(client, false);
            }
            Ok(None) => {}
            Err(error) => return self.failed(error),
        }

        self.transition(ConnectionState::Starting, None)?;
        let instance_id = crate::metadata::new_instance_id();
        config.instance_id = Some(instance_id.clone());
        let launch = self.inner.launcher.start_runtime(&config);
        let launch = match launch {
            Ok(launch) => launch,
            Err(error) if is_address_in_use(&error) && config.address.port() != 0 => {
                match probe(&config, None) {
                    Ok(client) => {
                        drop(startup_lock.take());
                        return self.connected(client, false);
                    }
                    Err(_) => return self.failed(RuntimeConnectError::Startup(error)),
                }
            }
            Err(error) => return self.failed(RuntimeConnectError::Startup(error)),
        };
        if !launch.endpoint.ip().is_loopback() || launch.endpoint.port() == 0 {
            return self.failed(RuntimeConnectError::Startup(
                "runtime launcher returned a non-loopback or unassigned endpoint".to_owned(),
            ));
        }
        if launch.instance_id != instance_id {
            return self.failed(RuntimeConnectError::Startup(
                "runtime launcher returned an unexpected instance ID".to_owned(),
            ));
        }
        if config.address.port() == 0 {
            let metadata = RuntimeMetadata::new(launch.endpoint, launch.instance_id.clone());
            if let Err(error) = self
                .inner
                .metadata_store
                .write(&config.workspace_root, &metadata)
            {
                return self.failed(RuntimeConnectError::Startup(format!(
                    "could not write runtime metadata: {error}"
                )));
            }
        }
        config.address = launch.endpoint;
        self.set_endpoint(launch.endpoint);
        self.set_started_runtime(true);
        let _startup_lock = startup_lock.take();
        self.wait_until_ready_normalized(&config, Some(&launch.instance_id))
    }

    /// Connects only to an already-running healthy Harness RPC endpoint.
    pub fn connect_existing(
        &self,
        config: &RuntimeLaunchConfig,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let config = normalized_config(config)?;
        validate_endpoint(config.address)?;
        self.begin(config.clone());
        self.transition(ConnectionState::Discovering, None)?;
        self.transition(ConnectionState::Connecting, None)?;
        if config.address.port() == 0 {
            match self.try_connect_from_metadata(&config, false)? {
                Some(client) => self.connected(client, false),
                None => self.failed(RuntimeConnectError::Rejected {
                    method: "rpc.initialize".to_owned(),
                    message: "no healthy runtime is registered for this workspace".to_owned(),
                }),
            }
        } else {
            match probe(&config, None) {
                Ok(client) => self.connected(client, false),
                Err(error) => self.failed(error),
            }
        }
    }

    fn try_connect_from_metadata(
        &self,
        config: &RuntimeLaunchConfig,
        clean_stale: bool,
    ) -> Result<Option<RpcClient>, RuntimeConnectError> {
        let snapshot = self
            .inner
            .metadata_store
            .read(&config.workspace_root)
            .map_err(|error| RuntimeConnectError::Startup(error.to_string()))?;
        let (metadata, bytes) = match snapshot {
            MetadataSnapshot::Missing => return Ok(None),
            MetadataSnapshot::Invalid(bytes) => {
                if clean_stale {
                    self.remove_metadata_snapshot(&config.workspace_root, &bytes)?;
                }
                return Ok(None);
            }
            MetadataSnapshot::Valid(metadata, bytes) => (metadata, bytes),
        };
        let endpoint = metadata.endpoint.parse::<SocketAddr>().ok();
        let compatible = metadata.metadata_version == RUNTIME_METADATA_FORMAT_VERSION
            && metadata.protocol_version == crate::RPC_PROTOCOL_VERSION
            && metadata.transport == RPC_TRANSPORT_TCP_LOOPBACK
            && !metadata.instance_id.is_empty()
            && endpoint.is_some_and(|endpoint| endpoint.ip().is_loopback() && endpoint.port() != 0);
        if !compatible {
            if clean_stale {
                self.remove_metadata_snapshot(&config.workspace_root, &bytes)?;
            }
            return Ok(None);
        }

        // PID presence is checked as a cheap stale-record signal, but RPC
        // identity remains authoritative because operating systems reuse PIDs.
        if process_exists(metadata.pid) == Some(false) {
            if clean_stale {
                self.remove_metadata_snapshot(&config.workspace_root, &bytes)?;
            }
            return Ok(None);
        }
        let mut candidate = config.clone();
        candidate.address = endpoint.expect("compatible metadata has an endpoint");
        match probe(&candidate, Some(&metadata.instance_id)) {
            Ok(client) => {
                self.set_endpoint(candidate.address);
                Ok(Some(client))
            }
            Err(_) => {
                if clean_stale {
                    self.remove_metadata_snapshot(&config.workspace_root, &bytes)?;
                }
                Ok(None)
            }
        }
    }

    fn remove_metadata_snapshot(
        &self,
        workspace_root: &std::path::Path,
        bytes: &[u8],
    ) -> Result<(), RuntimeConnectError> {
        self.inner
            .metadata_store
            .remove_if_unchanged(workspace_root, bytes)
            .map(|_| ())
            .map_err(|error| {
                RuntimeConnectError::Startup(format!(
                    "could not remove stale runtime metadata: {error}"
                ))
            })
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
        validate_endpoint(config.address)?;
        match self.status().state {
            ConnectionState::Disconnected | ConnectionState::Failed => {
                self.begin(config.clone());
                self.transition(ConnectionState::Discovering, None)?;
            }
            _ => {}
        }
        self.wait_until_ready_normalized(&config, None)
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
        expected_instance_id: Option<&str>,
    ) -> Result<RpcClient, RuntimeConnectError> {
        let deadline = Instant::now() + self.inner.readiness_timeout;
        self.transition(ConnectionState::Connecting, None)?;
        loop {
            if config.address.port() == 0 {
                if let Some(client) = self.try_connect_from_metadata(config, false)? {
                    return self.connected(client, self.status().started_runtime);
                }
            } else {
                match probe(config, expected_instance_id) {
                    Ok(client) => return self.connected(client, self.status().started_runtime),
                    Err(error) if is_runtime_absent(&error) => {}
                    Err(error) => return self.failed(error),
                }
            }
            if Instant::now() >= deadline {
                return self.failed(RuntimeConnectError::ReadinessTimeout {
                    endpoint: config.address,
                });
            }
            thread::sleep(self.inner.retry_interval);
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

    fn set_endpoint(&self, endpoint: SocketAddr) {
        self.inner
            .status
            .lock()
            .expect("runtime connection status lock poisoned")
            .endpoint = Some(endpoint);
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

fn validate_endpoint(endpoint: SocketAddr) -> Result<(), RuntimeConnectError> {
    if endpoint.ip().is_loopback() {
        Ok(())
    } else {
        Err(RuntimeConnectError::InvalidEndpoint(
            "RPC endpoints must use a loopback address".to_owned(),
        ))
    }
}

fn probe(
    config: &RuntimeLaunchConfig,
    expected_instance_id: Option<&str>,
) -> Result<RpcClient, RuntimeConnectError> {
    let mut client = RpcClient::connect_timeout(config.address, Duration::from_secs(1))
        .map_err(|error| RuntimeConnectError::Transport(error.to_string()))?;
    let initialized = response_result(
        client.request("rpc.initialize", json!({}))?,
        "rpc.initialize",
    )?;
    if initialized
        .get("version")
        .and_then(serde_json::Value::as_u64)
        != Some(u64::from(crate::RPC_PROTOCOL_VERSION))
    {
        return Err(RuntimeConnectError::Rejected {
            method: "rpc.initialize".to_owned(),
            message: "server reports an incompatible RPC protocol version".to_owned(),
        });
    }
    let instance_id = initialized
        .get("instanceId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| RuntimeConnectError::Rejected {
            method: "rpc.initialize".to_owned(),
            message: "server did not report its runtime instance ID".to_owned(),
        })?;
    if expected_instance_id.is_some_and(|expected| expected != instance_id) {
        return Err(RuntimeConnectError::Rejected {
            method: "rpc.initialize".to_owned(),
            message: "server instance does not match runtime metadata".to_owned(),
        });
    }
    let workspace = config.workspace_root.to_string_lossy();
    response_result(
        client.request("workspace.open", json!({"path": workspace}))?,
        "workspace.open",
    )?;
    client.clear_timeouts()?;
    Ok(client)
}

fn response_result(
    response: RpcResponse,
    method: &str,
) -> Result<serde_json::Value, RuntimeConnectError> {
    if response.ok {
        return Ok(response.result.unwrap_or(serde_json::Value::Null));
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

fn try_startup_lock(path: &std::path::Path) -> Result<Option<StartupLock>, RuntimeConnectError> {
    let lock_dir = path.parent().ok_or_else(|| {
        RuntimeConnectError::Startup("startup-lock path has no parent directory".to_owned())
    })?;
    fs::create_dir_all(lock_dir).map_err(|error| {
        RuntimeConnectError::Startup(format!("could not create startup-lock directory: {error}"))
    })?;
    match OpenOptions::new().write(true).create_new(true).open(path) {
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
                let _ = fs::remove_file(path);
                return Err(error);
            }
            Ok(Some(StartupLock {
                path: path.to_path_buf(),
                _file: file,
            }))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if stale_lock(path) {
                let _ = fs::remove_file(path);
            }
            Ok(None)
        }
        Err(error) => Err(RuntimeConnectError::Startup(format!(
            "could not acquire startup lock: {error}"
        ))),
    }
}

fn stale_lock(path: &std::path::Path) -> bool {
    let age = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    let owner_pid = fs::read_to_string(path).ok().and_then(|contents| {
        contents.lines().find_map(|line| {
            line.strip_prefix("pid=")
                .and_then(|pid| pid.parse::<u32>().ok())
        })
    });
    if let Some(pid) = owner_pid {
        if process_exists(pid) == Some(false) {
            return true;
        }
    }
    age.is_some_and(|age| age > STALE_LOCK_AGE)
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
    use crate::{
        RpcNotification, RpcRequest, RpcResponse, RuntimeLaunchInfo, RuntimeMetadata,
        RuntimeMetadataStore, ServerMessage, RPC_PROTOCOL_VERSION,
    };
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
        fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String> {
            if self.fail {
                return Err("test startup failure".to_owned());
            }
            self.started.store(true, Ordering::SeqCst);
            let listener = TcpListener::bind(config.address).map_err(|error| error.to_string())?;
            let endpoint = listener.local_addr().map_err(|error| error.to_string())?;
            let delay = self.delay;
            let instance_id = config
                .instance_id
                .clone()
                .ok_or_else(|| "test launch requires an instance ID".to_owned())?;
            let server_instance_id = instance_id.clone();
            thread::spawn(move || {
                thread::sleep(delay);
                for stream in listener.incoming().flatten() {
                    let instance_id = server_instance_id.clone();
                    thread::spawn(move || serve_test_connection(stream, instance_id));
                }
            });
            Ok(RuntimeLaunchInfo {
                endpoint,
                instance_id,
            })
        }
    }

    fn serve_test_connection(mut stream: TcpStream, instance_id: String) {
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
                "rpc.initialize" => Some(json!({
                    "version": RPC_PROTOCOL_VERSION,
                    "instanceId": instance_id.clone(),
                })),
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

    fn auto_config() -> RuntimeLaunchConfig {
        let workspace = std::env::current_dir().expect("current directory");
        RuntimeLaunchConfig::new("127.0.0.1:0".parse().unwrap(), workspace)
    }

    fn test_connector(
        launcher: Arc<dyn RuntimeLauncher>,
        readiness_timeout: Duration,
        retry_interval: Duration,
    ) -> (RuntimeConnector, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("isolated runtime metadata directory");
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            launcher,
            readiness_timeout,
            retry_interval,
            directory.path(),
        );
        (connector, directory)
    }

    fn run_fake_server(endpoint: SocketAddr, instance_id: &str) -> TcpListener {
        let listener = TcpListener::bind(endpoint).expect("fake RPC server bind");
        let server_listener = listener.try_clone().expect("clone server listener");
        let instance_id = instance_id.to_owned();
        thread::spawn(move || {
            for stream in server_listener.incoming().flatten() {
                let instance_id = instance_id.clone();
                thread::spawn(move || serve_test_connection(stream, instance_id));
            }
        });
        listener
    }

    fn free_address() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral address");
        listener.local_addr().expect("read local address")
    }

    #[test]
    fn endpoint_resolution_requests_dynamic_loopback_and_accepts_only_loopback_overrides() {
        let workspace = std::env::current_dir().expect("current directory");
        let first = RuntimeConnector::resolve_endpoint(&workspace, None).unwrap();
        let second = RuntimeConnector::resolve_endpoint(&workspace, None).unwrap();
        assert_eq!(first, "127.0.0.1:0".parse::<SocketAddr>().unwrap());
        assert_eq!(second, first);
        assert_eq!(
            RuntimeConnector::resolve_endpoint(&workspace, Some("127.0.0.1:4545")).unwrap(),
            "127.0.0.1:4545".parse::<SocketAddr>().unwrap()
        );
        assert!(RuntimeConnector::resolve_endpoint(&workspace, Some("not-an-address")).is_err());
        assert!(RuntimeConnector::resolve_endpoint(&workspace, Some("0.0.0.0:4545")).is_err());
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
    fn startup_lock_from_a_dead_process_is_recovered_immediately() {
        let directory = tempfile::tempdir().expect("runtime directory");
        let path = directory.path().join("runtime-test.lock");
        fs::write(&path, "pid=2147483647\n").expect("write abandoned lock");
        assert!(try_startup_lock(&path)
            .expect("inspect abandoned lock")
            .is_none());
        let lock = try_startup_lock(&path)
            .expect("retry after removing abandoned lock")
            .expect("dead owner lock should be replaced");
        assert!(path.exists());
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn connects_to_a_server_that_is_already_running_without_starting_another() {
        let config = test_config();
        let _listener = run_fake_server(config.address, "existing-test-instance");
        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let (connector, _directory) = test_connector(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
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
        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let (connector, _directory) = test_connector(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        let mut config = auto_config();
        let _client = connector.connect_or_start(&config).expect("runtime starts");
        config.address = connector.status().endpoint.expect("dynamic endpoint");
        assert!(launcher.started.load(Ordering::SeqCst));
        assert_eq!(connector.status().state, ConnectionState::Connected);
        assert!(connector.status().started_runtime);
        let metadata = match connector
            .inner
            .metadata_store
            .read(&config.workspace_root)
            .unwrap()
        {
            MetadataSnapshot::Valid(metadata, _) => metadata,
            other => panic!("expected published runtime metadata, got {other:?}"),
        };
        assert_eq!(metadata.endpoint, config.address.to_string());
        assert_eq!(metadata.pid, std::process::id());
        assert_eq!(metadata.protocol_version, RPC_PROTOCOL_VERSION);
        assert_eq!(metadata.transport, RPC_TRANSPORT_TCP_LOOPBACK);
    }

    #[test]
    fn startup_failure_is_reported_and_moves_the_connector_to_failed() {
        let config = test_config();
        let (connector, _directory) = test_connector(
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
        let (connector, _directory) = test_connector(
            Arc::new(TestLauncher::new(Duration::from_millis(40), false)),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );
        let config = auto_config();
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
        let launcher = Arc::new(TestLauncher::new(Duration::from_millis(140), false));
        let (connector, _directory) =
            test_connector(launcher, Duration::from_secs(2), Duration::from_millis(10));
        let config = auto_config();
        let _client = connector
            .connect_or_start(&config)
            .expect("slow server becomes ready");
        assert_eq!(connector.status().state, ConnectionState::Connected);
    }

    #[test]
    fn concurrent_clients_serialize_startup_until_the_listener_is_ready() {
        let first_launcher = Arc::new(TestLauncher::new(Duration::from_millis(180), false));
        let second_launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let directory = tempfile::tempdir().expect("shared runtime metadata directory");
        let first = RuntimeConnector::with_timing_and_runtime_directory(
            first_launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let second = RuntimeConnector::with_timing_and_runtime_directory(
            second_launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let config = auto_config();

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

    #[test]
    fn discovers_a_valid_running_server_from_metadata_without_starting_another() {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let first_launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let first = RuntimeConnector::with_timing_and_runtime_directory(
            first_launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let first_client = first
            .connect_or_start(&config)
            .expect("first runtime starts");
        let endpoint = first.status().endpoint.expect("published endpoint");

        let second_launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let second = RuntimeConnector::with_timing_and_runtime_directory(
            second_launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let _second_client = second
            .connect_or_start(&config)
            .expect("metadata points to the healthy runtime");
        assert_eq!(second.status().endpoint, Some(endpoint));
        assert!(!second_launcher.started.load(Ordering::SeqCst));
        drop(first_client);
    }

    #[test]
    fn stale_pid_metadata_is_removed_before_a_new_runtime_starts() {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let store = RuntimeMetadataStore::new(directory.path());
        store
            .write(
                &config.workspace_root,
                &RuntimeMetadata {
                    pid: 2_147_483_647,
                    protocol_version: RPC_PROTOCOL_VERSION,
                    transport: RPC_TRANSPORT_TCP_LOOPBACK.to_owned(),
                    endpoint: "127.0.0.1:1".to_owned(),
                    started_at: 1,
                    runtime_version: "0.0.0".to_owned(),
                    instance_id: "stale-instance".to_owned(),
                    metadata_version: RUNTIME_METADATA_FORMAT_VERSION,
                },
            )
            .unwrap();
        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let _client = connector.connect_or_start(&config).unwrap();
        assert!(launcher.started.load(Ordering::SeqCst));
        assert_ne!(
            connector.status().endpoint.unwrap().to_string(),
            "127.0.0.1:1"
        );
    }

    #[test]
    fn malformed_metadata_is_cleaned_and_missing_metadata_starts_a_runtime() {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let store = RuntimeMetadataStore::new(directory.path());
        fs::create_dir_all(store.directory()).unwrap();
        fs::write(store.metadata_path(&config.workspace_root), b"{ malformed").unwrap();
        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let _client = connector.connect_or_start(&config).unwrap();
        assert!(launcher.started.load(Ordering::SeqCst));
        assert!(matches!(
            store.read(&config.workspace_root).unwrap(),
            MetadataSnapshot::Valid(_, _)
        ));
    }

    #[test]
    fn live_pid_with_a_different_instance_id_is_treated_as_pid_reuse() {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let existing_endpoint = free_address();
        let _listener = run_fake_server(existing_endpoint, "actual-server-instance");
        let store = RuntimeMetadataStore::new(directory.path());
        let mut metadata = RuntimeMetadata::new(existing_endpoint, "stale-instance-id");
        metadata.pid = std::process::id();
        store.write(&config.workspace_root, &metadata).unwrap();

        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let _client = connector.connect_or_start(&config).unwrap();
        assert!(launcher.started.load(Ordering::SeqCst));
        assert_ne!(connector.status().endpoint, Some(existing_endpoint));
    }

    #[test]
    fn existing_process_with_an_unhealthy_rpc_endpoint_is_replaced() {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let unhealthy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = unhealthy_listener.local_addr().unwrap();
        let store = RuntimeMetadataStore::new(directory.path());
        store
            .write(
                &config.workspace_root,
                &RuntimeMetadata::new(endpoint, "unhealthy-instance"),
            )
            .unwrap();

        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            launcher.clone(),
            Duration::from_secs(5),
            Duration::from_millis(10),
            directory.path(),
        );
        let _client = connector.connect_or_start(&config).unwrap();
        assert!(launcher.started.load(Ordering::SeqCst));
        assert_ne!(connector.status().endpoint, Some(endpoint));
        drop(unhealthy_listener);
    }

    #[test]
    fn incompatible_metadata_protocol_is_removed_and_replaced() {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let store = RuntimeMetadataStore::new(directory.path());
        let mut metadata = RuntimeMetadata::new("127.0.0.1:1".parse().unwrap(), "old-protocol");
        metadata.protocol_version = RPC_PROTOCOL_VERSION + 1;
        store.write(&config.workspace_root, &metadata).unwrap();
        let launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let _client = connector.connect_or_start(&config).unwrap();
        assert!(launcher.started.load(Ordering::SeqCst));
        let MetadataSnapshot::Valid(current, _) = store.read(&config.workspace_root).unwrap()
        else {
            panic!("replacement metadata should be valid");
        };
        assert_eq!(current.protocol_version, RPC_PROTOCOL_VERSION);
    }
}
