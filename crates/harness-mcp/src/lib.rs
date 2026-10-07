//! MCP client and configuration boundary for local and remote tool servers.
//!
//! The manager deliberately exposes provider-neutral JSON descriptors rather
//! than rmcp types. Remote tools are untrusted and must be wrapped in the
//! harness tool/policy layer before invocation.

use std::collections::BTreeMap;
use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use harness_core::{redact_sensitive, register_sensitive_value};
use rmcp::model::{
    CallToolRequestParams, ClientConfig, PaginatedRequestParams, ReadResourceRequestParams,
};
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::process::Command;

const MAX_TOOL_COUNT: usize = 500;
const MAX_TOOL_SCHEMA_BYTES: usize = 4 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 1_024;
const MAX_RESOURCE_URI_BYTES: usize = 2 * 1_024;
const MAX_SERVER_ERROR_BYTES: usize = 2 * 1_024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 24 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub transport: McpTransportConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Child variable name -> name of the parent environment variable.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    StreamableHttp {
        url: String,
        /// Optional variable name containing a Bearer token. The value is
        /// resolved only inside the runtime and is never serialized.
        #[serde(default)]
        bearer_token_env: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum McpConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpToolDescriptor {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub estimated_definition_tokens: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpResourceDescriptor {
    pub server_id: String,
    pub name: String,
    pub uri: String,
    pub description: String,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpServerSnapshot {
    pub server_id: String,
    pub transport: String,
    pub state: McpConnectionState,
    pub tools: Vec<McpToolDescriptor>,
    pub resources: Vec<McpResourceDescriptor>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpSnapshot {
    pub servers: Vec<McpServerSnapshot>,
    pub total_tools: usize,
    pub estimated_tool_definition_tokens: usize,
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("invalid MCP configuration: {0}")]
    InvalidConfig(String),
    #[error("MCP server {server_id} is not configured or disabled")]
    UnknownServer { server_id: String },
    #[error("MCP server {server_id}: {message}")]
    Server { server_id: String, message: String },
    #[error("MCP tool {tool} was not discovered on server {server_id}")]
    UnknownTool { server_id: String, tool: String },
    #[error("MCP operation was cancelled")]
    Cancelled,
}

type Client = RunningService<RoleClient, ClientConfig>;
type ServerEntryHandle = Arc<tokio::sync::Mutex<ServerEntry>>;
type SelectedServerEntries = Vec<(String, ServerEntryHandle)>;

struct ServerEntry {
    config: McpServerConfig,
    state: McpConnectionState,
    client: Option<Client>,
    tools: Vec<McpToolDescriptor>,
    resources: Vec<McpResourceDescriptor>,
    error: Option<String>,
}

/// Owns configured MCP connections inside the privileged runtime process.
/// A failed call is never replayed; the next user/model operation reconnects.
pub struct McpManager {
    runtime: OnceLock<Result<tokio::runtime::Runtime, String>>,
    servers: Mutex<BTreeMap<String, ServerEntryHandle>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new(McpConfig::default()).expect("MCP runtime creation should succeed")
    }
}

impl McpManager {
    pub fn new(config: McpConfig) -> Result<Self, McpError> {
        validate_config(&config)?;
        let servers = config
            .servers
            .into_iter()
            .filter(|(_, config)| config.enabled)
            .map(|(server_id, config)| {
                (
                    server_id,
                    Arc::new(tokio::sync::Mutex::new(ServerEntry {
                        config,
                        state: McpConnectionState::Disconnected,
                        client: None,
                        tools: Vec::new(),
                        resources: Vec::new(),
                        error: None,
                    })),
                )
            })
            .collect();
        Ok(Self {
            runtime: OnceLock::new(),
            servers: Mutex::new(servers),
        })
    }

    fn runtime(&self) -> Result<&tokio::runtime::Runtime, McpError> {
        self.runtime
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("harness-mcp")
                    .build()
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(|error| McpError::InvalidConfig(format!("cannot start MCP runtime: {error}")))
    }

    /// Loads `.agent/mcp.toml`; absent configuration means MCP is disabled.
    pub fn from_workspace(root: &std::path::Path) -> Result<Self, McpError> {
        let path = root.join(".agent/mcp.toml");
        if !path.is_file() {
            return Self::new(McpConfig::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| McpError::InvalidConfig(format!("{}: {error}", path.display())))?;
        let config: McpConfig = toml::from_str(&text)
            .map_err(|error| McpError::InvalidConfig(format!("{}: {error}", path.display())))?;
        Self::new(config)
    }

    pub fn snapshot(&self) -> McpSnapshot {
        let servers =
            self.server_entries()
                .into_iter()
                .map(|(server_id, entry)| {
                    let entry = entry.blocking_lock();
                    let transport_closed = entry
                        .client
                        .as_ref()
                        .is_some_and(|client| client.is_transport_closed());
                    McpServerSnapshot {
                        server_id,
                        transport: transport_name(&entry.config.transport).to_owned(),
                        state: if transport_closed {
                            McpConnectionState::Failed
                        } else {
                            entry.state.clone()
                        },
                        tools: entry.tools.clone(),
                        resources: entry.resources.clone(),
                        error: entry.error.clone().or_else(|| {
                            transport_closed.then(|| "MCP transport is closed".to_owned())
                        }),
                    }
                })
                .collect::<Vec<_>>();
        let total_tools = servers.iter().map(|server| server.tools.len()).sum();
        let estimated_tool_definition_tokens = servers
            .iter()
            .flat_map(|server| &server.tools)
            .map(|tool| tool.estimated_definition_tokens)
            .sum();
        McpSnapshot {
            servers,
            total_tools,
            estimated_tool_definition_tokens,
        }
    }

    pub fn has_servers(&self) -> bool {
        !self
            .servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    pub fn refresh(&self, server_id: Option<&str>) -> Result<McpSnapshot, McpError> {
        self.refresh_cancellable(server_id, || false)
    }

    pub fn refresh_cancellable(
        &self,
        server_id: Option<&str>,
        is_cancelled: impl Fn() -> bool + Send + Sync,
    ) -> Result<McpSnapshot, McpError> {
        for (id, entry) in self.selected_entries(server_id)? {
            let result = self.runtime()?.block_on(async {
                let mut entry = entry.lock().await;
                cancellable(
                    async {
                        self.ensure_connected(&id, &mut entry).await?;
                        self.refresh_server(&id, &mut entry).await
                    },
                    &is_cancelled,
                )
                .await
            });
            match result {
                Err(error) if matches!(&error, McpError::Cancelled) => {
                    self.runtime()?.block_on(async {
                        let mut entry = entry.lock().await;
                        entry.state = McpConnectionState::Disconnected;
                        entry.error = None;
                        if let Some(client) = entry.client.take() {
                            let _ = client.cancel().await;
                        }
                    });
                    return Err(error);
                }
                Err(error) | Ok(Err(error)) => {
                    self.runtime()?.block_on(async {
                        let mut entry = entry.lock().await;
                        entry.state = McpConnectionState::Failed;
                        entry.error = Some(bounded_string(
                            &redact_sensitive(&error.to_string()),
                            MAX_SERVER_ERROR_BYTES,
                        ));
                        if let Some(client) = entry.client.take() {
                            let _ = client.cancel().await;
                        }
                    });
                }
                Ok(Ok(())) => {}
            }
        }
        Ok(self.snapshot())
    }

    pub fn disconnect(&self, server_id: &str) -> Result<McpSnapshot, McpError> {
        let (_, entry) = self.get_entry(server_id)?;
        self.runtime()?.block_on(async {
            let mut entry = entry.lock().await;
            if let Some(client) = entry.client.take() {
                let _ = tokio::time::timeout(REQUEST_TIMEOUT, client.cancel()).await;
            }
            entry.state = McpConnectionState::Disconnected;
            entry.error = None;
            entry.tools.clear();
            entry.resources.clear();
        });
        Ok(self.snapshot())
    }

    /// Connects lazily, fetches remote descriptors and returns only matching
    /// definitions. The agent does not receive the entire catalog by default.
    pub fn search_tools(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<McpToolDescriptor>, McpError> {
        self.search_tools_cancellable(query, limit, || false)
    }

    pub fn search_tools_cancellable(
        &self,
        query: &str,
        limit: usize,
        is_cancelled: impl Fn() -> bool + Send + Sync,
    ) -> Result<Vec<McpToolDescriptor>, McpError> {
        self.refresh_cancellable(None, is_cancelled)?;
        let query = query.trim().to_ascii_lowercase();
        let mut matches = self
            .snapshot()
            .servers
            .into_iter()
            .flat_map(|server| server.tools)
            .filter(|tool| {
                query.is_empty()
                    || tool.name.to_ascii_lowercase().contains(&query)
                    || tool.description.to_ascii_lowercase().contains(&query)
                    || tool.server_id.to_ascii_lowercase().contains(&query)
            })
            .collect::<Vec<_>>();
        matches.truncate(limit.clamp(1, 8));
        Ok(matches)
    }

    pub fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: Value,
        is_cancelled: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<(String, bool), McpError> {
        let (_, entry) = self.get_entry(server_id)?;
        let server_id = server_id.to_owned();
        let tool_name = tool_name.to_owned();
        if serde_json::to_vec(&arguments).map_or(true, |bytes| bytes.len() > MAX_ARGUMENT_BYTES) {
            return Err(McpError::InvalidConfig(format!(
                "MCP tool arguments exceed the {MAX_ARGUMENT_BYTES}-byte safety limit"
            )));
        }
        self.runtime()?.block_on(async move {
            let mut entry = entry.lock().await;
            let preparation = cancellable(
                async {
                    self.ensure_connected(&server_id, &mut entry).await?;
                    if !entry.tools.iter().any(|tool| tool.name == tool_name) {
                        self.refresh_server(&server_id, &mut entry).await?;
                    }
                    Ok::<(), McpError>(())
                },
                &is_cancelled,
            )
            .await;
            match preparation {
                Err(error) if matches!(&error, McpError::Cancelled) => {
                    entry.state = McpConnectionState::Disconnected;
                    entry.error = None;
                    if let Some(client) = entry.client.take() {
                        let _ = client.cancel().await;
                    }
                    return Err(error);
                }
                Err(error) | Ok(Err(error)) => {
                    entry.state = McpConnectionState::Failed;
                    entry.error = Some(bounded_string(
                        &redact_sensitive(&error.to_string()),
                        MAX_SERVER_ERROR_BYTES,
                    ));
                    if let Some(client) = entry.client.take() {
                        let _ = client.cancel().await;
                    }
                    return Err(error);
                }
                Ok(Ok(())) => {}
            }
            if !entry.tools.iter().any(|tool| tool.name == tool_name) {
                return Err(McpError::UnknownTool {
                    server_id,
                    tool: tool_name,
                });
            }
            if is_cancelled() {
                return Err(McpError::Cancelled);
            }
            let params = CallToolRequestParams::new(tool_name.clone())
                .with_arguments(arguments.as_object().cloned().unwrap_or_else(Map::new));
            let call_result = {
                let client = entry.client.as_ref().expect("connected MCP client");
                let call = client.call_tool(params);
                tokio::pin!(call);
                let mut poll_cancel = tokio::time::interval(Duration::from_millis(40));
                tokio::select! {
                    result = tokio::time::timeout(REQUEST_TIMEOUT, &mut call) => {
                        Some(match result {
                            Err(_) => Err("request timed out".to_owned()),
                            Ok(Err(error)) => Err(redact_sensitive(&error.to_string())),
                            Ok(Ok(result)) => Ok(result),
                        })
                    }
                    _ = async {
                        loop {
                            poll_cancel.tick().await;
                            if is_cancelled() { break; }
                        }
                    } => None,
                }
            };
            let result = match call_result {
                None => {
                    if let Some(client) = entry.client.take() {
                        let _ = client.cancel().await;
                    }
                    entry.state = McpConnectionState::Disconnected;
                    return Err(McpError::Cancelled);
                }
                Some(Ok(result)) => result,
                Some(Err(message)) => {
                    if let Some(client) = entry.client.take() {
                        let _ = client.cancel().await;
                    }
                    entry.state = McpConnectionState::Failed;
                    entry.error = Some(bounded_string(&message, MAX_SERVER_ERROR_BYTES));
                    return Err(McpError::Server { server_id, message });
                }
            };
            let value = serde_json::to_value(result).unwrap_or(Value::Null);
            let mut rendered = format!(
                "Untrusted MCP tool result from {server_id}/{tool_name}:\n{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
            );
            let truncated = rendered.len() > MAX_RESULT_BYTES;
            if truncated {
                truncate_utf8(&mut rendered, MAX_RESULT_BYTES);
                rendered.push_str("\n[untrusted MCP result truncated by harness]");
            }
            Ok((rendered, truncated))
        })
    }

    pub fn list_resources(&self, server_id: &str) -> Result<Vec<McpResourceDescriptor>, McpError> {
        self.list_resources_cancellable(server_id, || false)
    }

    pub fn list_resources_cancellable(
        &self,
        server_id: &str,
        is_cancelled: impl Fn() -> bool + Send + Sync,
    ) -> Result<Vec<McpResourceDescriptor>, McpError> {
        let (_, entry) = self.get_entry(server_id)?;
        let result = self.runtime()?.block_on(async {
            let mut entry = entry.lock().await;
            cancellable(
                async {
                    self.ensure_connected(server_id, &mut entry).await?;
                    self.refresh_server(server_id, &mut entry).await?;
                    Ok::<Vec<McpResourceDescriptor>, McpError>(entry.resources.clone())
                },
                &is_cancelled,
            )
            .await
        });
        match result {
            Err(error) => {
                self.close_entry(entry, McpConnectionState::Disconnected, None)?;
                Err(error)
            }
            Ok(Err(error)) => {
                self.close_entry(entry, McpConnectionState::Failed, Some(error.to_string()))?;
                Err(error)
            }
            Ok(Ok(resources)) => Ok(resources),
        }
    }

    pub fn read_resource(&self, server_id: &str, uri: &str) -> Result<String, McpError> {
        self.read_resource_cancellable(server_id, uri, || false)
    }

    pub fn read_resource_cancellable(
        &self,
        server_id: &str,
        uri: &str,
        is_cancelled: impl Fn() -> bool + Send + Sync,
    ) -> Result<String, McpError> {
        let (_, entry) = self.get_entry(server_id)?;
        let uri = uri.to_owned();
        let server_id = server_id.to_owned();
        let result = self.runtime()?.block_on(async {
            let mut entry = entry.lock().await;
            cancellable(
                async {
                    self.ensure_connected(&server_id, &mut entry).await?;
                    let result = {
                        let client = entry.client.as_ref().expect("connected MCP client");
                        tokio::time::timeout(
                            REQUEST_TIMEOUT,
                            client.read_resource(ReadResourceRequestParams::new(uri.clone())),
                        )
                        .await
                        .map_err(|_| "resource read timed out".to_owned())
                        .and_then(|result| {
                            result.map_err(|error| redact_sensitive(&error.to_string()))
                        })
                    }
                    .map_err(|message| McpError::Server {
                        server_id: server_id.clone(),
                        message,
                    })?;
                    let mut rendered =
                        serde_json::to_string(&result).unwrap_or_else(|_| "null".to_owned());
                    if rendered.len() > MAX_RESULT_BYTES {
                        truncate_utf8(&mut rendered, MAX_RESULT_BYTES);
                        rendered.push_str("\n[untrusted MCP resource truncated by harness]");
                    }
                    Ok::<String, McpError>(format!("Untrusted MCP resource {uri}:\n{rendered}"))
                },
                &is_cancelled,
            )
            .await
        });
        match result {
            Err(error) => {
                self.close_entry(entry, McpConnectionState::Disconnected, None)?;
                Err(error)
            }
            Ok(Err(error)) => {
                self.close_entry(entry, McpConnectionState::Failed, Some(error.to_string()))?;
                Err(error)
            }
            Ok(Ok(resource)) => Ok(resource),
        }
    }

    fn close_entry(
        &self,
        entry: ServerEntryHandle,
        state: McpConnectionState,
        error: Option<String>,
    ) -> Result<(), McpError> {
        self.runtime()?.block_on(async {
            let mut entry = entry.lock().await;
            entry.state = state;
            entry.error = error
                .map(|error| bounded_string(&redact_sensitive(&error), MAX_SERVER_ERROR_BYTES));
            if let Some(client) = entry.client.take() {
                let _ = client.cancel().await;
            }
        });
        Ok(())
    }

    fn server_entries(&self) -> SelectedServerEntries {
        self.servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(id, entry)| (id.clone(), Arc::clone(entry)))
            .collect()
    }

    fn selected_entries(&self, server_id: Option<&str>) -> Result<SelectedServerEntries, McpError> {
        let entries = self.server_entries();
        if let Some(id) = server_id {
            entries
                .into_iter()
                .find(|(candidate, _)| candidate == id)
                .map(|entry| vec![entry])
                .ok_or_else(|| McpError::UnknownServer {
                    server_id: id.to_owned(),
                })
        } else {
            Ok(entries)
        }
    }

    fn get_entry(&self, id: &str) -> Result<(String, ServerEntryHandle), McpError> {
        self.selected_entries(Some(id))?
            .pop()
            .ok_or_else(|| McpError::UnknownServer {
                server_id: id.to_owned(),
            })
    }

    async fn ensure_connected(&self, id: &str, entry: &mut ServerEntry) -> Result<(), McpError> {
        if entry
            .client
            .as_ref()
            .is_some_and(|client| !client.is_transport_closed())
        {
            return Ok(());
        }
        if let Some(client) = entry.client.take() {
            let _ = client.cancel().await;
        }
        entry.state = McpConnectionState::Connecting;
        entry.error = None;
        let connect = async {
            let client = match &entry.config.transport {
                McpTransportConfig::Stdio { command, args, env } => {
                    let mut cmd = Command::new(command);
                    cmd.args(args)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .kill_on_drop(true);
                    cmd.env_clear();
                    inherit_safe_environment(&mut cmd);
                    for (child_name, parent_name) in env {
                        let value = std::env::var_os(parent_name).ok_or_else(|| {
                            format!("required environment variable {parent_name} is not set")
                        })?;
                        if looks_sensitive(child_name) || looks_sensitive(parent_name) {
                            if let Some(value) = value.to_str() {
                                register_sensitive_value(value);
                            }
                        }
                        cmd.env(child_name, value);
                    }
                    #[cfg(windows)]
                    {
                        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                        cmd.creation_flags(CREATE_NO_WINDOW);
                    }
                    let transport = TokioChildProcess::new(cmd)
                        .map_err(|error| format!("could not start stdio process: {error}"))?;
                    ClientConfig::default()
                        .serve(transport)
                        .await
                        .map_err(|error| error.to_string())?
                }
                McpTransportConfig::StreamableHttp {
                    url,
                    bearer_token_env,
                } => {
                    let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                    if let Some(variable) = bearer_token_env {
                        let token = std::env::var(variable).map_err(|_| {
                            format!("required environment variable {variable} is not set")
                        })?;
                        register_sensitive_value(&token);
                        config = config.auth_header(token);
                    }
                    let transport = StreamableHttpClientTransport::from_config(config);
                    ClientConfig::default()
                        .serve(transport)
                        .await
                        .map_err(|error| error.to_string())?
                }
            };
            Ok::<Client, String>(client)
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, connect).await {
            Ok(Ok(client)) => {
                entry.client = Some(client);
                entry.state = McpConnectionState::Connected;
                Ok(())
            }
            Ok(Err(error)) => {
                let error = redact_sensitive(&error);
                entry.state = McpConnectionState::Failed;
                entry.error = Some(error.clone());
                Err(McpError::Server {
                    server_id: id.to_owned(),
                    message: error,
                })
            }
            Err(_) => {
                let error = "connection timed out".to_owned();
                entry.state = McpConnectionState::Failed;
                entry.error = Some(error.clone());
                Err(McpError::Server {
                    server_id: id.to_owned(),
                    message: error,
                })
            }
        }
    }

    async fn refresh_server(&self, id: &str, entry: &mut ServerEntry) -> Result<(), McpError> {
        let client = entry.client.as_ref().expect("connected MCP client");
        let mut tools = Vec::new();
        let mut cursor = None;
        loop {
            let result = tokio::time::timeout(
                REQUEST_TIMEOUT,
                client.list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor))),
            )
            .await
            .map_err(|_| McpError::Server {
                server_id: id.to_owned(),
                message: "tool discovery timed out".to_owned(),
            })?
            .map_err(|error| McpError::Server {
                server_id: id.to_owned(),
                message: redact_sensitive(&error.to_string()),
            })?;
            cursor = result.next_cursor.clone();
            for descriptor in result.tools {
                if tools.len() >= MAX_TOOL_COUNT {
                    break;
                }
                let serialized = serde_json::to_value(&descriptor).unwrap_or(Value::Null);
                let name = serialized
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES {
                    continue;
                }
                let description = serialized
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let description = bounded_string(description, MAX_TOOL_DESCRIPTION_BYTES);
                let input_schema = serialized
                    .get("inputSchema")
                    .or_else(|| serialized.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}));
                if serde_json::to_vec(&input_schema)
                    .map_or(true, |schema| schema.len() > MAX_TOOL_SCHEMA_BYTES)
                {
                    continue;
                }
                let cost = serde_json::to_string(&json!({
                    "description": description,
                    "input_schema": input_schema
                }))
                .map(|text| text.len().div_ceil(4))
                .unwrap_or_default();
                tools.push(McpToolDescriptor {
                    server_id: id.to_owned(),
                    name,
                    description,
                    input_schema,
                    estimated_definition_tokens: cost,
                });
            }
            if cursor.is_none() || tools.len() >= MAX_TOOL_COUNT {
                break;
            }
        }
        let mut resources = Vec::new();
        if let Ok(Ok(result)) =
            tokio::time::timeout(REQUEST_TIMEOUT, client.list_resources(None)).await
        {
            for resource in result.resources.into_iter().take(MAX_TOOL_COUNT) {
                let value = serde_json::to_value(resource).unwrap_or(Value::Null);
                let Some(uri) = value
                    .get("uri")
                    .and_then(Value::as_str)
                    .filter(|uri| !uri.is_empty() && uri.len() <= MAX_RESOURCE_URI_BYTES)
                    .map(str::to_owned)
                else {
                    continue;
                };
                resources.push(McpResourceDescriptor {
                    server_id: id.to_owned(),
                    name: bounded_string(
                        value.get("name").and_then(Value::as_str).unwrap_or(&uri),
                        MAX_TOOL_NAME_BYTES,
                    ),
                    uri,
                    description: bounded_string(
                        value
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        MAX_TOOL_DESCRIPTION_BYTES,
                    ),
                    mime_type: value
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
        }
        entry.tools = tools;
        entry.resources = resources;
        entry.state = McpConnectionState::Connected;
        entry.error = None;
        Ok(())
    }
}

fn validate_config(config: &McpConfig) -> Result<(), McpError> {
    for (id, server) in &config.servers {
        if id.is_empty()
            || id.len() > 64
            || !id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_".contains(ch))
        {
            return Err(McpError::InvalidConfig(format!(
                "server id {id:?} must be 1-64 ASCII letters, digits, '-' or '_'"
            )));
        }
        match &server.transport {
            McpTransportConfig::Stdio { command, args, env } => {
                if command.trim().is_empty() || command.len() > 1024 {
                    return Err(McpError::InvalidConfig(format!(
                        "server {id} has an invalid executable"
                    )));
                }
                if args.len() > 128
                    || args
                        .iter()
                        .any(|arg| arg.len() > 4096 || looks_like_secret_argument(arg))
                {
                    return Err(McpError::InvalidConfig(format!(
                        "server {id} has too many arguments or a secret-like command-line argument; use an environment variable"
                    )));
                }
                for (name, parent) in env {
                    if !valid_env_name(name) || !valid_env_name(parent) {
                        return Err(McpError::InvalidConfig(format!(
                            "server {id} contains an invalid environment variable name"
                        )));
                    }
                }
            }
            McpTransportConfig::StreamableHttp {
                url,
                bearer_token_env,
            } => {
                let parsed = url::Url::parse(url).map_err(|error| {
                    McpError::InvalidConfig(format!("server {id} URL is invalid: {error}"))
                })?;
                let local_http =
                    matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
                if (parsed.scheme() != "https" && !(parsed.scheme() == "http" && local_http))
                    || parsed.username() != ""
                    || parsed.password().is_some()
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(McpError::InvalidConfig(format!(
                        "server {id} must use HTTPS (HTTP is allowed only for localhost) and must not embed credentials in its URL"
                    )));
                }
                if bearer_token_env
                    .as_deref()
                    .is_some_and(|name| !valid_env_name(name))
                {
                    return Err(McpError::InvalidConfig(format!(
                        "server {id} bearer token environment variable name is invalid"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "credential",
        "authorization",
        "bearer",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || lower == "auth"
        || lower.starts_with("auth_")
        || lower.ends_with("_auth")
        || lower == "key"
        || lower.starts_with("key_")
        || lower.ends_with("_key")
}

fn looks_like_secret_argument(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    [
        "--token=",
        "--password=",
        "--api-key=",
        "--secret=",
        "--bearer=",
        "--auth=",
        "--key=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || (looks_sensitive(&lower) && arg.contains('='))
}

fn transport_name(transport: &McpTransportConfig) -> &'static str {
    match transport {
        McpTransportConfig::Stdio { .. } => "stdio",
        McpTransportConfig::StreamableHttp { .. } => "streamable_http",
    }
}

fn inherit_safe_environment(command: &mut Command) {
    for name in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn bounded_string(value: &str, max_bytes: usize) -> String {
    let mut value = value.to_owned();
    truncate_utf8(&mut value, max_bytes);
    value
}

async fn cancellable<F>(
    future: F,
    is_cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<F::Output, McpError>
where
    F: Future,
{
    tokio::pin!(future);
    let mut poll = tokio::time::interval(Duration::from_millis(40));
    loop {
        tokio::select! {
            result = &mut future => return Ok(result),
            _ = poll.tick() => {
                if is_cancelled() {
                    return Err(McpError::Cancelled);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::extract::Request as AxumRequest;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{http::StatusCode, Router};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    #[test]
    fn configuration_rejects_remote_http_and_url_credentials() {
        for url in [
            "http://example.com/mcp",
            "https://user:pass@example.com/mcp",
            "https://example.com/mcp?api_key=secret",
            "https://example.com/mcp?key=secret",
            "https://example.com/mcp#secret",
        ] {
            let config = McpConfig {
                servers: BTreeMap::from([(
                    "unsafe".into(),
                    McpServerConfig {
                        enabled: true,
                        transport: McpTransportConfig::StreamableHttp {
                            url: url.into(),
                            bearer_token_env: None,
                        },
                    },
                )]),
            };
            assert!(validate_config(&config).is_err(), "{url}");
        }
    }

    #[test]
    fn configuration_rejects_secret_cli_arguments() {
        for argument in [
            "--token=never-put-secrets-here",
            "--auth=never-put-secrets-here",
            "--bearer=never-put-secrets-here",
            "--key=never-put-secrets-here",
        ] {
            let config = McpConfig {
                servers: BTreeMap::from([(
                    "unsafe".into(),
                    McpServerConfig {
                        enabled: true,
                        transport: McpTransportConfig::Stdio {
                            command: "tool".into(),
                            args: vec![argument.into()],
                            env: BTreeMap::new(),
                        },
                    },
                )]),
            };
            assert!(validate_config(&config).is_err(), "{argument}");
        }
        assert!(looks_sensitive("MCP_AUTH"));
        assert!(looks_sensitive("PRIVATE_KEY"));
    }

    #[test]
    fn configuration_keeps_authentication_as_an_environment_reference() {
        let config: McpConfig = toml::from_str(
            "[servers.docs]\ntransport = 'streamable_http'\nurl = 'https://docs.example/mcp'\nbearer_token_env = 'DOCS_MCP_TOKEN'\n",
        )
        .unwrap();
        validate_config(&config).unwrap();
        let rendered = toml::to_string(&config).unwrap();
        assert!(rendered.contains("DOCS_MCP_TOKEN"));
        assert!(!rendered.contains("token-value"));
    }

    #[test]
    fn configuration_rejects_plaintext_credential_fields() {
        let config = toml::from_str::<McpConfig>(
            "[servers.docs]\ntransport = 'streamable_http'\nurl = 'https://docs.example/mcp'\napi_key = 'plaintext-secret'\n",
        );
        assert!(config.is_err());
    }

    #[test]
    fn fake_streamable_http_server_supports_tools_resources_disconnect_and_oversized_results() {
        let token_env = "HARNESS_MCP_TEST_BEARER";
        let token = "mcp-test-token-should-not-be-returned";
        std::env::set_var(token_env, token);
        let (url, stop, server) = start_fake_mcp_server(Some(token.to_owned()));
        let manager = Arc::new(
            McpManager::new(McpConfig {
                servers: BTreeMap::from([(
                    "fake".to_owned(),
                    McpServerConfig {
                        enabled: true,
                        transport: McpTransportConfig::StreamableHttp {
                            url,
                            bearer_token_env: Some(token_env.to_owned()),
                        },
                    },
                )]),
            })
            .unwrap(),
        );

        let snapshot = manager.refresh(None).unwrap();
        assert_eq!(snapshot.total_tools, 1);
        assert_eq!(snapshot.servers[0].state, McpConnectionState::Connected);
        assert!(snapshot.servers[0].tools[0].estimated_definition_tokens > 0);
        let resources = manager.list_resources("fake").unwrap();
        assert_eq!(resources[0].uri, "file:///guide.txt");
        assert!(manager
            .read_resource("fake", "file:///guide.txt")
            .unwrap()
            .contains("Untrusted MCP resource"));

        let (output, truncated) = manager
            .call_tool("fake", "oversized", json!({}), || false)
            .unwrap();
        assert!(truncated);
        assert!(output.len() < MAX_RESULT_BYTES + 128);
        assert!(output.contains("untrusted MCP result truncated"));

        manager.disconnect("fake").unwrap();
        assert_eq!(
            manager.snapshot().servers[0].state,
            McpConnectionState::Disconnected
        );
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        std::env::remove_var(token_env);
        assert!(!format!("{:?}", manager.snapshot()).contains(token));
    }

    #[test]
    fn server_crash_marks_failed_without_replaying_the_tool_call() {
        let (url, stop, server) = start_fake_mcp_server(None);
        let manager = McpManager::new(McpConfig {
            servers: BTreeMap::from([(
                "crashing".to_owned(),
                McpServerConfig {
                    enabled: true,
                    transport: McpTransportConfig::StreamableHttp {
                        url,
                        bearer_token_env: None,
                    },
                },
            )]),
        })
        .unwrap();
        manager.refresh(None).unwrap();
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();

        let result = manager.call_tool("crashing", "oversized", json!({}), || false);
        assert!(result.is_err());
        assert_eq!(
            manager.snapshot().servers[0].state,
            McpConnectionState::Failed
        );
    }

    #[test]
    fn cancellation_closes_a_slow_tool_call_without_replay() {
        let (url, stop, server) = start_fake_mcp_server_with_delay(None, true);
        let manager = McpManager::new(McpConfig {
            servers: BTreeMap::from([(
                "slow".to_owned(),
                McpServerConfig {
                    enabled: true,
                    transport: McpTransportConfig::StreamableHttp {
                        url,
                        bearer_token_env: None,
                    },
                },
            )]),
        })
        .unwrap();
        manager.refresh(None).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            thread_cancelled.store(true, Ordering::SeqCst);
        });
        let result = manager.call_tool("slow", "oversized", json!({}), move || {
            cancelled.load(Ordering::SeqCst)
        });
        canceller.join().unwrap();
        assert!(matches!(result, Err(McpError::Cancelled)));
        assert_eq!(
            manager.snapshot().servers[0].state,
            McpConnectionState::Disconnected
        );
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }

    fn start_fake_mcp_server(
        expected_token: Option<String>,
    ) -> (String, Arc<AtomicBool>, JoinHandle<()>) {
        start_fake_mcp_server_with_delay(expected_token, false)
    }

    fn start_fake_mcp_server_with_delay(
        expected_token: Option<String>,
        delay_tool_call: bool,
    ) -> (String, Arc<AtomicBool>, JoinHandle<()>) {
        async fn route(
            axum::extract::State((expected_token, delay_tool_call)): axum::extract::State<(
                Option<String>,
                bool,
            )>,
            request: AxumRequest,
        ) -> Response {
            let (parts, body) = request.into_parts();
            let expected_header = expected_token
                .as_ref()
                .map(|token| format!("Bearer {token}"));
            if expected_header.as_ref().is_some_and(|expected_header| {
                parts
                    .headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    != Some(expected_header.as_str())
            }) {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let body = match to_bytes(body, 1024 * 1024).await {
                Ok(body) => body,
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            };
            let request: Value = match serde_json::from_slice(&body) {
                Ok(request) => request,
                Err(_) => {
                    return StatusCode::BAD_REQUEST.into_response();
                }
            };
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if method == "tools/call" && delay_tool_call {
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
            let id = request.get("id").cloned();
            if method == "notifications/initialized" {
                return StatusCode::ACCEPTED.into_response();
            }
            let result = match method {
                "initialize" => json!({
                    "protocolVersion":"2025-11-25",
                    "capabilities":{"tools":{},"resources":{}},
                    "serverInfo":{"name":"harness-test-mcp","version":"0.1"}
                }),
                "tools/list" => json!({"tools":[{
                    "name":"oversized",
                    "description":"Return untrusted test data",
                    "inputSchema":{"type":"object","properties":{}}
                }]}),
                "resources/list" => json!({"resources":[{
                    "name":"guide",
                    "uri":"file:///guide.txt",
                    "mimeType":"text/plain"
                }]}),
                "resources/read" => json!({"contents":[{
                    "uri":"file:///guide.txt",
                    "mimeType":"text/plain",
                    "text":"untrusted guide"
                }]}),
                "tools/call" => json!({"content":[{
                    "type":"text",
                    "text":"x".repeat(40 * 1024)
                }]}),
                _ => json!({"isError":true,"content":[{"type":"text","text":"unknown method"}]}),
            };
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                json!({"jsonrpc":"2.0","id":id,"result":result}).to_string(),
            )
                .into_response()
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let app = Router::new()
                    .route("/mcp", post(route))
                    .with_state((expected_token, delay_tool_call));
                let _ = ready_tx.send(());
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        while !thread_stop.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await;
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        (format!("http://{address}/mcp"), stop, server)
    }
}
