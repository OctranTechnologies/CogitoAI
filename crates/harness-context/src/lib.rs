use std::collections::BTreeMap;
use std::path::PathBuf;

use harness_core::{Error, InstructionFile};
use harness_git::GitStatus;
use harness_session::{ConversationMessage, MessageRole};
use harness_tools::ToolResult;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextBudget {
    pub max_individual_file_bytes: usize,
    pub max_tool_result_bytes: usize,
    pub max_shell_output_bytes: usize,
    pub max_working_context_tokens: u32,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_individual_file_bytes: 256 * 1024,
            max_tool_result_bytes: 64 * 1024,
            max_shell_output_bytes: 16 * 1024,
            max_working_context_tokens: 32 * 1024,
        }
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContextCategory {
    System,
    Workspace,
    Instruction,
    Git,
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

pub struct ContextBuilder {
    budget: ContextBudget,
}

impl Default for ContextBuilder {
    fn default() -> Self {
        Self::new(ContextBudget::default())
    }
}

impl ContextBuilder {
    pub fn new(budget: ContextBudget) -> Self {
        Self { budget }
    }

    pub fn build(&self, input: &ContextInput) -> Result<ContextAssembly, Error> {
        self.budget.validate()?;
        let mut items = Vec::new();
        let mut used_tokens = 0;
        add_item(
            &mut items,
            &mut used_tokens,
            self.budget.clone(),
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
        if input.workspace.root.is_some()
            || !input.workspace.languages.is_empty()
            || !input.workspace.manifests.is_empty()
            || !input.workspace.details.is_empty()
        {
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
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
        let mut instructions = input.instructions.clone();
        instructions.sort_by_key(|instruction| instruction.precedence);
        for instruction in &instructions {
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
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
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
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
        if let Some(compacted) = &input.compacted_state {
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
                Candidate {
                    id: "compacted-state".to_owned(),
                    category: ContextCategory::CompactionSummary,
                    source: "session.context.compacted".to_owned(),
                    reason: ContextReason::CompactionSummary,
                    content: compacted.render(),
                    required: true,
                    limit: Some(ContextLimit::ToolResultSize),
                },
            );
        }
        if !input.user_request.trim().is_empty() {
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
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
        }
        for (index, message) in input.conversation.iter().enumerate() {
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
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
        for file in &input.files {
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
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
        for result in &input.tool_results {
            add_item(
                &mut items,
                &mut used_tokens,
                self.budget.clone(),
                Candidate {
                    id: format!("tool-{}", result.name),
                    category: ContextCategory::ToolResult,
                    source: result.name.clone(),
                    reason: ContextReason::RelevantToolResult,
                    content: result.result.output.clone(),
                    required: false,
                    limit: Some(if result.is_shell {
                        ContextLimit::ShellOutputSize
                    } else {
                        ContextLimit::ToolResultSize
                    }),
                },
            );
        }
        let excluded_items = items.iter().filter(|item| !item.included).count();
        let prompt = render_prompt(&items);
        Ok(ContextAssembly {
            prompt,
            items,
            estimated_tokens: used_tokens,
            budget: self.budget.clone(),
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
            truncate_bytes(&candidate.content, budget.max_shell_output_bytes),
            true,
            vec![ContextLimit::ShellOutputSize],
        ),
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

fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    const MARKER: &str = "\n[truncated]";
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    if max_bytes <= MARKER.len() {
        return value[..max_bytes].to_owned();
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
