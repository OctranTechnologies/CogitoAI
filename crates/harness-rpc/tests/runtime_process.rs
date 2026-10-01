use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use harness_models::ModelConfig;
use harness_rpc::{
    ProcessRuntimeLauncher, RpcClient, RuntimeConnector, RuntimeLaunchConfig, RuntimeMetadata,
    RuntimeMetadataStore,
};
use serde_json::{json, Value};

struct ShutdownGuard {
    endpoint: SocketAddr,
    active: bool,
}

impl ShutdownGuard {
    fn new(endpoint: SocketAddr) -> Self {
        Self {
            endpoint,
            active: true,
        }
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut client) =
            RpcClient::connect_timeout(self.endpoint, Duration::from_millis(500))
        {
            if let Ok(response) = client.request("health/check", json!({})) {
                if let Some(instance_id) = response
                    .result
                    .as_ref()
                    .and_then(|value| value.get("instanceId"))
                    .and_then(Value::as_str)
                {
                    let _ = client.request("rpc.shutdown", json!({"instanceId": instance_id}));
                }
            }
        }
    }
}

fn wait_until_stopped(endpoint: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect_timeout(&endpoint, Duration::from_millis(80)).is_err() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "runtime did not stop after admin shutdown"
        );
        thread::sleep(Duration::from_millis(30));
    }
}

fn shutdown_runtime(runtime_directory: &std::path::Path, workspace: &std::path::Path) {
    let store = RuntimeMetadataStore::new(runtime_directory);
    let metadata: RuntimeMetadata = serde_json::from_slice(
        &fs::read(store.metadata_path(workspace)).expect("runtime metadata should exist"),
    )
    .expect("valid runtime metadata");
    let endpoint: SocketAddr = metadata.endpoint.parse().expect("metadata endpoint");
    let mut client = RpcClient::connect_timeout(endpoint, Duration::from_secs(2))
        .expect("connect to spawned runtime");
    client
        .request("rpc.shutdown", json!({"instanceId": metadata.instance_id}))
        .expect("admin shutdown should be accepted");
    wait_until_stopped(endpoint);
}

#[test]
fn managed_runtime_survives_client_disconnect_and_shuts_down_explicitly() {
    let directory = tempfile::tempdir().expect("temporary runtime data directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir_all(&workspace).expect("temporary workspace");
    let runtime_directory = directory.path().join("runtime");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_cogito-harness-runtime"));
    let launcher = ProcessRuntimeLauncher::new()
        .with_executable(executable)
        .with_runtime_directory(runtime_directory.clone());
    let connector = RuntimeConnector::with_timing_and_runtime_directory(
        Arc::new(launcher),
        Duration::from_secs(8),
        Duration::from_millis(40),
        runtime_directory.clone(),
    );
    let address = RuntimeConnector::resolve_endpoint(&workspace, None).unwrap();
    let mut config = RuntimeLaunchConfig::new(address, &workspace);
    config.model = Some(ModelConfig::default());
    config.session_root = Some(workspace.join(".cogito/sessions"));
    config.apply_saved_preferences = false;

    let mut first_client = connector
        .connect_or_start(&config)
        .expect("runtime should spawn and become ready");
    assert!(connector.status().started_runtime);
    let endpoint = connector.status().endpoint.unwrap();
    let mut shutdown = ShutdownGuard::new(endpoint);
    let health = first_client
        .request("health/check", json!({}))
        .expect("health RPC")
        .result
        .expect("health result");
    assert_eq!(health["status"], "ready");
    assert_eq!(health["protocolVersion"], harness_rpc::RPC_PROTOCOL_VERSION);
    assert_ne!(health["pid"], std::process::id());
    let instance_id = health["instanceId"].as_str().unwrap().to_owned();

    drop(first_client);
    let mut second_client = connector
        .connect_existing(&config)
        .expect("runtime should remain alive after its first client closes");
    let second_health = second_client
        .request("health/check", json!({}))
        .expect("health after reconnect")
        .result
        .expect("health result");
    assert_eq!(second_health["instanceId"], instance_id);

    let attaching_connector = RuntimeConnector::with_runtime_directory(
        Arc::new(ProcessRuntimeLauncher::new().with_runtime_directory(runtime_directory.clone())),
        runtime_directory.clone(),
    );
    let attached_client = attaching_connector
        .connect_or_start(&config)
        .expect("a later client should reuse the healthy runtime");
    assert!(!attaching_connector.status().started_runtime);
    drop(attached_client);

    let metadata_path = RuntimeMetadataStore::new(&runtime_directory).metadata_path(&workspace);
    let metadata: RuntimeMetadata = serde_json::from_slice(
        &fs::read(metadata_path).expect("runtime metadata should be written"),
    )
    .expect("valid runtime metadata");
    assert_eq!(metadata.instance_id, instance_id);
    assert_eq!(metadata.pid, health["pid"].as_u64().unwrap() as u32);
    assert_eq!(metadata.endpoint, endpoint.to_string());
    assert!(fs::read_dir(&runtime_directory)
        .expect("runtime directory should exist")
        .filter_map(Result::ok)
        .all(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.ends_with(".ready") && !name.ends_with(".ready.tmp")
        }));

    second_client
        .request("rpc.shutdown", json!({"instanceId": instance_id}))
        .expect("explicit admin shutdown");
    shutdown.active = false;
    wait_until_stopped(endpoint);
}

