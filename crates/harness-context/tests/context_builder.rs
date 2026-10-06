use std::path::PathBuf;

use harness_context::{
    ContextBudget, ContextBudgetManager, ContextBuilder, ContextCategory, ContextInput,
    ContextLimit, ExplicitFile, ToolContextResult, WorkspaceMetadata,
};
use harness_core::InstructionFile;
use harness_git::GitStatus;
use harness_session::{CompactState, ConversationMessage, MessageRole};
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
            already_in_model_history: false,
        }],
        compacted_state: None,
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
        git_diff: None,
    }
}

#[test]
fn model_window_caps_the_working_budget_without_fabricating_unknown_limits() {
    let manager = ContextBudgetManager::new(ContextBudget::default());
    let small = manager.budget_for_context_window(Some(8_000));
    let large = manager.budget_for_context_window(Some(200_000));
    let unknown = manager.budget_for_context_window(None);

    assert_eq!(small.max_working_context_tokens, 6_000);
    assert_eq!(large.max_working_context_tokens, 32 * 1024);
    assert_eq!(unknown.max_working_context_tokens, 32 * 1024);
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
        max_git_diff_bytes: 1024,
        max_compaction_summary_bytes: 1024,
        max_working_context_tokens: 120,
    };

    let assembly = ContextBuilder::new(budget).build(&input).unwrap();

    assert!(assembly.prompt.contains("System instructions"));
    assert!(assembly.prompt.contains("User request"));
    assert!(assembly.estimated_tokens <= 120);
    assert!(assembly.excluded_items > 0);
}

#[test]
fn high_priority_request_instructions_and_current_diff_survive_budget_pressure() {
    let mut input = input();
    input.user_request = "CURRENT-REQUEST-KEEP".to_owned();
    input.instructions[0].content = "PROJECT-INSTRUCTIONS-KEEP".to_owned();
    input.git_diff = Some("CURRENT-DIFF-KEEP".to_owned());
    input.files[0].content = "LOW-PRIORITY-FILE".repeat(100);
    input
        .workspace
        .details
        .insert("repository_map".to_owned(), "LOW-PRIORITY-MAP".repeat(100));
    input.conversation = (0..20)
        .map(|index| ConversationMessage {
            role: MessageRole::Assistant,
            text: format!("old-history-marker-{index}"),
        })
        .collect();
    let budget = ContextBudget {
        max_individual_file_bytes: 16_000,
        max_working_context_tokens: 120,
        ..ContextBudget::default()
    };

    let assembly = ContextBuilder::new(budget).build(&input).unwrap();

    assert!(assembly.prompt.contains("CURRENT-REQUEST-KEEP"));
    assert!(assembly.prompt.contains("PROJECT-INSTRUCTIONS-KEEP"));
    assert!(assembly.prompt.contains("CURRENT-DIFF-KEEP"));
    assert!(!assembly.prompt.contains("old-history-marker-0"));
    assert!(!assembly.prompt.contains("old-history-marker-11"));
    assert!(assembly.estimated_tokens <= 120);
}

#[test]
fn shell_output_summary_keeps_diagnostics_and_the_tail() {
    let output = (0..200)
        .map(|index| match index {
            0 => "command: cargo test".to_owned(),
            90 => "error[E0425]: name not found".to_owned(),
            199 => "test result: FAILED".to_owned(),
            _ => format!("routine output line {index}"),
        })
        .collect::<Vec<_>>()
        .join("\n");

    let summary = harness_context::summarize_shell_output(&output, 512);

    assert!(summary.len() <= 512);
    assert!(summary.contains("command: cargo test"));
    assert!(summary.contains("error[E0425]"));
    assert!(summary.contains("test result: FAILED"));
    assert!(summary.contains("omitted"));
}

