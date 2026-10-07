use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use globset::Glob;
use harness_policy::{OperationKind, Permission};
use regex::RegexBuilder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{
    CancellationToken, ProcessError, ProcessEvent, ProcessRequest, ProcessRunner, Tool,
    ToolContext, ToolError, ToolRequest, ToolResult, ToolSpec,
};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_EDIT_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DIFF_BYTES: usize = 16 * 1024;
const MAX_RESULTS: usize = 200;
const MAX_SCAN_FILES: usize = 20_000;
const MAX_SCAN_DEPTH: usize = 16;

static EDIT_MUTEX: Mutex<()> = Mutex::new(());

pub struct ShellTool {
    runner: Option<Arc<dyn ProcessRunner>>,
    cancellation: CancellationToken,
    name: &'static str,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            runner: None,
            cancellation: CancellationToken::new(),
            name: "shell",
        }
    }
}

impl ShellTool {
    pub fn new(runner: Arc<dyn ProcessRunner>, cancellation: CancellationToken) -> Self {
        Self {
            runner: Some(runner),
            cancellation,
            name: "shell",
        }
    }

    pub fn with_cancellation(cancellation: CancellationToken) -> Self {
        Self {
            runner: None,
            cancellation,
            name: "shell",
        }
    }

    pub fn named(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_owned(),
            description: "Run an explicitly approved command in the workspace shell".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "minLength": 1 },
                    "shell": { "type": "string", "enum": ["auto", "bash", "sh", "cmd"] },
                    "working_directory": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 600000 },
                    "max_output_bytes": { "type": "integer", "minimum": 0, "maximum": 1048576 }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ExecuteCommand
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Command
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let command_text = required_string(&request, "command", "shell")?;
        let shell = optional_string(&request, "shell", "auto")?.to_ascii_lowercase();
        let (program, args) = shell_invocation(&shell, &command_text)?;
        let working_directory = optional_string(&request, "working_directory", ".")?;
        let (workspace_root, working_directory) =
            resolve_existing(context, &working_directory, "shell")?;
        let timeout_ms = optional_u64(&request, "timeout_ms", 30_000, 1, 600_000, "shell")?;
        let max_output_bytes = optional_usize(
            &request,
            "max_output_bytes",
            64 * 1024,
            0,
            1024 * 1024,
            "shell",
        )?;
        let event_working_directory = working_directory.clone();
        let request = ProcessRequest {
            program: program.to_owned(),
            args,
            working_directory,
            timeout: Duration::from_millis(timeout_ms),
            max_output_bytes,
        };
        let cancellation = context.cancellation.unwrap_or(&self.cancellation);
        let mut on_event = |event| {
            emit_process_event(
                context,
                &event,
                &command_text,
                &event_working_directory,
                timeout_ms,
            );
            Ok(())
        };
        let result = if let Some(runner) = &self.runner {
            runner.execute(request, cancellation, &mut on_event)
        } else {
            context.execution_environment.spawn_process(
                &workspace_root,
                request,
                cancellation,
                &mut on_event,
            )
        }
        .map_err(|error: ProcessError| ToolError::Process {
            message: error.to_string(),
        })?;
        let mut output = String::new();
        if !result.stdout.is_empty() {
            output.push_str("stdout:\n");
            output.push_str(&result.stdout);
        }
        if !result.stderr.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("stderr:\n");
            output.push_str(&result.stderr);
        }
        if result.timed_out {
            output.push_str("\nprocess timed out");
        } else if result.cancelled {
            output.push_str("\nprocess cancelled");
        } else if !result.success {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!(
                "command failed with exit code {:?}",
                result.exit_code
            ));
        }
        let mut tool_result = ToolResult::new(output);
        tool_result.is_error = !result.success;
        tool_result
            .metadata
            .insert("exit_code".to_owned(), json!(result.exit_code));
        tool_result
            .metadata
            .insert("success".to_owned(), json!(result.success));
        tool_result
            .metadata
            .insert("timed_out".to_owned(), json!(result.timed_out));
        tool_result
            .metadata
            .insert("cancelled".to_owned(), json!(result.cancelled));
        tool_result
            .metadata
            .insert("duration_ms".to_owned(), json!(result.duration_ms));
        tool_result
            .metadata
            .insert("stdout_bytes".to_owned(), json!(result.stdout.len()));
        tool_result
            .metadata
            .insert("stderr_bytes".to_owned(), json!(result.stderr.len()));
        Ok(tool_result)
    }
}

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a UTF-8 text file from the workspace. For a large file, request a bounded 1-based start_line/end_line range.".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Read
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "read_file")?;
        let (root, resolved) = resolve_existing(context, &path, "read_file")?;
        let start = request
            .arguments
            .get("start_line")
            .map(|_| required_line(&request, "start_line", "read_file"))
            .transpose()?;
        let end = request
            .arguments
            .get("end_line")
            .map(|_| required_line(&request, "end_line", "read_file"))
            .transpose()?;
        if start.is_some() != end.is_some() {
            return Err(invalid_arguments(
                "read_file",
                "start_line and end_line must be provided together",
            ));
        }
        let size = file_size(&resolved);
        if size > MAX_FILE_BYTES && start.is_none() {
            return Err(ToolError::FileTooLarge {
                path: resolved,
                limit: MAX_FILE_BYTES,
            });
        }
        let read_limit = if start.is_some() {
            MAX_EDIT_FILE_BYTES
        } else {
            MAX_FILE_BYTES
        };
        let (bytes, full_text) = read_text_snapshot(
            context.execution_environment,
            &root,
            &resolved,
            "read_file",
            read_limit,
        )?;
        let text = if let (Some(start), Some(end)) = (start, end) {
            if end < start {
                return Err(invalid_arguments(
                    "read_file",
                    "end_line must be greater than or equal to start_line",
                ));
            }
            let ranges = line_byte_ranges(&full_text);
            if start > ranges.len() || end > ranges.len() {
                return Err(invalid_arguments(
                    "read_file",
                    "requested line range is outside the file",
                ));
            }
            let selected = &full_text[ranges[start - 1].0..ranges[end - 1].1];
            if selected.len() as u64 > MAX_FILE_BYTES {
                return Err(ToolError::FileTooLarge {
                    path: resolved,
                    limit: MAX_FILE_BYTES,
                });
            }
            selected.to_owned()
        } else {
            full_text
        };
        let mut result = ToolResult::new(text);
        result
            .metadata
            .insert("path".to_owned(), json!(relative_string(&root, &resolved)));
        result
            .metadata
            .insert("bytes".to_owned(), json!(bytes.len()));
        result
            .metadata
            .insert("revision".to_owned(), json!(content_revision(&bytes)));
        if let Some(start) = start {
            result
                .metadata
                .insert("start_line".to_owned(), json!(start));
        }
        if let Some(end) = end {
            result.metadata.insert("end_line".to_owned(), json!(end));
        }
        Ok(result)
    }
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_owned(),
            description: "Create or replace a UTF-8 text file atomically. If the file was read this session, its revision is checked before writing; prefer apply_patch or replace_text for focused edits.".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "expected_revision": { "type": "string", "description": "Revision returned by read_file; the runtime also supplies this automatically when available." }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Write
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "write_file")?;
        let content = required_text(&request, "content", "write_file")?;
        if content.len() as u64 > MAX_EDIT_FILE_BYTES {
            return Err(ToolError::FileTooLarge {
                path: PathBuf::from(path),
                limit: MAX_EDIT_FILE_BYTES,
            });
        }
        let (root, resolved) = resolve_for_write(context, &path, "write_file")?;
        if resolved.exists() && !resolved.is_file() {
            return Err(ToolError::NotFile { path: resolved });
        }
        let _guard = EDIT_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = if resolved.exists() {
            Some(load_editable(
                context.execution_environment,
                &root,
                &resolved,
                "write_file",
            )?)
        } else {
            None
        };
        let expected = optional_string(&request, "expected_revision", "")?;
        if let (Some(original), true) = (&original, !expected.is_empty()) {
            if content_revision(&original.raw) != expected {
                return Ok(conflict_result(
                    &root,
                    &resolved,
                    "stale",
                    "The file changed after it was read. Read the latest contents before editing.",
                ));
            }
        }
        let expected_snapshot = original.as_ref().map(|value| value.raw.clone());
        let existed = original.is_some();
        let eol = original
            .as_ref()
            .map(|value| value.eol.as_str())
            .unwrap_or("\n");
        let bom = original.as_ref().is_some_and(|value| value.bom);
        let content = encode_new_text(&content, eol, bom);
        ensure_edit_not_cancelled(context)?;
        let relative =
            resolved
                .strip_prefix(&root)
                .map_err(|_| ToolError::PathOutsideWorkspace {
                    path: resolved.clone(),
                })?;
        let committed = if let Some(snapshot) = expected_snapshot.as_deref() {
            context
                .execution_environment
                .write_workspace_file_atomic(&root, relative, Some(snapshot), content.as_bytes())
                .map_err(|error| io_error("atomically replace file", error))?
        } else {
            context
                .execution_environment
                .create_workspace_file_atomic(&root, relative, content.as_bytes())
                .map_err(|error| io_error("atomically create file", error))?
        };
        if !committed {
            return Ok(conflict_result(
                &root,
                &resolved,
                "concurrent_modification",
                "The destination changed while the write was being prepared.",
            ));
        }
        context.emit(harness_session::EventPayload::FileChanged {
            path: resolved.clone(),
            change: if existed {
                harness_session::FileChange::Modified
            } else {
                harness_session::FileChange::Added
            },
        });
        let old = original.as_ref().map(|value| value.raw.as_slice());
        let mut result = edit_result(&root, &resolved, old, Some(content.as_bytes()), "wrote");
        result.changed_files.push(resolved);
        Ok(result)
    }
}

pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".to_owned(),
            description: "Apply a focused exact-context patch to a UTF-8 workspace file. Context must occur exactly once; stale revisions and ambiguous matches are rejected. Returns a bounded unified diff.".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string", "minLength": 1 },
                    "new_text": { "type": "string" },
                    "expected_revision": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Patch
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "apply_patch")?;
        replace_exact(context, &request, &path, "apply_patch")
    }
}

pub struct CreateFileTool;

impl Tool for CreateFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "create_file".to_owned(),
            description:
                "Create a new UTF-8 file atomically. Fails if the destination already exists."
                    .to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                "required": ["path", "content"], "additionalProperties": false
            }),
        }
    }
    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }
    fn operation(&self) -> OperationKind {
        OperationKind::Write
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "create_file")?;
        let content = required_text(&request, "content", "create_file")?;
        if content.len() as u64 > MAX_EDIT_FILE_BYTES {
            return Err(ToolError::FileTooLarge {
                path: PathBuf::from(path),
                limit: MAX_EDIT_FILE_BYTES,
            });
        }
        let (root, resolved) = resolve_for_write(context, &path, "create_file")?;
        let _guard = EDIT_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if resolved.exists() {
            return Ok(conflict_result(
                &root,
                &resolved,
                "stale",
                "The destination already exists. Read it and use a focused edit instead.",
            ));
        }
        ensure_edit_not_cancelled(context)?;
        let relative =
            resolved
                .strip_prefix(&root)
                .map_err(|_| ToolError::PathOutsideWorkspace {
                    path: resolved.clone(),
                })?;
        context
            .execution_environment
            .create_workspace_file_atomic(&root, relative, content.as_bytes())
            .map_err(|error| io_error("atomically create file", error))?;
        context.emit(harness_session::EventPayload::FileChanged {
            path: resolved.clone(),
            change: harness_session::FileChange::Added,
        });
        let mut result = edit_result(&root, &resolved, None, Some(content.as_bytes()), "created");
        result.changed_files.push(resolved);
        Ok(result)
    }
}

