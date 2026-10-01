use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const RPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Deserialize, Serialize)]
pub struct RpcRequest {
    pub version: u32,
    pub id: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl fmt::Debug for RpcRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("RpcRequest");
        debug
            .field("version", &self.version)
            .field("id", &self.id)
            .field("method", &self.method);
        if matches!(
            self.method.as_str(),
            "credentials.validate" | "credentials.connect"
        ) {
            debug.field("params", &"[redacted]");
        } else {
            debug.field("params", &self.params);
        }
        debug.finish()
    }
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
    "health/check",
    "rpc.initialize",
    "rpc.shutdown",
    "workspace.open",
    "workspace.inspect",
    "config.inspect",
    "settings.inspect",
    "settings.update_model",
    "settings.update_permissions",
    "settings.test_model",
    "credentials.list",
    "credentials.validate",
    "credentials.connect",
    "credentials.disconnect",
    "models.list",
    "models.refresh",
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
    "file.read",
    "git.file_diff",
    "checkpoint.list",
    "checkpoint.inspect",
    "checkpoint.undo",
    // Human-operated terminals. `terminal.open` requires `origin: "human"`;
    // see the server module for the security boundary between these and
    // agent-controlled command execution.
    "terminal.list",
    "terminal.open",
    "terminal.write",
    "terminal.resize",
    "terminal.close",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_request_debug_output_redacts_the_entire_params_object() {
        let secret = "api-key-that-must-not-appear";
        let request = RpcRequest {
            version: RPC_PROTOCOL_VERSION,
            id: Some("1".to_owned()),
            method: "credentials.connect".to_owned(),
            params: serde_json::json!({
                "provider_id": "openai",
                "api_key": secret,
            }),
        };

        let debug = format!("{request:?}");
        assert!(!debug.contains(secret));
        assert!(debug.contains("[redacted]"));
    }
}
