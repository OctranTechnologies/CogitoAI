mod background;
mod customization;
mod environment;
mod filesystem;
mod mcp;
mod process;
mod repository_index;
mod subagent;
mod web;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use harness_core::{Error, Id, SessionId};
use harness_policy::{
    OperationKind, Permission, Policy, PolicyDecision, PolicyEvaluation, PolicyRequest,
};
use harness_session::{EventBus, EventPayload, HarnessEvent};
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

pub use customization::{available_skills, SkillMetadata};
pub use environment::{
    local_execution_environment, ExecutionEnvironment, LocalExecutionEnvironment, WorkspaceSnapshot,
};
pub use filesystem::{
    ApplyPatchTool, CreateFileTool, DeleteFileTool, GlobTool, GrepTool, ListDirectoryTool,
    ReadFileTool, RenameFileTool, ReplaceRangeTool, ReplaceTextTool, ShellTool, WriteFileTool,
};
pub use harness_mcp::{
    McpConfig, McpConnectionState, McpError, McpManager, McpResourceDescriptor, McpServerConfig,
    McpServerSnapshot, McpSnapshot, McpToolDescriptor, McpTransportConfig,
};
pub use process::{
    BackgroundProcessStatus, ManagedProcessInfo, ProcessLogBatch, ProcessManager,
    ProcessStatusSink, ProcessWaitResult,
};
pub use process::{
    CancellationToken, LocalProcessRunner, ProcessError, ProcessEvent, ProcessRequest,
    ProcessResult, ProcessRunner, ProcessStartRequest,
};
pub use repository_index::{
    IndexedFile, RepositoryAction, RepositoryIndex, RepositoryIndexService, RepositoryTool,
    SymbolDefinition, SymbolKind,
};
pub use subagent::{
    DelegateSubagentsTool, SubagentContext, SubagentContextMetrics, SubagentExecutor,
    SubagentReport, SubagentRole, SubagentTask,
};
pub use web::{
    HttpWebClient, WebClient, WebError, WebFetchTool, WebPage, WebSearchResult, WebSearchTool,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolRequest {
    pub fn new(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub arguments_schema: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    pub metadata: BTreeMap<String, serde_json::Value>,
    pub changed_files: Vec<PathBuf>,
    pub truncated: bool,
    /// Tool execution errors are returned to the model as observations. Policy
    /// denials remain harness errors and never reach this result type.
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            metadata: BTreeMap::new(),
            changed_files: Vec::new(),
            truncated: false,
            is_error: false,
        }
    }
}

#[derive(Debug, ThisError)]
pub enum ToolError {
    #[error("invalid arguments for {tool}: {message}")]
    InvalidArguments { tool: String, message: String },
    #[error("path is outside the workspace: {path}")]
    PathOutsideWorkspace { path: PathBuf },
    #[error("path does not exist: {path}")]
    NotFound { path: PathBuf },
    #[error("path is not a file: {path}")]
    NotFile { path: PathBuf },
    #[error("path is not a directory: {path}")]
    NotDirectory { path: PathBuf },
    #[error("file is binary and cannot be processed: {path}")]
    BinaryFile { path: PathBuf },
    #[error("file exceeds the {limit}-byte limit: {path}")]
    FileTooLarge { path: PathBuf, limit: u64 },
    #[error("patch context did not match exactly once: {path}")]
    PatchConflict { path: PathBuf },
    #[error("no matches found")]
    NoMatches,
    #[error("filesystem operation {operation} failed: {message}")]
    Io { operation: String, message: String },
    #[error("process execution failed: {message}")]
    Process { message: String },
    #[error("network request failed: {message}")]
    Network { message: String },
}