pub struct ReplaceTextTool;
impl Tool for ReplaceTextTool {
    fn spec(&self) -> ToolSpec {
        let mut spec = ApplyPatchTool.spec();
        spec.name = "replace_text".to_owned();
        spec.description =
            "Replace one exact text range. The old text must match exactly once.".to_owned();
        spec
    }
    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }
    fn operation(&self) -> OperationKind {
        OperationKind::Patch
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "replace_text")?;
        replace_exact(context, &request, &path, "replace_text")
    }
}

pub struct ReplaceRangeTool;
impl Tool for ReplaceRangeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "replace_range".to_owned(),
            description: "Replace an inclusive 1-based line range after verifying expected_text. Use read_file revision to reject stale edits.".to_owned(),
            arguments_schema: json!({
                "type":"object","properties":{
                    "path":{"type":"string"},"start_line":{"type":"integer","minimum":1},
                    "end_line":{"type":"integer","minimum":1},"expected_text":{"type":"string"},
                    "new_text":{"type":"string"},"expected_revision":{"type":"string"}
                },"required":["path","start_line","end_line","expected_text","new_text"],"additionalProperties":false
            }),
        }
    }
    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }
    fn operation(&self) -> OperationKind {
        OperationKind::Patch
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "replace_range")?;
        let start = required_line(&request, "start_line", "replace_range")?;
        let end = required_line(&request, "end_line", "replace_range")?;
        if end < start {
            return Err(invalid_arguments(
                "replace_range",
                "end_line must be greater than or equal to start_line",
            ));
        }
        let expected_text = required_text(&request, "expected_text", "replace_range")?;
        let new_text = required_text(&request, "new_text", "replace_range")?;
        let (root, resolved) = resolve_existing(context, &path, "replace_range")?;
        let _guard = EDIT_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = load_editable(
            context.execution_environment,
            &root,
            &resolved,
            "replace_range",
        )?;
        if let Some(conflict) = stale_revision(&request, &root, &resolved, &original) {
            return Ok(conflict);
        }
        let (mapping_text, offsets) = normalized_offset_map(&original.text);
        let lines = line_byte_ranges(&mapping_text);
        if start > lines.len() || end > lines.len() {
            return Ok(conflict_result(
                &root,
                &resolved,
                "stale",
                "The requested line range no longer exists.",
            ));
        }
        let start_offset = lines[start - 1].0;
        let end_offset = lines[end - 1].1;
        let expected = normalize_eol(&expected_text);
        if mapping_text.get(start_offset..end_offset) != Some(expected.as_str()) {
            return Ok(conflict_result(&root, &resolved, "stale", "The selected lines differ from expected_text. Read the latest file before editing."));
        }
        let raw_start = offsets[start_offset];
        let raw_end = offsets[end_offset];
        let replacement = normalize_eol(&new_text).replace('\n', &original.eol);
        let updated_text = format!(
            "{}{}{}",
            &original.text[..raw_start],
            replacement,
            &original.text[raw_end..]
        );
        commit_existing(
            context,
            &root,
            &resolved,
            original,
            updated_text,
            "replaced range",
        )
    }
}

pub struct DeleteFileTool;
impl Tool for DeleteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delete_file".to_owned(),
            description: "Delete a workspace file after checking its read revision.".to_owned(),
            arguments_schema: json!({"type":"object","properties":{"path":{"type":"string"},"expected_revision":{"type":"string"}},"required":["path"],"additionalProperties":false}),
        }
    }
    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }
    fn operation(&self) -> OperationKind {
        OperationKind::Write
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "delete_file")?;
        let (root, resolved) = resolve_existing(context, &path, "delete_file")?;
        let _guard = EDIT_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = load_editable(
            context.execution_environment,
            &root,
            &resolved,
            "delete_file",
        )?;
        if let Some(conflict) = stale_revision(&request, &root, &resolved, &original) {
            return Ok(conflict);
        }
        ensure_edit_not_cancelled(context)?;
        let relative =
            resolved
                .strip_prefix(&root)
                .map_err(|_| ToolError::PathOutsideWorkspace {
                    path: resolved.clone(),
                })?;
        if !context
            .execution_environment
            .delete_workspace_file(&root, relative, &original.raw)
            .map_err(|error| io_error("delete file", error))?
        {
            return Ok(conflict_result(
                &root,
                &resolved,
                "concurrent_modification",
                "The file changed during deletion. No change was made.",
            ));
        }
        context.emit(harness_session::EventPayload::FileChanged {
            path: resolved.clone(),
            change: harness_session::FileChange::Deleted,
        });
        let mut result = edit_result(&root, &resolved, Some(&original.raw), None, "deleted");
        result.changed_files.push(resolved);
        Ok(result)
    }
}

