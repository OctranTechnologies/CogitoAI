//! Locates and launches the persistent, detached local RPC runtime process.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::host::{RuntimeLaunchConfig, RuntimeLaunchInfo, RuntimeLauncher};
use crate::metadata::{set_private_file_mode, RuntimeMetadataStore};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(12);

/// Starts the shared runtime as a detached child process and leaves it running
/// when this client disconnects. Binary discovery is independent of CWD.
pub struct ProcessRuntimeLauncher {
    executable: Option<PathBuf>,
    search_directories: Vec<PathBuf>,
    runtime_directory: PathBuf,
    startup_timeout: Duration,
    arguments: Vec<std::ffi::OsString>,
    children: Mutex<HashMap<String, Child>>,
}

impl Default for ProcessRuntimeLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessRuntimeLauncher {
    pub fn new() -> Self {
        Self {
            executable: None,
            search_directories: Vec::new(),
            runtime_directory: RuntimeMetadataStore::for_current_user_or_fallback()
                .directory()
                .to_path_buf(),
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            arguments: Vec::new(),
            children: Mutex::new(HashMap::new()),
        }
    }

    /// Pins a binary path, useful for an application host that resolves its
    /// packaged sidecar through its own resource API.
    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    /// Adds a packaged-resource or development-build directory to discovery.
    pub fn with_search_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.search_directories.push(directory.into());
        self
    }

    /// Uses a private runtime directory for logs and tests.
    pub fn with_runtime_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.runtime_directory = directory.into();
        self
    }

    /// Overrides the child startup handshake deadline.
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    fn locate_executable(&self) -> Result<PathBuf, String> {
        let environment_executable = std::env::var_os("COGITO_RUNTIME_BINARY").map(PathBuf::from);
        if let Some(executable) = self.executable.as_ref().or(environment_executable.as_ref()) {
            if !executable.is_absolute() {
                return Err(format!(
                    "configured Harness runtime path must be absolute: {}",
                    executable.display()
                ));
            }
            return checked_executable(executable).ok_or_else(|| {
                format!(
                    "configured Harness runtime executable does not exist: {}",
                    executable.display()
                )
            });
        }

        let mut directories = self.search_directories.clone();
        if let Ok(current_executable) = std::env::current_exe() {
            let mut ancestor = current_executable.parent();
            for _ in 0..6 {
                let Some(directory) = ancestor else { break };
                directories.push(directory.to_path_buf());
                directories.push(directory.join("binaries"));
                directories.push(directory.join("resources"));
                directories.push(directory.join("Resources"));
                ancestor = directory.parent();
            }
        }
        let mut visited = HashSet::new();
        directories.retain(|directory| visited.insert(directory.clone()));

        let binary_name = runtime_binary_name();
        let target_name = option_env!("TAURI_ENV_TARGET_TRIPLE")
            .map(|triple| format!("cogito-harness-runtime-{triple}{}", executable_extension()));
        for directory in &directories {
            for name in [Some(binary_name.to_owned()), target_name.clone()]
                .into_iter()
                .flatten()
            {
                if let Some(path) = checked_executable(&directory.join(name)) {
                    return Ok(path);
                }
            }
            if let Some(path) = find_sidecar_in(directory) {
                return Ok(path);
            }
        }

        Err(format!(
            "could not locate the Harness runtime executable; searched beside the current executable and in {}. Build the workspace with `cargo build --workspace` or set COGITO_RUNTIME_BINARY",
            directories
                .iter()
                .map(|directory| directory.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    fn create_log(&self, instance_id: &str) -> Result<(PathBuf, File), String> {
        let store = RuntimeMetadataStore::new(self.runtime_directory.clone());
        store
            .ensure_private_directory()
            .map_err(|error| format!("could not prepare runtime log directory: {error}"))?;
        let safe_id = instance_id
            .chars()
            .filter(char::is_ascii_hexdigit)
            .take(48)
            .collect::<String>();
        let path = self
            .runtime_directory
            .join(format!("runtime-{safe_id}.log"));
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        set_private_file_mode(&mut options);
        let file = options
            .open(&path)
            .map_err(|error| format!("could not open runtime log {}: {error}", path.display()))?;
        Ok((path, file))
    }

    fn terminate_child(&self, instance_id: &str) {
        let child = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(instance_id);
        if let Some(mut child) = child {
            terminate_and_reap(&mut child);
        }
    }
}

impl RuntimeLauncher for ProcessRuntimeLauncher {
    fn start_runtime(&self, config: &RuntimeLaunchConfig) -> Result<RuntimeLaunchInfo, String> {
        let instance_id = config
            .instance_id
            .as_deref()
            .ok_or_else(|| "runtime launch requires an instance ID".to_owned())?;
        let executable = self.locate_executable()?;
        let (log_path, log_file) = self.create_log(instance_id)?;
        let stderr = log_file
            .try_clone()
            .map_err(|error| format!("could not redirect runtime stderr: {error}"))?;
        let stdout_log = log_file
            .try_clone()
            .map_err(|error| format!("could not redirect runtime stdout: {error}"))?;
        let safe_id = instance_id
            .chars()
            .filter(char::is_ascii_hexdigit)
            .take(48)
            .collect::<String>();
        let ready_path = self
            .runtime_directory
            .join(format!("runtime-{safe_id}.ready"));
        let _ = fs::remove_file(&ready_path);
        let _ = fs::remove_file(ready_path.with_extension("ready.tmp"));

        let mut command = Command::new(&executable);
        command
            .args(&self.arguments)
            .current_dir(&config.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr));
        command.env("COGITO_RUNTIME_READY_FILE", &ready_path);
        configure_detached_process(&mut command);
        // The process inherits environment credentials needed by its model
        // provider, but no secret is copied into its command-line arguments.
        #[cfg(windows)]
        let spawn_result = {
            let _inheritance = windows_handle_inheritance::suppress().map_err(|error| {
                format!("could not isolate runtime child standard handles: {error}")
            })?;
            command.spawn()
        };
        #[cfg(not(windows))]
        let spawn_result = command.spawn();
        let mut child = spawn_result.map_err(|error| {
            format!(
                "could not start Harness runtime {}: {error}; runtime log: {}",
                executable.display(),
                log_path.display()
            )
        })?;

        let payload = match serde_json::to_vec(config) {
            Ok(payload) => payload,
            Err(error) => {
                terminate_and_reap(&mut child);
                return Err(format!(
                    "could not serialize runtime configuration: {error}"
                ));
            }
        };
        let write_result = child
            .stdin
            .take()
            .ok_or_else(|| "runtime child did not expose its startup input".to_owned())
            .and_then(|mut stdin| {
                stdin
                    .write_all(&payload)
                    .and_then(|()| stdin.flush())
                    .map_err(|error| {
                        format!("could not send runtime startup configuration: {error}")
                    })
            });
        if let Err(error) = write_result {
            terminate_and_reap(&mut child);
            return Err(format!("{error}; runtime log: {}", log_path.display()));
        }

        let launch =
            match wait_for_startup_announcement(&mut child, &ready_path, self.startup_timeout) {
                Ok(launch) => launch,
                Err(error) => {
                    let status = terminate_and_reap(&mut child);
                    let _ = fs::remove_file(&ready_path);
                    let _ = fs::remove_file(ready_path.with_extension("ready.tmp"));
                    return Err(format!(
                    "runtime did not announce readiness ({error}; exit {status}); runtime log: {}",
                    log_path.display()
                ));
                }
            };
        let _ = fs::remove_file(&ready_path);
        let _ = fs::remove_file(ready_path.with_extension("ready.tmp"));

        if launch.instance_id != instance_id || launch.pid != child.id() {
            let status = terminate_and_reap(&mut child);
            return Err(format!(
                "runtime startup identity did not match the launch request (exit {status}); runtime log: {}",
                log_path.display()
            ));
        }
        if !launch.endpoint.ip().is_loopback() || launch.endpoint.port() == 0 {
            let status = terminate_and_reap(&mut child);
            return Err(format!(
                "runtime announced an invalid endpoint (exit {status}); runtime log: {}",
                log_path.display()
            ));
        }
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(instance_id.to_owned(), child);
        Ok(launch)
    }

    fn startup_succeeded(&self, instance_id: &str) {
        let child = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(instance_id);
        if let Some(mut child) = child {
            // Dropping a std::process::Child does not terminate the process;
            // this waiter reaps it if it later exits while this client lives.
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    fn startup_failed(&self, instance_id: &str) {
        self.terminate_child(instance_id);
    }
}

impl Drop for ProcessRuntimeLauncher {
    fn drop(&mut self) {
        let children = self
            .children
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for child in children.values_mut() {
            terminate_and_reap(child);
        }
        children.clear();
    }
}

fn wait_for_startup_announcement(
    child: &mut Child,
    ready_path: &Path,
    timeout: Duration,
) -> Result<RuntimeLaunchInfo, String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut backoff = Duration::from_millis(10);
    loop {
        match fs::read(ready_path) {
            Ok(bytes) => {
                return serde_json::from_slice::<RuntimeLaunchInfo>(&bytes)
                    .map_err(|error| format!("invalid runtime readiness announcement: {error}"));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("could not read runtime readiness file: {error}"));
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("could not inspect runtime process: {error}"))?
        {
            return Err(format!("runtime exited before readiness (exit {status})"));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(format!("startup announcement timed out after {timeout:?}"));
        }
        thread::sleep(backoff.min(remaining));
        backoff = backoff.saturating_mul(2).min(Duration::from_millis(100));
    }
}

fn terminate_and_reap(child: &mut Child) -> String {
    let _ = child.kill();
    match child.wait() {
        Ok(status) => status.to_string(),
        Err(error) => format!("wait failed: {error}"),
    }
}

fn checked_executable(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}

fn runtime_binary_name() -> String {
    format!("cogito-harness-runtime{}", executable_extension())
}

fn executable_extension() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

fn find_sidecar_in(directory: &Path) -> Option<PathBuf> {
    let prefix = "cogito-harness-runtime-";
    let mut candidates = fs::read_dir(directory)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
                && (executable_extension().is_empty()
                    || path.extension().is_some_and(|extension| extension == "exe"))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let arch_match = name.contains(std::env::consts::ARCH);
        let os_match = name.contains(std::env::consts::OS)
            || (cfg!(windows) && name.contains("windows"))
            || (cfg!(target_os = "macos") && name.contains("apple-darwin"));
        (!arch_match, !os_match)
    });
    candidates.into_iter().next()
}

#[cfg(unix)]
fn configure_detached_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_handle_inheritance {
    use std::io;

    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// Temporarily prevents a detached runtime from retaining its client's
    /// parent-captured stdout/stderr handles. The handles explicitly supplied
    /// through `Command::stdin/stdout/stderr` remain inheritable for the spawn.
    pub(super) struct Guard {
        handles: Vec<HANDLE>,
    }

    pub(super) fn suppress() -> io::Result<Guard> {
        let mut guard = Guard {
            handles: Vec::new(),
        };
        for standard_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle accepts the documented standard-handle IDs.
            let handle = unsafe { GetStdHandle(standard_handle) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut flags = 0;
            // SAFETY: `handle` is a non-null process standard handle and `flags`
            // points to a writable DWORD for the duration of the call.
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if flags & HANDLE_FLAG_INHERIT != 0 {
                // SAFETY: This only changes the inherit flag for this valid
                // process-owned handle; Guard restores it when dropped.
                if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                guard.handles.push(handle);
            }
        }
        Ok(guard)
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            for handle in &self.handles {
                // SAFETY: Each handle was valid when its inherit flag was
                // cleared and remains owned by this process until spawn ends.
                let _ = unsafe {
                    SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
                };
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn configure_detached_process(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RpcClient, RuntimeConnector};
    use serde_json::{json, Value};
    use std::io::BufRead;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    fn fixture_launcher(directory: &Path, timeout: Duration) -> ProcessRuntimeLauncher {
        ProcessRuntimeLauncher::new()
            .with_executable(std::env::current_exe().expect("test executable"))
            .with_arguments([
                "--ignored",
                "--exact",
                "process::tests::child_process_fixture",
                "--nocapture",
            ])
            .with_runtime_directory(directory.join("runtime-data"))
            .with_startup_timeout(timeout)
    }

    fn launch_config(workspace: &Path) -> RuntimeLaunchConfig {
        let mut config = RuntimeLaunchConfig::new("127.0.0.1:0".parse().unwrap(), workspace);
        config.instance_id = Some("fixture-instance".to_owned());
        config
    }

    fn set_mode(directory: &Path, mode: &str) -> PathBuf {
        let workspace = directory.join("workspace");
        fs::create_dir_all(&workspace).expect("fixture workspace");
        fs::write(workspace.join(".runtime-fixture-mode"), mode).expect("fixture mode");
        workspace
    }

    fn fixture_pid(workspace: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(pid) = fs::read_to_string(workspace.join(".runtime-fixture-pid")) {
                return pid.parse().expect("fixture PID");
            }
            assert!(Instant::now() < deadline, "fixture did not record its PID");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_process_exited(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::metadata::process_exists(pid) != Some(false) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(crate::metadata::process_exists(pid), Some(false));
    }

    #[test]
    fn executable_missing_is_reported_before_spawn() {
        let directory = tempfile::tempdir().expect("temporary runtime dir");
        let launcher = ProcessRuntimeLauncher::new()
            .with_executable(directory.path().join("missing-runtime.exe"));
        let workspace = set_mode(directory.path(), "healthy");
        let error = launcher
            .start_runtime(&launch_config(&workspace))
            .expect_err("missing executable must fail");
        assert!(error.contains("missing-runtime.exe"));
    }

    #[test]
    fn relative_executable_override_is_rejected_instead_of_using_cwd() {
        let directory = tempfile::tempdir().expect("temporary runtime dir");
        let workspace = set_mode(directory.path(), "healthy");
        let launcher = ProcessRuntimeLauncher::new().with_executable("runtime.exe");
        let error = launcher
            .start_runtime(&launch_config(&workspace))
            .expect_err("relative executable paths would depend on CWD");
        assert!(error.contains("must be absolute"));
    }

    #[test]
    fn packaged_sidecar_discovery_handles_spaces_and_non_ascii_paths() {
        let directory = tempfile::tempdir().expect("temporary package dir");
        let resource_directory = directory.path().join("应用 bundle with spaces");
        fs::create_dir_all(&resource_directory).expect("resource directory");
        let sidecar_name = format!(
            "cogito-harness-runtime-{}-{}{}",
            std::env::consts::ARCH,
            std::env::consts::OS,
            executable_extension()
        );
        let sidecar = resource_directory.join(sidecar_name);
        fs::copy(std::env::current_exe().expect("test executable"), &sidecar)
            .expect("copy executable fixture");

        let located = ProcessRuntimeLauncher::new()
            .with_search_directory(&resource_directory)
            .locate_executable()
            .expect("sidecar is found in the packaged resource directory");

        assert_eq!(located, sidecar);
    }

    #[test]
    fn child_crash_before_readiness_is_reported_and_reaped() {
        let directory = tempfile::tempdir().expect("temporary runtime dir");
        let workspace = set_mode(directory.path(), "crash");
        let launcher = fixture_launcher(directory.path(), Duration::from_secs(2));
        let error = launcher
            .start_runtime(&launch_config(&workspace))
            .expect_err("crashing child must fail startup");
        assert!(error.contains("exited before readiness"));
        assert_process_exited(fixture_pid(&workspace));
    }

    #[test]
    fn child_that_never_announces_readiness_is_killed_at_deadline() {
        let directory = tempfile::tempdir().expect("temporary runtime dir");
        let workspace = set_mode(directory.path(), "hang");
        let launcher = fixture_launcher(directory.path(), Duration::from_millis(250));
        let error = launcher
            .start_runtime(&launch_config(&workspace))
            .expect_err("hung child must hit its startup deadline");
        assert!(error.contains("announcement timed out"));
        assert_process_exited(fixture_pid(&workspace));
    }

    #[test]
    fn delayed_rpc_readiness_uses_backoff_and_succeeds() {
        let directory = tempfile::tempdir().expect("temporary runtime dir");
        let workspace = set_mode(directory.path(), "slow");
        let runtime_directory = directory.path().join("connector-runtime");
        let launcher = fixture_launcher(directory.path(), Duration::from_secs(2));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            Arc::new(launcher),
            Duration::from_secs(3),
            Duration::from_millis(10),
            runtime_directory,
        );
        let mut client = match connector.connect_or_start(&launch_config(&workspace)) {
            Ok(client) => client,
            Err(error) => {
                thread::sleep(Duration::from_millis(30));
                let log_path = directory
                    .path()
                    .join("runtime-data")
                    .join("runtime-fixtureinstance.log");
                panic!(
                    "slow runtime should become ready: {error}; log: {}",
                    fs::read_to_string(log_path).unwrap_or_default(),
                );
            }
        };
        let health = client
            .request("health/check", json!({}))
            .expect("health request")
            .result
            .expect("health response");
        assert_eq!(health["status"], "ready");
        let instance_id = health["instanceId"].as_str().unwrap().to_owned();
        drop(client);
        let mut admin =
            RpcClient::connect_timeout(health_endpoint(&connector), Duration::from_secs(3))
                .expect("connect for fixture shutdown");
        let shutdown = admin.request("rpc.shutdown", json!({"instanceId": instance_id}));
        assert!(shutdown.is_ok(), "fixture shutdown: {:?}", shutdown.err());
    }

    fn health_endpoint(connector: &RuntimeConnector) -> SocketAddr {
        connector.status().endpoint.expect("runtime endpoint")
    }

    #[test]
    fn readiness_timeout_kills_unresponsive_child() {
        let directory = tempfile::tempdir().expect("temporary runtime dir");
        let workspace = set_mode(directory.path(), "no-response");
        let runtime_directory = directory.path().join("connector-runtime");
        let launcher = fixture_launcher(directory.path(), Duration::from_secs(2));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            Arc::new(launcher),
            Duration::from_millis(350),
            Duration::from_millis(10),
            runtime_directory,
        );
        let error = connector
            .connect_or_start(&launch_config(&workspace))
            .err()
            .expect("unresponsive child must fail readiness");
        assert!(error
            .to_string()
            .contains("timed out waiting for the runtime"));
        assert_process_exited(fixture_pid(&workspace));
    }

    #[test]
    fn incompatible_child_is_reported_once_and_not_relaunched() {
        let directory = tempfile::tempdir().expect("temporary runtime dir");
        let workspace = set_mode(directory.path(), "incompatible");
        let runtime_directory = directory.path().join("connector-runtime");
        let metadata_store = crate::metadata::RuntimeMetadataStore::new(&runtime_directory);
        let launcher: Arc<dyn RuntimeLauncher> =
            Arc::new(fixture_launcher(directory.path(), Duration::from_secs(2)));
        let connector = RuntimeConnector::with_timing_and_runtime_directory(
            Arc::clone(&launcher),
            Duration::from_secs(2),
            Duration::from_millis(10),
            runtime_directory.clone(),
        );
        let error = connector
            .connect_or_start(&launch_config(&workspace))
            .err()
            .expect("incompatible protocol must fail readiness");
        assert!(error.to_string().contains("protocol mismatch"));
        let pid = fixture_pid(&workspace);
        let process_cleanup = FixtureProcessCleanup {
            pid,
            stop_file: workspace.join(".runtime-fixture-stop"),
        };
        assert_eq!(crate::metadata::process_exists(pid), Some(true));

        let retry = connector
            .connect_or_start(&launch_config(&workspace))
            .err()
            .expect("later clients must see the existing protocol mismatch");
        assert!(retry.to_string().contains("protocol mismatch"));
        assert_eq!(
            fixture_pid(&workspace),
            pid,
            "incompatible child was relaunched"
        );

        let crate::metadata::MetadataSnapshot::Valid(metadata, _) = metadata_store
            .read(&workspace)
            .expect("incompatible runtime metadata remains available")
        else {
            panic!("incompatible runtime metadata must remain on disk");
        };
        assert_eq!(metadata.protocol_version, crate::RPC_PROTOCOL_VERSION);
        drop(process_cleanup);
        assert_process_exited(fixture_pid(&workspace));
    }

    struct FixtureProcessCleanup {
        pid: u32,
        stop_file: PathBuf,
    }

    impl Drop for FixtureProcessCleanup {
        fn drop(&mut self) {
            let _ = fs::write(&self.stop_file, b"stop");
            let deadline = Instant::now() + Duration::from_secs(2);
            while crate::metadata::process_exists(self.pid) != Some(false)
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn write_ready_announcement(ready_path: &Path, endpoint: SocketAddr, instance_id: String) {
        let info = RuntimeLaunchInfo {
            endpoint,
            instance_id,
            pid: std::process::id(),
            runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let temporary = ready_path.with_extension("ready.tmp");
        fs::write(&temporary, serde_json::to_vec(&info).unwrap()).unwrap();
        fs::rename(temporary, ready_path).unwrap();
    }

    #[test]
    #[ignore = "launched as a child-process fixture by the process launcher tests"]
    fn child_process_fixture() {
        let config: RuntimeLaunchConfig =
            serde_json::from_reader(io::stdin().lock()).expect("startup config from parent");
        let workspace = config.workspace_root;
        fs::write(
            workspace.join(".runtime-fixture-pid"),
            std::process::id().to_string(),
        )
        .expect("record child PID");
        let mode = fs::read_to_string(workspace.join(".runtime-fixture-mode"))
            .expect("read child fixture mode");
        match mode.trim() {
            "crash" => std::process::exit(17),
            "hang" => loop {
                thread::sleep(Duration::from_secs(60));
            },
            _ => {}
        }

        let listener = TcpListener::bind(config.address).expect("bind fixture listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let endpoint = listener.local_addr().expect("fixture endpoint");
        let instance_id = config.instance_id.expect("fixture instance ID");
        let ready_path = std::env::var_os("COGITO_RUNTIME_READY_FILE")
            .map(PathBuf::from)
            .expect("launcher supplies readiness file path");
        write_ready_announcement(&ready_path, endpoint, instance_id.clone());
        if mode.trim() == "slow" {
            thread::sleep(Duration::from_millis(180));
        }

        let stopping = AtomicBool::new(false);
        let mut held_connections = Vec::new();
        while !stopping.load(Ordering::Acquire) {
            if workspace.join(".runtime-fixture-stop").exists() {
                stopping.store(true, Ordering::Release);
                continue;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("blocking connection");
                    if mode.trim() == "no-response" {
                        held_connections.push(stream);
                        continue;
                    }
                    let mut line = String::new();
                    let mut reader =
                        std::io::BufReader::new(stream.try_clone().expect("clone fixture stream"));
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                        let request: crate::RpcRequest =
                            serde_json::from_str(&line).expect("valid fixture request");
                        let result: Value = match request.method.as_str() {
                            "health/check" => json!({
                                "status": "ready",
                                "protocolVersion": if mode.trim() == "incompatible" { 99 } else { crate::RPC_PROTOCOL_VERSION },
                                "clientProtocolVersion": request.params.get("clientProtocolVersion"),
                                "serverProtocolVersion": if mode.trim() == "incompatible" { 99 } else { crate::RPC_PROTOCOL_VERSION },
                                "clientVersion": request.params.get("clientVersion"),
                                "runtimeVersion": env!("CARGO_PKG_VERSION"),
                                "instanceId": instance_id,
                                "pid": std::process::id(),
                            }),
                            "workspace.open" => json!({"path": request.params["path"]}),
                            "rpc.shutdown" => {
                                assert_eq!(request.params["instanceId"], instance_id);
                                stopping.store(true, Ordering::Release);
                                json!({"status":"shutting_down"})
                            }
                            other => panic!("unexpected fixture request: {other}"),
                        };
                        let response = crate::RpcResponse {
                            version: crate::RPC_PROTOCOL_VERSION,
                            id: request.id,
                            ok: true,
                            result: Some(result),
                            error: None,
                        };
                        if serde_json::to_writer(&mut stream, &response).is_err()
                            || stream.write_all(b"\n").is_err()
                            || stream.flush().is_err()
                        {
                            break;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        drop(held_connections);
    }
}
