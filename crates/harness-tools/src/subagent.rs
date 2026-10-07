use std::path::Path;
use std::sync::Arc;

use harness_core::{Id, SessionId};
use harness_git::GitClient;
use harness_policy::{OperationKind, Permission};
use harness_session::EventBus;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{CancellationToken, Tool, ToolContext, ToolError, ToolRequest, ToolResult, ToolSpec};

const MAX_CHILDREN_PER_CALL: usize = 3;
const MAX_TASK_CHARS: usize = 2_000;
const MAX_CONTEXT_SNIPPETS: usize = 3;
const MAX_CONTEXT_SNIPPET_CHARS: usize = 4_000;
const MAX_CONTEXT_CHARS: usize = 12_000;
const MAX_GIT_DIFF_CHARS: usize = 16_000;

pub(crate) fn read_git_diff_tool() -> Box<dyn Tool> {
    Box::new(ReadGitDiffTool)
}

struct ReadGitDiffTool;

impl Tool for ReadGitDiffTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_git_diff".to_owned(),
            description: "Read the current staged and unstaged Git diff for review. Sensitive credential and key paths are omitted; this tool never changes Git state.".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "properties": {
                    "max_chars": {"type": "integer", "minimum": 1, "maximum": MAX_GIT_DIFF_CHARS}
                }
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
        let max_chars = request
            .arguments
            .get("max_chars")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(MAX_GIT_DIFF_CHARS as u64)
            .clamp(1, MAX_GIT_DIFF_CHARS as u64) as usize;
        let git =
            GitClient::open(context.working_directory).map_err(|error| ToolError::Process {
                message: format!("could not inspect repository diff: {error}"),
            })?;
        let diff = git.diff().map_err(|error| ToolError::Process {
            message: format!("could not read repository diff: {error}"),
        })?;
        let combined = format!("{}\n{}", diff.staged, diff.unstaged);
        let filtered = omit_sensitive_diff_sections(&combined);
        let was_truncated = filtered.chars().count() > max_chars;
        let output = if was_truncated {
            format!(
                "{}\n[diff truncated at {max_chars} characters]",
                filtered.chars().take(max_chars).collect::<String>()
            )
        } else if filtered.trim().is_empty() {
            "No staged or unstaged tracked-file changes.".to_owned()
        } else {
            filtered
        };
        let mut result = ToolResult::new(output);
        result.truncated = was_truncated;
        result
            .metadata
            .insert("sensitive_sections_omitted".to_owned(), json!(true));
        Ok(result)
    }
}