pub struct RenameFileTool;
impl Tool for RenameFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "rename_file".to_owned(),
            description:
                "Move or rename a workspace file atomically. The destination must not exist."
                    .to_owned(),
            arguments_schema: json!({"type":"object","properties":{"source_path":{"type":"string"},"destination_path":{"type":"string"},"expected_revision":{"type":"string"}},"required":["source_path","destination_path"],"additionalProperties":false}),
        }
    }
    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }
    fn operation(&self) -> OperationKind {
        OperationKind::Write
    }
    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let source = required_string(&request, "source_path", "rename_file")?;
        let destination = required_string(&request, "destination_path", "rename_file")?;
        let (root, from) = resolve_existing(context, &source, "rename_file")?;
        let (_, to) = resolve_for_write(context, &destination, "rename_file")?;
        if !from.is_file() {
            return Err(ToolError::NotFile { path: from });
        }
        let _guard = EDIT_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = load_editable(context.execution_environment, &root, &from, "rename_file")?;
        if let Some(conflict) = stale_revision(&request, &root, &from, &original) {
            return Ok(conflict);
        }
        if to.exists() {
            return Ok(conflict_result(
                &root,
                &to,
                "stale",
                "The destination already exists.",
            ));
        }
        ensure_edit_not_cancelled(context)?;
        let from_relative = from
            .strip_prefix(&root)
            .map_err(|_| ToolError::PathOutsideWorkspace { path: from.clone() })?;
        let to_relative = to
            .strip_prefix(&root)
            .map_err(|_| ToolError::PathOutsideWorkspace { path: to.clone() })?;
        if !context
            .execution_environment
            .move_workspace_file(&root, from_relative, to_relative, &original.raw)
            .map_err(|error| io_error("move file", error))?
        {
            return Ok(conflict_result(
                &root,
                &from,
                "concurrent_modification",
                "The source changed or destination appeared while preparing the move.",
            ));
        }
        context.emit(harness_session::EventPayload::FileChanged {
            path: from.clone(),
            change: harness_session::FileChange::Deleted,
        });
        context.emit(harness_session::EventPayload::FileChanged {
            path: to.clone(),
            change: harness_session::FileChange::Added,
        });
        let mut result = rename_result(&root, &from, &to, &original.raw);
        result.changed_files.extend([from, to]);
        Ok(result)
    }
}

pub struct ListDirectoryTool;

impl Tool for ListDirectoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_directory".to_owned(),
            description: "List immediate entries in a workspace directory".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "max_entries": { "type": "integer", "minimum": 1, "maximum": 200 }
                },
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Read
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = optional_string(&request, "path", ".")?;
        let limit = optional_limit(&request, "max_entries", MAX_RESULTS)?;
        let (root, directory) = resolve_existing(context, &path, "list_directory")?;
        if !directory.is_dir() {
            return Err(ToolError::NotDirectory { path: directory });
        }
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| io_error("list directory", error))?
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let kind = if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    "dir"
                } else {
                    "file"
                };
                format!("{kind}\t{name}")
            })
            .collect::<Vec<_>>();
        entries.sort();
        let truncated = entries.len() > limit;
        entries.truncate(limit);
        let mut result = ToolResult::new(entries.join("\n"));
        result
            .metadata
            .insert("path".to_owned(), json!(relative_string(&root, &directory)));
        result
            .metadata
            .insert("count".to_owned(), json!(entries.len()));
        result.truncated = truncated;
        Ok(result)
    }
}

pub struct GlobTool;

impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".to_owned(),
            description: "Find workspace files matching a glob pattern".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "minLength": 1 },
                    "path": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 200 }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Read
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let pattern = required_string(&request, "pattern", "glob")?;
        let path = optional_string(&request, "path", ".")?;
        let limit = optional_limit(&request, "max_results", MAX_RESULTS)?;
        let (_root, base) = resolve_existing(context, &path, "glob")?;
        let matcher = Glob::new(&pattern)
            .map_err(|error| invalid_arguments("glob", &error.to_string()))?
            .compile_matcher();
        let files = collect_files(&base);
        let mut matches = files
            .into_iter()
            .filter_map(|file| {
                let relative = file.strip_prefix(&base).ok()?;
                let relative = relative.to_string_lossy().replace('\\', "/");
                matcher.is_match(&relative).then_some(relative)
            })
            .collect::<Vec<_>>();
        matches.sort();
        if matches.is_empty() {
            return Err(ToolError::NoMatches);
        }
        let truncated = matches.len() > limit;
        matches.truncate(limit);
        let mut result = ToolResult::new(matches.join("\n"));
        result.metadata.insert("pattern".to_owned(), json!(pattern));
        result
            .metadata
            .insert("count".to_owned(), json!(matches.len()));
        result.truncated = truncated;
        Ok(result)
    }
}

pub struct GrepTool;

impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".to_owned(),
            description: "Search UTF-8 workspace files with a regular expression".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "minLength": 1 },
                    "path": { "type": "string" },
                    "glob": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "case_sensitive": { "type": "boolean" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Read
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let pattern = required_string(&request, "pattern", "grep")?;
        let path = optional_string(&request, "path", ".")?;
        let filter = optional_string(&request, "glob", "*")?;
        let limit = optional_limit(&request, "max_results", MAX_RESULTS)?;
        let case_sensitive = optional_bool(&request, "case_sensitive", true)?;
        let (workspace_root, base) = resolve_existing(context, &path, "grep")?;
        let matcher = Glob::new(&filter)
            .map_err(|error| invalid_arguments("grep", &error.to_string()))?
            .compile_matcher();
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|error| invalid_arguments("grep", &error.to_string()))?;
        let mut output = Vec::new();
        let mut truncated = false;
        for file in collect_files(&base) {
            let relative = match file.strip_prefix(&base) {
                Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if !matcher.is_match(&relative) {
                continue;
            }
            let Ok(contents) = read_text(
                context.execution_environment,
                &workspace_root,
                &file,
                "grep",
            ) else {
                continue;
            };
            for (line_number, line) in contents.lines().enumerate() {
                if regex.is_match(line) {
                    if output.len() == limit {
                        truncated = true;
                        break;
                    }
                    output.push(format!(
                        "{}:{}:{}",
                        relative,
                        line_number + 1,
                        truncate_line(line)
                    ));
                }
            }
            if truncated {
                break;
            }
        }
        if output.is_empty() {
            return Err(ToolError::NoMatches);
        }
        let mut result = ToolResult::new(output.join("\n"));
        result.metadata.insert("pattern".to_owned(), json!(pattern));
        result
            .metadata
            .insert("count".to_owned(), json!(output.len()));
        result.truncated = truncated;
        Ok(result)
    }
}

