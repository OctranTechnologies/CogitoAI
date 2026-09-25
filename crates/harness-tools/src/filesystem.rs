use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use globset::Glob;
use harness_policy::Permission;
use regex::RegexBuilder;
use serde_json::{json, Value};

use super::{
    CancellationToken, LocalProcessRunner, ProcessError, ProcessEvent, ProcessRequest,
    ProcessRunner, Tool, ToolContext, ToolError, ToolRequest, ToolResult, ToolSpec,
};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_RESULTS: usize = 200;
const MAX_SCAN_FILES: usize = 20_000;
const MAX_SCAN_DEPTH: usize = 16;

pub struct ShellTool {
    runner: Arc<dyn ProcessRunner>,
    cancellation: CancellationToken,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            runner: Arc::new(LocalProcessRunner),
            cancellation: CancellationToken::new(),
        }
    }
}

impl ShellTool {
    pub fn new(runner: Arc<dyn ProcessRunner>, cancellation: CancellationToken) -> Self {
        Self {
            runner,
            cancellation,
        }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell".to_owned(),
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

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let command_text = required_string(&request, "command", "shell")?;
        let shell = optional_string(&request, "shell", "auto")?.to_ascii_lowercase();
        let (program, args) = shell_invocation(&shell, &command_text)?;
        let working_directory = optional_string(&request, "working_directory", ".")?;
        let (_root, working_directory) = resolve_existing(context, &working_directory, "shell")?;
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
        let result = self
            .runner
            .execute(request, &self.cancellation, &mut |event| {
                emit_process_event(
                    context,
                    &event,
                    &command_text,
                    &event_working_directory,
                    timeout_ms,
                );
                Ok(())
            })
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
        }
        let mut tool_result = ToolResult::new(output);
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
            description: "Read a UTF-8 text file from the workspace".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "read_file")?;
        let (root, resolved) = resolve_existing(context, &path, "read_file")?;
        let text = read_text(&resolved, "read_file")?;
        let mut result = ToolResult::new(text);
        result
            .metadata
            .insert("path".to_owned(), json!(relative_string(&root, &resolved)));
        result
            .metadata
            .insert("bytes".to_owned(), json!(file_size(&resolved)));
        Ok(result)
    }
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_owned(),
            description: "Create or replace a UTF-8 text file in the workspace".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "write_file")?;
        let content = required_string(&request, "content", "write_file")?;
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(ToolError::FileTooLarge {
                path: PathBuf::from(path),
                limit: MAX_FILE_BYTES,
            });
        }
        let existed = context.working_directory.join(&path).exists();
        let (root, resolved) = resolve_for_write(context, &path, "write_file")?;
        if resolved.exists() && !resolved.is_file() {
            return Err(ToolError::NotFile { path: resolved });
        }
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("create parent directory", error))?;
        }
        fs::write(&resolved, &content).map_err(|error| io_error("write file", error))?;
        context.emit(harness_session::EventPayload::FileChanged {
            path: resolved.clone(),
            change: if existed {
                harness_session::FileChange::Modified
            } else {
                harness_session::FileChange::Added
            },
        });
        let mut result = ToolResult::new(format!("wrote {}", relative_string(&root, &resolved)));
        result
            .metadata
            .insert("path".to_owned(), json!(relative_string(&root, &resolved)));
        result
            .metadata
            .insert("bytes".to_owned(), json!(content.len()));
        result.changed_files.push(resolved);
        Ok(result)
    }
}

pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".to_owned(),
            description: "Replace one exact source context in a UTF-8 workspace file".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string", "minLength": 1 },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::WriteWorkspace
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = required_path(&request, "apply_patch")?;
        let old_text = required_string(&request, "old_text", "apply_patch")?;
        let new_text = required_string(&request, "new_text", "apply_patch")?;
        if old_text.is_empty() {
            return Err(invalid_arguments(
                "apply_patch",
                "old_text must not be empty",
            ));
        }
        let (root, resolved) = resolve_existing(context, &path, "apply_patch")?;
        let source = read_text(&resolved, "apply_patch")?;
        if source.matches(&old_text).count() != 1 {
            return Err(ToolError::PatchConflict { path: resolved });
        }
        let updated = source.replacen(&old_text, &new_text, 1);
        if updated.len() as u64 > MAX_FILE_BYTES {
            return Err(ToolError::FileTooLarge {
                path: resolved,
                limit: MAX_FILE_BYTES,
            });
        }
        fs::write(&resolved, updated).map_err(|error| io_error("apply patch", error))?;
        context.emit(harness_session::EventPayload::FileChanged {
            path: resolved.clone(),
            change: harness_session::FileChange::Modified,
        });
        let mut result = ToolResult::new(format!("patched {}", relative_string(&root, &resolved)));
        result
            .metadata
            .insert("path".to_owned(), json!(relative_string(&root, &resolved)));
        result
            .metadata
            .insert("bytes".to_owned(), json!(file_size(&resolved)));
        result.changed_files.push(resolved);
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
        let (_root, base) = resolve_existing(context, &path, "grep")?;
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
            let Ok(contents) = read_text(&file, "grep") else {
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

fn shell_invocation(shell: &str, command: &str) -> Result<(&'static str, Vec<String>), ToolError> {
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

fn resolve_existing(
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

fn read_text(path: &Path, tool: &str) -> Result<String, ToolError> {
    if !path.is_file() {
        return Err(ToolError::NotFile {
            path: path.to_path_buf(),
        });
    }
    if file_size(path) > MAX_FILE_BYTES {
        return Err(ToolError::FileTooLarge {
            path: path.to_path_buf(),
            limit: MAX_FILE_BYTES,
        });
    }
    let bytes = fs::read(path).map_err(|error| io_error(&format!("read {tool} file"), error))?;
    if bytes.contains(&0) {
        return Err(ToolError::BinaryFile {
            path: path.to_path_buf(),
        });
    }
    String::from_utf8(bytes).map_err(|_| ToolError::BinaryFile {
        path: path.to_path_buf(),
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
