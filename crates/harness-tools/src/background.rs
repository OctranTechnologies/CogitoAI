//! Bounded, persistent background command handles for coding-agent tasks.

use std::sync::Arc;
use std::time::Duration;

use harness_policy::{OperationKind, Permission};
use harness_session::{EventPayload, HarnessEvent};
use serde_json::json;

use crate::process::{BackgroundProcessStatus, ProcessManager, ProcessStatusSink};
use crate::{
    filesystem::shell_invocation, ProcessError, Tool, ToolContext, ToolError, ToolRequest,
    ToolResult, ToolSpec,
};

const MAX_READY_WAIT: Duration = Duration::from_secs(60);

pub(crate) struct BackgroundProcessTool {
    manager: Arc<ProcessManager>,
    action: ProcessToolAction,
}

#[derive(Clone, Copy)]
pub(crate) enum ProcessToolAction {
    Start,
    ReadOutput,
    List,
    Stop,
    WaitForOutput,
}

impl BackgroundProcessTool {
    pub(crate) fn new(manager: Arc<ProcessManager>, action: ProcessToolAction) -> Self {
        Self { manager, action }
    }

    fn spec_for(&self) -> ToolSpec {
        match self.action {
            ProcessToolAction::Start => ToolSpec {
                name: "start_background_command".to_owned(),
                description:
                    "Start a bounded background command and return its persistent process handle"
                        .to_owned(),
                arguments_schema: json!({
                    "type":"object", "required":["command"], "additionalProperties":false,
                    "properties":{
                        "command":{"type":"string","minLength":1},
                        "shell":{"type":"string","enum":["auto","bash","sh","cmd"]},
                        "working_directory":{"type":"string"},
                        "timeout_ms":{"type":"integer","minimum":0,"maximum":86400000}
                    }
                }),
            },
            ProcessToolAction::ReadOutput => ToolSpec {
                name: "read_process_output".to_owned(),
                description:
                    "Read new bounded stdout/stderr logs for a background process using its cursor"
                        .to_owned(),
                arguments_schema: json!({
                    "type":"object", "required":["process_id"], "additionalProperties":false,
                    "properties":{
                        "process_id":{"type":"string"},
                        "after_cursor":{"type":"integer","minimum":0},
                        "max_bytes":{"type":"integer","minimum":1,"maximum":65536}
                    }
                }),
            },
            ProcessToolAction::List => ToolSpec {
                name: "list_processes".to_owned(),
                description: "Inspect active and recently completed background commands".to_owned(),
                arguments_schema: json!({"type":"object","additionalProperties":false,"properties":{}}),
            },
            ProcessToolAction::Stop => ToolSpec {
                name: "stop_process".to_owned(),
                description: "Terminate a background process and its child process tree".to_owned(),
                arguments_schema: json!({
                    "type":"object", "required":["process_id"], "additionalProperties":false,
                    "properties":{"process_id":{"type":"string"}}
                }),
            },
            ProcessToolAction::WaitForOutput => ToolSpec {
                name: "wait_for_process_output".to_owned(),
                description: "Wait for a background process to print a readiness marker or exit"
                    .to_owned(),
                arguments_schema: json!({
                    "type":"object", "required":["process_id","pattern"], "additionalProperties":false,
                    "properties":{
                        "process_id":{"type":"string"}, "pattern":{"type":"string","minLength":1},
                        "timeout_ms":{"type":"integer","minimum":1,"maximum":60000}
                    }
                }),
            },
        }
    }
}

impl Tool for BackgroundProcessTool {
    fn spec(&self) -> ToolSpec {
        self.spec_for()
    }

    fn required_permission(&self) -> Permission {
        match self.action {
            ProcessToolAction::ReadOutput | ProcessToolAction::List => Permission::ReadWorkspace,
            ProcessToolAction::Start | ProcessToolAction::Stop => Permission::ExecuteCommand,
            ProcessToolAction::WaitForOutput => Permission::ReadWorkspace,
        }
    }