fn omit_sensitive_diff_sections(diff: &str) -> String {
    let mut visible = String::new();
    for section in diff.split("diff --git ").skip(1) {
        let header = section
            .lines()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let sensitive = [
            ".env",
            ".ssh/",
            ".aws/",
            "credentials",
            "id_rsa",
            "id_ed25519",
            ".pem",
            ".key",
            "secret",
        ]
        .iter()
        .any(|needle| header.contains(needle));
        if !sensitive {
            visible.push_str("diff --git ");
            visible.push_str(section);
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::omit_sensitive_diff_sections;

    #[test]
    fn git_diff_reader_omits_credential_and_key_paths() {
        let diff = concat!(
            "diff --git a/src/main.rs b/src/main.rs\n+",
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-code\n+updated\n",
            "diff --git a/.env.local b/.env.local\n--- a/.env.local\n+++ b/.env.local\n@@ -1 +1 @@\n-SECRET=old\n+SECRET=new\n",
            "diff --git a/certs/private.pem b/certs/private.pem\n--- a/certs/private.pem\n+++ b/certs/private.pem\n@@ -1 +1 @@\n-old\n+new\n"
        );
        let visible = omit_sensitive_diff_sections(diff);
        assert!(visible.contains("src/main.rs"));
        assert!(!visible.contains(".env.local"));
        assert!(!visible.contains("SECRET="));
        assert!(!visible.contains("private.pem"));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRole {
    Explore,
    Review,
    Test,
    Documentation,
}

impl SubagentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Review => "review",
            Self::Test => "test",
            Self::Documentation => "documentation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubagentContext {
    pub label: String,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubagentTask {
    pub role: SubagentRole,
    pub task: String,
    #[serde(default)]
    pub selected_context: Vec<SubagentContext>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubagentContextMetrics {
    /// Approximation based on the serialized parent event history. It is never
    /// sent to the child model.
    pub parent_history_estimated_tokens: u64,
    /// Tokens the child runtime actually estimated sending across its turns.
    pub child_context_tokens_sent: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubagentReport {
    pub role: SubagentRole,
    pub child_session_id: String,
    pub status: String,
    pub summary: String,
    pub findings: Vec<String>,
    pub relevant_files: Vec<String>,
    pub evidence: Vec<String>,
    pub recommended_next_action: String,
    #[serde(default)]
    pub context_metrics: SubagentContextMetrics,
}

/// Runtime-owned implementation. Child agents receive tasks and selected
/// context only; the trait deliberately has no parent conversation parameter.
pub trait SubagentExecutor: Send + Sync {
    fn execute_batch(
        &self,
        workspace_root: &Path,
        parent_session_id: &SessionId,
        event_bus: &EventBus,
        correlation_id: Option<&Id>,
        cancellation: &CancellationToken,
        tasks: Vec<SubagentTask>,
    ) -> Result<Vec<SubagentReport>, String>;
}

pub struct DelegateSubagentsTool {
    executor: Arc<dyn SubagentExecutor>,
}

impl DelegateSubagentsTool {
    pub fn new(executor: Arc<dyn SubagentExecutor>) -> Self {
        Self { executor }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateRequest {
    tasks: Vec<SubagentTask>,
}

impl Tool for DelegateSubagentsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate_subagents".to_owned(),
            description: "Delegate up to three independent read-only investigation tasks to isolated Explore, Review, Test, or Documentation agents. Children cannot edit files, run commands, access the network, or delegate again. Provide only the small context snippets each child needs; parent conversation history is never copied.".to_owned(),
            arguments_schema: json!({
                "type": "object",
                "required": ["tasks"],
                "properties": {
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_CHILDREN_PER_CALL,
                        "items": {
                            "type": "object",
                            "required": ["role", "task"],
                            "properties": {
                                "role": {"type": "string", "enum": ["explore", "review", "test", "documentation"]},
                                "task": {"type": "string", "minLength": 1, "maxLength": MAX_TASK_CHARS},
                                "selected_context": {
                                    "type": "array",
                                    "maxItems": MAX_CONTEXT_SNIPPETS,
                                    "items": {
                                        "type": "object",
                                        "required": ["label", "content"],
                                        "properties": {
                                            "label": {"type": "string", "maxLength": 200},
                                            "content": {"type": "string", "maxLength": MAX_CONTEXT_SNIPPET_CHARS}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
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
        let invalid = |message: String| ToolError::InvalidArguments {
            tool: request.name.clone(),
            message,
        };
        let parsed: DelegateRequest = serde_json::from_value(request.arguments)
            .map_err(|error| invalid(error.to_string()))?;
        if parsed.tasks.is_empty() || parsed.tasks.len() > MAX_CHILDREN_PER_CALL {
            return Err(invalid(format!(
                "provide between 1 and {MAX_CHILDREN_PER_CALL} independent tasks"
            )));
        }
        let mut context_chars = 0usize;
        for task in &parsed.tasks {
            if task.task.trim().is_empty() || task.task.chars().count() > MAX_TASK_CHARS {
                return Err(invalid(format!(
                    "each task must contain 1 to {MAX_TASK_CHARS} characters"
                )));
            }
            if task.selected_context.len() > MAX_CONTEXT_SNIPPETS {
                return Err(invalid(format!(
                    "each task may include at most {MAX_CONTEXT_SNIPPETS} context snippets"
                )));
            }
            for snippet in &task.selected_context {
                if snippet.label.trim().is_empty()
                    || snippet.label.chars().count() > 200
                    || snippet.content.chars().count() > MAX_CONTEXT_SNIPPET_CHARS
                {
                    return Err(invalid(
                        "context labels must be non-empty and snippets must be at most 4000 characters"
                            .to_owned(),
                    ));
                }
                context_chars = context_chars.saturating_add(snippet.content.chars().count());
            }
        }
        if context_chars > MAX_CONTEXT_CHARS {
            return Err(invalid(format!(
                "selected context is limited to {MAX_CONTEXT_CHARS} characters per delegation"
            )));
        }

        let (Some(session_id), Some(event_bus)) = (context.session_id, context.event_bus) else {
            return Err(invalid(
                "delegation is available only inside a persisted agent session".to_owned(),
            ));
        };
        let cancellation = context
            .cancellation
            .cloned()
            .unwrap_or_else(CancellationToken::new);
        let reports = self
            .executor
            .execute_batch(
                context.working_directory,
                session_id,
                event_bus,
                context.correlation_id,
                &cancellation,
                parsed.tasks,
            )
            .map_err(|message| ToolError::Process { message })?;
        let report_count = reports.len();
        let output = serde_json::to_string(&json!({
            "mode": "read_only",
            "context_chars_provided": context_chars,
            "results": reports,
        }))
        .map_err(|error| invalid(error.to_string()))?;
        let mut result = ToolResult::new(output);
        result
            .metadata
            .insert("delegation_count".to_owned(), json!(report_count));
        result
            .metadata
            .insert("context_chars_provided".to_owned(), json!(context_chars));
        Ok(result)
    }
}
