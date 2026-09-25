use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const RPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcRequest {
    pub version: u32,
    pub id: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcResponse {
    pub version: u32,
    pub id: Option<String>,
    pub ok: bool,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcNotification {
    pub version: u32,
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(RpcResponse),
    Notification(RpcNotification),
}

pub const METHODS: &[&str] = &[
    "rpc.initialize",
    "workspace.open",
    "workspace.inspect",
    "config.inspect",
    "config.update",
    "session.create",
    "session.list",
    "session.inspect",
    "session.state",
    "session.resume",
    "agent.send",
    "agent.run",
    "agent.approve",
    "agent.deny",
    "agent.cancel",
    "git.status",
    "git.diff",
    "checkpoint.list",
    "checkpoint.inspect",
    "checkpoint.undo",
];
