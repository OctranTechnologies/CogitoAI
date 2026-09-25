mod rpc_bridge {
    use std::collections::{HashMap, VecDeque};
    use std::net::SocketAddr;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    use harness_rpc::{RpcClient, RpcClientWriter, ServerMessage};
    use serde_json::Value;
    use tauri::State;

    #[derive(Default)]
    pub struct RpcBridgeState {
        connections: Mutex<HashMap<String, Connection>>,
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
    pub fn rpc_connect(
        state: State<'_, RpcBridgeState>,
        address: String,
    ) -> Result<String, String> {
        let address = address
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid runtime address: {error}"))?;
        let client = RpcClient::connect(address).map_err(|error| error.to_string())?;
        let (reader, writer) = client.split();
        let inbox = Inbox::new();
        let reader_inbox = Arc::clone(&inbox);
        thread::spawn(move || loop {
            match reader.receive() {
                Ok(message) => reader_inbox.push(message),
                Err(_) => {
                    reader_inbox.close();
                    break;
                }
            }
        });
        let id = format!("rpc-{}", address.port());
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
        state: State<'_, RpcBridgeState>,
        client_id: String,
    ) -> Result<(), String> {
        let connection = state
            .connections
            .lock()
            .expect("RPC bridge lock poisoned")
            .remove(&client_id);
        if let Some(connection) = connection {
            connection.writer.shutdown();
            connection.inbox.close();
        }
        Ok(())
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(rpc_bridge::RpcBridgeState::default())
        .invoke_handler(tauri::generate_handler![
            rpc_bridge::rpc_connect,
            rpc_bridge::rpc_request,
            rpc_bridge::rpc_receive,
            rpc_bridge::rpc_disconnect
        ])
        .run(tauri::generate_context!())
        .expect("error while running CogitoAI desktop application");
}