pub(crate) fn shell_invocation(
    shell: &str,
    command: &str,
) -> Result<(&'static str, Vec<String>), ToolError> {
    match shell {
        "auto" => {
            #[cfg(windows)]
            {
                Ok(("cmd", vec!["/C".to_owned(), command.to_owned()]))
            }
            #[cfg(not(windows))]
            {
                Ok(("sh", vec!["-lc".to_owned(), command.to_owned()]))
            }
        }
        "bash" => Ok(("bash", vec!["-lc".to_owned(), command.to_owned()])),
        "sh" => Ok(("sh", vec!["-lc".to_owned(), command.to_owned()])),
        "cmd" => Ok(("cmd", vec!["/C".to_owned(), command.to_owned()])),
        _ => Err(invalid_arguments(
            "shell",
            "shell must be one of auto, bash, sh, or cmd",
        )),
    }
}

fn emit_process_event(
    context: &ToolContext<'_>,
    event: &ProcessEvent,
    command: &str,
    working_directory: &Path,
    timeout_ms: u64,
) {
    match event {
        ProcessEvent::Started { .. } => {
            context.emit(harness_session::EventPayload::ProcessStarted {
                command: command.to_owned(),
                working_directory: working_directory.to_path_buf(),
                timeout_ms,
            })
        }
        ProcessEvent::Stdout { chunk } => {
            context.emit(harness_session::EventPayload::ProcessStdout {
                chunk: truncate_event(chunk),
            })
        }
        ProcessEvent::Stderr { chunk } => {
            context.emit(harness_session::EventPayload::ProcessStderr {
                chunk: truncate_event(chunk),
            })
        }
        ProcessEvent::Exited { result } => {
            context.emit(harness_session::EventPayload::ProcessExited {
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                cancelled: result.cancelled,
            })
        }
    }
}

fn optional_u64(
    request: &ToolRequest,
    key: &str,
    default: u64,
    minimum: u64,
    maximum: u64,
    tool: &str,
) -> Result<u64, ToolError> {
    request.arguments.get(key).map_or(Ok(default), |value| {
        value
            .as_u64()
            .filter(|value| (minimum..=maximum).contains(value))
            .ok_or_else(|| {
                invalid_arguments(
                    tool,
                    &format!("{key} must be between {minimum} and {maximum}"),
                )
            })
    })
}

fn optional_usize(
    request: &ToolRequest,
    key: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
    tool: &str,
) -> Result<usize, ToolError> {
    request.arguments.get(key).map_or(Ok(default), |value| {
        value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (minimum..=maximum).contains(value))
            .ok_or_else(|| {
                invalid_arguments(
                    tool,
                    &format!("{key} must be between {minimum} and {maximum}"),
                )
            })
    })
}

fn truncate_event(value: &str) -> String {
    const MAX_EVENT_BYTES: usize = 2_048;
    if value.len() <= MAX_EVENT_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_EVENT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", &value[..end])
}

fn required_path(request: &ToolRequest, tool: &str) -> Result<String, ToolError> {
    required_string(request, "path", tool)
}

fn required_string(request: &ToolRequest, key: &str, tool: &str) -> Result<String, ToolError> {
    request
        .arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid_arguments(tool, &format!("{key} must be a non-empty string")))
}

fn required_text(request: &ToolRequest, key: &str, tool: &str) -> Result<String, ToolError> {
    request
        .arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_arguments(tool, &format!("{key} must be a string")))
}

fn optional_string(request: &ToolRequest, key: &str, default: &str) -> Result<String, ToolError> {
    request
        .arguments
        .get(key)
        .map_or(Ok(default.to_owned()), |value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    invalid_arguments("tool", &format!("{key} must be a non-empty string"))
                })
        })
}

fn optional_bool(request: &ToolRequest, key: &str, default: bool) -> Result<bool, ToolError> {
    request.arguments.get(key).map_or(Ok(default), |value| {
        value
            .as_bool()
            .ok_or_else(|| invalid_arguments("tool", &format!("{key} must be a boolean")))
    })
}

fn optional_limit(request: &ToolRequest, key: &str, default: usize) -> Result<usize, ToolError> {
    request.arguments.get(key).map_or(Ok(default), |value| {
        value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (1..=MAX_RESULTS).contains(value))
            .ok_or_else(|| {
                invalid_arguments(
                    "tool",
                    &format!("{key} must be between 1 and {MAX_RESULTS}"),
                )
            })
    })
}

fn invalid_arguments(tool: &str, message: &str) -> ToolError {
    ToolError::InvalidArguments {
        tool: tool.to_owned(),
        message: message.to_owned(),
    }
}

fn io_error(operation: &str, error: std::io::Error) -> ToolError {
    ToolError::Io {
        operation: operation.to_owned(),
        message: error.to_string(),
    }
}

pub(crate) fn resolve_existing(
    context: &ToolContext<'_>,
    relative: &str,
    tool: &str,
) -> Result<(PathBuf, PathBuf), ToolError> {
    let root = fs::canonicalize(context.working_directory)
        .map_err(|error| io_error("resolve workspace", error))?;
    if !root.is_dir() {
        return Err(ToolError::NotDirectory { path: root });
    }
    let requested = Path::new(relative);
    if requested.is_absolute() {
        return Err(ToolError::PathOutsideWorkspace {
            path: requested.to_path_buf(),
        });
    }
    let candidate = root.join(requested);
    let resolved = fs::canonicalize(&candidate).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolError::NotFound {
                path: candidate.clone(),
            }
        } else {
            io_error("resolve path", error)
        }
    })?;
    ensure_inside(&root, &resolved, relative)?;
    let _ = tool;
    Ok((root, resolved))
}

