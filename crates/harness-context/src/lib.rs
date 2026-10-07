use std::collections::BTreeMap;
use std::path::PathBuf;

use harness_core::{Error, InstructionFile};
use harness_git::GitStatus;
use harness_session::{ConversationMessage, MessageRole};
use harness_tools::ToolResult;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextBudget {
    pub max_individual_file_bytes: usize,
    pub max_tool_result_bytes: usize,
    pub max_shell_output_bytes: usize,
    pub max_git_diff_bytes: usize,
    pub max_compaction_summary_bytes: usize,
    pub max_working_context_tokens: u32,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_individual_file_bytes: 256 * 1024,
            max_tool_result_bytes: 64 * 1024,
            max_shell_output_bytes: 16 * 1024,
            max_git_diff_bytes: 12 * 1024,
            max_compaction_summary_bytes: 8 * 1024,
            max_working_context_tokens: 32 * 1024,
        }
    }
}

/// Derives a safe prompt budget from the selected model's known input window.
/// Unknown windows use the configured local budget without fabricating model
/// metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBudgetManager {
    base: ContextBudget,
    context_fraction_percent: u32,
}

impl ContextBudgetManager {
    pub fn new(base: ContextBudget) -> Self {
        Self {
            base,
            context_fraction_percent: 75,
        }
    }

    pub fn budget_for_context_window(&self, context_window: Option<u32>) -> ContextBudget {
        let mut budget = self.base.clone();
        if let Some(context_window) = context_window {
            let model_budget = context_window
                .saturating_mul(self.context_fraction_percent)
                .checked_div(100)
                .unwrap_or(0)
                .max(1);
            budget.max_working_context_tokens = budget.max_working_context_tokens.min(model_budget);
        }
        budget
    }
}

