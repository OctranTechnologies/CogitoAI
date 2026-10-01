//! Shared local runtime discovery, startup, and RPC connection management.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
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
// OS file locks coordinate separate CLI/desktop processes. Windows locks are
// process-scoped, so also serialize competing connector threads per lock path.
static PROCESS_STARTUP_LOCKS: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<AtomicBool>>>> =
    OnceLock::new();

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
    #[error(
        "timed out waiting for runtime startup lock for {endpoint}; lock file {lock_path}; recorded owner PID {owner_pid:?}"
    )]
    StartupLockTimeout {
        endpoint: SocketAddr,
        lock_path: PathBuf,
        owner_pid: Option<u32>,
    },
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
        let process_lock = process_startup_lock(&lock_path);
        let _process_startup_guard = loop {
            if process_lock
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break ProcessStartupGuard(Arc::clone(&process_lock));
            }
            if Instant::now() >= deadline {
                return self.failed(RuntimeConnectError::StartupLockTimeout {
                    endpoint: config.address,
                    lock_path: lock_path.clone(),
                    owner_pid: lock_owner_pid(&lock_path),
                });
            }
            thread::sleep(self.inner.retry_interval);
        };
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
                            lock_path: lock_path.clone(),
                            owner_pid: lock_owner_pid(&lock_path),
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
                return self.wait_until_ready_normalized(&config, None);
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
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    crate::metadata::set_private_file_mode(&mut options);
    let file = options.open(path).map_err(|error| {
        RuntimeConnectError::Startup(format!(
            "could not open runtime startup lock {}: {error}",
            path.display()
        ))
    })?;

    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let owner_path = lock_owner_path(path);
            let mut owner_options = OpenOptions::new();
            owner_options.write(true).create(true).truncate(true);
            crate::metadata::set_private_file_mode(&mut owner_options);
            let write_result = owner_options.open(&owner_path).and_then(|mut owner_file| {
                writeln!(owner_file, "pid={}", std::process::id())?;
                owner_file.sync_all()
            });
            if let Err(error) = write_result {
                let _ = FileExt::unlock(&file);
                return Err(RuntimeConnectError::Startup(format!(
                    "could not record runtime startup lock owner in {}: {error}",
                    path.display()
                )));
            }
            Ok(Some(StartupLock { _file: file }))
        }
        Err(error) if is_lock_contention(&error) => Ok(None),
        Err(error) => Err(RuntimeConnectError::Startup(format!(
            "could not acquire runtime startup lock {}: {error}",
            path.display()
        ))),
    }
}

fn is_lock_contention(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    match error.raw_os_error() {
        #[cfg(windows)]
        Some(32 | 33) => true, // sharing violation or lock violation
        #[cfg(unix)]
        Some(11 | 35) => true, // EAGAIN / EWOULDBLOCK
        _ => false,
    }
}

fn lock_owner_pid(path: &std::path::Path) -> Option<u32> {
    let mut file = File::open(lock_owner_path(path)).ok()?;
    let mut contents = String::new();
    Read::by_ref(&mut file)
        .take(256)
        .read_to_string(&mut contents)
        .ok()?;
    contents.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|pid| pid.parse::<u32>().ok())
    })
}

fn lock_owner_path(path: &std::path::Path) -> PathBuf {
    let mut owner_path = path.as_os_str().to_os_string();
    owner_path.push(".owner");
    PathBuf::from(owner_path)
}

fn process_startup_lock(path: &std::path::Path) -> Arc<AtomicBool> {
    let locks = PROCESS_STARTUP_LOCKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        locks
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(AtomicBool::new(false))),
    )
}

struct ProcessStartupGuard(Arc<AtomicBool>);

impl Drop for ProcessStartupGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The kernel lock is released when its file handle closes, including when the
/// owner process exits unexpectedly. Keep the lock file itself in place to
/// avoid unlink/recreate races between waiters.
struct StartupLock {
    _file: File,
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Barrier;

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

    struct ConcurrentLauncher {
        starts: Arc<AtomicUsize>,
        shutdown: Arc<AtomicBool>,
        server_threads: Mutex<Vec<thread::JoinHandle<()>>>,
        startup_delay: Duration,
    }

    impl ConcurrentLauncher {
        fn new(startup_delay: Duration) -> Self {
            Self {
                starts: Arc::new(AtomicUsize::new(0)),
                shutdown: Arc::new(AtomicBool::new(false)),
                server_threads: Mutex::new(Vec::new()),
                startup_delay,
            }
        }