fn resolve_for_write(
    context: &ToolContext<'_>,
    relative: &str,
    tool: &str,
) -> Result<(PathBuf, PathBuf), ToolError> {
    let root = fs::canonicalize(context.working_directory)
        .map_err(|error| io_error("resolve workspace", error))?;
    if !root.is_dir() {
        return Err(ToolError::NotDirectory { path: root });
    }
    let requested = Path::new(relative);
    if requested.is_absolute() {
        return Err(ToolError::PathOutsideWorkspace {
            path: requested.to_path_buf(),
        });
    }
    let candidate = root.join(requested);
    let mut existing = candidate.clone();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(PathBuf::from) else {
            return Err(ToolError::NotFound { path: candidate });
        };
        existing.pop();
        suffix.push(name);
    }
    let base = fs::canonicalize(&existing).map_err(|error| io_error("resolve path", error))?;
    ensure_inside(&root, &base, relative)?;
    let resolved = suffix
        .into_iter()
        .rev()
        .fold(base, |path, component| path.join(component));
    let _ = tool;
    Ok((root, resolved))
}

fn ensure_inside(root: &Path, path: &Path, requested: &str) -> Result<(), ToolError> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(ToolError::PathOutsideWorkspace {
            path: PathBuf::from(requested),
        })
    }
}

struct EditableFile {
    raw: Vec<u8>,
    text: String,
    bom: bool,
    eol: String,
}

pub(crate) fn content_revision(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn load_editable(
    environment: &dyn super::ExecutionEnvironment,
    root: &Path,
    path: &Path,
    tool: &str,
) -> Result<EditableFile, ToolError> {
    let metadata = fs::metadata(path).map_err(|error| io_error("inspect editable file", error))?;
    if !metadata.is_file() {
        return Err(ToolError::NotFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > MAX_EDIT_FILE_BYTES {
        return Err(ToolError::FileTooLarge {
            path: path.to_path_buf(),
            limit: MAX_EDIT_FILE_BYTES,
        });
    }
    let raw = read_limited(environment, root, path, tool, MAX_EDIT_FILE_BYTES)?;
    if raw.contains(&0) {
        return Err(ToolError::BinaryFile {
            path: path.to_path_buf(),
        });
    }
    let bom = raw.starts_with(&[0xEF, 0xBB, 0xBF]);
    let text_bytes = if bom { &raw[3..] } else { &raw[..] };
    let text = String::from_utf8(text_bytes.to_vec()).map_err(|_| ToolError::BinaryFile {
        path: path.to_path_buf(),
    })?;
    let crlf = text.matches("\r\n").count();
    let lf = text
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_sub(crlf);
    let eol = if crlf > 0 && crlf >= lf { "\r\n" } else { "\n" }.to_owned();
    Ok(EditableFile {
        raw,
        text,
        bom,
        eol,
    })
}

fn normalize_eol(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn encode_new_text(text: &str, eol: &str, bom: bool) -> String {
    let normalized = normalize_eol(text);
    let converted = if eol == "\r\n" {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    };
    if bom {
        format!("\u{feff}{converted}")
    } else {
        converted
    }
}

/// Maps byte offsets in an LF-normalized string back to the original text.
fn normalized_offset_map(text: &str) -> (String, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut offsets = Vec::with_capacity(bytes.len() + 1);
    offsets.push(0);
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
            offsets.push(index);
        } else {
            normalized.push(bytes[index]);
            index += 1;
            offsets.push(index);
        }
    }
    (
        String::from_utf8(normalized).expect("normalizing line endings preserves UTF-8"),
        offsets,
    )
}

fn line_byte_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        ranges.push((offset, end));
        offset = end;
    }
    ranges
}

fn stale_revision(
    request: &ToolRequest,
    root: &Path,
    path: &Path,
    original: &EditableFile,
) -> Option<ToolResult> {
    let expected = request
        .arguments
        .get("expected_revision")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !expected.is_empty() && expected != content_revision(&original.raw) {
        Some(conflict_result(
            root,
            path,
            "stale",
            "The file changed after it was read. Read the latest contents before editing.",
        ))
    } else {
        None
    }
}

fn replace_exact(
    context: &ToolContext<'_>,
    request: &ToolRequest,
    relative: &str,
    tool: &str,
) -> Result<ToolResult, ToolError> {
    let old_text = optional_string(request, "old_text", "")?;
    let new_text = required_text(request, "new_text", tool)?;
    if old_text.is_empty() {
        return Err(invalid_arguments(tool, "old_text must not be empty"));
    }
    let (root, path) = resolve_existing(context, relative, tool)?;
    let _guard = EDIT_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original = load_editable(context.execution_environment, &root, &path, tool)?;
    if let Some(conflict) = stale_revision(request, &root, &path, &original) {
        return Ok(conflict);
    }
    let (normalized, offsets) = normalized_offset_map(&original.text);
    let old_normalized = normalize_eol(&old_text);
    let matches = normalized
        .match_indices(&old_normalized)
        .map(|(start, _)| start)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Ok(conflict_result(
            &root,
            &path,
            "stale",
            "Patch context no longer matches. Read the latest file before editing.",
        ));
    }
    if matches.len() != 1 {
        return Ok(conflict_result(
            &root,
            &path,
            "ambiguous",
            "Patch context matches more than once. Include more surrounding lines.",
        ));
    }
    let start = matches[0];
    let end = start + old_normalized.len();
    let raw_start = offsets[start];
    let raw_end = offsets[end];
    let replacement = normalize_eol(&new_text).replace('\n', &original.eol);
    let updated = format!(
        "{}{}{}",
        &original.text[..raw_start],
        replacement,
        &original.text[raw_end..]
    );
    commit_existing(context, &root, &path, original, updated, "patched")
}

