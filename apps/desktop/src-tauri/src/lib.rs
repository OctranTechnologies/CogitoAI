mod rpc_bridge {
    use std::collections::{HashMap, VecDeque};
    use std::fs;
    use std::net::TcpStream;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use harness_rpc::{
        metadata::RuntimeMetadataStore, ConnectionState, ConnectionStatus,
        HarnessConnectionManager, ProcessRuntimeLauncher, RpcClient, RpcClientReader,
        RpcClientWriter, RuntimeLaunchConfig, ServerMessage,
    };
    use serde::Serialize;
    use serde_json::Value;
    use tauri::{AppHandle, Emitter, State};

    pub struct RpcBridgeState {
        connections: Mutex<HashMap<String, Connection>>,
        connector: HarnessConnectionManager,
        next_connection: AtomicU64,
        connection_generation: Arc<AtomicU64>,
    }

    impl RpcBridgeState {
        pub fn new(connector: HarnessConnectionManager) -> Self {
            Self {
                connections: Mutex::new(HashMap::new()),
                connector,
                next_connection: AtomicU64::new(0),
                connection_generation: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    pub fn connector_with_resources(
        resource_dir: Option<std::path::PathBuf>,
    ) -> HarnessConnectionManager {
        let mut launcher = ProcessRuntimeLauncher::default();
        #[cfg(debug_assertions)]
        {
            // The development sidecar build script writes here. Keep this path
            // rooted in the Tauri crate manifest, never in the caller's CWD.
            launcher = launcher.with_search_directory(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries"),
            );
        }
        if let Some(resource_dir) = resource_dir {
            // Tauri places externalBin sidecars in the application resource
            // tree; search both that root and the configured `binaries/` path.
            launcher = launcher
                .with_search_directory(resource_dir.clone())
                .with_search_directory(resource_dir.join("binaries"));
        }
        HarnessConnectionManager::new(Arc::new(launcher))
    }

    struct Connection {
        writer: RpcClientWriter,
        inbox: Arc<Inbox>,
    }

    struct Inbox {
        state: Mutex<InboxState>,
        ready: Condvar,
    }

    struct InboxState {
        messages: VecDeque<ServerMessage>,
        closed: bool,
    }

    #[derive(Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct RuntimeConnectionEvent {
        state: &'static str,
        message: &'static str,
    }

    fn state_event_name(state: ConnectionState) -> &'static str {
        match state {
            ConnectionState::Disconnected => "runtime.disconnected",
            ConnectionState::Discovering => "runtime.discovering",
            ConnectionState::Starting => "runtime.starting",
            ConnectionState::Connecting => "runtime.connecting",
            ConnectionState::Connected => "runtime.connected",
            ConnectionState::Reconnecting => "runtime.reconnecting",
            ConnectionState::Failed => "runtime.failed",
        }
    }

    fn event_for_state(state: ConnectionState) -> RuntimeConnectionEvent {
        let (state, message) = match state {
            ConnectionState::Disconnected => ("disconnected", "Harness disconnected"),
            ConnectionState::Discovering => ("discovering", "Connecting to Harness…"),
            ConnectionState::Starting => ("starting", "Starting Harness…"),
            ConnectionState::Connecting => ("connecting", "Connecting…"),
            ConnectionState::Connected => ("connected", "Harness connected"),
            ConnectionState::Reconnecting => ("reconnecting", "Reconnecting…"),
            ConnectionState::Failed => ("failed", "Harness is unavailable"),
        };
        RuntimeConnectionEvent { state, message }
    }

    fn emit_runtime_state(app: &AppHandle, state: ConnectionState) {
        let _ = app.emit(state_event_name(state), event_for_state(state));
    }

    fn connection_config(
        address: &str,
        workspace_path: &str,
    ) -> Result<RuntimeLaunchConfig, String> {
        let endpoint =
            HarnessConnectionManager::resolve_endpoint(Path::new(workspace_path), Some(address))
                .map_err(|_| "Harness could not resolve the local runtime endpoint".to_owned())?;
        Ok(RuntimeLaunchConfig::new(endpoint, workspace_path))
    }

    fn connect_with_events(
        app: &AppHandle,
        connector: &HarnessConnectionManager,
        config: RuntimeLaunchConfig,
        reconnect: bool,
    ) -> Result<RpcClient, String> {
        let statuses = connector.subscribe_status();
        let connector = connector.clone();
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = if reconnect {
                connector.reconnect(&config)
            } else {
                connector.connect_or_start(&config)
            };
            let _ = result_sender.send(result);
        });

        while !worker.is_finished() {
            while let Ok(status) = statuses.try_recv() {
                emit_runtime_state(app, status.state);
            }
            if worker.is_finished() {
                break;
            }
            if let Ok(status) = statuses.recv_timeout(Duration::from_millis(25)) {
                emit_runtime_state(app, status.state);
            }
        }
        while let Ok(status) = statuses.try_recv() {
            emit_runtime_state(app, status.state);
        }
        let result = result_receiver
            .recv()
            .map_err(|_| "Harness connection attempt did not complete".to_owned())?;
        let _ = worker.join();
        result.map_err(|_| {
            emit_runtime_state(app, ConnectionState::Failed);
            "Harness could not connect or start. Retry, restart the runtime, or open logs for details."
                .to_owned()
        })
    }

    fn watch_runtime_connection(
        app: AppHandle,
        connector: HarnessConnectionManager,
        generation: Arc<AtomicU64>,
        expected_generation: u64,
        reader: RpcClientReader,
        inbox: Arc<Inbox>,
    ) {
        thread::spawn(move || loop {
            match reader.receive() {
                Ok(message) => inbox.push(message),
                Err(_) => {
                    inbox.close();
                    if generation.load(Ordering::Acquire) == expected_generation
                        && connector.status().state != ConnectionState::Disconnected
                        && connector.disconnect().is_ok()
                    {
                        emit_runtime_state(&app, ConnectionState::Disconnected);
                    }
                    break;
                }
            }
        });
    }

    impl Inbox {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(InboxState {
                    messages: VecDeque::new(),
                    closed: false,
                }),
                ready: Condvar::new(),
            })
        }

        fn push(&self, message: ServerMessage) {
            let mut state = self.state.lock().expect("RPC inbox lock poisoned");
            state.messages.push_back(message);
            self.ready.notify_all();
        }

        fn close(&self) {
            let mut state = self.state.lock().expect("RPC inbox lock poisoned");
            state.closed = true;
            self.ready.notify_all();
        }

        fn pop(&self) -> Result<Option<ServerMessage>, ()> {
            let mut state = self.state.lock().expect("RPC inbox lock poisoned");
            loop {
                if let Some(message) = state.messages.pop_front() {
                    return Ok(Some(message));
                }
                if state.closed {
                    return Ok(None);
                }
                state = self.ready.wait(state).expect("RPC inbox lock poisoned");
            }
        }

        fn wait_for_response(&self, id: &str) -> Result<Value, ()> {
            let mut state = self.state.lock().expect("RPC inbox lock poisoned");
            loop {
                if let Some(index) = state.messages.iter().position(|message| {
                matches!(message, ServerMessage::Response(response) if response.id.as_deref() == Some(id))
            }) {
                let message = state.messages.remove(index).expect("response index disappeared");
                let ServerMessage::Response(response) = message else {
                    return Err(());
                };
                return serde_json::to_value(response).map_err(|_| ());
            }
                if state.closed {
                    return Err(());
                }
                state = self.ready.wait(state).expect("RPC inbox lock poisoned");
            }
        }
    }

    #[tauri::command]
    pub async fn rpc_connect(
        app: AppHandle,
        state: State<'_, RpcBridgeState>,
        address: String,
        workspace_path: String,
        reconnect: bool,
    ) -> Result<String, String> {
        let generation = state.connection_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let config = connection_config(&address, &workspace_path)?;
        let app_for_connect = app.clone();
        let connector = state.connector.clone();
        let client = tauri::async_runtime::spawn_blocking(move || {
            connect_with_events(&app_for_connect, &connector, config, reconnect)
        })
        .await
        .map_err(|_| "Harness connection attempt did not complete".to_owned())??;
        let (reader, writer) = client.split();
        let inbox = Inbox::new();
        watch_runtime_connection(
            app,
            state.connector.clone(),
            Arc::clone(&state.connection_generation),
            generation,
            reader,
            Arc::clone(&inbox),
        );
        let sequence = state.next_connection.fetch_add(1, Ordering::Relaxed);
        let id = format!("rpc-{}-{sequence}", std::process::id());
        state
            .connections
            .lock()
            .expect("RPC bridge lock poisoned")
            .insert(id.clone(), Connection { writer, inbox });
        Ok(id)
    }

    #[tauri::command]
    pub fn rpc_request(
        state: State<'_, RpcBridgeState>,
        client_id: String,
        method: String,
        params: Value,
    ) -> Result<Value, String> {
        let connection = state
            .connections
            .lock()
            .expect("RPC bridge lock poisoned")
            .get(&client_id)
            .map(|connection| (connection.writer.clone(), Arc::clone(&connection.inbox)));
        let Some((writer, inbox)) = connection else {
            return Err("runtime connection is not available".to_owned());
        };
        let id = writer
            .request(&method, params)
            .map_err(|error| error.to_string())?;
        inbox
            .wait_for_response(&id)
            .map_err(|_| "runtime connection closed before a response arrived".to_owned())
    }

    #[tauri::command]
    pub fn rpc_receive(
        state: State<'_, RpcBridgeState>,
        client_id: String,
    ) -> Result<Option<Value>, String> {
        let inbox = state
            .connections
            .lock()
            .expect("RPC bridge lock poisoned")
            .get(&client_id)
            .map(|connection| Arc::clone(&connection.inbox));
        let Some(inbox) = inbox else {
            return Err("runtime connection is not available".to_owned());
        };
        inbox
            .pop()
            .map(|message| message.map(|value| serde_json::to_value(value).unwrap()))
            .map_err(|_| "runtime connection closed".to_owned())
    }

    #[tauri::command]
    pub fn rpc_disconnect(
        app: AppHandle,
        state: State<'_, RpcBridgeState>,
        client_id: String,
    ) -> Result<(), String> {
        let connection = state
            .connections
            .lock()
            .expect("RPC bridge lock poisoned")
            .remove(&client_id);
        if let Some(connection) = connection {
            state.connection_generation.fetch_add(1, Ordering::AcqRel);
            connection.writer.shutdown();
            connection.inbox.close();
        }
        if state.connector.status().state != ConnectionState::Disconnected {
            state
                .connector
                .disconnect()
                .map_err(|_| "Harness connection could not be closed cleanly".to_owned())?;
            emit_runtime_state(&app, ConnectionState::Disconnected);
        }
        Ok(())
    }

    #[tauri::command]
    pub fn rpc_connection_status(state: State<'_, RpcBridgeState>) -> ConnectionStatus {
        state.connector.status()
    }

    #[tauri::command]
    pub fn rpc_open_runtime_logs() -> Result<(), String> {
        let log_directory = RuntimeMetadataStore::for_current_user_or_fallback()
            .directory()
            .to_path_buf();
        fs::create_dir_all(&log_directory)
            .map_err(|_| "Harness logs could not be opened".to_owned())?;

        #[cfg(target_os = "windows")]
        let result = Command::new("explorer.exe").arg(&log_directory).spawn();
        #[cfg(target_os = "macos")]
        let result = Command::new("open").arg(&log_directory).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let result = Command::new("xdg-open").arg(&log_directory).spawn();

        result
            .map(|_| ())
            .map_err(|_| "Harness logs could not be opened".to_owned())
    }

    #[tauri::command]
    pub async fn rpc_restart_runtime(
        app: AppHandle,
        state: State<'_, RpcBridgeState>,
        address: String,
        workspace_path: String,
    ) -> Result<(), String> {
        state.connection_generation.fetch_add(1, Ordering::AcqRel);
        let config = connection_config(&address, &workspace_path)?;
        let app_for_restart = app.clone();
        let connector = state.connector.clone();
        tauri::async_runtime::spawn_blocking(move || {
            restart_runtime_process(app_for_restart, connector, config)
        })
        .await
        .map_err(|_| "Harness restart attempt did not complete".to_owned())??;

        let mut connections = state.connections.lock().expect("RPC bridge lock poisoned");
        for (_, connection) in connections.drain() {
            connection.writer.shutdown();
            connection.inbox.close();
        }
        Ok(())
    }

    fn restart_runtime_process(
        app: AppHandle,
        connector: HarnessConnectionManager,
        config: RuntimeLaunchConfig,
    ) -> Result<(), String> {
        if let Ok(mut client) = connector.connect_existing(&config) {
            let health = client
                .request("health/check", serde_json::json!({}))
                .map_err(|_| {
                    "The current Harness runtime did not answer its health check".to_owned()
                })?;
            let instance_id = health
                .result
                .as_ref()
                .and_then(|result| result.get("instanceId"))
                .and_then(Value::as_str)
                .ok_or_else(|| "The current Harness runtime has no instance identity".to_owned())?;
            let shutdown = client
                .request(
                    "rpc.shutdown",
                    serde_json::json!({"instanceId": instance_id}),
                )
                .map_err(|_| "The current Harness runtime could not be stopped".to_owned())?;
            if !shutdown.ok {
                return Err("The current Harness runtime refused to restart".to_owned());
            }
            drop(client);

            let endpoint = connector.status().endpoint;
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut stopped = false;
            while Instant::now() < deadline {
                let unavailable = endpoint.map_or(true, |endpoint| {
                    TcpStream::connect_timeout(&endpoint, Duration::from_millis(100)).is_err()
                });
                if unavailable {
                    stopped = true;
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !stopped {
                return Err("Timed out waiting for the current Harness runtime to stop".to_owned());
            }
        }

        let client = connect_with_events(&app, &connector, config, false)?;
        drop(client);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::{event_for_state, state_event_name};
        use harness_rpc::ConnectionState;

        #[test]
        fn runtime_lifecycle_events_are_stable_and_do_not_expose_diagnostics() {
            let states = [
                (
                    ConnectionState::Discovering,
                    "runtime.discovering",
                    "discovering",
                    "Connecting to Harness…",
                ),
                (
                    ConnectionState::Starting,
                    "runtime.starting",
                    "starting",
                    "Starting Harness…",
                ),
                (
                    ConnectionState::Connecting,
                    "runtime.connecting",
                    "connecting",
                    "Connecting…",
                ),
                (
                    ConnectionState::Connected,
                    "runtime.connected",
                    "connected",
                    "Harness connected",
                ),
                (
                    ConnectionState::Reconnecting,
                    "runtime.reconnecting",
                    "reconnecting",
                    "Reconnecting…",
                ),
                (
                    ConnectionState::Failed,
                    "runtime.failed",
                    "failed",
                    "Harness is unavailable",
                ),
                (
                    ConnectionState::Disconnected,
                    "runtime.disconnected",
                    "disconnected",
                    "Harness disconnected",
                ),
            ];

            for (state, event_name, serialized_state, message) in states {
                let event = event_for_state(state);
                assert_eq!(state_event_name(state), event_name);
                assert_eq!(event.state, serialized_state);
                assert_eq!(event.message, message);
                let payload = serde_json::to_value(event).expect("event serializes");
                assert!(payload.get("endpoint").is_none());
                assert!(payload.get("lastError").is_none());
            }
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            use tauri::Manager;
            let resource_dir = app.path().resource_dir().ok();
            app.manage(rpc_bridge::RpcBridgeState::new(
                rpc_bridge::connector_with_resources(resource_dir),
            ));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            rpc_bridge::rpc_connect,
            rpc_bridge::rpc_request,
            rpc_bridge::rpc_receive,
            rpc_bridge::rpc_disconnect,
            rpc_bridge::rpc_connection_status,
            rpc_bridge::rpc_open_runtime_logs,
            rpc_bridge::rpc_restart_runtime
        ])
        .run(tauri::generate_context!())
        .expect("error while running CogitoAI desktop application");
}