#[test]
fn detached_runtime_does_not_hold_client_output_pipes_open() {
    let directory = tempfile::tempdir().expect("temporary client/runtime directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir_all(&workspace).expect("temporary workspace");
    let runtime_directory = directory.path().join("runtime");
    let fixture = serde_json::json!({
        "workspace": workspace,
        "runtime_directory": runtime_directory,
        "runtime_binary": env!("CARGO_BIN_EXE_cogito-harness-runtime"),
    });

    let mut child = Command::new(std::env::current_exe().expect("integration test executable"))
        .args([
            "--ignored",
            "--exact",
            "client_process_fixture",
            "--nocapture",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("client fixture should start");
    child
        .stdin
        .take()
        .expect("fixture input pipe")
        .write_all(fixture.to_string().as_bytes())
        .expect("send fixture config");

    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    let mut stdout = child.stdout.take().expect("fixture stdout");
    let mut stderr = child.stderr.take().expect("fixture stderr");
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stdout_sender.send(result);
    });
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stderr_sender.send(result);
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("check fixture process") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("client process should exit while its runtime stays alive");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "client fixture exited with {status}");

    // A detached server must not inherit either capture pipe from the short-
    // lived client. Keep the runtime alive while checking for EOF.
    let stdout_closed_before_shutdown = stdout_receiver
        .recv_timeout(Duration::from_millis(500))
        .is_ok();
    let stderr_closed_before_shutdown = stderr_receiver
        .recv_timeout(Duration::from_millis(500))
        .is_ok();
    shutdown_runtime(&runtime_directory, &workspace);
    assert!(
        stdout_closed_before_shutdown,
        "runtime kept the client's captured stdout open"
    );
    assert!(
        stderr_closed_before_shutdown,
        "runtime kept the client's captured stderr open"
    );
}

#[test]
#[ignore = "launched as a subprocess fixture by detached_runtime_does_not_hold_client_output_pipes_open"]
fn client_process_fixture() {
    let config: serde_json::Value =
        serde_json::from_reader(std::io::stdin().lock()).expect("fixture config from parent");
    let workspace = PathBuf::from(config["workspace"].as_str().expect("workspace"));
    let runtime_directory = PathBuf::from(
        config["runtime_directory"]
            .as_str()
            .expect("runtime directory"),
    );
    let runtime_binary = PathBuf::from(
        config["runtime_binary"]
            .as_str()
            .expect("runtime executable"),
    );
    let launcher = ProcessRuntimeLauncher::new()
        .with_executable(runtime_binary)
        .with_runtime_directory(runtime_directory.clone());
    let connector =
        RuntimeConnector::with_runtime_directory(Arc::new(launcher), runtime_directory.clone());
    let launch = RuntimeLaunchConfig::new("127.0.0.1:0".parse().unwrap(), &workspace);
    let client = connector
        .connect_or_start(&launch)
        .expect("client should start its runtime");
    drop(client);
    println!("client disconnected; runtime remains available");
}