fn commit_existing(
    context: &ToolContext<'_>,
    root: &Path,
    path: &Path,
    original: EditableFile,
    updated_text: String,
    verb: &str,
) -> Result<ToolResult, ToolError> {
    let bom_bytes = if original.bom { 3 } else { 0 };
    if updated_text.len() as u64 + bom_bytes > MAX_EDIT_FILE_BYTES {
        return Err(ToolError::FileTooLarge {
            path: path.to_path_buf(),
            limit: MAX_EDIT_FILE_BYTES,
        });
    }
    ensure_edit_not_cancelled(context)?;
    let new_text = encode_new_text(&updated_text, &original.eol, original.bom);
    let new_bytes = new_text.as_bytes();
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ToolError::PathOutsideWorkspace {
            path: path.to_path_buf(),
        })?;
    if !context
        .execution_environment
        .write_workspace_file_atomic(root, relative, Some(&original.raw), new_bytes)
        .map_err(|error| io_error("atomically replace file", error))?
    {
        return Ok(conflict_result(
            root,
            path,
            "concurrent_modification",
            "The file changed while the edit was being prepared. No edit was applied.",
        ));
    }
    context.emit(harness_session::EventPayload::FileChanged {
        path: path.to_path_buf(),
        change: harness_session::FileChange::Modified,
    });
    let mut result = edit_result(root, path, Some(&original.raw), Some(new_bytes), verb);
    result.changed_files.push(path.to_path_buf());
    Ok(result)
}

fn ensure_edit_not_cancelled(context: &ToolContext<'_>) -> Result<(), ToolError> {
    if context
        .cancellation
        .is_some_and(CancellationToken::is_cancelled)
    {
        Err(ToolError::Process {
            message: "edit cancelled before commit".to_owned(),
        })
    } else {
        Ok(())
    }
}

fn conflict_result(root: &Path, path: &Path, status: &str, detail: &str) -> ToolResult {
    let relative = relative_string(root, path);
    let mut result = ToolResult::new(format!("Edit conflict ({status}) for {relative}: {detail}"));
    result.is_error = true;
    result.metadata.insert("file".to_owned(), json!(relative));
    result.metadata.insert("lines_changed".to_owned(), json!(0));
    result.metadata.insert("insertions".to_owned(), json!(0));
    result.metadata.insert("deletions".to_owned(), json!(0));
    result
        .metadata
        .insert("conflict_status".to_owned(), json!(status));
    result
        .metadata
        .insert("diagnostics".to_owned(), json!({"status":"not_run"}));
    result
}

fn edit_result(
    root: &Path,
    path: &Path,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    verb: &str,
) -> ToolResult {
    let relative = relative_string(root, path);
    let (diff, insertions, deletions, truncated) = unified_diff(&relative, &relative, old, new);
    let mut result = ToolResult::new(format!("{verb} {relative}\n{diff}"));
    result.truncated = truncated;
    result.metadata.insert("file".to_owned(), json!(relative));
    result
        .metadata
        .insert("lines_changed".to_owned(), json!(insertions.max(deletions)));
    result
        .metadata
        .insert("insertions".to_owned(), json!(insertions));
    result
        .metadata
        .insert("deletions".to_owned(), json!(deletions));
    result
        .metadata
        .insert("conflict_status".to_owned(), json!("none"));
    result.metadata.insert("diff".to_owned(), json!(diff));
    result
        .metadata
        .insert("diff_truncated".to_owned(), json!(truncated));
    result
        .metadata
        .insert("bytes".to_owned(), json!(new.map_or(0, <[u8]>::len)));
    if let Some(new) = new {
        result
            .metadata
            .insert("revision".to_owned(), json!(content_revision(new)));
    }
    result
}

fn rename_result(root: &Path, from: &Path, to: &Path, contents: &[u8]) -> ToolResult {
    let old_name = relative_string(root, from);
    let new_name = relative_string(root, to);
    let _ = contents;
    let diff = format!("--- a/{old_name}\n+++ b/{new_name}\n");
    let (diff, _, _, truncated) = bound_diff(diff);
    let mut result = ToolResult::new(format!("moved {old_name} → {new_name}\n{diff}"));
    result.truncated = truncated;
    result.metadata.insert("file".to_owned(), json!(new_name));
    result
        .metadata
        .insert("old_file".to_owned(), json!(old_name));
    result.metadata.insert("lines_changed".to_owned(), json!(0));
    result.metadata.insert("insertions".to_owned(), json!(0));
    result.metadata.insert("deletions".to_owned(), json!(0));
    result
        .metadata
        .insert("conflict_status".to_owned(), json!("none"));
    result.metadata.insert("diff".to_owned(), json!(diff));
    result
        .metadata
        .insert("diff_truncated".to_owned(), json!(truncated));
    result
        .metadata
        .insert("revision".to_owned(), json!(content_revision(contents)));
    result
}

