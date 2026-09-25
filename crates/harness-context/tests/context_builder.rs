use std::path::PathBuf;

use harness_context::{
    ContextAssembly, ContextBudget, ContextBuilder, ContextCategory, ContextInput, ContextLimit,
    ExplicitFile, ToolContextResult, WorkspaceMetadata,
};
use harness_core::InstructionFile;
use harness_git::GitStatus;
use harness_session::{ConversationMessage, MessageRole};
use harness_tools::ToolResult;

fn input() -> ContextInput {
    ContextInput {
        system_instructions: "Follow repository instructions.".to_owned(),
        workspace: WorkspaceMetadata {
            root: Some(PathBuf::from("/workspace")),
            branch: Some("main".to_owned()),
            monorepo: true,
            languages: vec!["Rust".to_owned()],
            manifests: vec!["Cargo.toml".to_owned()],
            details: Default::default(),
        },
        instructions: vec![
            InstructionFile {
                path: PathBuf::from("AGENTS.md"),
                kind: harness_core::InstructionKind::Agents,
                precedence: 0,
                content: "Prefer focused changes.".to_owned(),
            },
            InstructionFile {
                path: PathBuf::from("README.md"),
                kind: harness_core::InstructionKind::Readme,
                precedence: 2,
                content: "Run cargo test.".to_owned(),
            },
        ],
        user_request: "Explain the project.".to_owned(),
        conversation: vec![ConversationMessage {
            role: MessageRole::User,
            text: "Earlier question".to_owned(),
        }],
        files: vec![ExplicitFile {
            path: PathBuf::from("src/lib.rs"),
            content: "pub fn run() {}".to_owned(),
        }],
        tool_results: vec![ToolContextResult {
            name: "read_file".to_owned(),
            result: ToolResult::new("selected output"),
            is_shell: false,
        }],
        git_status: Some(GitStatus {
            repository_root: PathBuf::from("/workspace"),
            branch: Some("main".to_owned()),
            head: Some("abc123".to_owned()),
            is_clean: false,
            changed_files: vec!["src/lib.rs".to_owned()],
            staged_files: vec![],
            unstaged_files: vec!["src/lib.rs".to_owned()],
            untracked_files: vec![],
        }),
    }
}

#[test]
fn assembles_only_explicit_context_with_reasons() {
    let assembly = ContextBuilder::default().build(&input()).unwrap();

    assert!(assembly.prompt.contains("System instructions"));
    assert!(assembly.prompt.contains("AGENTS.md"));
    assert!(assembly.prompt.contains("src/lib.rs"));
    assert!(assembly.prompt.contains("selected output"));
    assert!(assembly.prompt.contains("branch: main"));
    assert_eq!(assembly.excluded_items, 0);
    assert!(assembly
        .included_items()
        .any(|item| item.reason
            == harness_context::ContextReason::ProjectInstruction { precedence: 0 }));
    assert!(assembly
        .items
        .iter()
        .any(|item| item.category == ContextCategory::File));
}

#[test]
fn truncates_large_files_and_tool_results_at_configured_limits() {
    let budget = ContextBudget {
        max_individual_file_bytes: 32,
        max_tool_result_bytes: 16,
        max_shell_output_bytes: 8,
        ..ContextBudget::default()
    };
    let mut input = input();
    input.files[0].content = "f".repeat(200);
    input.tool_results[0].result.output = "t".repeat(100);

    let assembly = ContextBuilder::new(budget).build(&input).unwrap();
    let file = assembly
        .items
        .iter()
        .find(|item| item.category == ContextCategory::File)
        .unwrap();
    let tool = assembly
        .items
        .iter()
        .find(|item| item.category == ContextCategory::ToolResult)
        .unwrap();

    assert!(file.truncated);
    assert!(file.limits.contains(&ContextLimit::FileSize));
    assert!(file.content.len() <= 32);
    assert!(tool.truncated);
    assert!(tool.limits.contains(&ContextLimit::ToolResultSize));
    assert!(tool.content.len() <= 16);
}

#[test]
fn working_budget_excludes_optional_items_and_keeps_required_request() {
    let mut input = input();
    input.system_instructions = "s".repeat(400);
    input.user_request = "u".repeat(400);
    input.files.clear();
    input.tool_results.clear();
    input.instructions.clear();
    input.conversation.clear();
    let budget = ContextBudget {
        max_individual_file_bytes: 1024,
        max_tool_result_bytes: 1024,
        max_shell_output_bytes: 1024,
        max_working_context_tokens: 120,
    };

    let assembly = ContextBuilder::new(budget).build(&input).unwrap();

    assert!(assembly.prompt.contains("System instructions"));
    assert!(assembly.prompt.contains("User request"));
    assert!(assembly.estimated_tokens <= 120);
    assert!(assembly.excluded_items > 0);
}

#[test]
fn instruction_precedence_is_preserved_in_prompt() {
    let mut input = input();
    input.files.clear();
    input.tool_results.clear();
    input.conversation.clear();
    input.git_status = None;
    input
        .instructions
        .sort_by_key(|instruction| instruction.precedence);

    let assembly = ContextBuilder::default().build(&input).unwrap();
    let prompt = assembly.prompt;
    let agents = prompt.find("Prefer focused changes.").unwrap();
    let readme = prompt.find("Run cargo test.").unwrap();

    assert!(agents < readme);
    assert_eq!(
        assembly
            .items
            .iter()
            .filter(|item| item.category == ContextCategory::Instruction)
            .count(),
        2
    );
}

#[test]
fn validates_zero_budget() {
    let budget = ContextBudget {
        max_working_context_tokens: 0,
        ..ContextBudget::default()
    };
    let result = ContextBuilder::new(budget).build(&ContextInput::default());

    assert!(result.is_err());
}

#[allow(dead_code)]
fn assembly_type_is_serializable(assembly: &ContextAssembly) -> serde_json::Value {
    serde_json::to_value(assembly).unwrap()
}