    fn operation(&self) -> OperationKind {
        match self.action {
            ProcessToolAction::ReadOutput
            | ProcessToolAction::List
            | ProcessToolAction::WaitForOutput => OperationKind::Read,
            ProcessToolAction::Start | ProcessToolAction::Stop => OperationKind::Command,
        }
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            ProcessToolAction::Start => {
                let command_text = string_arg(&request, "command")?;
                let shell = optional_string(&request, "shell", "auto")?;
                let (program, args) = shell_invocation(&shell, &command_text)?;
                let relative = optional_string(&request, "working_directory", ".")?;
                let (_workspace, cwd) = crate::filesystem::resolve_existing(
                    context,
                    &relative,
                    "start_background_command",
                )?;
                let timeout_ms = optional_u64(&request, "timeout_ms", 0, 0, 86_400_000)?;
                let timeout = (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms));
                let cancellation = context.cancellation.cloned().unwrap_or_default();
                let status_sink = process_status_sink(context);
                let info = self
                    .manager
                    .start(crate::process::ProcessStartRequest {
                        display_command: command_text,
                        program: program.to_owned(),
                        args,
                        working_directory: cwd,
                        timeout,
                        cancellation,
                        status_sink,
                    })
                    .map_err(process_tool_error)?;
                let mut result = ToolResult::new(format!("Started background process {} (PID {}). Use read_process_output, list_processes, and stop_process with this handle.", info.id, info.pid));
                result.metadata.insert(
                    "process".to_owned(),
                    serde_json::to_value(info).unwrap_or_default(),
                );
                Ok(result)
            }
            ProcessToolAction::ReadOutput => {
                let id = string_arg(&request, "process_id")?;
                let after = optional_u64(&request, "after_cursor", 0, 0, u64::MAX)?;
                let max = optional_u64(&request, "max_bytes", 16 * 1024, 1, 64 * 1024)? as usize;
                let batch = self
                    .manager
                    .read_output(&id, after, max)
                    .map_err(process_tool_error)?;
                let mut result = ToolResult::new(batch.text);
                result.metadata.insert("process_id".to_owned(), json!(id));
                result
                    .metadata
                    .insert("next_cursor".to_owned(), json!(batch.next_cursor));
                result
                    .metadata
                    .insert("truncated".to_owned(), json!(batch.truncated));
                result
                    .metadata
                    .insert("status".to_owned(), json!(batch.status));
                Ok(result)
            }
            ProcessToolAction::List => {
                let processes = self.manager.list();
                let output = if processes.is_empty() {
                    "No background processes.".to_owned()
                } else {
                    processes
                        .iter()
                        .map(|p| {
                            format!(
                                "{} · {} · {} · PID {} · {}",
                                p.id, p.status, p.command, p.pid, p.working_directory
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let mut result = ToolResult::new(output);
                result.metadata.insert(
                    "processes".to_owned(),
                    serde_json::to_value(processes).unwrap_or_default(),
                );
                Ok(result)
            }
            ProcessToolAction::Stop => {
                let id = string_arg(&request, "process_id")?;
                let info = self.manager.stop(&id).map_err(process_tool_error)?;
                let mut result =
                    ToolResult::new(format!("Process {} is {}.", info.id, info.status));
                result.metadata.insert(
                    "process".to_owned(),
                    serde_json::to_value(info).unwrap_or_default(),
                );
                Ok(result)
            }
            ProcessToolAction::WaitForOutput => {
                let id = string_arg(&request, "process_id")?;
                let pattern = string_arg(&request, "pattern")?;
                let timeout = Duration::from_millis(optional_u64(
                    &request,
                    "timeout_ms",
                    15_000,
                    1,
                    MAX_READY_WAIT.as_millis() as u64,
                )?);
                let cancellation = context.cancellation.cloned().unwrap_or_default();
                let ready = self
                    .manager
                    .wait_for_output(&id, &pattern, timeout, &cancellation)
                    .map_err(process_tool_error)?;
                let mut result = ToolResult::new(if ready.ready {
                    format!("Process {} emitted readiness marker {:?}.", id, pattern)
                } else {
                    format!(
                        "Process {} did not emit readiness marker {:?} before {}; status: {}.",
                        id,
                        pattern,
                        if ready.cancelled {
                            "cancellation"
                        } else {
                            "timeout or exit"
                        },
                        ready.process.status
                    )
                });
                result.is_error = !ready.ready;
                result
                    .metadata
                    .insert("ready".to_owned(), json!(ready.ready));
                result.metadata.insert(
                    "process".to_owned(),
                    serde_json::to_value(ready.process).unwrap_or_default(),
                );
                Ok(result)
            }
        }
    }
}

fn string_arg(request: &ToolRequest, key: &str) -> Result<String, ToolError> {
    request
        .arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ToolError::InvalidArguments {
            tool: request.name.clone(),
            message: format!("{key} must be a non-empty string"),
        })
}

fn optional_string(request: &ToolRequest, key: &str, default: &str) -> Result<String, ToolError> {
    request.arguments.get(key).map_or_else(
        || Ok(default.to_owned()),
        |value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| ToolError::InvalidArguments {
                    tool: request.name.clone(),
                    message: format!("{key} must be a string"),
                })
        },
    )
}

fn optional_u64(
    request: &ToolRequest,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ToolError> {
    let value = request
        .arguments
        .get(key)
        .map_or(Some(default), serde_json::Value::as_u64)
        .filter(|v| *v >= min && *v <= max);
    value.ok_or_else(|| ToolError::InvalidArguments {
        tool: request.name.clone(),
        message: format!("{key} must be between {min} and {max}"),
    })
}

fn process_tool_error(error: ProcessError) -> ToolError {
    ToolError::Process {
        message: error.to_string(),
    }
}

fn process_status_sink(context: &ToolContext<'_>) -> Option<ProcessStatusSink> {
    let event_bus = context.event_bus?.clone();
    let session_id = context.session_id?.clone();
    let correlation_id = context.correlation_id.cloned();
    Some(Arc::new(move |process| {
        let payload = if process.status == BackgroundProcessStatus::Running {
            EventPayload::BackgroundProcessStarted {
                process_id: process.id,
                command: process.command,
                working_directory: process.working_directory.into(),
                pid: process.pid,
                started_at_unix_ms: process.started_at_unix_ms,
            }
        } else {
            EventPayload::BackgroundProcessStatus {
                process_id: process.id,
                pid: process.pid,
                status: serde_json::to_value(process.status)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".to_owned()),
                exit_code: process.exit_code,
                timed_out: process.timed_out,
            }
        };
        event_bus.publish(&HarnessEvent::new(
            session_id.clone(),
            payload,
            None,
            correlation_id.clone(),
        ));
    }))
}