fn unified_diff(
    path_old: &str,
    path_new: &str,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) -> (String, usize, usize, bool) {
    let old_text = old
        .map(|bytes| {
            String::from_utf8_lossy(bytes)
                .trim_start_matches('\u{feff}')
                .to_owned()
        })
        .unwrap_or_default();
    let new_text = new
        .map(|bytes| {
            String::from_utf8_lossy(bytes)
                .trim_start_matches('\u{feff}')
                .to_owned()
        })
        .unwrap_or_default();
    let old_lines = normalize_eol(&old_text)
        .split_inclusive('\n')
        .map(|line| line.trim_end_matches('\n').to_owned())
        .collect::<Vec<_>>();
    let new_lines = normalize_eol(&new_text)
        .split_inclusive('\n')
        .map(|line| line.trim_end_matches('\n').to_owned())
        .collect::<Vec<_>>();
    let mut prefix = 0;
    while prefix < old_lines.len().min(new_lines.len()) && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix
        < old_lines
            .len()
            .saturating_sub(prefix)
            .min(new_lines.len().saturating_sub(prefix))
        && old_lines[old_lines.len() - suffix - 1] == new_lines[new_lines.len() - suffix - 1]
    {
        suffix += 1;
    }
    let old_end = old_lines.len().saturating_sub(suffix);
    let new_end = new_lines.len().saturating_sub(suffix);
    let deletions = old_end.saturating_sub(prefix);
    let insertions = new_end.saturating_sub(prefix);
    let start = prefix.saturating_sub(3);
    let old_hunk_end = (old_end + 3).min(old_lines.len());
    let new_hunk_end = (new_end + 3).min(new_lines.len());
    let old_count = old_hunk_end.saturating_sub(start);
    let new_count = new_hunk_end.saturating_sub(start);
    let old_header = if old.is_some() {
        format!("a/{path_old}")
    } else {
        "/dev/null".to_owned()
    };
    let new_header = if new.is_some() {
        format!("b/{path_new}")
    } else {
        "/dev/null".to_owned()
    };
    let mut diff = format!("--- {old_header}\n+++ {new_header}\n");
    if old_lines != new_lines {
        diff.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            start + 1,
            old_count,
            start + 1,
            new_count
        ));
        for line in &old_lines[start..prefix] {
            diff.push_str(&format!(" {line}\n"));
        }
        for line in &old_lines[prefix..old_end] {
            diff.push_str(&format!("-{line}\n"));
        }
        for line in &new_lines[prefix..new_end] {
            diff.push_str(&format!("+{line}\n"));
        }
        for line in &old_lines[old_end..old_hunk_end] {
            diff.push_str(&format!(" {line}\n"));
        }
    }
    let (diff, _, _, truncated) = bound_diff(diff);
    (diff, insertions, deletions, truncated)
}

fn bound_diff(mut diff: String) -> (String, usize, usize, bool) {
    if diff.len() <= MAX_DIFF_BYTES {
        return (diff, 0, 0, false);
    }
    let mut end = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    diff.truncate(end);
    diff.push_str(
        "\n... unified diff truncated; inspect the file or run git diff for the full change ...\n",
    );
    (diff, 0, 0, true)
}

fn required_line(request: &ToolRequest, key: &str, tool: &str) -> Result<usize, ToolError> {
    request
        .arguments
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_arguments(tool, &format!("{key} must be a positive integer")))
}

fn read_text(
    environment: &dyn super::ExecutionEnvironment,
    workspace_root: &Path,
    path: &Path,
    tool: &str,
) -> Result<String, ToolError> {
    read_text_snapshot(environment, workspace_root, path, tool, MAX_FILE_BYTES)
        .map(|(_, text)| text)
}

fn read_text_snapshot(
    environment: &dyn super::ExecutionEnvironment,
    workspace_root: &Path,
    path: &Path,
    tool: &str,
    limit: u64,
) -> Result<(Vec<u8>, String), ToolError> {
    if !path.is_file() {
        return Err(ToolError::NotFile {
            path: path.to_path_buf(),
        });
    }
    if file_size(path) > limit {
        return Err(ToolError::FileTooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    let bytes = read_limited(environment, workspace_root, path, tool, limit)?;
    if bytes.contains(&0) {
        return Err(ToolError::BinaryFile {
            path: path.to_path_buf(),
        });
    }
    let text = String::from_utf8(bytes.clone()).map_err(|_| ToolError::BinaryFile {
        path: path.to_path_buf(),
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
    Ok((bytes, text))
}

fn read_limited(
    environment: &dyn super::ExecutionEnvironment,
    workspace_root: &Path,
    path: &Path,
    tool: &str,
    limit: u64,
) -> Result<Vec<u8>, ToolError> {
    let relative =
        path.strip_prefix(workspace_root)
            .map_err(|_| ToolError::PathOutsideWorkspace {
                path: path.to_path_buf(),
            })?;
    environment
        .read_workspace_file(workspace_root, relative, limit)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                ToolError::FileTooLarge {
                    path: path.to_path_buf(),
                    limit,
                }
            } else {
                io_error(&format!("read {tool} file"), error)
            }
        })
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn relative_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_files(base: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![(base.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_SCAN_DEPTH || files.len() >= MAX_SCAN_FILES {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() || should_skip(&path) {
                continue;
            }
            if file_type.is_dir() {
                pending.push((path, depth + 1));
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn should_skip(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "coverage"
                | "vendor"
                | ".venv"
                | "venv"
                | "__pycache__"
        )
    )
}

fn truncate_line(line: &str) -> &str {
    if line.len() <= 500 {
        line
    } else {
        let mut end = 500;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        &line[..end]
    }
}

#[cfg(test)]
mod editing_engine_tests {
    use super::*;
    use crate::ExecutionEnvironment;
    use tempfile::tempdir;

    #[test]
    fn atomic_commit_rejects_a_concurrent_external_change() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("source.txt");
        fs::write(&path, "read version\n").unwrap();
        let snapshot = fs::read(&path).unwrap();
        fs::write(&path, "concurrent version\n").unwrap();

        assert!(!super::super::local_execution_environment()
            .write_workspace_file_atomic(
                temporary.path(),
                Path::new("source.txt"),
                Some(&snapshot),
                b"agent version\n",
            )
            .unwrap());
        assert_eq!(fs::read_to_string(path).unwrap(), "concurrent version\n");
    }
}