#[test]
fn protocol_history_results_are_not_duplicated_in_the_context_prompt() {
    let mut input = input();
    input.tool_results = vec![
        ToolContextResult {
            name: "read_file".to_owned(),
            result: ToolResult::new("older discovery stays in working context"),
            is_shell: false,
            already_in_model_history: false,
        },
        ToolContextResult {
            name: "shell".to_owned(),
            result: ToolResult::new("latest result is carried as a protocol tool message"),
            is_shell: true,
            already_in_model_history: true,
        },
        ToolContextResult {
            name: "verification".to_owned(),
            result: ToolResult::new("verification failure must remain visible"),
            is_shell: false,
            already_in_model_history: false,
        },
    ];

    let assembly = ContextBuilder::default().build(&input).unwrap();

    assert!(assembly
        .prompt
        .contains("older discovery stays in working context"));
    assert!(!assembly
        .prompt
        .contains("latest result is carried as a protocol tool message"));
    assert!(assembly
        .prompt
        .contains("verification failure must remain visible"));
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
fn includes_compacted_working_state_in_prompt() {
    let mut input = input();
    input.compacted_state = Some(CompactState {
        task: "Keep the session resumable".to_owned(),
        current_approach: "Persist a continuation state".to_owned(),
        discoveries: vec!["The original event log is durable".to_owned()],
        important_files: vec!["src/lib.rs".to_owned()],
        files_modified: vec!["src/lib.rs".to_owned()],
        decisions: vec!["Use a replaceable strategy".to_owned()],
        failed_attempts: vec![],
        test_status: vec!["workspace tests pass".to_owned()],
        remaining_work: vec!["Resume verification".to_owned()],
        ..CompactState::default()
    });

    let assembly = ContextBuilder::default().build(&input).unwrap();

    assert!(assembly.prompt.contains("Compacted working state"));
    assert!(assembly.prompt.contains("Keep the session resumable"));
    assert!(assembly
        .prompt
        .contains("The original event log is durable"));
    assert_eq!(
        assembly
            .items
            .iter()
            .find(|item| item.category == ContextCategory::CompactionSummary)
            .map(|item| item.reason.clone()),
        Some(harness_context::ContextReason::CompactionSummary)
    );
}

#[test]
fn compacted_summary_has_its_own_size_limit() {
    let mut input = input();
    input.compacted_state = Some(CompactState {
        task: "Keep this goal".to_owned(),
        discoveries: vec!["useful discovery ".repeat(500)],
        ..CompactState::default()
    });
    let budget = ContextBudget {
        max_compaction_summary_bytes: 128,
        ..ContextBudget::default()
    };

    let assembly = ContextBuilder::new(budget).build(&input).unwrap();
    let summary = assembly
        .items
        .iter()
        .find(|item| item.category == ContextCategory::CompactionSummary)
        .unwrap();

    assert!(summary.truncated);
    assert!(summary
        .limits
        .contains(&ContextLimit::CompactionSummarySize));
    assert!(summary.content.len() <= 128);
    assert!(summary.content.contains("Keep this goal"));
}

#[test]
fn compacted_history_does_not_displace_current_instructions_or_diff() {
    let mut input = input();
    input.instructions[0].content = "CURRENT-INSTRUCTIONS".to_owned();
    input.git_diff = Some("CURRENT-DIFF".to_owned());
    input.compacted_state = Some(CompactState {
        task: "Preserve the active goal".to_owned(),
        discoveries: vec!["old discovery ".repeat(2_000)],
        ..CompactState::default()
    });
    let budget = ContextBudget {
        max_working_context_tokens: 120,
        ..ContextBudget::default()
    };

    let assembly = ContextBuilder::new(budget).build(&input).unwrap();

    assert!(assembly.prompt.contains("CURRENT-INSTRUCTIONS"));
    assert!(assembly.prompt.contains("CURRENT-DIFF"));
    assert!(assembly.prompt.contains("Explain the project."));
    assert!(
        !assembly
            .items
            .iter()
            .find(|item| item.category == ContextCategory::CompactionSummary)
            .unwrap()
            .included
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

#[test]
fn workspace_repository_map_is_rendered_as_initial_context() {
    let mut input = input();
    input.workspace.details.insert(
        "repository_map".to_owned(),
        "Top-level: crates, apps\nPackages: crates/harness-tools (harness-tools)\nKey symbols:\n- src/lib.rs:1 function run".to_owned(),
    );
    let assembly = ContextBuilder::default().build(&input).unwrap();
    assert!(assembly
        .prompt
        .contains("repository_map: Top-level: crates, apps"));
    assert!(assembly.prompt.contains("harness-tools"));
    assert!(assembly.prompt.contains("function run"));
}