        fn stop(&self) {
            self.shutdown.store(true, Ordering::SeqCst);
            for server in self
                .server_threads
                .lock()
                .expect("test server thread list lock")
                .drain(..)
            {
                server.join().expect("test server stops cleanly");
            }
        }
    }

    impl RuntimeLauncher for ConcurrentLauncher {
        fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let listener = TcpListener::bind(config.address).map_err(|error| error.to_string())?;
            listener
                .set_nonblocking(true)
                .map_err(|error| error.to_string())?;
            let endpoint = listener.local_addr().map_err(|error| error.to_string())?;
            let instance_id = config
                .instance_id
                .clone()
                .ok_or_else(|| "test launch requires an instance ID".to_owned())?;
            let server_instance_id = instance_id.clone();
            let shutdown = Arc::clone(&self.shutdown);
            let startup_delay = self.startup_delay;
            let server = thread::spawn(move || {
                thread::sleep(startup_delay);
                while !shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let instance_id = server_instance_id.clone();
                            thread::spawn(move || serve_test_connection(stream, instance_id));
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => {
                            eprintln!("concurrent test server accept failed: {error}");
                            break;
                        }
                    }
                }
            });
            self.server_threads
                .lock()
                .expect("test server thread list lock")
                .push(server);
            Ok(RuntimeLaunchInfo {
                endpoint,
                instance_id,
            })
        }
    }

    struct ReadinessFailLauncher {
        starts: AtomicUsize,
        listeners: Mutex<Vec<TcpListener>>,
    }

    impl ReadinessFailLauncher {
        fn new() -> Self {
            Self {
                starts: AtomicUsize::new(0),
                listeners: Mutex::new(Vec::new()),
            }
        }
    }

    impl RuntimeLauncher for ReadinessFailLauncher {
        fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let listener = TcpListener::bind(config.address).map_err(|error| error.to_string())?;
            let endpoint = listener.local_addr().map_err(|error| error.to_string())?;
            let instance_id = config
                .instance_id
                .clone()
                .ok_or_else(|| "test launch requires an instance ID".to_owned())?;
            self.listeners
                .lock()
                .expect("test listener lock")
                .push(listener);
            Ok(RuntimeLaunchInfo {
                endpoint,
                instance_id,
            })
        }
    }

    struct ManualStartupRaceLauncher {
        starts: AtomicUsize,
        delay: Duration,
    }

    impl RuntimeLauncher for ManualStartupRaceLauncher {
        fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            // Simulate a manually started server winning the port bind after
            // the automatic client's initial health check.
            let listener = TcpListener::bind(config.address).map_err(|error| error.to_string())?;
            let instance_id = config
                .instance_id
                .clone()
                .ok_or_else(|| "test launch requires an instance ID".to_owned())?;
            let delay = self.delay;
            thread::spawn(move || {
                thread::sleep(delay);
                if let Some(Ok(stream)) = listener.incoming().next() {
                    serve_test_connection(stream, instance_id);
                }
            });
            Err("address already in use".to_owned())
        }
    }

    struct ProcessCountingLauncher {
        launch_log: PathBuf,
    }

    impl RuntimeLauncher for ProcessCountingLauncher {
        fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String> {
            let listener = TcpListener::bind(config.address).map_err(|error| error.to_string())?;
            let endpoint = listener.local_addr().map_err(|error| error.to_string())?;
            let instance_id = config
                .instance_id
                .clone()
                .ok_or_else(|| "test launch requires an instance ID".to_owned())?;
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.launch_log)
                .map_err(|error| error.to_string())?;
            writeln!(log, "pid={}", std::process::id()).map_err(|error| error.to_string())?;
            log.sync_all().map_err(|error| error.to_string())?;
            let server_instance_id = instance_id.clone();
            thread::spawn(move || {
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
        // On Windows a stream accepted from a nonblocking listener can retain
        // that mode, unlike Unix. The test RPC handler requires blocking I/O.
        if stream.set_nonblocking(false).is_err() {
            return;
        }
        let Ok(cloned) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(cloned);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
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
    fn startup_lock_releases_when_holder_process_exits_abruptly() {
        const CRASH_LOCK_PATH: &str = "COGITO_TEST_STARTUP_LOCK_CRASH_PATH";
        const HOLD_LOCK_PATH: &str = "COGITO_TEST_STARTUP_LOCK_HOLD_PATH";
        if let Some(path) = std::env::var_os(CRASH_LOCK_PATH) {
            let _lock = try_startup_lock(std::path::Path::new(&path))
                .expect("child acquires startup lock")
                .expect("startup lock is not already held");
            // Exit without unwinding or running StartupLock destructors. The
            // operating system must release the lock as it closes our handle.
            std::process::exit(73);
        }
        if let Some(path) = std::env::var_os(HOLD_LOCK_PATH) {
            let _lock = try_startup_lock(std::path::Path::new(&path))
                .expect("child acquires startup lock")
                .expect("startup lock is not already held");
            thread::sleep(Duration::from_millis(700));
            return;
        }

        let directory = tempfile::tempdir().expect("runtime directory");
        let path = directory.path().join("runtime-test.lock");
        let child = spawn_lock_test_process(CRASH_LOCK_PATH, &path)
            .status()
            .expect("start child lock holder");
        assert_eq!(child.code(), Some(73));

        let lock = try_startup_lock(&path)
            .expect("lock is recoverable after process exit")
            .expect("OS released crashed process lock");
        drop(lock);
        assert!(path.exists(), "lock file remains but is not locked");
    }

    #[test]
    fn startup_lock_wait_is_bounded_and_reports_owner_diagnostics() {
        let directory = tempfile::tempdir().expect("runtime directory");
        let config = auto_config();
        let store = RuntimeMetadataStore::new(directory.path());
        store.ensure_private_directory().unwrap();
        let normalized = normalized_config(&config).unwrap();
        let lock_path = store.startup_lock_path(&normalized.workspace_root);
        let mut child = spawn_lock_test_process(HOLD_LOCK_PATH, &lock_path)
            .spawn()
            .expect("start lock holder");

        let wait_deadline = Instant::now() + Duration::from_secs(2);
        while lock_owner_pid(&lock_path).is_none() && Instant::now() < wait_deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let owner_pid = lock_owner_pid(&lock_path).expect("lock owner diagnostic was written");
        assert!(
            try_startup_lock(&lock_path)
                .expect("probe held startup lock")
                .is_none(),
            "child PID {owner_pid} should hold {}",
            lock_path.display()
        );
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            Arc::new(TestLauncher::new(Duration::ZERO, false)),
            Duration::from_millis(120),
            Duration::from_millis(10),
            directory.path(),
        );
        let started = Instant::now();
        let error = match connector.connect_or_start(&config) {
            Ok(_) => panic!("held startup lock should time out"),
            Err(error) => error,
        };
        assert!(started.elapsed() < Duration::from_secs(1));
        let message = error.to_string();
        assert!(message.contains(&lock_path.display().to_string()));
        assert!(message.contains(&format!("Some({owner_pid})")));
        assert_eq!(connector.status().state, ConnectionState::Failed);

        assert!(child.wait().expect("lock holder exits").success());
    }

    const HOLD_LOCK_PATH: &str = "COGITO_TEST_STARTUP_LOCK_HOLD_PATH";

    fn spawn_lock_test_process(variable: &str, path: &std::path::Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "bootstrap::tests::startup_lock_releases_when_holder_process_exits_abruptly",
            ])
            .env(variable, path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
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
    fn manual_server_and_automatic_runtime_coexist_on_distinct_endpoints() {
        let automatic_config = auto_config();
        let manual_endpoint = free_address();
        let _manual_server = run_fake_server(manual_endpoint, "manual-server-instance");
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let launcher = Arc::new(ConcurrentLauncher::new(Duration::ZERO));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            launcher.clone(),
            Duration::from_secs(3),
            Duration::from_millis(10),
            directory.path(),
        );
        let manual_config =
            RuntimeLaunchConfig::new(manual_endpoint, automatic_config.workspace_root.clone());

        let _manual_client = connector
            .connect_or_start(&manual_config)
            .expect("explicit manual endpoint is reused");
        assert_eq!(launcher.starts.load(Ordering::SeqCst), 0);

        let _automatic_client = connector
            .connect_or_start(&automatic_config)
            .expect("automatic runtime starts independently");
        assert_ne!(connector.status().endpoint, Some(manual_endpoint));
        assert_eq!(launcher.starts.load(Ordering::SeqCst), 1);

        let _manual_again = connector
            .connect_existing(&manual_config)
            .expect("manual endpoint remains available");
        assert_eq!(launcher.starts.load(Ordering::SeqCst), 1);
        launcher.stop();
    }

    #[test]
    fn automatic_start_waits_for_a_manual_server_that_wins_the_port_race() {
        let config = test_config();
        let launcher = Arc::new(ManualStartupRaceLauncher {
            starts: AtomicUsize::new(0),
            delay: Duration::from_millis(80),
        });
        let (connector, _directory) = test_connector(
            launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
        );

        let client = connector
            .connect_or_start(&config)
            .expect("automatic client waits for the manual endpoint to become ready");
        assert_eq!(connector.status().state, ConnectionState::Connected);
        assert!(!connector.status().started_runtime);
        assert_eq!(launcher.starts.load(Ordering::SeqCst), 1);
        drop(client);
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
        let (connector, directory) = test_connector(
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
        let lock_path = connector
            .inner
            .metadata_store
            .startup_lock_path(&config.workspace_root);
        let lock = try_startup_lock(&lock_path)
            .expect("startup failure releases the kernel lock")
            .expect("another client can retry after startup failure");
        drop(lock);
        drop(directory);
    }

    #[test]
    fn readiness_failure_releases_the_lock_and_later_clients_can_retry() {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("runtime metadata directory");
        let failed_launcher = Arc::new(ReadinessFailLauncher::new());
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            failed_launcher.clone(),
            Duration::from_millis(120),
            Duration::from_millis(10),
            directory.path(),
        );
        let error = match connector.connect_or_start(&config) {
            Ok(_) => panic!("server that never responds should fail readiness"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            RuntimeConnectError::ReadinessTimeout { .. }
        ));
        assert_eq!(failed_launcher.starts.load(Ordering::SeqCst), 1);

        let lock_path = connector
            .inner
            .metadata_store
            .startup_lock_path(&config.workspace_root);
        let lock = try_startup_lock(&lock_path)
            .expect("readiness failure releases the kernel lock")
            .expect("another client can retry after readiness failure");
        drop(lock);

        let healthy_launcher = Arc::new(TestLauncher::new(Duration::ZERO, false));
        let healthy_connector = RuntimeConnector::with_timing_and_runtime_directory(
            healthy_launcher.clone(),
            Duration::from_secs(2),
            Duration::from_millis(10),
            directory.path(),
        );
        let _client = healthy_connector
            .connect_or_start(&config)
            .expect("later client replaces stale failed runtime and starts");
        assert!(healthy_launcher.started.load(Ordering::SeqCst));
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
        for _ in 0..3 {
            run_concurrent_start_case(2);
            run_concurrent_start_case(10);
        }
    }

    #[test]
    fn separate_client_processes_start_only_one_runtime() {
        const CLIENT_ENV: &str = "COGITO_TEST_CONCURRENT_CLIENT";
        const RUNTIME_DIR_ENV: &str = "COGITO_TEST_RUNTIME_DIR";
        const LAUNCH_LOG_ENV: &str = "COGITO_TEST_LAUNCH_LOG";
        const WORKSPACE_ENV: &str = "COGITO_TEST_WORKSPACE";
        const START_GATE_ENV: &str = "COGITO_TEST_START_GATE";
        const READY_DIR_ENV: &str = "COGITO_TEST_READY_DIR";
        const SERVER_HOLD_MS_ENV: &str = "COGITO_TEST_SERVER_HOLD_MS";

        if std::env::var_os(CLIENT_ENV).is_some() {
            let runtime_directory = PathBuf::from(std::env::var_os(RUNTIME_DIR_ENV).unwrap());
            let launch_log = PathBuf::from(std::env::var_os(LAUNCH_LOG_ENV).unwrap());
            let workspace = PathBuf::from(std::env::var_os(WORKSPACE_ENV).unwrap());
            let start_gate = PathBuf::from(std::env::var_os(START_GATE_ENV).unwrap());
            let ready_directory = PathBuf::from(std::env::var_os(READY_DIR_ENV).unwrap());
            let server_hold = std::env::var(SERVER_HOLD_MS_ENV)
                .unwrap()
                .parse::<u64>()
                .expect("server hold duration");
            fs::write(
                ready_directory.join(std::process::id().to_string()),
                "ready",
            )
            .expect("record simulated client ready");
            let gate_deadline = Instant::now() + Duration::from_secs(10);
            while !start_gate.exists() {
                assert!(
                    Instant::now() < gate_deadline,
                    "parent did not release start gate"
                );
                thread::sleep(Duration::from_millis(5));
            }

            let connector = RuntimeConnector::with_timing_and_runtime_directory(
                Arc::new(ProcessCountingLauncher { launch_log }),
                Duration::from_secs(8),
                Duration::from_millis(10),
                runtime_directory,
            );
            let config = RuntimeLaunchConfig::new(
                "127.0.0.1:0".parse().expect("loopback endpoint"),
                workspace,
            );
            let _client = connector
                .connect_or_start(&config)
                .expect("separate client process connects to the single runtime");
            thread::sleep(Duration::from_millis(server_hold));
            return;
        }

        for client_count in [2, 10] {
            let directory = tempfile::tempdir().expect("shared process runtime directory");
            let gate = directory.path().join("start.gate");
            let ready_directory = directory.path().join("ready");
            fs::create_dir_all(&ready_directory).expect("create process readiness directory");
            let launch_log = directory.path().join("launches.log");
            let workspace = std::env::current_dir().expect("current workspace");
            let mut children = Vec::new();
            for _ in 0..client_count {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "bootstrap::tests::separate_client_processes_start_only_one_runtime",
                    ])
                    .env(CLIENT_ENV, "1")
                    .env(RUNTIME_DIR_ENV, directory.path())
                    .env(LAUNCH_LOG_ENV, &launch_log)
                    .env(WORKSPACE_ENV, &workspace)
                    .env(START_GATE_ENV, &gate)
                    .env(READY_DIR_ENV, &ready_directory)
                    .env(
                        SERVER_HOLD_MS_ENV,
                        if client_count == 2 { "700" } else { "5000" },
                    )
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                children.push(command.spawn().expect("start simulated client process"));
            }
            let ready_deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let ready_count = fs::read_dir(&ready_directory)
                    .expect("read simulated client readiness markers")
                    .count();
                if ready_count == client_count {
                    break;
                }
                assert!(
                    Instant::now() < ready_deadline,
                    "only {ready_count} of {client_count} clients reached the start barrier"
                );
                thread::sleep(Duration::from_millis(10));
            }
            fs::write(&gate, "go").expect("release all simulated clients together");
            for mut child in children {
                assert!(child.wait().expect("simulated client exits").success());
            }
            let starts = fs::read_to_string(&launch_log)
                .expect("runtime launch was recorded")
                .lines()
                .count();
            assert_eq!(
                starts, 1,
                "{client_count} separate clients must start exactly one runtime"
            );
        }
    }

    fn run_concurrent_start_case(client_count: usize) {
        let config = auto_config();
        let directory = tempfile::tempdir().expect("shared runtime metadata directory");
        let launcher = Arc::new(ConcurrentLauncher::new(Duration::from_millis(80)));
        let barrier = Arc::new(Barrier::new(client_count + 1));
        let connectors = (0..client_count)
            .map(|_| {
                RuntimeConnector::with_timing_and_runtime_directory(
                    launcher.clone(),
                    Duration::from_secs(8),
                    Duration::from_millis(5),
                    directory.path(),
                )
            })
            .collect::<Vec<_>>();

        let clients = connectors
            .into_iter()
            .map(|connector| {
                let config = config.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    connector
                        .connect_or_start(&config)
                        .map(|client| {
                            (
                                client,
                                connector.status().endpoint.expect("connected endpoint"),
                            )
                        })
                        .map_err(|error| error.to_string())
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();

        let results = clients
            .into_iter()
            .map(|client| client.join().expect("connector thread completes"))
            .collect::<Vec<_>>();
        let errors = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "{client_count} concurrent clients failed: {errors:?}; runtime starts: {}",
            launcher.starts.load(Ordering::SeqCst)
        );
        let clients = results.into_iter().map(Result::unwrap).collect::<Vec<_>>();
        let shared_endpoint = clients[0].1;
        assert!(clients
            .iter()
            .all(|(_, endpoint)| *endpoint == shared_endpoint));
        assert_eq!(
            launcher.starts.load(Ordering::SeqCst),
            1,
            "{client_count} simultaneous clients must start exactly one runtime"
        );
        drop(clients);
        launcher.stop();
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