pub struct ToolContext<'a> {
    pub policy: &'a dyn Policy,
    pub working_directory: &'a std::path::Path,
    pub execution_environment: &'a dyn ExecutionEnvironment,
    /// Per-run cancellation supplied by the agent loop. Tools which support
    /// interruption should prefer it over their standalone default token.
    pub cancellation: Option<&'a CancellationToken>,
    pub event_bus: Option<&'a EventBus>,
    pub session_id: Option<&'a SessionId>,
    pub correlation_id: Option<&'a Id>,
}

impl ToolContext<'_> {
    pub fn emit(&self, payload: EventPayload) {
        let (Some(event_bus), Some(session_id)) = (self.event_bus, self.session_id) else {
            return;
        };
        event_bus.publish(&HarnessEvent::new(
            session_id.clone(),
            payload,
            None,
            self.correlation_id.cloned(),
        ));
    }
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn required_permission(&self) -> Permission;
    fn operation(&self) -> OperationKind;
    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    repository_index: Option<Arc<RepositoryIndexService>>,
    process_manager: Option<Arc<ProcessManager>>,
    read_revisions: Mutex<BTreeMap<String, String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_workspace_tools() -> Self {
        Self::with_workspace_tools_cancellation(CancellationToken::new())
    }

    pub fn with_workspace_tools_cancellation(cancellation: CancellationToken) -> Self {
        Self::with_workspace_tools_and_process_manager(
            cancellation,
            Arc::new(ProcessManager::default()),
        )
    }

    /// A child-agent tool catalog. It contains workspace reads, repository
    /// queries, and project instructions/skills only. In particular it has no
    /// filesystem mutation, process, network, or recursive delegation tools.
    pub fn with_read_only_workspace_tools() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ReadFileTool));
        registry.register(Box::new(ListDirectoryTool));
        registry.register(Box::new(GlobTool));
        registry.register(Box::new(GrepTool));
        registry.register(subagent::read_git_diff_tool());
        let index = Arc::new(RepositoryIndexService::default());
        for action in [
            RepositoryAction::SearchFiles,
            RepositoryAction::SearchText,
            RepositoryAction::FindSymbol,
            RepositoryAction::FindReferences,
            RepositoryAction::GotoDefinition,
            RepositoryAction::GetFileOutline,
            RepositoryAction::GetRepoTree,
        ] {
            registry.register(Box::new(RepositoryTool::new(action, Arc::clone(&index))));
        }
        registry.repository_index = Some(index);
        registry.register(Box::new(customization::InstructionsTool));
        registry.register(Box::new(customization::ListSkillsTool));
        registry.register(Box::new(customization::LoadSkillTool));
        registry
    }

    pub fn with_workspace_tools_and_process_manager(
        cancellation: CancellationToken,
        process_manager: Arc<ProcessManager>,
    ) -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ReadFileTool));
        registry.register(Box::new(CreateFileTool));
        registry.register(Box::new(WriteFileTool));
        registry.register(Box::new(ApplyPatchTool));
        registry.register(Box::new(ReplaceTextTool));
        registry.register(Box::new(ReplaceRangeTool));
        registry.register(Box::new(DeleteFileTool));
        registry.register(Box::new(RenameFileTool));
        registry.register(Box::new(ListDirectoryTool));
        registry.register(Box::new(GlobTool));
        registry.register(Box::new(GrepTool));
        registry.register(Box::new(ShellTool::with_cancellation(cancellation.clone())));
        registry.register(Box::new(
            ShellTool::with_cancellation(cancellation).named("run_command"),
        ));
        for action in [
            background::ProcessToolAction::Start,
            background::ProcessToolAction::ReadOutput,
            background::ProcessToolAction::List,
            background::ProcessToolAction::Stop,
            background::ProcessToolAction::WaitForOutput,
        ] {
            registry.register(Box::new(background::BackgroundProcessTool::new(
                Arc::clone(&process_manager),
                action,
            )));
        }
        registry.process_manager = Some(process_manager);
        let index = Arc::new(RepositoryIndexService::default());
        for action in [
            RepositoryAction::SearchFiles,
            RepositoryAction::SearchText,
            RepositoryAction::FindSymbol,
            RepositoryAction::FindReferences,
            RepositoryAction::GotoDefinition,
            RepositoryAction::GetDiagnostics,
            RepositoryAction::GetFileOutline,
            RepositoryAction::GetRepoTree,
        ] {
            registry.register(Box::new(RepositoryTool::new(action, Arc::clone(&index))));
        }
        registry.repository_index = Some(index);
        registry.register(Box::new(customization::InstructionsTool));
        registry.register(Box::new(customization::ListSkillsTool));
        registry.register(Box::new(customization::LoadSkillTool));
        let web_client: Arc<dyn WebClient> = Arc::new(HttpWebClient);
        registry.register(Box::new(WebSearchTool::new(Arc::clone(&web_client))));
        registry.register(Box::new(WebFetchTool::new(web_client)));
        registry
    }

    pub fn process_manager(&self) -> Option<Arc<ProcessManager>> {
        self.process_manager.as_ref().map(Arc::clone)
    }

    /// Adds a small lazy MCP tool surface. Remote definitions are fetched only
    /// when the model explicitly searches for an integration capability.
    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        mcp::register_mcp_tools(&mut self, manager);
        self
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Returns concise skill metadata for the initial working context. Full
    /// skill instructions remain available only through the `load_skill` tool.
    pub fn skill_catalog_summary(&self, workspace_root: &std::path::Path) -> String {
        available_skills(workspace_root)
            .into_iter()
            .take(48)
            .map(|skill| {
                format!(
                    "{}: {}{}",
                    skill.name,
                    skill.description,
                    skill
                        .when_to_use
                        .map(|when| format!(" (use when: {when})"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn execute(
        &self,
        context: &ToolContext<'_>,
        mut request: ToolRequest,
    ) -> Result<ToolResult, Error> {
        let tool_name = request.name.clone();
        context.emit(EventPayload::ToolRequested {
            tool: tool_name.clone(),
            arguments: event_arguments(&request.arguments),
        });
        let Some(tool) = self.tools.iter().find(|tool| tool.spec().name == tool_name) else {
            let error = Error::Tool {
                tool: tool_name.clone(),
                message: "tool is not registered".to_owned(),
            };
            context.emit(EventPayload::ToolFailed {
                tool: tool_name,
                error: error.to_string(),
            });
            return Err(error);
        };
        self.attach_read_revision(context, &tool_name, &mut request);
        let policy_request = build_policy_request(context, &tool_name, tool.as_ref(), &request);
        let evaluation = context.policy.evaluate(&policy_request);
        context.emit(EventPayload::PolicyDecision {
            tool: tool_name.clone(),
            action: evaluation.decision.to_string(),
            reason: evaluation.reason.clone(),
            rule: evaluation.rule.clone(),
            operation: policy_request.operation_name().to_owned(),
            mode: format!("{:?}", policy_request.mode).to_ascii_lowercase(),
            risk_categories: policy_request
                .risk_categories()
                .iter()
                .map(ToString::to_string)
                .collect(),
        });
        match evaluation.decision {
            PolicyDecision::Deny => {
                let error = Error::PermissionDenied {
                    capability: format!("{} {}", policy_request.operation_name(), tool_name),
                };
                context.emit(EventPayload::ToolDenied {
                    tool: tool_name,
                    reason: error.to_string(),
                });
                return Err(error);
            }
            PolicyDecision::Ask => {
                return Err(Error::PermissionRequired {
                    capability: format!("{} {}", policy_request.operation_name(), tool_name),
                    reason: evaluation.reason,
                });
            }
            PolicyDecision::Allow => {
                if tool_name == "rename_file" {
                    let destination_request = build_policy_request_for_path(
                        context,
                        &tool_name,
                        tool.as_ref(),
                        request
                            .arguments
                            .get("destination_path")
                            .and_then(serde_json::Value::as_str),
                    );
                    let destination_evaluation = context.policy.evaluate(&destination_request);
                    context.emit(EventPayload::PolicyDecision {
                        tool: tool_name.clone(),
                        action: destination_evaluation.decision.to_string(),
                        reason: destination_evaluation.reason.clone(),
                        rule: destination_evaluation.rule.clone(),
                        operation: destination_request.operation_name().to_owned(),
                        mode: format!("{:?}", destination_request.mode).to_ascii_lowercase(),
                        risk_categories: destination_request
                            .risk_categories()
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                    });
                    match destination_evaluation.decision {
                        PolicyDecision::Deny => {
                            let error = Error::PermissionDenied {
                                capability: format!(
                                    "{} destination",
                                    destination_request.operation_name()
                                ),
                            };
                            context.emit(EventPayload::ToolDenied {
                                tool: tool_name,
                                reason: error.to_string(),
                            });
                            return Err(error);
                        }
                        PolicyDecision::Ask => {
                            return Err(Error::PermissionRequired {
                                capability: format!(
                                    "{} destination",
                                    destination_request.operation_name()
                                ),
                                reason: destination_evaluation.reason,
                            });
                        }
                        PolicyDecision::Allow => {}
                    }
                }
                context.emit(EventPayload::ToolApproved {
                    tool: tool_name.clone(),
                    reason: Some(evaluation.rule),
                });
            }
        }
        context.emit(EventPayload::ToolStarted {
            tool: tool_name.clone(),
        });
        let result = tool.execute(context, request);
        match result {
            Ok(mut result) => {
                self.update_read_revisions(context, &tool_name, &result);
                if !result.changed_files.is_empty() {
                    if let Some(index) = &self.repository_index {
                        // A successful write updates the in-memory index before
                        // the next model turn can query it. A later query may
                        // still lazily rebuild the index if this update fails.
                        let _ =
                            index.update_changed(context.working_directory, &result.changed_files);
                        let diagnostic_path = result.changed_files.iter().find(|path| {
                            path.is_file()
                                && matches!(
                                    path.extension().and_then(|extension| extension.to_str()),
                                    Some("rs" | "ts" | "tsx" | "js" | "jsx" | "py")
                                )
                        });
                        if let Some(path) = diagnostic_path {
                            let workspace = fs::canonicalize(context.working_directory)
                                .unwrap_or_else(|_| context.working_directory.to_path_buf());
                            let relative = path
                                .strip_prefix(&workspace)
                                .unwrap_or(path)
                                .to_string_lossy()
                                .replace('\\', "/");
                            let diagnostic_tool = RepositoryTool::new(
                                RepositoryAction::GetDiagnostics,
                                Arc::clone(index),
                            );
                            let diagnostic = diagnostic_tool.execute(
                                context,
                                ToolRequest::new(
                                    "get_diagnostics",
                                    serde_json::json!({"path": relative, "quick": true}),
                                ),
                            );
                            let detail = diagnostic
                                .map(|diagnostic| diagnostic.output)
                                .unwrap_or_else(|error| {
                                    format!("Quick diagnostics unavailable: {error}")
                                });
                            result.metadata.insert(
                                "diagnostics".to_owned(),
                                serde_json::json!({"status":"best_effort","detail":detail}),
                            );
                            if !detail.is_empty() {
                                result.output.push_str("\nDiagnostics: ");
                                result
                                    .output
                                    .push_str(&detail.chars().take(1_500).collect::<String>());
                            }
                        } else {
                            result.metadata.insert(
                                "diagnostics".to_owned(),
                                serde_json::json!({
                                    "status": "repository_index_refreshed",
                                    "detail": "Changed files are ready for symbol and outline queries."
                                }),
                            );
                        }
                    }
                }
                context.emit(EventPayload::ToolOutput {
                    tool: tool_name.clone(),
                    output: concise_event_value(&result.output),
                });
                context.emit(EventPayload::ToolCompleted { tool: tool_name });
                Ok(result)
            }
            Err(error) => {
                context.emit(EventPayload::ToolFailed {
                    tool: tool_name,
                    error: error.to_string(),
                });
                Err(Error::Tool {
                    tool: tool.spec().name,
                    message: error.to_string(),
                })
            }
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.spec().name).collect()
    }

    /// Return a short workspace map from the same cached index used by query
    /// tools. The complete index is never returned to the model.
    pub fn repository_map(&self, root: &std::path::Path) -> Result<String, ToolError> {
        if let Some(index) = &self.repository_index {
            index.repo_map(root)
        } else {
            let index = RepositoryIndex::build(root)?;
            Ok(index.repo_map())
        }
    }

    /// Returns the registered operation for a tool so higher-level runtime
    /// modes can restrict which classes of tools are available and executable.
    pub fn operation_for(&self, name: &str) -> Option<OperationKind> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .map(|tool| tool.operation())
    }

    /// Evaluates a tool request without executing it or emitting events. This
    /// lets the agent run veto-only BeforeTool hooks only after the requested
    /// operation itself is allowed or approved.
    pub fn preflight(
        &self,
        context: &ToolContext<'_>,
        request: &ToolRequest,
    ) -> Result<PolicyEvaluation, Error> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.spec().name == request.name)
            .ok_or_else(|| Error::Tool {
                tool: request.name.clone(),
                message: "tool is not registered".to_owned(),
            })?;
        let policy_request = build_policy_request(context, &request.name, tool.as_ref(), request);
        let evaluation = context.policy.evaluate(&policy_request);
        if evaluation.decision != PolicyDecision::Allow || request.name != "rename_file" {
            return Ok(evaluation);
        }
        let destination = build_policy_request_for_path(
            context,
            &request.name,
            tool.as_ref(),
            request
                .arguments
                .get("destination_path")
                .and_then(serde_json::Value::as_str),
        );
        let destination_evaluation = context.policy.evaluate(&destination);
        if destination_evaluation.decision == PolicyDecision::Allow {
            Ok(evaluation)
        } else {
            Ok(destination_evaluation)
        }
    }

    /// Returns the concrete policy requests for a tool. Rename tools include
    /// both source and destination so an approval can be scoped to the exact
    /// paths involved rather than to the tool name globally.
    pub fn policy_requests(
        &self,
        context: &ToolContext<'_>,
        request: &ToolRequest,
    ) -> Result<Vec<PolicyRequest>, Error> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.spec().name == request.name)
            .ok_or_else(|| Error::Tool {
                tool: request.name.clone(),
                message: "tool is not registered".to_owned(),
            })?;
        let mut requests = vec![build_policy_request(
            context,
            &request.name,
            tool.as_ref(),
            request,
        )];
        if request.name == "rename_file" {
            requests.push(build_policy_request_for_path(
                context,
                &request.name,
                tool.as_ref(),
                request
                    .arguments
                    .get("destination_path")
                    .and_then(serde_json::Value::as_str),
            ));
        }
        Ok(requests)
    }

    fn revision_key(&self, context: &ToolContext<'_>, path: &str) -> String {
        let workspace = fs::canonicalize(context.working_directory)
            .unwrap_or_else(|_| context.working_directory.to_path_buf());
        let session = context
            .session_id
            .map(ToString::to_string)
            .unwrap_or_default();
        let path = path.replace('\\', "/");
        #[cfg(windows)]
        let path = path.to_ascii_lowercase();
        format!("{}\0{session}\0{path}", workspace.to_string_lossy())
    }

    fn attach_read_revision(
        &self,
        context: &ToolContext<'_>,
        tool_name: &str,
        request: &mut ToolRequest,
    ) {
        if !matches!(
            tool_name,
            "write_file"
                | "apply_patch"
                | "replace_text"
                | "replace_range"
                | "delete_file"
                | "rename_file"
        ) {
            return;
        }
        let Some(arguments) = request.arguments.as_object_mut() else {
            return;
        };
        if arguments.contains_key("expected_revision") {
            return;
        }
        let source = if tool_name == "rename_file" {
            arguments.get("source_path")
        } else {
            arguments.get("path")
        }
        .and_then(serde_json::Value::as_str);
        let Some(source) = source else { return };
        let key = self.revision_key(context, source);
        if let Some(revision) = self
            .read_revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
        {
            arguments.insert("expected_revision".to_owned(), serde_json::json!(revision));
        }
    }

    fn update_read_revisions(
        &self,
        context: &ToolContext<'_>,
        tool_name: &str,
        result: &ToolResult,
    ) {
        if result.is_error {
            return;
        }
        let mut revisions = self
            .read_revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tool_name == "read_file" {
            if let (Some(path), Some(revision)) = (
                result
                    .metadata
                    .get("path")
                    .and_then(serde_json::Value::as_str),
                result
                    .metadata
                    .get("revision")
                    .and_then(serde_json::Value::as_str),
            ) {
                revisions.insert(self.revision_key(context, path), revision.to_owned());
            }
            return;
        }
        if let Some(path) = result
            .metadata
            .get("old_file")
            .and_then(serde_json::Value::as_str)
        {
            revisions.remove(&self.revision_key(context, path));
        }
        if let Some(path) = result
            .metadata
            .get("file")
            .and_then(serde_json::Value::as_str)
        {
            let key = self.revision_key(context, path);
            if let Some(revision) = result
                .metadata
                .get("revision")
                .and_then(serde_json::Value::as_str)
            {
                revisions.insert(key, revision.to_owned());
            } else {
                revisions.remove(&key);
            }
        }
    }
}

