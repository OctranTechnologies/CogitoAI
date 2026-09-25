mod filesystem;
mod process;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use harness_core::{Error, Id, SessionId};
use harness_policy::{OperationKind, Permission, Policy, PolicyDecision, PolicyRequest};
use harness_session::{EventBus, EventPayload, HarnessEvent};
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

pub use filesystem::{
    ApplyPatchTool, GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ShellTool, WriteFileTool,
};
pub use process::{
    CancellationToken, LocalProcessRunner, ProcessError, ProcessEvent, ProcessRequest,
    ProcessResult, ProcessRunner,
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
}

impl ToolResult {
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            metadata: BTreeMap::new(),
            changed_files: Vec::new(),
            truncated: false,
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
}

pub struct ToolContext<'a> {
    pub policy: &'a dyn Policy,
    pub working_directory: &'a std::path::Path,
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
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_workspace_tools() -> Self {
        Self::with_workspace_tools_cancellation(CancellationToken::new())
    }

    pub fn with_workspace_tools_cancellation(cancellation: CancellationToken) -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ReadFileTool));
        registry.register(Box::new(WriteFileTool));
        registry.register(Box::new(ApplyPatchTool));
        registry.register(Box::new(ListDirectoryTool));
        registry.register(Box::new(GlobTool));
        registry.register(Box::new(GrepTool));
        registry.register(Box::new(ShellTool::new(
            Arc::new(LocalProcessRunner),
            cancellation,
        )));
        registry
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
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
        let policy_request = build_policy_request(context, &tool_name, tool.as_ref(), &request);
        let evaluation = context.policy.evaluate(&policy_request);
        context.emit(EventPayload::PolicyDecision {
            tool: tool_name.clone(),
            action: evaluation.decision.to_string(),
            reason: evaluation.reason.clone(),
            rule: evaluation.rule.clone(),
            operation: policy_request.operation_name().to_owned(),
            mode: format!("{:?}", policy_request.mode).to_ascii_lowercase(),
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
            Ok(result) => {
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

fn event_arguments(arguments: &serde_json::Value) -> BTreeMap<String, String> {
    arguments
        .as_object()
        .map(|arguments| {
            arguments
                .iter()
                .map(|(name, value)| (name.clone(), concise_event_value(&value.to_string())))
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