impl ContextBudget {
    pub fn validate(&self) -> Result<(), Error> {
        if self.max_working_context_tokens == 0 {
            return Err(Error::InvalidConfig {
                reason: "max_working_context_tokens must be greater than zero".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMetadata {
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub monorepo: bool,
    pub languages: Vec<String>,
    pub manifests: Vec<String>,
    pub details: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExplicitFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolContextResult {
    pub name: String,
    pub result: ToolResult,
    pub is_shell: bool,
    #[serde(default)]
    pub already_in_model_history: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextInput {
    pub system_instructions: String,
    pub workspace: WorkspaceMetadata,
    pub instructions: Vec<InstructionFile>,
    pub user_request: String,
    pub conversation: Vec<ConversationMessage>,
    pub files: Vec<ExplicitFile>,
    pub tool_results: Vec<ToolContextResult>,
    pub compacted_state: Option<harness_session::CompactState>,
    pub git_status: Option<GitStatus>,
    #[serde(default)]
    pub git_diff: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContextCategory {
    System,
    Workspace,
    Instruction,
    Git,
    GitDiff,
    UserRequest,
    Conversation,
    File,
    ToolResult,
    CompactionSummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContextReason {
    Required,
    WorkspaceMetadata,
    ProjectInstruction { precedence: u8 },
    GitState,
    CurrentUserRequest,
    RecentConversation,
    ExplicitlySelectedFile,
    RelevantToolResult,
    CompactionSummary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContextLimit {
    FileSize,
    ToolResultSize,
    ShellOutputSize,
    GitDiffSize,
    CompactionSummarySize,
    WorkingBudget,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: String,
    pub category: ContextCategory,
    pub source: String,
    pub reason: ContextReason,
    pub content: String,
    pub original_bytes: usize,
    pub estimated_tokens: u32,
    pub included: bool,
    pub truncated: bool,
    pub limits: Vec<ContextLimit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextAssembly {
    pub prompt: String,
    pub items: Vec<ContextItem>,
    pub estimated_tokens: u32,
    pub budget: ContextBudget,
    pub excluded_items: usize,
}

impl ContextAssembly {
    pub fn included_items(&self) -> impl Iterator<Item = &ContextItem> {
        self.items.iter().filter(|item| item.included)
    }
}

#[derive(Clone)]
pub struct ContextBuilder {
    budget_manager: ContextBudgetManager,
}

impl Default for ContextBuilder {
    fn default() -> Self {
        Self::new(ContextBudget::default())
    }
}

impl ContextBuilder {
    pub fn new(budget: ContextBudget) -> Self {
        Self {
            budget_manager: ContextBudgetManager::new(budget),
        }
    }

    pub fn build(&self, input: &ContextInput) -> Result<ContextAssembly, Error> {
        self.build_for_context_window(input, None)
    }

    pub fn build_for_context_window(
        &self,
        input: &ContextInput,
        context_window: Option<u32>,
    ) -> Result<ContextAssembly, Error> {
        self.build_for_context_window_reserving(input, context_window, 0)
    }

    pub fn budget_for_context_window(&self, context_window: Option<u32>) -> ContextBudget {
        self.budget_manager
            .budget_for_context_window(context_window)
    }

    pub fn build_for_context_window_reserving(
        &self,
        input: &ContextInput,
        context_window: Option<u32>,
        reserved_tokens: u32,
    ) -> Result<ContextAssembly, Error> {
        let mut budget = self
            .budget_manager
            .budget_for_context_window(context_window);
        budget.validate()?;
        budget.max_working_context_tokens = budget
            .max_working_context_tokens
            .saturating_sub(reserved_tokens)
            .max(1);
        budget.validate()?;
        const MAX_RECENT_CONVERSATION_MESSAGES: usize = 8;
        const MAX_RECENT_TOOL_RESULTS: usize = 24;
        let mut items = Vec::new();
        let mut used_tokens = 0;
        let mut candidates = Vec::<(u8, usize, Candidate)>::new();
        let mut add = |priority, recency, candidate| {
            candidates.push((priority, recency, candidate));
        };
        add(
            0,
            0,
            Candidate {
                id: "system-instructions".to_owned(),
                category: ContextCategory::System,
                source: "harness".to_owned(),
                reason: ContextReason::Required,
                content: input.system_instructions.clone(),
                required: true,
                limit: None,
            },
        );
        add(
            0,
            1,
            Candidate {
                id: "user-request".to_owned(),
                category: ContextCategory::UserRequest,
                source: "conversation".to_owned(),
                reason: ContextReason::CurrentUserRequest,
                content: input.user_request.clone(),
                required: true,
                limit: None,
            },
        );
        if let Some(compacted) = &input.compacted_state {
            add(
                3,
                usize::MAX,
                Candidate {
                    id: "compacted-state".to_owned(),
                    category: ContextCategory::CompactionSummary,
                    source: "session.context.compacted".to_owned(),
                    reason: ContextReason::CompactionSummary,
                    content: compacted.render(),
                    // Current goal/plan/failures are also present in the
                    // runtime-owned task state. This historical supplement
                    // must not displace instructions, relevant code, or the
                    // current diff when the working budget is tight.
                    required: false,
                    limit: Some(ContextLimit::CompactionSummarySize),
                },
            );
        }
        if let Some(diff) = &input.git_diff {
            add(
                1,
                0,
                Candidate {
                    id: "current-git-diff".to_owned(),
                    category: ContextCategory::GitDiff,
                    source: "workspace task changes".to_owned(),
                    reason: ContextReason::GitState,
                    content: diff.clone(),
                    required: false,
                    limit: Some(ContextLimit::GitDiffSize),
                },
            );
        }
        let mut instructions = input.instructions.clone();
        instructions.sort_by_key(|instruction| instruction.precedence);
        for (index, instruction) in instructions.iter().enumerate() {
            add(
                1,
                index,
                Candidate {
                    id: format!("instruction-{}", instruction.precedence),
                    category: ContextCategory::Instruction,
                    source: instruction.path.display().to_string(),
                    reason: ContextReason::ProjectInstruction {
                        precedence: instruction.precedence,
                    },
                    content: instruction.content.clone(),
                    required: false,
                    limit: None,
                },
            );
        }
        if let Some(git_status) = &input.git_status {
            add(
                1,
                instructions.len() + 1,
                Candidate {
                    id: "git-status".to_owned(),
                    category: ContextCategory::Git,
                    source: git_status.repository_root.display().to_string(),
                    reason: ContextReason::GitState,
                    content: git_text(git_status),
                    required: false,
                    limit: None,
                },
            );
        }
        if input.workspace.root.is_some()
            || !input.workspace.languages.is_empty()
            || !input.workspace.manifests.is_empty()
            || !input.workspace.details.is_empty()
        {
            add(
                3,
                0,
                Candidate {
                    id: "workspace-metadata".to_owned(),
                    category: ContextCategory::Workspace,
                    source: "workspace".to_owned(),
                    reason: ContextReason::WorkspaceMetadata,
                    content: workspace_text(&input.workspace),
                    required: false,
                    limit: None,
                },
            );
        }
        let conversation_start = input
            .conversation
            .len()
            .saturating_sub(MAX_RECENT_CONVERSATION_MESSAGES);
        for (index, message) in input
            .conversation
            .iter()
            .enumerate()
            .skip(conversation_start)
        {
            add(
                3,
                input.conversation.len() - index,
                Candidate {
                    id: format!("conversation-{index}"),
                    category: ContextCategory::Conversation,
                    source: role_name(message).to_owned(),
                    reason: ContextReason::RecentConversation,
                    content: message.text.clone(),
                    required: false,
                    limit: None,
                },
            );
        }
        for (index, file) in input.files.iter().enumerate() {
            add(
                2,
                index,
                Candidate {
                    id: format!("file-{}", file.path.display()),
                    category: ContextCategory::File,
                    source: file.path.display().to_string(),
                    reason: ContextReason::ExplicitlySelectedFile,
                    content: file.content.clone(),
                    required: false,
                    limit: Some(ContextLimit::FileSize),
                },
            );
        }
        let visible_tool_results = input
            .tool_results
            .iter()
            .enumerate()
            .filter(|(_, result)| !result.already_in_model_history)
            .collect::<Vec<_>>();
        let visible_tool_count = visible_tool_results.len();
        let tool_start = visible_tool_count.saturating_sub(MAX_RECENT_TOOL_RESULTS);
        for (visible_index, (index, result)) in visible_tool_results
            .into_iter()
            .enumerate()
            .skip(tool_start)
            .rev()
        {
            let is_failure = result.result.is_error
                || result.name == "task_completion_check"
                || (result.name == "verification" && result.result.output.contains("passed=false"));
            let priority = if is_failure {
                1
            } else if result.name == "read_file" {
                2
            } else if result.is_shell {
                5
            } else if is_repository_retrieval_source(&result.name) {
                3
            } else {
                4
            };
            let content = if result.is_shell {
                summarize_shell_output(&result.result.output, budget.max_shell_output_bytes)
            } else {
                result.result.output.clone()
            };
            add(
                priority,
                visible_tool_count - visible_index,
                Candidate {
                    id: format!("tool-{index}-{}", result.name),
                    category: ContextCategory::ToolResult,
                    source: result.name.clone(),
                    reason: ContextReason::RelevantToolResult,
                    content,
                    // Keep at least the identifying diagnostics when the
                    // context budget is tight. `add_item` truncates required
                    // candidates to the remaining budget instead of dropping
                    // them, which is preferable to losing a fresh failure.
                    required: is_failure,
                    limit: Some(if result.is_shell {
                        ContextLimit::ShellOutputSize
                    } else {
                        ContextLimit::ToolResultSize
                    }),
                },
            );
        }
        candidates.sort_by_key(|(priority, recency, _)| (*priority, *recency));
        for (_, _, candidate) in candidates {
            add_item(&mut items, &mut used_tokens, budget.clone(), candidate);
        }
        let excluded_items = items.iter().filter(|item| !item.included).count();
        let prompt = render_prompt(&items);
        Ok(ContextAssembly {
            prompt,
            items,
            estimated_tokens: used_tokens,
            budget,
            excluded_items,
        })
    }
}

struct Candidate {
    id: String,
    category: ContextCategory,
    source: String,
    reason: ContextReason,
    content: String,
    required: bool,
    limit: Option<ContextLimit>,
}

fn add_item(
    items: &mut Vec<ContextItem>,
    used_tokens: &mut u32,
    budget: ContextBudget,
    candidate: Candidate,
) {
    let original_bytes = candidate.content.len();
    let (content, truncated, mut limits) = match candidate.limit {
        Some(ContextLimit::FileSize) if original_bytes > budget.max_individual_file_bytes => (
            truncate_bytes(&candidate.content, budget.max_individual_file_bytes),
            true,
            vec![ContextLimit::FileSize],
        ),
        Some(ContextLimit::ToolResultSize) if original_bytes > budget.max_tool_result_bytes => (
            truncate_bytes(&candidate.content, budget.max_tool_result_bytes),
            true,
            vec![ContextLimit::ToolResultSize],
        ),
        Some(ContextLimit::ShellOutputSize) if original_bytes > budget.max_shell_output_bytes => (
            summarize_shell_output(&candidate.content, budget.max_shell_output_bytes),
            true,
            vec![ContextLimit::ShellOutputSize],
        ),
        Some(ContextLimit::GitDiffSize) if original_bytes > budget.max_git_diff_bytes => (
            truncate_bytes(&candidate.content, budget.max_git_diff_bytes),
            true,
            vec![ContextLimit::GitDiffSize],
        ),
        Some(ContextLimit::CompactionSummarySize)
            if original_bytes > budget.max_compaction_summary_bytes =>
        {
            (
                truncate_bytes(&candidate.content, budget.max_compaction_summary_bytes),
                true,
                vec![ContextLimit::CompactionSummarySize],
            )
        }
        _ => (candidate.content, false, Vec::new()),
    };
    let estimated_tokens = estimate_tokens(&content);
    if *used_tokens + estimated_tokens > budget.max_working_context_tokens {
        if candidate.required {
            let remaining_bytes = budget
                .max_working_context_tokens
                .saturating_sub(*used_tokens)
                .saturating_mul(4) as usize;
            let content = truncate_bytes(&content, remaining_bytes);
            let estimated_tokens = estimate_tokens(&content);
            if estimated_tokens > 0 || content.is_empty() {
                limits.push(ContextLimit::WorkingBudget);
                *used_tokens = used_tokens.saturating_add(estimated_tokens);
                items.push(ContextItem {
                    id: candidate.id,
                    category: candidate.category,
                    source: candidate.source,
                    reason: candidate.reason,
                    content,
                    original_bytes,
                    estimated_tokens,
                    included: true,
                    truncated: true,
                    limits,
                });
            }
        } else {
            items.push(ContextItem {
                id: candidate.id,
                category: candidate.category,
                source: candidate.source,
                reason: candidate.reason,
                content: String::new(),
                original_bytes,
                estimated_tokens,
                included: false,
                truncated: false,
                limits: vec![ContextLimit::WorkingBudget],
            });
        }
        return;
    }
    *used_tokens += estimated_tokens;
    items.push(ContextItem {
        id: candidate.id,
        category: candidate.category,
        source: candidate.source,
        reason: candidate.reason,
        content,
        original_bytes,
        estimated_tokens,
        included: true,
        truncated,
        limits,
    });
}

pub fn estimate_tokens(value: &str) -> u32 {
    value.len().saturating_add(3) as u32 / 4
}

pub fn summarize_shell_output(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let lines = value.lines().collect::<Vec<_>>();
    let mut selected = Vec::new();
    selected.extend(lines.iter().take(4).copied());
    let mut diagnostic_count = 0;
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if [
            "error",
            "failed",
            "failure",
            "warning",
            "passed",
            "test result",
        ]
        .iter()
        .any(|signal| lower.contains(signal))
        {
            selected.push(*line);
            diagnostic_count += 1;
            if diagnostic_count >= 20 {
                break;
            }
        }
    }
    selected.extend(lines.iter().rev().take(8).rev().copied());
    selected.dedup();
    let omitted = lines.len().saturating_sub(selected.len());
    let summary = format!(
        "[shell output summarized: {} lines, omitted {omitted}]\n{}",
        lines.len(),
        selected.join("\n")
    );
    truncate_bytes(&summary, max_bytes)
}

fn is_repository_retrieval_source(name: &str) -> bool {
    matches!(
        name,
        "search_files"
            | "search_text"
            | "find_symbol"
            | "find_references"
            | "goto_definition"
            | "get_diagnostics"
            | "get_file_outline"
            | "get_repo_tree"
    )
}

fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    const MARKER: &str = "\n[truncated]";
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    if max_bytes <= MARKER.len() {
        let mut end = max_bytes.min(value.len());
        while !value.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        return value[..end].to_owned();
    }
    let mut end = max_bytes - MARKER.len();
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{MARKER}", &value[..end])
}

fn workspace_text(workspace: &WorkspaceMetadata) -> String {
    let mut text = String::new();
    if let Some(root) = &workspace.root {
        text.push_str(&format!("root: {}\n", root.display()));
    }
    if let Some(branch) = &workspace.branch {
        text.push_str(&format!("branch: {branch}\n"));
    }
    text.push_str(&format!("monorepo: {}\n", workspace.monorepo));
    text.push_str(&format!("languages: {}\n", workspace.languages.join(", ")));
    text.push_str(&format!("manifests: {}", workspace.manifests.join(", ")));
    for (key, value) in &workspace.details {
        text.push_str(&format!("{key}: {value}\n"));
    }
    text
}

fn git_text(status: &GitStatus) -> String {
    format!(
        "branch: {}\nhead: {}\nclean: {}\nchanged: {}\nstaged: {}\nunstaged: {}\nuntracked: {}",
        status.branch.as_deref().unwrap_or("<detached>"),
        status.head.as_deref().unwrap_or("<none>"),
        status.is_clean,
        status.changed_files.join(", "),
        status.staged_files.join(", "),
        status.unstaged_files.join(", "),
        status.untracked_files.join(", ")
    )
}

fn render_prompt(items: &[ContextItem]) -> String {
    let mut sections = Vec::new();
    for item in items.iter().filter(|item| item.included) {
        let heading = match item.category {
            ContextCategory::System => "System instructions",
            ContextCategory::Workspace => "Workspace metadata",
            ContextCategory::Instruction => "Project instructions",
            ContextCategory::Git => "Git status",
            ContextCategory::GitDiff => "Current Git diff",
            ContextCategory::UserRequest => "User request",
            ContextCategory::Conversation => "Recent conversation",
            ContextCategory::File => "Selected files",
            ContextCategory::ToolResult => "Relevant tool results",
            ContextCategory::CompactionSummary => "Compacted working state",
        };
        sections.push(format!(
            "## {heading}\nSource: {}\n{}",
            item.source, item.content
        ));
    }
    sections.join("\n\n")
}

fn role_name(message: &ConversationMessage) -> &'static str {
    match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}