fn build_policy_request(
    context: &ToolContext<'_>,
    tool_name: &str,
    tool: &dyn Tool,
    request: &ToolRequest,
) -> PolicyRequest {
    let workspace_root = fs::canonicalize(context.working_directory)
        .unwrap_or_else(|_| context.working_directory.to_path_buf());
    let operation = tool.operation();
    let path_argument = request
        .arguments
        .get("path")
        .or_else(|| request.arguments.get("working_directory"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            matches!(operation, OperationKind::Read | OperationKind::Search).then(|| ".".to_owned())
        });
    let path = path_argument.map(|path| workspace_root.join(path));
    let command = request
        .arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    PolicyRequest {
        tool_name: tool_name.to_owned(),
        operation,
        workspace_root,
        path,
        command,
        mode: context.policy.mode(),
    }
}

fn build_policy_request_for_path(
    context: &ToolContext<'_>,
    tool_name: &str,
    tool: &dyn Tool,
    relative_path: Option<&str>,
) -> PolicyRequest {
    let workspace_root = fs::canonicalize(context.working_directory)
        .unwrap_or_else(|_| context.working_directory.to_path_buf());
    let path = relative_path.map(|path| workspace_root.join(path));
    PolicyRequest {
        tool_name: tool_name.to_owned(),
        operation: tool.operation(),
        workspace_root,
        path,
        command: None,
        mode: context.policy.mode(),
    }
}

fn event_arguments(arguments: &serde_json::Value) -> BTreeMap<String, String> {
    arguments
        .as_object()
        .map(|arguments| {
            arguments
                .iter()
                // A string argument is recorded as the string the model asked
                // for. Serialising the `Value` would store its JSON encoding
                // instead, which puts literal quotes in the event log and in
                // every surface that reads it back.
                .map(|(name, value)| {
                    let rendered = match value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    (
                        name.clone(),
                        concise_event_value(&harness_core::redact_sensitive(&rendered)),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn concise_event_value(value: &str) -> String {
    const MAX_EVENT_VALUE_BYTES: usize = 2_048;
    if value.len() <= MAX_EVENT_VALUE_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_EVENT_VALUE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", &value[..end])
}
