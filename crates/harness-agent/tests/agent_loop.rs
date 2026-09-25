use std::path::Path;
use std::sync::Arc;

use harness_agent::{AgentLimits, AgentRunner, AgentTask, ApprovalHandler, DenyApprovalHandler};
use harness_context::{ContextBuilder, WorkspaceMetadata};
use harness_models::{ContentBlock, ModelResponse, ScriptedMockProvider, ToolCall, Usage};
use harness_policy::{AllowAllPolicy, DenyAllPolicy, ExecutionMode, PolicyEngine};
use harness_session::{JsonlSessionStore, SessionStatus, SessionStore};
use harness_tools::{CancellationToken, ToolRegistry};
use tempfile::tempdir;

fn response(text: &str, tool: Option<(&str, serde_json::Value)>) -> ModelResponse {
    let has_tool = tool.is_some();
    let mut response = ModelResponse {
        id: "scripted".to_owned(),
        model: "scripted".to_owned(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }]
        },
        tool_calls: tool
            .map(|(name, arguments)| ToolCall {
                id: format!("call-{name}"),
                name: name.to_owned(),
                arguments,
            })
            .into_iter()
            .collect(),
        finish_reason: if has_tool {
            harness_models::FinishReason::ToolCalls
        } else {
            harness_models::FinishReason::Stop
        },
        usage: Some(Usage::new(10, 2)),
    };
    response.model = "scripted".to_owned();
    response
}

struct ApproveAll;

impl ApprovalHandler for ApproveAll {
    fn request(
        &self,
        _tool: &harness_tools::ToolRequest,
    ) -> Result<bool, harness_agent::AgentError> {
        Ok(true)
    }
}

fn task(workspace: &Path) -> AgentTask {
    AgentTask {
        workspace_root: workspace.to_path_buf(),
        user_task: "Read, edit, run, and finish".to_owned(),
        system_instructions: "Use tools carefully.".to_owned(),
        workspace: WorkspaceMetadata {
            root: Some(workspace.to_path_buf()),
            ..WorkspaceMetadata::default()
        },
        ..AgentTask::default()
    }
}

fn runner(
    provider: Arc<ScriptedMockProvider>,
    workspace: &Path,
    policy: Arc<dyn harness_policy::Policy>,
    approval: Arc<dyn ApprovalHandler>,
) -> (AgentRunner, Arc<JsonlSessionStore>) {
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let runner = AgentRunner::new(
        provider,
        "scripted",
        ToolRegistry::with_workspace_tools(),
        policy,
        sessions.clone(),
        ContextBuilder::default(),
        AgentLimits::default(),
        approval,
    );
    (runner, sessions)
}

#[test]
fn mock_agent_reads_edits_runs_observes_and_finishes() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(workspace.join("file.txt"), "before\n").unwrap();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response(
                "",
                Some(("read_file", serde_json::json!({ "path": "file.txt" }))),
            ),
            response(
                "",
                Some((
                    "write_file",
                    serde_json::json!({ "path": "file.txt", "content": "after\n" }),
                )),
            ),
            response(
                "",
                Some(("shell", serde_json::json!({ "command": "echo command-ok" }))),
            ),
            response("All done", None),
        ],
    ));
    let policy = Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace));
    let (runner, sessions) = runner(provider, workspace, policy, Arc::new(ApproveAll));

    let outcome = runner
        .run(&task(workspace), &CancellationToken::new())
        .unwrap();

    assert_eq!(outcome.final_message, "All done");
    assert_eq!(outcome.turns, 4);
    assert_eq!(outcome.tool_calls, 3);
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "after\n"
    );
    let session = sessions.load(&outcome.session_id).unwrap();
    assert_eq!(session.state().unwrap().status, SessionStatus::Completed);
    let event_types = session
        .events
        .iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(event_types.contains(&harness_session::EventType::ToolRequested));
    assert!(event_types.contains(&harness_session::EventType::PolicyDecision));
    assert!(event_types.contains(&harness_session::EventType::FileChanged));
    assert!(event_types.contains(&harness_session::EventType::ProcessExited));
    assert_eq!(
        event_types.last(),
        Some(&harness_session::EventType::SessionCompleted)
    );
}

#[test]
fn denied_tool_cannot_bypass_policy() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![response(
            "",
            Some((
                "write_file",
                serde_json::json!({ "path": "blocked.txt", "content": "blocked" }),
            )),
        )],
    ));
    let (runner, sessions) = runner(
        provider,
        workspace,
        Arc::new(DenyAllPolicy),
        Arc::new(DenyApprovalHandler),
    );

    let result = runner.run(&task(workspace), &CancellationToken::new());

    assert!(result.is_err());
    assert!(!workspace.join("blocked.txt").exists());
    let session_id = sessions.recent(1).unwrap()[0].id.clone();
    let session = sessions.load(&session_id).unwrap();
    assert!(session
        .events
        .iter()
        .any(|event| event.event_type == harness_session::EventType::ToolDenied));
}

#[test]
fn cancellation_records_failed_session_before_model_call() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new("scripted", vec![]));
    let (runner, sessions) = runner(
        provider,
        workspace,
        Arc::new(AllowAllPolicy),
        Arc::new(DenyApprovalHandler),
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = runner.run(&task(workspace), &cancellation);

    assert!(matches!(result, Err(harness_agent::AgentError::Cancelled)));
    let session_id = sessions.recent(1).unwrap()[0].id.clone();
    let session = sessions.load(&session_id).unwrap();
    assert_eq!(session.state().unwrap().status, SessionStatus::Failed);
}

#[test]
fn turn_limit_stops_before_extra_tool_execution() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![response(
            "",
            Some((
                "write_file",
                serde_json::json!({ "path": "blocked.txt", "content": "blocked" }),
            )),
        )],
    ));
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let runner = AgentRunner::new(
        provider,
        "scripted",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        sessions,
        ContextBuilder::default(),
        AgentLimits {
            max_turns: 1,
            ..AgentLimits::default()
        },
        Arc::new(DenyApprovalHandler),
    );

    let result = runner.run(&task(workspace), &CancellationToken::new());

    assert!(matches!(
        result,
        Err(harness_agent::AgentError::LimitExceeded { .. })
    ));
    assert!(!workspace.join("blocked.txt").exists());
}
